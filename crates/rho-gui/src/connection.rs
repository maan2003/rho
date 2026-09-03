//! Daemon connection: an IO task on the shared tokio runtime ([`gpui_tokio`]),
//! bridged to the GUI through channels. Inbound server messages become
//! [`ConnEvent`]s on a futures channel the workspace awaits (no polling);
//! outbound commands are fire-and-forget.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use camino::Utf8PathBuf;
use futures::channel::mpsc as futures_mpsc;
use futures::{SinkExt as _, StreamExt as _};
use gpui::{App, Task};
use gpui_tokio::Tokio;
use rho_ui_proto::client::Client;
use rho_ui_proto::remote::AgentRemoteFrame;
use rho_ui_proto::{
    AgentId, ClientMessage, GitService, GitTransportRequest, ServerMessage, UiAgentSummary,
    UiProject, WorkspaceInfo, read_frame, write_frame,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const INITIAL_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
const MAX_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

fn next_reconnect_delay(delay: std::time::Duration) -> std::time::Duration {
    (delay * 2).min(MAX_RECONNECT_DELAY)
}

fn shell_request_id(message: &ClientMessage) -> Option<u64> {
    match message {
        ClientMessage::ShellStart { request_id, .. }
        | ClientMessage::ShellList { request_id, .. }
        | ClientMessage::ShellClose { request_id, .. } => Some(*request_id),
        _ => None,
    }
}

use crate::registry::HostId;
use crate::workspace::AttachTarget;

/// Owns the transport pumps for a dedicated stream. Dropping it cancels both
/// directions on every target.
pub type ChannelTask = rho_rpc::ChannelTask;

/// A connection event tagged with the daemon it came from. Every attached
/// host feeds the same channel, so the workspace handles one ordered stream
/// rather than polling several.
pub struct HostEvent {
    pub host: HostId,
    pub event: ConnEvent,
}

/// One host's end of the shared event channel. Every IO task holds a clone
/// and stamps its own [`HostId`] on whatever it sends.
#[derive(Clone)]
pub(crate) struct EventSink {
    host: HostId,
    events: futures_mpsc::UnboundedSender<HostEvent>,
}

impl EventSink {
    /// Mirrors [`futures_mpsc::UnboundedSender::unbounded_send`]; the error
    /// carries nothing, since a closed GUI channel means the same thing
    /// whatever the event was.
    pub(crate) fn unbounded_send(&self, event: ConnEvent) -> Result<(), ()> {
        self.events
            .unbounded_send(HostEvent {
                host: self.host,
                event,
            })
            .map_err(|_| ())
    }
}

pub enum ConnEvent {
    DeskSynced {
        node_namespace: u16,
        delta: rho_desk::cells::Snapshot,
        texts: Vec<rho_desk::NodeTextSnapshot>,
    },
    DeskMutationAccepted {
        stamp: rho_desk::cells::Stamp,
    },
    DeskMutationRejected {
        stamp: rho_desk::cells::Stamp,
        reason: String,
    },
    DeskCellsAvailable {
        frontier: rho_desk::cells::Version,
    },
    DeskTextApplied {
        node_id: rho_desk::NodeId,
        operation: rho_desk::TextOperation,
    },
    DeskResyncRequired,
    DeskBindingResult {
        request_id: u64,
        node_id: Option<rho_desk::NodeId>,
        error: Option<String>,
    },
    Ready {
        agents: Vec<UiAgentSummary>,
        iris_agent: Option<AgentId>,
        projects: Vec<UiProject>,
        auth: rho_ui_proto::AuthState,
        machine_seed: u64,
        agent_counter: u64,
    },
    AuthState(rho_ui_proto::AuthState),
    AgentCreated {
        agent_id: AgentId,
    },
    AgentSubscribed(AgentId),
    AgentUnloaded {
        agent_id: AgentId,
        reason: rho_ui_proto::AgentUnloadReason,
    },
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
        facts: rho_ui_proto::UiAgentFacts,
    },
    AgentTurnReport {
        agent_id: AgentId,
        report: rho_ui_proto::UiTurnReport,
    },
    ChatGptUsage {
        used_percent: f64,
        reset_at_unix: i64,
    },
    QuotaUsage(Vec<rho_ui_proto::QuotaSummary>),
    QuotaHistory(Vec<rho_ui_proto::QuotaSeries>),
    GlobalUsage(Vec<rho_ui_proto::AgentUsageSeries>),
    AgentCostDistribution(Vec<rho_ui_proto::AgentCostSeries>),
    ServerError(String),
    Recovering(std::time::Duration),
    Recovered,
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

/// One workspace file channel. Dropping the owner cancels the transport and
/// tears down its daemon-side watcher.
pub struct WorkspaceChannel {
    pub outgoing: futures_mpsc::Sender<rho_ui_proto::WorkspaceClientFrame>,
    pub incoming: futures_mpsc::Receiver<anyhow::Result<rho_ui_proto::WorkspaceServerFrame>>,
    pub transport: ChannelTask,
}

/// How to dial an extra workspace-file stream to the daemon: locally a
/// second Unix connection, remotely another bi-stream on the already
/// authenticated iroh connection. Set by the IO task once connected.
pub(crate) type ChannelDialer = rho_rpc::Dialer;

async fn dial_channel(
    dialer: ChannelDialer,
    workspace: WorkspaceInfo,
) -> anyhow::Result<WorkspaceChannel> {
    let mut stream = dialer.open(None).await?;
    write_frame(&mut stream, &ClientMessage::ChannelOpen { workspace }).await?;
    let reply: ServerMessage = read_frame(&mut stream).await?;
    match reply {
        ServerMessage::ChannelOpened => {}
        ServerMessage::ChannelClosed { reason } => {
            anyhow::bail!("daemon refused workspace file channel: {reason}")
        }
        _ => anyhow::bail!("unexpected reply to ChannelOpen"),
    }

    let channel = stream.into_channel(rho_rpc::ChannelConfig {
        tx_limit: rho_ui_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
        rx_limit: rho_ui_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
        tx_capacity: 16,
        rx_capacity: 32,
    });
    let (outgoing, incoming, transport) = channel.into_parts();
    Ok(WorkspaceChannel {
        outgoing,
        incoming,
        transport,
    })
}

/// One attached terminal: a dedicated stream carrying [`rho_ui_proto::term`]
/// frames after the handshake. Dropping the owner cancels the attachment; the
/// terminal keeps running in the daemon.
pub struct TerminalChannel {
    pub terminal_id: u64,
    pub frames: futures_mpsc::Receiver<anyhow::Result<rho_ui_proto::term::TermServerFrame>>,
    pub input: futures_mpsc::Sender<rho_ui_proto::term::TermClientFrame>,
    pub transport: rho_rpc::ChannelTask,
}

/// One attachment to an agent's daemon-owned Comint-style shell. Dropping
/// `input` detaches this GUI but does not stop the shell process.
pub struct ShellChannel {
    pub frames: futures_mpsc::Receiver<rho_ui_proto::shell::ShellServerFrame>,
    pub submit: tokio::sync::mpsc::Sender<ShellSubmission>,
    pub control: tokio::sync::mpsc::Sender<rho_ui_proto::shell::ShellClientFrame>,
}

pub struct ShellSubmission {
    pub command: String,
    pub accepted: tokio::sync::oneshot::Sender<u64>,
}

pub(crate) async fn dial_realtime(
    dialer: ChannelDialer,
    offer_sdp: String,
) -> anyhow::Result<crate::realtime_client::RealtimeChannel> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(&mut stream, &ClientMessage::RealtimeOpen { offer_sdp }).await?;
    let answer_sdp = match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::RealtimeOpened { answer_sdp } => answer_sdp,
        ServerMessage::RealtimeRefused { reason } => anyhow::bail!("{reason}"),
        _ => anyhow::bail!("unexpected reply to RealtimeOpen"),
    };
    let channel = stream.into_channel(rho_rpc::ChannelConfig {
        tx_limit: rho_ui_proto::MAX_FRAME_LEN,
        rx_limit: rho_ui_proto::MAX_FRAME_LEN,
        tx_capacity: 32,
        rx_capacity: 32,
    });
    let (requests, replies, transport) = channel.into_parts();
    Ok(crate::realtime_client::RealtimeChannel {
        answer_sdp,
        requests,
        replies,
        _transport: transport,
    })
}

