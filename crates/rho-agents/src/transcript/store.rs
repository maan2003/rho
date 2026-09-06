//! The transcripts this client holds open.
//!
//! An agent's transcript is two things joined: the fold of its mirror on
//! disk, and the runtime's live tail. Both land in one rendered state per
//! agent, which is what a screen draws. Which agents are held open is the
//! same question as which agents this client asks the model thread for
//! rows about, so it is answered here.

use std::collections::{BTreeSet, HashMap};

use rho_ui_proto::AgentId;
use rho_ui_proto::mirror::{AgentPos, MirrorEvent};

use crate::TranscriptFold;
use crate::state::UiAgentState;
use crate::store::{AgentStore, FrameSummary};

/// One change to an agent's transcript: a delta to the runtime's live
/// tail, or the fold of its mirror made again.
pub enum TranscriptFrame {
    Live(rho_ui_proto::mirror::Live),
    /// The mirror's fold, whole. What an agent's first read hands, and
    /// nothing else: a transcript is handed once and appended to after.
    Fold(UiAgentState),
    /// The rows one telling of the mirror moved.
    Folded(crate::fold::FoldDelta),
}

/// What a frame did, for a caller deciding what to redraw.
pub struct FrameChange {
    pub summary: FrameSummary,
    /// The context figure before the frame, to compare against the one
    /// after; a status line only changes when this does.
    pub context_before: Option<u64>,
    pub usage_changed: bool,
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
}

impl Transcripts {
    pub fn state(&self, agent_id: &AgentId) -> Option<&UiAgentState> {
        self.store.get(agent_id)
    }

    pub fn context_used(&self, agent_id: &AgentId) -> Option<u64> {
        self.store
            .get(agent_id)
            .and_then(|state| state.context_used)
    }

    pub fn is_open(&self, agent_id: &AgentId) -> bool {
        self.open.contains_key(agent_id)
    }

    /// The agents whose rows this client wants: the transcripts it has
    /// open. Everything else it hears as a digest.
    pub fn open_agents(&self) -> BTreeSet<AgentId> {
        self.open.keys().copied().collect()
    }

    /// Lets go of one agent's transcript, held state and all.
    pub fn forget(&mut self, agent_id: AgentId) {
        self.open.remove(&agent_id);
        self.store.forget(agent_id);
    }

    /// Opens an agent's transcript from the mirror the client already
    /// holds: the fold, for a reader who opened it before any live frame,
    /// or with the daemon down. The live frame rides on its tail.
    ///
    /// Answers whether it opened one; an agent already open, or one with
    /// nothing on disk, is left as it was.
    pub fn seed(&mut self, agent_id: AgentId, events: &[(AgentPos, MirrorEvent)]) -> bool {
        if self.open.contains_key(&agent_id) || events.is_empty() {
            return false;
        }
        let mut fold = TranscriptFold::new(events);
        // The one time a transcript is handed whole: nothing was here to
        // append to. Every telling after this hands the rows it moved.
        self.store.set_fold(agent_id, fold.state());
        fold.delta();
        self.open.insert(agent_id, fold);
        true
    }

