//! One host's agents, as the client reaches them: calls, terminals,
//! shells and visualizations, each on a stream of its
//! own. The session beside them is [`crate::stream`].

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::SinkExt as _;
use futures::channel::mpsc as futures_mpsc;
use rho_agent_host_proto::agents::{self, VisualizationContent};
use rho_agent_host_proto::{Call, Opened, read_frame, shell, term, write_frame, write_open};
use rho_hosts::{Dialer, Link};

/// One host's agents, as a client reaches them. Cheap to clone; valid
/// across reconnects, since each use dials whatever connection is up.
#[derive(Clone)]
pub struct AgentsLink {
    link: Link,
}

impl AgentsLink {
    pub fn new(link: Link) -> Self {
        Self { link }
    }

    /// Agents with no host behind them, for an agent whose host has been
    /// detached: its retained transcript still renders, and asking for
    /// anything reports the same "not connected" as a dropped connection.
    pub fn detached() -> Self {
        Self::new(Link::detached())
    }

    /// Makes one call on a stream of its own. The answer needs no
    /// particular executor; a refusal is an error.
    pub fn call<C: Call>(
        &self,
        call: C,
    ) -> impl Future<Output = anyhow::Result<C::Reply>> + Send + 'static {
        self.link.run(|dialer| dial_call(dialer, call))
    }

    /// Dials a dedicated terminal stream for an agent and runs the
    /// handshake: attach its first running terminal (spawning the default
    /// one when none run), or spawn a fresh one with `new`.
    pub fn open_terminal(
        &self,
        agent: String,
        new: bool,
        cols: u16,
        rows: u16,
    ) -> impl Future<Output = anyhow::Result<TerminalChannel>> + Send + 'static {
        self.link
            .run(move |dialer| dial_terminal(dialer, agent, new, cols, rows))
    }

    /// Starts the agent's shell when absent, otherwise attaches.
    pub fn open_shell(
        &self,
        agent: String,
    ) -> impl Future<Output = anyhow::Result<ShellChannel>> + Send + 'static {
        self.link.run(|dialer| start_and_dial_shell(dialer, agent))
    }

    /// Gracefully closes the agent's persistent shell.
    pub fn close_shell(
        &self,
        agent: String,
    ) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
        self.call(shell::ShellClose { agent })
    }

    /// A recorded visualization.
    pub fn visualization(
        &self,
        id: String,
    ) -> impl Future<Output = anyhow::Result<VisualizationContent>> + Send + 'static {
        self.call(agents::Visualization { id })
    }
}

/// One call on a stream of its own. A refusal is an error.
async fn dial_call<C: Call>(dialer: Dialer, call: C) -> anyhow::Result<C::Reply> {
    let mut stream = dialer.open(C::PRIORITY).await?;
    rho_agent_host_proto::call(&mut stream, call).await
}

async fn dial_stream(dialer: Dialer) -> anyhow::Result<rho_rpc::Stream> {
    // Interactive streams outrank calls and sessions (priority 1 and below).
    dialer.open(Some(50)).await
}

/// One attached terminal: a dedicated stream carrying
/// [`rho_agent_host_proto::term`] frames after the handshake. Dropping the
/// owner cancels the attachment; the terminal keeps running in the daemon.
pub struct TerminalChannel {
    pub terminal_id: u64,
    pub frames: futures_mpsc::Receiver<anyhow::Result<rho_agent_host_proto::term::TermServerFrame>>,
    pub input: futures_mpsc::Sender<rho_agent_host_proto::term::TermClientFrame>,
    pub transport: rho_rpc::ChannelTask,
}

/// One attachment to an agent's daemon-owned Comint-style shell. Dropping
/// `input` detaches this GUI but does not stop the shell process.
pub struct ShellChannel {
    pub frames: futures_mpsc::Receiver<rho_agent_host_proto::shell::ShellServerFrame>,
    pub submit: tokio::sync::mpsc::Sender<ShellSubmission>,
    pub control: tokio::sync::mpsc::Sender<rho_agent_host_proto::shell::ShellClientFrame>,
}

pub struct ShellSubmission {
    pub command: String,
    pub accepted: tokio::sync::oneshot::Sender<u64>,
}

