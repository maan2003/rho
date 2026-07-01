use std::sync::Arc;

use rho_agent::{AgentState, AgentStateKind, ToolPreviewMetadata};
use rho_core::{
    AStr, ApplyPatchMetadata, ContextBlock, Diff as AStrDiffKind, InferenceResponseItem,
    MessagePhase, PendingInferenceResponse, StreamingContextItem, StreamingContextItemState,
    ToolFileStatus, ToolOutputStatus, ToolResultMetadata, UnixMs, text_content,
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
        let current_sent = UiAgentState::from_agent_state(&current);
        let frame = match (&self.last_agent, &self.last_sent) {
            (Some(previous_agent), Some(previous_sent)) => AgentRemoteFrame::Diff {
                blocks: diff_blocks(previous_sent, &current_sent),
                status: (ui_status(&previous_agent.kind) != ui_status(&current.kind))
                    .then(|| ui_status(&current.kind)),
                pending_response: diff_pending_response_kind(&previous_agent.kind, &current.kind),
            },
            _ => AgentRemoteFrame::Snapshot(UiAgentState::from_agent_state(&current)),
        };

        let mut sent = self.last_sent.take().unwrap_or(current_sent);
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
        blocks: UiBlocksDiff,
        status: Option<UiAgentStatus>,
        pending_response: UiPendingResponseDiff,
    },
}