    /// Rows of an agent whose transcript is open, folded into the copy
    /// behind it. Positions already held are skipped, so the rows the
    /// reader's own open read already picked up cost nothing. Answers
    /// with the rows the telling moved, and nothing when it moved none.
    pub fn refold(
        &mut self,
        agent_id: AgentId,
        rows: &[(AgentPos, MirrorEvent)],
    ) -> Option<crate::fold::FoldDelta> {
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

    /// Lands one frame on an agent's rendered state.
    pub fn apply(&mut self, agent_id: AgentId, frame: TranscriptFrame) -> FrameChange {
        let context_before = self.context_used(&agent_id);
        let usage_before = self.store.get(&agent_id).map(|state| state.usage.clone());
        let (summary, was_live) = match frame {
            TranscriptFrame::Live(live) => (self.store.apply_live(agent_id, live), true),
            TranscriptFrame::Fold(state) => (self.store.set_fold(agent_id, state), false),
            TranscriptFrame::Folded(delta) => (self.store.apply_fold_delta(agent_id, delta), false),
        };
        let usage_changed =
            usage_before.as_ref() != self.store.get(&agent_id).map(|state| &state.usage);
        FrameChange {
            summary,
            context_before,
            usage_changed,
            was_live,
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_core::{MessageDelivery, UnixMs};

    use super::*;

    fn agent() -> AgentId {
        AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(0)).expect("an agent id")
    }

    fn said(text: &str, at: u64) -> MirrorEvent {
        MirrorEvent::Message {
            from: None,
            text: text.to_owned(),
            delivery: MessageDelivery::Immediate,
            at: UnixMs(at),
        }
    }

    fn sent(at: u64) -> MirrorEvent {
        MirrorEvent::Sent {
            results: Vec::new(),
            compaction: false,
            at: UnixMs(at),
        }
    }

    fn rows(events: Vec<MirrorEvent>, from: u64) -> Vec<(AgentPos, MirrorEvent)> {
        events
            .into_iter()
            .enumerate()
            .map(|(nth, event)| (AgentPos(from + nth as u64), event))
            .collect()
    }

    /// Opening an agent reads its disk copy once; the rows that follow
    /// cost the rows they are, not the transcript they land in.
    #[test]
    fn an_open_transcript_grows_by_what_it_is_told() {
        let mut transcripts = Transcripts::default();
        let agent_id = agent();
        assert!(!transcripts.is_open(&agent_id));

        let opened = transcripts.seed(agent_id, &rows(vec![said("first", 1), sent(1)], 0));
        assert!(opened, "the disk copy had rows to fold");
        assert!(transcripts.is_open(&agent_id));
        assert_eq!(
            transcripts.open_agents(),
            BTreeSet::from([agent_id]),
            "an open transcript is what this client asks for rows about"
        );
        assert_eq!(
            transcripts
                .state(&agent_id)
                .expect("the seeded state")
                .blocks
                .len(),
            1
        );

        let delta = transcripts
            .refold(agent_id, &rows(vec![said("second", 2), sent(2)], 2))
            .expect("the telling moved rows");
        assert_eq!(delta.from, 1, "the transcript is appended to, not remade");
        assert_eq!(delta.blocks.len(), 1);

        transcripts.apply(agent_id, TranscriptFrame::Folded(delta));
        assert_eq!(
            transcripts
                .state(&agent_id)
                .expect("the state after the delta")
                .blocks
                .len(),
            2
        );
    }

    /// A transcript is opened once. A second open, or one with nothing on
    /// disk, leaves what is held alone rather than handing it whole again.
    #[test]
    fn a_transcript_is_handed_whole_once() {
        let mut transcripts = Transcripts::default();
        let agent_id = agent();
        assert!(!transcripts.seed(agent_id, &[]), "nothing on disk");
        assert!(transcripts.seed(agent_id, &rows(vec![said("first", 1), sent(1)], 0)));
        assert!(
            !transcripts.seed(agent_id, &rows(vec![said("again", 1), sent(1)], 0)),
            "already open"
        );

        transcripts.forget(agent_id);
        assert!(!transcripts.is_open(&agent_id));
        assert!(transcripts.state(&agent_id).is_none());
        assert!(transcripts.open_agents().is_empty());
    }

    /// Rows for an agent nobody has open are dropped: the client folds
    /// only what a reader is looking at.
    #[test]
    fn rows_for_a_closed_transcript_cost_nothing() {
        let mut transcripts = Transcripts::default();
        assert!(
            transcripts
                .refold(agent(), &rows(vec![said("first", 1), sent(1)], 0))
                .is_none()
        );
        assert!(transcripts.state(&agent()).is_none());
    }
}