/// One agent's running terminals.
async fn dial_terminal_list(
    dialer: Dialer,
    agent: String,
) -> anyhow::Result<Vec<rho_agent_host_proto::term::TerminalInfo>> {
    dial_call(dialer, term::TerminalList { agent: Some(agent) }).await
}

/// Dials a dedicated terminal stream: attach the agent's first running
/// terminal (creating id 0 when none run), or spawn a fresh one with `new`.
async fn dial_terminal(
    dialer: Dialer,
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
    let open = term::Open::Terminal {
        agent,
        terminal_id,
        open: if create {
            rho_agent_host_proto::term::TerminalOpen::Create { attach: true }
        } else {
            rho_agent_host_proto::term::TerminalOpen::Attach
        },
        cols,
        rows,
    };
    let mut stream = dial_stream(dialer).await?;
    write_open(&mut stream, &open).await?;
    if let Opened::Refused { reason } = read_frame(&mut stream).await? {
        anyhow::bail!("{reason}")
    }

    let channel = stream.into_channel(rho_rpc::ChannelConfig {
        tx_limit: rho_agent_host_proto::MAX_FRAME_LEN,
        rx_limit: rho_agent_host_proto::MAX_FRAME_LEN,
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

/// Starts the agent's shell when none runs, then attaches.
async fn start_and_dial_shell(dialer: Dialer, agent: String) -> anyhow::Result<ShellChannel> {
    let list = shell::ShellList {
        agent: Some(agent.clone()),
    };
    if dial_call(dialer.clone(), list).await?.is_empty() {
        let start = shell::ShellStart {
            agent: agent.clone(),
        };
        dial_call(dialer.clone(), start).await?;
    }
    dial_shell(dialer, agent).await
}

async fn dial_shell(dialer: Dialer, agent: String) -> anyhow::Result<ShellChannel> {
    let mut stream = dial_stream(dialer).await?;
    write_open(&mut stream, &shell::Open::Attach { agent }).await?;
    if let Opened::Refused { reason } = read_frame(&mut stream).await? {
        anyhow::bail!("{reason}")
    }

    let (mut reader, mut writer) = tokio::io::split(stream);
    let (mut frames_tx, frames_rx) = futures_mpsc::channel(32);
    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::channel::<ShellSubmission>(8);
    let (control_tx, mut control_rx) =
        tokio::sync::mpsc::channel::<rho_agent_host_proto::shell::ShellClientFrame>(8);
    let pending = Arc::new(Mutex::new(
        HashMap::<u64, tokio::sync::oneshot::Sender<u64>>::new(),
    ));
    let reader_pending = Arc::clone(&pending);
    tokio::spawn(async move {
        while let Ok(frame) =
            read_frame::<_, rho_agent_host_proto::shell::ShellServerFrame>(&mut reader).await
        {
            match frame {
                rho_agent_host_proto::shell::ShellServerFrame::Accepted {
                    submission,
                    execution,
                } => {
                    let accepted = reader_pending.lock().unwrap().remove(&submission);
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(execution);
                    }
                }
                frame => {
                    if frames_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            }
        }
        reader_pending.lock().unwrap().clear();
    });
    tokio::spawn(async move {
        let mut next_submission = 1_u64;
        loop {
            let result = tokio::select! {
                biased;
                Some(frame) = control_rx.recv() => write_frame(&mut writer, &frame).await,
                Some(submission) = submit_rx.recv() => {
                    let submission_id = next_submission;
                    next_submission = next_submission.wrapping_add(1).max(1);
                    pending.lock().unwrap().insert(submission_id, submission.accepted);
                    let result = write_frame(
                        &mut writer,
                        &rho_agent_host_proto::shell::ShellClientFrame::Submit {
                            submission: submission_id,
                            command: submission.command,
                        },
                    )
                    .await;
                    if result.is_err() {
                        pending.lock().unwrap().remove(&submission_id);
                    }
                    result
                }
                else => break,
            };
            if result.is_err() {
                break;
            }
        }
        pending.lock().unwrap().clear();
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
    });
    Ok(ShellChannel {
        frames: frames_rx,
        submit: submit_tx,
        control: control_tx,
    })
}
