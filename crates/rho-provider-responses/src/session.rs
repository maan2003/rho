use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::ws::WebSocketPool;
use crate::{CHATGPT_CODEX_MODELS, DEFAULT_CHATGPT_BASE_URL, ResponsesAuth, ResponsesCompaction};

#[derive(Clone)]
pub struct ProviderSession {
    pub(super) websocket_pool: Arc<Mutex<WebSocketPool>>,
    pub(crate) base_url: String,
    pub(crate) auth: ResponsesAuth,
    pub(crate) compaction: Option<ResponsesCompaction>,
    pub(crate) extra_body: BTreeMap<String, Value>,
    pub(crate) model: String,
    pub(crate) temperature: Option<f32>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) reasoning_summary: ReasoningSummary,
    pub(crate) verbosity: Option<Verbosity>,
    pub(crate) service_tier: Option<ServiceTier>,
    pub(crate) tool_choice: ToolChoice,
    pub(crate) prompt_cache_key: Option<String>,
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

impl ProviderSession {
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            websocket_pool: Arc::new(Mutex::new(WebSocketPool::new())),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth: ResponsesAuth::none(),
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
