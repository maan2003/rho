use std::sync::Arc;

use rho_agent::{AgentState, AgentStateKind, MessageDelivery, QueuedItem, QueuedItemKind};
use rho_core::{
    ApplyPatchMetadata, ContextBlock, InferenceResponseItem, MessagePhase, StreamingContextItem,
    StreamingContextItemState, ToolFileStatus, ToolOutputStatus, ToolResultMetadata, UnixMs,
    text_content,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Sender-side remote UI-state encoder.
///
/// This projects runtime agent state into a deliberately smaller UI shape
/// before diffing, so the wire protocol does not inherit provider replay data,
/// tool schemas, or full tool outputs. Diffing the projected states keeps
/// append-only history and streaming text updates cheap.
#[derive(Default)]
pub struct AgentRemoteEncoder {
    last_sent: Option<UiAgentState>,
}

impl AgentRemoteEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(&mut self, current: AgentState) -> AgentRemoteFrame {
        let current = UiAgentState::from_agent_state(&current);
        let frame = match &self.last_sent {
            Some(previous) => AgentRemoteFrame::Diff {
                blocks: diff_blocks(&previous.blocks, &current.blocks),
                status: (previous.status != current.status).then_some(current.status),
                context_used: (previous.context_used != current.context_used)
                    .then_some(current.context_used),
            },
            None => AgentRemoteFrame::Snapshot(current.clone()),
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

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentRemoteFrame {
    Snapshot(UiAgentState),
    Diff {
        blocks: UiBlocksDiff,
        status: Option<UiAgentStatus>,
        /// `None` means unchanged; `Some(value)` overwrites.
        context_used: Option<Option<u64>>,
    },
}

impl AgentRemoteFrame {
    pub fn apply_diff(self, state: &mut UiAgentState) {
        match self {
            Self::Snapshot(snapshot) => *state = snapshot,
            Self::Diff {
                blocks,
                status,
                context_used,
            } => {
                blocks.apply_diff(&mut state.blocks);
                if let Some(status) = status {
                    state.status = status;
                }
                if let Some(context_used) = context_used {
                    state.context_used = context_used;
                }
            }
        }
    }
}

/// One agent's UI state: a flat block list plus a coarse status.
///
/// The list is the whole truth; every change arrives as an explicit indexed
/// update, so receivers key caches off change indexes. No block is immutable:
/// tool blocks keep receiving status/timing/preview updates while their calls
/// run, the in-flight response's blocks stream and may be replaced or
/// removed, and compaction may rewrite or truncate history. When a response
/// settles into context its blocks keep their indexes, so a block that
/// projects identically before and after costs nothing on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiAgentState {
    pub blocks: Vec<UiBlock>,
    pub status: UiAgentStatus,
    /// Tokens occupying the model's context window after the latest
    /// response; `None` until the agent's first response.
    pub context_used: Option<u64>,
}

impl UiAgentState {
    fn from_agent_state(state: &AgentState) -> Self {
        let mut blocks = ui_blocks(&state.blocks);
        merge_active_tool_state(&mut blocks, &state.kind);
        blocks.extend(in_flight_blocks(&state.kind));
        blocks.extend(state.queued_inputs.iter().map(|input| match input {
            QueuedItem {
                kind:
                    QueuedItemKind::UserMessage {
                        sender, content, ..
                    },
                delivery,
            } => UiBlock::QueuedMessage {
                text: text_content(content),
                delivery: *delivery,
                sender: match sender {
                    rho_agent::MessageSender::User => None,
                    rho_agent::MessageSender::Agent { id } => Some(*id),
                },
            },
            QueuedItem {
                kind: QueuedItemKind::Compaction,
                ..
            } => UiBlock::Notice {
                text: "compacting context".to_owned(),
            },
            QueuedItem {
                kind: QueuedItemKind::ToolUpdate(update),
                ..
            } => UiBlock::Notice {
                text: format!("tool update: {}", update.output),
            },
        }));
        Self {
            blocks,
            status: ui_status(&state.kind),
            context_used: state.context_used,
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
        phase: Option<UiMessagePhase>,
    },
    Reasoning {
        text: String,
    },
    Tool(UiTool),
    Notice {
        text: String,
    },
    /// A message waiting in the agent's queue; becomes a `UserMessage` (or
    /// `AgentMessage`) block at delivery. Always trails the transcript.
    QueuedMessage {
        text: String,
        delivery: MessageDelivery,
        /// The sending agent; `None` for the user.
        sender: Option<crate::AgentId>,
    },
    /// A delivered message from another agent.
    AgentMessage {
        /// The sending agent.
        sender: crate::AgentId,
        text: String,
    },
}

/// Purely index-based block-list diff: truncation to the new length (when
/// shorter), then per-index updates in ascending order. An update whose index
/// is one past the end appends.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiBlocksDiff {
    pub truncate_to: Option<usize>,
    pub updates: Vec<UiBlockUpdate>,
}

impl UiBlocksDiff {
    fn apply_diff(self, blocks: &mut Vec<UiBlock>) {
        if let Some(truncate_to) = self.truncate_to {
            blocks.truncate(truncate_to);
        }
        for update in self.updates {
            update.apply_diff(blocks);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiBlockUpdate {
    pub index: usize,
    pub block: UiBlockDiff,
}

impl UiBlockUpdate {
    fn apply_diff(self, blocks: &mut Vec<UiBlock>) {
        match blocks.get_mut(self.index) {
            Some(block) => self.block.apply_diff(block),
            None => {
                // Appends arrive as in-order updates just past the end; fill
                // any gap a malformed sender leaves so application stays
                // total.
                while blocks.len() < self.index {
                    blocks.push(UiBlock::Notice {
                        text: String::new(),
                    });
                }
                let mut block = UiBlock::Notice {
                    text: String::new(),
                };
                self.block.apply_diff(&mut block);
                blocks.push(block);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiBlockDiff {
    Replace(UiBlock),
    Tool(UiToolDiff),
    /// Text extension of an assistant message whose phase is unchanged.
    AssistantText(UiTextDiff),
    /// Text extension of a reasoning block.
    ReasoningText(UiTextDiff),
}

impl UiBlockDiff {
    fn apply_diff(self, block: &mut UiBlock) {
        match self {
            Self::Replace(replacement) => *block = replacement,
            Self::Tool(diff) => {
                if let UiBlock::Tool(tool) = block {
                    diff.apply_diff(tool);
                } else {
                    *block = UiBlock::Tool(diff.into_tool());
                }
            }
            Self::AssistantText(diff) => {
                if let UiBlock::AssistantMessage { text, .. } = block {
                    *text = diff.apply_to(text);
                } else {
                    *block = UiBlock::AssistantMessage {
                        text: diff.apply_to(""),
                        phase: None,
                    };
                }
            }
            Self::ReasoningText(diff) => {
                if let UiBlock::Reasoning { text } = block {
                    *text = diff.apply_to(text);
                } else {
                    *block = UiBlock::Reasoning {
                        text: diff.apply_to(""),
                    };
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiAgentStatus {
    Idle,
    Streaming,
    ToolCalling {
        /// Deadline of the batch's armed `wait` call, if one is parked
        /// until mail arrives or the wall clock passes it.
        waiting: Option<UnixMs>,
    },
    UnfinishedTurn {
        outstanding_calls: usize,
    },
    /// The turn failed permanently; the error text is the trailing unsealed
    /// [`UiBlock::Notice`].
    Error,
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
pub enum UiToolMetadata {
    ApplyPatch(UiApplyPatchMetadata),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiApplyPatchMetadata {
    pub changes: Vec<UiToolFileChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiToolFileChange {
    pub path: String,
    pub status: UiToolFileStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolFileStatus {
    Added,
    Modified,
    Deleted,
    Moved,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiTool {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub preview: Option<String>,
    pub status: UiToolStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<UnixMs>,
    pub finished_at: Option<UnixMs>,
    pub metadata: Option<UiToolMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiToolDiff {
    pub id: String,
    pub name: String,
    pub arguments: Option<UiTextDiff>,
    pub preview: Option<Option<String>>,
    pub status: Option<UiToolStatus>,
    pub output: Option<Option<String>>,
    pub error: Option<Option<String>>,
    pub started_at: Option<Option<UnixMs>>,
    pub finished_at: Option<Option<UnixMs>>,
    pub metadata: Option<Option<UiToolMetadata>>,
}

impl UiToolDiff {
    fn from_changed(previous: &UiTool, current: &UiTool) -> Self {
        Self {
            id: current.id.clone(),
            name: current.name.clone(),
            arguments: (previous.arguments != current.arguments)
                .then(|| diff_text(&previous.arguments, &current.arguments)),
            preview: (previous.preview != current.preview).then(|| current.preview.clone()),
            status: (previous.status != current.status).then_some(current.status),
            output: (previous.output != current.output).then(|| current.output.clone()),
            error: (previous.error != current.error).then(|| current.error.clone()),
            started_at: (previous.started_at != current.started_at).then_some(current.started_at),
            finished_at: (previous.finished_at != current.finished_at)
                .then_some(current.finished_at),
            metadata: (previous.metadata != current.metadata).then(|| current.metadata.clone()),
        }
    }

    fn apply_diff(self, tool: &mut UiTool) {
        tool.id = self.id;
        tool.name = self.name;
        if let Some(arguments) = self.arguments {
            tool.arguments = arguments.apply_to(&tool.arguments);
        }
        if let Some(preview) = self.preview {
            tool.preview = preview;
        }
        if let Some(status) = self.status {
            tool.status = status;
        }
        if let Some(output) = self.output {
            tool.output = output;
        }
        if let Some(error) = self.error {
            tool.error = error;
        }
        if let Some(started_at) = self.started_at {
            tool.started_at = started_at;
        }
        if let Some(finished_at) = self.finished_at {
            tool.finished_at = finished_at;
        }
        if let Some(metadata) = self.metadata {
            tool.metadata = metadata;
        }
    }

    fn into_tool(self) -> UiTool {
        UiTool {
            id: self.id,
            name: self.name,
            arguments: self
                .arguments
                .map(|arguments| arguments.apply_to(""))
                .unwrap_or_default(),
            preview: self.preview.flatten(),
            status: self.status.unwrap_or(UiToolStatus::Running),
            output: self.output.flatten(),
            error: self.error.flatten(),
            started_at: self.started_at.flatten(),
            finished_at: self.finished_at.flatten(),
            metadata: self.metadata.flatten(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolStatus {
    Running,
    Success,
    Error,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiMessagePhase {
    Commentary,
    FinalAnswer,
}

impl From<MessagePhase> for UiMessagePhase {
    fn from(phase: MessagePhase) -> Self {
        match phase {
            MessagePhase::Commentary => Self::Commentary,
            MessagePhase::FinalAnswer => Self::FinalAnswer,
        }
    }
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

fn ui_tool_metadata(metadata: &ToolResultMetadata) -> UiToolMetadata {
    match metadata {
        ToolResultMetadata::ApplyPatch(metadata) => {
            UiToolMetadata::ApplyPatch(ui_apply_patch_metadata(metadata))
        }
    }
}

fn ui_apply_patch_metadata(metadata: &ApplyPatchMetadata) -> UiApplyPatchMetadata {
    UiApplyPatchMetadata {
        changes: metadata
            .changes
            .iter()
            .map(|change| UiToolFileChange {
                path: change.path.clone(),
                status: ui_tool_file_status(change.status),
            })
            .collect(),
    }
}

fn ui_tool_file_status(status: ToolFileStatus) -> UiToolFileStatus {
    match status {
        ToolFileStatus::Added => UiToolFileStatus::Added,
        ToolFileStatus::Modified => UiToolFileStatus::Modified,
        ToolFileStatus::Deleted => UiToolFileStatus::Deleted,
        ToolFileStatus::Moved => UiToolFileStatus::Moved,
    }
}

fn diff_blocks(previous: &[UiBlock], current: &[UiBlock]) -> UiBlocksDiff {
    let common_len = previous.len().min(current.len());
    let mut updates = previous[..common_len]
        .iter()
        .zip(&current[..common_len])
        .enumerate()
        .filter(|(_, (previous, current))| previous != current)
        .map(|(index, (previous, current))| UiBlockUpdate {
            index,
            block: diff_block(previous, current),
        })
        .collect::<Vec<_>>();
    updates.extend(
        current[common_len..]
            .iter()
            .enumerate()
            .map(|(offset, block)| UiBlockUpdate {
                index: common_len + offset,
                block: UiBlockDiff::Replace(block.clone()),
            }),
    );
    UiBlocksDiff {
        truncate_to: (current.len() < previous.len()).then_some(current.len()),
        updates,
    }
}

fn diff_block(previous: &UiBlock, current: &UiBlock) -> UiBlockDiff {
    match (previous, current) {
        (UiBlock::Tool(previous), UiBlock::Tool(current))
            if previous.id == current.id && previous.name == current.name =>
        {
            UiBlockDiff::Tool(UiToolDiff::from_changed(previous, current))
        }
        (
            UiBlock::AssistantMessage {
                text: previous_text,
                phase: previous_phase,
            },
            UiBlock::AssistantMessage {
                text: current_text,
                phase: current_phase,
            },
        ) if previous_phase == current_phase => {
            UiBlockDiff::AssistantText(diff_text(previous_text, current_text))
        }
        (
            UiBlock::Reasoning {
                text: previous_text,
            },
            UiBlock::Reasoning { text: current_text },
        ) => UiBlockDiff::ReasoningText(diff_text(previous_text, current_text)),
        _ => UiBlockDiff::Replace(current.clone()),
    }
}

fn ui_blocks(blocks: &[Arc<ContextBlock>]) -> Vec<UiBlock> {
    let mut ui_blocks = Vec::new();
    for block in blocks {
        match block.as_ref() {
            ContextBlock::UserMessage { sender, content } => match sender {
                rho_agent::MessageSender::User => ui_blocks.push(UiBlock::UserMessage {
                    text: text_content(content),
                }),
                rho_agent::MessageSender::Agent { id } => ui_blocks.push(UiBlock::AgentMessage {
                    sender: *id,
                    text: text_content(content),
                }),
            },
            ContextBlock::CompactionTrigger => ui_blocks.push(UiBlock::Notice {
                text: "compacting context".to_owned(),
            }),
            ContextBlock::InferenceResponse { items, .. } => {
                ui_blocks.extend(items.iter().filter_map(ui_block_from_response_item));
            }
            ContextBlock::ToolUpdate(update) => ui_blocks.push(UiBlock::Notice {
                text: format!("tool update: {}", update.output),
            }),
            ContextBlock::ToolResults { results } => {
                for result in results {
                    if let Some(UiBlock::Tool(tool)) = ui_blocks.iter_mut().rev().find(|block| {
                        matches!(block, UiBlock::Tool(tool) if tool.id == result.call_id.as_str())
                    }) {
                        tool.status = result.body.status.into();
                        tool.started_at = Some(result.started_at);
                        tool.finished_at = Some(result.finished_at);
                        tool.metadata = result.metadata.as_ref().map(ui_tool_metadata);
                    }
                }
            }
        }
    }
    ui_blocks
}

fn merge_active_tool_state(blocks: &mut [UiBlock], kind: &AgentStateKind) {
    let AgentStateKind::ToolCalling {
        previews, results, ..
    } = kind
    else {
        return;
    };

    for preview in previews.values() {
        if let Some(tool) = find_tool_block_mut(blocks, preview.call.id.as_str()) {
            tool.name = preview.call.name.as_str().to_owned();
            tool.arguments = preview.call.arguments.clone();
            tool.status = UiToolStatus::Running;
            tool.started_at = Some(preview.started_at);
            tool.finished_at = None;
        }
    }

    for result in results {
        if let Some(tool) = find_tool_block_mut(blocks, result.call_id.as_str()) {
            tool.status = result.body.status.into();
            tool.started_at = Some(result.started_at);
            tool.finished_at = Some(result.finished_at);
            tool.metadata = result.metadata.as_ref().map(ui_tool_metadata);
        }
    }
}

fn find_tool_block_mut<'a>(blocks: &'a mut [UiBlock], id: &str) -> Option<&'a mut UiTool> {
    blocks.iter_mut().rev().find_map(|block| match block {
        UiBlock::Tool(tool) if tool.id == id => Some(tool),
        _ => None,
    })
}

fn ui_block_from_response_item(item: &InferenceResponseItem) -> Option<UiBlock> {
    match item {
        InferenceResponseItem::AssistantMessage { content, phase, .. } => {
            Some(UiBlock::AssistantMessage {
                text: text_content(content),
                phase: phase.map(Into::into),
            })
        }
        InferenceResponseItem::RawReasoning {
            content, summary, ..
        } => Some(UiBlock::Reasoning {
            text: reasoning_text(content, summary),
        }),
        InferenceResponseItem::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Some(UiBlock::Tool(UiTool {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.clone(),
            preview: None,
            status: UiToolStatus::Running,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            metadata: None,
        })),
        InferenceResponseItem::Compaction { .. } => Some(UiBlock::Notice {
            text: "compacting context".to_owned(),
        }),
        InferenceResponseItem::EncryptedReasoning { summary, .. } => {
            (!summary.is_empty()).then(|| UiBlock::Reasoning {
                text: summary.join("\n"),
            })
        }
        InferenceResponseItem::Unknown { .. } => None,
    }
}

fn ui_status(kind: &AgentStateKind) -> UiAgentStatus {
    match kind {
        AgentStateKind::ApiStreaming { .. } => UiAgentStatus::Streaming,
        AgentStateKind::ToolCalling { waiting, .. } => UiAgentStatus::ToolCalling {
            waiting: waiting.as_ref().map(|wait| wait.until),
        },
        AgentStateKind::UnfinishedTurn {
            outstanding_calls, ..
        } => UiAgentStatus::UnfinishedTurn {
            outstanding_calls: outstanding_calls.len(),
        },
        AgentStateKind::Error(_) => UiAgentStatus::Error,
        AgentStateKind::Idle => UiAgentStatus::Idle,
    }
}

/// The in-flight tail of the block list: the response being streamed, or the
/// partial response plus an error notice after a permanent failure.
fn in_flight_blocks(kind: &AgentStateKind) -> Vec<UiBlock> {
    match kind {
        AgentStateKind::ApiStreaming {
            pending_response, ..
        } => pending_response
            .items
            .iter()
            .filter_map(streaming_block)
            .collect(),
        AgentStateKind::Error(failure) => {
            let mut blocks = failure
                .partial_response
                .items
                .iter()
                .filter_map(streaming_block)
                .collect::<Vec<_>>();
            blocks.push(UiBlock::Notice {
                text: format!("agent error: {}", failure.error),
            });
            blocks
        }
        AgentStateKind::Idle
        | AgentStateKind::ToolCalling { .. }
        | AgentStateKind::UnfinishedTurn { .. } => Vec::new(),
    }
}

fn streaming_block(item: &StreamingContextItemState) -> Option<UiBlock> {
    match item {
        StreamingContextItemState::Pending(item) | StreamingContextItemState::Finished(item) => {
            block_from_streaming_item(item)
        }
        StreamingContextItemState::Empty => None,
    }
}

fn block_from_streaming_item(item: &StreamingContextItem) -> Option<UiBlock> {
    match item {
        StreamingContextItem::AssistantMessage { content, phase, .. } => {
            Some(UiBlock::AssistantMessage {
                text: content.iter().map(ToString::to_string).collect(),
                phase: phase.map(Into::into),
            })
        }
        StreamingContextItem::RawReasoning {
            content, summary, ..
        } => Some(UiBlock::Reasoning {
            text: reasoning_text(content, summary),
        }),
        StreamingContextItem::EncryptedReasoning { summary, .. } => {
            (!summary.is_empty()).then(|| UiBlock::Reasoning {
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
        } => Some(UiBlock::Tool(UiTool {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.to_string(),
            preview: None,
            status: UiToolStatus::Running,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            metadata: None,
        })),
        StreamingContextItem::Compaction { .. } => Some(UiBlock::Notice {
            text: "compacting context".to_owned(),
        }),
        StreamingContextItem::Unknown { .. } => None,
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
    use std::collections::BTreeMap;
    use std::num::NonZeroU64;

    use rho_agent::{FailedInferenceResponse, ToolPreview, ToolPreviewMetadata};
    // `register_senax_tagged!` names the trait and its registry entry type
    // unqualified, so both must be imported from the declaring crate.
    use rho_core::{
        __SenaxProviderSpecificDataEntry, AStr, ApplyPatchMetadata, ContentPart,
        PendingInferenceResponse, ProviderSpecificData, ToolCall, ToolCallId, ToolFileChange,
        ToolFileStatus, ToolName, ToolOutput, ToolResult, ToolType,
    };
    use senax_encoder::{Decode, Encode};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct UiTestProviderSpecificData {
        item_id: String,
    }

    senax_encoder::register_senax_tagged!(
        trait = ProviderSpecificData,
        type = UiTestProviderSpecificData,
        tag = "rho-ui-proto-test.provider-data",
    );

    fn test_provider_specific_data() -> Box<dyn ProviderSpecificData> {
        Box::new(UiTestProviderSpecificData {
            item_id: "ui_item_1".to_owned(),
        })
    }

    fn streaming_state(text: &str) -> AgentState {
        AgentState {
            blocks: Vec::new(),
            tool_specs: Arc::from([]),
            system_prompt: Arc::from(""),
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::ApiStreaming {
                pending_response: PendingInferenceResponse {
                    items: vec![StreamingContextItemState::Pending(
                        StreamingContextItem::AssistantMessage {
                            provider_specific: test_provider_specific_data(),
                            content: vec![AStr::from(text)],
                            phase: None,
                        },
                    )],
                },
                previous_attempt: None,
            },
            context_used: None,
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

    fn error_state(partial_text: &str, message: &str) -> AgentState {
        AgentState {
            blocks: Vec::new(),
            tool_specs: Arc::from([]),
            system_prompt: Arc::from(""),
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::Error(FailedInferenceResponse {
                partial_response: PendingInferenceResponse {
                    items: vec![StreamingContextItemState::Pending(
                        StreamingContextItem::AssistantMessage {
                            provider_specific: test_provider_specific_data(),
                            content: vec![AStr::from(partial_text)],
                            phase: None,
                        },
                    )],
                },
                attempt_count: NonZeroU64::MIN,
                error: Arc::new(message.to_owned()),
            }),
            context_used: None,
        }
    }

    fn finished_state(text: &str) -> AgentState {
        AgentState {
            blocks: vec![Arc::new(ContextBlock::InferenceResponse {
                items: vec![InferenceResponseItem::AssistantMessage {
                    provider_specific: test_provider_specific_data(),
                    content: vec![ContentPart::Text {
                        text: text.to_owned(),
                    }],
                    phase: None,
                }],
                provider_response_id: None,
            })],
            tool_specs: Arc::from([]),
            system_prompt: Arc::from(""),
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
        }
    }

    fn finished_tool_state() -> AgentState {
        let call_id = ToolCallId::try_from("call-1").unwrap();
        AgentState {
            blocks: vec![
                Arc::new(ContextBlock::InferenceResponse {
                    items: vec![InferenceResponseItem::ToolCall {
                        provider_specific: test_provider_specific_data(),
                        id: call_id.clone(),
                        name: ToolName::try_from("shell_command").unwrap(),
                        tool_type: ToolType::Function,
                        arguments: r#"{"command":"printf hi"}"#.to_owned(),
                    }],
                    provider_response_id: None,
                }),
                Arc::new(ContextBlock::ToolResults {
                    results: vec![ToolResult {
                        call_id,
                        tool_type: ToolType::Function,
                        body: ToolOutput {
                            output: Arc::new("hi".to_owned()),
                            status: ToolOutputStatus::Success,
                        },
                        started_at: UnixMs(1),
                        finished_at: UnixMs(3),
                        metadata: None,
                    }],
                }),
            ],
            tool_specs: Arc::from([]),
            system_prompt: Arc::from(""),
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
        }
    }

    fn tool_calling_state() -> AgentState {
        let call_id = ToolCallId::try_from("call-1").unwrap();
        let mut previews = BTreeMap::new();
        previews.insert(
            call_id.clone(),
            ToolPreview {
                call: ToolCall {
                    id: call_id,
                    name: ToolName::try_from("apply_patch").unwrap(),
                    tool_type: ToolType::Function,
                    arguments: "*** Begin Patch\n*** End Patch\n".to_owned(),
                },
                started_at: UnixMs(10),
                metadata: Some(ToolPreviewMetadata::ApplyPatch(ApplyPatchMetadata {
                    changes: vec![ToolFileChange {
                        path: "src/lib.rs".to_owned(),
                        status: ToolFileStatus::Modified,
                    }],
                })),
            },
        );
        AgentState {
            blocks: Vec::new(),
            tool_specs: Arc::from([]),
            system_prompt: Arc::from(""),
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::ToolCalling {
                previews,
                results: Vec::new(),
                waiting: None,
            },
            context_used: None,
        }
    }

    fn tool_calling_state_with_call_block() -> AgentState {
        let mut state = tool_calling_state();
        let AgentStateKind::ToolCalling { previews, .. } = &state.kind else {
            unreachable!()
        };
        let preview = previews.values().next().unwrap();
        state.blocks.push(Arc::new(ContextBlock::InferenceResponse {
            items: vec![InferenceResponseItem::ToolCall {
                provider_specific: test_provider_specific_data(),
                id: preview.call.id.clone(),
                name: preview.call.name.clone(),
                tool_type: preview.call.tool_type,
                arguments: String::new(),
            }],
            provider_response_id: None,
        }));
        state
    }

    #[test]
    fn streaming_update_sends_text_suffix_diff() {
        let mut encoder = AgentRemoteEncoder::new();
        let AgentRemoteFrame::Snapshot(mut receiver) = encoder.encode(streaming_state("hel"))
        else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(streaming_state("hello"));
        let AgentRemoteFrame::Diff { blocks, status, .. } = &frame else {
            panic!("second frame should be a diff");
        };
        assert_eq!(*status, None);
        assert_eq!(
            blocks.updates,
            [UiBlockUpdate {
                index: 0,
                block: UiBlockDiff::AssistantText(UiTextDiff {
                    keep_bytes: 3,
                    value: "lo".to_owned(),
                }),
            }]
        );

        frame.apply_diff(&mut receiver);
        assert_eq!(
            receiver,
            UiAgentState::from_agent_state(&streaming_state("hello"))
        );
    }

    #[test]
    fn agent_message_sender_preserves_agent_id() {
        let sender = crate::AgentId::from_counter(2, &crate::AgentIdDomain(0)).unwrap();
        let mut state = finished_state("");
        state.blocks = vec![Arc::new(ContextBlock::UserMessage {
            sender: rho_agent::MessageSender::Agent { id: sender },
            content: vec![ContentPart::Text {
                text: "hello".to_owned(),
            }],
        })];

        let state = UiAgentState::from_agent_state(&state);

        assert_eq!(
            state.blocks,
            [UiBlock::AgentMessage {
                sender,
                text: "hello".to_owned(),
            }]
        );
    }

    #[test]
    fn tiny_streaming_update_has_small_wire_frame() {
        let mut encoder = AgentRemoteEncoder::new();
        let _ = encoder.encode(streaming_state("hel"));
        let frame = encoder.encode(streaming_state("hello"));
        let bytes = crate::protocol_frame_bytes(&crate::ServerMessage::Agent {
            agent_id: crate::AgentId::from_counter(1, &crate::AgentIdDomain(0)).unwrap(),
            frame,
        })
        .unwrap();
        assert!(bytes.len() < 56, "tiny frame was {} bytes", bytes.len());
    }

    #[test]
    fn finishing_a_streamed_response_sends_only_a_status_change() {
        let mut encoder = AgentRemoteEncoder::new();
        let AgentRemoteFrame::Snapshot(mut receiver) = encoder.encode(streaming_state("hello"))
        else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(finished_state("hello"));
        let AgentRemoteFrame::Diff { blocks, status, .. } = &frame else {
            panic!("second frame should be a diff");
        };
        assert_eq!(
            blocks,
            &UiBlocksDiff {
                truncate_to: None,
                updates: Vec::new(),
            }
        );
        assert_eq!(*status, Some(UiAgentStatus::Idle));

        let bytes = crate::protocol_frame_bytes(&crate::ServerMessage::Agent {
            agent_id: crate::AgentId::from_counter(1, &crate::AgentIdDomain(0)).unwrap(),
            frame: frame.clone(),
        })
        .unwrap();
        assert!(
            bytes.len() < 36,
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
    fn snapshots_do_not_render_tool_result_placeholders() {
        let state = UiAgentState::from_agent_state(&finished_tool_state());
        assert_eq!(state.blocks.len(), 1);
        assert!(matches!(
            &state.blocks[0],
            UiBlock::Tool(UiTool {
                name,
                arguments,
                status: UiToolStatus::Success,
                output: None,
                error: None,
                started_at: Some(UnixMs(1)),
                finished_at: Some(UnixMs(3)),
                metadata: None,
                ..
            }) if name == "shell_command" && arguments.contains("printf hi")
        ));
    }

    #[test]
    fn tool_calling_maps_to_plain_status() {
        let state = UiAgentState::from_agent_state(&tool_calling_state());
        assert_eq!(state.status, UiAgentStatus::ToolCalling { waiting: None });
    }

    #[test]
    fn tool_calling_preview_updates_existing_tool_block() {
        let state = UiAgentState::from_agent_state(&tool_calling_state_with_call_block());
        assert!(matches!(
            state.blocks.as_slice(),
            [UiBlock::Tool(UiTool {
                name,
                arguments,
                status: UiToolStatus::Running,
                started_at: Some(UnixMs(10)),
                finished_at: None,
                ..
            })] if name == "apply_patch"
                && arguments == "*** Begin Patch\n*** End Patch\n"
        ));
    }

    #[test]
    fn tool_calling_preview_timing_diffs_existing_tool_block() {
        let mut encoder = AgentRemoteEncoder::new();
        let mut initial = tool_calling_state_with_call_block();
        let AgentStateKind::ToolCalling { previews, .. } = &mut initial.kind else {
            unreachable!()
        };
        previews.clear();
        let AgentRemoteFrame::Snapshot(mut receiver) = encoder.encode(initial) else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(tool_calling_state_with_call_block());
        let AgentRemoteFrame::Diff { blocks, .. } = &frame else {
            panic!("second frame should be a diff");
        };
        assert!(matches!(
            blocks.updates.as_slice(),
            [UiBlockUpdate {
                index: 0,
                block: UiBlockDiff::Tool(UiToolDiff {
                    arguments: Some(UiTextDiff {
                        keep_bytes: 0,
                        value,
                    }),
                    started_at: Some(Some(UnixMs(10))),
                    finished_at: None,
                    ..
                })
            }] if value == "*** Begin Patch\n*** End Patch\n"
        ));

        frame.apply_diff(&mut receiver);
        assert_eq!(
            receiver,
            UiAgentState::from_agent_state(&tool_calling_state_with_call_block())
        );
    }

    #[test]
    fn tool_result_updates_existing_tool_block() {
        let mut encoder = AgentRemoteEncoder::new();
        let mut running = finished_tool_state();
        running.blocks.pop();
        let AgentRemoteFrame::Snapshot(mut receiver) = encoder.encode(running) else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(finished_tool_state());
        let AgentRemoteFrame::Diff { blocks, .. } = &frame else {
            panic!("second frame should be a diff");
        };
        assert_eq!(blocks.truncate_to, None);
        assert!(matches!(
            blocks.updates.as_slice(),
            [UiBlockUpdate {
                index: 0,
                block: UiBlockDiff::Tool(UiToolDiff {
                    status: Some(UiToolStatus::Success),
                    output: None,
                    error: None,
                    started_at: Some(Some(UnixMs(1))),
                    finished_at: Some(Some(UnixMs(3))),
                    metadata: None,
                    ..
                })
            }]
        ));

        frame.apply_diff(&mut receiver);
        assert_eq!(
            receiver,
            UiAgentState::from_agent_state(&finished_tool_state())
        );
    }

    #[test]
    fn retry_streaming_updates_still_use_text_diffs() {
        let mut encoder = AgentRemoteEncoder::new();
        let _ = encoder.encode(retry_streaming_state("hel"));
        let frame = encoder.encode(retry_streaming_state("hello"));
        let AgentRemoteFrame::Diff { blocks, .. } = frame else {
            panic!("second frame should be a diff");
        };
        assert_eq!(
            blocks.updates,
            [UiBlockUpdate {
                index: 0,
                block: UiBlockDiff::AssistantText(UiTextDiff {
                    keep_bytes: 3,
                    value: "lo".to_owned(),
                }),
            }]
        );
    }

    #[test]
    fn error_state_keeps_partial_response_and_appends_notice() {
        let state = UiAgentState::from_agent_state(&error_state("partial answer", "quota"));
        assert_eq!(state.status, UiAgentStatus::Error);
        assert_eq!(
            state.blocks,
            [
                UiBlock::AssistantMessage {
                    text: "partial answer".to_owned(),
                    phase: None,
                },
                UiBlock::Notice {
                    text: "agent error: quota".to_owned(),
                },
            ]
        );
    }
}
