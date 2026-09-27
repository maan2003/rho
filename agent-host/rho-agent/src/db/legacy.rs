//! Temporary decoder for pre-code-first native rows. No runtime writes these.
use rho_agent_types::UnixMs;
use rho_agent_types::transcript::{ContextBlock, PendingInferenceResponse};
use senax_encoder::{Decode, Encode};

use crate::{ContextChange, WakeFacts};

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum NativeEvent {
    RequestStarted {
        input: Vec<ContextBlock>,
        context: Option<ContextChange>,
        wake: Option<WakeFacts>,
        at: UnixMs,
    },
    ResponseFinished {
        /// Live completion commits one InferenceResponse entry; migrated
        /// records preserve all original entries and response boundaries.
        output: Vec<ContextBlock>,
        context_used: Option<u64>,
        usage: Option<crate::db::AgentUsageBucket>,
        at: UnixMs,
    },
    RequestFailed {
        partial: PendingInferenceResponse,
        error: String,
        retrying: bool,
        at: UnixMs,
    },
}

impl crate::AgentEvent<'_> {
    #[cfg(test)]
    pub fn native_event(&self) -> Option<&NativeEvent> {
        match self {
            Self::Native(event) => Some(event),
            _ => None,
        }
    }
}

// Temporary decoder for the pre-typed-report log format.
use crate::entry::{Block, MessageId, Notice, Party, ResponseUsage, Wake};
pub(super) mod provider;
use std::collections::HashSet;

use provider::{Call, CallResult, Carry};

use crate::inference::{Image, Usage};

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Entry {
    /// One model response and the calls it started.
    Step {
        at: UnixMs,
        calls: Vec<Call>,
        prose: String,
        carry: Carry,
        usage: Usage,
    },
    /// What one response cost, billed once, right after its step.
    Usage {
        at: UnixMs,
        usage: ResponseUsage,
    },
    /// What the model was shown when woken: news from the notebook, and the
    /// messages delivered with it.
    Woken {
        at: UnixMs,
        why: Wake,
        report: String,
        images: Vec<Image>,
        /// Messages this wake delivered into model context.
        messages: Vec<MessageId>,
        /// Messages handled without entering model context.
        acknowledged: Vec<MessageId>,
        /// The report as the answer to each call of the step before.
        results: Vec<CallResult>,
    },
    Received {
        at: UnixMs,
        id: MessageId,
        from: Party,
        body: Vec<Block>,
    },
    Sent {
        at: UnixMs,
        id: MessageId,
        to: Party,
        text: String,
    },
    Status {
        at: UnixMs,
        text: String,
    },
    /// Since when some task awaits `human.reply()`; `None` once none does.
    Awaiting {
        at: UnixMs,
        since: Option<UnixMs>,
    },
    /// Legacy snapshot, decoded for existing logs only. New runtimes publish
    /// live state.
    Activity {
        at: UnixMs,
        responding: bool,
        running_tasks: u32,
        checkin_at: Option<UnixMs>,
        archived: bool,
    },
    Notice {
        at: UnixMs,
        notice: Notice,
    },
    /// Provider compaction, asked for through the model's normal input.
    CompactionTrigger {
        at: UnixMs,
        /// A pure manual request needs no model answer after compaction.
        manual: bool,
    },
}

/// Read-only projection of historical requests for migration diagnostics.
/// In old replay, results for evicted calls were discarded; a report with no
/// retained results became one user item instead.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Item {
    Step(Vec<String>),
    Result(CallResult),
    User { text: String, images: Vec<Image> },
    CompactionTrigger,
}

pub(super) fn request(entries: &[Entry]) -> Vec<Item> {
    let start = entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Step { carry, .. } if carry.has_compaction()))
        .unwrap_or(0);
    let entries = &entries[start..];
    let mut messages = std::collections::HashMap::new();
    let mut items = Vec::new();
    for entry in entries {
        match entry {
            Entry::Received { id, from, body, .. } => {
                messages.insert(*id, (*from, body.clone()));
            }
            Entry::Step { carry, .. } => items.push(Item::Step(carry.call_ids())),
            Entry::Woken {
                report,
                images,
                results,
                messages: delivered,
                acknowledged,
                ..
            } => {
                items.push(Item::User {
                    text: report.clone(),
                    images: images.clone(),
                });
                items.extend(results.iter().cloned().map(Item::Result));
                for id in delivered {
                    if let Some((from, body)) = messages.remove(id) {
                        items.push(Item::User {
                            text: crate::agent::context::render_message(&from, &body),
                            images: body
                                .into_iter()
                                .filter_map(|b| match b {
                                    Block::Image(i) => Some(i),
                                    _ => None,
                                })
                                .collect(),
                        });
                    }
                }
                for id in acknowledged {
                    messages.remove(id);
                }
            }
            Entry::CompactionTrigger { .. } => items.push(Item::CompactionTrigger),
            _ => {}
        }
    }
    resolve(items)
}

fn resolve(items: Vec<Item>) -> Vec<Item> {
    let mut retained = HashSet::new();
    let mut out = Vec::new();
    let mut iter = items.into_iter().peekable();
    while let Some(item) = iter.next() {
        match item {
            Item::Step(calls) => {
                retained.extend(calls.iter().cloned());
                out.push(Item::Step(calls));
            }
            Item::User { text, images } if matches!(iter.peek(), Some(Item::Result(_))) => {
                let mut answered = false;
                while matches!(iter.peek(), Some(Item::Result(_))) {
                    let Item::Result(result) = iter.next().unwrap() else {
                        unreachable!()
                    };
                    if retained.contains(result.display_id()) {
                        answered = true;
                        out.push(Item::Result(result));
                    }
                }
                if !answered && (!text.is_empty() || !images.is_empty()) {
                    out.push(Item::User { text, images });
                }
            }
            Item::User { text, images } if text.is_empty() && images.is_empty() => {}
            item => out.push(item),
        }
    }
    out
}
