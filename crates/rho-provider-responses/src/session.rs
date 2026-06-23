use std::sync::Arc;

use tokio::sync::Mutex;

use crate::ws::WebSocketPool;
use crate::{DEFAULT_CHATGPT_BASE_URL, ResponsesAuth, ResponsesCompaction};

#[derive(Clone)]
pub struct ProviderSession {
    pub(super) websocket_pool: Arc<Mutex<WebSocketPool>>,
    pub(crate) base_url: String,
    pub(crate) auth: ResponsesAuth,
    pub(crate) compaction: Option<ResponsesCompaction>,
    pub(crate) model: String,
    pub(crate) prompt_cache_key: Option<String>,
}

impl ProviderSession {
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            websocket_pool: Arc::new(Mutex::new(WebSocketPool::new())),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth: ResponsesAuth::none(),
            compaction: None,
            model: model.into(),
            prompt_cache_key: None,
        }
    }

    pub fn chatgpt_codex(model: impl Into<String>, auth: ResponsesAuth) -> Self {
        let mut session = Self::new(model);
        session.auth = auth;
        session
    }

    pub fn with_compaction(mut self) -> Self {
        self.compaction = Some(ResponsesCompaction::default());
        self
    }

    pub fn with_compaction_threshold(mut self, compact_threshold: u64) -> Self {
        self.compaction = Some(ResponsesCompaction {
            compact_threshold: Some(compact_threshold),
        });
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

impl std::fmt::Debug for ProviderSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSession")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("compaction", &self.compaction)
            .field("model", &self.model)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .finish()
    }
}
