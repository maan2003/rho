//! Small shared vocabulary for rho crates.
//!
//! This crate intentionally avoids owning agent policy. Harnesses, providers,
//! tools, and stores can add their own richer types around these basics.

use std::sync::Arc;
use std::time::Instant;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod util;

use crate::util::validated_string_type;

validated_string_type!(
    /// Identifies a tool call so its result can be matched back to it.
    pub ToolCallId,
    crate::util::validate_identifier
);

validated_string_type!(
    /// Name of a tool, shared by [`ToolSpec`] and the [`ToolCall`] that invokes it.
    pub ToolName,
    crate::util::validate_identifier
);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextBlock {
    // intentionally limiting to prevent misuse
    UserMessage {
        content: Vec<ContentPart>,
    },
    ToolResults {
        results: Vec<ToolResult>,
    },
    InferenceResponse {
        items: Vec<ContextItem>,
        provider_response_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextItem {
    Message(Message),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Reasoning(ReasoningItem),
    Compaction(OpaqueProviderData),
    Unknown(OpaqueProviderData),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentPart {
    Text { text: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    Commentary,
    FinalAnswer,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: ToolName,
    pub tool_type: ToolType,
    pub description: String,
    pub input_schema: Value,
    pub format: Option<ToolFormat>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: ToolName,
    pub tool_type: ToolType,
    // arbitrary could be json!
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub tool_type: ToolType,
    pub status: ToolResultStatus,
    pub output: ToolOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolType {
    Function,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGrammarSyntax {
    Lark,
    Regex,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolFormat {
    Text,
    Grammar {
        syntax: ToolGrammarSyntax,
        definition: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResultStatus {
    Success,
    Error { message: String },
    Cancelled { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningItem {
    pub kind: ReasoningItemKind,
    pub summary: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningItemKind {
    Encrypted(OpaqueProviderData),
    RawReasoning { content: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueProviderData {
    pub tag: String,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InferenceRequest {
    // todo: add instruction
    // arc is used to avoid cloning context blocks too much between requests
    pub input: Vec<Arc<ContextBlock>>,
    pub tools: Arc<[ToolSpec]>,
}

/// Still streaming and pending context item
#[derive(Clone, Debug, PartialEq)]
pub enum PendingContextItem {
    CompactionStarted,
    Reasoning {
        content: Option<String>,
        summary: Vec<String>,
    },
    ToolCall(ToolCall),
    Message(Message),
}

#[derive(Clone, Debug, PartialEq)]
pub enum MaybePendingContextItem {
    // No item here
    Absent,
    Pending(PendingContextItem),
    Complete(ContextItem),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingInferenceResponse {
    pub items: Vec<MaybePendingContextItem>,
}

impl PendingInferenceResponse {
    pub fn apply(&mut self, index: usize, event: ContextItemEvent) -> anyhow::Result<()> {
        if self.items.len() <= index {
            self.items
                .resize(index + 1, MaybePendingContextItem::Absent);
        }

        match (&mut self.items[index], event) {
            (slot @ MaybePendingContextItem::Absent, ContextItemEvent::Add(item)) => {
                *slot = MaybePendingContextItem::Pending(item);
            }

            (
                MaybePendingContextItem::Pending(PendingContextItem::ToolCall(call)),
                ContextItemEvent::ToolCallArgumentDelta { delta },
            ) => {
                call.arguments.push_str(&delta);
            }
            (
                slot @ MaybePendingContextItem::Pending(PendingContextItem::ToolCall(_)),
                ContextItemEvent::ToolCallFinish {},
            ) => {
                let MaybePendingContextItem::Pending(PendingContextItem::ToolCall(call)) =
                    std::mem::replace(slot, MaybePendingContextItem::Absent)
                else {
                    unreachable!();
                };
                *slot = MaybePendingContextItem::Complete(ContextItem::ToolCall(call));
            }
            (
                MaybePendingContextItem::Pending(PendingContextItem::Message(message)),
                ContextItemEvent::MessageDelta {
                    content_part_index,
                    delta,
                },
            ) => {
                if content_part_index < message.content.len() {
                    let ContentPart::Text { text } = &mut message.content[content_part_index];
                    text.push_str(&delta);
                } else if content_part_index == message.content.len() {
                    message.content.push(ContentPart::Text { text: delta });
                } else {
                    anyhow::bail!("invalid transition");
                }
            }
            (
                slot @ MaybePendingContextItem::Pending(PendingContextItem::Message(_)),
                ContextItemEvent::MessageFinish {},
            ) => {
                let MaybePendingContextItem::Pending(PendingContextItem::Message(message)) =
                    std::mem::replace(slot, MaybePendingContextItem::Absent)
                else {
                    unreachable!();
                };
                *slot = MaybePendingContextItem::Complete(ContextItem::Message(message));
            }
            (
                MaybePendingContextItem::Pending(PendingContextItem::Reasoning { summary, .. }),
                ContextItemEvent::ReasoningItemSummaryDelta {
                    summary_index,
                    delta,
                },
            ) => {
                if summary_index < summary.len() {
                    summary[summary_index].push_str(&delta);
                } else if summary_index == summary.len() {
                    summary.push(delta);
                } else {
                    anyhow::bail!("invalid transition");
                }
            }
            (
                MaybePendingContextItem::Pending(PendingContextItem::Reasoning { content, .. }),
                ContextItemEvent::ReasoningItemContentDelta { delta },
            ) => {
                content.get_or_insert_with(String::new).push_str(&delta);
            }
            (
                slot @ MaybePendingContextItem::Pending(PendingContextItem::Reasoning { .. }),
                ContextItemEvent::ReasoningFinish {},
            ) => {
                let MaybePendingContextItem::Pending(PendingContextItem::Reasoning {
                    content,
                    summary,
                }) = std::mem::replace(slot, MaybePendingContextItem::Absent)
                else {
                    unreachable!();
                };
                *slot = MaybePendingContextItem::Complete(ContextItem::Reasoning(ReasoningItem {
                    kind: ReasoningItemKind::RawReasoning {
                        content: content.unwrap_or_default(),
                    },
                    summary,
                }));
            }
            (
                slot @ MaybePendingContextItem::Pending(PendingContextItem::Reasoning { .. }),
                ContextItemEvent::ReasoningEncryptedFinish { encrypted_content },
            ) => {
                let MaybePendingContextItem::Pending(PendingContextItem::Reasoning {
                    summary, ..
                }) = std::mem::replace(slot, MaybePendingContextItem::Absent)
                else {
                    unreachable!();
                };
                *slot = MaybePendingContextItem::Complete(ContextItem::Reasoning(ReasoningItem {
                    kind: ReasoningItemKind::Encrypted(encrypted_content),
                    summary,
                }));
            }
            (
                slot @ MaybePendingContextItem::Pending(PendingContextItem::CompactionStarted),
                ContextItemEvent::CompactionFinish { payload },
            ) => {
                *slot = MaybePendingContextItem::Complete(ContextItem::Compaction(payload));
            }
            (
                slot @ MaybePendingContextItem::Absent,
                ContextItemEvent::UnknownFinish { payload },
            ) => {
                *slot = MaybePendingContextItem::Complete(ContextItem::Unknown(payload));
            }
            _ => {
                anyhow::bail!("invalid transition");
            }
        }

        Ok(())
    }

    /// Consume the streamed response, asserting it is structurally complete:
    /// every slot must have been filled and finished. Returns an error if any
    /// slot is still `Absent` (a gap in the index sequence) or `Pending` (an
    /// item that never received its `*Finish` event).
    pub fn finish(self) -> anyhow::Result<Vec<ContextItem>> {
        self.items
            .into_iter()
            .enumerate()
            .map(|(index, slot)| match slot {
                MaybePendingContextItem::Complete(item) => Ok(item),
                MaybePendingContextItem::Absent => {
                    anyhow::bail!("response is incomplete: gap at index {index}")
                }
                MaybePendingContextItem::Pending(pending) => {
                    anyhow::bail!(
                        "response is incomplete: item at index {index} never finished: {pending:?}"
                    )
                }
            })
            .collect()
    }
}

// we could use bump allocation if we are having performance issues
#[derive(Clone, Debug, PartialEq)]
pub enum ContextItemEvent {
    Add(PendingContextItem),
    ToolCallArgumentDelta {
        delta: String,
    },
    ToolCallFinish {},
    MessageDelta {
        content_part_index: usize,
        delta: String,
    },
    MessageFinish {},
    ReasoningItemSummaryDelta {
        summary_index: usize,
        delta: String,
    },
    ReasoningItemContentDelta {
        delta: String,
    },
    ReasoningFinish {},
    ReasoningEncryptedFinish {
        encrypted_content: OpaqueProviderData,
    },
    CompactionFinish {
        payload: OpaqueProviderData,
    },
    UnknownFinish {
        payload: OpaqueProviderData,
    },
}

#[derive(Debug)]
pub enum InferenceUpdate {
    ContextItem {
        index: usize,
        event: ContextItemEvent,
    },
    Finished {
        usage: Option<TokenUsage>,
        provider_response_id: Option<String>,
    },
    /// You should see RequestSent soon
    TemporaryFailure {
        error: anyhow::Error,
        retrying_at: Instant,
    },
    /// We have sent the request
    RequestSent,
    /// server has started sending tokens
    StreamingStarted,
    /// turn has failed due to some reason
    /// you shouldn't retry, that is already done internally
    Failed {
        // TODO: specific error message if needed if future
        error: anyhow::Error,
    },
}

/// A single inference session, driven by one owner at a time as a small
/// pollable sub-actor — "a sub-actor you fold into your own `select!`".
///
/// You `request` a turn (cheap, synchronous) and then poll `run` for its
/// updates. `run` is the single place the connection is driven: between events
/// it keeps the socket warm (pongs server pings, sends client pings), and with
/// no active request it keeps a warm socket alive and otherwise pends. So
/// keeping `run` in a `select!` arm gives keepalive for free, whether or not a
/// turn is in flight.
///
/// `run` is cancel-safe: its progress lives in `self`, so dropping the future
/// (because another `select!` arm fired) and calling it again loses nothing.
pub trait IInferenceSession: Send + 'static {
    /// Queue a turn, replacing any previously queued/active one. The work
    /// happens in `run`.
    fn request(&mut self, request: InferenceRequest);

    /// Drive the connection and return the next update for the active request.
    /// Pends (while keeping any warm socket alive) when no request is active.
    fn run(&mut self) -> BoxFuture<'_, InferenceUpdate>;

    /// Abort the active request and drop the now-indeterminate connection, so
    /// the next `request` reconnects from clean state.
    fn abort(&mut self);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl Message {
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

impl ToolResult {
    pub fn success(call_id: ToolCallId, content: impl Into<String>) -> Self {
        Self {
            call_id,
            tool_type: ToolType::Function,
            status: ToolResultStatus::Success,
            output: ToolOutput {
                content: content.into(),
            },
        }
    }

    pub fn error(call_id: ToolCallId, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            call_id,
            tool_type: ToolType::Function,
            status: ToolResultStatus::Error {
                message: message.clone(),
            },
            output: ToolOutput { content: message },
        }
    }

    pub fn cancelled(call_id: ToolCallId, reason: impl Into<String>) -> Self {
        Self {
            call_id,
            tool_type: ToolType::Function,
            status: ToolResultStatus::Cancelled {
                reason: reason.into(),
            },
            output: ToolOutput {
                content: String::new(),
            },
        }
    }

    pub fn rendered_output(&self) -> String {
        match &self.status {
            ToolResultStatus::Success => self.output.content.clone(),
            ToolResultStatus::Error { message } => {
                format!("error: {message}\n\n{}", self.output.content)
            }
            ToolResultStatus::Cancelled { reason } => format!("cancelled: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_id_converts_and_borrows_as_str() {
        let from_str = ToolCallId::try_from("call-1").unwrap();
        assert_eq!(from_str.as_ref(), "call-1");

        let arc: Arc<str> = Arc::from("call-2");
        let from_arc = ToolCallId::try_from(arc.clone()).unwrap();
        assert_eq!(from_arc.as_str(), "call-2");
    }

    #[test]
    fn validated_string_type_rejects_invalid_characters() {
        let error = ToolName::try_from("bad name").unwrap_err();
        assert!(
            error.to_string().contains("invalid character"),
            "unexpected error: {error}"
        );
        assert!(ToolCallId::try_from("").is_err());
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

    #[test]
    fn pending_response_accumulates_message_events() {
        let mut response = PendingInferenceResponse { items: Vec::new() };

        response
            .apply(
                0,
                ContextItemEvent::Add(PendingContextItem::Message(Message {
                    role: Role::Assistant,
                    content: Vec::new(),
                    phase: None,
                })),
            )
            .unwrap();
        response
            .apply(
                0,
                ContextItemEvent::MessageDelta {
                    content_part_index: 0,
                    delta: "hel".to_owned(),
                },
            )
            .unwrap();
        response
            .apply(
                0,
                ContextItemEvent::MessageDelta {
                    content_part_index: 0,
                    delta: "lo".to_owned(),
                },
            )
            .unwrap();
        response
            .apply(0, ContextItemEvent::MessageFinish {})
            .unwrap();

        assert_eq!(
            response.items,
            vec![MaybePendingContextItem::Complete(ContextItem::Message(
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::Text {
                        text: "hello".to_owned()
                    }],
                    phase: None,
                }
            ))]
        );
    }

    #[test]
    fn pending_response_rejects_out_of_order_message_delta() {
        let mut response = PendingInferenceResponse { items: Vec::new() };

        response
            .apply(
                0,
                ContextItemEvent::Add(PendingContextItem::Message(Message {
                    role: Role::Assistant,
                    content: Vec::new(),
                    phase: None,
                })),
            )
            .unwrap();

        let error = response
            .apply(
                0,
                ContextItemEvent::MessageDelta {
                    content_part_index: 1,
                    delta: "late".to_owned(),
                },
            )
            .unwrap_err();

        assert_eq!(error.to_string(), "invalid transition");
    }

    #[test]
    fn pending_response_accumulates_unknown_provider_item() {
        let payload = OpaqueProviderData {
            tag: "provider.event".to_owned(),
            data: "{}".to_owned(),
        };
        let mut response = PendingInferenceResponse { items: Vec::new() };

        response
            .apply(
                0,
                ContextItemEvent::UnknownFinish {
                    payload: payload.clone(),
                },
            )
            .unwrap();

        assert_eq!(
            response.items,
            vec![MaybePendingContextItem::Complete(ContextItem::Unknown(
                payload
            ))]
        );
    }

    #[test]
    fn finish_collects_completed_items_in_order() {
        let mut response = PendingInferenceResponse { items: Vec::new() };

        response
            .apply(
                0,
                ContextItemEvent::Add(PendingContextItem::Message(Message {
                    role: Role::Assistant,
                    content: Vec::new(),
                    phase: None,
                })),
            )
            .unwrap();
        response
            .apply(
                0,
                ContextItemEvent::MessageDelta {
                    content_part_index: 0,
                    delta: "hi".to_owned(),
                },
            )
            .unwrap();
        response
            .apply(0, ContextItemEvent::MessageFinish {})
            .unwrap();

        response
            .apply(
                1,
                ContextItemEvent::Add(PendingContextItem::ToolCall(ToolCall {
                    id: ToolCallId::try_from("call-1").unwrap(),
                    name: ToolName::try_from("shell").unwrap(),
                    tool_type: ToolType::Function,
                    arguments: String::new(),
                })),
            )
            .unwrap();
        response
            .apply(
                1,
                ContextItemEvent::ToolCallArgumentDelta {
                    delta: "{\"cmd\":\"ls\"}".to_owned(),
                },
            )
            .unwrap();
        response
            .apply(1, ContextItemEvent::ToolCallFinish {})
            .unwrap();

        let items = response.finish().unwrap();
        assert_eq!(
            items,
            vec![
                ContextItem::Message(Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::Text {
                        text: "hi".to_owned()
                    }],
                    phase: None,
                }),
                ContextItem::ToolCall(ToolCall {
                    id: ToolCallId::try_from("call-1").unwrap(),
                    name: ToolName::try_from("shell").unwrap(),
                    tool_type: ToolType::Function,
                    arguments: "{\"cmd\":\"ls\"}".to_owned(),
                }),
            ]
        );
    }

    #[test]
    fn finish_rejects_absent_gap() {
        let payload = OpaqueProviderData {
            tag: "provider.event".to_owned(),
            data: "{}".to_owned(),
        };
        let mut response = PendingInferenceResponse { items: Vec::new() };

        // Only index 1 is filled; index 0 stays `Absent`.
        response
            .apply(1, ContextItemEvent::UnknownFinish { payload })
            .unwrap();

        let error = response.finish().unwrap_err();
        assert_eq!(error.to_string(), "response is incomplete: gap at index 0");
    }

    #[test]
    fn finish_rejects_unfinished_pending_item() {
        let mut response = PendingInferenceResponse { items: Vec::new() };

        response
            .apply(
                0,
                ContextItemEvent::Add(PendingContextItem::Message(Message {
                    role: Role::Assistant,
                    content: Vec::new(),
                    phase: None,
                })),
            )
            .unwrap();

        // No `MessageFinish`, so the slot is still `Pending`.
        let error = response.finish().unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("response is incomplete: item at index 0 never finished"),
            "unexpected error: {error}"
        );
    }
}
