//! The services and turn vocabulary the agent requires from an inference
//! provider. Providers interpret replay data; the agent stores it without
//! interpreting it.
use std::sync::Arc;

use futures::future::BoxFuture;
use senax_encoder::{Decode, Encode};
use tokio::sync::mpsc;

mod cache_key;
pub mod config;
#[cfg(test)]
pub mod testing;
pub use cache_key::PromptCacheKey;
pub use config::{InferenceModel, InferenceProfile};
pub type Inference = Arc<dyn Backend>;
pub type InferenceSession = Arc<dyn Session>;

pub trait Backend: Send + Sync {
    fn session(&self, profile: InferenceProfile, model: InferenceModel) -> InferenceSession;
    fn text(&self, instructions: Arc<str>, input: String) -> BoxFuture<'_, anyhow::Result<String>>;
    fn web_credentials(&self) -> BoxFuture<'_, anyhow::Result<rho_web_search::Credentials>>;
}

pub trait Session: Send + Sync {
    /// Dropping the receiver cancels this exchange. A provider classifies
    /// failures, but never schedules a retry: that decision belongs to the
    /// agent.
    fn start(&self, request: Request) -> Response;
}

pub type Response = mpsc::UnboundedReceiver<Event>;
#[derive(Debug)]
pub enum Event {
    Call { carry: Carry },
    Code(String),
    Completed(Step),
    NeedsContext,
    Failed(anyhow::Error),
}
#[derive(Debug)]
pub struct Retryable(pub String);
impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Retryable {}
pub fn is_retryable(error: &anyhow::Error) -> bool {
    error.is::<Retryable>()
}

/// An exchange continuation understood only by the provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Continuation(String);
impl Continuation {
    pub fn new(token: String) -> Self {
        Self(token)
    }
    pub fn into_token(self) -> String {
        self.0
    }
}
#[derive(Clone, Debug)]
pub struct Request {
    pub instructions: Arc<str>,
    pub items: Vec<Item>,
    pub cache_key: CacheKey,
    pub continuation: Option<Continuation>,
}
impl Request {
    pub fn new(instructions: Arc<str>, items: Vec<Item>, cache_key: CacheKey) -> Self {
        Self {
            instructions,
            items,
            cache_key,
            continuation: None,
        }
    }
    pub fn continuation(
        instructions: Arc<str>,
        items: Vec<Item>,
        cache_key: CacheKey,
        previous: Continuation,
    ) -> Self {
        Self {
            instructions,
            items,
            cache_key,
            continuation: Some(previous),
        }
    }
    pub fn items(&self) -> &[Item] {
        &self.items
    }
}
#[derive(Clone, Debug)]
pub enum Item {
    Step {
        carry: Carry,
        exec: Option<String>,
    },
    CompactionTrigger,
    Report {
        text: String,
        images: Vec<Image>,
        reply_to: Option<Carry>,
    },
    User {
        text: String,
        images: Vec<Image>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, serde::Serialize, serde::Deserialize)]
pub struct Image {
    pub media_type: String,
    pub data: Vec<u8>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}
#[derive(Clone, Debug)]
pub struct Step {
    pub continuation: Option<Continuation>,
    pub call: Option<Call>,
    pub prose: String,
    pub carry: Carry,
    pub usage: Usage,
}
/// Transcript presentation, not provider result pairing.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Call {
    id: String,
    pub code: String,
}
impl Call {
    pub fn new(id: impl Into<String>, code: String) -> Self {
        Self {
            id: id.into(),
            code,
        }
    }
    pub fn display_id(&self) -> &str {
        &self.id
    }
}

/// Provider-defined JSON plus facts the runtime and transcript readers need.
/// Raw JSON is stored once and emitted verbatim; only the provider interprets
/// it.
#[derive(Clone, Debug, Encode, Decode)]
pub struct Carry {
    data: Arc<Box<serde_json::value::RawValue>>,
    calls: Vec<Call>,
    compacted: bool,
}
impl PartialEq for Carry {
    fn eq(&self, other: &Self) -> bool {
        self.data.get() == other.data.get()
            && self.calls == other.calls
            && self.compacted == other.compacted
    }
}
impl Eq for Carry {}

impl Carry {
    pub fn new(data: impl serde::Serialize, calls: Vec<Call>, compacted: bool) -> Self {
        Self {
            data: Arc::new(serde_json::value::to_raw_value(&data).expect("provider replay JSON")),
            calls,
            compacted,
        }
    }
    pub fn data(&self) -> &serde_json::value::RawValue {
        &self.data
    }
    pub fn has_compaction(&self) -> bool {
        self.compacted
    }
    pub fn has_call(&self) -> bool {
        !self.calls.is_empty()
    }
    pub fn display_calls(&self) -> Vec<Call> {
        self.calls.clone()
    }
    pub fn set_exec(&mut self, code: &str) {
        self.calls.last_mut().expect("call-start carry").code = code.to_owned();
    }
    pub fn with_code(&self, code: String) -> Call {
        let mut call = self.calls.last().expect("call-start carry").clone();
        call.code = code;
        call
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct CacheKey(pub u128);
impl CacheKey {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().as_u128())
    }
    pub fn from_u128(key: u128) -> Self {
        Self(key)
    }
}
impl Default for CacheKey {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        uuid::Uuid::from_u128(self.0).fmt(f)
    }
}

/// Workset transport forwards these frames without knowing account policy.
pub type PolicySender =
    Arc<dyn Fn(Vec<u8>) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
pub trait PolicyClient: Send + Sync {
    fn receive(&self, bytes: &[u8]) -> anyhow::Result<()>;
    fn disconnect(&self);
}
pub struct WorkerInference {
    pub inference: Inference,
    pub policy: Arc<dyn PolicyClient>,
}
pub type WorkerFactory = fn(&str, PolicySender) -> anyhow::Result<WorkerInference>;
pub type Accounts = Arc<dyn InferenceHost>;
pub trait InferenceHost: Send + Sync {
    fn client(&self) -> Inference;
    fn responses_base_url(&self) -> &str;
    fn serve_policy(
        &self,
        sender: PolicySender,
        // Each request comes with an in-flight token to drop once its reply
        // is sent.
        incoming: mpsc::Receiver<(Vec<u8>, Arc<()>)>,
    ) -> BoxFuture<'static, anyhow::Result<()>>;
}
