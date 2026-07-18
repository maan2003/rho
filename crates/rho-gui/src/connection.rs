//! Daemon connection: an IO task on the shared tokio runtime ([`gpui_tokio`]),
//! bridged to the GUI through channels. Inbound server messages become
//! [`ConnEvent`]s on a futures channel the workspace awaits (no polling);
//! outbound commands are fire-and-forget.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use anyhow::Context as _;
use camino::Utf8PathBuf;
use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use gpui::{App, Task};
use gpui_tokio::Tokio;
use rho_ui_proto::client::Client;
use rho_ui_proto::remote::AgentRemoteFrame;
use rho_ui_proto::{
    AgentId, ClientMessage, ServerMessage, UiProject, UiTopic, WorkspaceInfo, read_frame,
    write_frame,
};
use tokio::io::AsyncReadExt as _;

use crate::workspace::AttachTarget;

trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncStream for T {}

struct IrohStream {
    inner: tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    _endpoint: iroh::Endpoint,
}

impl tokio::io::AsyncRead for IrohStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for IrohStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

pub enum ConnEvent {
    Ready {
        topics: Vec<UiTopic>,
        projects: Vec<UiProject>,
        default_topic_id: rho_ui_proto::TopicId,
        machine_seed: u64,
        agent_counter: u64,
        workspace_counter: u64,
    },
    TopicCreated(UiTopic),
    /// The daemon created an agent this connection asked for.
    AgentCreated(AgentId),
    AgentLoaded(AgentId),
    Frame {
        agent_id: AgentId,
        frame: AgentRemoteFrame,
        /// Holds aggregate decode budget until the GUI consumes this frame.
        allocation: Option<AgentFrameAllocation>,
    },
    TurnCancelled,
    AgentAttention {
        agent_id: AgentId,
        attention: rho_ui_proto::UiAttention,
    },
    ServerError(String),
    Disconnected(String),
}

/// One open zed channel: a dedicated stream to the daemon carrying raw
/// prost-envelope frames after the handshake. Dropping `outgoing` half-closes
/// the stream; the daemon tears the headless project session down on EOF.
pub struct ZedChannel {
    /// The project root to open worktrees under (the daemon's view of the
    /// workspace checkout).
    pub root: Utf8PathBuf,
    /// prost-encoded envelopes, GUI → headless project.
    pub outgoing: futures_mpsc::UnboundedSender<Vec<u8>>,
    /// prost-encoded envelopes, headless project → GUI.
    pub incoming: futures_mpsc::UnboundedReceiver<Vec<u8>>,
}

/// How to dial an extra stream to the daemon for a zed channel: locally a
/// second Unix connection, remotely another bi-stream on the already
/// authenticated iroh connection. Set by the IO task once connected.
#[derive(Clone)]
enum ChannelDialer {
    Unix(PathBuf),
    Iroh(iroh::endpoint::Connection),
}

async fn dial_channel(
    dialer: ChannelDialer,
    workspace: WorkspaceInfo,
) -> anyhow::Result<ZedChannel> {
    let mut stream = match dialer {
        ChannelDialer::Unix(socket_path) => {
            let client = Client::connect(&socket_path)
                .await
                .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
            Box::new(client.into_stream()) as Box<dyn AsyncStream>
        }
        ChannelDialer::Iroh(connection) => {
            let (send, recv) = connection
                .open_bi()
                .await
                .context("open iroh zed-channel stream")?;
            Box::new(tokio::io::join(recv, send)) as Box<dyn AsyncStream>
        }
    };
    write_frame(&mut stream, &ClientMessage::ChannelOpen { workspace }).await?;
    let reply: ServerMessage = read_frame(&mut stream).await?;
    let root = match reply {
        ServerMessage::ChannelOpened { root } => root,
        ServerMessage::ChannelClosed { reason } => {
            anyhow::bail!("daemon refused zed channel: {reason}")
        }
        _ => anyhow::bail!("unexpected reply to ChannelOpen"),
    };

    let (mut reader, mut writer) = tokio::io::split(stream);
    let (incoming_tx, incoming_rx) = futures_mpsc::unbounded();
    let (outgoing_tx, mut outgoing_rx) = futures_mpsc::unbounded::<Vec<u8>>();
    tokio::spawn(async move {
        while let Ok(Some(payload)) = rho_ui_proto::read_raw_frame(&mut reader).await {
            if incoming_tx.unbounded_send(payload).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(payload) = outgoing_rx.next().await {
            if rho_ui_proto::write_raw_frame(&mut writer, &payload).await.is_err() {
                break;
            }
        }
        // Half-close so the daemon sees EOF and tears the session down.
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
    });
    Ok(ZedChannel {
        root,
        outgoing: outgoing_tx,
        incoming: incoming_rx,
    })
}

pub struct Connection {
    commands: futures_mpsc::UnboundedSender<ClientMessage>,
    iroh: bool,
    /// `None` until the IO task connects; channels cannot open earlier.
    dialer: Arc<Mutex<Option<ChannelDialer>>>,
    /// Dropping this aborts the IO task, tearing the connection down with the
    /// workspace.
    _io_task: Task<Result<(), gpui_tokio::JoinError>>,
}

impl Connection {
    pub fn send(&self, message: ClientMessage) {
        let _ = self.commands.unbounded_send(message);
    }

    pub fn focus_agent(&self, agent_id: Option<AgentId>) {
        if self.iroh {
            self.send(ClientMessage::AgentStreamFocus { agent_id });
        }
    }

    /// Dials a dedicated stream for a zed channel onto `workspace` and runs
    /// the handshake.
    pub fn open_channel(
        &self,
        workspace: WorkspaceInfo,
        cx: &App,
    ) -> Task<Result<anyhow::Result<ZedChannel>, gpui_tokio::JoinError>> {
        let dialer = self.dialer.lock().unwrap().clone();
        Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            dial_channel(dialer, workspace).await
        })
    }
}

