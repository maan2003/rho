use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::{Deserialize, Serialize};
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
    New { session_id: Uuid },
    Resume { session_id: Uuid },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum InputMessage {
    User(UserInput),
}

impl InputMessage {
    pub(crate) fn user(text: impl Into<String>) -> Self {
        Self::User(UserInput::text(text))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct UserInput {
    session_id: String,
    message: ConversationMessage,
    parent_tool_use_id: Option<String>,
}

impl UserInput {
    fn text(text: impl Into<String>) -> Self {
        Self {
            session_id: String::new(),
            message: ConversationMessage::user_text(text),
            parent_tool_use_id: None,
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
    RateLimitEvent,
    Result(ResultMessage),
    System(SystemMessage),
    StreamEvent(StreamEvent),
    User(UserOutputMessage),
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
pub struct SystemMessage {
    pub subtype: Option<String>,
    pub session_id: Option<Uuid>,
    pub uuid: Option<String>,
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
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
    CitationsDelta { citation: Value },
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputConversationMessage {
    pub role: Role,
    #[serde(default)]
    pub content: Vec<OutputContent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    Text { text: String },
    ToolResult { tool_use_id: String, content: Value },
}
