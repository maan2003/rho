//! The agents protocol of a host, [`rho_rpc::protocol::Protocol::Agents`].
//!
//! Its session ([`Open::Session`]) carries the host's journal and its
//! agents' live tails: a stream of its own, so a catch-up of thousands of
//! pages never queues ahead of anything else, and its reader is the agents
//! client alone. Every frame after the opening one is a [`ClientFrame`] or
//! a [`ServerFrame`]. Whatever else is asked of the agents is one call
//! ([`Open::Request`]) on a stream of its own.

use camino::Utf8PathBuf;
use rho_agent_types::{
    AgentId, AgentPos, AgentRole, ContentPart, MessageDelivery, Seq, WorksetMode, WorkspaceInfo,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use self::transcript::{DetailBody, Live, LogEntry};

pub mod transcript;

/// What an agents stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// The journal and the live tails, for as long as the client stays.
    Session,
    /// One [`rho_rpc::protocol::Call`], answered with one
    /// [`rho_rpc::protocol::Answer`]; then the
    /// stream closes.
    Request(Request),
}

rho_rpc::calls! {
    /// Every call the agents answer, as it goes on the wire.
    pub enum Request {
        /// Answered with the new agent's id.
        New(NewAgent) -> AgentId;
        Command(AgentCommand) -> ();
        // Bulk: an answer that waits behind interactive traffic.
        Visualization(Visualization) -> VisualizationContent, priority None;
        /// Answered with the recorded visualization's id.
        RecordVisualization(RecordVisualization) -> String;
        QuotaUsage(QuotaUsage) -> Vec<QuotaSummary>;
        QuotaHistory(QuotaHistory) -> Vec<QuotaSeries>;
        GlobalUsage(GlobalUsage) -> Vec<AgentUsageSeries>;
        AgentCostDistribution(AgentCostDistribution) -> Vec<AgentCostSeries>;
        ClaudeAccounts(ClaudeAccounts) -> ClaudeAccountList;
        /// Answered with the accounts as they stand after the switch.
        SetClaudeAccount(SetClaudeAccount) -> ClaudeAccountList;
        SetAuthAccountEnabled(SetAuthAccountEnabled) -> ();
    }
}

impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Agents;

    fn debug_reply(&self, frame: &[u8]) -> Option<String> {
        match self {
            Self::Request(request) => Some(request.debug_answer(frame)),
            Self::Session => None,
        }
    }
}

/// A recorded visualization.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Visualization {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct VisualizationContent {
    pub mime_type: String,
    pub content: Vec<u8>,
}

/// Stores an immutable visualization snapshot.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct RecordVisualization {
    pub mime_type: String,
    pub content: Vec<u8>,
}

/// Every provider's quota as it stands.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct QuotaUsage;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct QuotaHistory;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GlobalUsage {
    pub since_ms: u64,
}

/// Raw per-agent usage needed to form cost distributions beginning at
/// `since_ms`, with the fixed trailing-window lookback.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct AgentCostDistribution {
    pub since_ms: u64,
}

/// Which Claude accounts exist and which one agents run on.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ClaudeAccounts;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ClaudeAccountList {
    pub accounts: Vec<String>,
    pub current: String,
}

/// Puts every agent on `name` from its next turn.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct SetClaudeAccount {
    pub name: String,
}

/// Enables or disables one provider account namespace on this host.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct SetAuthAccountEnabled {
    pub name: String,
    pub enabled: bool,
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
        /// The last agent-id counter handed out, for short-prefix
        /// rendering.
        agent_counter: u64,
    },
    /// Which provider accounts agents may run on: after the opening
    /// [`ServerFrame::JournalHead`], and again whenever it changes.
    Auth { auth: AuthState },
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
    /// An agent was created on the host, by any client or agent, and the
    /// agent-id counter moved to `agent_counter`.
    AgentCreated {
        agent_id: AgentId,
        agent_counter: u64,
    },
    /// Every provider's quota as it stands: after the opening
    /// [`ServerFrame::JournalHead`], whenever an observation changes it,
    /// and every ten minutes besides, because burn and resets move with
    /// time alone.
    QuotaUsage { summaries: Vec<QuotaSummary> },
}

/// Window represented by each point in the agent-cost distribution graph.
pub const AGENT_COST_WINDOW_DAYS: u64 = 7;

/// A new agent for a host to start.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct NewAgent {
    pub role: AgentRole,
    /// Where the agent's working copy starts (including which repo, for
    /// the modes that need one).
    pub start: StartMode,
    /// How the agent sees the filesystem around its workset: a minimal
    /// generated root, or the host.
    pub mode: WorksetMode,
    pub content: Option<Vec<ContentPart>>,
}

/// What a client tells a host to do to one of its agents.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum AgentCommand {
    Send {
        agent_id: AgentId,
        content: Vec<ContentPart>,
        delivery: MessageDelivery,
    },
    Compact {
        agent_id: AgentId,
        delivery: MessageDelivery,
    },
    ChangeRole {
        agent_id: AgentId,
        role: AgentRole,
    },
    /// How the agent sees the filesystem from now on. Its loop restarts
    /// in the new view, so the Python notebook's state is lost.
    ChangeMode {
        agent_id: AgentId,
        mode: WorksetMode,
    },
    Cancel {
        agent_id: AgentId,
    },
    Rewind {
        agent_id: AgentId,
        turns: u32,
    },
    Continue {
        agent_id: AgentId,
    },
    /// Gives a Rho-runtime agent a fresh key for subsequent provider
    /// requests.
    ChangePromptCacheKey {
        agent_id: AgentId,
    },
}

