//! Canonical per-agent transcript state and change summaries.
//!
//! Each agent's `UiAgentState` exists exactly once, here: the fold of its
//! mirror, with the runtime's live tail layered after it while a turn
//! runs. The tail is kept from the deltas the runtime tells; every change
//! returns a [`FrameSummary`] telling views the minimal region they must
//! re-render, so per-event cost is O(changed suffix), never O(session).

use std::collections::HashMap;
use std::sync::Arc;

use rho_ui_proto::AgentId;
use rho_ui_proto::mirror::{Item, Live};

use crate::state::{UiAgentState, UiAgentStatus, UiBlock, UiTool, UiToolStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSummary {
    /// First block index whose rendered content may have changed; everything
    /// from here to the end of the transcript needs re-rendering. `None`
    /// means nothing visible changed.
    pub first_changed_block: Option<usize>,
    /// A single block that may be safe to apply as a targeted rendered-text
    /// patch. Merging updates to different blocks drops this hint and falls
    /// back to suffix re-rendering.
    pub incremental: Option<IncrementalUpdate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncrementalUpdate {
    AssistantText { index: usize },
    ReasoningText { index: usize },
    Tool { index: usize },
}

impl FrameSummary {
    pub fn everything() -> Self {
        Self {
            first_changed_block: Some(0),
            incremental: None,
        }
    }

    pub fn nothing() -> Self {
        Self {
            first_changed_block: None,
            incremental: None,
        }
    }

    /// Combines two summaries into one covering both changes, so hidden
    /// views can accumulate frames and render once when shown.
    pub fn merge(self, other: Self) -> Self {
        let incremental = match (self.incremental, other.incremental) {
            (Some(a), Some(b)) if a == b => Some(a),
            (Some(a), None) if other.first_changed_block.is_none() => Some(a),
            (None, Some(b)) if self.first_changed_block.is_none() => Some(b),
            _ => None,
        };
        Self {
            first_changed_block: match (self.first_changed_block, other.first_changed_block) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            incremental,
        }
    }
}

/// One agent's transcript: the fold, the live tail, and the two composed.
struct Layered {
    fold: UiAgentState,
    tail: Tail,
    state: UiAgentState,
}

impl Layered {
    /// Composes from one index on, leaving the blocks before it as the
    /// pointers they already were.
    fn compose_from(&mut self, from: usize) {
        let from = from
            .min(self.state.blocks.len())
            .min(self.fold.blocks.len());
        self.state.blocks.truncate(from);
        self.state
            .blocks
            .extend(self.fold.blocks[from..].iter().cloned());
        self.state.blocks.extend(
            self.tail
                .items
                .iter()
                .flatten()
                .map(|item| Arc::new(block(item))),
        );
        self.state.status = match self.tail.phase {
            Phase::Requesting => UiAgentStatus::Streaming,
            Phase::Waiting(until) => UiAgentStatus::ToolCalling { waiting: until },
            Phase::Idle | Phase::Unknown => self.fold.status,
        };
        self.state.context_used = self.fold.context_used;
        self.state.usage = self.fold.usage.clone();
    }

    fn compose(&mut self) {
        let mut state = self.fold.clone();
        state.blocks.extend(
            self.tail
                .items
                .iter()
                .flatten()
                .map(|item| Arc::new(block(item))),
        );
        state.status = match self.tail.phase {
            Phase::Requesting => UiAgentStatus::Streaming,
            Phase::Waiting(until) => UiAgentStatus::ToolCalling { waiting: until },
            Phase::Idle | Phase::Unknown => state.status,
        };
        self.state = state;
    }
}

/// What the runtime has past the log, kept from its deltas. Every phase
/// message empties the items: a request starts with none, and once calls
/// run or the turn ends the row carries the response.
#[derive(Default)]
struct Tail {
    phase: Phase,
    /// By the runtime's index; `None` where nothing was told.
    items: Vec<Option<Item>>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    /// Nothing told yet: the fold's status stands.
    #[default]
    Unknown,
    Requesting,
    Waiting(Option<rho_core::UnixMs>),
    Idle,
}

impl Tail {
    fn apply(&mut self, live: Live) {
        match live {
            Live::Requesting => {
                self.phase = Phase::Requesting;
                self.items.clear();
            }
            Live::Item { index, item } => {
                let index = index as usize;
                if self.items.len() <= index {
                    self.items.resize(index + 1, None);
                }
                self.items[index] = Some(item);
            }
            // An index not held is from before this client was told the
            // tail whole; the whole item follows.
            Live::Appended { index, text } => {
                if let Some(Some(item)) = self.items.get_mut(index as usize) {
                    match item {
                        Item::Text { text: held, .. } | Item::Reasoning { text: held } => {
                            held.push_str(&text)
                        }
                        Item::ToolCall { arguments, .. } => arguments.push_str(&text),
                    }
                }
            }
            Live::Waiting { until } => {
                self.phase = Phase::Waiting(until);
                self.items.clear();
            }
            Live::Idle => {
                self.phase = Phase::Idle;
                self.items.clear();
            }
        }
    }
}

/// An in-flight item as the transcript draws it.
pub fn block(item: &Item) -> UiBlock {
    match item {
        Item::Text { text, phase } => UiBlock::AssistantMessage {
            text: text.clone(),
            phase: phase.map(Into::into),
        },
        Item::Reasoning { text } => UiBlock::Reasoning { text: text.clone() },
        Item::ToolCall {
            id,
            name,
            arguments,
        } => UiBlock::Tool(UiTool {
            id: id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
            preview: None,
            status: UiToolStatus::Running,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            metadata: None,
        }),
    }
}

#[derive(Default)]
pub struct AgentStore {
    states: HashMap<AgentId, Layered>,
}

impl AgentStore {
    /// The transcript as folded from the mirror. The live tail, if any,
    /// stays on top of it.
    pub fn set_fold(&mut self, agent_id: AgentId, fold: UiAgentState) -> FrameSummary {
        self.change(agent_id, |layered| layered.fold = fold)
    }

    /// One telling of the mirror, as the suffix it moved. The reader is
    /// told where the transcript first differs rather than handed a state
    /// to compare, so a row appended to a long transcript costs the rows
    /// it appended and not the ones above them.
    pub fn apply_fold_delta(
        &mut self,
        agent_id: AgentId,
        delta: crate::fold::FoldDelta,
    ) -> FrameSummary {
        let layered = self.states.entry(agent_id).or_insert_with(|| Layered {
            fold: empty_state(),
            tail: Tail::default(),
            state: empty_state(),
        });
        let from = delta.from.min(layered.fold.blocks.len());
        let open_before = turn_open(layered.state.status);
        layered.fold.blocks.truncate(from);
        layered.fold.blocks.extend(delta.blocks);
        layered.fold.status = delta.status;
        layered.fold.context_used = delta.context_used;
        layered.fold.usage = delta.usage;
        layered.compose_from(from);
        let mut summary = FrameSummary {
            first_changed_block: Some(from),
            incremental: None,
        };
        // Elision gives the last fold in an open turn a limited visible
        // tail, so ending or reopening a turn re-renders its last block.
        if open_before != turn_open(layered.state.status) && !layered.state.blocks.is_empty() {
            summary = summary.merge(FrameSummary {
                first_changed_block: Some(layered.state.blocks.len() - 1),
                incremental: None,
            });
        }
        summary
    }

    /// One change to what the runtime has past the mirror.
    pub fn apply_live(&mut self, agent_id: AgentId, live: Live) -> FrameSummary {
        self.change(agent_id, |layered| layered.tail.apply(live))
    }

    pub fn get(&self, agent_id: &AgentId) -> Option<&UiAgentState> {
        self.states.get(agent_id).map(|layered| &layered.state)
    }

    /// Drops a transcript: the agent left this client's active set, or
    /// its daemon is gone.
    pub fn forget(&mut self, agent_id: AgentId) {
        self.states.remove(&agent_id);
    }

    fn change(&mut self, agent_id: AgentId, change: impl FnOnce(&mut Layered)) -> FrameSummary {
        let layered = self.states.entry(agent_id).or_insert_with(|| Layered {
            fold: empty_state(),
            tail: Tail::default(),
            state: empty_state(),
        });
        let old = std::mem::replace(&mut layered.state, empty_state());
        change(layered);
        layered.compose();
        let mut summary = summarize(&old.blocks, &layered.state.blocks);
        // Elision gives the last fold in an open turn a limited visible tail,
        // so ending (or reopening) a turn re-renders its last block even when
        // no block content changed.
        if turn_open(old.status) != turn_open(layered.state.status)
            && !layered.state.blocks.is_empty()
        {
            summary = summary.merge(FrameSummary {
                first_changed_block: Some(layered.state.blocks.len() - 1),
                incremental: None,
            });
        }
        summary
    }
}

/// Whether the agent is still producing the last turn; while open, the final
/// working fold keeps a limited visible tail.
pub fn turn_open(status: UiAgentStatus) -> bool {
    match status {
        UiAgentStatus::Streaming
        | UiAgentStatus::ToolCalling { .. }
        | UiAgentStatus::UnfinishedTurn { .. } => true,
        UiAgentStatus::Idle | UiAgentStatus::Error | UiAgentStatus::Unloaded => false,
    }
}

fn empty_state() -> UiAgentState {
    UiAgentState {
        blocks: Vec::new(),
        status: UiAgentStatus::Idle,
        context_used: None,
        usage: Default::default(),
    }
}

/// What changed between two block lists: the first index that differs,
/// and whether it is one block growing in place. Blocks the fold kept
/// are the same pointer on both sides, which `Arc`'s equality sees first.
fn summarize(old: &[Arc<UiBlock>], new: &[Arc<UiBlock>]) -> FrameSummary {
    let shared = old.len().min(new.len());
    let first_changed = (0..shared).find(|index| old[*index] != new[*index]);
    let first_changed = match first_changed {
        Some(index) => index,
        None if old.len() == new.len() => return FrameSummary::nothing(),
        None => shared,
    };
    let incremental = (old.len() == new.len()
        && old[first_changed + 1..] == new[first_changed + 1..])
        .then(|| match &*new[first_changed] {
            UiBlock::AssistantMessage { .. } => Some(IncrementalUpdate::AssistantText {
                index: first_changed,
            }),
            UiBlock::Reasoning { .. } => Some(IncrementalUpdate::ReasoningText {
                index: first_changed,
            }),
            UiBlock::Tool(_) => Some(IncrementalUpdate::Tool {
                index: first_changed,
            }),
            _ => None,
        })
        .flatten();
    FrameSummary {
        first_changed_block: Some(first_changed),
        incremental,
    }
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::AgentIdDomain;

    use super::*;

    fn agent() -> AgentId {
        AgentId::from_counter(1, &AgentIdDomain(0)).unwrap()
    }

    fn fold(texts: &[&str]) -> UiAgentState {
        UiAgentState {
            blocks: texts
                .iter()
                .map(|text| {
                    Arc::new(UiBlock::UserMessage {
                        text: (*text).to_owned(),
                    })
                })
                .collect(),
            ..empty_state()
        }
    }

    #[test]
    fn the_live_tail_rides_on_the_fold() {
        let mut store = AgentStore::default();
        assert_eq!(
            store.set_fold(agent(), fold(&["one", "two"])),
            FrameSummary {
                first_changed_block: Some(0),
                incremental: None,
            }
        );
        let summary = store.apply_live(agent(), Live::Requesting);
        assert_eq!(summary.first_changed_block, Some(1));
        assert_eq!(
            store.get(&agent()).unwrap().status,
            UiAgentStatus::Streaming
        );
        let summary = store.apply_live(
            agent(),
            Live::Item {
                index: 0,
                item: Item::Text {
                    text: "th".to_owned(),
                    phase: None,
                },
            },
        );
        assert_eq!(summary.first_changed_block, Some(2));
        let state = store.get(&agent()).unwrap();
        assert_eq!(state.blocks.len(), 3);
        assert_eq!(state.status, UiAgentStatus::Streaming);

        // The same block growing is an incremental update.
        let summary = store.apply_live(
            agent(),
            Live::Appended {
                index: 0,
                text: "ree".to_owned(),
            },
        );
        assert_eq!(
            summary.incremental,
            Some(IncrementalUpdate::AssistantText { index: 2 })
        );
        assert_eq!(
            *store.get(&agent()).unwrap().blocks[2],
            UiBlock::AssistantMessage {
                text: "three".to_owned(),
                phase: None
            }
        );

        // An append to an index never told is from before this client
        // was told the tail; it is dropped.
        assert_eq!(
            store.apply_live(
                agent(),
                Live::Appended {
                    index: 3,
                    text: "x".to_owned()
                }
            ),
            FrameSummary::nothing()
        );

        // The row lands first, then the tail says the turn is over.
        store.set_fold(agent(), fold(&["one", "two", "three"]));
        assert_eq!(store.get(&agent()).unwrap().blocks.len(), 4);
        let summary = store.apply_live(agent(), Live::Idle);
        assert_eq!(summary.first_changed_block, Some(2));
        let state = store.get(&agent()).unwrap();
        assert_eq!(state.blocks.len(), 3);
        assert_eq!(state.status, UiAgentStatus::Idle);

        // An unchanged fold is no change at all.
        assert_eq!(
            store.set_fold(agent(), fold(&["one", "two", "three"])),
            FrameSummary::nothing()
        );
    }
}