pub fn spawn(
    target: AttachTarget,
    cx: &App,
) -> (Connection, futures_mpsc::UnboundedReceiver<ConnEvent>) {
    let iroh = matches!(&target, AttachTarget::Iroh { .. });
    let (event_tx, event_rx) = futures_mpsc::unbounded();
    let (command_tx, command_rx) = futures_mpsc::unbounded();
    let dialer = Arc::new(Mutex::new(None));
    let io_dialer = dialer.clone();
    let io_task = Tokio::spawn(cx, async move {
        if let Err(error) = run(target, &event_tx, command_rx, &io_dialer).await {
            let _ = event_tx.unbounded_send(ConnEvent::Disconnected(format!("{error:#}")));
        }
    });
    (
        Connection {
            commands: command_tx,
            iroh,
            dialer,
            _io_task: io_task,
        },
        event_rx,
    )
}

async fn run(
    target: AttachTarget,
    events: &futures_mpsc::UnboundedSender<ConnEvent>,
    mut commands: futures_mpsc::UnboundedReceiver<ClientMessage>,
    dialer: &Mutex<Option<ChannelDialer>>,
) -> anyhow::Result<()> {
    let (mut stream, agent_connection) = match target {
        AttachTarget::Unix(socket_path) => {
            let client = Client::connect(&socket_path)
                .await
                .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
            *dialer.lock().unwrap() = Some(ChannelDialer::Unix(socket_path));
            (Box::new(client.into_stream()) as Box<dyn AsyncStream>, None)
        }
        AttachTarget::Iroh {
            endpoint_id,
            ssh_destination,
            remote_rho,
        } => {
            let (stream, connection) =
                connect_iroh(endpoint_id, &ssh_destination, &remote_rho).await?;
            *dialer.lock().unwrap() = Some(ChannelDialer::Iroh(connection.clone()));
            (stream, Some(connection))
        }
    };
    write_frame(&mut stream, &ClientMessage::Subscribe).await?;
    let message: ServerMessage = read_frame(&mut stream).await?;
    let ServerMessage::Ready {
        topics,
        projects,
        default_topic_id,
        machine_seed,
        agent_counter,
        workspace_counter,
    } = message
    else {
        anyhow::bail!("rho daemon did not send ready message");
    };
    if events
        .unbounded_send(ConnEvent::Ready {
            topics,
            projects,
            default_topic_id,
            machine_seed,
            agent_counter,
            workspace_counter,
        })
        .is_err()
    {
        return Ok(());
    }

    let agent_stream_task = agent_connection.map(|connection| {
        let events = events.clone();
        tokio::spawn(run_agent_streams(connection, events))
    });

    let (mut reader, mut writer) = tokio::io::split(stream);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = commands.next().await {
            if write_frame(&mut writer, &message).await.is_err() {
                break;
            }
        }
    });

    loop {
        let message: ServerMessage = match read_frame(&mut reader).await {
            Ok(message) => message,
            Err(error) => {
                let _ = events.unbounded_send(ConnEvent::Disconnected(error.to_string()));
                break;
            }
        };
        let event = match message {
            ServerMessage::Ready {
                topics,
                projects,
                default_topic_id,
                machine_seed,
                agent_counter,
                workspace_counter,
            } => Some(ConnEvent::Ready {
                topics,
                projects,
                default_topic_id,
                machine_seed,
                agent_counter,
                workspace_counter,
            }),
            ServerMessage::TopicCreated { topic } => Some(ConnEvent::TopicCreated(topic)),
            ServerMessage::AgentCreated { agent_id, .. } => Some(ConnEvent::AgentCreated(agent_id)),
            ServerMessage::AgentLoaded { agent_id } => Some(ConnEvent::AgentLoaded(agent_id)),
            ServerMessage::Agent { agent_id, frame } => Some(ConnEvent::Frame {
                agent_id,
                frame,
                allocation: None,
            }),
            ServerMessage::TurnCancelled { .. } => Some(ConnEvent::TurnCancelled),
            ServerMessage::AgentAttention {
                agent_id,
                attention,
            } => Some(ConnEvent::AgentAttention {
                agent_id,
                attention,
            }),
            ServerMessage::Error { message } => Some(ConnEvent::ServerError(message)),
            ServerMessage::Pong
            | ServerMessage::LandLeaseQueued { .. }
            | ServerMessage::LandLeaseGranted { .. }
            | ServerMessage::LandStatus { .. }
            | ServerMessage::McpAgentToolResult(_)
            | ServerMessage::PlatformStatus { .. }
            | ServerMessage::IrohApproved { .. }
            | ServerMessage::IrohRevoked { .. }
            | ServerMessage::PrCommandResult { .. } => None,
            // Zed channel handshake replies belong to dedicated channel
            // streams, never the UI session.
            ServerMessage::ChannelOpened { .. }
            | ServerMessage::ChannelClosed { .. }
            | ServerMessage::AgentStreamOpened { .. } => None,
        };
        if let Some(event) = event
            && events.unbounded_send(event).is_err()
        {
            break;
        }
    }
    writer_task.abort();
    if let Some(task) = agent_stream_task {
        task.abort();
    }
    Ok(())
}

