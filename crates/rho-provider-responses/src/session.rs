use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::ws::WebSocketPool;
use crate::{
    CHATGPT_CODEX_MODELS, DEFAULT_CHATGPT_BASE_URL, DEFAULT_MODEL, ResponsesAuth,
    ResponsesCompaction,
};

#[derive(Clone)]
pub struct ProviderSession {
    pub(super) websocket_pool: Arc<Mutex<WebSocketPool>>,
    pub base_url: String,
    pub auth: ResponsesAuth,
    pub compaction: Option<ResponsesCompaction>,
    pub extra_body: BTreeMap<String, Value>,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_summary: ReasoningSummary,
    pub verbosity: Option<Verbosity>,
    pub service_tier: Option<ServiceTier>,
    pub tool_choice: ToolChoice,
    pub prompt_cache_key: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReasoningSummary {
    #[default]
    Off,
    Auto,
    Concise,
    Detailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceTier {
    Auto,
    Default,
    Flex,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
}

impl Default for ProviderSession {
    fn default() -> Self {
        Self::new(DEFAULT_MODEL)
    }
}

impl ProviderSession {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            websocket_pool: Arc::new(Mutex::new(WebSocketPool::new())),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth: ResponsesAuth::None,
            compaction: None,
            extra_body: BTreeMap::new(),
            model: model.into(),
            temperature: None,
            max_output_tokens: None,
            reasoning_effort: None,
            reasoning_summary: ReasoningSummary::Off,
            verbosity: None,
            service_tier: None,
            tool_choice: ToolChoice::Auto,
            prompt_cache_key: None,
        }
    }

    pub fn chatgpt_codex(model: impl Into<String>, auth: ResponsesAuth) -> Self {
        let mut session = Self::new(model);
        session.auth = auth;
        session
    }

    pub fn chatgpt_codex_models() -> &'static [&'static str] {
        CHATGPT_CODEX_MODELS
    }

    pub fn with_compaction(mut self, compaction: ResponsesCompaction) -> Self {
        self.compaction = Some(compaction);
        self
    }

    pub fn with_prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
        self
    }
}

impl std::fmt::Debug for ProviderSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSession")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("compaction", &self.compaction)
            .field("extra_body", &self.extra_body)
            .field("model", &self.model)
            .field("temperature", &self.temperature)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("reasoning_summary", &self.reasoning_summary)
            .field("verbosity", &self.verbosity)
            .field("service_tier", &self.service_tier)
            .field("tool_choice", &self.tool_choice)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .finish()
    }
}
