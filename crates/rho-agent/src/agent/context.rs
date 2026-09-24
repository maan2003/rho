//! Active-window policy and durable transition replay. Full history stays
//! intact.
use std::sync::Arc;

use rho_inference::types::{ContextBlock, InferenceResponseItem, ToolCallId};

use crate::ContextChange;

/// Preserve a recent 40k-token suffix and aim for 40k estimated tokens
/// remaining. Protected context can keep usage above that target.
pub(super) const RETAIN_TOKENS: u64 = 40000;

pub(super) const MANUAL_COMPACTION: &str = "Manual compaction was requested. Earlier retention and preparation notices are canceled; do not resume their preparation.";
pub(super) const POLICY_CHANGED: &str = "The context-management role has changed. Earlier retention and preparation notices are canceled; do not resume their preparation. Existing history remains available through Python.";
pub(super) const EVICTED: &str = "Older tool exchanges were removed to free context space. Recent exchanges, conversation, and reasoning remain. Original history is available through `transcript` in Python.";

/// Measured result/update budgets, indexed by immutable transcript positions.
/// Live recording and replay use the same observer; no extra persisted format.
#[derive(Default)]
pub(super) struct UsageCaps {
    model: Option<crate::db::AgentUsageModel>,
    previous: Option<(usize, u64)>,
    outputs: std::collections::BTreeMap<(usize, usize), u64>,
}

impl UsageCaps {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn observe(&mut self, event: &crate::native::NativeEvent, history: &[Arc<ContextBlock>]) {
        use crate::native::NativeEvent;
        match event {
            NativeEvent::RequestStarted { input, .. } => {
                if input.iter().any(|block| {
                    matches!(
                        block,
                        ContextBlock::CompactionTrigger
                            | ContextBlock::ContextRotation { .. }
                            | ContextBlock::ToolHistoryEvicted { .. }
                    )
                }) {
                    // Existing allocations remain fixed, including portions already
                    // evicted. Never redistribute their budget to surviving exchanges.
                    self.previous = None;
                }
            }
            NativeEvent::ResponseFinished {
                output,
                context_used,
                usage,
                ..
            } => {
                let (Some(total), Some(usage)) = (context_used, usage) else {
                    self.previous = None;
                    return;
                };
                if usage.approximate || usage.requests != 1 {
                    self.previous = None;
                    return;
                }
                if self.model != Some(usage.model) {
                    self.reset();
                    self.model = Some(usage.model);
                }
                let Some(input) = total.checked_sub(usage.output_tokens) else {
                    self.previous = None;
                    return;
                };
                if let Some((start, previous_total)) = self.previous
                    && let Some(budget) = input.checked_sub(previous_total)
                {
                    let mut costs = Vec::new();
                    for (index, block) in history.iter().enumerate().skip(start) {
                        match &**block {
                            ContextBlock::ToolResults { results } => {
                                for (part, result) in results.iter().enumerate() {
                                    costs.push((
                                        (index, part),
                                        output_tokens(
                                            &result.body.output,
                                            result.body.images.len(),
                                        ),
                                    ));
                                }
                            }
                            ContextBlock::ToolUpdate(update) => {
                                costs.push((
                                    (index, 0),
                                    output_tokens(&update.output, update.images.len()),
                                ));
                            }
                            _ => {}
                        }
                    }
                    let estimated: u64 = costs.iter().map(|(_, cost)| *cost).sum();
                    if budget < estimated {
                        for (key, cost) in costs {
                            // Proportional, rounded down: all outputs in the window
                            // share one cap, even when evicted in separate passes.
                            let capped = (u128::from(cost) * u128::from(budget)
                                / u128::from(estimated))
                                as u64;
                            self.outputs.insert(key, capped);
                        }
                    }
                }
                self.previous = Some((history.len() + output.len(), *total));
            }
            NativeEvent::RequestFailed { .. } => self.previous = None,
        }
    }

    fn output(&self, block: usize, part: usize, estimate: u64) -> u64 {
        self.outputs
            .get(&(block, part))
            .copied()
            .unwrap_or(estimate)
    }
}

fn output_tokens(text: &str, images: usize) -> u64 {
    text_tokens(text) + images as u64 * 10000 + 8
}

pub(super) struct Eviction {
    pub call_ids: Vec<ToolCallId>,
    pub freed_tokens: u64,
}

