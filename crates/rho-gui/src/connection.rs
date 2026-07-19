//! Daemon connection: an IO task on the shared tokio runtime ([`gpui_tokio`]),
//! bridged to the GUI through channels. Inbound server messages become
//! [`ConnEvent`]s on a futures channel the workspace awaits (no polling);
//! outbound commands are fire-and-forget.

use std::collections::HashMap;
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
    AgentId, ClientMessage, GitService, GitTransportRequest, ServerMessage, UiAgentSummary,
    UiProject, UiWorkstream, WorkspaceInfo, read_frame, write_frame,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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
        workstreams: Vec<UiWorkstream>,
        agents: Vec<UiAgentSummary>,
        projects: Vec<UiProject>,
        machine_seed: u64,
        agent_counter: u64,
        workspace_counter: u64,
    },
    WorkstreamCreated(UiWorkstream),
    /// The daemon created an agent this connection asked for. The workstream
    /// rides along so the agent's workstream context resolves before the
    /// next `Ready` refresh lands.
    AgentCreated {
        agent_id: AgentId,
        workstream: rho_ui_proto::WorkstreamId,
    },
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
    GitTransportApproval {
        request_id: u64,
        prompt: String,
        response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
    },
    GitTransportDone {
        request_id: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitApprovalDecision {
    Allow,
    Deny,
    Done,
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
            if rho_ui_proto::write_raw_frame(&mut writer, &payload)
                .await
                .is_err()
            {
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

/// One attached terminal: a dedicated stream carrying [`rho_ui_proto::term`]
/// frames after the handshake. Dropping `input` half-closes the stream,
/// which only detaches — the terminal keeps running in the daemon.
pub struct TerminalChannel {
    pub terminal_id: u64,
    pub frames: futures_mpsc::UnboundedReceiver<rho_ui_proto::term::TermServerFrame>,
    pub input: futures_mpsc::UnboundedSender<rho_ui_proto::term::TermClientFrame>,
}

async fn dial_stream(dialer: ChannelDialer) -> anyhow::Result<Box<dyn AsyncStream>> {
    Ok(match dialer {
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
                .context("open iroh terminal stream")?;
            // Interactive terminals outrank the control session (priority 1).
            send.set_priority(50)
                .context("set iroh terminal stream priority")?;
            Box::new(tokio::io::join(recv, send)) as Box<dyn AsyncStream>
        }
    })
}

/// One-shot `TerminalList` request for one agent's running terminals.
async fn dial_terminal_list(
    dialer: ChannelDialer,
    agent: String,
) -> anyhow::Result<Vec<rho_ui_proto::term::TerminalInfo>> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(
        &mut stream,
        &ClientMessage::TerminalList { agent: Some(agent) },
    )
    .await?;
    match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::TerminalList { terminals } => Ok(terminals),
        ServerMessage::TerminalRefused { reason } => anyhow::bail!("{reason}"),
        _ => anyhow::bail!("unexpected reply to TerminalList"),
    }
}

