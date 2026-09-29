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

#[derive(Encode, Decode)]
pub(crate) enum Message {
    Policy(Vec<u8>),
    Action { id: u64, action: Action },
    Attach { id: u64, port: Port, attach: Attach },
    Reply { id: u64, body: Reply },
    Detach(Port),
    /// The agent host is handing over to its successor: write `Paused` and
    /// then nothing until `Resume`.
    Pause,
    /// The worker's last frame before a handoff.
    Paused,
    Resume,
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
