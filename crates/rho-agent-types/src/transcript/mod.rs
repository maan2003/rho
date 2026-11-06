//! Shared Claude stream/tool shapes.
//! Native inference executes through its own step API.

use std::sync::Arc;

use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AgentId, ContentPart, MessagePhase, ToolOutputStatus, UnixMs};

mod util;

use self::util::validated_string_type;

senax_encoder::declare_senax_tagged_trait!(
    pub trait ProviderSpecificData,
    unknown = UnknownProviderSpecificData,
);

validated_string_type!(
    /// Provider-issued identity of an exec, shared with its notebook cell and reports.
    pub ExecId,
    self::util::validate_identifier
);

validated_string_type!(
    /// Name of a tool, shared by [`ToolSpec`] and the [`ToolCall`] that invokes it.
    pub ToolName,
    self::util::validate_identifier
);

validated_string_type!(
    pub ProviderResponseId,
    self::util::validate_identifier
);

validated_string_type!(
    pub ProviderResponseItemId,
    self::util::validate_identifier
);

/// Who authored a message entering an agent's context. Providers render both
/// as user-role input; UIs render agents' messages distinctly from user
/// messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum MessageSender {
    User,
    Agent { id: AgentId },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ContextBlock {
    // intentionally limiting to prevent misuse
    UserMessage {
        sender: MessageSender,
        content: Vec<ContentPart>,
    },
    ToolResults {
        results: Vec<ToolResult>,
    },
    /// An out-of-band extra output for an earlier tool call: the call's
    /// result closes the call, updates annotate it while it (or work it
    /// started) is still running.
    ToolUpdate(ToolUpdate),
    InferenceResponse {
        items: Vec<InferenceResponseItem>,
        provider_response_id: Option<ProviderResponseId>,
    },
    CompactionTrigger,
    /// Activate a new provider window without deleting transcript history.
    /// `retain_from` is an inclusive index into the complete block history
    /// (including control blocks), no later than this block. Cutoffs may only
    /// advance. This item emits no provider content and invalidates all
    /// provider continuations established before it.
    ContextRotation {
        retain_from: u64,
    },
    /// Harness-authored context, distinct from user input and tool output.
    DeveloperMessage {
        text: String,
    },
    /// Remove completed tool exchanges from provider replay only. Original
    /// blocks remain available in transcript history. Invalidates continuations
    /// established before this item.
    ToolHistoryEvicted {
        call_ids: Vec<ExecId>,
    },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum InferenceResponseItem {
    AssistantMessage {
        provider_specific: Box<dyn ProviderSpecificData>,
        content: Vec<ContentPart>,
        phase: Option<MessagePhase>,
    },
    ToolCall {
        provider_specific: Box<dyn ProviderSpecificData>,
        id: ExecId,
        name: ToolName,
        tool_type: ToolType,
        // arbitrary could be json!
        arguments: String,
    },
    EncryptedReasoning {
        provider_specific: Box<dyn ProviderSpecificData>,
        summary: Vec<String>,
    },
    RawReasoning {
        provider_specific: Box<dyn ProviderSpecificData>,
        content: String,
        summary: Vec<String>,
    },
    Compaction {
        provider_specific: Box<dyn ProviderSpecificData>,
    },
    Unknown {
        provider_specific: Box<dyn ProviderSpecificData>,
    },
}

/// A decoded and normalized image suitable for provider input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Pack, Unpack)]
pub struct ImageContent {
    pub media_type: String,
    pub data: Vec<u8>,
    pub detail: ImageDetail,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Pack, Unpack,
)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    #[default]
    High,
    Original,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ToolSpec {
    pub name: ToolName,
    pub tool_type: ToolType,
    pub description: String,
    pub input_schema: Value,
    pub format: Option<ToolFormat>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ToolCall {
    pub id: ExecId,
    pub name: ToolName,
    pub tool_type: ToolType,
    // arbitrary could be json!
    pub arguments: String,
}

/// Turn-local context available to tools without placing it in model-authored
/// arguments. Tools should retain only the bounded parts they actually need.
#[derive(Clone, Debug, Default)]
pub struct ToolExecutionContext {
    pub model: Arc<str>,
    pub input: Arc<[Arc<ContextBlock>]>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct ToolOutput {
    /// Bounded rendering sent to the model verbatim.
    pub output: Arc<String>,
    /// Complete textual record for persistence and readers. `None` means the
    /// model-facing output is already the complete record.
    #[serde(default)]
    pub full_output: Option<Arc<String>>,
    /// Typed image items sent to the model after the textual output.
    pub images: Arc<Vec<ImageContent>>,
    /// Harness/UI metadata only; not included in the provider wire payload.
    pub status: ToolOutputStatus,
}

impl ToolOutput {
    pub fn recorded_output(&self) -> &str {
        self.full_output.as_deref().unwrap_or(&self.output)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ToolResult {
    /// Matches the [`ToolCall`] this result answers.
    pub call_id: ExecId,
    /// Wire shape for replaying this result to the provider.
    pub tool_type: ToolType,
    pub body: ToolOutput,
    /// Historical call display start. Exec lifecycle observations are
    /// authoritative.
    pub started_at: UnixMs,
    /// Historical first-result timestamp, not proof that Python or its jobs
    /// finished.
    pub finished_at: UnixMs,
}

/// An extra output item for a tool call that has (or will have) its own
/// result — a progress note, not an execution summary. Responses serializes
/// updates as named standalone outputs without a provider call id; the local
/// call id retains transcript attribution across compaction.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ToolUpdate {
    /// Status of this contribution when recorded natively; unknown on old
    /// wire-only reports.
    pub status: Option<ToolOutputStatus>,
    /// The [`ToolCall`] this update annotates.
    pub call_id: ExecId,
    /// Wire shape for replaying this update to the provider.
    pub tool_type: ToolType,
    pub output: Arc<String>,
    /// Complete textual record when `output` is a bounded model view.
    pub full_output: Option<Arc<String>>,
    /// When the tool emitted the update (a single instant; updates have no
    /// duration).
    pub at: UnixMs,
    pub images: Arc<Vec<ImageContent>>,
}

impl ToolUpdate {
    pub fn recorded_output(&self) -> &str {
        self.full_output.as_deref().unwrap_or(&self.output)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum ToolType {
    Function,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
pub enum ToolGrammarSyntax {
    Lark,
    Regex,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolFormat {
    Text,
    Grammar {
        syntax: ToolGrammarSyntax,
        definition: String,
    },
}

/// Concatenate the text parts of a message.
pub fn text_content(parts: &[ContentPart]) -> String {
    let mut output = String::new();
    for part in parts {
        match part {
            ContentPart::Text { text } => output.push_str(text),
            ContentPart::Image { media_type, .. } => {
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(match media_type.as_str() {
                    "image/png" => "[image: PNG]",
                    "image/jpeg" => "[image: JPEG]",
                    "image/webp" => "[image: WebP]",
                    "image/gif" => "[image: GIF]",
                    _ => "[image]",
                });
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_round_trips_its_complete_record() {
        let output = ToolOutput {
            output: Arc::new("bounded".to_owned()),
            full_output: Some(Arc::new("complete".to_owned())),
            images: Arc::new(Vec::new()),
            status: ToolOutputStatus::Success,
        };

        let mut encoded = senax_encoder::encode(&output).unwrap();
        let decoded = senax_encoder::decode::<ToolOutput>(&mut encoded).unwrap();
        assert_eq!(decoded, output);
        assert_eq!(decoded.recorded_output(), "complete");
    }

    #[test]
    fn tool_call_id_converts_and_borrows_as_str() {
        let from_str = ExecId::try_from("call-1").unwrap();
        assert_eq!(from_str.as_ref(), "call-1");

        let arc: Arc<str> = Arc::from("call-2");
        let from_arc = ExecId::try_from(arc.clone()).unwrap();
        assert_eq!(from_arc.as_str(), "call-2");
    }

    #[test]
    fn validated_string_type_rejects_invalid_characters() {
        let error = ToolName::try_from("bad name").unwrap_err();
        assert!(
            error.to_string().contains("invalid character"),
            "unexpected error: {error}"
        );
        assert!(ExecId::try_from("").is_err());
    }

    #[test]
    fn validated_string_type_validates_on_deserialize() {
        let ok: ToolName = serde_json::from_str("\"shell_command\"").unwrap();
        assert_eq!(ok.as_str(), "shell_command");

        let err = serde_json::from_str::<ToolName>("\"bad name\"").unwrap_err();
        assert!(
            err.to_string().contains("invalid character"),
            "unexpected error: {err}"
        );
    }
}