/// Dials a dedicated terminal stream: attach the agent's first running
/// terminal (creating id 0 when none run), or spawn a fresh one with `new`.
async fn dial_terminal(
    dialer: ChannelDialer,
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
    let open = if create {
        ClientMessage::TerminalCreate {
            agent,
            terminal_id,
            attach: true,
            cols,
            rows,
        }
    } else {
        ClientMessage::TerminalAttach {
            agent,
            terminal_id,
            cols,
            rows,
        }
    };
    let mut stream = dial_stream(dialer).await?;
    write_frame(&mut stream, &open).await?;
    match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::TerminalOpened { .. } => {}
        ServerMessage::TerminalRefused { reason } => anyhow::bail!("{reason}"),
        _ => anyhow::bail!("unexpected reply on terminal stream"),
    }

    let (mut reader, mut writer) = tokio::io::split(stream);
    let (frames_tx, frames_rx) = futures_mpsc::unbounded();
    let (input_tx, mut input_rx) = futures_mpsc::unbounded::<rho_ui_proto::term::TermClientFrame>();
    tokio::spawn(async move {
        while let Ok(frame) =
            read_frame::<_, rho_ui_proto::term::TermServerFrame>(&mut reader).await
        {
            if frames_tx.unbounded_send(frame).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(frame) = input_rx.next().await {
            if write_frame(&mut writer, &frame).await.is_err() {
                break;
            }
        }
        // Half-close so the daemon sees EOF and detaches this client.
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
    });
    Ok(TerminalChannel {
        terminal_id,
        frames: frames_rx,
        input: input_tx,
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

    /// Dials a dedicated terminal stream for an agent and runs the
    /// handshake: attach its first running terminal (spawning the default
    /// one when none run), or spawn a fresh one with `new`.
    pub fn open_terminal(
        &self,
        agent: String,
        new: bool,
        cols: u16,
        rows: u16,
        cx: &App,
    ) -> Task<Result<anyhow::Result<TerminalChannel>, gpui_tokio::JoinError>> {
        let dialer = self.dialer.lock().unwrap().clone();
        Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            dial_terminal(dialer, agent, new, cols, rows).await
        })
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
        workstreams,
        agents,
        projects,
        view_config: _,
        machine_seed,
        agent_counter,
        workspace_counter,
    } = message
    else {
        anyhow::bail!("rho daemon did not send ready message");
    };
    if events
        .unbounded_send(ConnEvent::Ready {
            workstreams,
            agents,
            projects,
            machine_seed,
            agent_counter,
            workspace_counter,
        })
        .is_err()
    {
        return Ok(());
    }

    write_frame(&mut stream, &ClientMessage::GitTransportRegister).await?;

    let agent_stream_task = agent_connection.map(|connection| {
        let events = events.clone();
        tokio::spawn(run_agent_streams(connection, events))
    });
    let git_transport_limit = Arc::new(tokio::sync::Semaphore::new(1));
    let git_requests = Arc::new(Mutex::new(
        HashMap::<u64, tokio::sync::watch::Sender<bool>>::new(),
    ));

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
                workstreams,
                agents,
                projects,
                view_config: _,
                machine_seed,
                agent_counter,
                workspace_counter,
            } => Some(ConnEvent::Ready {
                workstreams,
                agents,
                projects,
                machine_seed,
                agent_counter,
                workspace_counter,
            }),
            ServerMessage::WorkstreamCreated { workstream } => {
                Some(ConnEvent::WorkstreamCreated(workstream))
            }
            ServerMessage::AgentCreated {
                agent_id,
                workstream,
            } => Some(ConnEvent::AgentCreated {
                agent_id,
                workstream,
            }),
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
            ServerMessage::GitTransportRequested {
                request_id,
                provider_id,
                request,
            } => {
                let events = events.clone();
                let provider_dialer = dialer.lock().unwrap().clone();
                let git_transport_limit = git_transport_limit.clone();
                let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
                git_requests.lock().unwrap().insert(request_id, done_tx);
                let git_requests = git_requests.clone();
                tokio::spawn(async move {
                    let result = async {
                        let _permit = tokio::select! {
                            permit = git_transport_limit.acquire_owned() => {
                                permit.context("Git transport provider closed")?
                            }
                            _ = done_rx.changed() => return Ok(()),
                        };
                        let provider_dialer =
                            provider_dialer.context("not connected to rho daemon")?;
                        run_git_transport_provider(
                            provider_dialer,
                            request_id,
                            provider_id,
                            request,
                            events.clone(),
                        )
                        .await
                    }
                    .await;
                    if let Err(error) = result {
                        let _ = events.unbounded_send(ConnEvent::ServerError(format!(
                            "SSH Git transport failed: {error:#}"
                        )));
                    }
                    git_requests.lock().unwrap().remove(&request_id);
                });
                None
            }
            ServerMessage::GitTransportDone { request_id } => {
                if let Some(done) = git_requests.lock().unwrap().remove(&request_id) {
                    done.send_replace(true);
                }
                Some(ConnEvent::GitTransportDone { request_id })
            }
            ServerMessage::Pong
            | ServerMessage::LandLeaseQueued { .. }
            | ServerMessage::LandLeaseGranted { .. }
            | ServerMessage::LandStatus { .. }
            | ServerMessage::McpAgentToolResult(_)
            | ServerMessage::PlatformStatus { .. }
            | ServerMessage::IrohApproved { .. }
            | ServerMessage::IrohRevoked { .. }
            | ServerMessage::PrCommandResult { .. }
            | ServerMessage::GitTransportReady
            | ServerMessage::GitTransportRefused { .. }
            | ServerMessage::GitTransportPolicy { .. } => None,
            // Zed channel and terminal handshake replies belong to dedicated
            // streams, never the UI session.
            ServerMessage::ChannelOpened { .. }
            | ServerMessage::ChannelClosed { .. }
            | ServerMessage::TerminalOpened { .. }
            | ServerMessage::TerminalRefused { .. }
            | ServerMessage::TerminalList { .. }
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

