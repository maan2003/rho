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
    /// The workset's props, before its first agent.
    SetWorksetProps(rho_agent_types::Props),
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
    /// A message from an agent, under `parent` or as a root.
    SendMail {
        sender: rho_agent_types::AgentId,
        parent: Option<rho_agent_types::MessageId>,
        reply_to: Option<rho_agent_types::MessageId>,
        mentions: Vec<rho_agent_types::Mention>,
        content: Vec<rho_agent_types::ContentPart>,
    },
    Join {
        thread: rho_agent_types::MessageId,
        agent: rho_agent_types::AgentId,
    },
    Leave {
        thread: rho_agent_types::MessageId,
        agent: rho_agent_types::AgentId,
    },
    /// Puts a message and its thread under `parent`, or among the roots,
    /// for `agent`.
    Reparent {
        agent: rho_agent_types::AgentId,
        message: rho_agent_types::MessageId,
        parent: Option<rho_agent_types::MessageId>,
    },
    /// Names a thread for `agent`; empty takes the name back.
    Name {
        agent: rho_agent_types::AgentId,
        thread: rho_agent_types::MessageId,
        name: String,
    },
    /// The thread under a message (`Thread`).
    Thread(rho_agent_types::MessageId),
    /// One message: `Messages` with it, or with none.
    Message(rho_agent_types::MessageId),
    /// A thread's `last` newest messages, oldest first; the roots for
    /// `None`.
    Read {
        thread: Option<rho_agent_types::MessageId>,
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
    /// The full role-prefixed address of an agent, if still on this host.
    AgentHandle(rho_agent_types::AgentId),
    /// The agent's own GCCP address (`Text`).
    ExternalAddress(rho_agent_types::AgentId),
    /// The agent accepts a GCCP address under a name.
    ExternalAccept {
        agent: rho_agent_types::AgentId,
        peer: String,
        name: String,
    },
    /// The agent sends a message to an address it accepted. Answered once
    /// the address's host took it, with the name the agent gave the address
    /// (`Text`), or with what went wrong.
    ExternalSend {
        agent: rho_agent_types::AgentId,
        peer: String,
        body: String,
    },
    /// The addresses the agent accepted.
    ExternalContacts(rho_agent_types::AgentId),
    /// `ping` turns the ping of mail to the user on or off; `None` keeps it.
    Edit {
        agent: rho_agent_types::AgentId,
        message: rho_agent_types::MessageId,
        content: Vec<rho_agent_types::ContentPart>,
        ping: Option<bool>,
    },
    Delete {
        agent: rho_agent_types::AgentId,
        message: rho_agent_types::MessageId,
    },
    Search {
        query: String,
        project: Option<rho_agent_types::MessageId>,
        limit: u64,
    },
    ProjectNamed(String),
}

#[derive(Encode, Decode)]
pub(crate) enum PyReply {
    Agent(rho_agent_types::AgentId),
    Agents(Vec<(rho_agent_types::AgentId, crate::record::AgentSpec)>),
    Seq(rho_agent_types::ChatSeq),
    Messages(Vec<rho_agent_types::Message>),
    Thread(Option<crate::store::ThreadHead>),
    Models(Vec<crate::record::Runtime>),
    Text(String),
    ExternalContacts(Vec<crate::store::GccpContact>),
    Done,
    Error(String),
    Search(Vec<crate::store::MailHit>),
    Project(Option<rho_agent_types::MessageId>),
    Handle(Option<String>),
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
    /// Worker to host, once: init.py ended, and its traceback if it failed.
    /// The host hands out the process only after a clean one.
    Initialized {
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
