//! Canonical per-agent protocol state and frame change summaries.
//!
//! Each agent's `UiAgentState` exists exactly once, here. Frames mutate it in
//! place; the returned [`FrameSummary`] tells views the minimal region they
//! must re-render, so per-event cost is O(changed suffix), never O(session).

use std::collections::HashMap;

use rho_ui_proto::AgentId;
use rho_ui_proto::remote::{
    AgentRemoteFrame, UiAgentState, UiAgentStatus, UiPendingResponseDiff,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSummary {
    /// First block index whose rendered content may have changed; everything
    /// from here to the end of the transcript needs re-rendering. `None`
    /// means no block changed.
    pub first_changed_block: Option<usize>,
    pub pending_changed: bool,
}

impl FrameSummary {
    pub fn everything() -> Self {
        Self {
            first_changed_block: Some(0),
            pending_changed: true,
        }
    }

    pub fn is_noop(&self) -> bool {
        self.first_changed_block.is_none() && !self.pending_changed
    }

    /// Combines two summaries into one covering both changes, so hidden
    /// views can accumulate frames and render once when shown.
    pub fn merge(self, other: Self) -> Self {
        Self {
            first_changed_block: match (self.first_changed_block, other.first_changed_block) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            pending_changed: self.pending_changed || other.pending_changed,
        }
    }
}

#[derive(Default)]
pub struct AgentStore {
    states: HashMap<AgentId, UiAgentState>,
}

impl AgentStore {
    pub fn apply(&mut self, agent_id: AgentId, frame: AgentRemoteFrame) -> FrameSummary {
        let state = self.states.entry(agent_id).or_insert_with(empty_state);
        let summary = summarize(&frame, state.blocks.len());
        frame.apply_diff(state);
        summary
    }

    pub fn get(&self, agent_id: &AgentId) -> Option<&UiAgentState> {
        self.states.get(agent_id)
    }
}

fn empty_state() -> UiAgentState {
    UiAgentState {
        blocks: Vec::new(),
        status: UiAgentStatus::Idle,
        pending_response: Vec::new(),
    }
}

/// Computes what a frame will change, before it is applied to a state with
/// `blocks_len` blocks.
fn summarize(frame: &AgentRemoteFrame, blocks_len: usize) -> FrameSummary {
    match frame {
        AgentRemoteFrame::Snapshot(_) => FrameSummary::everything(),
        AgentRemoteFrame::Diff {
            blocks,
            pending_response,
            ..
        } => {
            let mut first_changed = None;
            let mut note = |index: usize| {
                first_changed = Some(first_changed.map_or(index, |first: usize| first.min(index)));
            };
            for update in &blocks.updates {
                note(update.index);
            }
            if let Some(truncate_to) = blocks.truncate_to {
                note(truncate_to);
            }
            if !blocks.append.is_empty() {
                note(blocks.truncate_to.unwrap_or(blocks_len));
            }
            let pending_changed = match pending_response {
                UiPendingResponseDiff::Replace(_) => true,
                UiPendingResponseDiff::Items(items) => !items.is_empty(),
            };
            FrameSummary {
                first_changed_block: first_changed,
                pending_changed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::remote::{
        UiBlock, UiBlockAppend, UiBlockDiff, UiBlockUpdate, UiBlocksDiff, UiStreamingItemUpdate,
        UiStreamingItemDiff, UiTextDiff,
    };

    use super::*;

    fn diff_frame(blocks: UiBlocksDiff, pending: UiPendingResponseDiff) -> AgentRemoteFrame {
        AgentRemoteFrame::Diff {
            blocks,
            status: None,
            pending_response: pending,
        }
    }

    fn empty_blocks_diff() -> UiBlocksDiff {
        UiBlocksDiff {
            updates: Vec::new(),
            truncate_to: None,
            append: Vec::new(),
        }
    }

    #[test]
    fn snapshot_changes_everything() {
        let frame = AgentRemoteFrame::Snapshot(empty_state());
        assert_eq!(summarize(&frame, 5), FrameSummary::everything());
    }

    #[test]
    fn append_only_diff_changes_from_old_length() {
        let frame = diff_frame(
            UiBlocksDiff {
                updates: Vec::new(),
                truncate_to: None,
                append: vec![UiBlockAppend::Block(UiBlock::UserMessage {
                    text: "hi".to_owned(),
                })],
            },
            UiPendingResponseDiff::Items(Vec::new()),
        );
        assert_eq!(
            summarize(&frame, 3),
            FrameSummary {
                first_changed_block: Some(3),
                pending_changed: false,
            }
        );
    }

    #[test]
    fn streaming_only_diff_touches_no_blocks() {
        let frame = diff_frame(
            empty_blocks_diff(),
            UiPendingResponseDiff::Items(vec![UiStreamingItemUpdate {
                index: 0,
                item: UiStreamingItemDiff::AssistantMessage {
                    text: UiTextDiff {
                        keep_bytes: 3,
                        value: "lo".to_owned(),
                    },
                },
            }]),
        );
        assert_eq!(
            summarize(&frame, 3),
            FrameSummary {
                first_changed_block: None,
                pending_changed: true,
            }
        );
    }

    #[test]
    fn update_and_truncate_take_the_smaller_index() {
        let frame = diff_frame(
            UiBlocksDiff {
                updates: vec![UiBlockUpdate {
                    index: 4,
                    block: UiBlockDiff::Replace(UiBlock::Notice {
                        text: "x".to_owned(),
                    }),
                }],
                truncate_to: Some(2),
                append: vec![UiBlockAppend::Block(UiBlock::Notice {
                    text: "y".to_owned(),
                })],
            },
            UiPendingResponseDiff::Replace(Vec::new()),
        );
        assert_eq!(
            summarize(&frame, 6),
            FrameSummary {
                first_changed_block: Some(2),
                pending_changed: true,
            }
        );
    }
}
