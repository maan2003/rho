//! Direct browser iroh connection to the daemon's `rho/ui/3` protocol.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::{SinkExt as _, StreamExt as _};
use gpui::App;
use hkdf::Hkdf;
use iroh::EndpointId;
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use rho_registry::session::AgentStreamGenerations;
use rho_ui_proto::realtime::{RealtimeClientFrame, RealtimeServerFrame};
use rho_ui_proto::remote::AgentRemoteFrame;
use rho_ui_proto::{
    AgentId, ClientMessage, ServerMessage, UiAgentSummary, UiProject, UiWorkstream, WorkspaceInfo,
};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use zeroize::Zeroize as _;

#[derive(Clone, Debug)]
pub enum AttachTarget {
    Iroh(EndpointId),
}

impl AttachTarget {
    pub fn iroh(endpoint: impl AsRef<str>) -> anyhow::Result<Self> {
        Ok(Self::Iroh(
            EndpointId::from_str(endpoint.as_ref().trim())
                .map_err(|error| anyhow::anyhow!("invalid daemon endpoint id: {error}"))?,
        ))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Iroh(endpoint) => endpoint.to_string(),
        }
    }

    fn endpoint(&self) -> EndpointId {
        match self {
            Self::Iroh(endpoint) => *endpoint,
        }
    }
}

pub struct HostEvent {
    pub host: crate::registry::HostId,
    pub event: ConnEvent,
}

pub enum ConnEvent {
    Ready {
        workstreams: Vec<UiWorkstream>,
        agents: Vec<UiAgentSummary>,
        projects: Vec<UiProject>,
        machine_seed: u64,
        agent_counter: u64,
    },
    WorkstreamCreated(UiWorkstream),
    AgentCreated {
        agent_id: AgentId,
        workstream: rho_ui_proto::WorkstreamId,
    },
    AgentSubscribed(AgentId),
    AgentUnloaded {
        agent_id: AgentId,
        reason: rho_ui_proto::AgentUnloadReason,
    },
    Frame {
        agent_id: AgentId,
        frame: AgentRemoteFrame,
        allocation: Option<AgentFrameAllocation>,
    },
    TurnCancelled,
    AgentAttention {
        agent_id: AgentId,
        attention: rho_ui_proto::UiAttention,
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
    ServerError(String),
    Recovering(std::time::Duration),
    Recovered,
    Disconnected(String),
    AuthorizationRequired,
    EnrollmentRequired(String),
}

/// Keeps the connection-wide decompressed-frame budget reserved until the
/// workspace applies or discards the event.
pub struct AgentFrameAllocation {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Clone)]
struct HostSink {
    host: crate::registry::HostId,
    events: UnboundedSender<HostEvent>,
}

impl HostSink {
    fn send(&self, event: ConnEvent) {
        let _ = self.events.unbounded_send(HostEvent {
            host: self.host,
            event,
        });
    }
}

const CREDENTIAL_KEY: &str = "rho-gui-web-passkey-credential";
const LEGACY_SECRET_KEY: &str = "rho-gui-web-secret";
const DAEMON_KEY: &str = "rho-gui-web-daemon";
const DAEMONS_KEY: &str = "rho-gui-web-daemons";
const AUTHENTICATOR_KEY: &str = "rho-gui-web-authenticator";
const PRF_LABEL: &[u8] = b"rho webui iroh prf v1";
const HKDF_INFO: &[u8] = b"rho webui iroh ed25519 seed v1";
const MAX_CREDENTIAL_ID_LEN: usize = 1024;

#[derive(Clone, Debug)]
pub enum Event {
    Phase(Phase),
    Message(ServerMessage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Unlock(String),
    Connecting,
    Enroll(String),
    Online,
    Failed(String),
}

pub struct Connection {
    commands: UnboundedSender<Command>,
    receiver: Rc<RefCell<Option<UnboundedReceiver<Command>>>>,
    events: UnboundedSender<Event>,
    host_sink: Option<HostSink>,
    target: Option<AttachTarget>,
    shell_requests: Arc<Mutex<ShellControlRequests>>,
    dialer: Rc<RefCell<Option<rho_rpc::Dialer>>>,
}

enum Command {
    Control(ClientMessage),
    Realtime {
        offer_sdp: String,
        response: oneshot::Sender<anyhow::Result<crate::realtime_client::RealtimeChannel>>,
    },
}

#[derive(Clone)]
pub(crate) struct RealtimeDialer {
    commands: UnboundedSender<Command>,
}

impl RealtimeDialer {
    pub(crate) async fn open(
        &self,
        offer_sdp: String,
    ) -> anyhow::Result<crate::realtime_client::RealtimeChannel> {
        let (response, reply) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Realtime {
                offer_sdp,
                response,
            })
            .map_err(|_| anyhow::anyhow!("daemon connection closed"))?;
        reply
            .await
            .map_err(|_| anyhow::anyhow!("daemon connection closed"))?
    }
}

/// Owns both pumps of a dedicated typed stream.
pub type ChannelTask = rho_rpc::ChannelTask;

/// One workspace file channel. Dropping the owner cancels the transport and
/// its daemon-side watcher.
pub struct WorkspaceChannel {
    pub outgoing: mpsc::Sender<rho_ui_proto::WorkspaceClientFrame>,
    pub incoming: mpsc::Receiver<anyhow::Result<rho_ui_proto::WorkspaceServerFrame>>,
    pub transport: ChannelTask,
}

pub struct VisualizationArtifact {
    pub mime_type: String,
    pub content: Vec<u8>,
}

