//! Small shared vocabulary for rho crates.
//!
//! This crate intentionally avoids owning agent policy. Harnesses, providers,
//! tools, and stores can add their own richer types around these basics.

use std::sync::Arc;
use std::time::Instant;

use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod append_string;
mod util;

pub use crate::append_string::{AStr, AppendString, Diff};
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

validated_string_type!(
    pub ProviderResponseId,
    crate::util::validate_identifier
);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ContextBlock {
    // intentionally limiting to prevent misuse
    UserMessage {
        content: Vec<ContentPart>,
    },
    ToolResults {
        results: Vec<ToolResult>,
    },
    InferenceResponse {
        items: Vec<InferenceResponseItem>,
        provider_response_id: Option<ProviderResponseId>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum InferenceResponseItem {
    AssistantMessage {
        content: Vec<ContentPart>,
        phase: Option<MessagePhase>,
    },
    ToolCall {
        id: ToolCallId,
        name: ToolName,
        tool_type: ToolType,
        // arbitrary could be json!
        arguments: String,
    },
    EncryptedReasoning {
        payload: OpaqueProviderData,
        summary: Vec<String>,
    },
    RawReasoning {
        content: String,
        summary: Vec<String>,
    },
    Compaction(OpaqueProviderData),
    Unknown(OpaqueProviderData),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Pack, Unpack)]
pub enum ContentPart {
    Text { text: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    Commentary,
    FinalAnswer,
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
    pub id: ToolCallId,
    pub name: ToolName,
    pub tool_type: ToolType,
    // arbitrary could be json!
    pub arguments: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum ToolOutputStatus {
    Success,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct ToolOutput {
    /// Sent to the model verbatim.
    pub output: Arc<String>,
    /// Harness/UI metadata only; not included in the provider wire payload.
    pub status: ToolOutputStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ToolResult {
    /// Matches the [`ToolCall`] this result answers.
    pub call_id: ToolCallId,
    /// Wire shape for replaying this result to the provider.
    pub tool_type: ToolType,
    pub body: ToolOutput,
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

/// A provider item captured whole so it can be replayed byte-for-byte. It is
/// never appended to (unlike streamed text), so the fields are `Arc<str>` for
/// O(1) cloning rather than [`AStr`]; the same value rides along in both
/// pending items and finalized history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueProviderData {
    pub tag: Arc<str>,
    pub data: Arc<str>,
}

impl senax_encoder::Encoder for OpaqueProviderData {
    fn encode(&self, writer: &mut bytes::BytesMut) -> senax_encoder::Result<()> {
        (self.tag.to_string(), self.data.to_string()).encode(writer)
    }

    fn is_default(&self) -> bool {
        self.tag.is_empty() && self.data.is_empty()
    }
}

impl senax_encoder::Decoder for OpaqueProviderData {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        let (tag, data) = <(String, String)>::decode(reader)?;
        Ok(Self {
            tag: std::sync::Arc::from(tag),
            data: std::sync::Arc::from(data),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InferenceRequest {
    // todo: add instruction
    // arc is used to avoid cloning context blocks too much between requests
    pub input: Vec<Arc<ContextBlock>>,
    pub tools: Arc<[ToolSpec]>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum StreamingContextItem {
    AssistantMessage {
        content: Vec<AStr>,
        phase: Option<MessagePhase>,
    },
    ToolCall {
        id: ToolCallId,
        name: ToolName,
        tool_type: ToolType,
        arguments: AStr,
    },
    RawReasoning {
        content: AStr,
        summary: Vec<AStr>,
    },
    EncryptedReasoning {
        payload: OpaqueProviderData,
        summary: Vec<AStr>,
    },
    Compaction(Option<OpaqueProviderData>),
    Unknown(OpaqueProviderData),
}

impl StreamingContextItem {
    pub fn to_context_item(&self) -> anyhow::Result<InferenceResponseItem> {
        Ok(match self {
            StreamingContextItem::AssistantMessage { content, phase } => {
                InferenceResponseItem::AssistantMessage {
                    content: content
                        .iter()
                        .map(|text| ContentPart::Text {
                            text: text.to_string(),
                        })
                        .collect(),
                    phase: *phase,
                }
            }
            StreamingContextItem::ToolCall {
                id,
                name,
                tool_type,
                arguments,
            } => InferenceResponseItem::ToolCall {
                id: id.clone(),
                name: name.clone(),
                tool_type: *tool_type,
                arguments: arguments.to_string(),
            },
            StreamingContextItem::RawReasoning { content, summary } => {
                InferenceResponseItem::RawReasoning {
                    content: content.to_string(),
                    summary: summary.iter().map(AStr::to_string).collect(),
                }
            }
            StreamingContextItem::EncryptedReasoning { payload, summary } => {
                InferenceResponseItem::EncryptedReasoning {
                    payload: payload.clone(),
                    summary: summary.iter().map(AStr::to_string).collect(),
                }
            }
            StreamingContextItem::Compaction(Some(payload)) => {
                InferenceResponseItem::Compaction(payload.clone())
            }
            StreamingContextItem::Unknown(payload) => {
                InferenceResponseItem::Unknown(payload.clone())
            }
            StreamingContextItem::Compaction(None) => {
                anyhow::bail!("compaction never finished")
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum StreamingContextItemState {
    Empty,
    Pending(StreamingContextItem),
    Finished(StreamingContextItem),
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct PendingInferenceResponse {
    pub items: Vec<StreamingContextItemState>,
}

impl PendingInferenceResponse {
    pub fn apply(&mut self, index: usize, event: ContextItemEvent) {
        match event {
            ContextItemEvent::Update(item) => {
                if self.items.len() <= index {
                    self.items
                        .resize(index + 1, StreamingContextItemState::Empty);
                }
                self.items[index] = StreamingContextItemState::Pending(item);
            }
            // The slot already holds the final snapshot from the preceding
            // `Update`; `Finish` just promotes it to `Finished`.
            ContextItemEvent::Finish => {
                if let Some(slot @ StreamingContextItemState::Pending(_)) =
                    self.items.get_mut(index)
                {
                    let StreamingContextItemState::Pending(item) =
                        std::mem::replace(slot, StreamingContextItemState::Empty)
                    else {
                        unreachable!()
                    };
                    *slot = StreamingContextItemState::Finished(item);
                }
            }
        }
    }

    pub fn finish(&self) -> anyhow::Result<Vec<InferenceResponseItem>> {
        self.items
            .iter()
            .enumerate()
            .map(|(index, slot)| match slot {
                StreamingContextItemState::Empty => {
                    anyhow::bail!("response is incomplete: gap at index {index}")
                }
                StreamingContextItemState::Pending(_) => {
                    anyhow::bail!("response is incomplete: item at index {index} never finished")
                }
                StreamingContextItemState::Finished(item) => item.to_context_item(),
            })
            .collect()
    }
}

/// A change to the pending item at some `output_index`.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextItemEvent {
    /// The pending item advanced; carries its latest snapshot. The producer
    /// accumulates into [`AppendString`] buffers and emits the whole refreshed
    /// snapshot, so a single `Update` covers both first-sight and every
    /// subsequent delta.
    Update(StreamingContextItem),
    /// No more updates will arrive for this index — the value is whatever the
    /// last `Update` delivered. Lets a consumer act on a finished item (e.g.
    /// dispatch a tool call) before the whole response completes.
    Finish,
}

#[derive(Debug, Clone)]
pub enum InferenceEvent {
    ContextItem {
        index: usize,
        event: ContextItemEvent,
    },
    Finished {
        usage: Option<TokenUsage>,
        provider_response_id: Option<ProviderResponseId>,
    },
    /// You should see RequestSent soon
    TemporaryFailure {
        error: Arc<anyhow::Error>,
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
        error: Arc<anyhow::Error>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

/// Concatenate the text parts of a message.
pub fn text_content(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("")
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

    fn pending_message(text: &str) -> StreamingContextItem {
        StreamingContextItem::AssistantMessage {
            content: vec![AStr::from(text)],
            phase: None,
        }
    }

    fn message_item(text: &str) -> InferenceResponseItem {
        InferenceResponseItem::AssistantMessage {
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
        }
    }

    #[test]
    fn apply_keeps_latest_snapshot_per_index() {
        let mut response = PendingInferenceResponse::default();

        // Each streamed snapshot supersedes the previous one for that index;
        // there is no accumulation here, the producer already did it.
        response.apply(0, ContextItemEvent::Update(pending_message("hel")));
        response.apply(0, ContextItemEvent::Update(pending_message("hello")));
        response.apply(0, ContextItemEvent::Finish);

        assert_eq!(response.finish().unwrap(), vec![message_item("hello")]);
    }

    #[test]
    fn finish_event_marks_slot_without_disturbing_snapshot() {
        let mut response = PendingInferenceResponse::default();

        response.apply(0, ContextItemEvent::Update(pending_message("done")));
        assert_eq!(
            response.items[0],
            StreamingContextItemState::Pending(pending_message("done"))
        );

        // `Finish` promotes the slot but leaves the snapshot untouched.
        response.apply(0, ContextItemEvent::Finish);
        assert_eq!(
            response.items[0],
            StreamingContextItemState::Finished(pending_message("done"))
        );

        assert_eq!(response.finish().unwrap(), vec![message_item("done")]);
    }

    #[test]
    fn finalizes_unknown_provider_item() {
        let payload = OpaqueProviderData {
            tag: "provider.event".into(),
            data: "{}".into(),
        };
        let mut response = PendingInferenceResponse::default();

        response.apply(
            0,
            ContextItemEvent::Update(StreamingContextItem::Unknown(payload.clone())),
        );
        response.apply(0, ContextItemEvent::Finish);

        assert_eq!(
            response.finish().unwrap(),
            vec![InferenceResponseItem::Unknown(payload)]
        );
    }

    #[test]
    fn finish_collects_items_in_index_order() {
        let mut response = PendingInferenceResponse::default();

        response.apply(0, ContextItemEvent::Update(pending_message("hi")));
        response.apply(
            1,
            ContextItemEvent::Update(StreamingContextItem::ToolCall {
                id: ToolCallId::try_from("call-1").unwrap(),
                name: ToolName::try_from("shell").unwrap(),
                tool_type: ToolType::Function,
                arguments: AStr::from(r#"{"cmd":"ls"}"#),
            }),
        );
        response.apply(0, ContextItemEvent::Finish);
        response.apply(1, ContextItemEvent::Finish);

        assert_eq!(
            response.finish().unwrap(),
            vec![
                message_item("hi"),
                InferenceResponseItem::ToolCall {
                    id: ToolCallId::try_from("call-1").unwrap(),
                    name: ToolName::try_from("shell").unwrap(),
                    tool_type: ToolType::Function,
                    arguments: r#"{"cmd":"ls"}"#.to_owned(),
                },
            ]
        );
    }

    #[test]
    fn finish_rejects_item_without_finish() {
        let mut response = PendingInferenceResponse::default();

        // Updated but never signalled finished.
        response.apply(0, ContextItemEvent::Update(pending_message("partial")));

        let error = response.finish().unwrap_err();
        assert_eq!(
            error.to_string(),
            "response is incomplete: item at index 0 never finished"
        );
    }

    #[test]
    fn finish_rejects_payloadless_compaction() {
        let mut response = PendingInferenceResponse::default();

        // Compaction was marked finished but its payload never arrived.
        response.apply(
            0,
            ContextItemEvent::Update(StreamingContextItem::Compaction(None)),
        );
        response.apply(0, ContextItemEvent::Finish);

        let error = response.finish().unwrap_err();
        assert_eq!(error.to_string(), "compaction never finished");
    }

    #[test]
    fn finish_rejects_absent_gap() {
        let payload = OpaqueProviderData {
            tag: "provider.event".into(),
            data: "{}".into(),
        };
        let mut response = PendingInferenceResponse::default();

        // Only index 1 is filled; index 0 stays a gap.
        response.apply(
            1,
            ContextItemEvent::Update(StreamingContextItem::Unknown(payload)),
        );

        let error = response.finish().unwrap_err();
        assert_eq!(error.to_string(), "response is incomplete: gap at index 0");
    }
}
