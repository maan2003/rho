//! The transcripts this client holds open.
//!
//! An agent's transcript is two things joined: the fold of its workset rows
//! and its messages, and the runtime's live tail. Both land in one rendered
//! state per agent, which is what a screen draws. Which agents are held open is
//! the same question as which agents this client asks the model thread for
//! rows about, so it is answered here.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use rho_agent_types::AgentId;
use rho_agents_client::fold::TranscriptRow;
use rho_agents_client::protocol::transcript::Draft;
use rho_agents_client::state::{UiAgentState, UiBlock};
use rho_agents_client::store::{AgentStore, FrameSummary};
use rho_agents_client::{HostTopic, TranscriptFold};

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
    pub summary: FrameSummary,
    /// The frame came from the runtime, so the agent is live.
    pub was_live: bool,
}

#[derive(Default)]
pub struct Transcripts {
    /// The rendered state per agent: what a screen draws.
    store: AgentStore,
    /// The transcript fold of every agent held open, in memory, so a row
    /// folds into it without waiting on the disk copy.
    open: HashMap<AgentId, TranscriptFold>,
    /// The chat topics held open, each as its messages make it.
    topics: HashMap<HostTopic, UiAgentState>,
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

    /// A topic as it stands now, and what moved since it was last set:
    /// everything, the first time.
    pub fn set_topic(&mut self, topic: HostTopic, state: UiAgentState) -> FrameSummary {
        let summary = match self.topics.get(&topic) {
            Some(held) => rho_agents_client::store::summarize(&held.blocks, &state.blocks),
            None => FrameSummary::everything(),
        };
        self.topics.insert(topic, state);
        summary
    }

    pub fn topic(&self, topic: HostTopic) -> Option<&UiAgentState> {
        self.topics.get(&topic)
    }

