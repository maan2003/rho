//! Canonical per-agent transcript state and change summaries.
//!
//! Each agent's `UiAgentState` exists exactly once, here: the fold of its
//! mirror, with the runtime's live tail layered after it while a turn
//! runs. The tail is kept from the deltas the runtime tells; every change
//! returns a [`FrameSummary`] telling views the minimal region they must
//! re-render, so per-event cost is O(changed suffix), never O(session).

use std::collections::HashMap;
use std::sync::Arc;

use rho_agent_types::AgentId;

use crate::protocol::transcript::{
    InferenceState, Item, Live, QueuedItem, RuntimeState, StreamingResponse,
};
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
    MessageDraft { index: usize },
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

/// One agent's transcript: the fold, the live tail, what the user has not
/// yet sent, the agent's status line, and the four composed.
struct Layered {
    fold: UiAgentState,
    tail: Tail,
    unsent: Vec<Arc<UiBlock>>,
    status: Option<Arc<UiBlock>>,
    state: UiAgentState,
    committed_tools: std::collections::HashSet<String>,
}

impl Layered {
    fn refresh_committed_tools(&mut self) {
        self.committed_tools = self
            .fold
            .blocks
            .iter()
            .filter_map(|block| match block.as_ref() {
                UiBlock::Tool(tool) => Some(tool.id.clone()),
                _ => None,
            })
            .collect();
    }

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
        self.state.blocks.extend(self.tail.response.iter().flat_map(|response| &response.items)
            .filter(|item| !matches!(item, Item::ToolCall { id, .. } if self.committed_tools.contains(id)))
            .map(|item| {
                let mut block = block(item);
                if let UiBlock::Tool(tool) = &mut block {
                    tool.timing = self.fold.exec_timings.get(&tool.id).copied().unwrap_or_default();
                }
                Arc::new(block)
            }));
        if let Some(text) = &self.tail.draft {
            self.state
                .blocks
                .push(Arc::new(UiBlock::MessageDraft { text: text.clone() }));
        }
        self.state
            .blocks
            .extend(self.tail.queue.iter().cloned().map(Arc::new));
        self.state.blocks.extend(self.unsent.iter().cloned());
        self.state.blocks.extend(self.status.iter().cloned());
        self.state.status = match self.tail.runtime.as_ref() {
            Some(RuntimeState {
                inference: InferenceState::Responding,
                ..
            }) => UiAgentStatus::Streaming,
            Some(RuntimeState {
                inference: InferenceState::Retrying { at, .. },
                ..
            }) => UiAgentStatus::Retrying { at: *at },
            Some(RuntimeState {
                inference: InferenceState::Failed { .. },
                ..
            }) => UiAgentStatus::Error,
            Some(RuntimeState {
                running_tasks,
                archived: false,
                ..
            }) if *running_tasks > 0 => UiAgentStatus::ToolCalling { waiting: None },
            Some(_) => UiAgentStatus::Idle,
            None => self.fold.status,
        };
        self.state.runtime = self.tail.runtime.clone();
        self.state.context_used = self.fold.context_used;
        self.state.usage = self.fold.usage.clone();
        self.state.exec_timings = self.fold.exec_timings.clone();
    }
}

/// Runtime snapshots replace the response and state together. The Claude
/// queue is a separate live stream and survives a response replacement.
#[derive(Default)]
struct Tail {
    runtime: Option<RuntimeState>,
    response: Option<StreamingResponse>,
    draft: Option<String>,
    queue: Vec<UiBlock>,
}

impl Tail {
    fn apply(&mut self, live: Live) {
        match live {
            Live::Snapshot {
                state,
                response,
                draft,
            } => {
                self.runtime = Some(state);
                self.response = response;
                self.draft = draft;
            }
            Live::Queued { items } => self.queue = items.into_iter().map(queued_block).collect(),
        }
    }
}