enum ShellControlReply {
    Started,
    List(Vec<rho_ui_proto::shell::ShellInfo>),
    Closed,
    Failed(String),
}

struct ShellControlRequests {
    next: u64,
    pending: HashMap<u64, tokio::sync::oneshot::Sender<ShellControlReply>>,
}

impl Default for ShellControlRequests {
    fn default() -> Self {
        Self {
            next: 1,
            pending: HashMap::new(),
        }
    }
}

async fn shell_control_request(
    commands: &futures_mpsc::UnboundedSender<ClientMessage>,
    requests: &Arc<Mutex<ShellControlRequests>>,
    make_message: impl FnOnce(u64) -> ClientMessage,
) -> anyhow::Result<ShellControlReply> {
    let (request_id, receiver) = {
        let mut requests = requests.lock().unwrap();
        let request_id = requests.next;
        requests.next = requests
            .next
            .checked_add(1)
            .context("shell request ids exhausted")?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        requests.pending.insert(request_id, sender);
        (request_id, receiver)
    };
    if commands.unbounded_send(make_message(request_id)).is_err() {
        requests.lock().unwrap().pending.remove(&request_id);
        anyhow::bail!("daemon control connection closed");
    }
    receiver
        .await
        .context("shell lifecycle request was dropped")
}

async fn dial_stream(dialer: ChannelDialer) -> anyhow::Result<rho_rpc::Stream> {
    // Interactive streams outrank the control session (priority 1).
    dialer.open(Some(50)).await
}

async fn dial_diff_snapshot(
    dialer: ChannelDialer,
    workspace: WorkspaceInfo,
    known_commit_id: Option<String>,
    include_paths: Vec<Utf8PathBuf>,
) -> anyhow::Result<Option<rho_ui_proto::WorkspaceDiffSnapshot>> {
    let mut stream = dial_bulk_stream(dialer).await?;
    write_frame(
        &mut stream,
        &ClientMessage::DiffSnapshot {
            workspace,
            known_commit_id,
            include_paths,
        },
    )
    .await?;
    match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::DiffSnapshot { snapshot } => Ok(Some(snapshot)),
        ServerMessage::DiffUnchanged { .. } => Ok(None),
        ServerMessage::DiffRefused { reason } => anyhow::bail!("{reason}"),
        _ => anyhow::bail!("unexpected reply to DiffSnapshot"),
    }
}

async fn dial_diff_base_contents(
    dialer: ChannelDialer,
    workspace: WorkspaceInfo,
    operation_id: String,
    commit_id: String,
    paths: Vec<Utf8PathBuf>,
) -> anyhow::Result<Vec<rho_ui_proto::WorkspaceDiffBaseContent>> {
    let mut stream = dial_bulk_stream(dialer).await?;
    write_frame(
        &mut stream,
        &ClientMessage::DiffBaseContents {
            workspace,
            operation_id,
            commit_id,
            paths,
        },
    )
    .await?;
    match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::DiffBaseContents { contents } => Ok(contents),
        ServerMessage::DiffRefused { reason } => anyhow::bail!("{reason}"),
        _ => anyhow::bail!("unexpected reply to DiffBaseContents"),
    }
}

async fn dial_visualization(
    dialer: ChannelDialer,
    id: String,
) -> anyhow::Result<VisualizationArtifact> {
    let mut stream = dial_bulk_stream(dialer).await?;
    write_frame(
        &mut stream,
        &ClientMessage::VisualizationGet { id: id.clone() },
    )
    .await?;
    match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::VisualizationContent {
            id: response_id,
            mime_type,
            content,
        } if response_id == id => Ok(VisualizationArtifact { mime_type, content }),
        ServerMessage::VisualizationRefused { reason } => anyhow::bail!(reason),
        _ => anyhow::bail!("unexpected reply to VisualizationGet"),
    }
}

async fn dial_gui_telemetry(dialer: ChannelDialer, snapshot: Vec<u8>) -> anyhow::Result<String> {
    anyhow::ensure!(
        snapshot.len() <= rho_ui_proto::MAX_GUI_TELEMETRY_BYTES,
        "GUI telemetry snapshot is too large"
    );
    let mut stream = dial_bulk_stream(dialer).await?;
    write_frame(&mut stream, &ClientMessage::GuiTelemetryUpload { snapshot }).await?;
    match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::GuiTelemetryStored { path } => Ok(path),
        ServerMessage::GuiTelemetryRefused { reason } => anyhow::bail!(reason),
        _ => anyhow::bail!("unexpected reply to GuiTelemetryUpload"),
    }
}