async fn run_agent_streams(
    connection: iroh::endpoint::Connection,
    events: futures_mpsc::UnboundedSender<ConnEvent>,
) {
    const AGENT_FRAME_ALLOCATION_BUDGET: usize = 128 * 1024 * 1024;
    let mut streams = tokio::task::JoinSet::new();
    let allocation_budget = Arc::new(AgentFrameAllocationBudget::new(
        AGENT_FRAME_ALLOCATION_BUDGET,
    ));
    loop {
        tokio::select! {
            accepted = connection.accept_uni() => {
                let Ok(mut recv) = accepted else { break };
                let events = events.clone();
                let allocation_budget = allocation_budget.clone();
                streams.spawn(async move {
                    let (header, header_allocation) =
                        read_agent_stream_message(&mut recv, &allocation_budget).await?;
                    let ServerMessage::AgentStreamOpened { agent_id } = header
                    else {
                        anyhow::bail!("invalid agent stream header");
                    };
                    drop(header_allocation);
                    loop {
                        let (message, allocation) =
                            read_agent_stream_message(&mut recv, &allocation_budget).await?;
                        let ServerMessage::Agent {
                            agent_id: frame_agent_id,
                            frame,
                        } = message
                        else {
                            anyhow::bail!("invalid message on agent stream");
                        };
                        anyhow::ensure!(frame_agent_id == agent_id, "agent stream id changed");
                        if events
                            .unbounded_send(ConnEvent::Frame {
                                agent_id,
                                frame,
                                allocation: Some(allocation),
                            })
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), anyhow::Error>(())
                });
            }
            joined = streams.join_next(), if !streams.is_empty() => {
                match joined {
                    Some(Ok(Err(error))) => {
                        let _ = events.unbounded_send(ConnEvent::ServerError(
                            format!("agent state stream failed; retrying: {error:#}"),
                        ));
                    }
                    Some(Err(error)) => {
                        let _ = events.unbounded_send(ConnEvent::ServerError(
                            format!("agent state stream task failed: {error}"),
                        ));
                    }
                    Some(Ok(Ok(()))) | None => {}
                }
            }
        }
    }
}

async fn read_agent_stream_message(
    recv: &mut iroh::endpoint::RecvStream,
    allocation_budget: &Arc<AgentFrameAllocationBudget>,
) -> anyhow::Result<(ServerMessage, AgentFrameAllocation)> {
    let len = recv
        .read_u32_le()
        .await
        .context("read agent stream frame length")? as usize;
    anyhow::ensure!(
        len <= rho_ui_proto::MAX_FRAME_LEN,
        "agent stream frame length {len} exceeds {}",
        rho_ui_proto::MAX_FRAME_LEN,
    );
    let allocation = allocation_budget.reserve(len).await;
    let mut payload = vec![0; len];
    recv.read_exact(&mut payload)
        .await
        .context("read agent stream frame payload")?;
    let mut payload = payload.as_slice();
    let message = senax_encoder::unpack(&mut payload).context("unpack agent stream frame")?;
    Ok((message, allocation))
}