impl AgentRemoteFrame {
    pub fn apply_diff(self, state: &mut UiAgentState) {
        match self {
            Self::Snapshot(snapshot) => *state = snapshot,
            Self::Diff {
                blocks,
                status,
                pending_response,
            } => {
                blocks.apply_diff(&mut state.blocks, &state.pending_response);
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
            blocks: ui_blocks(&state.blocks),
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
        phase: Option<UiMessagePhase>,
    },
    Reasoning {
        text: String,
    },
    Tool(UiTool),
    Notice {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiBlockAppend {
    Block(UiBlock),
    Pending { index: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiBlocksDiff {
    pub updates: Vec<UiBlockUpdate>,
    pub truncate_to: Option<usize>,
    pub append: Vec<UiBlockAppend>,
}

impl UiBlocksDiff {
    fn apply_diff(self, blocks: &mut Vec<UiBlock>, pending_response: &[UiStreamingItem]) {
        for update in self.updates {
            update.apply_diff(blocks);
        }
        if let Some(truncate_to) = self.truncate_to {
            blocks.truncate(truncate_to);
        }
        blocks.extend(
            self.append
                .into_iter()
                .filter_map(|block| block.into_block(pending_response)),
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiBlockUpdate {
    pub index: usize,
    pub block: UiBlockDiff,
}

impl UiBlockUpdate {
    fn apply_diff(self, blocks: &mut [UiBlock]) {
        if let Some(block) = blocks.get_mut(self.index) {
            self.block.apply_diff(block);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiBlockDiff {
    Replace(UiBlock),
    Tool(UiToolDiff),
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
        }
    }
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
    ToolCalling {
        previews: Vec<UiToolPreview>,
        results: Vec<UiToolResult>,
    },
    UnfinishedTurn {
        outstanding_calls: usize,
    },
    Error {
        message: String,
    },
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
        phase: Option<UiMessagePhase>,
    },
    Reasoning {
        text: String,
    },
    Tool(UiTool),
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
    Tool { arguments: UiTextDiff },
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
                        phase: None,
                    };
                }
                let UiStreamingItem::AssistantMessage { text: current, .. } = item else {
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
            Self::Tool { arguments } => {
                let UiStreamingItem::Tool(UiTool {
                    arguments: current, ..
                }) = item
                else {
                    *item = UiStreamingItem::Tool(UiTool::empty());
                    let UiStreamingItem::Tool(UiTool {
                        arguments: current, ..
                    }) = item
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
    pub started_at: UnixMs,
    pub finished_at: UnixMs,
    pub metadata: Option<UiToolMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiToolPreview {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub started_at: UnixMs,
    pub metadata: Option<UiToolPreviewMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolPreviewMetadata {
    ShellCommand { output_tail: String },
    ApplyPatch(UiApplyPatchMetadata),
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

impl UiTool {
    fn empty() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
            preview: None,
            status: UiToolStatus::Running,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            metadata: None,
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

fn ui_tool_preview_metadata(metadata: &ToolPreviewMetadata) -> UiToolPreviewMetadata {
    match metadata {
        ToolPreviewMetadata::ShellCommand { output_tail } => UiToolPreviewMetadata::ShellCommand {
            output_tail: output_tail.clone(),
        },
        ToolPreviewMetadata::ApplyPatch(metadata) => {
            UiToolPreviewMetadata::ApplyPatch(ui_apply_patch_metadata(metadata))
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

fn diff_blocks(previous: &UiAgentState, current: &UiAgentState) -> UiBlocksDiff {
    let common_len = previous.blocks.len().min(current.blocks.len());
    let updates = previous.blocks[..common_len]
        .iter()
        .zip(&current.blocks[..common_len])
        .enumerate()
        .filter_map(|(index, (previous, current))| {
            (previous != current).then(|| UiBlockUpdate {
                index,
                block: diff_block(previous, current),
            })
        })
        .collect();
    let append = current.blocks[common_len..]
        .iter()
        .cloned()
        .map(|block| UiBlockAppend::from_previous_pending(block, previous))
        .collect();
    UiBlocksDiff {
        updates,
        truncate_to: (current.blocks.len() < previous.blocks.len()).then_some(current.blocks.len()),
        append,
    }
}

fn diff_block(previous: &UiBlock, current: &UiBlock) -> UiBlockDiff {
    match (previous, current) {
        (UiBlock::Tool(previous), UiBlock::Tool(current))
            if previous.id == current.id && previous.name == current.name =>
        {
            UiBlockDiff::Tool(UiToolDiff::from_changed(previous, current))
        }
        _ => UiBlockDiff::Replace(current.clone()),
    }
}

fn ui_blocks(blocks: &[Arc<ContextBlock>]) -> Vec<UiBlock> {
    let mut ui_blocks = Vec::new();
    for block in blocks {
        match block.as_ref() {
            ContextBlock::UserMessage { content } => ui_blocks.push(UiBlock::UserMessage {
                text: text_content(content),
            }),
            ContextBlock::InferenceResponse { items, .. } => {
                ui_blocks.extend(items.iter().filter_map(ui_block_from_response_item));
            }
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

fn ui_block_from_response_item(item: &InferenceResponseItem) -> Option<UiBlock> {
    match item {
        InferenceResponseItem::AssistantMessage { content, phase } => {
            Some(UiBlock::AssistantMessage {
                text: text_content(content),
                phase: phase.map(Into::into),
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
        AgentStateKind::ToolCalling { previews, results } => UiAgentStatus::ToolCalling {
            previews: previews
                .values()
                .map(|preview| UiToolPreview {
                    id: preview.call.id.as_str().to_owned(),
                    name: preview.call.name.as_str().to_owned(),
                    arguments: preview.call.arguments.clone(),
                    started_at: preview.started_at,
                    metadata: preview.metadata.as_ref().map(ui_tool_preview_metadata),
                })
                .collect(),
            results: results
                .iter()
                .map(|result| UiToolResult {
                    call_id: result.call_id.as_str().to_owned(),
                    status: result.body.status.into(),
                    started_at: result.started_at,
                    finished_at: result.finished_at,
                    metadata: result.metadata.as_ref().map(ui_tool_metadata),
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
            UiStreamingItemDiff::Tool {
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
        StreamingContextItem::AssistantMessage { content, phase } => {
            Some(UiStreamingItem::AssistantMessage {
                text: content.iter().map(ToString::to_string).collect(),
                phase: phase.map(Into::into),
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
        } => Some(UiStreamingItem::Tool(UiTool {
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
        StreamingContextItem::Compaction(_) => Some(UiStreamingItem::Notice {
            text: "compacting context".to_owned(),
        }),
        StreamingContextItem::Unknown(_) => None,
    }
}

fn ui_block_from_pending(item: &UiStreamingItem) -> Option<UiBlock> {
    match item {
        UiStreamingItem::AssistantMessage { text, phase } => Some(UiBlock::AssistantMessage {
            text: text.clone(),
            phase: *phase,
        }),
        UiStreamingItem::Reasoning { text } => Some(UiBlock::Reasoning { text: text.clone() }),
        UiStreamingItem::Tool(tool) => Some(UiBlock::Tool(tool.clone())),
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
    use std::collections::BTreeMap;
    use std::num::NonZeroU64;

    use rho_agent::{FailedInferenceResponse, ToolPreview};
    use rho_core::{
        AStr, ApplyPatchMetadata, ContentPart, PendingInferenceResponse, ToolCall, ToolCallId,
        ToolFileChange, ToolFileStatus, ToolName, ToolOutput, ToolResult, ToolType,
    };

    use super::*;

    fn streaming_state(text: &str) -> AgentState {
        AgentState {
            blocks: Vec::new(),
            tool_specs: Arc::from([]),
            system_prompt: Arc::from(""),
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
            system_prompt: Arc::from(""),
            kind: AgentStateKind::Idle,
        }
    }

    fn finished_tool_state() -> AgentState {
        let call_id = ToolCallId::try_from("call-1").unwrap();
        AgentState {
            blocks: vec![
                Arc::new(ContextBlock::InferenceResponse {
                    items: vec![InferenceResponseItem::ToolCall {
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
            kind: AgentStateKind::Idle,
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
            kind: AgentStateKind::ToolCalling {
                previews,
                results: Vec::new(),
            },
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
        let bytes = crate::protocol_frame_bytes(&crate::ServerMessage::Agent {
            agent_id: "agent-1".to_owned(),
            frame,
        })
        .unwrap();
        assert!(bytes.len() < 56, "tiny frame was {} bytes", bytes.len());
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
        assert_eq!(
            blocks,
            &UiBlocksDiff {
                updates: Vec::new(),
                truncate_to: None,
                append: vec![UiBlockAppend::Pending { index: 0 }],
            }
        );
        assert_eq!(*status, Some(UiAgentStatus::Idle));
        assert_eq!(
            *pending_response,
            UiPendingResponseDiff::Replace(Vec::new())
        );

        let bytes = crate::protocol_frame_bytes(&crate::ServerMessage::Agent {
            agent_id: "agent-1".to_owned(),
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
    fn tool_calling_status_includes_live_previews() {
        let state = UiAgentState::from_agent_state(&tool_calling_state());
        assert!(matches!(
            state.status,
            UiAgentStatus::ToolCalling {
                previews,
                results
            } if results.is_empty()
                && matches!(
                    previews.as_slice(),
                    [UiToolPreview {
                        name,
                        started_at: UnixMs(10),
                        metadata: Some(UiToolPreviewMetadata::ApplyPatch(UiApplyPatchMetadata {
                            changes
                        })),
                        ..
                    }] if name == "apply_patch"
                        && matches!(
                            changes.as_slice(),
                            [UiToolFileChange {
                                path,
                                status: UiToolFileStatus::Modified,
                            }] if path == "src/lib.rs"
                        )
                )
        ));
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
        assert_eq!(blocks.append, Vec::new());
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