/// Opens a low-priority one-shot/bulk stream. Unlike terminal streams this
/// deliberately keeps iroh's default priority below interactive traffic.
async fn dial_bulk_stream(dialer: ChannelDialer) -> anyhow::Result<rho_rpc::Stream> {
    dialer.open(None).await
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

    let channel = stream.into_channel(rho_rpc::ChannelConfig {
        tx_limit: rho_ui_proto::MAX_FRAME_LEN,
        rx_limit: rho_ui_proto::MAX_FRAME_LEN,
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

async fn dial_shell(dialer: ChannelDialer, agent: String) -> anyhow::Result<ShellChannel> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(&mut stream, &ClientMessage::ShellAttach { agent }).await?;
    match read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::ShellOpened => {}
        ServerMessage::ShellAttachRefused { reason } => anyhow::bail!("{reason}"),
        _ => anyhow::bail!("unexpected reply on shell stream"),
    }

    let (mut reader, mut writer) = tokio::io::split(stream);
    let (mut frames_tx, frames_rx) = futures_mpsc::channel(32);
    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::channel::<ShellSubmission>(8);
    let (control_tx, mut control_rx) =
        tokio::sync::mpsc::channel::<rho_ui_proto::shell::ShellClientFrame>(8);
    let pending = Arc::new(Mutex::new(
        HashMap::<u64, tokio::sync::oneshot::Sender<u64>>::new(),
    ));
    let reader_pending = Arc::clone(&pending);
    tokio::spawn(async move {
        while let Ok(frame) =
            read_frame::<_, rho_ui_proto::shell::ShellServerFrame>(&mut reader).await
        {
            match frame {
                rho_ui_proto::shell::ShellServerFrame::Accepted {
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
                        &rho_ui_proto::shell::ShellClientFrame::Submit {
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

pub struct Connection {
    commands: futures_mpsc::UnboundedSender<ClientMessage>,
    iroh: bool,
    /// `None` until the IO task connects; channels cannot open earlier.
    dialer: Arc<Mutex<Option<ChannelDialer>>>,
    shell_requests: Arc<Mutex<ShellControlRequests>>,
    /// Dropping this aborts the IO task, tearing the connection down with the
    /// workspace.
    _io_task: Task<Result<(), gpui_tokio::JoinError>>,
    #[cfg(test)]
    sent: Arc<Mutex<Vec<ClientMessage>>>,
}

pub struct VisualizationArtifact {
    pub mime_type: String,
    pub content: Vec<u8>,
}

#[derive(Clone)]
pub struct VisualizationClient {
    dialer: Arc<Mutex<Option<ChannelDialer>>>,
}

impl VisualizationClient {
    /// A client with no daemon behind it, for an agent whose host has been
    /// detached: its retained transcript still renders, and asking for an
    /// artifact reports the same "not connected" as a dropped connection.
    pub fn detached() -> Self {
        Self {
            dialer: Arc::new(Mutex::new(None)),
        }
    }

    pub fn get(&self, id: String, cx: &App) -> Task<anyhow::Result<VisualizationArtifact>> {
        let dialer = self.dialer.lock().unwrap().clone();
        let task = Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            dial_visualization(dialer, id).await
        });
        cx.spawn(async move |_| {
            task.await
                .map_err(|error| anyhow::anyhow!("visualization task failed: {error}"))?
        })
    }
}

#[derive(Clone)]
pub struct DiffClient {
    dialer: Arc<Mutex<Option<ChannelDialer>>>,
}

impl DiffClient {
    pub fn snapshot(
        &self,
        workspace: WorkspaceInfo,
        known_commit_id: Option<String>,
        include_paths: Vec<Utf8PathBuf>,
        cx: &App,
    ) -> Task<anyhow::Result<Option<rho_ui_proto::WorkspaceDiffSnapshot>>> {
        let dialer = self.dialer.lock().unwrap().clone();
        let task = Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            dial_diff_snapshot(dialer, workspace, known_commit_id, include_paths).await
        });
        cx.spawn(async move |_| {
            task.await
                .map_err(|error| anyhow::anyhow!("diff snapshot task failed: {error}"))?
        })
    }

    pub fn base_contents(
        &self,
        workspace: WorkspaceInfo,
        operation_id: String,
        commit_id: String,
        paths: Vec<Utf8PathBuf>,
        cx: &App,
    ) -> Task<anyhow::Result<Vec<rho_ui_proto::WorkspaceDiffBaseContent>>> {
        let dialer = self.dialer.lock().unwrap().clone();
        let task = Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            dial_diff_base_contents(dialer, workspace, operation_id, commit_id, paths).await
        });
        cx.spawn(async move |_| {
            task.await
                .map_err(|error| anyhow::anyhow!("diff contents task failed: {error}"))?
        })
    }
}

impl Connection {
    pub fn upload_gui_telemetry_task(
        &self,
        snapshot: Vec<u8>,
        cx: &App,
    ) -> Task<anyhow::Result<String>> {
        let dialer = self.dialer.lock().unwrap().clone();
        let task = Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            dial_gui_telemetry(dialer, snapshot).await
        });
        cx.spawn(async move |_| {
            task.await
                .map_err(|error| anyhow::anyhow!("GUI telemetry task failed: {error}"))?
        })
    }

    /// Target-neutral GPUI task API used by portable terminal surfaces.
    pub fn open_terminal_task(
        &self,
        agent: String,
        new: bool,
        cols: u16,
        rows: u16,
        cx: &App,
    ) -> Task<anyhow::Result<TerminalChannel>> {
        let dialer = self.dialer.lock().unwrap().clone();
        let task = Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            dial_terminal(dialer, agent, new, cols, rows).await
        });
        cx.spawn(async move |_| {
            task.await
                .map_err(|error| anyhow::anyhow!("terminal task failed: {error}"))?
        })
    }

    /// Target-neutral GPUI task API used by portable shell surfaces.
    pub fn open_shell_task(&self, agent: String, cx: &App) -> Task<anyhow::Result<ShellChannel>> {
        let dialer = self.dialer.lock().unwrap().clone();
        let commands = self.commands.clone();
        let requests = Arc::clone(&self.shell_requests);
        let task = Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            let reply = shell_control_request(&commands, &requests, |request_id| {
                ClientMessage::ShellList {
                    request_id,
                    agent: Some(agent.clone()),
                }
            })
            .await?;
            let running = match reply {
                ShellControlReply::List(shells) => !shells.is_empty(),
                ShellControlReply::Failed(reason) => anyhow::bail!(reason),
                _ => anyhow::bail!("unexpected shell list reply"),
            };
            if !running {
                match shell_control_request(&commands, &requests, |request_id| {
                    ClientMessage::ShellStart {
                        request_id,
                        agent: agent.clone(),
                    }
                })
                .await?
                {
                    ShellControlReply::Started => {}
                    ShellControlReply::Failed(reason) => anyhow::bail!(reason),
                    _ => anyhow::bail!("unexpected shell start reply"),
                }
            }
            dial_shell(dialer, agent).await
        });
        cx.spawn(async move |_| {
            task.await
                .map_err(|error| anyhow::anyhow!("shell task failed: {error}"))?
        })
    }

    pub fn close_shell_task(&self, agent: String, cx: &App) -> Task<anyhow::Result<()>> {
        let commands = self.commands.clone();
        let requests = Arc::clone(&self.shell_requests);
        cx.spawn(async move |_| {
            match shell_control_request(&commands, &requests, |request_id| {
                ClientMessage::ShellClose { request_id, agent }
            })
            .await?
            {
                ShellControlReply::Closed => Ok(()),
                ShellControlReply::Failed(reason) => anyhow::bail!(reason),
                _ => anyhow::bail!("unexpected shell close reply"),
            }
        })
    }

    pub fn visualization_client(&self) -> VisualizationClient {
        VisualizationClient {
            dialer: self.dialer.clone(),
        }
    }

    pub fn diff_client(&self) -> DiffClient {
        DiffClient {
            dialer: self.dialer.clone(),
        }
    }
    pub fn send(&self, message: ClientMessage) {
        #[cfg(test)]
        self.sent.lock().unwrap().push(message.clone());
        let _ = self.commands.unbounded_send(message);
    }

    #[cfg(test)]
    pub(crate) fn take_sent_for_test(&self) -> Vec<ClientMessage> {
        std::mem::take(&mut *self.sent.lock().unwrap())
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

    /// Starts the selected agent's shell when absent, otherwise attaches.
    pub fn open_shell(
        &self,
        agent: String,
        cx: &App,
    ) -> Task<Result<anyhow::Result<ShellChannel>, gpui_tokio::JoinError>> {
        let dialer = self.dialer.lock().unwrap().clone();
        let commands = self.commands.clone();
        let requests = Arc::clone(&self.shell_requests);
        Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            let reply = shell_control_request(&commands, &requests, |request_id| {
                ClientMessage::ShellList {
                    request_id,
                    agent: Some(agent.clone()),
                }
            })
            .await?;
            let running = match reply {
                ShellControlReply::List(shells) => !shells.is_empty(),
                ShellControlReply::Failed(reason) => anyhow::bail!("{reason}"),
                _ => anyhow::bail!("unexpected shell list reply"),
            };
            if !running {
                let reply = shell_control_request(&commands, &requests, |request_id| {
                    ClientMessage::ShellStart {
                        request_id,
                        agent: agent.clone(),
                    }
                })
                .await?;
                match reply {
                    ShellControlReply::Started => {}
                    ShellControlReply::Failed(reason) => anyhow::bail!("{reason}"),
                    _ => anyhow::bail!("unexpected shell start reply"),
                }
            }
            dial_shell(dialer, agent).await
        })
    }

    /// Gracefully closes the selected agent's persistent shell.
    pub fn close_shell(
        &self,
        agent: String,
        cx: &App,
    ) -> Task<Result<anyhow::Result<()>, gpui_tokio::JoinError>> {
        let commands = self.commands.clone();
        let requests = Arc::clone(&self.shell_requests);
        Tokio::spawn(cx, async move {
            let reply = shell_control_request(&commands, &requests, |request_id| {
                ClientMessage::ShellClose { request_id, agent }
            })
            .await?;
            match reply {
                ShellControlReply::Closed => Ok(()),
                ShellControlReply::Failed(reason) => anyhow::bail!("{reason}"),
                _ => anyhow::bail!("unexpected shell close reply"),
            }
        })
    }

    /// Dials a dedicated workspace file stream and runs
    /// the handshake.
    pub fn open_channel(
        &self,
        workspace: WorkspaceInfo,
        cx: &App,
    ) -> Task<anyhow::Result<WorkspaceChannel>> {
        let dialer = self.dialer.lock().unwrap().clone();
        let task = Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            dial_channel(dialer, workspace).await
        });
        cx.spawn(async move |_| {
            task.await
                .map_err(|error| anyhow::anyhow!("workspace channel task failed: {error}"))?
        })
    }

    pub fn start_native_realtime(
        &self,
        stop: tokio::sync::oneshot::Receiver<()>,
        input_muted: tokio::sync::watch::Receiver<bool>,
        cx: &App,
    ) -> Task<Result<anyhow::Result<()>, gpui_tokio::JoinError>> {
        let dialer = self.dialer.lock().unwrap().clone();
        Tokio::spawn(cx, async move {
            let dialer = dialer.context("not connected to rho-daemon")?;
            crate::realtime_client::run_native(dialer, stop, input_muted).await
        })
    }
}