struct AgentFrameAllocationBudget {
    available: std::sync::atomic::AtomicUsize,
    notify: tokio::sync::Notify,
}

impl AgentFrameAllocationBudget {
    fn new(bytes: usize) -> Self {
        Self {
            available: std::sync::atomic::AtomicUsize::new(bytes),
            notify: tokio::sync::Notify::new(),
        }
    }

    async fn reserve(self: &Arc<Self>, bytes: usize) -> AgentFrameAllocation {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut available = self.available.load(std::sync::atomic::Ordering::Acquire);
            while available >= bytes {
                match self.available.compare_exchange_weak(
                    available,
                    available - bytes,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                ) {
                    Ok(_) => {
                        return AgentFrameAllocation {
                            budget: self.clone(),
                            bytes,
                        };
                    }
                    Err(current) => available = current,
                }
            }
            notified.as_mut().await;
        }
    }
}

pub(crate) struct AgentFrameAllocation {
    budget: Arc<AgentFrameAllocationBudget>,
    bytes: usize,
}

impl Drop for AgentFrameAllocation {
    fn drop(&mut self) {
        self.budget
            .available
            .fetch_add(self.bytes, std::sync::atomic::Ordering::Release);
        self.budget.notify.notify_waiters();
    }
}

async fn connect_iroh(
    daemon_id: iroh::EndpointId,
    ssh_destination: &str,
    remote_rho: &str,
) -> anyhow::Result<(Box<dyn AsyncStream>, iroh::endpoint::Connection)> {
    // The native client's identity intentionally lives only as long as this
    // process. The daemon can trust it in memory via an existing SSH login.
    let secret = iroh::SecretKey::generate();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(secret)
        .transport_config(
            iroh::endpoint::QuicTransportConfig::builder()
                .max_concurrent_uni_streams(1024u32.into())
                .build(),
        )
        .bind()
        .await
        .context("bind iroh client endpoint")?;
    tracing::info!(
        destination = ssh_destination,
        "trusting ephemeral iroh client over SSH"
    );
    trust_in_memory_over_ssh(ssh_destination, remote_rho, endpoint.id()).await?;
    tracing::info!(
        destination = ssh_destination,
        "ephemeral iroh client trusted over SSH"
    );
    let connection = endpoint
        .connect(daemon_id, rho_ui_proto::IROH_ALPN)
        .await
        .context("connect to daemon over iroh")?;
    anyhow::ensure!(
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            rho_iroh_auth::authenticate_client(&connection, endpoint.id()),
        )
        .await
        .map_err(|_| anyhow::anyhow!("iroh authentication timed out"))??
            == rho_iroh_auth::ClientAuthResult::Approved,
        "daemon did not approve SSH-trusted iroh client"
    );
    let (send, recv) = connection.open_bi().await.context("open iroh UI stream")?;
    send.set_priority(1)
        .context("set iroh UI control stream priority")?;
    let stream = Box::new(IrohStream {
        inner: tokio::io::join(recv, send),
        _endpoint: endpoint,
    });
    Ok((stream, connection))
}

async fn trust_in_memory_over_ssh(
    destination: &str,
    remote_rho: &str,
    endpoint_id: iroh::EndpointId,
) -> anyhow::Result<()> {
    anyhow::ensure!(!destination.starts_with('-'), "invalid SSH destination");
    anyhow::ensure!(
        is_safe_remote_executable(remote_rho),
        "invalid remote rho executable path"
    );
    // EndpointId's text form has a fixed safe alphabet even though OpenSSH
    // sends the remote argv through the login shell.
    let endpoint_id = endpoint_id.to_string();
    let status = tokio::process::Command::new("ssh")
        .arg("--")
        .arg(destination)
        .args([remote_rho, "iroh", "trust-in-memory", &endpoint_id])
        .status()
        .await
        .context("run SSH enrollment approval")?;
    anyhow::ensure!(
        status.success(),
        "SSH enrollment approval failed with {status}"
    );
    Ok(())
}

fn is_safe_remote_executable(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'+' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::AgentFrameAllocationBudget;

    #[test]
    fn small_frame_bypasses_waiting_large_allocation() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let budget = Arc::new(AgentFrameAllocationBudget::new(10));
                let held = budget.reserve(6).await;
                let large_budget = budget.clone();
                let large = tokio::spawn(async move { large_budget.reserve(10).await });
                tokio::task::yield_now().await;

                let small =
                    tokio::time::timeout(std::time::Duration::from_millis(100), budget.reserve(4))
                        .await
                        .expect("small allocation should bypass the waiting large one");
                drop(small);
                drop(held);
                drop(large.await.unwrap());
            });
    }
}
