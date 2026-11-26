//! The transcripts this client holds open.
//!
//! An agent's transcript is two things joined: the fold of its workset rows
//! and the runtime's live tail. Both land in one rendered state per agent.
//! A screen draws a [`Scope`]: a workset's agents, or one of them. Its
//! blocks are its agents' merged in the order of the workset rows that
//! told them, with a heading wherever the work turns from one agent to
//! another. Which agents are held open is the same question as which
//! agents this client asks the model thread for rows about, so it is
//! answered here.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use rho_agent_types::{AgentId, Wpos};
use rho_agents_client::TranscriptFold;
use rho_agents_client::protocol::transcript::Draft;
use rho_agents_client::protocol::worksets::LogEvent;
use rho_agents_client::state::{UiAgentState, UiAgentStatus, UiBlock};
use rho_agents_client::store::{AgentStore, FrameSummary, IncrementalUpdate, summarize, turn_open};

use crate::render::block_visible;
use crate::system::Scope;

/// One change to an agent's transcript: a delta to the runtime's live
/// tail, or the fold of its mirror made again.
pub enum TranscriptFrame {
    Live(rho_agents_client::protocol::transcript::Live),
    /// The mirror's fold, whole. What an agent's first read hands, and
    /// nothing else: a transcript is handed once and appended to after.
    Fold(UiAgentState),
    /// The rows one telling of the mirror moved.
    Folded(rho_agents_client::fold::FoldDelta),
    /// What the agent's runtime is doing, as its host tells every agent's.
    Runtime(rho_agents_client::protocol::transcript::RuntimeState),
}

/// What a frame did, for a caller deciding what to redraw.
pub struct FrameChange {
    /// What moved in each view that shows the agent.
    pub views: Vec<(Scope, FrameSummary)>,
    /// The frame came from the runtime, so the agent is live.
    pub was_live: bool,
}

#[derive(Default)]
pub struct Transcripts {
    /// The rendered state per agent.
    store: AgentStore,
    /// The transcript fold of every agent held open, in memory, so a row
    /// folds into it without waiting on the disk copy.
    open: HashMap<AgentId, TranscriptFold>,
    /// What each open screen draws.
    views: HashMap<Scope, View>,
}

/// Where a block stands among the workset's rows. What the runtime is
/// still saying comes after everything told.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Order {
    Told(Wpos),
    Live,
}

/// A screen's blocks: its agents' transcripts, merged.
struct View {
    /// The agent whose work needs no heading: the one the screen is
    /// named after and whose brief it shows.
    lead: AgentId,
    /// The agents merged; equal places go to the one listed first.
    agents: Vec<AgentId>,
    state: UiAgentState,
    /// Where each block of each agent stands in `state.blocks`.
    placed: HashMap<AgentId, Vec<usize>>,
    /// Where each heading stands in `state.blocks`, and whose it is.
    headings: Vec<(usize, AgentId)>,
}

impl View {
    fn new(lead: AgentId, agents: Vec<AgentId>) -> Self {
        Self {
            lead,
            placed: agents.iter().map(|agent| (*agent, Vec::new())).collect(),
            agents,
            state: UiAgentState {
                blocks: Vec::new(),
                status: UiAgentStatus::Idle,
                runtime: None,
                context_used: None,
                usage: Default::default(),
            },
            headings: Vec::new(),
        }
    }

    /// The first block that can move when `agent`'s blocks move from
    /// `from` on: the one after its last block that did not. Every block
    /// before it was placed without looking at the blocks that moved.
    fn start_of(&self, agent: AgentId, from: usize) -> Option<usize> {
        let placed = self.placed.get(&agent)?;
        Some(match from.checked_sub(1) {
            None => 0,
            Some(kept) => placed
                .get(kept)
                .map_or(self.state.blocks.len(), |at| at + 1),
        })
    }