fn queued_block(item: QueuedItem) -> UiBlock {
    match item {
        QueuedItem::Message { from, text } => UiBlock::QueuedMessage { text, sender: from },
        QueuedItem::Compaction => UiBlock::Notice {
            text: "compacting context".to_string(),
        },
    }
}

/// A streamed or committed response item as the transcript draws it.
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
            format,
        } => UiBlock::Tool(UiTool {
            timing: Default::default(),
            id: id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
            format: *format,
            preview: None,
            status: UiToolStatus::Running,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            // Set when the event carrying its result closes it.
        }),
    }
}

#[derive(Default)]
pub struct AgentStore {
    states: HashMap<AgentId, Layered>,
    /// Kept apart from `states` so a transcript forgotten and opened
    /// again still shows what waits to be sent.
    unsent: HashMap<AgentId, Vec<Arc<UiBlock>>>,
    status: HashMap<AgentId, Arc<UiBlock>>,
}

impl AgentStore {
    /// The transcript as folded from the mirror. The live tail, if any,
    /// stays on top of it.
    pub fn set_fold(&mut self, agent_id: AgentId, fold: UiAgentState) -> FrameSummary {
        self.change(agent_id, false, |layered| {
            layered.fold = fold;
            layered.refresh_committed_tools();
        })
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
        let layered = self.layered(agent_id);
        let from = delta.from.min(layered.fold.blocks.len());
        let open_before = turn_open(layered.state.status);
        for block in &layered.fold.blocks[from..] {
            if let UiBlock::Tool(tool) = block.as_ref() {
                layered.committed_tools.remove(&tool.id);
            }
        }
        layered.fold.blocks.truncate(from);
        layered.fold.blocks.extend(delta.blocks);
        for block in &layered.fold.blocks[from..] {
            if let UiBlock::Tool(tool) = block.as_ref() {
                layered.committed_tools.insert(tool.id.clone());
            }
        }
        layered.fold.status = delta.status;
        layered.fold.context_used = delta.context_used;
        layered.fold.usage = delta.usage;
        layered.fold.exec_timings = delta.exec_timings;
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
        self.change(agent_id, true, |layered| layered.tail.apply(live))
    }

    /// What the user wrote that no host has taken yet, oldest first.
    pub fn set_unsent(&mut self, agent_id: AgentId, texts: Vec<String>) -> FrameSummary {
        let unsent: Vec<_> = texts
            .into_iter()
            .map(|text| Arc::new(UiBlock::Unsent { text }))
            .collect();
        if unsent.is_empty() {
            self.unsent.remove(&agent_id);
        } else {
            self.unsent.insert(agent_id, unsent.clone());
        }
        // A transcript not yet open picks them up when it opens; opening
        // one here would pass for a transcript with nothing else in it.
        if !self.states.contains_key(&agent_id) {
            return FrameSummary::nothing();
        }
        self.change(agent_id, true, |layered| layered.unsent = unsent)
    }

    /// What the agent says it is doing, or `None` once it stops saying.
    pub fn set_status(&mut self, agent_id: AgentId, text: Option<String>) -> FrameSummary {
        let status = text
            .filter(|text| !text.is_empty())
            .map(|text| Arc::new(UiBlock::Status { text }));
        if self.status.get(&agent_id) == status.as_ref() {
            return FrameSummary::nothing();
        }
        match &status {
            Some(block) => self.status.insert(agent_id, block.clone()),
            None => self.status.remove(&agent_id),
        };
        if !self.states.contains_key(&agent_id) {
            return FrameSummary::nothing();
        }
        self.change(agent_id, true, |layered| layered.status = status)
    }

    /// Drop ephemeral status and response when transport is lost.
    pub fn disconnect(&mut self, agent_id: AgentId) -> FrameSummary {
        self.change(agent_id, true, |layered| {
            layered.tail.runtime = None;
            layered.tail.response = None;
            layered.tail.draft = None;
            layered.tail.queue.clear();
        })
    }