#[derive(Clone)]
pub struct VisualizationClient {
    dialer: Rc<RefCell<Option<rho_rpc::Dialer>>>,
}

impl VisualizationClient {
    pub fn detached() -> Self {
        Self {
            dialer: Rc::new(RefCell::new(None)),
        }
    }

    pub fn get(&self, id: String, cx: &App) -> gpui::Task<anyhow::Result<VisualizationArtifact>> {
        let dialer = self.dialer.borrow().clone();
        cx.spawn(async move |_| {
            let dialer = dialer.ok_or_else(|| anyhow::anyhow!("not connected to rho-daemon"))?;
            dial_visualization(dialer, id).await
        })
    }
}

#[derive(Clone)]
pub struct DiffClient {
    dialer: Rc<RefCell<Option<rho_rpc::Dialer>>>,
}

impl DiffClient {
    pub fn snapshot(
        &self,
        workspace: WorkspaceInfo,
        known_commit_id: Option<String>,
        include_paths: Vec<Utf8PathBuf>,
        cx: &App,
    ) -> gpui::Task<anyhow::Result<Option<rho_ui_proto::WorkspaceDiffSnapshot>>> {
        let dialer = self.dialer.borrow().clone();
        cx.spawn(async move |_| {
            let dialer = dialer.ok_or_else(|| anyhow::anyhow!("not connected to rho-daemon"))?;
            dial_diff_snapshot(dialer, workspace, known_commit_id, include_paths).await
        })
    }

    pub fn base_contents(
        &self,
        workspace: WorkspaceInfo,
        operation_id: String,
        commit_id: String,
        paths: Vec<Utf8PathBuf>,
        cx: &App,
    ) -> gpui::Task<anyhow::Result<Vec<rho_ui_proto::WorkspaceDiffBaseContent>>> {
        let dialer = self.dialer.borrow().clone();
        cx.spawn(async move |_| {
            let dialer = dialer.ok_or_else(|| anyhow::anyhow!("not connected to rho-daemon"))?;
            dial_diff_base_contents(dialer, workspace, operation_id, commit_id, paths).await
        })
    }
}

pub struct TerminalChannel {
    pub terminal_id: u64,
    pub frames: mpsc::Receiver<anyhow::Result<rho_ui_proto::term::TermServerFrame>>,
    pub input: mpsc::Sender<rho_ui_proto::term::TermClientFrame>,
    pub transport: ChannelTask,
}

pub struct ShellChannel {
    pub frames: mpsc::Receiver<rho_ui_proto::shell::ShellServerFrame>,
    pub submit: tokio::sync::mpsc::Sender<ShellSubmission>,
    pub control: tokio::sync::mpsc::Sender<rho_ui_proto::shell::ShellClientFrame>,
}

pub struct ShellSubmission {
    pub command: String,
    pub accepted: tokio::sync::oneshot::Sender<u64>,
}

enum ShellControlReply {
    Started,
    List(Vec<rho_ui_proto::shell::ShellInfo>),
    Closed,
    Failed(String),
}

struct ShellControlRequests {
    next: u64,
    pending: HashMap<u64, oneshot::Sender<ShellControlReply>>,
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
    commands: &UnboundedSender<Command>,
    requests: &Arc<Mutex<ShellControlRequests>>,
    make_message: impl FnOnce(u64) -> ClientMessage,
) -> anyhow::Result<ShellControlReply> {
    let (request_id, receiver) = {
        let mut requests = requests.lock().unwrap();
        let request_id = requests.next;
        requests.next = requests
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("shell request ids exhausted"))?;
        let (sender, receiver) = oneshot::channel();
        requests.pending.insert(request_id, sender);
        (request_id, receiver)
    };
    if commands
        .unbounded_send(Command::Control(make_message(request_id)))
        .is_err()
    {
        requests.lock().unwrap().pending.remove(&request_id);
        anyhow::bail!("daemon control connection closed");
    }
    receiver
        .await
        .map_err(|_| anyhow::anyhow!("shell lifecycle request was dropped"))
}