    /// A held topic with `drafts` after its messages, in place of the
    /// drafts it had there, and what moved.
    pub fn set_topic_drafts(
        &mut self,
        topic: HostTopic,
        drafts: Vec<Arc<UiBlock>>,
    ) -> FrameSummary {
        let Some(held) = self.topics.get(&topic) else {
            return FrameSummary::nothing();
        };
        let said = held
            .blocks
            .iter()
            .rposition(|block| !matches!(**block, UiBlock::MessageDraft { .. }))
            .map_or(0, |last| last + 1);
        let mut state = held.clone();
        state.blocks.truncate(said);
        state.blocks.extend(drafts);
        self.set_topic(topic, state)
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

    pub fn forget_topic(&mut self, topic: HostTopic) {
        self.topics.remove(&topic);
    }

    /// Lets go of one agent's transcript, held state and all.
    pub fn forget(&mut self, agent_id: AgentId) {
        self.open.remove(&agent_id);
        self.store.forget(agent_id);
    }

    /// Opens an agent's transcript from what the client has read of it:
    /// its rows and messages, interleaved. An agent with nothing read yet opens
    /// empty, so the rows that follow land in it. The live frame rides on
    /// its tail.
    ///
    /// Answers whether it opened one; an agent already open is left as it
    /// was.
    pub fn seed(&mut self, agent_id: AgentId, rows: &[TranscriptRow]) -> bool {
        if self.open.contains_key(&agent_id) {
            return false;
        }
        let mut fold = TranscriptFold::new(agent_id, rows);
        // The one time a transcript is handed whole: nothing was here to
        // append to. Every telling after this hands the rows it moved.
        self.store.set_fold(agent_id, fold.state());
        fold.delta();
        self.open.insert(agent_id, fold);
        true
    }

    /// An open transcript folded again from everything read of it, when
    /// its log's past has been read: the rows came after messages newer than
    /// them, so they are put in time order with it rather than appended.
    /// Answers the state to land as a [`TranscriptFrame::Fold`]; nothing
    /// for an agent not open.
    pub fn reread(&mut self, agent_id: AgentId, rows: &[TranscriptRow]) -> Option<UiAgentState> {
        let fold = self.open.get_mut(&agent_id)?;
        *fold = TranscriptFold::new(agent_id, rows);
        // Handed whole; the rows told after this append to it.
        fold.delta();
        Some(fold.state())
    }

    /// Rows and messages of an agent whose transcript is open, folded into
    /// the copy behind it. Positions and messages already held are skipped, so
    /// what the reader's own open read already picked up costs nothing.
    /// Answers with the rows the telling moved, and nothing when it moved
    /// none.
    pub fn refold(
        &mut self,
        agent_id: AgentId,
        rows: &[TranscriptRow],
    ) -> Option<rho_agents_client::fold::FoldDelta> {
        let fold = self.open.get_mut(&agent_id)?;
        let mut refolded = false;
        for row in rows {
            refolded |= fold.tell(row);
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

    /// Lands one frame on an agent's rendered state.
    pub fn apply(&mut self, agent_id: AgentId, frame: TranscriptFrame) -> FrameChange {
        let (summary, was_live) = match frame {
            TranscriptFrame::Live(live) => (self.store.apply_live(agent_id, live), true),
            TranscriptFrame::Fold(state) => (self.store.set_fold(agent_id, state), false),
            TranscriptFrame::Folded(delta) => (self.store.apply_fold_delta(agent_id, delta), false),
            TranscriptFrame::Runtime(state) => (self.store.set_runtime(agent_id, state), false),
        };
        FrameChange { summary, was_live }
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{
        Address, ChatSeq, ContentPart, Message, MessageId, Role, TopicId, UnixMs, Wpos,
    };
    use rho_agents_client::protocol::worksets::{InputView, LogEvent};

    use super::*;

    fn agent() -> AgentId {
        AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).expect("an agent id")
    }

    fn other() -> AgentId {
        AgentId::from_counter(2, &rho_agent_types::AgentIdDomain(0)).expect("an agent id")
    }

    /// The user's message to the agent, then the input that hands it over
    /// and the request that carries it: one told turn.
    fn turn(message: u64, pos: u64, text: &str, at: u64) -> Vec<TranscriptRow> {
        vec![
            TranscriptRow::Message(Message {
                id: MessageId(ChatSeq(message)),
                at: UnixMs(at),
                topic: TopicId(ChatSeq(1)),
                from: Address::User,
                mentions: Vec::new(),
                content: vec![ContentPart::Text {
                    text: text.to_owned(),
                }],
            }),
            TranscriptRow::Log(
                Wpos(pos),
                LogEvent::Request {
                    agent: agent(),
                    inputs: vec![InputView {
                        message: Some(MessageId(ChatSeq(message))),
                        text: text.to_owned(),
                        role: Role::User,
                    }],
                    calls: Vec::new(),
                    compaction: false,
                    at: UnixMs(at),
                },
            ),
        ]
    }

    fn texts(transcripts: &Transcripts) -> Vec<String> {
        transcripts
            .state(&agent())
            .expect("the open state")
            .blocks
            .iter()
            .filter_map(|block| match &**block {
                rho_agents_client::state::UiBlock::UserMessage { text, .. } => Some(text.clone()),
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
        assert!(opened);
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
        let elsewhere = TranscriptRow::Log(
            Wpos(9),
            LogEvent::Request {
                agent: other(),
                inputs: Vec::new(),
                calls: Vec::new(),
                compaction: false,
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
        assert!(transcripts.seed(agent_id, &[]), "a fresh agent opens empty");
        assert!(
            !transcripts.seed(agent_id, &turn(0, 0, "again", 1)),
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

    /// The log's past read after newer messages land before it, in time
    /// order, when the transcript is read again whole.
    #[test]
    fn a_reread_puts_the_past_before_newer_messages() {
        let mut transcripts = Transcripts::default();
        let agent_id = agent();
        let first = turn(0, 0, "first", 1);
        let second = turn(1, 2, "second", 5);
        // Opened with the messages on disk and none of the rows yet.
        assert!(transcripts.seed(agent_id, &[first[0].clone(), second[0].clone()]));
        let all = rho_agents_client::fold::interleave(
            agent_id,
            [&first[1..], &second[1..]]
                .concat()
                .into_iter()
                .map(|row| match row {
                    TranscriptRow::Log(pos, event) => (pos, event),
                    TranscriptRow::Message(_) => unreachable!(),
                }),
            [first[0].clone(), second[0].clone()]
                .into_iter()
                .map(|row| match row {
                    TranscriptRow::Message(message) => message,
                    TranscriptRow::Log(..) => unreachable!(),
                }),
        );
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
}
