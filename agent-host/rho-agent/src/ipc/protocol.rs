//! Private agent messages shared by the host and worker.
use std::io;

use rho_agent_types::{ContentPart, InputWake, MailId, Wpos};
use senax_encoder::{Decode, Encode};

use crate::log::AgentUsageBucket;
use crate::record::Record;
use crate::store::{AgentHead, NativeCursor};

pub(crate) const VERSION: u32 = 25;

/// The worker's second connection, inherited at this fd. It carries only the
/// requests the worker makes of the agent host and their answers, so a host
/// handing over can stop reading requests yet read everything else.
pub(crate) const REQUESTS_FD: i32 = 3;

#[derive(Encode, Decode)]
pub(crate) enum Control {
    Retire,
    /// Log this input as the agent's own row; the ack follows the commit.
    Input {
        content: Vec<ContentPart>,
        wake: InputWake,
        mail: Option<MailId>,
    },
    TellTail,
    Compact,
    Cancel,
    Retry,
    CacheKey,
    Rewind(u32),
}

/// A notebook host function the agent host answers for the worker.
#[derive(Debug, Encode, Decode)]
pub(crate) enum SharedCall {
    Papercut(crate::papercut::PapercutArgs),
}

/// How the agent host answered a [`SharedCall`]: text for the model either way.
#[derive(Encode, Decode)]
pub(crate) enum SharedReply {
    Ok(String),
    Err(String),
}

/// The rows a native context reads: from its start through the newest row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub(crate) struct ContextBoundary {
    pub from: Wpos,
    pub through: Wpos,
}

#[derive(Encode, Decode)]
pub(crate) enum Request<'a> {
    SharedTool(SharedCall),
    Usage(AgentUsageBucket),
    Head,
    /// Every visible row of the agent.
    History,
    /// The agent's visible rows from its native context start, or from the
    /// given boundary.
    NativeHistory(Option<ContextBoundary>),
    Append(Record<'a>),
    AppendBatch(Vec<Record<'static>>),
    ClaudeAccount,
    ClaudePendingOutput,
}

#[derive(Encode, Decode)]
pub(crate) enum Reply {
    Shared(SharedReply),
    Head(AgentHead),
    History {
        rows: Vec<(Wpos, Record<'static>)>,
    },
    NativeHistory {
        boundary: ContextBoundary,
        cursor: NativeCursor,
        rows: Vec<(Wpos, Record<'static>)>,
    },
    Boundary(ContextBoundary),
    Position(Wpos),
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
    /// Start the agent on this port.
    Bootstrap,
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
    /// The agent's head changed outside its loop: a new spec or title.
    Named(AgentHead),
    Status {
        status: crate::AgentStatus,
    },
    HistoryBatch {
        id: u64,
        rows: Vec<(Wpos, Record<'static>)>,
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
    pub workset: rho_agent_types::WorksetId,
    pub layout: rho_fs_view::Layout,
    pub claude: rho_claude::accounts::ClaudePaths,
    pub responses_base_url: String,
    /// The host's `init.py`, run in the workset namespace at each start.
    pub init: Option<camino::Utf8PathBuf>,
    /// Where the workset namespace and new notebooks start.
    pub root: camino::Utf8PathBuf,
}
