//! Elision policy: which stretches of "working" output (commentary, tool
//! bursts, reasoning) should collapse behind a `N tools` fold.
//!
//! Pure over the block list plus a per-block visibility mask; the transcript
//! model materializes the resulting index ranges into buffer anchors.

use rho_ui_proto::remote::UiBlock;

use super::{BlockKind, block_kind};

pub const LIMITED_TAIL_ROWS: u32 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElisionPlan {
    pub start_block: usize,
    /// Inclusive block index.
    pub end_block: usize,
    pub tool_count: usize,
    pub tail_rows: u32,
}

/// Computes the working-output elisions for blocks at `from_block` onward in
/// one pass over that region.
///
/// `visible` marks blocks that rendered any text (reasoning blocks, for
/// example, render nothing and cannot bound a fold).
///
/// `from_block` must be a turn start (a user-message index, or 0); plans
/// depend only on their own turn, so earlier turns' plans can be cached by
/// the caller. `carry` is a plan from before `from_block` still open at the
/// boundary (its end is the last visible block before `from_block`); it is
/// extended or flushed exactly as a full recomputation would.
pub fn elision_plans_from(
    blocks: &[UiBlock],
    visible: &[bool],
    from_block: usize,
    carry: Option<ElisionPlan>,
) -> Vec<ElisionPlan> {
    let mut plans = Vec::new();
    let mut current: Option<ElisionPlan> = carry;

    let mut turn_start = from_block;
    while turn_start < blocks.len() {
        let turn_end = blocks[turn_start + 1..]
            .iter()
            .position(is_user)
            .map(|offset| turn_start + 1 + offset)
            .unwrap_or(blocks.len());
        let turn = &blocks[turn_start..turn_end];
        let tool_count = turn
            .iter()
            .filter(|block| matches!(block, UiBlock::Tool(_)))
            .count();
        let has_non_working = turn
            .iter()
            .any(|block| !is_user(block) && !block_is_working(block));
        let tail_rows = if has_non_working { 0 } else { LIMITED_TAIL_ROWS };

        for (offset, block) in turn.iter().enumerate() {
            let index = turn_start + offset;
            if !visible.get(index).copied().unwrap_or(false) {
                continue;
            }
            if block_is_working(block) {
                match current.as_mut() {
                    Some(plan) if plan.tool_count == tool_count && plan.tail_rows == tail_rows => {
                        plan.end_block = index;
                    }
                    _ => {
                        plans.extend(current.take());
                        current = Some(ElisionPlan {
                            start_block: index,
                            end_block: index,
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

    plans.extend(current);
    plans
}

/// Start of the turn containing `block_index` (clamped to the last turn).
pub fn turn_start_index(blocks: &[UiBlock], block_index: usize) -> usize {
    let end = block_index.saturating_add(1).min(blocks.len());
    blocks[..end].iter().rposition(is_user).unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use rho_ui_proto::remote::{UiMessagePhase, UiTool, UiToolStatus};

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

    fn all_visible(blocks: &[UiBlock]) -> Vec<bool> {
        vec![true; blocks.len()]
    }

    fn elision_plans(blocks: &[UiBlock], visible: &[bool]) -> Vec<ElisionPlan> {
        elision_plans_from(blocks, visible, 0, None)
    }

    #[test]
    fn working_blocks_merge_into_one_plan() {
        let blocks = vec![user("go"), commentary("thinking"), tool("a"), tool("b")];
        let plans = elision_plans(&blocks, &all_visible(&blocks));
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end_block: 3,
                tool_count: 2,
                tail_rows: LIMITED_TAIL_ROWS,
            }]
        );
    }

    #[test]
    fn final_answer_collapses_working_blocks_fully() {
        let blocks = vec![user("go"), commentary("thinking"), final_answer("done")];
        let plans = elision_plans(&blocks, &all_visible(&blocks));
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end_block: 1,
                tool_count: 0,
                tail_rows: 0,
            }]
        );
    }

    #[test]
    fn streaming_final_answer_collapses_committed_commentary() {
        // The unsealed final answer is just the turn's last block; its
        // presence zeroes the turn's fold tail.
        let blocks = vec![user("go"), commentary("working"), final_answer("final begins")];
        let plans = elision_plans(&blocks, &all_visible(&blocks));
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end_block: 1,
                tool_count: 0,
                tail_rows: 0,
            }]
        );
    }

    #[test]
    fn all_working_turn_keeps_a_limited_tail() {
        let blocks = vec![user("go"), commentary("working"), commentary("more working")];
        let plans = elision_plans(&blocks, &all_visible(&blocks));
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end_block: 2,
                tool_count: 0,
                tail_rows: LIMITED_TAIL_ROWS,
            }]
        );
    }

    #[test]
    fn plans_from_a_turn_start_match_the_full_recompute_suffix() {
        let blocks = vec![
            user("one"),
            commentary("a"),
            tool("t1"),
            final_answer("done"),
            user("two"),
            commentary("b"),
            tool("t2"),
            commentary("more"),
        ];
        let visible = all_visible(&blocks);
        let full = elision_plans(&blocks, &visible);
        let boundary = 4;
        assert_eq!(turn_start_index(&blocks, 5), boundary);
        assert_eq!(turn_start_index(&blocks, blocks.len()), boundary);
        let cached = full
            .iter()
            .copied()
            .filter(|plan| plan.end_block < boundary)
            .collect::<Vec<_>>();
        let tail = elision_plans_from(&blocks, &visible, boundary, None);
        let mut recombined = cached;
        recombined.extend(tail);
        assert_eq!(recombined, full);
    }

    #[test]
    fn carry_merges_a_run_across_the_recompute_boundary() {
        // An invisible user message is the only way a working run can cross a
        // turn boundary; the carried plan must extend exactly as a full
        // recompute would.
        let blocks = vec![commentary("a"), user(""), commentary("b")];
        let visible = vec![true, false, true];
        let full = elision_plans(&blocks, &visible);
        assert_eq!(
            full,
            vec![ElisionPlan {
                start_block: 0,
                end_block: 2,
                tool_count: 0,
                tail_rows: LIMITED_TAIL_ROWS,
            }]
        );
        let carry = Some(ElisionPlan {
            start_block: 0,
            end_block: 0,
            tool_count: 0,
            tail_rows: LIMITED_TAIL_ROWS,
        });
        let tail = elision_plans_from(&blocks, &visible, 1, carry);
        assert_eq!(tail, full);
    }

    #[test]
    fn invisible_blocks_do_not_break_a_run() {
        let blocks = vec![
            user("go"),
            commentary("before"),
            UiBlock::Reasoning {
                text: "hidden".to_owned(),
            },
            commentary("after"),
        ];
        let mut visible = all_visible(&blocks);
        visible[2] = false;
        let plans = elision_plans(&blocks, &visible);
        assert_eq!(
            plans,
            vec![ElisionPlan {
                start_block: 1,
                end_block: 3,
                tool_count: 0,
                tail_rows: LIMITED_TAIL_ROWS,
            }]
        );
    }
}