/// Attaches one daemon. Its events join `events`, tagged with `host`, so
/// several daemons feed the workspace through a single ordered stream.
pub fn spawn(
    host: HostId,
    target: AttachTarget,
    events: futures_mpsc::UnboundedSender<HostEvent>,
    cx: &App,
) -> Connection {
    let iroh = matches!(&target, AttachTarget::Iroh { .. });
    let event_tx = EventSink { host, events };
    let (command_tx, command_rx) = futures_mpsc::unbounded();
    let command_rx = Arc::new(tokio::sync::Mutex::new(command_rx));
    let pending_command = Arc::new(Mutex::new(None));
    let dialer = Arc::new(Mutex::new(None));
    let shell_requests = Arc::new(Mutex::new(ShellControlRequests::default()));
    let io_task = if cfg!(test) {
        Tokio::spawn(cx, async {})
    } else {
        Tokio::spawn(
            cx,
            supervise(
                target,
                event_tx,
                command_rx,
                pending_command,
                dialer.clone(),
                Arc::clone(&shell_requests),
            ),
        )
    };
    Connection {
        commands: command_tx,
        iroh,
        dialer,
        shell_requests,
        _io_task: io_task,
        #[cfg(test)]
        sent: Arc::new(Mutex::new(Vec::new())),
    }
}

async fn supervise(
    target: AttachTarget,
    events: EventSink,
    commands: Arc<tokio::sync::Mutex<futures_mpsc::UnboundedReceiver<ClientMessage>>>,
    pending_command: Arc<Mutex<Option<ClientMessage>>>,
    dialer: Arc<Mutex<Option<ChannelDialer>>>,
    shell_requests: Arc<Mutex<ShellControlRequests>>,
) {
    let mut delay = INITIAL_RECONNECT_DELAY;
    let mut reconnecting = false;
    loop {
        let mut connected = false;
        let result = run(
            target.clone(),
            &events,
            Arc::clone(&commands),
            Arc::clone(&pending_command),
            &dialer,
            Arc::clone(&shell_requests),
            &mut connected,
        )
        .await;
        *dialer.lock().unwrap() = None;
        if events.events.is_closed() {
            break;
        }
        if connected {
            delay = INITIAL_RECONNECT_DELAY;
            reconnecting = false;
        }
        let reason = result
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_else(|| "daemon connection closed".to_owned());
        if (!reconnecting
            && events
                .unbounded_send(ConnEvent::Disconnected(reason))
                .is_err())
            || events.unbounded_send(ConnEvent::Recovering(delay)).is_err()
        {
            break;
        }
        tokio::time::sleep(delay).await;
        delay = next_reconnect_delay(delay);
        reconnecting = true;
    }
}

async fn run_control_writer<W>(
    mut writer: W,
    commands: Arc<tokio::sync::Mutex<futures_mpsc::UnboundedReceiver<ClientMessage>>>,
    pending_command: Arc<Mutex<Option<ClientMessage>>>,
    attempted_shell_requests: Arc<Mutex<HashSet<u64>>>,
    events: Option<EventSink>,
    shell_requests: Option<Arc<Mutex<ShellControlRequests>>>,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut usage_refresh = tokio::time::interval(std::time::Duration::from_secs(10 * 60));
    usage_refresh.tick().await;
    loop {
        let message = if let Some(message) = pending_command.lock().unwrap().clone() {
            message
        } else {
            tokio::select! {
                message = async { commands.lock().await.next().await } => {
                    let Some(message) = message else { return Ok(()) };
                    *pending_command.lock().unwrap() = Some(message.clone());
                    message
                }
                _ = usage_refresh.tick() => {
                    write_frame(&mut writer, &ClientMessage::ChatGptUsage).await?;
                    continue;
                }
            }
        };
        if let Err(error) = validate_control_message(&message) {
            pending_command.lock().unwrap().take();
            let reason = format!("command rejected before sending: {error:#}");
            if let Some(request_id) = shell_request_id(&message)
                && let Some(requests) = &shell_requests
                && let Some(response) = requests.lock().unwrap().pending.remove(&request_id)
            {
                let _ = response.send(ShellControlReply::Failed(reason.clone()));
            }
            if let Some(events) = &events {
                let _ = events.unbounded_send(ConnEvent::ServerError(reason));
            }
            continue;
        }
        if let Err(error) = write_frame(&mut writer, &message).await {
            if !replay_safe(&message) {
                pending_command.lock().unwrap().take();
                let reason = format!("command outcome unknown after disconnect: {error:#}");
                if let Some(request_id) = shell_request_id(&message)
                    && let Some(requests) = &shell_requests
                    && let Some(response) = requests.lock().unwrap().pending.remove(&request_id)
                {
                    let _ = response.send(ShellControlReply::Failed(reason.clone()));
                }
                if let Some(events) = &events {
                    let _ = events.unbounded_send(ConnEvent::ServerError(reason));
                }
            }
            return Err(error);
        }
        pending_command.lock().unwrap().take();
        if let Some(request_id) = shell_request_id(&message) {
            attempted_shell_requests.lock().unwrap().insert(request_id);
        }
    }
}

fn replay_safe(message: &ClientMessage) -> bool {
    matches!(
        message,
        ClientMessage::Ping
            | ClientMessage::Subscribe
            | ClientMessage::SubscribeAgent { .. }
            | ClientMessage::AgentStreamFocus { .. }
            | ClientMessage::GitTransportRegister
            | ClientMessage::ShellList { .. }
            | ClientMessage::ChatGptUsage
            | ClientMessage::QuotaHistory
            | ClientMessage::AgentUsage { .. }
            | ClientMessage::GlobalUsage { .. }
            | ClientMessage::AgentCostDistribution { .. }
            | ClientMessage::SubscribeAgents { .. }
            | ClientMessage::UnsubscribeAgents { .. }
    )
}

fn validate_control_message(message: &ClientMessage) -> anyhow::Result<()> {
    let payload = senax_encoder::pack(message).context("pack protocol frame")?;
    anyhow::ensure!(
        payload.len() <= rho_ui_proto::MAX_FRAME_LEN,
        "protocol frame length {} exceeds {}",
        payload.len(),
        rho_ui_proto::MAX_FRAME_LEN
    );
    Ok(())
}

fn fail_attempted_shell_requests(
    shell_requests: &Mutex<ShellControlRequests>,
    attempted: &Mutex<HashSet<u64>>,
    reason: &str,
) {
    let attempted = std::mem::take(&mut *attempted.lock().unwrap());
    let mut requests = shell_requests.lock().unwrap();
    for request_id in attempted {
        if let Some(response) = requests.pending.remove(&request_id) {
            let _ = response.send(ShellControlReply::Failed(reason.to_owned()));
        }
    }
}

