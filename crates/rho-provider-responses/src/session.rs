use crate::DEFAULT_MODEL;

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderSession {
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

    pub fn with_prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
        self
    }
}