    pub fn get(&self, agent_id: &AgentId) -> Option<&UiAgentState> {
        self.states.get(agent_id).map(|layered| &layered.state)
    }

    /// Drops a transcript: the agent left this client's active set, or
    /// its agent host is gone.
    pub fn forget(&mut self, agent_id: AgentId) {
        self.states.remove(&agent_id);
    }

    fn layered(&mut self, agent_id: AgentId) -> &mut Layered {
        let unsent = &self.unsent;
        let status = &self.status;
        self.states.entry(agent_id).or_insert_with(|| Layered {
            fold: empty_state(),
            tail: Tail::default(),
            unsent: unsent.get(&agent_id).cloned().unwrap_or_default(),
            status: status.get(&agent_id).cloned(),
            state: empty_state(),
            committed_tools: Default::default(),
        })
    }

    fn change(
        &mut self,
        agent_id: AgentId,
        tail_only: bool,
        change: impl FnOnce(&mut Layered),
    ) -> FrameSummary {
        let layered = self.layered(agent_id);
        // The durable prefix cannot change on a live update. Keep its Arc
        // pointers in place and compare only the old and new live suffix:
        // O(tail blocks + their payload), independent of loaded history.
        // A replacement snapshot still has to materialize its whole tail.
        let from = if tail_only {
            layered.fold.blocks.len()
        } else {
            0
        };
        let old_status = layered.state.status;
        let old_tail = layered.state.blocks.split_off(from);
        change(layered);
        layered.compose_from(from);
        let mut summary = summarize(&old_tail, &layered.state.blocks[from..]);
        if let Some(index) = &mut summary.first_changed_block {
            *index += from;
        }
        match &mut summary.incremental {
            Some(
                IncrementalUpdate::AssistantText { index }
                | IncrementalUpdate::MessageDraft { index }
                | IncrementalUpdate::ReasoningText { index }
                | IncrementalUpdate::Tool { index },
            ) => *index += from,
            None => {}
        }
        // Elision gives the last fold in an open turn a limited visible tail,
        // so ending (or reopening) a turn re-renders its last block even when
        // no block content changed.
        if turn_open(old_status) != turn_open(layered.state.status)
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
        | UiAgentStatus::Retrying { .. }
        | UiAgentStatus::ToolCalling { .. }
        | UiAgentStatus::UnfinishedTurn { .. } => true,
        UiAgentStatus::Idle | UiAgentStatus::Error => false,
    }
}

