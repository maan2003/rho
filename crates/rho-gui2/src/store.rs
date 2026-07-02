//! Canonical per-agent protocol state and frame change summaries.
//!
//! Each agent's `UiAgentState` exists exactly once, here. Frames mutate it in
//! place; the returned [`FrameSummary`] tells views the minimal region they
//! must re-render, so per-event cost is O(changed suffix), never O(session).

use std::collections::HashMap;

use rho_ui_proto::AgentId;
use rho_ui_proto::remote::{AgentRemoteFrame, UiAgentState, UiAgentStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSummary {
    /// First block index whose rendered content may have changed; everything
    /// from here to the end of the transcript needs re-rendering. `None`
    /// means nothing visible changed.
    pub first_changed_block: Option<usize>,
}

impl FrameSummary {
    pub fn everything() -> Self {
        Self {
            first_changed_block: Some(0),
        }
    }

    /// Combines two summaries into one covering both changes, so hidden
    /// views can accumulate frames and render once when shown.
    pub fn merge(self, other: Self) -> Self {
        Self {
            first_changed_block: match (self.first_changed_block, other.first_changed_block) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
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
        let summary = summarize(&frame);
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
    }
}

/// Computes what a frame will change, before it is applied.
fn summarize(frame: &AgentRemoteFrame) -> FrameSummary {
    match frame {
        AgentRemoteFrame::Snapshot(_) => FrameSummary::everything(),
        AgentRemoteFrame::Diff { blocks, status: _ } => {
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
            FrameSummary {
                first_changed_block: first_changed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::remote::{UiBlock, UiBlockDiff, UiBlockUpdate, UiBlocksDiff, UiTextDiff};

    use super::*;

    fn diff_frame(blocks: UiBlocksDiff) -> AgentRemoteFrame {
        AgentRemoteFrame::Diff {
            blocks,
            status: None,
        }
    }

    #[test]
    fn snapshot_changes_everything() {
        let frame = AgentRemoteFrame::Snapshot(empty_state());
        assert_eq!(summarize(&frame), FrameSummary::everything());
    }

    #[test]
    fn status_only_diff_is_a_noop() {
        let frame = diff_frame(UiBlocksDiff {
            truncate_to: None,
            updates: Vec::new(),
        });
        assert_eq!(
            summarize(&frame),
            FrameSummary {
                first_changed_block: None,
            }
        );
    }

    #[test]
    fn streaming_update_changes_only_that_block() {
        let frame = diff_frame(UiBlocksDiff {
            truncate_to: None,
            updates: vec![UiBlockUpdate {
                index: 4,
                block: UiBlockDiff::AssistantText(UiTextDiff {
                    keep_bytes: 3,
                    value: "lo".to_owned(),
                }),
            }],
        });
        assert_eq!(
            summarize(&frame),
            FrameSummary {
                first_changed_block: Some(4),
            }
        );
    }

    #[test]
    fn update_and_truncate_take_the_smaller_index() {
        let frame = diff_frame(UiBlocksDiff {
            truncate_to: Some(2),
            updates: vec![UiBlockUpdate {
                index: 4,
                block: UiBlockDiff::Replace(UiBlock::Notice {
                    text: "x".to_owned(),
                }),
            }],
        });
        assert_eq!(
            summarize(&frame),
            FrameSummary {
                first_changed_block: Some(2),
            }
        );
    }
}