async fn request_git_approval(
    events: &futures_mpsc::UnboundedSender<ConnEvent>,
    request_id: u64,
    prompt: String,
) -> anyhow::Result<GitApprovalDecision> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    events
        .unbounded_send(ConnEvent::GitTransportApproval {
            request_id,
            prompt,
            response: tx,
        })
        .map_err(|_| anyhow::anyhow!("GUI closed before Git transport approval"))?;
    rx.await
        .context("GUI closed the Git transport approval prompt")
}

async fn run_git_transport_provider(
    dialer: ChannelDialer,
    request_id: u64,
    provider_id: u64,
    request: GitTransportRequest,
    events: futures_mpsc::UnboundedSender<ConnEvent>,
) -> anyhow::Result<()> {
    validate_git_transport_request(&request)?;
    let action = match request.service {
        GitService::UploadPack => "fetch from",
        GitService::ReceivePack => "push to",
    };
    let decision = request_git_approval(
        &events,
        request_id,
        format!(
            "SSH Git: {action} {}@{}:{} / {}? (type allow):",
            display_field(&request.user),
            display_field(&request.host),
            request.port,
            display_field(&request.repository),
        ),
    )
    .await?;
    match decision {
        GitApprovalDecision::Allow => {}
        GitApprovalDecision::Deny => {
            report_git_transport_decision(dialer, request_id, provider_id, false).await?;
            return Ok(());
        }
        GitApprovalDecision::Done => return Ok(()),
    }

    let Some(mut stream) = open_git_transport_provider(dialer, request_id, provider_id).await?
    else {
        return Ok(());
    };
    let remote_command = format!(
        "{} '{}'",
        match request.service {
            GitService::UploadPack => "git-upload-pack",
            GitService::ReceivePack => "git-receive-pack",
        },
        request.repository
    );
    let mut child = tokio::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes"])
        .args(["-o", "ClearAllForwardings=yes"])
        .args(["-o", "PermitLocalCommand=no"])
        .args(["-o", "ControlMaster=no"])
        .arg("-p")
        .arg(request.port.to_string())
        .arg("-l")
        .arg(&request.user)
        .arg("--")
        .arg(&request.host)
        .arg(remote_command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("launch local OpenSSH")?;
    let mut ssh_stdin = child.stdin.take().context("OpenSSH stdin unavailable")?;
    let mut ssh_stdout = child.stdout.take().context("OpenSSH stdout unavailable")?;
    let ssh_stderr = child.stderr.take().context("OpenSSH stderr unavailable")?;
    let (mut transport_read, mut transport_write) = tokio::io::split(&mut stream);

    let input = async {
        if request.service == GitService::ReceivePack {
            let (prefix, commands) = read_receive_pack_prefix(&mut transport_read).await?;
            let needs_ref_approval = commands
                .updates
                .iter()
                .any(|update| !update.reference.starts_with("refs/heads/rho/"));
            if needs_ref_approval {
                let mut details = format!(
                    "Approve refs for {} / {} (type allow):",
                    display_field(&request.host),
                    display_field(&request.repository)
                );
                for update in &commands.updates {
                    use std::fmt::Write as _;
                    let zero = update.old.bytes().all(|byte| byte == b'0');
                    let deletion = update.new.bytes().all(|byte| byte == b'0');
                    let action = if zero {
                        "create"
                    } else if deletion {
                        "delete"
                    } else {
                        "update"
                    };
                    let _ = write!(
                        details,
                        " {action} {} {}..{};",
                        display_field(&update.reference),
                        update.old,
                        update.new
                    );
                }
                anyhow::ensure!(
                    request_git_approval(&events, request_id, details).await?
                        == GitApprovalDecision::Allow,
                    "Git ref update denied by the GUI user"
                );
            }
            ssh_stdin.write_all(&prefix).await?;
        }
        tokio::io::copy(&mut transport_read, &mut ssh_stdin).await?;
        ssh_stdin.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    };
    let output = async {
        tokio::io::copy(&mut ssh_stdout, &mut transport_write).await?;
        transport_write.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    };
    let stderr = async {
        const MAX_STDERR: usize = 64 * 1024;
        let mut bytes = Vec::new();
        ssh_stderr
            .take(MAX_STDERR as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > MAX_STDERR {
            bytes.truncate(MAX_STDERR);
            bytes.extend_from_slice(b"\n[SSH stderr truncated]");
        }
        Ok::<Vec<u8>, anyhow::Error>(bytes)
    };
    let ((), (), stderr) = tokio::try_join!(input, output, stderr)?;
    let status = child.wait().await.context("wait for local OpenSSH")?;
    anyhow::ensure!(
        status.success(),
        "OpenSSH exited with {status}: {}",
        String::from_utf8_lossy(&stderr)
    );
    Ok(())
}

async fn report_git_transport_decision(
    dialer: ChannelDialer,
    request_id: u64,
    provider_id: u64,
    approved: bool,
) -> anyhow::Result<()> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(
        &mut stream,
        &ClientMessage::GitTransportProvide {
            request_id,
            provider_id,
            approved,
        },
    )
    .await?;
    let _: ServerMessage = read_frame(&mut stream).await?;
    Ok(())
}

async fn open_git_transport_provider(
    dialer: ChannelDialer,
    request_id: u64,
    provider_id: u64,
) -> anyhow::Result<Option<Box<dyn AsyncStream>>> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(
        &mut stream,
        &ClientMessage::GitTransportProvide {
            request_id,
            provider_id,
            approved: true,
        },
    )
    .await?;
    match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::GitTransportReady => Ok(Some(stream)),
        ServerMessage::GitTransportDone { .. } => Ok(None),
        ServerMessage::GitTransportRefused { reason } => anyhow::bail!(reason),
        _ => anyhow::bail!("unexpected Git transport provider handshake reply"),
    }
}

