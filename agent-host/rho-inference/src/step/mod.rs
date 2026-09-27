//! One model step at a time, for an agent whose every response is code.
//!
//! The agent appends model-facing items to [`Context`]; this crate owns the
//! active window, warm connection and continuation selection. Each wake takes
//! an O(1) [`Request`] snapshot and receives a [`Step`]. The model answers only
//! by calling `exec` with a cell of Python. Anything else it writes is kept as
//! `prose`, which the agent does not deliver anywhere.
//!
//! What only the provider understands (item ids, encrypted reasoning) rides
//! in a [`Carry`]: stored by the agent, replayed verbatim, never read.

use std::sync::{Arc, OnceLock};

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
    pub(crate) items: Arc<Vec<Item>>,
    // Only Context can mint an append-only lineage. Rebuild/rewind gets a new one.
    pub(crate) lineage: uuid::Uuid,
    pub cache_key: CacheKey,
}

impl Request {
    pub fn items(&self) -> &[Item] {
        &self.items
    }
    pub fn new(instructions: Arc<str>, items: Vec<Item>, cache_key: CacheKey) -> Self {
        let mut context = Context::default();
        for item in items {
            context.push(item);
        }
        context.request(instructions, cache_key)
    }
}

/// The active provider context, independent of the agent's durable/display log.
/// Appends preserve lineage; compaction or reconstruction invalidate
/// continuation.
pub struct Context {
    items: Arc<Vec<Item>>,
    lineage: uuid::Uuid,
}
impl Default for Context {
    fn default() -> Self {
        Self {
            items: Arc::default(),
            lineage: uuid::Uuid::new_v4(),
        }
    }
}
impl Context {
    pub fn push(&mut self, item: Item) {
        if matches!(&item, Item::Step(carry) if carry.has_compaction()) {
            self.items = Arc::default();
            self.lineage = uuid::Uuid::new_v4();
        }
        Arc::make_mut(&mut self.items).push(item);
    }

    /// O(1) snapshot. The runtime drops it before appending the next response.
    pub fn request(&self, instructions: Arc<str>, cache_key: CacheKey) -> Request {
        Request {
            instructions,
            items: self.items.clone(),
            cache_key,
            lineage: self.lineage,
        }
    }
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

    pub(crate) fn wire_uuid(self, base_url: &str, client_secret: [u8; 32]) -> uuid::Uuid {
        use std::hash::Hasher;
        let mut bytes = [0; 16];
        for (part, tag) in bytes
            .chunks_mut(8)
            .zip([b"rho-step-cache:v1:0", b"rho-step-cache:v1:1"])
        {
            let mut hash = fnv::FnvHasher::default();
            for input in [
                &tag[..],
                &self.0.to_le_bytes(),
                base_url.as_bytes(),
                &client_secret,
            ] {
                hash.write(input);
            }
            part.copy_from_slice(&hash.finish().to_be_bytes());
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        uuid::Uuid::from_bytes(bytes)
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
pub struct Carry(Arc<Inner>);

#[derive(Clone, Debug, Encode, Decode)]
enum Inner {
    /// Responses API output items, normalized for replay, as JSON text.
    OpenAi {
        items: Vec<String>,
        #[senax(skip_encode, skip_decode)]
        _prepared: Arc<OnceLock<Prepared>>,
    },
    /// A scripted step: the call is all there is.
    Scripted { call: Option<Call> },
    /// A scripted response standing in for a provider compaction.
    ScriptedCompaction,
}

// The cache does not participate in persisted identity.
impl PartialEq for Inner {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::OpenAi { items: a, .. }, Self::OpenAi { items: b, .. }) => a == b,
            (Self::Scripted { call: a }, Self::Scripted { call: b }) => a == b,
            (Self::ScriptedCompaction, Self::ScriptedCompaction) => true,
            _ => false,
        }
    }
}
impl Eq for Inner {}

