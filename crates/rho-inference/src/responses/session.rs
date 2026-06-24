use std::sync::Arc;

use tokio::sync::Mutex;

use super::oauth::InferenceAuth;
use super::ws::WebSocketConnection;
use super::{Compaction, DEFAULT_CHATGPT_BASE_URL, DEFAULT_MODEL};

#[derive(Clone)]
pub struct InferenceSession {
    /// The session's warm WebSocket, kept alive across turns. One socket per
    /// session (i.e. per agent/thread); reopened lazily when missing, stale, or
    /// dropped after a failed turn. Shared across clones of the same session.
    pub(super) connection: Arc<Mutex<Option<WebSocketConnection>>>,
    pub(crate) base_url: String,
    pub(crate) auth: InferenceAuth,
    pub(crate) compaction: Option<Compaction>,
    pub(crate) model: String,
    pub(crate) prompt_cache_key: Option<String>,
}

impl InferenceSession {
    pub const DEFAULT_MODEL: &'static str = DEFAULT_MODEL;

    pub fn new(model: impl Into<String>, auth: InferenceAuth) -> Self {
        Self {
            connection: Arc::new(Mutex::new(None)),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth,
            compaction: None,
            model: model.into(),
            prompt_cache_key: None,
        }
    }

    pub fn with_compaction(mut self) -> Self {
        self.compaction = Some(Compaction::Default);
        self
    }

    pub fn with_compaction_threshold(mut self, compact_threshold: u64) -> Self {
        self.compaction = Some(Compaction::Threshold(compact_threshold));
        self
    }

    pub fn with_prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
        self
    }

    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }
}

impl std::fmt::Debug for InferenceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceSession")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("compaction", &self.compaction)
            .field("model", &self.model)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .finish()
    }
}