async fn read_receive_pack_prefix<R>(
    reader: &mut R,
) -> anyhow::Result<(Vec<u8>, octo_types::ReceivePackCommands)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut prefix = Vec::new();
    loop {
        let mut chunk = [0_u8; 8192];
        let read = reader.read(&mut chunk).await?;
        anyhow::ensure!(read != 0, "truncated Git receive-pack command list");
        prefix.extend_from_slice(&chunk[..read]);
        match octo_types::parse_receive_pack_commands(&prefix) {
            Ok(Some(commands)) => return Ok((prefix, commands)),
            Ok(None) => {}
            Err(error) => anyhow::bail!(error),
        }
    }
}

fn validate_git_transport_request(request: &GitTransportRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        !request.host.is_empty()
            && request.host.len() <= 255
            && !request.host.starts_with('-')
            && request.host.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':')
            }),
        "invalid SSH Git host"
    );
    anyhow::ensure!(request.port != 0, "invalid SSH Git port");
    anyhow::ensure!(
        !request.user.is_empty()
            && request.user.len() <= 64
            && !request.user.starts_with('-')
            && request
                .user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid SSH Git user"
    );
    anyhow::ensure!(
        !request.repository.is_empty()
            && request.repository.len() <= 1024
            && !request.repository.starts_with(['-', '/'])
            && !request.repository.contains("//")
            && request.repository.split('/').all(|component| {
                !component.is_empty()
                    && component != "."
                    && component != ".."
                    && !component.starts_with('-')
                    && component.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }),
        "invalid SSH Git repository path"
    );
    Ok(())
}

fn display_field(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
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
                .qlog_from_env("rho-gui")
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

    use rho_ui_proto::{GitService, GitTransportRequest};

    use super::{AgentFrameAllocationBudget, display_field, validate_git_transport_request};

    #[test]
    fn client_rejects_unsafe_git_transport_fields() {
        let valid = GitTransportRequest {
            host: "git.example".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "team/repo.git".to_owned(),
            service: GitService::ReceivePack,
        };
        assert!(validate_git_transport_request(&valid).is_ok());
        for repository in ["../repo", "-oProxyCommand=bad", "team//repo"] {
            let mut request = valid.clone();
            request.repository = repository.to_owned();
            assert!(validate_git_transport_request(&request).is_err());
        }
    }

    #[test]
    fn git_prompt_fields_replace_bidi_controls() {
        assert_eq!(display_field("main\u{202e}txt"), "main\u{fffd}txt");
    }

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
