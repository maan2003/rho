use rho_core::{MessagePhase, ToolOutputStatus, UnixMs};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::MessageDelivery;

/// Sender-side remote UI-state encoder.
///
/// The daemon supplies an already-projected UI shape, so this crate does not
/// inherit runtime agent dependencies. Diffing those states keeps append-only
/// history and streaming text updates cheap.
#[derive(Default)]
pub struct AgentRemoteEncoder {
    last_sent: Option<UiAgentState>,
}

impl AgentRemoteEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(&mut self, current: UiAgentState) -> AgentRemoteFrame {
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
    /// The daemon stopped this client's live state stream. Retained transcript
    /// content may still be displayed, but it is no longer being updated.
    Unloaded,
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
