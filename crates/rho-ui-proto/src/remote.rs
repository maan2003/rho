use std::sync::Arc;

use rho_agent::{AgentState, AgentStateKind};
use rho_core::{
    AStr, ContextBlock, Diff as AStrDiffKind, MessagePhase, OpaqueProviderData,
    PendingInferenceResponse, StreamingContextItem, StreamingContextItemState,
};
use senax_encoder::{Decode, Encode};

/// Sender-side remote state encoder.
///
/// It keeps the last state sent on this connection so the caller can cheaply
/// send append-only updates when possible, and fall back to a full snapshot
/// when the shape is not obviously append-only.
#[derive(Default)]
pub struct AgentRemoteEncoder {
    last_sent: Option<AgentState>,
}

impl AgentRemoteEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(&mut self, current: AgentState) -> AgentRemoteFrame {
        let frame = match &self.last_sent {
            Some(previous) if same_tool_specs(previous, &current) => {
                let keep_blocks = common_block_prefix_len(previous, &current);
                AgentRemoteFrame::Diff {
                    keep_blocks,
                    blocks: current.blocks[keep_blocks..].to_vec(),
                    kind: diff_state_kind(&previous.kind, &current.kind),
                }
            }
            _ => AgentRemoteFrame::Snapshot(current.clone()),
        };
        self.last_sent = Some(current);
        frame
    }

    /// Forget connection-local diff state. The next frame will be a full
    /// snapshot.
    pub fn reset(&mut self) {
        self.last_sent = None;
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AgentRemoteFrame {
    Snapshot(AgentState),
    Diff {
        /// Number of already-rendered blocks the receiver should keep before
        /// appending `blocks`.
        keep_blocks: usize,
        blocks: Vec<Arc<ContextBlock>>,
        kind: AgentStateKindDiff,
    },
}

impl AgentRemoteFrame {
    pub fn apply_diff(self, state: &mut AgentState) {
        match self {
            Self::Snapshot(snapshot) => *state = snapshot,
            Self::Diff {
                keep_blocks,
                blocks,
                kind,
            } => {
                state.blocks.truncate(keep_blocks);
                state.blocks.extend(blocks);
                kind.apply_diff(&mut state.kind);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AgentStateKindDiff {
    Replace(AgentStateKind),
    PendingInference(PendingInferenceResponseDiff),
}

impl AgentStateKindDiff {
    pub fn apply_diff(self, kind: &mut AgentStateKind) {
        match self {
            Self::Replace(replacement) => *kind = replacement,
            Self::PendingInference(diff) => {
                let AgentStateKind::ApiStreaming {
                    pending_response, ..
                } = kind
                else {
                    panic!("pending inference diff applied outside ApiStreaming");
                };
                diff.apply_diff(pending_response);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum PendingInferenceResponseDiff {
    Replace(PendingInferenceResponse),
    Items(Vec<PendingInferenceItemDiff>),
}

impl PendingInferenceResponseDiff {
    pub fn apply_diff(self, pending_response: &mut PendingInferenceResponse) {
        match self {
            Self::Replace(replacement) => *pending_response = replacement,
            Self::Items(items) => {
                for item in items {
                    if pending_response.items.len() <= item.index {
                        pending_response
                            .items
                            .resize(item.index + 1, StreamingContextItemState::Empty);
                    }
                    item.state
                        .apply_diff(&mut pending_response.items[item.index]);
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct PendingInferenceItemDiff {
    pub index: usize,
    pub state: StreamingContextItemStateDiff,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum StreamingContextItemStateDiff {
    Empty,
    Pending(StreamingContextItemDiff),
    Finished(StreamingContextItemDiff),
}

impl StreamingContextItemStateDiff {
    pub fn apply_diff(self, state: &mut StreamingContextItemState) {
        match self {
            Self::Empty => *state = StreamingContextItemState::Empty,
            Self::Pending(diff) => {
                *state = StreamingContextItemState::Pending(diff.apply_to_state(state));
            }
            Self::Finished(diff) => {
                *state = StreamingContextItemState::Finished(diff.apply_to_state(state));
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum StreamingContextItemDiff {
    Replace(StreamingContextItem),
    AssistantMessage {
        content: Vec<AStrDiff>,
        phase: Option<MessagePhase>,
    },
    ToolCall {
        arguments: AStrDiff,
    },
    RawReasoning {
        content: AStrDiff,
        summary: Vec<AStrDiff>,
    },
    EncryptedReasoning {
        payload: OpaqueProviderData,
        summary: Vec<AStrDiff>,
    },
    Compaction(Option<OpaqueProviderData>),
    Unknown(OpaqueProviderData),
}

impl StreamingContextItemDiff {
    fn apply_to_state(self, state: &StreamingContextItemState) -> StreamingContextItem {
        match state {
            StreamingContextItemState::Pending(previous)
            | StreamingContextItemState::Finished(previous) => self.apply_diff(previous),
            StreamingContextItemState::Empty => match self {
                Self::Replace(item) => item,
                _ => panic!("non-replace streaming item diff applied to empty item"),
            },
        }
    }

    pub fn apply_diff(self, previous: &StreamingContextItem) -> StreamingContextItem {
        match self {
            Self::Replace(item) => item,
            Self::AssistantMessage { content, phase } => {
                let StreamingContextItem::AssistantMessage { .. } = previous else {
                    panic!("assistant message diff applied to different item kind");
                };
                StreamingContextItem::AssistantMessage {
                    content: content.into_iter().map(AStrDiff::apply_diff).collect(),
                    phase,
                }
            }
            Self::ToolCall { arguments } => {
                let StreamingContextItem::ToolCall {
                    id,
                    name,
                    tool_type,
                    ..
                } = previous
                else {
                    panic!("tool call diff applied to different item kind");
                };
                StreamingContextItem::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    tool_type: *tool_type,
                    arguments: arguments.apply_diff(),
                }
            }
            Self::RawReasoning { content, summary } => {
                let StreamingContextItem::RawReasoning { .. } = previous else {
                    panic!("raw reasoning diff applied to different item kind");
                };
                StreamingContextItem::RawReasoning {
                    content: content.apply_diff(),
                    summary: summary.into_iter().map(AStrDiff::apply_diff).collect(),
                }
            }
            Self::EncryptedReasoning { payload, summary } => {
                let StreamingContextItem::EncryptedReasoning { .. } = previous else {
                    panic!("encrypted reasoning diff applied to different item kind");
                };
                StreamingContextItem::EncryptedReasoning {
                    payload,
                    summary: summary.into_iter().map(AStrDiff::apply_diff).collect(),
                }
            }
            Self::Compaction(payload) => StreamingContextItem::Compaction(payload),
            Self::Unknown(payload) => StreamingContextItem::Unknown(payload),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AStrDiff {
    /// Number of bytes to keep from the receiver's previous string before
    /// appending `value[keep_bytes..]`.
    pub keep_bytes: usize,
    pub value: AStr,
}

impl AStrDiff {
    pub fn apply_diff(self) -> AStr {
        self.value
    }
}

fn same_tool_specs(previous: &AgentState, current: &AgentState) -> bool {
    Arc::ptr_eq(&previous.tool_specs, &current.tool_specs)
}

fn common_block_prefix_len(previous: &AgentState, current: &AgentState) -> usize {
    previous
        .blocks
        .iter()
        .zip(&current.blocks)
        .take_while(|(previous, current)| Arc::ptr_eq(previous, current) || previous == current)
        .count()
}

fn diff_state_kind(previous: &AgentStateKind, current: &AgentStateKind) -> AgentStateKindDiff {
    match (previous, current) {
        (
            AgentStateKind::ApiStreaming {
                pending_response: previous,
                previous_attempt: None,
            },
            AgentStateKind::ApiStreaming {
                pending_response: current,
                previous_attempt: None,
            },
        ) => AgentStateKindDiff::PendingInference(diff_pending_response(previous, current)),
        _ => AgentStateKindDiff::Replace(current.clone()),
    }
}

fn diff_pending_response(
    previous: &PendingInferenceResponse,
    current: &PendingInferenceResponse,
) -> PendingInferenceResponseDiff {
    let max_len = previous.items.len().max(current.items.len());
    let items = (0..max_len)
        .filter_map(|index| {
            let previous = previous
                .items
                .get(index)
                .unwrap_or(&StreamingContextItemState::Empty);
            let current = current
                .items
                .get(index)
                .unwrap_or(&StreamingContextItemState::Empty);
            (previous != current).then(|| PendingInferenceItemDiff {
                index,
                state: diff_streaming_item_state(previous, current),
            })
        })
        .collect();
    PendingInferenceResponseDiff::Items(items)
}

fn diff_streaming_item_state(
    previous: &StreamingContextItemState,
    current: &StreamingContextItemState,
) -> StreamingContextItemStateDiff {
    match (previous, current) {
        (_, StreamingContextItemState::Empty) => StreamingContextItemStateDiff::Empty,
        (
            StreamingContextItemState::Pending(previous),
            StreamingContextItemState::Pending(current),
        ) => StreamingContextItemStateDiff::Pending(diff_streaming_item(previous, current)),
        (
            StreamingContextItemState::Finished(previous),
            StreamingContextItemState::Finished(current),
        )
        | (
            StreamingContextItemState::Pending(previous),
            StreamingContextItemState::Finished(current),
        ) => StreamingContextItemStateDiff::Finished(diff_streaming_item(previous, current)),
        (_, StreamingContextItemState::Pending(current)) => StreamingContextItemStateDiff::Pending(
            StreamingContextItemDiff::Replace(current.clone()),
        ),
        (_, StreamingContextItemState::Finished(current)) => {
            StreamingContextItemStateDiff::Finished(StreamingContextItemDiff::Replace(
                current.clone(),
            ))
        }
    }
}

fn diff_streaming_item(
    previous: &StreamingContextItem,
    current: &StreamingContextItem,
) -> StreamingContextItemDiff {
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
        ) if previous_phase == current_phase => StreamingContextItemDiff::AssistantMessage {
            content: diff_astr_vec(previous_content, current_content),
            phase: *current_phase,
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
            StreamingContextItemDiff::ToolCall {
                arguments: diff_astr(previous_arguments, current_arguments),
            }
        }
        (
            StreamingContextItem::RawReasoning {
                content: previous_content,
                summary: previous_summary,
            },
            StreamingContextItem::RawReasoning {
                content: current_content,
                summary: current_summary,
            },
        ) => StreamingContextItemDiff::RawReasoning {
            content: diff_astr(previous_content, current_content),
            summary: diff_astr_vec(previous_summary, current_summary),
        },
        (
            StreamingContextItem::EncryptedReasoning { .. },
            StreamingContextItem::EncryptedReasoning { payload, summary },
        ) => StreamingContextItemDiff::EncryptedReasoning {
            payload: payload.clone(),
            summary: summary.iter().map(replace_astr).collect(),
        },
        (_, StreamingContextItem::Compaction(payload)) => {
            StreamingContextItemDiff::Compaction(payload.clone())
        }
        (_, StreamingContextItem::Unknown(payload)) => {
            StreamingContextItemDiff::Unknown(payload.clone())
        }
        _ => StreamingContextItemDiff::Replace(current.clone()),
    }
}

fn diff_astr_vec(previous: &[AStr], current: &[AStr]) -> Vec<AStrDiff> {
    current
        .iter()
        .enumerate()
        .map(|(index, value)| {
            previous.get(index).map_or_else(
                || replace_astr(value),
                |previous| diff_astr(previous, value),
            )
        })
        .collect()
}

fn diff_astr(previous: &AStr, current: &AStr) -> AStrDiff {
    match previous.diff(current) {
        AStrDiffKind::LeftIsPrefix => AStrDiff {
            keep_bytes: previous.len(),
            value: current.clone(),
        },
        AStrDiffKind::Unrelated | AStrDiffKind::RightIsPrefix => replace_astr(current),
    }
}

fn replace_astr(value: &AStr) -> AStrDiff {
    AStrDiff {
        keep_bytes: 0,
        value: value.clone(),
    }
}
