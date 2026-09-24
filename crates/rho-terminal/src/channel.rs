//! Dialing a terminal on a host.

use std::future::Future;

use futures::channel::mpsc as futures_mpsc;
use rho_rpc::protocol::{Opened, read_frame, write_open};

use crate::protocol::{
    Open, TermClientFrame, TermServerFrame, TerminalInfo, TerminalList, TerminalOpen,
};

/// Dials a dedicated terminal stream for an agent on the host `link`
/// reaches, and runs the handshake: attach its first running terminal
/// (spawning the default one when none run), or spawn a fresh one with
/// `new`.
pub fn open(
    link: &rho_hosts::Link,
    agent: String,
    new: bool,
    cols: u16,
    rows: u16,
) -> impl Future<Output = anyhow::Result<TerminalChannel>> + Send + 'static {
    link.run(move |dialer| dial_terminal(dialer, agent, new, cols, rows))
}

/// One attached terminal: a dedicated stream carrying
/// [`crate::protocol`] frames after the handshake. Dropping the
/// owner cancels the attachment; the terminal keeps running in the daemon.
pub struct TerminalChannel {
    pub terminal_id: u64,
    pub frames: futures_mpsc::Receiver<anyhow::Result<TermServerFrame>>,
    pub input: futures_mpsc::Sender<TermClientFrame>,
    pub transport: rho_rpc::ChannelTask,
}

/// One agent's running terminals.
async fn dial_terminal_list(
    dialer: rho_hosts::Dialer,
    agent: String,
) -> anyhow::Result<Vec<TerminalInfo>> {
    let mut stream = dialer
        .open(<TerminalList as rho_rpc::protocol::Call>::PRIORITY)
        .await?;
    rho_rpc::protocol::call(&mut stream, TerminalList { agent: Some(agent) }).await
}

/// Dials a dedicated terminal stream: attach the agent's first running
/// terminal (creating id 0 when none run), or spawn a fresh one with `new`.
async fn dial_terminal(
    dialer: rho_hosts::Dialer,
    agent: String,
    new: bool,
    cols: u16,
    rows: u16,
) -> anyhow::Result<TerminalChannel> {
    let running = dial_terminal_list(dialer.clone(), agent.clone()).await?;
    let (terminal_id, create) = if new {
        let next = running
            .iter()
            .map(|info| info.terminal_id.saturating_add(1))
            .max()
            .unwrap_or(0);
        (next, true)
    } else {
        match running.first() {
            Some(info) => (info.terminal_id, false),
            None => (0, true),
        }
    };
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