#[derive(Debug)]
struct Prepared {
    items: Vec<serde_json::Value>,
    compaction: Option<usize>,
    calls: Vec<CallId>,
}
impl Prepared {
    fn new(items: Vec<serde_json::Value>) -> Self {
        let compaction = items.iter().rposition(|item| item["type"] == "compaction");
        let calls = items
            .iter()
            .skip(compaction.unwrap_or(0))
            .filter(|item| item["type"] == "custom_tool_call")
            .filter_map(|item| item["call_id"].as_str().map(CallId::new))
            .collect();
        Self {
            items,
            compaction,
            calls,
        }
    }
}
impl Carry {
    fn prepared(&self) -> Option<&Prepared> {
        let Inner::OpenAi { items, _prepared } = &*self.0 else {
            return None;
        };
        Some(_prepared.get_or_init(|| {
            Prepared::new(
                items
                    .iter()
                    .map(|item| serde_json::from_str(item).expect("persisted provider replay item"))
                    .collect(),
            )
        }))
    }

    /// Live output is already parsed. Keep it rather than decoding our own
    /// serialized persistence representation on the next request.
    fn from_openai_values(items: Vec<serde_json::Value>) -> Self {
        let encoded = items.iter().map(serde_json::Value::to_string).collect();
        let prepared = OnceLock::new();
        prepared
            .set(Prepared::new(items))
            .expect("new response cache");
        Self(Arc::new(Inner::OpenAi {
            items: encoded,
            _prepared: Arc::new(prepared),
        }))
    }

    fn same_response(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
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
    /// Decode once on restoration; live responses initialize the same cache.
    pub fn from_openai_items(items: Vec<String>) -> Self {
        Self(Arc::new(Inner::OpenAi {
            items,
            _prepared: Arc::default(),
        }))
    }

    pub fn has_compaction(&self) -> bool {
        match &*self.0 {
            Inner::OpenAi { .. } => self.prepared().unwrap().compaction.is_some(),
            Inner::ScriptedCompaction => true,
            Inner::Scripted { .. } => false,
        }
    }

    pub fn call_ids(&self) -> Vec<CallId> {
        match &*self.0 {
            Inner::OpenAi { .. } => self.prepared().unwrap().calls.clone(),
            Inner::Scripted { call } => call.iter().map(|call| call.id.clone()).collect(),
            Inner::ScriptedCompaction => Vec::new(),
        }
    }

    pub fn bare(call: Call) -> Self {
        Self(Arc::new(Inner::Scripted { call: Some(call) }))
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

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn carry_bytes_remain_compatible_and_metadata_is_cached_across_clones() {
        #[derive(Encode)]
        struct OldCarry(OldInner);
        #[derive(Encode)]
        enum OldInner {
            OpenAi { items: Vec<String> },
        }
        let items = vec![r#"{"type":"custom_tool_call","call_id":"c","input":"pass"}"#.to_owned()];
        let old = senax_encoder::encode(&OldCarry(OldInner::OpenAi {
            items: items.clone(),
        }))
        .unwrap();
        let carry: Carry = senax_encoder::decode(&mut old.as_ref()).unwrap();
        assert_eq!(carry.call_ids(), [CallId::new("c")]);
        let cloned = carry.clone();
        assert!(std::ptr::eq(
            carry.prepared().unwrap(),
            cloned.prepared().unwrap()
        ));
        assert_eq!(old, senax_encoder::encode(&carry).unwrap());
    }

    #[test]
    fn context_snapshot_shares_storage_and_compaction_changes_lineage() {
        let mut context = Context::default();
        context.push(Item::User {
            text: "discard".repeat(10000),
            images: vec![],
        });
        let key = CacheKey::from_u128(7);
        let first = context.request("instructions".into(), key);
        let second = context.request("instructions".into(), key);
        assert!(Arc::ptr_eq(&first.items, &second.items));
        let lineage = first.lineage;
        context.push(Item::Step(Carry(Arc::new(Inner::ScriptedCompaction))));
        context.push(Item::User {
            text: "keep".into(),
            images: vec![],
        });
        let compacted = context.request("instructions".into(), key);
        assert_ne!(lineage, compacted.lineage);
        assert_eq!(compacted.items.len(), 2);
        assert_eq!(first.items.len(), 1); // existing immutable request stays unchanged
    }

    #[test]
    fn wire_cache_identity_is_endpoint_and_credential_scoped() {
        let key = CacheKey::from_u128(19);
        let id = key.wire_uuid("https://one", [1; 32]);
        assert_eq!(id, key.wire_uuid("https://one", [1; 32]));
        assert_ne!(id, key.wire_uuid("https://two", [1; 32]));
        assert_ne!(id, key.wire_uuid("https://one", [2; 32]));
    }
}
