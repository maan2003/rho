//! Private agent messages shared by the host and worker.
use std::io;

use rho_agent_types::{AgentRole, TurnEdge, UnixMs};
use senax_encoder::{Decode, Encode};

use crate::AgentEvent;
use crate::log::{AgentEventPos, AgentHead, AgentUsageBucket, ClaudeRewind, SessionBinding};

pub(crate) const VERSION: u32 = 22;

/// The worker's second connection, inherited at this fd. It carries only the
/// requests the worker makes of the agent host and their answers, so a host
/// handing over can stop reading requests yet read everything else.
pub(crate) const REQUESTS_FD: i32 = 3;

#[derive(Encode, Decode)]
pub(crate) struct Bootstrap {
    pub cwd: camino::Utf8PathBuf,
}

#[derive(Encode, Decode)]
pub(crate) enum Control {
    Retire,
    User {
        id: crate::entry::MessageId,
        content: Vec<rho_agent_types::ContentPart>,
    },
    Mail {
        sender: rho_agent_types::AgentId,
        label: String,
        body: String,
    },
    NoticeCarried,
    TellTail,
    Compact,
    Cancel,
    Retry,
    Effort(rho_claude::Effort),
    Role(rho_agent_types::AgentRole),
    CacheKey,
    Rewind(u32),
}

/// A notebook host function the agent host answers for the worker.
#[derive(Debug, Encode, Decode)]
pub(crate) enum SharedCall {
    Agent(crate::multi_agent_tools::AgentCall),
    Papercut(crate::papercut::PapercutArgs),
}

/// How the agent host answered a [`SharedCall`]: text for the model either way.
#[derive(Encode, Decode)]
pub(crate) enum SharedReply {
    Ok(String),
    Err(String),
}

#[derive(Encode, Decode)]
pub(crate) enum Request<'a> {
    Name(String),
    Team,
    SharedTool(SharedCall),
    Usage(AgentUsageBucket),
    MessageSent(String),
    Failed(String),
    Settled,
    Head,
    History,
    NativeHistory(Option<crate::log::ContextBoundary>),
    Append(AgentEvent<'a>),
    AppendBatch(Vec<AgentEvent<'static>>),
    Profile {
        role: AgentRole,
        binding: SessionBinding,
    },
    CacheKey(crate::inference::PromptCacheKey),
    Rewind {
        at: UnixMs,
        to: AgentEventPos,
    },
    ClaudeRewind {
        at: UnixMs,
        to: Option<AgentEventPos>,
        rewind: Option<ClaudeRewind>,
    },
    CompleteClaudeRewind(uuid::Uuid),
    ClaudeAccount,
    ClaudePendingOutput,
    UsageTotal,
    Turn {
        at: UnixMs,
        edge: TurnEdge,
    },
}

#[derive(Encode, Decode)]
pub(crate) enum Reply {
    Team(Option<crate::multi_agent_tools::Team>),
    Shared(SharedReply),
    Head(AgentHead),
    History {
        next: AgentEventPos,
        rows: Vec<(AgentEventPos, AgentEvent<'static>)>,
    },
    NativeHistory {
        boundary: crate::log::ContextBoundary,
        recovery: crate::log::NativeRecovery,
        rows: Vec<(AgentEventPos, AgentEvent<'static>)>,
    },
    Boundary(crate::log::ContextBoundary),
    Position(AgentEventPos),
    ClaudeAccount(String),
    ClaudePendingOutput(Option<crate::ClaudeOutputBatch>),
    Usage(AgentUsageBucket),
    Error(String),
    Done,
}

#[derive(Encode, Decode)]
pub(crate) enum Message<'a> {
    Stop,
    Stopped {
        error: Option<String>,
    },
    Bootstrap(Bootstrap),
    Ready {
        status: crate::AgentStatus,
    },
    Control {
        id: u64,
        body: Control,
    },
    Controlled {
        id: u64,
        error: Option<String>,
    },
    Named(AgentHead),
    Status {
        status: crate::AgentStatus,
        queue: Option<Vec<crate::QueuedInput>>,
    },
    HistoryBatch {
        id: u64,
        rows: Vec<(AgentEventPos, AgentEvent<'static>)>,
    },
    Request {
        id: u64,
        body: Request<'a>,
    },
    Reply {
        id: u64,
        body: Reply,
    },
}

pub(crate) fn decode(bytes: &[u8]) -> io::Result<Message<'static>> {
    let mut remaining = bytes;
    let message = senax_encoder::decode(&mut remaining)
        .map_err(|_| io::Error::other("invalid agent message"))?;
    if !remaining.is_empty() {
        return Err(io::Error::other("trailing agent message data"));
    }
    Ok(message)
}

pub(crate) fn encode(message: &Message<'_>) -> io::Result<bytes::Bytes> {
    let mut bytes = bytes::BytesMut::new();
    senax_encoder::encode_to(message, &mut bytes)
        .map_err(|_| io::Error::other("invalid agent message"))?;
    Ok(bytes.freeze())
}

#[derive(senax_encoder::Encode, senax_encoder::Decode)]
pub(crate) struct Startup {
    pub version: u32,
    pub layout: rho_fs_view::WorksetLayout,
    pub claude: rho_claude::accounts::ClaudePaths,
    pub responses_base_url: String,
}
