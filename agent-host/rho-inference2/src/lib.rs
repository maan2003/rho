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
    /// One response. `stream` sees the call's code as it arrives; the step
    /// holds all of it, and is what to keep.
    pub async fn step(
        &self,
        request: &Request,
        stream: &mut (dyn FnMut(Stream<'_>) + Send),
    ) -> anyhow::Result<Step> {
        match self {
            Model::OpenAi(model) => model.step(request, stream).await,
            Model::Scripted(model) => model.step(request, stream).await,
        }
    }
}

impl Carry {
    /// A call that was cut off: all that can be replayed is the code that
    /// ran.
    pub fn bare(call: Call) -> Self {
        Self(Inner::Scripted { call: Some(call) })
    }
}