/// Work from full history, excluding exchanges already evicted or summarized.
/// A call's final contribution determines its age, and live calls are
/// protected.
pub(super) fn evict_tools(
    history: &[Arc<ContextBlock>],
    active: &std::collections::BTreeSet<ToolCallId>,
    used: u64,
    caps: &UsageCaps,
) -> Eviction {
    use std::collections::{BTreeMap, BTreeSet};
    let start = rho_inference::types::context_window_start(history);
    let mut first_block = start;
    let mut first_item = 0;
    for (i, block) in history.iter().enumerate().skip(start) {
        if let ContextBlock::InferenceResponse { items, .. } = &**block
            && let Some(j) = items
                .iter()
                .rposition(|item| matches!(item, InferenceResponseItem::Compaction { .. }))
        {
            first_block = i;
            first_item = j;
        }
    }
    let removed: BTreeSet<_> = history
        .iter()
        .filter_map(|block| match &**block {
            ContextBlock::ToolHistoryEvicted { call_ids } => Some(call_ids),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect();
    let mut suffix = 0;
    let mut recent_start = history.len();
    for (i, block) in history.iter().enumerate().skip(first_block).rev() {
        recent_start = i;
        suffix += estimate_visible(block, &removed);
        if suffix >= RETAIN_TOKENS {
            break;
        }
    }
    // (last contribution, token estimate, has call, has result)
    let mut calls = BTreeMap::<ToolCallId, (usize, u64, bool, bool)>::new();
    for (i, block) in history.iter().enumerate().skip(first_block) {
        match &**block {
            ContextBlock::InferenceResponse { items, .. } => {
                for item in items
                    .iter()
                    .skip(if i == first_block { first_item } else { 0 })
                {
                    if let InferenceResponseItem::ToolCall { id, arguments, .. } = item {
                        let entry = calls.entry(id.clone()).or_default();
                        entry.0 = i;
                        entry.1 += text_tokens(arguments) + 8;
                        entry.2 = true;
                    }
                }
            }
            ContextBlock::ToolResults { results } => {
                for (part, result) in results.iter().enumerate() {
                    let entry = calls.entry(result.call_id.clone()).or_default();
                    entry.0 = i;
                    entry.1 += caps.output(
                        i,
                        part,
                        output_tokens(&result.body.output, result.body.images.len()),
                    );
                    entry.3 = true;
                }
            }
            ContextBlock::ToolUpdate(update) => {
                let entry = calls.entry(update.call_id.clone()).or_default();
                entry.0 = i;
                entry.1 += caps.output(i, 0, output_tokens(&update.output, update.images.len()));
            }
            _ => {}
        }
    }
    let mut candidates = calls
        .into_iter()
        .filter(|(id, (last, _, call, result))| {
            *call
                && *result
                && *last < recent_start
                && !active.contains(id)
                && !removed.contains(id)
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| a.1.0.cmp(&b.1.0).then_with(|| a.0.cmp(&b.0)));
    let needed = used.saturating_sub(RETAIN_TOKENS);
    let mut eviction = Eviction {
        call_ids: Vec::new(),
        freed_tokens: 0,
    };
    for (id, (_, tokens, _, _)) in candidates {
        if eviction.freed_tokens >= needed {
            break;
        }
        eviction.call_ids.push(id);
        eviction.freed_tokens += tokens;
    }
    eviction
}

fn text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(3)
}

/// Legacy notices are still interpreted when loading older transcripts, but
/// no new preparation exchanges are scheduled.
#[derive(Clone, Debug, Default)]
pub(super) struct Window {
    pub marker: Option<usize>,
    pub preparation: Option<()>,
}

impl Window {
    pub fn sent(&mut self, change: &ContextChange) {
        match change {
            ContextChange::Marked { retain_from } => self.marker = Some(*retain_from as usize),
            ContextChange::Preparing { retain_from, .. } => {
                self.marker = Some(*retain_from as usize);
                self.preparation = Some(());
            }
        }
    }
    pub fn rotated(&mut self) {
        *self = Self::default();
    }
}

/// Local estimates select eviction candidates; provider-reported occupancy
/// remains authoritative after the next response.
pub(super) fn estimate(block: &ContextBlock) -> u64 {
    estimate_visible(block, &Default::default())
}

fn estimate_visible(block: &ContextBlock, removed: &std::collections::BTreeSet<ToolCallId>) -> u64 {
    fn parts(parts: &[rho_agent_types::ContentPart]) -> u64 {
        parts
            .iter()
            .map(|part| match part {
                rho_agent_types::ContentPart::Text { text } => text_tokens(text),
                rho_agent_types::ContentPart::Image { .. } => 10000,
            })
            .sum()
    }
    8 + match block {
        ContextBlock::UserMessage { content, .. } => parts(content),
        ContextBlock::DeveloperMessage { text } => text_tokens(text),
        ContextBlock::ToolResults { results } => results
            .iter()
            .filter(|result| !removed.contains(&result.call_id))
            .map(|result| {
                text_tokens(&result.body.output) + result.body.images.len() as u64 * 10000
            })
            .sum(),
        ContextBlock::ToolUpdate(update) => {
            if removed.contains(&update.call_id) {
                0
            } else {
                text_tokens(&update.output) + update.images.len() as u64 * 10000
            }
        }
        ContextBlock::InferenceResponse { items, .. } => items
            .iter()
            .map(|item| match item {
                InferenceResponseItem::AssistantMessage { content, .. } => parts(content),
                InferenceResponseItem::ToolCall { id, arguments, .. } => {
                    if removed.contains(id) {
                        0
                    } else {
                        text_tokens(arguments)
                    }
                }
                InferenceResponseItem::RawReasoning {
                    content, summary, ..
                } => {
                    text_tokens(content)
                        + summary.iter().map(|value| text_tokens(value)).sum::<u64>()
                }
                InferenceResponseItem::EncryptedReasoning { summary, .. } => {
                    summary.iter().map(|value| text_tokens(value)).sum()
                }
                _ => 0,
            })
            .sum(),
        ContextBlock::CompactionTrigger
        | ContextBlock::ContextRotation { .. }
        | ContextBlock::ToolHistoryEvicted { .. } => 0,
    }
}