fn empty_state() -> UiAgentState {
    UiAgentState {
        exec_timings: Default::default(),
        blocks: Vec::new(),
        status: UiAgentStatus::Idle,
        runtime: None,
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
            UiBlock::MessageDraft { .. } => Some(IncrementalUpdate::MessageDraft {
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
        .flatten()
        .or_else(|| {
            // The source changes with its preview. Conversation hides the
            // tool, so only the draft needs an edit; Activity will reject
            // this hint and update its visible tool instead.
            let draft = first_changed + 1;
            (old.len() == new.len()
                && draft < new.len()
                && matches!(
                    (&*old[first_changed], &*new[first_changed]),
                    (UiBlock::Tool(_), UiBlock::Tool(_))
                )
                && matches!(&*old[draft], UiBlock::MessageDraft { .. })
                && matches!(&*new[draft], UiBlock::MessageDraft { .. })
                && old[draft] != new[draft]
                && old[draft + 1..] == new[draft + 1..])
                .then_some(IncrementalUpdate::MessageDraft { index: draft })
        });
    FrameSummary {
        first_changed_block: Some(first_changed),
        incremental,
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentIdDomain, AgentPos, ExecMilestone, UnixMs};

    use super::*;
    use crate::fold::TranscriptFold;
    use crate::protocol::transcript::{ArgumentsFormat, TranscriptEvent};

    fn agent() -> AgentId {
        AgentId::from_counter(1, &AgentIdDomain(0)).unwrap()
    }
    fn text(value: &str) -> Item {
        Item::Text {
            text: value.into(),
            phase: None,
        }
    }
    fn call(id: &str) -> Item {
        Item::ToolCall {
            id: id.into(),
            name: "exec".into(),
            arguments: "x".into(),
            format: ArgumentsFormat::Text,
        }
    }
    fn snapshot(id: &str, items: Vec<Item>) -> Live {
        Live::Snapshot {
            state: RuntimeState {
                inference: InferenceState::Responding,
                ..Default::default()
            },
            response: Some(StreamingResponse {
                id: id.into(),
                items,
            }),
            draft: None,
        }
    }
    fn blocks(store: &AgentStore) -> Vec<UiBlock> {
        store
            .get(&agent())
            .unwrap()
            .blocks
            .iter()
            .map(|block| (**block).clone())
            .collect()
    }

    /// Unsent messages trail everything, the agent's own queue included,
    /// and are still there when a forgotten transcript opens again; setting
    /// them opens no transcript by itself.
    #[test]
    fn unsent_messages_trail_the_transcript_and_outlive_forgetting_it() {
        let mut store = AgentStore::default();
        store.set_unsent(agent(), vec!["later".into()]);
        assert!(store.get(&agent()).is_none(), "no transcript is opened");

        let mut fold = empty_state();
        fold.blocks
            .push(Arc::new(UiBlock::UserMessage { text: "hi".into() }));
        store.set_fold(agent(), fold.clone());
        store.apply_live(
            agent(),
            Live::Queued {
                items: vec![QueuedItem::Message {
                    from: None,
                    text: "queued".into(),
                }],
            },
        );
        let unsent = UiBlock::Unsent {
            text: "later".into(),
        };
        let queued = UiBlock::QueuedMessage {
            text: "queued".into(),
            sender: None,
        };
        let said = UiBlock::UserMessage { text: "hi".into() };
        assert_eq!(blocks(&store), [said.clone(), queued, unsent.clone()]);

        store.forget(agent());
        store.set_fold(agent(), fold);
        assert_eq!(blocks(&store), [said.clone(), unsent]);

        let summary = store.set_unsent(agent(), Vec::new());
        assert_eq!(summary.first_changed_block, Some(1));
        assert_eq!(blocks(&store), [said]);
    }

    #[test]
    fn snapshots_replace_shrinking_or_new_responses_and_repeat_idempotently() {
        let mut store = AgentStore::default();
        store.apply_live(
            agent(),
            snapshot("first", vec![text("a long answer"), call("one")]),
        );
        store.apply_live(agent(), snapshot("first", vec![text("short")]));
        assert_eq!(blocks(&store), vec![block(&text("short"))]);
        assert_eq!(
            store.apply_live(agent(), snapshot("first", vec![text("short")])),
            FrameSummary::nothing()
        );
        store.apply_live(agent(), snapshot("second", vec![text("new")]));
        assert_eq!(blocks(&store), vec![block(&text("new"))]);
    }

    #[test]
    fn live_changes_touch_only_the_suffix_of_a_long_fold() {
        let mut store = AgentStore::default();
        let mut fold = empty_state();
        fold.blocks = (0..1_000)
            .map(|index| {
                Arc::new(UiBlock::Notice {
                    text: index.to_string(),
                })
            })
            .collect();
        let first = Arc::clone(&fold.blocks[0]);
        let last = Arc::clone(&fold.blocks[999]);
        store.set_fold(agent(), fold);
        assert_eq!(
            store.apply_live(agent(), snapshot("response", vec![text("part")])),
            FrameSummary {
                first_changed_block: Some(1_000),
                incremental: None,
            }
        );
        assert_eq!(
            store.apply_live(agent(), snapshot("response", vec![text("partial")])),
            FrameSummary {
                first_changed_block: Some(1_000),
                incremental: Some(IncrementalUpdate::AssistantText { index: 1_000 }),
            }
        );
        let state = store.get(&agent()).unwrap();
        assert_eq!(state.blocks.len(), 1_001);
        assert!(Arc::ptr_eq(&state.blocks[0], &first));
        assert!(Arc::ptr_eq(&state.blocks[999], &last));
        // Ending the turn also changes the final folded row's elision.
        assert_eq!(
            store.disconnect(agent()),
            FrameSummary {
                first_changed_block: Some(999),
                incremental: None,
            }
        );
        assert_eq!(store.get(&agent()).unwrap().blocks.len(), 1_000);
    }

    #[test]
    fn runtime_changes_do_not_erase_response_and_none_removes_it() {
        let mut store = AgentStore::default();
        store.apply_live(agent(), snapshot("first", vec![text("answer")]));
        store.apply_live(
            agent(),
            Live::Snapshot {
                state: RuntimeState {
                    running_tasks: 2,
                    awaiting_human: true,
                    ..Default::default()
                },
                response: Some(StreamingResponse {
                    id: "first".into(),
                    items: vec![text("answer")],
                }),
                draft: None,
            },
        );
        assert_eq!(blocks(&store), vec![block(&text("answer"))]);
        assert_eq!(
            store
                .get(&agent())
                .unwrap()
                .runtime
                .as_ref()
                .unwrap()
                .running_tasks,
            2
        );
        store.apply_live(
            agent(),
            Live::Snapshot {
                state: RuntimeState::default(),
                response: None,
                draft: None,
            },
        );
        assert!(blocks(&store).is_empty());
        assert_eq!(store.get(&agent()).unwrap().status, UiAgentStatus::Idle);
    }

    #[test]
    fn growing_draft_targets_its_block_when_the_tool_source_also_changes() {
        let mut store = AgentStore::default();
        let live = |source: &str, draft: &str| Live::Snapshot {
            state: RuntimeState {
                inference: InferenceState::Responding,
                ..Default::default()
            },
            response: Some(StreamingResponse {
                id: "response".into(),
                items: vec![Item::ToolCall {
                    id: "call".into(),
                    name: "exec".into(),
                    arguments: source.into(),
                    format: ArgumentsFormat::Text,
                }],
            }),
            draft: Some(draft.into()),
        };
        store.apply_live(agent(), live("human.send('Hel", "Hel"));
        assert_eq!(
            store.apply_live(agent(), live("human.send('Hello", "Hello")),
            FrameSummary {
                first_changed_block: Some(0),
                incremental: Some(IncrementalUpdate::MessageDraft { index: 1 }),
            },
            "the tool changes too, but Conversation hides it"
        );
        assert_eq!(
            store.apply_live(agent(), live("human.send('Hello')", "Hello")),
            FrameSummary {
                first_changed_block: Some(0),
                incremental: Some(IncrementalUpdate::Tool { index: 0 }),
            },
            "Activity must still update the tool if only its source changes"
        );
    }

    #[test]
    fn draft_survives_source_completion_and_unrelated_sends_until_cell_publishes() {
        let mut store = AgentStore::default();
        let live = |source: Option<&str>, draft: Option<&str>| Live::Snapshot {
            state: RuntimeState {
                running_tasks: 1,
                ..Default::default()
            },
            response: source.map(|source| StreamingResponse {
                id: "response".into(),
                items: vec![Item::ToolCall {
                    id: "call".into(),
                    name: "exec".into(),
                    arguments: source.into(),
                    format: ArgumentsFormat::Text,
                }],
            }),
            draft: draft.map(str::to_owned),
        };
        store.apply_live(agent(), live(Some("human.send('Hel"), Some("Hel")));
        let text = |store: &AgentStore| {
            store
                .get(&agent())
                .unwrap()
                .blocks
                .iter()
                .find_map(|block| match block.as_ref() {
                    UiBlock::MessageDraft { text } => Some(text.clone()),
                    _ => None,
                })
        };
        assert_eq!(text(&store), Some("Hel".into()));
        store.apply_live(agent(), live(Some("human.send('Hello')"), Some("Hello")));
        assert_eq!(
            text(&store),
            Some("Hello".into()),
            "closing the call must not withdraw it"
        );
        store.apply_live(agent(), live(None, Some("Hello")));
        assert_eq!(
            text(&store),
            Some("Hello".into()),
            "provider completion must not withdraw it"
        );

        let mut fold = TranscriptFold::default();
        fold.tell(
            AgentPos(0),
            &TranscriptEvent::MessageSent {
                to: None,
                text: "other cell".into(),
                kind: rho_agent_types::SendKind::Result,
                at: UnixMs(1),
            },
        );
        store.apply_fold_delta(agent(), fold.delta().unwrap());
        assert_eq!(
            text(&store),
            Some("Hello".into()),
            "an older cell cannot withdraw it"
        );
        store.apply_live(agent(), live(None, None));
        assert_eq!(
            text(&store),
            None,
            "host withdraws after the latest cell sends"
        );
        assert!(matches!(store.get(&agent()).unwrap().blocks[0].as_ref(),
            UiBlock::MessageSent { text, .. } if text == "other cell"));
    }

    #[test]
    fn committed_tool_is_not_repeated_while_snapshot_remains() {
        let mut store = AgentStore::default();
        store.apply_live(agent(), snapshot("first", vec![call("one"), call("two")]));
        let mut fold = TranscriptFold::default();
        fold.tell(
            AgentPos(0),
            &TranscriptEvent::Replied {
                items: vec![call("one")],
                compacted: false,
                usage: None,
                context_used: None,
                at: UnixMs(1),
            },
        );
        store.apply_fold_delta(agent(), fold.delta().unwrap());
        let ids: Vec<_> = store
            .get(&agent())
            .unwrap()
            .blocks
            .iter()
            .filter_map(|block| match block.as_ref() {
                UiBlock::Tool(tool) => Some(tool.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["one", "two"]);
        store.apply_live(
            agent(),
            Live::Snapshot {
                state: RuntimeState::default(),
                response: None,
                draft: None,
            },
        );
        let ids: Vec<_> = store
            .get(&agent())
            .unwrap()
            .blocks
            .iter()
            .filter_map(|block| match block.as_ref() {
                UiBlock::Tool(tool) => Some(tool.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["one"]);
    }

    #[test]
    fn disconnect_clears_runtime_and_tail() {
        let mut store = AgentStore::default();
        store.apply_live(
            agent(),
            Live::Snapshot {
                state: RuntimeState {
                    running_tasks: 3,
                    ..Default::default()
                },
                response: Some(StreamingResponse {
                    id: "first".into(),
                    items: vec![text("live")],
                }),
                draft: None,
            },
        );
        store.disconnect(agent());
        let state = store.get(&agent()).unwrap();
        assert!(state.runtime.is_none());
        assert!(state.blocks.is_empty());
        assert_eq!(state.status, UiAgentStatus::Idle);
    }

    #[test]
    fn durable_provider_timing_reaches_the_live_tail() {
        let mut store = AgentStore::default();
        store.apply_live(agent(), snapshot("first", vec![call("one")]));
        let mut fold = TranscriptFold::default();
        fold.tell(
            AgentPos(0),
            &TranscriptEvent::ExecObserved {
                id: "one".into(),
                milestone: ExecMilestone::FirstBlock,
                at: UnixMs(10),
            },
        );
        store.apply_fold_delta(agent(), fold.delta().unwrap());
        let UiBlock::Tool(tool) = &*store.get(&agent()).unwrap().blocks[0] else {
            panic!("missing tool")
        };
        assert_eq!(tool.timing.first_block_at, Some(UnixMs(10)));
    }
}