fn handle_host_message(
    sink: Option<&HostSink>,
    shell_requests: &Mutex<ShellControlRequests>,
    message: &ServerMessage,
) {
    match message {
        ServerMessage::ShellStarted { request_id } => {
            if let Some(tx) = shell_requests.lock().unwrap().pending.remove(request_id) {
                let _ = tx.send(ShellControlReply::Started);
            }
        }
        ServerMessage::ShellList { request_id, shells } => {
            if let Some(tx) = shell_requests.lock().unwrap().pending.remove(request_id) {
                let _ = tx.send(ShellControlReply::List(shells.clone()));
            }
        }
        ServerMessage::ShellClosed { request_id } => {
            if let Some(tx) = shell_requests.lock().unwrap().pending.remove(request_id) {
                let _ = tx.send(ShellControlReply::Closed);
            }
        }
        ServerMessage::ShellRequestFailed { request_id, reason } => {
            if let Some(tx) = shell_requests.lock().unwrap().pending.remove(request_id) {
                let _ = tx.send(ShellControlReply::Failed(reason.clone()));
            }
        }
        _ => {}
    }
    let Some(sink) = sink else { return };
    let event = match message {
        ServerMessage::Ready {
            workstreams,
            agents,
            projects,
            machine_seed,
            agent_counter,
            ..
        } => Some(ConnEvent::Ready {
            workstreams: workstreams.clone(),
            agents: agents.clone(),
            projects: projects.clone(),
            machine_seed: *machine_seed,
            agent_counter: *agent_counter,
        }),
        ServerMessage::WorkstreamCreated { workstream } => {
            Some(ConnEvent::WorkstreamCreated(workstream.clone()))
        }
        ServerMessage::AgentCreated {
            agent_id,
            workstream,
        } => Some(ConnEvent::AgentCreated {
            agent_id: *agent_id,
            workstream: *workstream,
        }),
        ServerMessage::AgentSubscribed { agent_id } => Some(ConnEvent::AgentSubscribed(*agent_id)),
        ServerMessage::AgentUnloaded { agent_id, reason } => Some(ConnEvent::AgentUnloaded {
            agent_id: *agent_id,
            reason: reason.clone(),
        }),
        ServerMessage::Agent { agent_id, frame } => Some(ConnEvent::Frame {
            agent_id: *agent_id,
            frame: frame.clone(),
            allocation: None,
        }),
        ServerMessage::TurnCancelled { .. } => Some(ConnEvent::TurnCancelled),
        ServerMessage::AgentAttention {
            agent_id,
            attention,
        } => Some(ConnEvent::AgentAttention {
            agent_id: *agent_id,
            attention: attention.clone(),
        }),
        ServerMessage::AgentTurnReport { agent_id, report } => Some(ConnEvent::AgentTurnReport {
            agent_id: *agent_id,
            report: report.clone(),
        }),
        ServerMessage::ChatGptUsage {
            used_percent,
            reset_at_unix,
        } => Some(ConnEvent::ChatGptUsage {
            used_percent: *used_percent,
            reset_at_unix: *reset_at_unix,
        }),
        ServerMessage::QuotaUsage { summaries } => Some(ConnEvent::QuotaUsage(summaries.clone())),
        ServerMessage::QuotaHistory { series } => Some(ConnEvent::QuotaHistory(series.clone())),
        ServerMessage::GlobalUsage { series } => Some(ConnEvent::GlobalUsage(series.clone())),
        ServerMessage::Error { message } => Some(ConnEvent::ServerError(message.clone())),
        _ => None,
    };
    if let Some(event) = event {
        sink.send(event);
    }
}

/// Creates a locked browser host. Call Connection::authorize from the
/// corresponding user gesture; hosts are never authorized in a batch.
pub fn spawn(
    host: crate::registry::HostId,
    target: AttachTarget,
    events: UnboundedSender<HostEvent>,
    _cx: &App,
) -> Connection {
    let (commands, receiver) = mpsc::unbounded();
    let (legacy, _legacy_rx) = mpsc::unbounded();
    let sink = HostSink { host, events };
    sink.send(ConnEvent::AuthorizationRequired);
    Connection {
        commands,
        receiver: Rc::new(RefCell::new(Some(receiver))),
        events: legacy,
        host_sink: Some(sink),
        target: Some(target),
        shell_requests: Arc::new(Mutex::new(ShellControlRequests::default())),
        dialer: Rc::new(RefCell::new(None)),
    }
}

async fn dial_channel(
    dialer: rho_rpc::Dialer,
    workspace: WorkspaceInfo,
) -> anyhow::Result<WorkspaceChannel> {
    let mut stream = dialer.open(None).await?;
    rho_ui_proto::write_frame(&mut stream, &ClientMessage::ChannelOpen { workspace }).await?;
    match rho_ui_proto::read_frame::<_, ServerMessage>(&mut stream).await? {
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

async fn dial_diff_snapshot(
    dialer: rho_rpc::Dialer,
    workspace: WorkspaceInfo,
    known_commit_id: Option<String>,
    include_paths: Vec<Utf8PathBuf>,
) -> anyhow::Result<Option<rho_ui_proto::WorkspaceDiffSnapshot>> {
    let mut stream = dialer.open(None).await?;
    rho_ui_proto::write_frame(
        &mut stream,
        &ClientMessage::DiffSnapshot {
            workspace,
            known_commit_id,
            include_paths,
        },
    )
    .await?;
    match rho_ui_proto::read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::DiffSnapshot { snapshot } => Ok(Some(snapshot)),
        ServerMessage::DiffUnchanged { .. } => Ok(None),
        ServerMessage::DiffRefused { reason } => anyhow::bail!(reason),
        _ => anyhow::bail!("unexpected reply to DiffSnapshot"),
    }
}

async fn dial_diff_base_contents(
    dialer: rho_rpc::Dialer,
    workspace: WorkspaceInfo,
    operation_id: String,
    commit_id: String,
    paths: Vec<Utf8PathBuf>,
) -> anyhow::Result<Vec<rho_ui_proto::WorkspaceDiffBaseContent>> {
    let mut stream = dialer.open(None).await?;
    rho_ui_proto::write_frame(
        &mut stream,
        &ClientMessage::DiffBaseContents {
            workspace,
            operation_id,
            commit_id,
            paths,
        },
    )
    .await?;
    match rho_ui_proto::read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::DiffBaseContents { contents } => Ok(contents),
        ServerMessage::DiffRefused { reason } => anyhow::bail!(reason),
        _ => anyhow::bail!("unexpected reply to DiffBaseContents"),
    }
}

async fn dial_visualization(
    dialer: rho_rpc::Dialer,
    id: String,
) -> anyhow::Result<VisualizationArtifact> {
    let mut stream = dialer.open(None).await?;
    rho_ui_proto::write_frame(
        &mut stream,
        &ClientMessage::VisualizationGet { id: id.clone() },
    )
    .await?;
    match rho_ui_proto::read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::VisualizationContent {
            id: response_id,
            mime_type,
            content,
        } if response_id == id => Ok(VisualizationArtifact { mime_type, content }),
        ServerMessage::VisualizationRefused { reason } => anyhow::bail!(reason),
        _ => anyhow::bail!("unexpected reply to VisualizationGet"),
    }
}

