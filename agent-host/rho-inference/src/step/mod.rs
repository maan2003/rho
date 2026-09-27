//! One model step at a time, for an agent whose every response is code.
//!
//! The agent keeps its own log and renders it into a [`Request`] each time
//! the model wakes; this crate turns that into one provider exchange and
//! hands back a [`Step`]. The model answers only by calling `exec` with a
//! cell of Python. Anything else it writes is kept as `prose`, which the
//! agent does not deliver anywhere.
//!
//! What only the provider understands (item ids, encrypted reasoning) rides
//! in a [`Carry`]: stored by the agent, replayed verbatim, never read.

use std::sync::Arc;

use senax_encoder::{Decode, Encode};

pub mod openai;
pub mod scripted;

/// A temporary provider failure. The runtime owns backoff and retry admission.
#[derive(Debug)]
pub struct Retryable(pub String);

impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Retryable {}

pub fn is_retryable(error: &anyhow::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    if error.is::<Retryable>() || error.is::<tokio::time::error::Elapsed>() {
        return true;
    }
    if let Some(error) = error.downcast_ref::<Error>() {
        return match error {
            Error::Io(_)
            | Error::ConnectionClosed
            | Error::AlreadyClosed
            | Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
            Error::Http(response) => {
                response.status().is_server_error() || response.status().as_u16() == 408
            }
            _ => false,
        };
    }
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::BrokenPipe
        )
    })
}

/// The name of the one tool.
pub const EXEC: &str = "exec";

/// Everything the model is shown, oldest first.
#[derive(Clone, Debug)]
pub struct Request {
    pub instructions: Arc<str>,
    pub items: Vec<Item>,
    pub cache_key: CacheKey,
}

/// Stable per agent, so the provider can reuse its cache across steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct CacheKey(u128);

impl CacheKey {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().as_u128())
    }

    pub fn from_u128(key: u128) -> Self {
        Self(key)
    }

    fn uuid(self) -> uuid::Uuid {
        uuid::Uuid::from_u128(self.0)
    }
}

impl Default for CacheKey {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.uuid().fmt(f)
    }
}

/// The provider's id for one `exec` call; a result names the call it answers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct CallId(String);

impl CallId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for CallId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

#[derive(Clone, Debug)]
pub enum Item {
    /// One of the model's earlier responses, replayed as it came.
    Step(Carry),
    /// Ask the provider to compact its context on the next response.
    CompactionTrigger,
    /// What an earlier step's `exec` call produced.
    Result {
        call_id: CallId,
        text: String,
        images: Vec<Image>,
    },
    /// Anything else the model is told: messages, and a report on a step
    /// that made no call.
    User { text: String, images: Vec<Image> },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Image {
    pub media_type: String,
    pub data: Vec<u8>,
}

/// One model response.
#[derive(Clone, Debug)]
pub struct Step {
    /// The `exec` call, if it made one.
    pub call: Option<Call>,
    /// Text outside the call. Not delivered to anyone.
    pub prose: String,
    pub carry: Carry,
    pub usage: Usage,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Call {
    pub id: CallId,
    pub code: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}

/// What a provider needs to see again, opaque to everyone else.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Carry(Inner);

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum Inner {
    /// Responses API output items, normalized for replay, as JSON text.
    OpenAi { items: Vec<String> },
    /// A scripted step: the call is all there is.
    Scripted { call: Option<Call> },
    /// A scripted response standing in for a provider compaction.
    ScriptedCompaction,
}

/// The `exec` call as it arrives, so its code can run while the rest of
/// the response is still being written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream<'a> {
    /// The call has begun.
    Call { id: &'a CallId },
    /// More of its code.
    Code(&'a str),
}

/// A model: one request in, one step out. The caller owns retries.
pub enum Model {
    OpenAi(openai::OpenAi),
    Scripted(Arc<scripted::Scripted>),
}

impl Model {
    /// One response. The caller owns retries; never replay after tool
    /// admission.
    pub async fn step(
        &self,
        request: &Request,
        stream: &mut (dyn FnMut(Stream<'_>) + Send),
    ) -> anyhow::Result<Step> {
        match self {
            Self::OpenAi(model) => model.step(request, stream).await,
            Self::Scripted(model) => model.step(request, stream).await,
        }
    }

    pub async fn text(&self, instructions: Arc<str>, input: String) -> anyhow::Result<String> {
        match self {
            Self::OpenAi(model) => model.text(instructions, input).await,
            Self::Scripted(_) => anyhow::bail!("scripted model has no text mode"),
        }
    }
}

impl Carry {
    /// Reconstitute normalized Responses output items from an older typed
    /// agent log. The importer has already validated their JSON and ids.
    pub fn from_openai_items(items: Vec<String>) -> Self {
        Self(Inner::OpenAi { items })
    }

    /// Whether this response contains a provider compaction boundary.
    pub fn has_compaction(&self) -> bool {
        match &self.0 {
            Inner::OpenAi { items } => items.iter().any(|item| {
                serde_json::from_str::<serde_json::Value>(item)
                    .is_ok_and(|item| item["type"] == "compaction")
            }),
            Inner::ScriptedCompaction => true,
            Inner::Scripted { .. } => false,
        }
    }

    /// The calls this response replays; a result for any other call has
    /// nothing to answer.
    pub fn call_ids(&self) -> Vec<CallId> {
        match &self.0 {
            Inner::OpenAi { items } => items
                .iter()
                .filter_map(|item| serde_json::from_str::<serde_json::Value>(item).ok())
                .filter(|item| item["type"] == "custom_tool_call")
                .filter_map(|item| item["call_id"].as_str().map(CallId::new))
                .collect(),
            Inner::Scripted { call } => call.iter().map(|call| call.id.clone()).collect(),
            Inner::ScriptedCompaction => Vec::new(),
        }
    }

    /// A call that was cut off: all that can be replayed is the code that
    /// ran.
    pub fn bare(call: Call) -> Self {
        Self(Inner::Scripted { call: Some(call) })
    }
}

#[cfg(test)]
mod retry_tests {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::http::Response;

    use super::*;

    #[test]
    fn transient_transport_errors_are_distinct_from_auth_and_bad_requests() {
        for status in [400, 401, 403, 404, 422] {
            let error = anyhow::Error::from(Error::Http(Box::new(
                Response::builder().status(status).body(None).unwrap(),
            )))
            .context("handshake");
            assert!(!is_retryable(&error), "{status}");
        }
        for status in [408, 500, 502, 503, 504] {
            let error = anyhow::Error::from(Error::Http(Box::new(
                Response::builder().status(status).body(None).unwrap(),
            )))
            .context("handshake");
            assert!(is_retryable(&error), "{status}");
        }
        assert!(is_retryable(
            &anyhow::Error::from(Retryable("throttled".into())).context("provider")
        ));
        assert!(!is_retryable(&anyhow::anyhow!("invalid request")));
        assert!(!is_retryable(&anyhow::Error::from(
            serde_json::from_str::<serde_json::Value>("invalid").unwrap_err()
        )));
    }
}
