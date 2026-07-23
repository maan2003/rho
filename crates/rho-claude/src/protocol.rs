use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Model {
    Opus,
    Sonnet,
    Fable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub(crate) fn as_arg(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl Model {
    pub(crate) fn as_arg(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
            Self::Fable => "fable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Session {
    New {
        session_id: Uuid,
    },
    Resume {
        session_id: Uuid,
    },
    Fork {
        session_id: Uuid,
        source_session_id: Uuid,
        resume_at: Uuid,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum InputMessage {
    User(UserInput),
}

impl InputMessage {
    pub(crate) fn user(text: impl Into<String>) -> Self {
        Self::User(UserInput::text(text, None))
    }

    pub(crate) fn user_with_uuid(text: impl Into<String>, uuid: String) -> Self {
        Self::User(UserInput::text(text, Some(uuid)))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct UserInput {
    session_id: String,
    message: ConversationMessage,
    parent_tool_use_id: Option<String>,
    uuid: Option<String>,
}

impl UserInput {
    fn text(text: impl Into<String>, uuid: Option<String>) -> Self {
        Self {
            session_id: String::new(),
            message: ConversationMessage::user_text(text),
            parent_tool_use_id: None,
            uuid,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct ConversationMessage {
    role: Role,
    content: Vec<InputContent>,
}

impl ConversationMessage {
    fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![InputContent::Text { text: text.into() }],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputContent {
    Text { text: String },
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeEvent {
    Assistant(AssistantMessage),
    CommandLifecycle(CommandLifecycleMessage),
    ControlResponse(ControlResponseMessage),
    RateLimitEvent(RateLimitEvent),
    Result(ResultMessage),
    System(SystemMessage),
    StreamEvent(StreamEvent),
    User(UserOutputMessage),
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RateLimitEvent {
    pub rate_limit_info: RateLimitInfo,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitInfo {
    pub rate_limit_type: Option<String>,
    pub resets_at: Option<i64>,
    pub utilization: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CommandLifecycleMessage {
    pub command_uuid: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ControlResponseMessage {
    pub response: ControlResponse,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ControlResponse {
    pub request_id: String,
    pub subtype: String,
    pub error: Option<String>,
    pub response: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AssistantMessage {
    pub session_id: Option<Uuid>,
    pub message: AssistantConversationMessage,
    pub parent_tool_use_id: Option<String>,
    pub uuid: Option<String>,
}

impl AssistantMessage {
    pub fn text(&self) -> String {
        self.message
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantConversationMessage {
    pub role: Option<Role>,
    #[serde(default)]
    pub content: Vec<AssistantContent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ResultMessage {
    pub subtype: ResultSubtype,
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub is_error: bool,
    pub result: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    pub stop_reason: Option<String>,
    pub terminal_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultSubtype {
    Success,
    ErrorDuringExecution,
    ErrorMaxTurns,
    ErrorMaxBudgetUsd,
    ErrorMaxStructuredOutputRetries,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(
    tag = "subtype",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SystemMessage {
    Init {
        #[serde(alias = "session_id")]
        session_id: Option<Uuid>,
        uuid: Option<String>,
        capabilities: Option<Vec<String>>,
    },
    ApiError {
        #[serde(alias = "session_id")]
        session_id: Option<Uuid>,
        uuid: Option<String>,
        level: Option<String>,
        error: Option<Value>,
        retry_attempt: Option<u64>,
        retry_in_ms: Option<u64>,
        max_retries: Option<u64>,
    },
    AwaySummary {
        #[serde(alias = "session_id")]
        session_id: Option<Uuid>,
        uuid: Option<String>,
        content: Option<String>,
    },
    CompactBoundary {
        #[serde(alias = "session_id")]
        session_id: Option<Uuid>,
        uuid: Option<String>,
        content: Option<String>,
        compact_metadata: Option<SystemCompactMetadata>,
    },
    Informational {
        #[serde(alias = "session_id")]
        session_id: Option<Uuid>,
        uuid: Option<String>,
        content: Option<String>,
        level: Option<String>,
        session_kind: Option<String>,
    },
    LocalCommand {
        #[serde(alias = "session_id")]
        session_id: Option<Uuid>,
        uuid: Option<String>,
        content: Option<String>,
        level: Option<String>,
    },
    SessionStateChanged {
        #[serde(alias = "session_id")]
        session_id: Option<Uuid>,
        uuid: Option<String>,
        state: Option<String>,
    },
    TurnDuration {
        #[serde(alias = "session_id")]
        session_id: Option<Uuid>,
        uuid: Option<String>,
        duration_ms: Option<u64>,
        message_count: Option<u64>,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemCompactMetadata {
    pub trigger: Option<String>,
    pub pre_tokens: Option<u64>,
    pub post_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct StreamEvent {
    pub event: MessageStreamEvent,
    pub session_id: Option<Uuid>,
    pub uuid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageStreamEvent {
    MessageStart {
        message: StreamMessage,
    },
    MessageDelta {
        delta: MessageDelta,
        usage: Option<TokenUsage>,
    },
    MessageStop,
    ContentBlockStart {
        index: usize,
        content_block: StreamContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: ContentBlockDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    Ping,
    Error {
        error: StreamError,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct StreamMessage {
    pub id: Option<String>,
    pub role: Role,
    #[serde(default)]
    pub content: Vec<StreamContentBlock>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Option<TokenUsage>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct MessageDelta {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    CitationsDelta {
        citation: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

impl TokenUsage {
    /// Tokens this message occupies in the context window: every input
    /// bucket (fresh, cache creation, cache read) plus output all count
    /// toward the window.
    pub fn context_total(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
            + self.cache_creation_input_tokens.unwrap_or(0)
            + self.cache_read_input_tokens.unwrap_or(0)
            + self.output_tokens.unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct StreamError {
    #[serde(rename = "type")]
    pub error_type: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct UserOutputMessage {
    pub session_id: Option<Uuid>,
    pub message: Option<OutputConversationMessage>,
    pub parent_tool_use_id: Option<String>,
    pub uuid: Option<String>,
    #[serde(rename = "isReplay")]
    pub is_replay: Option<bool>,
    #[serde(rename = "isSynthetic")]
    pub is_synthetic: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputConversationMessage {
    pub role: Role,
    #[serde(default, deserialize_with = "deserialize_output_content")]
    pub content: Vec<OutputContent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    Text {
        text: String,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    #[serde(other)]
    Other,
}

fn deserialize_output_content<'de, D>(deserializer: D) -> Result<Vec<OutputContent>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![OutputContent::Text { text }]),
        Some(value @ Value::Array(_)) => {
            serde_json::from_value(value).map_err(serde::de::Error::custom)
        }
        Some(value) => Err(serde::de::Error::custom(format!(
            "expected user output content string or array, got {value}"
        ))),
    }
}
