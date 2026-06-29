use std::sync::Arc;

use rho_agent::{AgentState, AgentStateKind};
use rho_core::{
    AStr, ContextBlock, Diff as AStrDiffKind, InferenceResponseItem, PendingInferenceResponse,
    StreamingContextItem, StreamingContextItemState, ToolOutputStatus, text_content,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Sender-side remote UI-state encoder.
///
/// This projects runtime agent state into a deliberately smaller UI shape
/// before diffing, so the wire protocol does not inherit provider replay data,
/// tool schemas, or full tool outputs. It diffs from the previous runtime state
/// so append-only history and streaming text updates stay cheap.
#[derive(Default)]
pub struct AgentRemoteEncoder {
    last_agent: Option<AgentState>,
    last_sent: Option<UiAgentState>,
}

impl AgentRemoteEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(&mut self, current: AgentState) -> AgentRemoteFrame {
        let frame = match (&self.last_agent, &self.last_sent) {
            (Some(previous_agent), Some(previous_sent)) => {
                let keep_agent_blocks = common_agent_block_prefix_len(previous_agent, &current);
                let keep_blocks = previous_agent.blocks[..keep_agent_blocks]
                    .iter()
                    .map(|block| ui_blocks(block).len())
                    .sum();
                let blocks = current.blocks[keep_agent_blocks..]
                    .iter()
                    .flat_map(|block| ui_blocks(block))
                    .map(|block| UiBlockAppend::from_previous_pending(block, previous_sent))
                    .collect();
                AgentRemoteFrame::Diff {
                    keep_blocks,
                    blocks,
                    status: (ui_status(&previous_agent.kind) != ui_status(&current.kind))
                        .then(|| ui_status(&current.kind)),
                    pending_response: diff_pending_response_kind(
                        &previous_agent.kind,
                        &current.kind,
                    ),
                }
            }
            _ => AgentRemoteFrame::Snapshot(UiAgentState::from_agent_state(&current)),
        };

        let mut sent = self
            .last_sent
            .take()
            .unwrap_or_else(|| UiAgentState::from_agent_state(&current));
        frame.clone().apply_diff(&mut sent);
        self.last_agent = Some(current);
        self.last_sent = Some(sent);
        frame
    }

    /// Forget connection-local diff state. The next frame will be a full
    /// snapshot.
    pub fn reset(&mut self) {
        self.last_agent = None;
        self.last_sent = None;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentRemoteFrame {
    Snapshot(UiAgentState),
    Diff {
        /// Number of already-rendered blocks the receiver should keep before
        /// appending `blocks`.
        keep_blocks: usize,
        blocks: Vec<UiBlockAppend>,
        status: Option<UiAgentStatus>,
        pending_response: UiPendingResponseDiff,
    },
}

impl AgentRemoteFrame {
    pub fn apply_diff(self, state: &mut UiAgentState) {
        match self {
            Self::Snapshot(snapshot) => *state = snapshot,
            Self::Diff {
                keep_blocks,
                blocks,
                status,
                pending_response,
            } => {
                state.blocks.truncate(keep_blocks);
                state.blocks.extend(
                    blocks
                        .into_iter()
                        .filter_map(|block| block.into_block(&state.pending_response)),
                );
                if let Some(status) = status {
                    state.status = status;
                }
                pending_response.apply_diff(&mut state.pending_response);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiAgentState {
    pub blocks: Vec<UiBlock>,
    pub status: UiAgentStatus,
    pub pending_response: Vec<UiStreamingItem>,
}

impl UiAgentState {
    fn from_agent_state(state: &AgentState) -> Self {
        Self {
            blocks: state
                .blocks
                .iter()
                .flat_map(|block| ui_blocks(block))
                .collect(),
            status: ui_status(&state.kind),
            pending_response: ui_pending_response(&state.kind),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiBlock {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        status: UiToolStatus,
    },
    Notice {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiBlockAppend {
    Block(UiBlock),
    Pending { index: usize },
}

impl UiBlockAppend {
    fn from_previous_pending(block: UiBlock, previous: &UiAgentState) -> Self {
        previous
            .pending_response
            .iter()
            .position(|item| ui_block_from_pending(item).as_ref() == Some(&block))
            .map(|index| Self::Pending { index })
            .unwrap_or(Self::Block(block))
    }

    fn into_block(self, pending_response: &[UiStreamingItem]) -> Option<UiBlock> {
        match self {
            Self::Block(block) => Some(block),
            Self::Pending { index } => pending_response.get(index).and_then(ui_block_from_pending),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiAgentStatus {
    Idle,
    Streaming,
    ToolCalling { results: Vec<UiToolResult> },
    UnfinishedTurn { outstanding_calls: usize },
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiPendingResponseDiff {
    Replace(Vec<UiStreamingItem>),
    Items(Vec<UiStreamingItemUpdate>),
}

impl UiPendingResponseDiff {
    fn apply_diff(self, pending_response: &mut Vec<UiStreamingItem>) {
        match self {
            Self::Replace(replacement) => *pending_response = replacement,
            Self::Items(items) => {
                for item in items {
                    item.apply_diff(pending_response);
                }
                while pending_response
                    .last()
                    .is_some_and(UiStreamingItem::is_empty_placeholder)
                {
                    pending_response.pop();
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiStreamingItem {
    AssistantMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Notice {
        text: String,
    },
}

impl UiStreamingItem {
    fn empty_placeholder() -> Self {
        Self::Notice {
            text: String::new(),
        }
    }

    fn is_empty_placeholder(&self) -> bool {
        matches!(self, Self::Notice { text } if text.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiStreamingItemUpdate {
    pub index: usize,
    pub item: UiStreamingItemDiff,
}

impl UiStreamingItemUpdate {
    fn apply_diff(self, items: &mut Vec<UiStreamingItem>) {
        if items.len() <= self.index {
            items.resize_with(self.index + 1, UiStreamingItem::empty_placeholder);
        }
        self.item.apply_diff(&mut items[self.index]);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiStreamingItemDiff {
    Remove,
    Replace(UiStreamingItem),
    AssistantMessage { text: UiTextDiff },
    Reasoning { text: UiTextDiff },
    ToolCall { arguments: UiTextDiff },
}

impl UiStreamingItemDiff {
    fn apply_diff(self, item: &mut UiStreamingItem) {
        match self {
            Self::Remove => *item = UiStreamingItem::empty_placeholder(),
            Self::Replace(replacement) => *item = replacement,
            Self::AssistantMessage { text } => {
                if !matches!(item, UiStreamingItem::AssistantMessage { .. }) {
                    *item = UiStreamingItem::AssistantMessage {
                        text: String::new(),
                    };
                }
                let UiStreamingItem::AssistantMessage { text: current } = item else {
                    unreachable!("assistant item was just installed");
                };
                *current = text.apply_to(current);
            }
            Self::Reasoning { text } => {
                if !matches!(item, UiStreamingItem::Reasoning { .. }) {
                    *item = UiStreamingItem::Reasoning {
                        text: String::new(),
                    };
                }
                let UiStreamingItem::Reasoning { text: current } = item else {
                    unreachable!("reasoning item was just installed");
                };
                *current = text.apply_to(current);
            }
            Self::ToolCall { arguments } => {
                let UiStreamingItem::ToolCall {
                    arguments: current, ..
                } = item
                else {
                    *item = UiStreamingItem::ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    };
                    let UiStreamingItem::ToolCall {
                        arguments: current, ..
                    } = item
                    else {
                        unreachable!("tool item was just installed");
                    };
                    *current = arguments.apply_to("");
                    return;
                };
                *current = arguments.apply_to(current);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiTextDiff {
    pub keep_bytes: usize,
    pub value: String,
}

impl UiTextDiff {
    fn replace(value: impl ToString) -> Self {
        Self {
            keep_bytes: 0,
            value: value.to_string(),
        }
    }

    fn apply_to(self, previous: &str) -> String {
        let keep_bytes = self.keep_bytes.min(previous.len());
        let mut output = previous[..keep_bytes].to_owned();
        output.push_str(&self.value);
        output
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiToolResult {
    pub call_id: String,
    pub status: UiToolStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolStatus {
    Running,
    Success,
    Error,
    Cancelled,
}

impl From<ToolOutputStatus> for UiToolStatus {
    fn from(status: ToolOutputStatus) -> Self {
        match status {
            ToolOutputStatus::Success => Self::Success,
            ToolOutputStatus::Error => Self::Error,
            ToolOutputStatus::Cancelled => Self::Cancelled,
        }
    }
}

fn common_agent_block_prefix_len(previous: &AgentState, current: &AgentState) -> usize {
    previous
        .blocks
        .iter()
        .zip(&current.blocks)
        .take_while(|(previous, current)| Arc::ptr_eq(previous, current) || previous == current)
        .count()
}

fn ui_blocks(block: &ContextBlock) -> Vec<UiBlock> {
    match block {
        ContextBlock::UserMessage { content } => {
            vec![UiBlock::UserMessage {
                text: text_content(content),
            }]
        }
        ContextBlock::InferenceResponse { items, .. } => items
            .iter()
            .filter_map(ui_block_from_response_item)
            .collect(),
        ContextBlock::ToolResults { results } => results
            .iter()
            .map(|result| UiBlock::ToolCall {
                id: result.call_id.as_str().to_owned(),
                name: "tool".to_owned(),
                arguments: String::new(),
                status: result.body.status.into(),
            })
            .collect(),
    }
}

fn ui_block_from_response_item(item: &InferenceResponseItem) -> Option<UiBlock> {
    match item {
        InferenceResponseItem::AssistantMessage { content, .. } => {
            Some(UiBlock::AssistantMessage {
                text: text_content(content),
            })
        }
        InferenceResponseItem::RawReasoning { content, summary } => Some(UiBlock::Reasoning {
            text: reasoning_text(content, summary),
        }),
        InferenceResponseItem::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Some(UiBlock::ToolCall {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.clone(),
            status: UiToolStatus::Running,
        }),
        InferenceResponseItem::Compaction(_) => Some(UiBlock::Notice {
            text: "compacting context".to_owned(),
        }),
        InferenceResponseItem::EncryptedReasoning { summary, .. } => {
            (!summary.is_empty()).then(|| UiBlock::Reasoning {
                text: summary.join("\n"),
            })
        }
        InferenceResponseItem::Unknown(_) => None,
    }
}

fn ui_status(kind: &AgentStateKind) -> UiAgentStatus {
    match kind {
        AgentStateKind::ApiStreaming { .. } => UiAgentStatus::Streaming,
        AgentStateKind::ToolCalling { results } => UiAgentStatus::ToolCalling {
            results: results
                .iter()
                .map(|result| UiToolResult {
                    call_id: result.call_id.as_str().to_owned(),
                    status: result.body.status.into(),
                })
                .collect(),
        },
        AgentStateKind::UnfinishedTurn {
            outstanding_calls, ..
        } => UiAgentStatus::UnfinishedTurn {
            outstanding_calls: outstanding_calls.len(),
        },
        AgentStateKind::Error(error) => UiAgentStatus::Error {
            message: error.error.to_string(),
        },
        AgentStateKind::Idle => UiAgentStatus::Idle,
    }
}

fn ui_pending_response(kind: &AgentStateKind) -> Vec<UiStreamingItem> {
    match kind {
        AgentStateKind::ApiStreaming {
            pending_response, ..
        } => pending_response
            .items
            .iter()
            .filter_map(ui_streaming_item)
            .collect(),
        AgentStateKind::Idle
        | AgentStateKind::ToolCalling { .. }
        | AgentStateKind::UnfinishedTurn { .. }
        | AgentStateKind::Error(_) => Vec::new(),
    }
}

fn diff_pending_response_kind(
    previous: &AgentStateKind,
    current: &AgentStateKind,
) -> UiPendingResponseDiff {
    match (previous, current) {
        (
            AgentStateKind::ApiStreaming {
                pending_response: previous,
                ..
            },
            AgentStateKind::ApiStreaming {
                pending_response: current,
                ..
            },
        ) => UiPendingResponseDiff::Items(diff_pending_response(previous, current)),
        _ => UiPendingResponseDiff::Replace(ui_pending_response(current)),
    }
}

fn diff_pending_response(
    previous: &PendingInferenceResponse,
    current: &PendingInferenceResponse,
) -> Vec<UiStreamingItemUpdate> {
    let max_len = previous.items.len().max(current.items.len());
    (0..max_len)
        .filter_map(|index| {
            let previous = previous
                .items
                .get(index)
                .unwrap_or(&StreamingContextItemState::Empty);
            let current = current
                .items
                .get(index)
                .unwrap_or(&StreamingContextItemState::Empty);
            (previous != current).then(|| UiStreamingItemUpdate {
                index,
                item: diff_streaming_item_state(previous, current),
            })
        })
        .collect()
}

fn diff_streaming_item_state(
    previous: &StreamingContextItemState,
    current: &StreamingContextItemState,
) -> UiStreamingItemDiff {
    match (previous, current) {
        (_, StreamingContextItemState::Empty) => UiStreamingItemDiff::Remove,
        (
            StreamingContextItemState::Pending(previous),
            StreamingContextItemState::Pending(current),
        )
        | (
            StreamingContextItemState::Finished(previous),
            StreamingContextItemState::Finished(current),
        )
        | (
            StreamingContextItemState::Pending(previous),
            StreamingContextItemState::Finished(current),
        ) => diff_streaming_item(previous, current),
        (_, StreamingContextItemState::Pending(current))
        | (_, StreamingContextItemState::Finished(current)) => UiStreamingItemDiff::Replace(
            ui_streaming_item_from_item(current).unwrap_or_else(UiStreamingItem::empty_placeholder),
        ),
    }
}

fn diff_streaming_item(
    previous: &StreamingContextItem,
    current: &StreamingContextItem,
) -> UiStreamingItemDiff {
    match (previous, current) {
        (
            StreamingContextItem::AssistantMessage {
                content: previous_content,
                phase: previous_phase,
            },
            StreamingContextItem::AssistantMessage {
                content: current_content,
                phase: current_phase,
            },
        ) if previous_phase == current_phase => UiStreamingItemDiff::AssistantMessage {
            text: diff_joined_astr(previous_content, current_content),
        },
        (
            StreamingContextItem::RawReasoning {
                content: previous_content,
                summary: previous_summary,
            },
            StreamingContextItem::RawReasoning {
                content: current_content,
                summary: current_summary,
            },
        ) => UiStreamingItemDiff::Reasoning {
            text: diff_text(
                &reasoning_text(previous_content, previous_summary),
                &reasoning_text(current_content, current_summary),
            ),
        },
        (
            StreamingContextItem::ToolCall {
                id: previous_id,
                name: previous_name,
                tool_type: previous_tool_type,
                arguments: previous_arguments,
            },
            StreamingContextItem::ToolCall {
                id: current_id,
                name: current_name,
                tool_type: current_tool_type,
                arguments: current_arguments,
            },
        ) if previous_id == current_id
            && previous_name == current_name
            && previous_tool_type == current_tool_type =>
        {
            UiStreamingItemDiff::ToolCall {
                arguments: diff_astr(previous_arguments, current_arguments),
            }
        }
        _ => UiStreamingItemDiff::Replace(
            ui_streaming_item_from_item(current).unwrap_or_else(UiStreamingItem::empty_placeholder),
        ),
    }
}

fn ui_streaming_item(item: &StreamingContextItemState) -> Option<UiStreamingItem> {
    match item {
        StreamingContextItemState::Pending(item) | StreamingContextItemState::Finished(item) => {
            ui_streaming_item_from_item(item)
        }
        StreamingContextItemState::Empty => None,
    }
}

fn ui_streaming_item_from_item(item: &StreamingContextItem) -> Option<UiStreamingItem> {
    match item {
        StreamingContextItem::AssistantMessage { content, .. } => {
            Some(UiStreamingItem::AssistantMessage {
                text: content.iter().map(ToString::to_string).collect(),
            })
        }
        StreamingContextItem::RawReasoning { content, summary } => {
            Some(UiStreamingItem::Reasoning {
                text: reasoning_text(content, summary),
            })
        }
        StreamingContextItem::EncryptedReasoning { summary, .. } => {
            (!summary.is_empty()).then(|| UiStreamingItem::Reasoning {
                text: summary
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            })
        }
        StreamingContextItem::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Some(UiStreamingItem::ToolCall {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.to_string(),
        }),
        StreamingContextItem::Compaction(_) => Some(UiStreamingItem::Notice {
            text: "compacting context".to_owned(),
        }),
        StreamingContextItem::Unknown(_) => None,
    }
}

fn ui_block_from_pending(item: &UiStreamingItem) -> Option<UiBlock> {
    match item {
        UiStreamingItem::AssistantMessage { text } => {
            Some(UiBlock::AssistantMessage { text: text.clone() })
        }
        UiStreamingItem::Reasoning { text } => Some(UiBlock::Reasoning { text: text.clone() }),
        UiStreamingItem::ToolCall {
            id,
            name,
            arguments,
        } => Some(UiBlock::ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
            status: UiToolStatus::Running,
        }),
        UiStreamingItem::Notice { text } => Some(UiBlock::Notice { text: text.clone() }),
    }
}

fn reasoning_text(content: &impl ToString, summary: &[impl ToString]) -> String {
    if summary.is_empty() {
        content.to_string()
    } else {
        summary
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn diff_joined_astr(previous: &[AStr], current: &[AStr]) -> UiTextDiff {
    diff_text(
        &previous.iter().map(ToString::to_string).collect::<String>(),
        &current.iter().map(ToString::to_string).collect::<String>(),
    )
}

fn diff_astr(previous: &AStr, current: &AStr) -> UiTextDiff {
    match previous.diff(current) {
        AStrDiffKind::LeftIsPrefix => UiTextDiff {
            keep_bytes: previous.len(),
            value: current.to_string()[previous.len()..].to_owned(),
        },
        AStrDiffKind::Unrelated | AStrDiffKind::RightIsPrefix => UiTextDiff::replace(current),
    }
}

fn diff_text(previous: &str, current: &str) -> UiTextDiff {
    if let Some(suffix) = current.strip_prefix(previous) {
        UiTextDiff {
            keep_bytes: previous.len(),
            value: suffix.to_owned(),
        }
    } else {
        UiTextDiff::replace(current)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use rho_agent::FailedInferenceResponse;
    use rho_core::{AStr, ContentPart, PendingInferenceResponse};

    use super::*;

    fn streaming_state(text: &str) -> AgentState {
        AgentState {
            blocks: Vec::new(),
            tool_specs: Arc::from([]),
            kind: AgentStateKind::ApiStreaming {
                pending_response: PendingInferenceResponse {
                    items: vec![StreamingContextItemState::Pending(
                        StreamingContextItem::AssistantMessage {
                            content: vec![AStr::from(text)],
                            phase: None,
                        },
                    )],
                },
                previous_attempt: None,
            },
        }
    }

    fn retry_streaming_state(text: &str) -> AgentState {
        let mut state = streaming_state(text);
        let AgentStateKind::ApiStreaming {
            previous_attempt, ..
        } = &mut state.kind
        else {
            unreachable!()
        };
        *previous_attempt = Some(FailedInferenceResponse {
            partial_response: PendingInferenceResponse::default(),
            attempt_count: NonZeroU64::MIN,
            error: Arc::new("temporary failure".to_owned()),
        });
        state
    }

    fn finished_state(text: &str) -> AgentState {
        AgentState {
            blocks: vec![Arc::new(ContextBlock::InferenceResponse {
                items: vec![InferenceResponseItem::AssistantMessage {
                    content: vec![ContentPart::Text {
                        text: text.to_owned(),
                    }],
                    phase: None,
                }],
                provider_response_id: None,
            })],
            tool_specs: Arc::from([]),
            kind: AgentStateKind::Idle,
        }
    }

    #[test]
    fn streaming_update_sends_text_suffix_diff() {
        let mut encoder = AgentRemoteEncoder::new();
        let AgentRemoteFrame::Snapshot(mut receiver) = encoder.encode(streaming_state("hel"))
        else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(streaming_state("hello"));
        let AgentRemoteFrame::Diff {
            status,
            pending_response,
            ..
        } = &frame
        else {
            panic!("second frame should be a diff");
        };
        assert_eq!(*status, None);
        let UiPendingResponseDiff::Items(items) = pending_response else {
            panic!("streaming state should use item diff");
        };
        assert_eq!(
            items,
            &[UiStreamingItemUpdate {
                index: 0,
                item: UiStreamingItemDiff::AssistantMessage {
                    text: UiTextDiff {
                        keep_bytes: 3,
                        value: "lo".to_owned(),
                    },
                },
            }]
        );

        frame.apply_diff(&mut receiver);
        assert_eq!(
            receiver,
            UiAgentState::from_agent_state(&streaming_state("hello"))
        );
    }

    #[test]
    fn tiny_streaming_update_has_small_wire_frame() {
        let mut encoder = AgentRemoteEncoder::new();
        let _ = encoder.encode(streaming_state("hel"));
        let frame = encoder.encode(streaming_state("hello"));
        let bytes = crate::protocol_frame_bytes(&crate::ServerMessage::Agent(frame)).unwrap();
        assert!(bytes.len() < 40, "tiny frame was {} bytes", bytes.len());
    }

    #[test]
    fn finishing_streamed_text_appends_pending_block_by_reference() {
        let mut encoder = AgentRemoteEncoder::new();
        let AgentRemoteFrame::Snapshot(mut receiver) = encoder.encode(streaming_state("hello"))
        else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(finished_state("hello"));
        let AgentRemoteFrame::Diff {
            blocks,
            pending_response,
            status,
            ..
        } = &frame
        else {
            panic!("second frame should be a diff");
        };
        assert_eq!(blocks, &[UiBlockAppend::Pending { index: 0 }]);
        assert_eq!(*status, Some(UiAgentStatus::Idle));
        assert_eq!(
            *pending_response,
            UiPendingResponseDiff::Replace(Vec::new())
        );

        let bytes =
            crate::protocol_frame_bytes(&crate::ServerMessage::Agent(frame.clone())).unwrap();
        assert!(
            bytes.len() < 20,
            "finish frame resent too much data: {} bytes",
            bytes.len()
        );
        frame.apply_diff(&mut receiver);
        assert_eq!(
            receiver,
            UiAgentState::from_agent_state(&finished_state("hello"))
        );
    }

    #[test]
    fn retry_streaming_updates_still_use_item_diffs() {
        let mut encoder = AgentRemoteEncoder::new();
        let _ = encoder.encode(retry_streaming_state("hel"));
        let frame = encoder.encode(retry_streaming_state("hello"));
        let AgentRemoteFrame::Diff {
            pending_response, ..
        } = frame
        else {
            panic!("second frame should be a diff");
        };
        assert_eq!(
            pending_response,
            UiPendingResponseDiff::Items(vec![UiStreamingItemUpdate {
                index: 0,
                item: UiStreamingItemDiff::AssistantMessage {
                    text: UiTextDiff {
                        keep_bytes: 3,
                        value: "lo".to_owned(),
                    },
                },
            }])
        );
    }
}