    /// Merges again from block `from` on, keeping the blocks before it as
    /// the pointers they are. Answers what moved.
    fn merge_from(
        &mut self,
        from: usize,
        store: &AgentStore,
        folds: &HashMap<AgentId, TranscriptFold>,
    ) -> FrameSummary {
        let from = from.min(self.state.blocks.len());
        let old_status = self.state.status;
        let old_tail = self.state.blocks.split_off(from);
        let kept = self.headings.partition_point(|(at, _)| *at < from);
        self.headings.truncate(kept);
        let mut speaker = self.headings.last().map_or(self.lead, |(_, agent)| *agent);
        let sources: Vec<(AgentId, &[Arc<UiBlock>])> = self
            .agents
            .iter()
            .map(|agent| {
                let blocks = store.get(agent).map_or(&[][..], |state| &state.blocks[..]);
                (*agent, blocks)
            })
            .collect();
        let mut heads: Vec<usize> = sources
            .iter()
            .map(|(agent, _)| {
                let placed = self.placed.entry(*agent).or_default();
                let held = placed.partition_point(|at| *at < from);
                placed.truncate(held);
                held
            })
            .collect();
        let order = |agent: &AgentId, index: usize| {
            folds
                .get(agent)
                .and_then(|fold| fold.position(index))
                .map_or(Order::Live, Order::Told)
        };
        let mut last = None;
        loop {
            let next = sources
                .iter()
                .enumerate()
                .filter(|(rank, (_, blocks))| heads[*rank] < blocks.len())
                .min_by_key(|(rank, (agent, _))| (order(agent, heads[*rank]), *rank))
                .map(|(rank, _)| rank);
            let Some(rank) = next else {
                break;
            };
            let (agent, blocks) = sources[rank];
            let block = &blocks[heads[rank]];
            if agent != speaker && block_visible(block) {
                self.headings.push((self.state.blocks.len(), agent));
                self.state
                    .blocks
                    .push(Arc::new(UiBlock::AgentHeading { agent }));
                speaker = agent;
            }
            if let Some(placed) = self.placed.get_mut(&agent) {
                placed.push(self.state.blocks.len());
            }
            self.state.blocks.push(block.clone());
            heads[rank] += 1;
            last = Some(agent);
        }
        // The screen is as busy as whoever spoke last; with nothing said
        // yet, as its lead.
        let last = self
            .agents
            .iter()
            .filter_map(|agent| Some((*self.placed.get(agent)?.last()?, *agent)))
            .max()
            .map(|(_, agent)| agent)
            .or(last)
            .unwrap_or(self.lead);
        if let Some(state) = store.get(&last) {
            self.state.status = state.status;
            self.state.runtime = state.runtime.clone();
            self.state.context_used = state.context_used;
            self.state.usage = state.usage.clone();
        } else {
            self.state.status = UiAgentStatus::Idle;
            self.state.runtime = None;
        }
        let mut summary = summarize(&old_tail, &self.state.blocks[from..]);
        if let Some(index) = &mut summary.first_changed_block {
            *index += from;
        }
        if let Some(
            IncrementalUpdate::AssistantText { index }
            | IncrementalUpdate::MessageDraft { index }
            | IncrementalUpdate::ReasoningText { index }
            | IncrementalUpdate::Tool { index },
        ) = &mut summary.incremental
        {
            *index += from;
        }
        // Ending or opening a turn changes how its last block is drawn.
        if turn_open(old_status) != turn_open(self.state.status) && !self.state.blocks.is_empty() {
            summary = summary.merge(FrameSummary {
                first_changed_block: Some(self.state.blocks.len() - 1),
                incremental: None,
            });
        }
        summary
    }
}

impl Transcripts {
    pub fn state(&self, agent_id: &AgentId) -> Option<&UiAgentState> {
        self.store.get(agent_id)
    }

    pub fn is_open(&self, agent_id: &AgentId) -> bool {
        self.open.contains_key(agent_id)
    }

    /// The agents whose rows this client wants: the transcripts it has
    /// open. Everything else it hears as a digest.
    pub fn open_agents(&self) -> BTreeSet<AgentId> {
        self.open.keys().copied().collect()
    }