async fn abort_tasks<T: 'static>(tasks: &mut tokio::task::JoinSet<T>) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn run(
    target: AttachTarget,
    events: &EventSink,
    commands: Arc<tokio::sync::Mutex<futures_mpsc::UnboundedReceiver<ClientMessage>>>,
    pending_command: Arc<Mutex<Option<ClientMessage>>>,
    dialer: &Mutex<Option<ChannelDialer>>,
    shell_requests: Arc<Mutex<ShellControlRequests>>,
    connected: &mut bool,
) -> anyhow::Result<()> {
    let (mut stream, agent_connection, _endpoint) = match target {
        AttachTarget::Unix(socket_path) => {
            let client = Client::connect(&socket_path)
                .await
                .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
            *dialer.lock().unwrap() = Some(ChannelDialer::Unix(socket_path));
            (client.into_stream(), None, None)
        }
        AttachTarget::Iroh {
            endpoint_id,
            ssh_destination,
            remote_rho,
        } => {
            let (stream, connection, endpoint) =
                connect_iroh(endpoint_id, &ssh_destination, &remote_rho).await?;
            *dialer.lock().unwrap() = Some(ChannelDialer::Iroh(connection.clone()));
            (stream, Some(connection), Some(endpoint))
        }
    };
    write_frame(&mut stream, &ClientMessage::Subscribe).await?;
    let message: ServerMessage = read_frame(&mut stream).await?;
    let ServerMessage::Ready {
        agents,
        iris_agent,
        projects,
        auth,
        view_config: _,
        machine_seed,
        agent_counter,
    } = message
    else {
        anyhow::bail!("rho daemon did not send ready message");
    };
    if events
        .unbounded_send(ConnEvent::Ready {
            agents,
            iris_agent,
            projects,
            auth,
            machine_seed,
            agent_counter,
        })
        .is_err()
    {
        return Ok(());
    }
    *connected = true;
    if events.unbounded_send(ConnEvent::Recovered).is_err() {
        return Ok(());
    }

    write_frame(&mut stream, &ClientMessage::ChatGptUsage).await?;

    write_frame(&mut stream, &ClientMessage::GitTransportRegister).await?;

    let health_connection = agent_connection.clone();
    let agent_stream_task = agent_connection.map(|connection| {
        let events = events.clone();
        let (shutdown, requested) = tokio::sync::oneshot::channel();
        (
            shutdown,
            tokio::spawn(run_agent_streams(connection, events, requested)),
        )
    });
    let git_transport_limit = Arc::new(tokio::sync::Semaphore::new(1));
    let git_requests = Arc::new(Mutex::new(
        HashMap::<u64, tokio::sync::watch::Sender<bool>>::new(),
    ));

    let (mut reader, writer) = tokio::io::split(stream);
    let health_task = health_connection.map(|connection| {
        let events = events.clone();
        tokio::spawn(async move {
            const RECOVERY_NOTICE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            let mut received = connection.stats().authenticated_packets;
            let mut last_received = tokio::time::Instant::now();
            let mut recovering = false;
            loop {
                interval.tick().await;
                let current = connection.stats().authenticated_packets;
                if current != received {
                    received = current;
                    last_received = tokio::time::Instant::now();
                }
                let elapsed = last_received.elapsed();
                if elapsed >= RECOVERY_NOTICE_AFTER {
                    recovering = true;
                    if events
                        .unbounded_send(ConnEvent::Recovering(elapsed))
                        .is_err()
                    {
                        break;
                    }
                } else if recovering {
                    recovering = false;
                    if events.unbounded_send(ConnEvent::Recovered).is_err() {
                        break;
                    }
                }
            }
        })
    });
    let attempted_shell_requests = Arc::new(Mutex::new(HashSet::new()));
    let mut writer_task = tokio::spawn(run_control_writer(
        writer,
        commands,
        pending_command,
        Arc::clone(&attempted_shell_requests),
        Some(events.clone()),
        Some(Arc::clone(&shell_requests)),
    ));
    let mut writer_finished = false;
    let mut git_provider_tasks = tokio::task::JoinSet::new();

    let read_error = loop {
        let message: ServerMessage = tokio::select! {
            result = &mut writer_task => {
                writer_finished = true;
                break Some(match result {
                    Ok(Ok(())) => anyhow::anyhow!("daemon control writer stopped"),
                    Ok(Err(error)) => error.context("write daemon control frame"),
                    Err(error) => anyhow::anyhow!("daemon control writer task failed: {error}"),
                });
            }
            result = read_frame(&mut reader) => match result {
                Ok(message) => message,
                Err(error) => break Some(error.into()),
            },
        };
        let event = match message {
            ServerMessage::DeskSynced {
                node_namespace,
                delta,
                texts,
            } => Some(ConnEvent::DeskSynced {
                node_namespace,
                delta,
                texts,
            }),
            ServerMessage::DeskMutationAccepted { stamp } => {
                Some(ConnEvent::DeskMutationAccepted { stamp })
            }
            ServerMessage::DeskMutationRejected { stamp, reason } => {
                Some(ConnEvent::DeskMutationRejected { stamp, reason })
            }
            ServerMessage::DeskCellsAvailable { frontier } => {
                Some(ConnEvent::DeskCellsAvailable { frontier })
            }
            ServerMessage::DeskTextApplied {
                node_id,
                operation,
                transaction: _,
            } => Some(ConnEvent::DeskTextApplied { node_id, operation }),
            ServerMessage::DeskResyncRequired => Some(ConnEvent::DeskResyncRequired),
            // The old tree stream is dead weight until 5pha's side drops
            // it from the protocol; nothing here listens.
            ServerMessage::DeskTreeSnapshot { .. }
            | ServerMessage::DeskTreeApplied { .. }
            | ServerMessage::DeskNodeTextApplied { .. }
            | ServerMessage::DeskTreeBatchApplied { .. }
            | ServerMessage::DeskTreeBatchRejected { .. }
            | ServerMessage::DeskTreeResyncRequired => None,
            ServerMessage::DeskBindingResult {
                request_id,
                node_id,
                error,
            } => Some(ConnEvent::DeskBindingResult {
                request_id,
                node_id,
                error,
            }),
            // Read-only CLI reply; the GUI subscribes instead.
            ServerMessage::DeskTreeDocument { .. } => None,
            ServerMessage::Ready {
                agents,
                iris_agent,
                projects,
                auth,
                view_config: _,
                machine_seed,
                agent_counter,
            } => Some(ConnEvent::Ready {
                agents,
                iris_agent,
                projects,
                auth,
                machine_seed,
                agent_counter,
            }),
            ServerMessage::AuthState { auth } => Some(ConnEvent::AuthState(auth)),
            ServerMessage::AgentCreated { agent_id } => Some(ConnEvent::AgentCreated { agent_id }),
            ServerMessage::AgentSubscribed { agent_id } => {
                Some(ConnEvent::AgentSubscribed(agent_id))
            }
            ServerMessage::AgentUnloaded { agent_id, reason } => {
                Some(ConnEvent::AgentUnloaded { agent_id, reason })
            }
            ServerMessage::Agent { agent_id, frame } => Some(ConnEvent::Frame {
                agent_id,
                frame,
                allocation: None,
            }),
            ServerMessage::TurnCancelled { .. } => Some(ConnEvent::TurnCancelled),
            ServerMessage::AgentAttention {
                agent_id,
                attention,
                facts,
            } => Some(ConnEvent::AgentAttention {
                agent_id,
                attention,
                facts,
            }),
            ServerMessage::AgentTurnReport { agent_id, report } => {
                Some(ConnEvent::AgentTurnReport { agent_id, report })
            }
            ServerMessage::Error { message } => Some(ConnEvent::ServerError(message)),
            ServerMessage::ChatGptUsage {
                used_percent,
                reset_at_unix,
            } => Some(ConnEvent::ChatGptUsage {
                used_percent,
                reset_at_unix,
            }),
            ServerMessage::QuotaUsage { summaries } => Some(ConnEvent::QuotaUsage(summaries)),
            ServerMessage::QuotaHistory { series } => Some(ConnEvent::QuotaHistory(series)),
            ServerMessage::AgentUsage { .. } => None,
            ServerMessage::GlobalUsage { series } => Some(ConnEvent::GlobalUsage(series)),
            ServerMessage::AgentCostDistribution { series } => {
                Some(ConnEvent::AgentCostDistribution(series))
            }
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
                git_provider_tasks.spawn(async move {
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
                    let _ = events.unbounded_send(ConnEvent::GitTransportDone { request_id });
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
            ServerMessage::ShellStarted { request_id } => {
                if let Some(request) = shell_requests.lock().unwrap().pending.remove(&request_id) {
                    let _ = request.send(ShellControlReply::Started);
                }
                None
            }
            ServerMessage::ShellList { request_id, shells } => {
                if let Some(request) = shell_requests.lock().unwrap().pending.remove(&request_id) {
                    let _ = request.send(ShellControlReply::List(shells));
                }
                None
            }
            ServerMessage::ShellClosed { request_id } => {
                if let Some(request) = shell_requests.lock().unwrap().pending.remove(&request_id) {
                    let _ = request.send(ShellControlReply::Closed);
                }
                None
            }
            ServerMessage::ShellRequestFailed { request_id, reason } => {
                if let Some(request) = shell_requests.lock().unwrap().pending.remove(&request_id) {
                    let _ = request.send(ShellControlReply::Failed(reason));
                }
                None
            }
            ServerMessage::Pong
            | ServerMessage::VisualizationRecorded { .. }
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
            // Dedicated-stream handshake replies never belong to the UI session.
            ServerMessage::ChannelOpened
            | ServerMessage::ChannelClosed { .. }
            | ServerMessage::TerminalOpened { .. }
            | ServerMessage::TerminalRefused { .. }
            | ServerMessage::TerminalList { .. }
            | ServerMessage::ShellOpened
            | ServerMessage::ShellAttachRefused { .. }
            | ServerMessage::DiffSnapshot { .. }
            | ServerMessage::DiffBaseContents { .. }
            | ServerMessage::DiffUnchanged { .. }
            | ServerMessage::DiffRefused { .. }
            | ServerMessage::GuiTelemetryStored { .. }
            | ServerMessage::GuiTelemetryRefused { .. }
            | ServerMessage::RealtimeOpened { .. }
            | ServerMessage::RealtimeRefused { .. }
            | ServerMessage::AgentStreamOpened { .. }
            | ServerMessage::VisualizationContent { .. }
            | ServerMessage::VisualizationRefused { .. } => None,
        };
        if let Some(event) = event
            && events.unbounded_send(event).is_err()
        {
            break None;
        }
    };
    if !writer_finished {
        writer_task.abort();
        let _ = writer_task.await;
    }
    if let Some(task) = health_task {
        task.abort();
        let _ = task.await;
    }
    if let Some((shutdown, task)) = agent_stream_task {
        let _ = shutdown.send(());
        let _ = task.await;
    }
    abort_tasks(&mut git_provider_tasks).await;
    let failure = read_error
        .as_ref()
        .map(|error| format!("daemon connection lost: {error:#}"))
        .unwrap_or_else(|| "daemon connection closed".to_owned());
    fail_attempted_shell_requests(&shell_requests, &attempted_shell_requests, &failure);
    git_requests.lock().unwrap().clear();
    match read_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn request_git_approval(
    events: &EventSink,
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
    tokio::time::timeout(std::time::Duration::from_secs(60), rx)
        .await
        .context("Git transport approval timed out after 60 seconds")?
        .context("GUI closed the Git transport approval prompt")
}

async fn run_git_transport_provider(
    dialer: ChannelDialer,
    request_id: u64,
    provider_id: u64,
    request: GitTransportRequest,
    events: EventSink,
) -> anyhow::Result<()> {
    if let Err(error) = validate_git_transport_request(&request) {
        report_git_transport_decision(dialer, request_id, provider_id, false).await?;
        return Err(error);
    }
    let prompt = match request.service {
        GitService::UploadPack => format!(
            "Fetch via SSH from {}:{}/{}? [shift-Y/N]",
            display_field(&request.host),
            request.port,
            display_field(&request.repository),
        ),
        GitService::ReceivePack => git_push_prompt(
            &request,
            request
                .planned_refs
                .as_deref()
                .context("SSH Git push is missing its destination ref plan")?,
        ),
    };
    match request_git_approval(&events, request_id, prompt).await? {
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
            copy_planned_receive_pack(
                &mut transport_read,
                &mut ssh_stdin,
                request
                    .planned_refs
                    .as_deref()
                    .context("SSH Git push is missing its destination ref plan")?,
            )
            .await?;
        } else {
            rho_rpc::copy_flush(&mut transport_read, &mut ssh_stdin).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let output = async {
        rho_rpc::copy_flush(&mut ssh_stdout, &mut transport_write).await?;
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

async fn copy_planned_receive_pack<R, W>(
    reader: &mut R,
    writer: &mut W,
    planned_refs: &[String],
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (prefix, commands) = read_receive_pack_prefix(reader).await?;
    anyhow::ensure!(
        receive_pack_refs_match(planned_refs, &commands),
        "Git receive-pack destination refs differ from the approved plan"
    );
    writer.write_all(&prefix).await?;
    rho_rpc::copy_flush(reader, writer).await?;
    Ok(())
}

fn receive_pack_refs_match(
    planned_refs: &[String],
    commands: &octo_types::ReceivePackCommands,
) -> bool {
    if planned_refs.len() != commands.updates.len() {
        return false;
    }
    let planned = planned_refs
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let actual = commands
        .updates
        .iter()
        .map(|update| update.reference.as_str())
        .collect::<BTreeSet<_>>();
    planned.len() == planned_refs.len() && planned == actual
}

fn git_push_prompt(request: &GitTransportRequest, planned_refs: &[String]) -> String {
    let destination = format!(
        "ssh://{}:{}/{}",
        display_field(&request.host),
        request.port,
        display_field(&request.repository)
    );
    let mut prompt = format!("Push via SSH to {destination}:");
    for reference in planned_refs {
        use std::fmt::Write as _;
        let reference = reference
            .strip_prefix("refs/heads/")
            .map(|name| format!("branch {name}"))
            .or_else(|| {
                reference
                    .strip_prefix("refs/tags/")
                    .map(|name| format!("tag {name}"))
            })
            .unwrap_or_else(|| reference.clone());
        let _ = write!(prompt, "\n  {reference}");
    }
    prompt.push_str("\nApprove? [shift-Y/N]");
    prompt
}

async fn report_git_transport_decision(
    dialer: ChannelDialer,
    request_id: u64,
    provider_id: u64,
    claim: bool,
) -> anyhow::Result<()> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(
        &mut stream,
        &ClientMessage::GitTransportProvide {
            request_id,
            provider_id,
            claim,
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
) -> anyhow::Result<Option<rho_rpc::Stream>> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(
        &mut stream,
        &ClientMessage::GitTransportProvide {
            request_id,
            provider_id,
            claim: true,
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
        matches!(request.host.as_str(), "github.com" | "git.sr.ht"),
        "invalid SSH Git host"
    );
    anyhow::ensure!(request.port != 0, "invalid SSH Git port");
    anyhow::ensure!(request.user == "git", "invalid SSH Git user");
    anyhow::ensure!(
        octo_types::valid_ssh_repository(&request.host, &request.repository),
        "invalid SSH Git repository path"
    );
    match (&request.service, &request.planned_refs) {
        (GitService::UploadPack, None) => {}
        (GitService::ReceivePack, Some(planned_refs)) => {
            anyhow::ensure!(
                !planned_refs.is_empty(),
                "SSH Git push has an empty ref plan"
            );
            anyhow::ensure!(
                planned_refs.iter().map(String::len).sum::<usize>()
                    <= octo_types::MAX_RECEIVE_PACK_COMMAND_BYTES,
                "SSH Git push ref plan is too large"
            );
            let mut unique = HashSet::new();
            anyhow::ensure!(
                planned_refs.iter().all(|reference| {
                    octo_types::valid_git_ref(reference) && unique.insert(reference)
                }),
                "SSH Git push ref plan is invalid"
            );
        }
        _ => anyhow::bail!("SSH Git transport has an invalid ref plan"),
    }
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
    events: EventSink,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    const AGENT_FRAME_ALLOCATION_BUDGET: usize = 128 * 1024 * 1024;
    let mut streams = tokio::task::JoinSet::new();
    let allocation_budget = Arc::new(AgentFrameAllocationBudget::new(
        AGENT_FRAME_ALLOCATION_BUDGET,
    ));
    let generations = Arc::new(tokio::sync::Mutex::new(
        crate::registry::session::AgentStreamGenerations::default(),
    ));
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = connection.accept_uni() => {
                let Ok(recv) = accepted else { break };
                let mut recv = rho_rpc::Reader::new(recv);
                let events = events.clone();
                let allocation_budget = allocation_budget.clone();
                let generations = generations.clone();
                streams.spawn(async move {
                    let (header, header_allocation) =
                        read_agent_stream_message(&mut recv, &allocation_budget).await?;
                    let ServerMessage::AgentStreamOpened { agent_id } = header
                    else {
                        anyhow::bail!("invalid agent stream header");
                    };
                    drop(header_allocation);
                    let generation = generations.lock().await.open(agent_id);
                    loop {
                        let (message, allocation) = match read_agent_stream_message(
                            &mut recv,
                            &allocation_budget,
                        )
                        .await
                        {
                            Ok(message) => message,
                            Err(error)
                                if error
                                    .downcast_ref::<std::io::Error>()
                                    .is_some_and(|error| {
                                        error.kind() == std::io::ErrorKind::UnexpectedEof
                                    }) =>
                            {
                                return Ok(())
                            }
                            Err(error) => return Err(error),
                        };
                        let ServerMessage::Agent {
                            agent_id: frame_agent_id,
                            frame,
                        } = message
                        else {
                            anyhow::bail!("invalid message on agent stream");
                        };
                        anyhow::ensure!(frame_agent_id == agent_id, "agent stream id changed");
                        let generations = generations.lock().await;
                        if !generations.is_current(agent_id, generation) {
                            continue;
                        }
                        // Keep generation validation and enqueue atomic with
                        // respect to a replacement stream registering itself.
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
                        drop(generations);
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
    abort_tasks(&mut streams).await;
}

async fn read_agent_stream_message(
    recv: &mut rho_rpc::Reader,
    allocation_budget: &Arc<AgentFrameAllocationBudget>,
) -> anyhow::Result<(ServerMessage, AgentFrameAllocation)> {
    let (message, allocation, _) =
        rho_rpc::read_frame_allocated(recv, rho_ui_proto::MAX_FRAME_LEN, |len| {
            allocation_budget.reserve(len)
        })
        .await?;
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

pub struct AgentFrameAllocation {
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

/// The client's iroh identity, bound once and shared by every attached
/// daemon. One identity means one key for the user to recognize across
/// hosts, and each host still enrolls it separately over its own SSH login.
static CLIENT_ENDPOINT: tokio::sync::OnceCell<iroh::Endpoint> = tokio::sync::OnceCell::const_new();

async fn client_endpoint() -> anyhow::Result<iroh::Endpoint> {
    CLIENT_ENDPOINT
        .get_or_try_init(rho_rpc::bind_ephemeral_iroh_client)
        .await
        .cloned()
}

async fn connect_iroh(
    daemon_id: iroh::EndpointId,
    ssh_destination: &str,
    remote_rho: &str,
) -> anyhow::Result<(rho_rpc::Stream, iroh::endpoint::Connection, iroh::Endpoint)> {
    // The native client's identity intentionally lives only as long as this
    // process. Each daemon can trust it in memory via an existing SSH login.
    let endpoint = client_endpoint().await?;
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
        rho_rpc::authenticate_iroh_client(&connection, endpoint.id()).await?
            == rho_iroh_auth::ClientAuthResult::Approved,
        "daemon did not approve SSH-trusted iroh client"
    );
    let (send, recv) = connection.open_bi().await.context("open iroh UI stream")?;
    send.set_priority(1)
        .context("set iroh UI control stream priority")?;
    let stream = rho_rpc::Stream::new(recv, send);
    Ok((stream, connection, endpoint))
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
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use octo_types::{ReceivePackCommands, RefUpdate};
    use rho_ui_proto::{ClientMessage, GitService, GitTransportRequest};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{
        AgentFrameAllocationBudget, INITIAL_RECONNECT_DELAY, MAX_RECONNECT_DELAY,
        ShellControlReply, ShellControlRequests, abort_tasks, copy_planned_receive_pack,
        display_field, fail_attempted_shell_requests, git_push_prompt, next_reconnect_delay,
        read_frame, receive_pack_refs_match, run_control_writer, validate_git_transport_request,
    };

    #[test]
    fn reconnect_backoff_caps_at_ten_seconds() {
        let mut delay = INITIAL_RECONNECT_DELAY;
        for _ in 0..10 {
            delay = next_reconnect_delay(delay);
        }
        assert_eq!(delay, MAX_RECONNECT_DELAY);
        assert_eq!(next_reconnect_delay(delay), MAX_RECONNECT_DELAY);
    }

    #[test]
    fn writer_failure_returns_to_the_connection_supervisor() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let (writer, peer) = tokio::io::duplex(64);
                drop(peer);
                let (commands_tx, commands_rx) = futures::channel::mpsc::unbounded();
                commands_tx.unbounded_send(ClientMessage::Ping).unwrap();
                let result = run_control_writer(
                    writer,
                    Arc::new(tokio::sync::Mutex::new(commands_rx)),
                    Arc::new(Mutex::new(None)),
                    Arc::new(Mutex::new(HashSet::new())),
                    None,
                    None,
                )
                .await;
                assert!(result.is_err());
            });
    }

    #[test]
    fn failed_write_replays_the_in_flight_command() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let (commands_tx, commands_rx) = futures::channel::mpsc::unbounded();
                commands_tx.unbounded_send(ClientMessage::Ping).unwrap();
                let commands = Arc::new(tokio::sync::Mutex::new(commands_rx));
                let pending = Arc::new(Mutex::new(None));
                let attempted = Arc::new(Mutex::new(HashSet::new()));
                let (failed_writer, peer) = tokio::io::duplex(64);
                drop(peer);
                assert!(
                    run_control_writer(
                        failed_writer,
                        Arc::clone(&commands),
                        Arc::clone(&pending),
                        Arc::clone(&attempted),
                        None,
                        None,
                    )
                    .await
                    .is_err()
                );
                assert!(matches!(
                    pending.lock().unwrap().as_ref(),
                    Some(ClientMessage::Ping)
                ));

                let (writer, mut reader) = tokio::io::duplex(1024);
                let replay = tokio::spawn(run_control_writer(
                    writer, commands, pending, attempted, None, None,
                ));
                let message: ClientMessage = read_frame(&mut reader).await.unwrap();
                assert!(matches!(message, ClientMessage::Ping));
                replay.abort();
                let _ = replay.await;
            });
    }

    #[test]
    fn permanently_invalid_command_does_not_poison_the_queue() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let (commands_tx, commands_rx) = futures::channel::mpsc::unbounded();
                commands_tx
                    .unbounded_send(ClientMessage::ViewConfigSet {
                        data: vec![0; rho_ui_proto::MAX_FRAME_LEN + 1],
                    })
                    .unwrap();
                commands_tx.unbounded_send(ClientMessage::Ping).unwrap();
                let (writer, mut reader) = tokio::io::duplex(1024);
                let task = tokio::spawn(run_control_writer(
                    writer,
                    Arc::new(tokio::sync::Mutex::new(commands_rx)),
                    Arc::new(Mutex::new(None)),
                    Arc::new(Mutex::new(HashSet::new())),
                    None,
                    None,
                ));

                let message: ClientMessage = read_frame(&mut reader).await.unwrap();
                assert!(matches!(message, ClientMessage::Ping));
                task.abort();
                let _ = task.await;
            });
    }

    #[test]
    fn ambiguous_non_idempotent_write_is_not_replayed() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let (commands_tx, commands_rx) = futures::channel::mpsc::unbounded();
                commands_tx
                    .unbounded_send(ClientMessage::ShellStart {
                        request_id: 1,
                        agent: "test".to_owned(),
                    })
                    .unwrap();
                let pending = Arc::new(Mutex::new(None));
                let (writer, peer) = tokio::io::duplex(64);
                drop(peer);
                assert!(
                    run_control_writer(
                        writer,
                        Arc::new(tokio::sync::Mutex::new(commands_rx)),
                        Arc::clone(&pending),
                        Arc::new(Mutex::new(HashSet::new())),
                        None,
                        None,
                    )
                    .await
                    .is_err()
                );
                assert!(pending.lock().unwrap().is_none());
            });
    }

    #[test]
    fn teardown_fails_only_requests_attempted_on_the_dead_session() {
        let mut requests = ShellControlRequests::default();
        let (attempted_tx, attempted_rx) = tokio::sync::oneshot::channel();
        let (queued_tx, _queued_rx) = tokio::sync::oneshot::channel();
        requests.pending.insert(1, attempted_tx);
        requests.pending.insert(2, queued_tx);
        let requests = Mutex::new(requests);
        let attempted = Mutex::new(HashSet::from([1]));

        fail_attempted_shell_requests(&requests, &attempted, "lost");

        assert!(matches!(
            attempted_rx.blocking_recv().unwrap(),
            ShellControlReply::Failed(reason) if reason == "lost"
        ));
        assert!(requests.lock().unwrap().pending.contains_key(&2));
    }

    #[test]
    fn session_teardown_aborts_and_awaits_child_tasks() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
                struct Dropped(Arc<std::sync::atomic::AtomicBool>);
                impl Drop for Dropped {
                    fn drop(&mut self) {
                        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                let guard = Dropped(Arc::clone(&dropped));
                let mut tasks = tokio::task::JoinSet::new();
                tasks.spawn(async move {
                    let _guard = guard;
                    futures::future::pending::<()>().await;
                });
                tokio::task::yield_now().await;

                abort_tasks(&mut tasks).await;

                assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
                assert!(tasks.is_empty());
            });
    }

    fn receive_pack_input(reference: &str, old: &str, new: &str, tail: &[u8]) -> Vec<u8> {
        let command = format!("{old} {new} {reference}\0report-status\n");
        let mut input = format!("{:04x}{command}", command.len() + 4).into_bytes();
        input.extend_from_slice(b"0000");
        input.extend_from_slice(tail);
        input
    }

    #[test]
    fn client_rejects_unsafe_git_transport_fields() {
        let valid = GitTransportRequest {
            host: "github.com".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "team/repo".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec!["refs/heads/main".to_owned()]),
        };
        assert!(validate_git_transport_request(&valid).is_ok());
        let mut sourcehut = valid.clone();
        sourcehut.host = "git.sr.ht".to_owned();
        sourcehut.repository = "~alice/project".to_owned();
        assert!(validate_git_transport_request(&sourcehut).is_ok());
        for host in ["github.com", "git.sr.ht"] {
            let mut request = valid.clone();
            request.host = host.to_owned();
            request.user = "root".to_owned();
            assert!(validate_git_transport_request(&request).is_err());
        }
        let mut unknown_host = valid.clone();
        unknown_host.host = "git.example".to_owned();
        assert!(validate_git_transport_request(&unknown_host).is_err());
        for repository in ["team/repo-name", "team/repo.git"] {
            let mut request = valid.clone();
            request.repository = repository.to_owned();
            assert!(validate_git_transport_request(&request).is_ok());
        }
        for repository in ["../repo", "team//repo"] {
            let mut request = valid.clone();
            request.repository = repository.to_owned();
            assert!(validate_git_transport_request(&request).is_err());
        }
        for planned_refs in [
            None,
            Some(Vec::new()),
            Some(vec![
                "refs/heads/main".to_owned(),
                "refs/heads/main".to_owned(),
            ]),
            Some(vec!["refs/heads/../main".to_owned()]),
        ] {
            let mut request = valid.clone();
            request.planned_refs = planned_refs;
            assert!(validate_git_transport_request(&request).is_err());
        }
        let mut fetch = valid;
        fetch.service = GitService::UploadPack;
        assert!(validate_git_transport_request(&fetch).is_err());
        fetch.planned_refs = None;
        assert!(validate_git_transport_request(&fetch).is_ok());
    }

    #[test]
    fn git_prompt_fields_replace_bidi_controls() {
        assert_eq!(display_field("main\u{202e}txt"), "main\u{fffd}txt");
    }

    #[test]
    fn push_prompt_names_destination_refs() {
        let request = GitTransportRequest {
            host: "github.com".to_owned(),
            port: 2222,
            user: "git".to_owned(),
            repository: "acme/repo".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec![
                "refs/heads/main".to_owned(),
                "refs/tags/v1".to_owned(),
                "refs/heads/rho/test".to_owned(),
                "refs/notes/review".to_owned(),
            ]),
        };
        let prompt = git_push_prompt(&request, request.planned_refs.as_deref().unwrap());
        assert!(prompt.contains("ssh://github.com:2222/acme/repo"));
        assert!(!prompt.contains("git@"));
        assert!(prompt.contains("branch main"));
        assert!(prompt.contains("tag v1"));
        assert!(prompt.contains("branch rho/test"));
        assert!(prompt.contains("refs/notes/review"));
        assert!(prompt.ends_with("Approve? [shift-Y/N]"));
    }

    #[test]
    fn receive_pack_plan_comparison_ignores_order_and_object_ids() {
        let commands = ReceivePackCommands {
            end: 0,
            updates: vec![
                RefUpdate {
                    old: "1".repeat(40),
                    new: "2".repeat(40),
                    reference: "refs/tags/v1".to_owned(),
                },
                RefUpdate {
                    old: "3".repeat(40),
                    new: "4".repeat(40),
                    reference: "refs/heads/main".to_owned(),
                },
            ],
        };
        assert!(receive_pack_refs_match(
            &["refs/heads/main".to_owned(), "refs/tags/v1".to_owned()],
            &commands
        ));
        assert!(!receive_pack_refs_match(
            &["refs/heads/main".to_owned()],
            &commands
        ));
    }

    #[test]
    fn matching_receive_pack_plan_forwards_exact_bytes() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let old = "1".repeat(40);
                let new = "2".repeat(40);
                let input = receive_pack_input("refs/heads/main", &old, &new, b"PACK tail");
                let (mut client, mut transport) = tokio::io::duplex(4096);
                let (mut ssh, mut remote) = tokio::io::duplex(4096);
                client.write_all(&input).await.unwrap();
                client.shutdown().await.unwrap();
                let copy = tokio::spawn(async move {
                    copy_planned_receive_pack(
                        &mut transport,
                        &mut ssh,
                        &["refs/heads/main".to_owned()],
                    )
                    .await
                });
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                copy.await.unwrap().unwrap();
                assert_eq!(received, input);
            });
    }

    #[test]
    fn mismatched_receive_pack_plan_writes_nothing() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let input = receive_pack_input(
                    "refs/heads/rho/test",
                    &"1".repeat(40),
                    &"2".repeat(40),
                    b"PACK tail",
                );
                let (mut client, mut transport) = tokio::io::duplex(4096);
                let (mut ssh, mut remote) = tokio::io::duplex(4096);
                client.write_all(&input).await.unwrap();
                client.shutdown().await.unwrap();
                let result = copy_planned_receive_pack(
                    &mut transport,
                    &mut ssh,
                    &["refs/heads/main".to_owned()],
                )
                .await;
                assert!(result.is_err());
                drop(ssh);
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                assert!(received.is_empty());
            });
    }

    #[test]
    fn malformed_receive_pack_writes_nothing() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (mut client, mut transport) = tokio::io::duplex(64);
                let (mut ssh, mut remote) = tokio::io::duplex(64);
                client.write_all(b"0003").await.unwrap();
                client.shutdown().await.unwrap();
                let result = copy_planned_receive_pack(
                    &mut transport,
                    &mut ssh,
                    &["refs/heads/main".to_owned()],
                )
                .await;
                assert!(result.is_err());
                drop(ssh);
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                assert!(received.is_empty());
            });
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
