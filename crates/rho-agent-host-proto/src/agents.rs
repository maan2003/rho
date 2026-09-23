//! The agents part of a host, opened by [`crate::Open::Agents`].
//!
//! Its session ([`Open::Session`]) carries the host's journal and its
//! agents' live tails: a stream of its own, so a catch-up of thousands of
//! pages never queues ahead of anything else, and its reader is the agents
//! client alone. Every frame after the opening one is a [`ClientFrame`] or
//! a [`ServerFrame`]. Whatever else is asked of the agents is a stream of
//! its own: one [`Request`] answered with one [`Reply`], or a terminal,
//! shell or workspace channel.

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::transcript::{AgentPos, DetailBody, Live, LogEntry, Seq};
use crate::{
    AgentCommand, AgentCostSeries, AgentId, AgentUsageSeries, QuotaSeries, QuotaSummary,
    WorkspaceInfo, shell, term,
};

/// What an agents stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// The journal and the live tails, for as long as the client stays.
    Session,
    /// One request, answered with one [`Reply`]; then the stream closes.
    Request(Request),
    /// A daemon-owned terminal for an agent. Answered with
    /// [`crate::Opened`]; an attached stream then carries
    /// [`term::TermClientFrame`] and [`term::TermServerFrame`], the first of
    /// them a snapshot of the screen preceded by history. Otherwise the
    /// terminal runs headless and the stream closes.
    Terminal {
        /// Display handle or id prefix, resolved by the daemon ("eng-ht08").
        agent: String,
        /// Client-chosen id, unique among the agent's running terminals
        /// ([`Request::TerminalList`] enumerates them).
        terminal_id: u64,
        open: term::TerminalOpen,
        /// The client's viewport, applied to the PTY (last writer wins).
        cols: u16,
        rows: u16,
    },
    /// Attaches to an agent's running shell ([`Request::ShellStart`]).
    /// Answered with [`crate::Opened`], then [`shell`] frames. Closing the
    /// stream only detaches; the shell keeps running.
    Shell { agent: String },
    /// File access for one agent's workspace. Answered with
    /// [`crate::Opened`]; after `Ready` the stream carries
    /// [`crate::workspace::WorkspaceClientFrame`] and
    /// [`crate::workspace::WorkspaceServerFrame`], and closing it closes the
    /// channel and its filesystem watcher.
    Workspace { workspace: WorkspaceInfo },
}

/// What a client can ask of a host's agents in one round trip.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Request {
    /// Answered with [`Reply::AgentCreated`] for [`AgentCommand::New`] and
    /// [`Reply::Done`] for the rest.
    Command(AgentCommand),
    /// Every running terminal, of one agent if it names one (display
    /// handle or id prefix). Answered with [`Reply::TerminalList`].
    TerminalList { agent: Option<String> },
    /// Starts the daemon-owned Comint-style shell for an agent. Attaching
    /// is [`Open::Shell`].
    ShellStart { agent: String },
    /// Running shells, of one agent if it names one. Answered with
    /// [`Reply::ShellList`].
    ShellList { agent: Option<String> },
    /// Stops an agent's running shell gracefully.
    ShellClose { agent: String },
    /// A recorded visualization. Answered with [`Reply::Visualization`].
    Visualization { id: String },
    /// Stores an immutable visualization snapshot. Answered with
    /// [`Reply::VisualizationRecorded`].
    RecordVisualization { mime_type: String, content: Vec<u8> },
    /// Answered with [`Reply::QuotaUsage`].
    QuotaUsage,
    /// Answered with [`Reply::QuotaHistory`].
    QuotaHistory,
    /// Answered with [`Reply::GlobalUsage`].
    GlobalUsage { since_ms: u64 },
    /// Raw per-agent usage needed to form cost distributions beginning at
    /// `since_ms`, with the fixed trailing-window lookback. Answered with
    /// [`Reply::AgentCostDistribution`].
    AgentCostDistribution { since_ms: u64 },
    /// Which Claude accounts exist and which one agents run on. Answered
    /// with [`Reply::ClaudeAccounts`].
    ClaudeAccounts,
    /// Puts every agent on `name` from its next turn. Answered with
    /// [`Reply::ClaudeAccounts`] as it stands after the switch.
    SetClaudeAccount { name: String },
    /// Enables or disables one provider account namespace on this host.
    SetAuthAccountEnabled { name: String, enabled: bool },
}

/// The answer to a [`Request`].
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Reply {
    /// Done, with nothing to say.
    Done,
    /// Not done, and why: the whole chain of causes.
    Failed {
        reason: String,
    },
    AgentCreated {
        agent_id: AgentId,
    },
    TerminalList {
        terminals: Vec<term::TerminalInfo>,
    },
    ShellList {
        shells: Vec<shell::ShellInfo>,
    },
    Visualization {
        id: String,
        mime_type: String,
        content: Vec<u8>,
    },
    VisualizationRecorded {
        id: String,
    },
    QuotaUsage {
        summaries: Vec<QuotaSummary>,
    },
    QuotaHistory {
        series: Vec<QuotaSeries>,
    },
    GlobalUsage {
        series: Vec<AgentUsageSeries>,
    },
    AgentCostDistribution {
        series: Vec<AgentCostSeries>,
    },
    ClaudeAccounts {
        accounts: Vec<String>,
        current: String,
    },
}

/// What a client says on its agents session.
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

/// What a host says on an agents session.
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
    /// An agent was created on the host, by any client or agent.
    AgentCreated { agent_id: AgentId },
    /// Every provider's quota as it stands: after the opening
    /// [`ServerFrame::JournalHead`], whenever an observation changes it,
    /// and every ten minutes besides, because burn and resets move with
    /// time alone.
    QuotaUsage { summaries: Vec<QuotaSummary> },
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
