//! Elision policy: which stretches of "working" output (commentary, tool
//! bursts, reasoning) should collapse behind a `N tools` fold.
//!
//! Pure over protocol state plus a per-block visibility mask; the transcript
//! model materializes the resulting index ranges into buffer anchors.

use rho_ui_proto::remote::{UiAgentState, UiBlock, UiStreamingItem};

use super::{BlockKind, block_kind, streaming_item_kind};

pub const LIMITED_TAIL_ROWS: u32 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElisionEnd {
    /// Inclusive block index.
    Block(usize),
    /// The elision extends through the streaming frontier.
    Frontier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElisionPlan {
    pub start_block: usize,
    pub end: ElisionEnd,
    pub tool_count: usize,
    pub tail_rows: u32,
}

/// Computes the working-output elisions for the current state.
///
/// `visible` marks blocks that rendered any text (reasoning blocks, for
/// example, render nothing and cannot bound a fold). `frontier_visible` is
/// whether the streaming frontier rendered any text.
pub fn elision_plans(
    state: &UiAgentState,
    visible: &[bool],
    frontier_visible: bool,
) -> Vec<ElisionPlan> {
    let mut plans = Vec::new();
    let mut current: Option<ElisionPlan> = None;
    let active_turn_has_pending_non_working = state
        .pending_response
        .iter()
        .any(|item| !streaming_item_is_working(item));
    let pending_tool_count = state
        .pending_response
        .iter()
        .filter(|item| matches!(item, UiStreamingItem::Tool(_)))
        .count();

    for (index, block) in state.blocks.iter().enumerate() {
        if !visible.get(index).copied().unwrap_or(false) {
            continue;
        }
        let mut tool_count = turn_tool_count(state, index);
        if block_is_in_active_turn(state, index) {
            tool_count += pending_tool_count;
        }
        let tail_rows = if turn_has_non_working_response(state, index)
            || (active_turn_has_pending_non_working && block_is_in_active_turn(state, index))
        {
            0
        } else {
            LIMITED_TAIL_ROWS
        };

        if block_is_working(block) {
            match current.as_mut() {
                Some(plan) if plan.tool_count == tool_count && plan.tail_rows == tail_rows => {
                    plan.end = ElisionEnd::Block(index);
                }
                _ => {
                    plans.extend(current.take());
                    current = Some(ElisionPlan {
                        start_block: index,
                        end: ElisionEnd::Block(index),
                        tool_count,
                        tail_rows,
                    });
                }
            }
        } else {
            plans.extend(current.take());
        }
    }

    if frontier_visible
        && !state.pending_response.is_empty()
        && state.pending_response.iter().all(streaming_item_is_working)
    {
        let tool_count = active_turn_tool_count(state) + pending_tool_count;
        match current.as_mut() {
            Some(plan)
                if plan.tool_count == tool_count && plan.tail_rows == LIMITED_TAIL_ROWS =>
            {
                plan.end = ElisionEnd::Frontier;
            }
            _ => {
                plans.extend(current.take());
                current = Some(ElisionPlan {
                    start_block: state.blocks.len(),
                    end: ElisionEnd::Frontier,
                    tool_count,
                    tail_rows: LIMITED_TAIL_ROWS,
                });
            }
        }
    }

    plans.extend(current);
    plans
}

pub fn elision_label(tool_count: usize) -> String {
    match tool_count {
        0 => "working".to_owned(),
        1 => "1 tool".to_owned(),
        count => format!("{count} tools"),
    }
}

fn block_is_working(block: &UiBlock) -> bool {
    matches!(block_kind(block), BlockKind::Response { working: true })
}

fn streaming_item_is_working(item: &UiStreamingItem) -> bool {
    matches!(streaming_item_kind(item), BlockKind::Response { working: true })
}

fn turn_range(state: &UiAgentState, block_index: usize) -> std::ops::Range<usize> {
    let turn_start = state.blocks[..=block_index]
        .iter()
        .rposition(|block| matches!(block, UiBlock::UserMessage { .. }))
        .unwrap_or(0);
    let turn_end = state.blocks[block_index + 1..]
        .iter()
        .position(|block| matches!(block, UiBlock::UserMessage { .. }))
        .map(|offset| block_index + 1 + offset)
        .unwrap_or(state.blocks.len());
    turn_start..turn_end
}

fn turn_tool_count(state: &UiAgentState, block_index: usize) -> usize {
    state.blocks[turn_range(state, block_index)]
        .iter()
        .filter(|block| matches!(block, UiBlock::Tool(_)))
        .count()
}

fn turn_has_non_working_response(state: &UiAgentState, block_index: usize) -> bool {
    state.blocks[turn_range(state, block_index)].iter().any(|block| {
        !matches!(block, UiBlock::UserMessage { .. }) && !block_is_working(block)
    })
}

fn block_is_in_active_turn(state: &UiAgentState, block_index: usize) -> bool {
    state
        .blocks
        .iter()
        .rposition(|block| matches!(block, UiBlock::UserMessage { .. }))
        .is_none_or(|turn_start| block_index >= turn_start)
}

fn active_turn_tool_count(state: &UiAgentState) -> usize {
    let turn_start = state
        .blocks
        .iter()
        .rposition(|block| matches!(block, UiBlock::UserMessage { .. }))
        .unwrap_or(0);
    state.blocks[turn_start..]
        .iter()
        .filter(|block| matches!(block, UiBlock::Tool(_)))
        .count()
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::remote::{UiAgentStatus, UiMessagePhase, UiTool, UiToolStatus};

    use super::*;

    fn user(text: &str) -> UiBlock {
        UiBlock::UserMessage {
            text: text.to_owned(),
        }
    }

    fn commentary(text: &str) -> UiBlock {
        UiBlock::AssistantMessage {
            text: text.to_owned(),
            phase: Some(UiMessagePhase::Commentary),
        }
    }

    fn final_answer(text: &str) -> UiBlock {
        UiBlock::AssistantMessage {
            text: text.to_owned(),
            phase: Some(UiMessagePhase::FinalAnswer),
        }
    }

    fn tool(id: &str) -> UiBlock {
        UiBlock::Tool(UiTool {
            id: id.to_owned(),
            name: "shell".to_owned(),
            arguments: String::new(),
            preview: None,
            status: UiToolStatus::Success,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            metadata: None,
        })
    }

    fn state(blocks: Vec<UiBlock>, pending: Vec<UiStreamingItem>) -> UiAgentState {
        UiAgentState {
            blocks,
            status: UiAgentStatus::Streaming,
            pending_response: pending,
        }
    }

    fn all_visible(state: &UiAgentState) -> Vec<bool> {
        vec![true; state.blocks.len()]
    }

    #[test]
    fn working_blocks_merge_into_one_plan() {
        let state = state(
            vec![user("go"), commentary("thinking"), tool("a"), tool("b")],
            Vec::new(),
        );
        let plans = elision_plans(&state, &all_visible(&state), false);
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end: ElisionEnd::Block(3),
                tool_count: 2,
                tail_rows: LIMITED_TAIL_ROWS,
            }]
        );
    }

    #[test]
    fn final_answer_collapses_working_blocks_fully() {
        let state = state(
            vec![user("go"), commentary("thinking"), final_answer("done")],
            Vec::new(),
        );
        let plans = elision_plans(&state, &all_visible(&state), false);
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end: ElisionEnd::Block(1),
                tool_count: 0,
                tail_rows: 0,
            }]
        );
    }

    #[test]
    fn streaming_final_answer_collapses_committed_commentary() {
        let state = state(
            vec![user("go"), commentary("working")],
            vec![UiStreamingItem::AssistantMessage {
                text: "final begins".to_owned(),
                phase: Some(UiMessagePhase::FinalAnswer),
            }],
        );
        let plans = elision_plans(&state, &all_visible(&state), true);
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end: ElisionEnd::Block(1),
                tool_count: 0,
                tail_rows: 0,
            }]
        );
    }

    #[test]
    fn all_working_pending_extends_through_frontier() {
        let state = state(
            vec![user("go"), commentary("working")],
            vec![UiStreamingItem::AssistantMessage {
                text: "more working".to_owned(),
                phase: Some(UiMessagePhase::Commentary),
            }],
        );
        let plans = elision_plans(&state, &all_visible(&state), true);
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end: ElisionEnd::Frontier,
                tool_count: 0,
                tail_rows: LIMITED_TAIL_ROWS,
            }]
        );
    }

    #[test]
    fn invisible_blocks_do_not_break_a_run() {
        let state = state(
            vec![
                user("go"),
                commentary("before"),
                UiBlock::Reasoning {
                    text: "hidden".to_owned(),
                },
                commentary("after"),
            ],
            Vec::new(),
        );
        let mut visible = all_visible(&state);
        visible[2] = false;
        let plans = elision_plans(&state, &visible, false);
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end: ElisionEnd::Block(3),
                tool_count: 0,
                tail_rows: LIMITED_TAIL_ROWS,
            }]
        );
    }
}
