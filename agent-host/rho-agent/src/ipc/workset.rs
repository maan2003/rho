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
    SendMail {
        sender: rho_agent_types::AgentId,
        to: rho_agent_types::Address,
        content: Vec<rho_agent_types::ContentPart>,
        kind: Option<rho_agent_types::SendKind>,
        re: Option<rho_agent_types::MailId>,
    },
    /// Mail to `address` after `after`, answered once there is some.
    Inbox {
        address: rho_agent_types::Address,
        after: rho_agent_types::MailId,
    },
    Clone {
        url: String,
        dest: camino::Utf8PathBuf,
        rev: String,
    },
    Models,
}

#[derive(Encode, Decode)]
pub(crate) enum PyReply {
    Agent(rho_agent_types::AgentId),
    Agents(Vec<(rho_agent_types::AgentId, crate::record::AgentSpec)>),
    Mail(rho_agent_types::MailId),
    Mails(Vec<rho_agent_types::Mail>),
    Models(Vec<crate::record::Runtime>),
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
