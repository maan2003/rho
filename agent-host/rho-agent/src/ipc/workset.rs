//! Workset messages and payload encoding shared by both processes.
use senax_encoder::{Decode, Encode};

use crate::ipc::transport::Port;

#[derive(Encode, Decode)]
pub enum Action {
    TerminalList,
    ShellList,
    ShellStart {
        agent: rho_agent_types::AgentId,
        cwd: camino::Utf8PathBuf,
        program: std::path::PathBuf,
        pager: std::path::PathBuf,
    },
    ShellClose {
        agent: rho_agent_types::AgentId,
    },
    Desktop {
        agent: rho_agent_types::AgentId,
        session: String,
    },
    DesktopList,
}

#[derive(Encode, Decode)]
pub enum Attach {
    Terminal {
        agent: rho_agent_types::AgentId,
        terminal: u64,
        create: bool,
        cols: u16,
        rows: u16,
        cwd: camino::Utf8PathBuf,
        shell: String,
    },
    Shell {
        agent: rho_agent_types::AgentId,
    },
}

#[derive(Encode, Decode)]
pub enum Reply {
    Done,
    Terminals(Vec<rho_terminal::protocol::TerminalInfo>),
    Shells(Vec<rho_shell_view::protocol::ShellInfo>),
    Error(String),
    Desktop { socket: String },
    DesktopSessions(Vec<rho_desktop_client::protocol::DesktopSession>),
}

/// What the workset's Python asks of the agent host (`_rho`).
#[derive(Encode, Decode)]
pub(crate) enum PyRequest {
    CreateAgent(crate::record::AgentSpec),
    Agents,
    SetSpec {
        agent: rho_agent_types::AgentId,
        spec: crate::record::AgentSpec,
    },
    /// Route a control to the agent's loop, loading it first if needed.
    Control {
        agent: rho_agent_types::AgentId,
        control: crate::ipc::protocol::Control,
    },
    Delivered(rho_agent_types::AgentId),
    /// A message from an agent.
    Post {
        sender: rho_agent_types::AgentId,
        topic: rho_agent_types::TopicId,
        mentions: Vec<rho_agent_types::Mention>,
        content: Vec<rho_agent_types::ContentPart>,
    },
    /// A topic an agent starts, with its first message, in the stream of
    /// that name: created if there is none. Answered with the message.
    /// (`Messages` of one).
    StartTopic {
        sender: rho_agent_types::AgentId,
        stream: String,
        name: String,
        mentions: Vec<rho_agent_types::Mention>,
        content: Vec<rho_agent_types::ContentPart>,
    },
    Join {
        topic: rho_agent_types::TopicId,
        agent: rho_agent_types::AgentId,
    },
    Leave {
        topic: rho_agent_types::TopicId,
        agent: rho_agent_types::AgentId,
    },
    /// Gives a topic its whole head, for `agent`. The stream of that name
    /// is created if there is none.
    ChangeTopic {
        agent: rho_agent_types::AgentId,
        topic: rho_agent_types::TopicId,
        stream: String,
        name: String,
        resolved: bool,
    },
    /// Moves messages for `agent`; answered with the topic they are in.
    Move {
        agent: rho_agent_types::AgentId,
        messages: Vec<rho_agent_types::MessageId>,
        to: PyMoveTo,
    },
    /// The direct conversation of `sender` and `other`, started if needed.
    Direct {
        sender: rho_agent_types::AgentId,
        other: rho_agent_types::AgentId,
    },
    Topic(rho_agent_types::TopicId),
    /// One message: `Messages` with it, or with none.
    Message(rho_agent_types::MessageId),
    /// A conversation's `last` newest messages, oldest first.
    Read {
        topic: rho_agent_types::TopicId,
        last: u64,
    },
    /// What `agent` receives after `after`, answered once there is some.
    Inbox {
        agent: rho_agent_types::AgentId,
        after: rho_agent_types::ChatSeq,
    },
    Clone {
        url: String,
        dest: camino::Utf8PathBuf,
        rev: String,
    },
    Models,
    /// The one agent of the host whose id starts with this.
    ResolveAgent(String),
    /// The agent's own GCCP address (`Text`).
    ExternalAddress(rho_agent_types::AgentId),
    /// The agent accepts a GCCP address under a name.
    ExternalAccept {
        agent: rho_agent_types::AgentId,
        peer: String,
        name: String,
    },
    /// The agent sends a message to an address it accepted; answered with
    /// the name it gave the address (`Text`).
    ExternalSend {
        agent: rho_agent_types::AgentId,
        peer: String,
        body: String,
    },
    /// The addresses the agent accepted.
    ExternalContacts(rho_agent_types::AgentId),
}

/// Where a move from Python puts messages.
#[derive(Encode, Decode)]
pub(crate) enum PyMoveTo {
    Topic(rho_agent_types::TopicId),
    /// A new topic in the stream of that name, created if there is none.
    NewTopic {
        stream: String,
        name: String,
    },
}

#[derive(Encode, Decode)]
pub(crate) enum PyReply {
    Agent(rho_agent_types::AgentId),
    Agents(Vec<(rho_agent_types::AgentId, crate::record::AgentSpec)>),
    Seq(rho_agent_types::ChatSeq),
    Messages(Vec<rho_agent_types::Message>),
    /// A conversation, and the name of its stream if it is a topic.
    Topic(Option<(crate::store::TopicHead, Option<String>)>),
    Models(Vec<crate::record::Runtime>),
    Text(String),
    ExternalContacts(Vec<crate::store::GccpContact>),
    Done,
    Error(String),
}

#[derive(Encode, Decode)]
pub(crate) enum Message {
    Policy(Vec<u8>),
    Action {
        id: u64,
        action: Action,
    },
    Attach {
        id: u64,
        port: Port,
        attach: Attach,
    },
    Reply {
        id: u64,
        body: Reply,
    },
    Detach(Port),
    /// Worker to host: a `_rho` call.
    Python {
        id: u64,
        request: PyRequest,
    },
    /// Host to worker: the answer to a `Python` call.
    PythonReply {
        id: u64,
        body: PyReply,
    },
    /// Host to worker: run code in the workset namespace.
    Eval {
        id: u64,
        code: String,
    },
    /// Worker to host: what an `Eval` printed, and its traceback.
    Evaluated {
        id: u64,
        output: String,
        error: Option<String>,
    },
}

pub(crate) fn encode<T: senax_encoder::Encoder>(value: &T) -> anyhow::Result<bytes::Bytes> {
    let mut bytes = bytes::BytesMut::new();
    senax_encoder::encode_to(value, &mut bytes)
        .map_err(|_| anyhow::anyhow!("encode workset message"))?;
    Ok(bytes.freeze())
}

pub(crate) fn decode<T: senax_encoder::Decoder>(bytes: &[u8]) -> anyhow::Result<T> {
    let mut remaining = bytes;
    let value = senax_encoder::decode(&mut remaining)
        .map_err(|_| anyhow::anyhow!("invalid workset message"))?;
    anyhow::ensure!(remaining.is_empty(), "trailing workset message data");
    Ok(value)
}
