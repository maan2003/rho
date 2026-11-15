//! Dialing a terminal on a host.

use std::future::Future;

use futures::channel::mpsc as futures_mpsc;
use rho_rpc::protocol::{Opened, read_frame, write_open};

use crate::protocol::{
    Open, TermClientFrame, TermServerFrame, TerminalInfo, TerminalList, TerminalOpen,
};

/// Dials a dedicated terminal stream to the agent's terminal
/// `terminal_id` on the host `link` reaches, and runs the handshake: it
/// attaches while that terminal runs and spawns it when it does not.
pub fn open(
    link: &rho_agent_hosts::Link,
    agent: String,
    terminal_id: u64,
    cols: u16,
    rows: u16,
) -> impl Future<Output = anyhow::Result<TerminalChannel>> + Send + 'static {
    link.run(move |dialer| dial_terminal(dialer, agent, terminal_id, cols, rows))
}

/// The running terminals on the host `link` reaches, of one agent when it
/// names one.
pub fn list(
    link: &rho_agent_hosts::Link,
    agent: Option<String>,
) -> impl Future<Output = anyhow::Result<Vec<TerminalInfo>>> + Send + 'static {
    link.run(move |dialer| dial_terminal_list(dialer, agent))
}

/// The id a fresh terminal of an agent takes: one past the highest of
/// those running, so it never collides with one.
pub fn next_terminal_id(running: &[TerminalInfo]) -> u64 {
    running
        .iter()
        .map(|info| info.terminal_id.saturating_add(1))
        .max()
        .unwrap_or(0)
}

/// One attached terminal: a dedicated stream carrying
/// [`crate::protocol`] frames after the handshake. Dropping the
/// owner cancels the attachment; the terminal keeps running in the agent host.
pub struct TerminalChannel {
    pub terminal_id: u64,
    pub frames: futures_mpsc::Receiver<anyhow::Result<TermServerFrame>>,
    pub input: futures_mpsc::Sender<TermClientFrame>,
    pub transport: rho_rpc::ChannelTask,
}

async fn dial_terminal_list(
    dialer: rho_agent_hosts::Dialer,
    agent: Option<String>,
) -> anyhow::Result<Vec<TerminalInfo>> {
    let mut stream = dialer
        .open(<TerminalList as rho_rpc::protocol::Call>::PRIORITY)
        .await?;
    rho_rpc::protocol::call(&mut stream, TerminalList { agent }).await
}

/// Dials a dedicated terminal stream to `terminal_id`: attach while it
/// runs, spawn it when it does not.
async fn dial_terminal(
    dialer: rho_agent_hosts::Dialer,
    agent: String,
    terminal_id: u64,
    cols: u16,
    rows: u16,
) -> anyhow::Result<TerminalChannel> {
    let running = dial_terminal_list(dialer.clone(), Some(agent.clone())).await?;
    let create = !running.iter().any(|info| info.terminal_id == terminal_id);
    let open = Open::Terminal {
        agent,
        terminal_id,
        open: if create {
            TerminalOpen::Create { attach: true }
        } else {
            TerminalOpen::Attach
        },
        cols,
        rows,
    };
    // Interactive streams outrank calls and sessions (priority 1 and below).
    let mut stream = dialer.open(Some(50)).await?;
    write_open(&mut stream, &open).await?;
    if let Opened::Refused { reason } = read_frame(&mut stream).await? {
        anyhow::bail!("{reason}")
    }

    let channel = stream.into_channel(rho_rpc::ChannelConfig {
        tx_limit: rho_rpc::protocol::MAX_FRAME_LEN,
        rx_limit: rho_rpc::protocol::MAX_FRAME_LEN,
        tx_capacity: 64,
        rx_capacity: 256,
    });
    let (input, frames, transport) = channel.into_parts();
    Ok(TerminalChannel {
        terminal_id,
        frames,
        input,
        transport,
    })
}
