//! The agents stream: a host's journal and its agents' live tails.
//!
//! A stream of its own, opened by [`crate::ClientMessage::AgentsOpen`], so
//! that a catch-up of thousands of pages never queues ahead of anything
//! else the host says and its reader is the agents client alone. Every
//! frame after the opening one is a [`ClientFrame`] or a [`ServerFrame`].

use senax_encoder::{Pack, Unpack};

use crate::AgentId;
use crate::transcript::{AgentPos, DetailBody, Live, LogEntry, Seq};

/// What a client says on its agents stream.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ClientFrame {
    /// After [`ServerFrame::JournalHead`]: the last journal entry this
    /// client holds for this host (zero for none). The host answers
    /// [`ServerFrame::Log`] pages for everything past it, then follows:
    /// every later append on any agent, and the live tails, are pushed on
    /// this stream. A second `Follow` replaces the first.
    Follow { since: Seq },
    /// The agents whose live frames this client wants: the ones on screen.
    /// Everything durable arrives on the journal regardless, so this only
    /// decides who streams partial text and tools in flight. Replaces the
    /// set wholesale; an empty set asks for none.
    Focus { agent_ids: Vec<AgentId> },
    /// The bodies of raw events: tool output, a response whole.
    ///
    /// One request per chunk of transcript rather than one per call: a
    /// chunk's tool calls are one `Sent` each (measured at 1.01 results per
    /// `Sent` over the whole corpus), so asking per call would ask the same
    /// events over again. The host answers one [`ServerFrame::Detail`] per
    /// position, each naming its own `pos`, so the answers need no order
    /// and no correlation id. `pos` is the first position and `more` the
    /// rest.
    Detail {
        agent_id: AgentId,
        pos: AgentPos,
        more: Vec<AgentPos>,
    },
}

/// What a host says on an agents stream.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    /// The first frame: whose journal this is and how far it runs, so a
    /// client knows whether its copy counts in it and how far behind it is
    /// before it follows.
    JournalHead {
        machine_seed: u64,
        journal_head: Seq,
    },
    /// A run of the host's journal in order: the answer to
    /// [`ClientFrame::Follow`], paged, and afterwards every append as it
    /// lands. Entries never repeat and never skip within one stream.
    Log { entries: Vec<LogEntry> },
    /// What a runtime has past the log, as it changes, for every agent any
    /// client is looking at.
    Live { agent_id: AgentId, live: Live },
    /// The answer to [`ClientFrame::Detail`].
    Detail {
        agent_id: AgentId,
        pos: AgentPos,
        body: DetailBody,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentIdDomain;
    use crate::transcript::{Item, TextPhase};

    fn round_trips<
        T: senax_encoder::Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
    >(
        frame: T,
    ) {
        let bytes = senax_encoder::pack(&frame).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: T = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn frames_round_trip() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(7)).unwrap();
        round_trips(ClientFrame::Focus {
            agent_ids: vec![agent_id],
        });
        round_trips(ClientFrame::Focus { agent_ids: vec![] });
        round_trips(ClientFrame::Follow { since: Seq(9) });
        round_trips(ClientFrame::Detail {
            agent_id,
            pos: AgentPos(3),
            more: vec![AgentPos(4), AgentPos(9)],
        });
        for live in [
            Live::Requesting,
            Live::Item {
                index: 0,
                item: Item::Text {
                    text: "hel".to_owned(),
                    phase: Some(TextPhase::FinalAnswer),
                },
            },
            Live::Appended {
                index: 0,
                text: "lo".to_owned(),
            },
            Live::Waiting {
                until: Some(crate::UnixMs(5)),
            },
            Live::Idle,
        ] {
            round_trips(ServerFrame::Live { agent_id, live });
        }
    }
}
