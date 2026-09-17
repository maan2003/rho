//! Active-window policy and durable transition replay. Full history stays
//! intact.
use std::sync::Arc;

use rho_core::{ContextBlock, InferenceResponseItem, ToolCallId};

use crate::ContextChange;

/// Keep a recent 40k-token suffix; reclaim enough for another such interval.
pub(super) const RETAIN_TOKENS: u64 = 40000;

pub(super) const MANUAL_COMPACTION: &str = "Manual compaction was requested. Earlier retention and preparation notices are canceled; do not resume their preparation.";
pub(super) const POLICY_CHANGED: &str = "The context-management role has changed. Earlier retention and preparation notices are canceled; do not resume their preparation. Existing history remains available through Python.";
pub(super) const EVICTED: &str = "Older tool exchanges were removed to free context space. Recent exchanges, conversation, and reasoning remain. Original history is available through `history` in Python.";

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
    limit: u64,
) -> Eviction {
    use std::collections::{BTreeMap, BTreeSet};
    let start = rho_core::context_window_start(history);
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
                for result in results {
                    let entry = calls.entry(result.call_id.clone()).or_default();
                    entry.0 = i;
                    entry.1 += text_tokens(&result.body.output)
                        + result.body.images.len() as u64 * 10000
                        + 8;
                    entry.3 = true;
                }
            }
            ContextBlock::ToolUpdate(update) => {
                let entry = calls.entry(update.call_id.clone()).or_default();
                entry.0 = i;
                entry.1 += text_tokens(&update.output) + update.images.len() as u64 * 10000 + 8;
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
    let needed = used.saturating_sub(limit.saturating_sub(RETAIN_TOKENS));
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
    fn parts(parts: &[rho_core::ContentPart]) -> u64 {
        parts
            .iter()
            .map(|part| match part {
                rho_core::ContentPart::Text { text } => text_tokens(text),
                rho_core::ContentPart::Image { .. } => 10000,
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
