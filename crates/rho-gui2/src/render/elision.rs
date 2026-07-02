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

/// Computes the working-output elisions for blocks at `from_block` onward
/// plus the streaming frontier, in one pass over that region.
///
/// `visible` marks blocks that rendered any text (reasoning blocks, for
/// example, render nothing and cannot bound a fold). `frontier_visible` is
/// whether the streaming frontier rendered any text.
///
/// `from_block` must be a turn start (a user-message index, or 0) at or
/// before the active turn's start; plans for earlier turns depend only on
/// their own turn and can be cached by the caller. `carry` is a plan from
/// before `from_block` still open at the boundary (its end is the last
/// visible block before `from_block`); it is extended or flushed exactly as
/// a full recomputation would.
pub fn elision_plans_from(
    state: &UiAgentState,
    visible: &[bool],
    frontier_visible: bool,
    from_block: usize,
    carry: Option<ElisionPlan>,
) -> Vec<ElisionPlan> {
    let mut plans = Vec::new();
    let mut current: Option<ElisionPlan> = carry;
    let active_turn_has_pending_non_working = state
        .pending_response
        .iter()
        .any(|item| !streaming_item_is_working(item));
    let pending_tool_count = state
        .pending_response
        .iter()
        .filter(|item| matches!(item, UiStreamingItem::Tool(_)))
        .count();

    let mut active_tool_count = pending_tool_count;
    let mut turn_start = from_block;
    while turn_start < state.blocks.len() {
        let turn_end = state.blocks[turn_start + 1..]
            .iter()
            .position(is_user)
            .map(|offset| turn_start + 1 + offset)
            .unwrap_or(state.blocks.len());
        let turn = &state.blocks[turn_start..turn_end];
        let is_active = turn_end == state.blocks.len();
        let mut tool_count = turn
            .iter()
            .filter(|block| matches!(block, UiBlock::Tool(_)))
            .count();
        let has_non_working = turn
            .iter()
            .any(|block| !is_user(block) && !block_is_working(block));
        if is_active {
            tool_count += pending_tool_count;
            active_tool_count = tool_count;
        }
        let tail_rows = if has_non_working || (is_active && active_turn_has_pending_non_working) {
            0
        } else {
            LIMITED_TAIL_ROWS
        };

        for (offset, block) in turn.iter().enumerate() {
            let index = turn_start + offset;
            if !visible.get(index).copied().unwrap_or(false) {
                continue;
            }
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
        turn_start = turn_end;
    }

    if frontier_visible
        && !state.pending_response.is_empty()
        && state.pending_response.iter().all(streaming_item_is_working)
    {
        let tool_count = active_tool_count;
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

/// Start of the turn containing `block_index` (clamped to the last turn).
pub fn turn_start_index(state: &UiAgentState, block_index: usize) -> usize {
    let end = block_index.saturating_add(1).min(state.blocks.len());
    state.blocks[..end].iter().rposition(is_user).unwrap_or(0)
}

fn is_user(block: &UiBlock) -> bool {
    matches!(block, UiBlock::UserMessage { .. })
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

    fn elision_plans(
        state: &UiAgentState,
        visible: &[bool],
        frontier_visible: bool,
    ) -> Vec<ElisionPlan> {
        elision_plans_from(state, visible, frontier_visible, 0, None)
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
    fn plans_from_a_turn_start_match_the_full_recompute_suffix() {
        let state = state(
            vec![
                user("one"),
                commentary("a"),
                tool("t1"),
                final_answer("done"),
                user("two"),
                commentary("b"),
                tool("t2"),
            ],
            vec![UiStreamingItem::AssistantMessage {
                text: "more".to_owned(),
                phase: Some(UiMessagePhase::Commentary),
            }],
        );
        let visible = all_visible(&state);
        let full = elision_plans(&state, &visible, true);
        let boundary = 4;
        assert_eq!(turn_start_index(&state, 5), boundary);
        assert_eq!(turn_start_index(&state, state.blocks.len()), boundary);
        let cached = full
            .iter()
            .copied()
            .filter(|plan| matches!(plan.end, ElisionEnd::Block(end) if end < boundary))
            .collect::<Vec<_>>();
        let tail = elision_plans_from(&state, &visible, true, boundary, None);
        let mut recombined = cached;
        recombined.extend(tail);
        assert_eq!(recombined, full);
    }

    #[test]
    fn carry_merges_a_run_across_the_recompute_boundary() {
        // An invisible user message is the only way a working run can cross a
        // turn boundary; the carried plan must extend exactly as a full
        // recompute would.
        let state = state(
            vec![commentary("a"), user(""), commentary("b")],
            Vec::new(),
        );
        let visible = vec![true, false, true];
        let full = elision_plans(&state, &visible, false);
        assert_eq!(
            full,
            vec![ElisionPlan {
                start_block: 0,
                end: ElisionEnd::Block(2),
                tool_count: 0,
                tail_rows: LIMITED_TAIL_ROWS,
            }]
        );
        let carry = Some(ElisionPlan {
            start_block: 0,
            end: ElisionEnd::Block(0),
            tool_count: 0,
            tail_rows: LIMITED_TAIL_ROWS,
        });
        let tail = elision_plans_from(&state, &visible, false, 1, carry);
        assert_eq!(tail, full);
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
