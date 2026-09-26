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

use futures::future::BoxFuture;
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
    /// Stable per agent, so the provider can reuse its cache across steps.
    pub cache_key: uuid::Uuid,
}

#[derive(Clone, Debug)]
pub enum Item {
    /// One of the model's earlier responses, replayed as it came.
    Step(Carry),
    /// What an earlier step's `exec` call produced.
    Result {
        call_id: String,
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
    pub id: String,
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

/// A model: one request in, one step out. The caller owns retries.
pub trait Model: Send + Sync {
    fn step<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, anyhow::Result<Step>>;
}