impl AgentCommand {
    /// The agent the command is for.
    pub fn agent_id(&self) -> AgentId {
        match self {
            Self::Send { agent_id, .. }
            | Self::Compact { agent_id, .. }
            | Self::ChangeRole { agent_id, .. }
            | Self::ChangeMode { agent_id, .. }
            | Self::Cancel { agent_id }
            | Self::Rewind { agent_id, .. }
            | Self::Continue { agent_id }
            | Self::ChangePromptCacheKey { agent_id } => *agent_id,
        }
    }
}

/// Where a new agent works. Each mode carries exactly the data it needs.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum StartMode {
    /// A fresh workset holding a clone of `repo` (a URL or a daemon-side
    /// path), with a new change on top of the revset.
    NewOn { repo: Utf8PathBuf, revset: String },
    /// The SAME place as the target: the new agent works in the target
    /// agent's directory, seeing its edits instantly.
    Join(JoinTarget),
}

/// Whose workspace [`StartMode::Join`] joins.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum JoinTarget {
    /// A known workspace, sent back verbatim from the mirror's `Created`.
    Workspace(WorkspaceInfo),
    /// The user's own checkout of `repo`.
    User { repo: Utf8PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSummary {
    pub model: String,
    /// Daemon-local ChatGPT OAuth namespace; absent for Claude.
    pub auth_namespace: Option<String>,
    pub remaining_percent: u8,
    pub burn_10m: u16,
    pub burn_2h: u16,
    pub burn_1d: u16,
    pub burn_3d: u16,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSeries {
    pub model: String,
    /// Daemon-local ChatGPT OAuth namespace; absent for Claude.
    pub auth_namespace: Option<String>,
    pub points: Vec<QuotaPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaPoint {
    pub observed_at_ms: u64,
    pub remaining_percent: u8,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    pub approximate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentUsageSeries {
    pub model: String,
    pub buckets: Vec<AgentUsageBucket>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentCostSeries {
    /// Host-local identity. Clients combining hosts must keep the host in the
    /// distribution key rather than merging equal counters.
    pub agent_id: AgentId,
    pub model: String,
    pub buckets: Vec<AgentUsageBucket>,
}

/// Daemon-wide authentication settings presented by a GUI host.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AuthState {
    pub namespaces: Vec<String>,
    pub disabled_namespaces: Vec<String>,
    pub active_namespace: Option<String>,
}

#[cfg(test)]
mod tests {
    use rho_agent_types::AgentIdDomain;

    use super::transcript::{Item, TextPhase};
    use super::*;

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
                until: Some(rho_agent_types::UnixMs(5)),
            },
            Live::Idle,
        ] {
            round_trips(ServerFrame::Live { agent_id, live });
        }
    }

    #[test]
    fn requests_and_replies_round_trip() {
        let agent_id = AgentId::from_counter(7, &AgentIdDomain(1)).unwrap();
        for request in [
            SetAuthAccountEnabled {
                name: "work".to_owned(),
                enabled: false,
            }
            .into(),
            AgentCostDistribution { since_ms: 42 }.into(),
            RecordVisualization {
                mime_type: "image/svg+xml".to_owned(),
                content: b"<svg viewBox=\"0 0 1 1\"/>".to_vec(),
            }
            .into(),
            QuotaUsage.into(),
            AgentCommand::Send {
                agent_id,
                content: vec![
                    ContentPart::Text {
                        text: "inspect".to_owned(),
                    },
                    ContentPart::Image {
                        media_type: "image/gif".to_owned(),
                        data: vec![1, 2, 3],
                    },
                ],
                delivery: MessageDelivery::NextRequest,
            }
            .into(),
        ] {
            round_trips(Open::Request(request));
        }
        round_trips(rho_rpc::protocol::Answer::Done(vec![AgentUsageSeries {
            model: "fable".to_owned(),
            buckets: vec![AgentUsageBucket {
                bucket_start_ms: 300_000,
                input_tokens: 10,
                ..AgentUsageBucket::default()
            }],
        }]));
        round_trips(rho_rpc::protocol::Answer::Done(vec![AgentCostSeries {
            agent_id,
            model: "gpt".to_owned(),
            buckets: vec![AgentUsageBucket {
                bucket_start_ms: 3_600_000,
                output_tokens: 10,
                requests: 1,
                ..AgentUsageBucket::default()
            }],
        }]));
        round_trips(rho_rpc::protocol::Answer::Done(VisualizationContent {
            mime_type: "image/svg+xml".to_owned(),
            content: b"<svg viewBox=\"0 0 1 1\"/>".to_vec(),
        }));
        round_trips(rho_rpc::protocol::Answer::Done(agent_id));
    }

    #[test]
    fn opening_survives_the_envelope() {
        let envelope = rho_rpc::protocol::Open::of(&Open::Session).unwrap();
        assert_eq!(envelope.unpack::<Open>().unwrap(), Open::Session);
        assert!(envelope.unpack::<rho_hosts::protocol::Open>().is_err());
    }
}