async fn dial_terminal(
    dialer: rho_rpc::Dialer,
    agent: String,
    new: bool,
    cols: u16,
    rows: u16,
) -> anyhow::Result<TerminalChannel> {
    let mut list = dialer.open(Some(50)).await?;
    rho_ui_proto::write_frame(
        &mut list,
        &ClientMessage::TerminalList {
            agent: Some(agent.clone()),
        },
    )
    .await?;
    let running = match rho_ui_proto::read_frame::<_, ServerMessage>(&mut list).await? {
        ServerMessage::TerminalList { terminals } => terminals,
        ServerMessage::TerminalRefused { reason } => anyhow::bail!(reason),
        _ => anyhow::bail!("unexpected reply to TerminalList"),
    };
    let (terminal_id, create) = if new {
        (
            running
                .iter()
                .map(|info| info.terminal_id.saturating_add(1))
                .max()
                .unwrap_or(0),
            true,
        )
    } else {
        running
            .first()
            .map(|info| (info.terminal_id, false))
            .unwrap_or((0, true))
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
    let mut stream = dialer.open(Some(50)).await?;
    rho_ui_proto::write_frame(&mut stream, &open).await?;
    match rho_ui_proto::read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::TerminalOpened { .. } => {}
        ServerMessage::TerminalRefused { reason } => anyhow::bail!(reason),
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

async fn dial_shell(dialer: rho_rpc::Dialer, agent: String) -> anyhow::Result<ShellChannel> {
    let mut stream = dialer.open(Some(50)).await?;
    rho_ui_proto::write_frame(&mut stream, &ClientMessage::ShellAttach { agent }).await?;
    match rho_ui_proto::read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::ShellOpened => {}
        ServerMessage::ShellAttachRefused { reason } => anyhow::bail!(reason),
        _ => anyhow::bail!("unexpected reply on shell stream"),
    }
    let (mut reader, mut writer) = stream.into_split();
    let (mut frames_tx, frames) = mpsc::channel(32);
    let (submit, mut submit_rx) = tokio::sync::mpsc::channel::<ShellSubmission>(8);
    let (control, mut control_rx) = tokio::sync::mpsc::channel(8);
    let pending = Arc::new(Mutex::new(
        HashMap::<u64, tokio::sync::oneshot::Sender<u64>>::new(),
    ));
    let reader_pending = Arc::clone(&pending);
    spawn_local(async move {
        while let Ok(frame) =
            rho_ui_proto::read_frame::<_, rho_ui_proto::shell::ShellServerFrame>(&mut reader).await
        {
            match frame {
                rho_ui_proto::shell::ShellServerFrame::Accepted {
                    submission,
                    execution,
                } => {
                    if let Some(tx) = reader_pending.lock().unwrap().remove(&submission) {
                        let _ = tx.send(execution);
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
    spawn_local(async move {
        let mut next = 1_u64;
        loop {
            let result = tokio::select! {
                biased;
                Some(frame) = control_rx.recv() => rho_ui_proto::write_frame(&mut writer, &frame).await,
                Some(submission) = submit_rx.recv() => {
                    let id = next;
                    next = next.wrapping_add(1).max(1);
                    pending.lock().unwrap().insert(id, submission.accepted);
                    rho_ui_proto::write_frame(&mut writer, &rho_ui_proto::shell::ShellClientFrame::Submit { submission: id, command: submission.command }).await
                }
                else => break,
            };
            if result.is_err() {
                break;
            }
        }
        pending.lock().unwrap().clear();
        let _ = writer.shutdown().await;
    });
    Ok(ShellChannel {
        frames,
        submit,
        control,
    })
}

impl Connection {
    pub fn visualization_client(&self) -> VisualizationClient {
        VisualizationClient {
            dialer: Rc::clone(&self.dialer),
        }
    }

    pub fn diff_client(&self) -> DiffClient {
        DiffClient {
            dialer: Rc::clone(&self.dialer),
        }
    }

    pub fn new() -> (Self, UnboundedReceiver<Event>) {
        let (commands, receiver) = mpsc::unbounded();
        let (events, event_rx) = mpsc::unbounded();
        (
            Self {
                commands,
                receiver: Rc::new(RefCell::new(Some(receiver))),
                events,
                host_sink: None,
                target: None,
                shell_requests: Arc::new(Mutex::new(ShellControlRequests::default())),
                dialer: Rc::new(RefCell::new(None)),
            },
            event_rx,
        )
    }

    pub fn send(&self, message: ClientMessage) {
        let _ = self.commands.unbounded_send(Command::Control(message));
    }

    pub fn focus_agent(&self, agent_id: Option<AgentId>) {
        self.send(ClientMessage::AgentStreamFocus { agent_id });
    }

    pub fn open_channel(
        &self,
        workspace: WorkspaceInfo,
        cx: &App,
    ) -> gpui::Task<anyhow::Result<WorkspaceChannel>> {
        let dialer = self.dialer.borrow().clone();
        cx.spawn(async move |_| {
            let dialer = dialer.ok_or_else(|| anyhow::anyhow!("not connected to rho-daemon"))?;
            dial_channel(dialer, workspace).await
        })
    }

    pub fn open_terminal_task(
        &self,
        agent: String,
        new: bool,
        cols: u16,
        rows: u16,
        cx: &App,
    ) -> gpui::Task<anyhow::Result<TerminalChannel>> {
        let dialer = self.dialer.borrow().clone();
        cx.spawn(async move |_| {
            let dialer = dialer.ok_or_else(|| anyhow::anyhow!("not connected to rho-daemon"))?;
            dial_terminal(dialer, agent, new, cols, rows).await
        })
    }

    pub fn open_shell_task(
        &self,
        agent: String,
        cx: &App,
    ) -> gpui::Task<anyhow::Result<ShellChannel>> {
        let dialer = self.dialer.borrow().clone();
        let commands = self.commands.clone();
        let requests = Arc::clone(&self.shell_requests);
        cx.spawn(async move |_| {
            let dialer = dialer.ok_or_else(|| anyhow::anyhow!("not connected to rho-daemon"))?;
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
        })
    }

    pub fn close_shell_task(&self, agent: String, cx: &App) -> gpui::Task<anyhow::Result<()>> {
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

    pub(crate) fn open_realtime_task(
        &self,
        offer_sdp: String,
        cx: &App,
    ) -> gpui::Task<anyhow::Result<crate::realtime_client::RealtimeChannel>> {
        let dialer = self.realtime_dialer();
        cx.spawn(async move |_| dialer.open(offer_sdp).await)
    }

    pub(crate) fn realtime_dialer(&self) -> RealtimeDialer {
        RealtimeDialer {
            commands: self.commands.clone(),
        }
    }

    pub fn connect(&self, daemon: String) {
        let endpoint = match EndpointId::from_str(daemon.trim()) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.emit_phase(Phase::Failed(format!(
                    "invalid daemon endpoint id: {error}"
                )));
                return;
            }
        };
        remember_daemon(&daemon);
        self.start(endpoint);
    }

    pub fn authorize(&self) {
        if let Some(target) = &self.target {
            self.start(target.endpoint());
        }
    }

    fn start(&self, daemon: EndpointId) {
        let Some(receiver) = self.receiver.borrow_mut().take() else {
            return;
        };
        let events = self.events.clone();
        let host_sink = self.host_sink.clone();
        let shell_requests = Arc::clone(&self.shell_requests);
        let dialer = Rc::clone(&self.dialer);
        self.emit_phase(Phase::Connecting);
        spawn_local(async move {
            if let Err(error) = run(
                daemon,
                receiver,
                events.clone(),
                host_sink.clone(),
                shell_requests,
                dialer,
            )
            .await
            {
                let reason = format!("{error:#}");
                let _ = events.unbounded_send(Event::Phase(Phase::Failed(reason.clone())));
                if let Some(sink) = host_sink {
                    sink.send(ConnEvent::Disconnected(reason));
                }
            }
        });
    }

    fn emit_phase(&self, phase: Phase) {
        let _ = self.events.unbounded_send(Event::Phase(phase.clone()));
        if let Some(sink) = &self.host_sink {
            match phase {
                Phase::Unlock(_) => sink.send(ConnEvent::AuthorizationRequired),
                Phase::Enroll(code) => sink.send(ConnEvent::EnrollmentRequired(code)),
                Phase::Failed(reason) => sink.send(ConnEvent::Disconnected(reason)),
                Phase::Connecting | Phase::Online => {}
            }
        }
    }
}

pub fn daemon_id_from_page() -> Option<String> {
    daemon_targets_from_page()
        .ok()?
        .into_iter()
        .next()
        .map(|(_, target)| target.describe())
}

/// Reads every repeated `daemon=` query/fragment value. A value may be an
/// endpoint id (named `daemon`, `daemon-2`, …) or `name@endpoint-id`. The
/// non-secret list is remembered in local storage; the legacy single-daemon
/// key remains a fallback for existing installations.
pub fn daemon_targets_from_page() -> anyhow::Result<Vec<(String, AttachTarget)>> {
    let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("browser window unavailable"))?;
    let storage = window
        .local_storage()
        .ok()
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("local storage unavailable"))?;
    let location = window.location();
    let mut values = [
        location.hash().ok().map(|s| (s, '#')),
        location.search().ok().map(|s| (s, '?')),
    ]
    .into_iter()
    .flatten()
    .flat_map(|(part, prefix)| {
        part.trim_start_matches(prefix)
            .split('&')
            .filter_map(|pair| pair.strip_prefix("daemon="))
            .filter(|daemon| !daemon.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    })
    .collect::<Vec<_>>();
    if values.is_empty() {
        values = storage
            .get_item(DAEMONS_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .or_else(|| {
                storage
                    .get_item(DAEMON_KEY)
                    .ok()
                    .flatten()
                    .map(|value| vec![value])
            })
            .unwrap_or_default();
    } else {
        let _ = storage.set_item(DAEMONS_KEY, &serde_json::to_string(&values)?);
        if let Some(first) = values.first() {
            let endpoint = first.rsplit_once('@').map_or(first.as_str(), |(_, id)| id);
            let _ = storage.set_item(DAEMON_KEY, endpoint);
        }
    }
    let multiple = values.len() > 1;
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let (name, endpoint) = match value.split_once('@') {
                Some((name, endpoint)) if !name.is_empty() => (name.to_owned(), endpoint),
                Some(_) => anyhow::bail!("daemon host name is empty"),
                None if multiple => (format!("daemon-{}", index + 1), value.as_str()),
                None => ("daemon".to_owned(), value.as_str()),
            };
            Ok((name, AttachTarget::iroh(endpoint)?))
        })
        .collect()
}

fn remember_daemon(daemon: &str) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(DAEMON_KEY, daemon);
    }
}

/// Progress breadcrumbs for the connect path; "Connecting…" is otherwise a
/// black box when any await in here stalls.
fn conn_log(message: &str) {
    web_sys::console::log_1(&JsValue::from_str(&format!("[rho-conn] {message}")));
}

async fn run(
    daemon: EndpointId,
    mut receiver: UnboundedReceiver<Command>,
    events: UnboundedSender<Event>,
    host_sink: Option<HostSink>,
    shell_requests: Arc<Mutex<ShellControlRequests>>,
    dialer: Rc<RefCell<Option<rho_rpc::Dialer>>>,
) -> anyhow::Result<()> {
    conn_log("unlocking passkey identity");
    let secret = passkey_secret(daemon).await?;
    conn_log("passkey identity ready; binding browser iroh endpoint");
    let endpoint = rho_rpc::bind_browser_iroh_client(secret).await?;
    conn_log(&format!(
        "endpoint bound as {}; dialing daemon",
        endpoint.id()
    ));
    let connection = endpoint
        .connect(daemon, rho_ui_proto::IROH_ALPN)
        .await
        .map_err(|error| anyhow::anyhow!("connect to daemon: {error}"))?;
    conn_log("iroh connection established; authenticating");
    match rho_rpc::authenticate_iroh_client(&connection, endpoint.id()).await? {
        rho_iroh_auth::ClientAuthResult::Approved => conn_log("authenticated as enrolled client"),
        rho_iroh_auth::ClientAuthResult::EnrollmentRequired(code) => {
            conn_log(&format!("enrollment required, code {code}"));
            let code = code.to_string();
            let _ = events.unbounded_send(Event::Phase(Phase::Enroll(code.clone())));
            if let Some(sink) = &host_sink {
                sink.send(ConnEvent::EnrollmentRequired(code));
            }
            return Ok(());
        }
        rho_iroh_auth::ClientAuthResult::Unavailable => {
            anyhow::bail!("daemon cannot accept another enrollment right now")
        }
    }
    *dialer.borrow_mut() = Some(rho_rpc::Dialer::Iroh(connection.clone()));
    let (send, recv) = connection
        .open_bi()
        .await
        .map_err(|error| anyhow::anyhow!("open stream: {error}"))?;
    conn_log("control stream open; subscribing");
    send.set_priority(1)
        .map_err(|error| anyhow::anyhow!("set control stream priority: {error}"))?;
    let mut send = rho_rpc::Writer::new(send);
    let mut recv = rho_rpc::Reader::new(recv);
    rho_ui_proto::write_frame(&mut send, &ClientMessage::Subscribe).await?;
    let channel_connection = connection.clone();
    spawn_local(async move {
        while let Some(command) = receiver.next().await {
            match command {
                Command::Control(message) => {
                    if rho_ui_proto::write_frame(&mut send, &message)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Command::Realtime {
                    offer_sdp,
                    response,
                } => {
                    let connection = channel_connection.clone();
                    spawn_local(async move {
                        let _ = response.send(dial_realtime(connection, offer_sdp).await);
                    });
                }
            }
        }
    });
    futures::try_join!(
        read_loop(&events, host_sink.as_ref(), &shell_requests, &mut recv),
        read_agent_streams(events.clone(), host_sink.clone(), connection)
    )?;
    Ok(())
}

async fn dial_realtime(
    connection: iroh::endpoint::Connection,
    offer_sdp: String,
) -> anyhow::Result<crate::realtime_client::RealtimeChannel> {
    let mut stream = rho_rpc::Dialer::Iroh(connection).open(Some(50)).await?;
    rho_ui_proto::write_frame(&mut stream, &ClientMessage::RealtimeOpen { offer_sdp }).await?;
    let answer_sdp = match rho_ui_proto::read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::RealtimeOpened { answer_sdp } => answer_sdp,
        ServerMessage::RealtimeRefused { reason } => anyhow::bail!(reason),
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

async fn read_loop(
    events: &UnboundedSender<Event>,
    host_sink: Option<&HostSink>,
    shell_requests: &Mutex<ShellControlRequests>,
    recv: &mut rho_rpc::Reader,
) -> anyhow::Result<()> {
    loop {
        let message = rho_ui_proto::read_frame(recv)
            .await
            .map_err(|error| anyhow::anyhow!("daemon connection lost: {error}"))?;
        handle_host_message(host_sink, shell_requests, &message);
        let _ = events.unbounded_send(Event::Message(message));
    }
}

async fn read_agent_streams(
    events: UnboundedSender<Event>,
    host_sink: Option<HostSink>,
    connection: iroh::endpoint::Connection,
) -> anyhow::Result<()> {
    const FRAME_BUDGET: usize = 64 * 1024 * 1024;
    let budget = Arc::new(tokio::sync::Semaphore::new(FRAME_BUDGET));
    let generations = Rc::new(RefCell::new(AgentStreamGenerations::default()));
    loop {
        let recv = connection
            .accept_uni()
            .await
            .map_err(|error| anyhow::anyhow!("accept agent stream: {error}"))?;
        let events = events.clone();
        let budget = Arc::clone(&budget);
        let generations = Rc::clone(&generations);
        let host_sink = host_sink.clone();
        spawn_local(async move {
            if let Err(error) =
                read_agent_stream(events.clone(), host_sink, recv, budget, generations).await
            {
                let _ = events.unbounded_send(Event::Phase(Phase::Failed(format!(
                    "agent stream closed: {error:#}"
                ))));
            }
        });
    }
}

async fn read_agent_stream(
    events: UnboundedSender<Event>,
    host_sink: Option<HostSink>,
    recv: iroh::endpoint::RecvStream,
    budget: Arc<tokio::sync::Semaphore>,
    generations: Rc<RefCell<AgentStreamGenerations>>,
) -> anyhow::Result<()> {
    let mut recv = rho_rpc::Reader::new(recv);
    let (header, _header_allocation) = read_budgeted(&mut recv, &budget)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent stream closed before its header"))?;
    let ServerMessage::AgentStreamOpened { agent_id } = header else {
        anyhow::bail!("invalid agent stream header")
    };
    let generation = generations.borrow_mut().open(agent_id);
    loop {
        let Some((message, allocation)) = read_budgeted(&mut recv, &budget).await? else {
            return Ok(());
        };
        let ServerMessage::Agent {
            agent_id: frame_agent_id,
            ..
        } = &message
        else {
            anyhow::bail!("invalid message on agent stream")
        };
        anyhow::ensure!(*frame_agent_id == agent_id, "agent stream id changed");
        if generations.borrow().is_current(agent_id, generation) {
            if let Some(sink) = &host_sink {
                if let ServerMessage::Agent { agent_id, frame } = &message {
                    sink.send(ConnEvent::Frame {
                        agent_id: *agent_id,
                        frame: frame.clone(),
                        allocation: Some(allocation),
                    });
                }
            }
            let _ = events.unbounded_send(Event::Message(message));
        }
    }
}

async fn read_budgeted(
    recv: &mut rho_rpc::Reader,
    budget: &Arc<tokio::sync::Semaphore>,
) -> anyhow::Result<Option<(ServerMessage, AgentFrameAllocation)>> {
    let message =
        rho_rpc::read_frame_allocated_optional(recv, rho_ui_proto::MAX_FRAME_LEN, |len| {
            Arc::clone(budget).acquire_many_owned(len as u32)
        })
        .await?;
    let Some((message, permit, _)) = message else {
        return Ok(None);
    };
    let permit = permit.map_err(|_| anyhow::anyhow!("agent frame budget closed"))?;
    Ok(Some((message, AgentFrameAllocation { _permit: permit })))
}

async fn passkey_secret(daemon: EndpointId) -> anyhow::Result<iroh::SecretKey> {
    let storage = local_storage().ok_or_else(|| anyhow::anyhow!("local storage unavailable"))?;
    let credential_id = match storage
        .get_item(CREDENTIAL_KEY)
        .ok()
        .flatten()
        .and_then(|hex| decode_hex_vec(&hex))
        .filter(|id| id.len() <= MAX_CREDENTIAL_ID_LEN)
    {
        Some(id) => {
            conn_log("using stored passkey credential");
            id
        }
        None => {
            let _ = storage.remove_item(CREDENTIAL_KEY);
            conn_log("no stored credential; creating passkey (browser prompt)");
            let id = create_passkey().await?;
            storage
                .set_item(CREDENTIAL_KEY, &encode_hex(&id))
                .map_err(|_| anyhow::anyhow!("store passkey credential id"))?;
            id
        }
    };

    let mut input = Sha256::new();
    input.update(PRF_LABEL);
    input.update(daemon.as_bytes());
    conn_log("evaluating passkey PRF (browser prompt)");
    let mut prf = evaluate_prf(&credential_id, &input.finalize()).await?;
    let hkdf = Hkdf::<Sha256>::new(Some(daemon.as_bytes()), &prf);
    let mut seed = [0u8; 32];
    hkdf.expand(HKDF_INFO, &mut seed)
        .map_err(|_| anyhow::anyhow!("derive iroh key from passkey PRF"))?;
    let secret = iroh::SecretKey::from_bytes(&seed);
    prf.zeroize();
    seed.zeroize();
    // Do not leave identities created by older builds readable by page script.
    let _ = storage.remove_item(LEGACY_SECRET_KEY);
    Ok(secret)
}

async fn create_passkey() -> anyhow::Result<Vec<u8>> {
    let challenge = random_bytes(32)?;
    let user_id = random_bytes(32)?;
    let public_key = Object::new();
    set(
        &public_key,
        "challenge",
        &Uint8Array::from(challenge.as_slice()),
    )?;

    let rp = Object::new();
    set(&rp, "name", &JsValue::from_str("Rho Web UI"))?;
    set(&public_key, "rp", &rp)?;

    let user = Object::new();
    set(&user, "id", &Uint8Array::from(user_id.as_slice()))?;
    set(&user, "name", &JsValue::from_str("rho-webui"))?;
    set(&user, "displayName", &JsValue::from_str("Rho Web UI"))?;
    set(&public_key, "user", &user)?;

    let parameter = Object::new();
    set(&parameter, "type", &JsValue::from_str("public-key"))?;
    set(&parameter, "alg", &JsValue::from_f64(-7.0))?;
    let parameters = Array::new();
    parameters.push(&parameter);
    set(&public_key, "pubKeyCredParams", &parameters)?;
    set(&public_key, "attestation", &JsValue::from_str("none"))?;

    let selection = Object::new();
    set(
        &selection,
        "userVerification",
        &JsValue::from_str("required"),
    )?;
    set(&selection, "residentKey", &JsValue::from_str("preferred"))?;
    if let Some(attachment) = local_storage().and_then(|s| s.get_item(AUTHENTICATOR_KEY).ok()?) {
        set(
            &selection,
            "authenticatorAttachment",
            &JsValue::from_str(&attachment),
        )?;
    }
    set(&public_key, "authenticatorSelection", &selection)?;
    let extensions = Object::new();
    set(&extensions, "prf", &Object::new())?;
    set(&public_key, "extensions", &extensions)?;

    let options = Object::new();
    set(&options, "publicKey", &public_key)?;
    let credential = credentials_call("create", &options).await?;
    extension_prf_enabled(&credential)?;
    let raw_id = Reflect::get(&credential, &JsValue::from_str("rawId"))
        .map_err(|_| anyhow::anyhow!("passkey response has no credential id"))?;
    Ok(Uint8Array::new(&raw_id).to_vec())
}

async fn evaluate_prf(credential_id: &[u8], input: &[u8]) -> anyhow::Result<[u8; 32]> {
    let public_key = Object::new();
    set(
        &public_key,
        "challenge",
        &Uint8Array::from(random_bytes(32)?.as_slice()),
    )?;
    set(
        &public_key,
        "userVerification",
        &JsValue::from_str("required"),
    )?;
    let descriptor = Object::new();
    set(&descriptor, "type", &JsValue::from_str("public-key"))?;
    set(&descriptor, "id", &Uint8Array::from(credential_id))?;
    let allowed = Array::new();
    allowed.push(&descriptor);
    set(&public_key, "allowCredentials", &allowed)?;

    let eval = Object::new();
    set(&eval, "first", &Uint8Array::from(input))?;
    let prf = Object::new();
    set(&prf, "eval", &eval)?;
    let extensions = Object::new();
    set(&extensions, "prf", &prf)?;
    set(&public_key, "extensions", &extensions)?;
    let options = Object::new();
    set(&options, "publicKey", &public_key)?;

    let credential = credentials_call("get", &options).await?;
    let results = extension_results(&credential)?;
    let prf = Reflect::get(&results, &JsValue::from_str("prf"))
        .map_err(|_| anyhow::anyhow!("passkey did not return PRF results"))?;
    let results = Reflect::get(&prf, &JsValue::from_str("results"))
        .map_err(|_| anyhow::anyhow!("passkey did not evaluate its PRF"))?;
    let first = Reflect::get(&results, &JsValue::from_str("first"))
        .map_err(|_| anyhow::anyhow!("passkey did not return the requested PRF output"))?;
    let output = Uint8Array::new(&first).to_vec();
    output
        .try_into()
        .map_err(|_| anyhow::anyhow!("passkey PRF output is not 32 bytes"))
}

async fn credentials_call(method: &str, options: &Object) -> anyhow::Result<JsValue> {
    let navigator = web_sys::window()
        .ok_or_else(|| anyhow::anyhow!("browser window unavailable"))?
        .navigator();
    let credentials = Reflect::get(navigator.as_ref(), &JsValue::from_str("credentials"))
        .map_err(|_| anyhow::anyhow!("WebAuthn is unavailable"))?;
    let function: Function = Reflect::get(&credentials, &JsValue::from_str(method))
        .map_err(|_| anyhow::anyhow!("WebAuthn credentials.{method} is unavailable"))?
        .dyn_into()
        .map_err(|_| anyhow::anyhow!("WebAuthn credentials.{method} is unavailable"))?;
    let promise: Promise = function
        .call1(&credentials, options)
        .map_err(|error| anyhow::anyhow!("WebAuthn {method} failed: {error:?}"))?
        .dyn_into()
        .map_err(|_| anyhow::anyhow!("WebAuthn credentials.{method} returned no promise"))?;
    JsFuture::from(promise)
        .await
        .map_err(|error| anyhow::anyhow!("WebAuthn {method} failed: {error:?}"))
}

fn extension_results(credential: &JsValue) -> anyhow::Result<JsValue> {
    let function: Function =
        Reflect::get(credential, &JsValue::from_str("getClientExtensionResults"))
            .map_err(|_| anyhow::anyhow!("passkey extension results unavailable"))?
            .dyn_into()
            .map_err(|_| anyhow::anyhow!("passkey extension results unavailable"))?;
    function
        .call0(credential)
        .map_err(|_| anyhow::anyhow!("read passkey extension results"))
}

fn extension_prf_enabled(credential: &JsValue) -> anyhow::Result<()> {
    let results = extension_results(credential)?;
    let prf = Reflect::get(&results, &JsValue::from_str("prf"))
        .map_err(|_| anyhow::anyhow!("this browser or passkey does not support WebAuthn PRF"))?;
    let enabled = Reflect::get(&prf, &JsValue::from_str("enabled"))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    anyhow::ensure!(
        enabled,
        "this browser or passkey does not support WebAuthn PRF"
    );
    Ok(())
}

fn random_bytes(len: usize) -> anyhow::Result<Vec<u8>> {
    // Not `crypto.getRandomValues` on the wasm memory directly: GPUI-web
    // builds with threads, so wasm memory is a SharedArrayBuffer, and the
    // WebCrypto spec rejects views of shared buffers. The getrandom crate
    // copies through a non-shared scratch buffer.
    let mut bytes = vec![0u8; len];
    getrandom_02::getrandom(&mut bytes)
        .map_err(|_| anyhow::anyhow!("browser random number generation failed"))?;
    Ok(bytes)
}

fn set(target: &Object, name: &str, value: &JsValue) -> anyhow::Result<()> {
    Reflect::set(target, &JsValue::from_str(name), value)
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("build WebAuthn options"))
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex_vec(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for chunk in text.as_bytes().chunks(2) {
        let chunk = std::str::from_utf8(chunk).ok()?;
        bytes.push(u8::from_str_radix(chunk, 16).ok()?);
    }
    Some(bytes)
}