    /// Every agent whose live tail has a draft, and the draft.
    pub fn drafts(&self) -> impl Iterator<Item = (AgentId, &Draft)> + '_ {
        self.store.drafts()
    }

    pub fn draft(&self, agent_id: &AgentId) -> Option<&Draft> {
        self.store.draft(agent_id)
    }

    /// Drops the live tails of agents that have no transcript open and
    /// that `keep` does not name: no live frame comes for them any more,
    /// so what their tail says has gone stale.
    pub fn forget_live_except(&mut self, keep: impl Fn(AgentId) -> bool) {
        let open = &self.open;
        self.store
            .retain(|agent_id| open.contains_key(&agent_id) || keep(agent_id));
    }

    /// The agent whose block stands at `block` on the screen of `scope`,
    /// and the workset row that told it; `None` for a heading, or what the
    /// runtime is still saying.
    pub fn told_at(&self, scope: &Scope, block: usize) -> Option<(AgentId, Wpos)> {
        let view = self.views.get(scope)?;
        view.placed.iter().find_map(|(agent, placed)| {
            let index = placed.binary_search(&block).ok()?;
            Some((*agent, self.open.get(agent)?.position(index)?))
        })
    }

    /// What the screen of `scope` draws, once [`Self::show`] made it.
    pub fn view(&self, scope: &Scope) -> Option<&UiAgentState> {
        self.views.get(scope).map(|view| &view.state)
    }

    /// Shows `scope` as the merge of `agents`, headed by `lead`, each
    /// opened with [`Self::seed`] first. A view already showing them is
    /// left as it is; one whose agents changed is merged again. Answers
    /// what moved.
    pub fn show(&mut self, scope: Scope, lead: AgentId, agents: Vec<AgentId>) -> FrameSummary {
        if let Some(view) = self.views.get(&scope)
            && view.lead == lead
            && view.agents == agents
        {
            return FrameSummary::nothing();
        }
        let old = self
            .views
            .remove(&scope)
            .map(|view| view.state.blocks)
            .unwrap_or_default();
        let mut view = View::new(lead, agents);
        view.merge_from(0, &self.store, &self.open);
        let summary = summarize(&old, &view.state.blocks);
        self.views.insert(scope, view);
        summary
    }

    /// Lets go of a screen's view; the agents it showed stay open until
    /// [`Self::forget`].
    pub fn hide(&mut self, scope: &Scope) {
        self.views.remove(scope);
    }

    /// Lets go of one agent's transcript, held state and all.
    pub fn forget(&mut self, agent_id: AgentId) {
        self.open.remove(&agent_id);
        self.store.forget(agent_id);
    }

    /// Opens an agent's transcript from what the client has read of it:
    /// its rows. An agent with nothing read yet opens empty, so the rows
    /// that follow land in it. The live frame rides on its tail.
    ///
    /// Answers what moved on each view that shows the agent when it
    /// opened one, and `None` for an agent already open, which is left as
    /// it was.
    pub fn seed(
        &mut self,
        agent_id: AgentId,
        rows: &[(Wpos, LogEvent)],
    ) -> Option<Vec<(Scope, FrameSummary)>> {
        if self.open.contains_key(&agent_id) {
            return None;
        }
        let mut fold = TranscriptFold::new(agent_id, rows);
        fold.delta();
        let state = fold.state();
        // In before the views merge it: they order its blocks by the fold.
        self.open.insert(agent_id, fold);
        // The one time a transcript is handed whole: nothing was here to
        // append to. Every telling after this hands the rows it moved.
        Some(self.apply(agent_id, TranscriptFrame::Fold(state)).views)
    }

    /// An open transcript folded again from everything read of it, when
    /// its log's past has been read: the past came after rows newer than
    /// it, so it is put in order with them rather than appended.
    /// Answers the state to land as a [`TranscriptFrame::Fold`]; nothing
    /// for an agent not open.
    pub fn reread(&mut self, agent_id: AgentId, rows: &[(Wpos, LogEvent)]) -> Option<UiAgentState> {
        let fold = self.open.get_mut(&agent_id)?;
        *fold = TranscriptFold::new(agent_id, rows);
        // Handed whole; the rows told after this append to it.
        fold.delta();
        Some(fold.state())
    }

    /// Rows of an agent whose transcript is open, folded into the copy
    /// behind it. Positions already held are skipped, so what the reader's
    /// own open read already picked up costs nothing.
    /// Answers with the rows the telling moved, and nothing when it moved
    /// none.
    pub fn refold(
        &mut self,
        agent_id: AgentId,
        rows: &[(Wpos, LogEvent)],
    ) -> Option<rho_agents_client::fold::FoldDelta> {
        let fold = self.open.get_mut(&agent_id)?;
        let mut refolded = false;
        for (pos, event) in rows {
            refolded |= fold.tell(*pos, event);
        }
        if !refolded {
            return None;
        }
        fold.delta()
    }

    /// Remove ephemeral live state while retaining mirrored history.
    pub fn disconnect(&mut self, agent_id: AgentId) -> FrameSummary {
        self.store.disconnect(agent_id)
    }

    /// Lands one frame on an agent's rendered state, and on every view
    /// that shows the agent.
    pub fn apply(&mut self, agent_id: AgentId, frame: TranscriptFrame) -> FrameChange {
        let (summary, was_live) = match frame {
            TranscriptFrame::Live(live) => (self.store.apply_live(agent_id, live), true),
            TranscriptFrame::Fold(state) => (self.store.set_fold(agent_id, state), false),
            TranscriptFrame::Folded(delta) => (self.store.apply_fold_delta(agent_id, delta), false),
            TranscriptFrame::Runtime(state) => (self.store.set_runtime(agent_id, state), false),
        };
        let views = match summary.first_changed_block {
            None => Vec::new(),
            Some(from) => self
                .views
                .iter_mut()
                .filter_map(|(scope, view)| {
                    let start = view.start_of(agent_id, from)?;
                    Some((*scope, view.merge_from(start, &self.store, &self.open)))
                })
                .collect(),
        };
        FrameChange { views, was_live }
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{ChatSeq, MessageId, Role, UnixMs};
    use rho_agents_client::protocol::worksets::{InputView, LogEvent};
    use rho_agents_client::store::FrameSummary;

    use super::*;

    fn agent() -> AgentId {
        AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).expect("an agent id")
    }

    fn other() -> AgentId {
        AgentId::from_counter(2, &rho_agent_types::AgentIdDomain(0)).expect("an agent id")
    }

    /// The user's message to the agent, then the input that hands it over
    /// and the request that carries it: one told turn.
    /// The input that hands the agent the user's message and the request
    /// that carries it: one told turn.
    fn turn(message: u64, pos: u64, text: &str, at: u64) -> Vec<(Wpos, LogEvent)> {
        vec![(
            Wpos(pos),
            LogEvent::Request {
                agent: agent(),
                inputs: vec![InputView {
                    message: Some(MessageId(ChatSeq(message))),
                    text: text.to_owned(),
                    role: Role::User,
                }],
                answers: true,
                compaction: false,
                timing: None,
                output: String::new(),
                at: UnixMs(at),
            },
        )]
    }

    fn texts(transcripts: &Transcripts) -> Vec<String> {
        transcripts
            .state(&agent())
            .expect("the open state")
            .blocks
            .iter()
            .filter_map(|block| match &**block {
                rho_agents_client::state::UiBlock::UserMessage { text }
                | rho_agents_client::state::UiBlock::Delivered { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Opening an agent reads what was read of it once; the rows that
    /// follow cost the rows they are, not the transcript they land in.
    #[test]
    fn an_open_transcript_grows_by_what_it_is_told() {
        let mut transcripts = Transcripts::default();
        let agent_id = agent();
        assert!(!transcripts.is_open(&agent_id));

        let opened = transcripts.seed(agent_id, &turn(0, 0, "first", 1));
        assert!(opened.is_some());
        assert!(transcripts.is_open(&agent_id));
        assert_eq!(
            transcripts.open_agents(),
            BTreeSet::from([agent_id]),
            "an open transcript is what this client asks for rows about"
        );
        assert_eq!(texts(&transcripts), ["first"]);
        let held = transcripts.state(&agent_id).unwrap().blocks.len();

        let delta = transcripts
            .refold(agent_id, &turn(1, 2, "second", 2))
            .expect("the telling moved rows");
        assert_eq!(
            delta.from, held,
            "the transcript is appended to, not remade"
        );

        transcripts.apply(agent_id, TranscriptFrame::Folded(delta));
        assert_eq!(texts(&transcripts), ["first", "second"]);

        assert!(
            transcripts
                .refold(agent_id, &turn(1, 2, "second", 2))
                .is_none(),
            "a page read twice moves nothing"
        );
        let elsewhere = (
            Wpos(9),
            LogEvent::Request {
                agent: other(),
                inputs: Vec::new(),
                answers: true,
                compaction: false,
                timing: None,
                output: String::new(),
                at: UnixMs(3),
            },
        );
        assert!(
            transcripts.refold(agent_id, &[elsewhere]).is_none(),
            "a row about another agent in the workset is not this transcript"
        );
    }

    /// A transcript is opened once, even one with nothing read yet; a
    /// second open leaves what is held alone rather than handing it whole
    /// again.
    #[test]
    fn a_transcript_is_handed_whole_once() {
        let mut transcripts = Transcripts::default();
        let agent_id = agent();
        assert!(
            transcripts.seed(agent_id, &[]).is_some(),
            "a fresh agent opens empty"
        );
        assert!(
            transcripts
                .seed(agent_id, &turn(0, 0, "again", 1))
                .is_none(),
            "already open"
        );
        let delta = transcripts
            .refold(agent_id, &turn(0, 0, "first", 1))
            .expect("rows land in the empty transcript");
        transcripts.apply(agent_id, TranscriptFrame::Folded(delta));
        assert_eq!(texts(&transcripts), ["first"]);

        transcripts.forget(agent_id);
        assert!(!transcripts.is_open(&agent_id));
        assert!(transcripts.state(&agent_id).is_none());
        assert!(transcripts.open_agents().is_empty());
    }

    /// The log's past, read after newer rows, is folded in order when the
    /// transcript is read again whole, and is not told again after.
    #[test]
    fn a_reread_puts_the_past_in_order() {
        let mut transcripts = Transcripts::default();
        let agent_id = agent();
        let first = turn(0, 0, "first", 1);
        let second = turn(1, 2, "second", 5);
        assert!(transcripts.seed(agent_id, &[]).is_some());
        let delta = transcripts.refold(agent_id, &second).expect("rows land");
        transcripts.apply(agent_id, TranscriptFrame::Folded(delta));
        assert_eq!(texts(&transcripts), ["second"]);
        let all = [first, second.clone()].concat();
        let state = transcripts.reread(agent_id, &all).expect("open");
        transcripts.apply(agent_id, TranscriptFrame::Fold(state));
        assert_eq!(texts(&transcripts), ["first", "second"]);
        assert!(
            transcripts.refold(agent_id, &second).is_none(),
            "what the reread held is not told again"
        );
        assert!(transcripts.reread(other(), &all).is_none(), "not open");
    }

    /// Rows for an agent nobody has open are dropped: the client folds
    /// only what a reader is looking at.
    #[test]
    fn rows_for_a_closed_transcript_cost_nothing() {
        let mut transcripts = Transcripts::default();
        assert!(
            transcripts
                .refold(agent(), &turn(0, 0, "first", 1))
                .is_none()
        );
        assert!(transcripts.state(&agent()).is_none());
    }
    fn workset() -> Scope {
        Scope::Workset("0123456789ab".parse().unwrap())
    }

    /// What `agent` said at `pos`: one reply of one text.
    fn said(agent: AgentId, pos: u64, text: &str) -> (Wpos, LogEvent) {
        (
            Wpos(pos),
            LogEvent::Replied {
                agent,
                items: vec![rho_agents_client::protocol::transcript::Item::Text {
                    text: text.to_owned(),
                    phase: None,
                }],
                compacted: false,
                usage: None,
                context_used: None,
                at: UnixMs(pos),
            },
        )
    }

    /// A view as lines: whose heading, or what was said.
    fn lines(transcripts: &Transcripts, scope: &Scope) -> Vec<String> {
        transcripts
            .view(scope)
            .expect("a shown view")
            .blocks
            .iter()
            .filter_map(|block| match &**block {
                UiBlock::AgentHeading { agent } if *agent == self::agent() => {
                    Some("# lead".to_owned())
                }
                UiBlock::AgentHeading { .. } => Some("# helper".to_owned()),
                UiBlock::AssistantMessage { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Lands what a telling moved, as the system does, and answers what
    /// moved on `scope`.
    fn tell(
        transcripts: &mut Transcripts,
        scope: &Scope,
        agent: AgentId,
        rows: &[(Wpos, LogEvent)],
    ) -> FrameSummary {
        let delta = transcripts.refold(agent, rows).expect("the rows are new");
        transcripts
            .apply(agent, TranscriptFrame::Folded(delta))
            .views
            .into_iter()
            .find(|(moved, _)| moved == scope)
            .map_or(FrameSummary::nothing(), |(_, summary)| summary)
    }

    /// A workset's transcript is one chronological log: the agents' work
    /// interleaves by the row that told it, and a heading names whoever
    /// takes over. Narrowed to one agent, it is that agent's alone.
    #[test]
    fn a_workset_merges_its_agents_in_the_order_they_worked() {
        let (lead, helper) = (agent(), other());
        let mut transcripts = Transcripts::default();
        transcripts.seed(lead, &[said(lead, 0, "plan"), said(lead, 4, "review")]);
        transcripts.seed(helper, &[said(helper, 2, "patch")]);
        transcripts.show(workset(), lead, vec![lead, helper]);
        transcripts.show(Scope::Agent(helper), helper, vec![helper]);
        assert_eq!(
            lines(&transcripts, &workset()),
            ["plan", "# helper", "patch", "# lead", "review"]
        );
        assert_eq!(lines(&transcripts, &Scope::Agent(helper)), ["patch"]);

        // The helper works on after the lead: appended, under its heading.
        let moved = tell(
            &mut transcripts,
            &workset(),
            helper,
            &[said(helper, 6, "tested")],
        );
        assert_eq!(
            lines(&transcripts, &workset()),
            [
                "plan", "# helper", "patch", "# lead", "review", "# helper", "tested"
            ]
        );
        assert_eq!(
            moved.first_changed_block,
            Some(5),
            "what stood before the helper's new work is not drawn again"
        );
        assert_eq!(
            lines(&transcripts, &Scope::Agent(helper)),
            ["patch", "tested"]
        );
    }

    /// An agent's own transcript costs what its rows change, as before
    /// worksets: appending a step re-merges nothing before it, and a
    /// growing reply keeps its in-place update.
    #[test]
    fn one_agent_view_moves_only_what_its_agent_moved() {
        let lead = agent();
        let mut transcripts = Transcripts::default();
        transcripts.seed(lead, &[said(lead, 0, "one"), said(lead, 1, "two")]);
        let scope = Scope::Agent(lead);
        let shown = transcripts.show(scope, lead, vec![lead]);
        assert_eq!(
            shown.first_changed_block,
            Some(0),
            "a new view is drawn whole"
        );
        let held = transcripts.view(&scope).unwrap().blocks.len();
        assert_eq!(
            transcripts.show(scope, lead, vec![lead]),
            FrameSummary::nothing(),
            "showing it again moves nothing"
        );
        let moved = tell(&mut transcripts, &scope, lead, &[said(lead, 2, "three")]);
        assert_eq!(moved.first_changed_block, Some(held));
        assert_eq!(lines(&transcripts, &scope), ["one", "two", "three"]);
    }

    /// A rewind takes back the helper's step from the merge and the
    /// heading that only stood for it.
    #[test]
    fn a_rewind_takes_back_its_steps_and_their_heading() {
        let (lead, helper) = (agent(), other());
        let mut transcripts = Transcripts::default();
        transcripts.seed(lead, &[said(lead, 0, "plan")]);
        transcripts.seed(helper, &[said(helper, 2, "wrong turn")]);
        transcripts.show(workset(), lead, vec![lead, helper]);
        assert_eq!(
            lines(&transcripts, &workset()),
            ["plan", "# helper", "wrong turn"]
        );
        let rewound = (
            Wpos(3),
            LogEvent::Rewound {
                agent: helper,
                to: Wpos(2),
                at: UnixMs(3),
            },
        );
        let moved = tell(&mut transcripts, &workset(), helper, &[rewound]);
        assert_eq!(lines(&transcripts, &workset()), ["plan"]);
        assert!(moved.first_changed_block.is_some());
    }

    /// An agent that joins the workset is merged in where its work
    /// stands, not appended after everything shown.
    #[test]
    fn an_agent_joining_is_merged_in_place() {
        let (lead, helper) = (agent(), other());
        let mut transcripts = Transcripts::default();
        transcripts.seed(lead, &[said(lead, 0, "plan"), said(lead, 9, "merged")]);
        transcripts.show(workset(), lead, vec![lead]);
        transcripts.seed(helper, &[said(helper, 5, "patch")]);
        let moved = transcripts.show(workset(), lead, vec![lead, helper]);
        assert_eq!(
            lines(&transcripts, &workset()),
            ["plan", "# helper", "patch", "# lead", "merged"]
        );
        assert_eq!(moved.first_changed_block, Some(1));
    }
}
