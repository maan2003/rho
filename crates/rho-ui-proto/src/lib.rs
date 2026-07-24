//! Senax-framed Unix-socket protocol shared by rho UI processes.
//!
//! This crate intentionally owns only the wire vocabulary and framing. The CLI
//! and daemon can map these messages onto concrete `rho-agent` handles without
//! teaching lower crates about sockets or UI policy.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, bail};
use camino::Utf8PathBuf;
pub use rho_agent::MessageDelivery;
pub use rho_agent::db::{
    AdvisorIntelligence, AgentDisposition, AgentId, AgentIdDomain, AgentRole, EngineerIntelligence,
    WorkstreamId,
};
use rho_core::ContentPart;
pub use rho_workspaces::{
    WorkspaceDiffContent, WorkspaceDiffFile, WorkspaceDiffSnapshot, WorkspaceDiffStatus,
    WorkspaceDiffTarget, WorkspaceId, WorkspaceIdDomain, WorkspaceInfo,
};
use senax_encoder::{Decode, Encode, Pack, Packer, Unpack, Unpacker};

pub mod client;
pub mod remote;
pub mod server;
pub mod shell;
pub mod term;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Maximum accepted frame payload size.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
/// ALPN identifying this protocol on iroh connections to the daemon.
pub const IROH_ALPN: &[u8] = b"rho/ui/2";
const FRAME_LEN_BYTES: u64 = size_of::<u32>() as u64;
const PROTOCOL_LOG_MAGIC: &[u8; 4] = b"RUP2";

/// Fixed per-user daemon socket used by normal CLI and Git helper clients.
pub fn socket_path() -> anyhow::Result<std::path::PathBuf> {
    let base = dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .ok_or_else(|| anyhow::anyhow!("runtime directory not available"))?;
    Ok(base.join("rho").join("rho.sock"))
}

/// Shared byte counters for one UI protocol connection.
///
/// Counts successful length-prefixed frames on the wire, including the 4-byte
/// little-endian frame length.
#[derive(Clone, Debug, Default)]
pub struct IoCounters {
    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

impl IoCounters {
    pub fn snapshot(&self) -> IoStats {
        IoStats {
            sent: self.sent.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
        }
    }

    fn record_sent(&self, payload_len: usize) {
        self.sent
            .fetch_add(frame_wire_len(payload_len), Ordering::Relaxed);
    }

    fn record_received(&self, payload_len: usize) {
        self.received
            .fetch_add(frame_wire_len(payload_len), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IoStats {
    pub sent: u64,
    pub received: u64,
}

/// Message sent from a UI client to the rho daemon.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum ClientMessage {
    Ping,
    Subscribe,
    NewAgent {
        /// The workstream to join; `None` founds a fresh one, named
        /// provisionally until the agent's generated title lands.
        workstream: Option<WorkstreamId>,
        role: AgentRole,
        /// Where the agent's working copy starts (including which repo, for
        /// the modes that need one).
        start: StartMode,
        content: Option<Vec<ContentPart>>,
    },
    LoadAgent {
        agent_id: AgentId,
    },
    SendUserMessage {
        agent_id: AgentId,
        content: Vec<ContentPart>,
        delivery: MessageDelivery,
    },
    CompactAgent {
        agent_id: AgentId,
        delivery: MessageDelivery,
    },
    RenameAgent {
        agent_id: AgentId,
        name: String,
    },
    /// Renames a workstream; a colliding name gets a numeric suffix.
    WorkstreamRename {
        workstream_id: WorkstreamId,
        name: String,
    },
    CancelTurn {
        agent_id: AgentId,
    },
    RewindAgent {
        agent_id: AgentId,
        turns: u32,
    },
    ContinueTurn {
        agent_id: AgentId,
    },
    /// Adds or removes one free-form label on a workstream; semantics
    /// ("pin", "group:slack", …) live in the client's view layer.
    WorkstreamLabel {
        workstream_id: WorkstreamId,
        label: String,
        add: bool,
    },
    /// Adds or removes one free-form label on an agent.
    AgentLabel {
        agent_id: AgentId,
        label: String,
        add: bool,
    },
    /// Moves an agent to another workstream; its spawn subtree moves with
    /// it (an agent's workstream is always its root's).
    AgentMove {
        agent_id: AgentId,
        target: WorkstreamTarget,
    },
    /// Replaces the stored client view configuration; the daemon keeps the
    /// bytes opaque and hands them back on [`ServerMessage::Ready`].
    ViewConfigSet {
        data: Vec<u8>,
    },
    /// The user's verdict on an agent's last finished turn. Attention is
    /// action-cleared: viewing an agent never clears it; `Done`, snoozing,
    /// replying, landing, or hiding do.
    SetAgentDisposition {
        agent_id: AgentId,
        disposition: AgentDisposition,
    },
    /// Registers a project, or updates it if `path` is already registered.
    /// `name` defaults to the path's basename.
    ProjectSet {
        path: Utf8PathBuf,
        name: Option<String>,
        description: String,
    },
    ProjectRemove {
        path: Utf8PathBuf,
    },
    AcquireLandLease {
        repo: Utf8PathBuf,
        agent_id: Option<AgentId>,
    },
    LandStatus {
        repo: Utf8PathBuf,
        agent_id: Option<AgentId>,
        status: LandStatus,
    },
    ReleaseLandLease {
        repo: Utf8PathBuf,
        agent_id: Option<AgentId>,
    },
    McpAgentTool {
        request_id: u64,
        self_agent_id: AgentId,
        request: McpAgentToolRequest,
    },
    /// Install messaging-platform secrets (e.g. Slack tokens) into the
    /// daemon's RAM-only store and (re)start the platform connection.
    PlatformSecretsSet {
        secrets: Vec<(String, String)>,
        coordinator_repo: Option<Utf8PathBuf>,
    },
    /// Approve a pending iroh client enrollment by its displayed code,
    /// trusting that client's endpoint key persistently.
    IrohApprove {
        code: String,
    },
    /// Directly trust an iroh endpoint in daemon memory. This is a privileged
    /// local-control operation intended to be invoked through SSH.
    IrohTrustInMemory {
        endpoint_id: String,
    },
    /// Revoke persistent trust for an iroh client endpoint.
    IrohRevoke {
        endpoint_id: String,
    },
    PrCommand {
        request_id: u64,
        agent_id: Option<String>,
        command: PrCommand,
    },
    /// Give a Rho-runtime agent a fresh key for subsequent provider requests.
    ChangePromptCacheKey {
        agent_id: AgentId,
    },
    /// Dedicates this whole stream to a zed-remote channel: sent as the
    /// *first* message on a fresh stream (a new iroh bi-stream or Unix
    /// connection), never inside a UI session. The daemon binds a headless
    /// project and replies [`ServerMessage::ChannelOpened`]; after that
    /// handshake the stream carries raw frames ([`read_raw_frame`]) holding
    /// prost-encoded zed proto envelopes. Closing the stream closes the
    /// channel.
    ChannelOpen {
        workspace: WorkspaceInfo,
    },
    /// Selects the high-weight agent state stream on an iroh connection.
    /// Ignored on transports that carry agent state in the control session.
    AgentStreamFocus {
        agent_id: Option<AgentId>,
    },
    /// Spawns a daemon-owned terminal for an agent: sent as the *first*
    /// message on a fresh stream, like [`ClientMessage::ChannelOpen`].
    /// Refused ([`ServerMessage::TerminalRefused`]) if `terminal_id` is
    /// already running. On success the daemon replies
    /// [`ServerMessage::TerminalOpened`]; with `attach` the stream then
    /// carries senax frames of [`term::TermClientFrame`] /
    /// [`term::TermServerFrame`], otherwise the terminal runs headless and
    /// the stream closes.
    TerminalCreate {
        /// Display handle or id prefix, resolved by the daemon ("eng-ht08").
        agent: String,
        /// Client-chosen id, unique among the agent's running terminals
        /// ([`ClientMessage::TerminalList`] enumerates them).
        terminal_id: u64,
        /// Continue this stream as an attached terminal stream.
        attach: bool,
        /// Initial PTY size.
        cols: u16,
        rows: u16,
    },
    /// Attaches this whole stream to a *running* terminal (refused if it is
    /// not running): handshake and frames as in
    /// [`ClientMessage::TerminalCreate`] with `attach`. Closing the stream
    /// detaches; the terminal keeps running.
    TerminalAttach {
        agent: String,
        terminal_id: u64,
        /// The client's viewport, applied to the PTY (last writer wins).
        cols: u16,
        rows: u16,
    },
    /// One-shot request on a fresh stream: the daemon replies with a single
    /// [`ServerMessage::TerminalList`] (or [`ServerMessage::TerminalRefused`]
    /// if `agent` does not resolve) and closes the stream.
    TerminalList {
        /// Restrict to one agent's terminals (display handle or id prefix).
        agent: Option<String>,
    },
    /// Advertise this control connection as a client-held SSH Git transport
    /// provider. Every native GUI registers and may receive approval requests.
    GitTransportRegister,
    /// First frame on a Git remote-helper stream. The daemon pairs it with
    /// the registered GUI provider, then the stream switches to raw Git data.
    GitTransportRequest {
        request: GitTransportRequest,
    },
    /// First frame on the GUI's dedicated provider stream.
    GitTransportProvide {
        request_id: u64,
        provider_id: u64,
        /// Whether this GUI claims the transport after approving the operation.
        /// The first claim selects the credential provider.
        claim: bool,
    },
    /// One-shot query on a fresh local stream used by the remote helper to
    /// choose PAT-backed GitHub HTTP or client-held SSH before negotiation.
    GitTransportQuery {
        host: String,
    },
    /// Starts the daemon-owned Comint-style shell for an agent. This travels
    /// on the main UI control stream; attachment is a separate stream.
    ShellStart {
        request_id: u64,
        /// Display handle or id prefix, resolved by the daemon ("eng-ht08").
        agent: String,
    },
    /// Attaches this dedicated stream to an already-running shell. Closing
    /// the stream only detaches; it does not stop the shell.
    ShellAttach {
        agent: String,
    },
    /// Main-control request listing running shells, optionally for one agent.
    ShellList {
        request_id: u64,
        agent: Option<String>,
    },
    /// Main-control request to gracefully stop an agent's running shell.
    ShellClose {
        request_id: u64,
        agent: String,
    },
    /// One-shot request on a fresh stream for a persistent jj snapshot and
    /// parent-side diff manifest. Current-side text remains in Zed buffers.
    /// The daemon replies with
    /// [`ServerMessage::DiffSnapshot`] or [`ServerMessage::DiffRefused`] and
    /// closes the stream.
    DiffSnapshot {
        workspace: WorkspaceInfo,
        known_commit_id: Option<String>,
        /// Dirty Zed buffers whose paths may not yet exist in jj's disk
        /// snapshot. The daemon supplies their immutable parent side.
        include_paths: Vec<Utf8PathBuf>,
    },
    /// Requests the daemon account's weekly ChatGPT Codex allowance.
    ChatGptUsage,
    QuotaHistory,
    AgentUsage {
        agent_id: AgentId,
        since_ms: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum GitService {
    UploadPack,
    ReceivePack,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct GitTransportRequest {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub repository: String,
    pub service: GitService,
    /// Destination refs authorized by the first GUI approval for a push.
    /// Fetches carry `None`.
    pub planned_refs: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum PrCommand {
    Create {
        owner: String,
        repo: String,
        head: String,
        base: String,
        title: String,
        body: String,
        review_bots: Vec<String>,
    },
    Subscribe {
        url: String,
        replay_existing: bool,
        review_bots: Vec<String>,
    },
    Status {
        url: String,
    },
    List,
    Stop {
        url: String,
    },
    Comment {
        url: String,
        reply: Option<String>,
        body: String,
    },
    Rerun {
        url: String,
        run_id: u64,
    },
    Logs {
        url: String,
        run_id: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum McpAgentToolRequest {
    SpawnEngineer {
        task_name: String,
        prompt: String,
        /// The child's working set, primary first; empty forks the spawning
        /// agent's whole working set.
        workdirs: Vec<McpSpawnWorkdir>,
    },
    MessageAgent {
        agent_id: String,
        message: String,
    },
    InterruptEngineer {
        engineer_id: String,
    },
    AskAdvisor {
        message: String,
    },
    FollowupAdvisor {
        advisor_id: String,
        message: String,
    },
    Wait {
        timeout_seconds: Option<u64>,
    },
}

/// One spawn `workdirs` entry, passed through as the tool surface received
/// it; the daemon validates and parses it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct McpSpawnWorkdir {
    pub repo: String,
    pub revset: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct McpAgentToolResponse {
    pub request_id: u64,
    pub output: String,
    pub is_error: bool,
}

/// Where a new agent works. Each mode carries exactly the data it needs:
/// joining an existing workspace already knows its repo, the others say
/// which repo they mean.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum StartMode {
    /// A fresh workspace in `repo` with a new change on top of the revset.
    /// Clients resolve agent targets to `<workspace name>@` themselves
    /// (workspace names arrive on [`UiAgentSummary`]).
    NewOn { repo: Utf8PathBuf, revset: String },
    /// A fresh restricted workspace in `repo` on top of the revset.
    Sandbox { repo: Utf8PathBuf, revset: String },
    /// The SAME workspace as the target: no new checkout — agents share the
    /// directory (and namespace), seeing each other's edits instantly.
    /// Joining the user means working directly in the user's checkout.
    Join(JoinTarget),
}

/// Whose workspace [`StartMode::Join`] joins.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum JoinTarget {
    /// A known workspace, sent back verbatim from [`UiAgentSummary`].
    Workspace(WorkspaceInfo),
    /// The user's own checkout of `repo`.
    User { repo: Utf8PathBuf },
}

/// Destination of [`ClientMessage::AgentMove`]. `Named` is resolved by the
/// daemon against workstream names and creates the workstream when no match
/// exists, so "spin off a workstream around this agent" is one message.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkstreamTarget {
    Existing(WorkstreamId),
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum LandStatus {
    Queued,
    Preparing,
    Checking,
    Publishing,
    Landed,
    Bounced,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct LandLeaseHolder {
    pub pid: Option<u32>,
    pub uid: u32,
    pub gid: u32,
}

/// Message sent from the rho daemon to a UI client.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum ServerMessage {
    Pong,
    Ready {
        workstreams: Vec<UiWorkstream>,
        agents: Vec<UiAgentSummary>,
        projects: Vec<UiProject>,
        /// The client-owned view configuration blob, verbatim from the last
        /// [`ClientMessage::ViewConfigSet`] (empty if never set).
        view_config: Vec<u8>,
        /// The daemon database's machine seed; clients need it to encode
        /// agent IDs (see [`AgentIdDomain`]).
        machine_seed: u64,
        /// Last allocated agent-id counter; clients use it for uniform
        /// short-prefix rendering.
        agent_counter: u64,
    },
    Error {
        message: String,
    },
    PlatformStatus {
        running: bool,
        detail: String,
    },
    Agent {
        agent_id: AgentId,
        frame: remote::AgentRemoteFrame,
    },
    AgentCreated {
        agent_id: AgentId,
        workstream: WorkstreamId,
    },
    WorkstreamCreated {
        workstream: UiWorkstream,
    },
    AgentLoaded {
        agent_id: AgentId,
    },
    TurnCancelled {
        agent_id: AgentId,
    },
    /// An agent's attention level changed; broadcast to every connection so
    /// rails stay truthful without loading the agent.
    AgentAttention {
        agent_id: AgentId,
        attention: UiAttention,
    },
    LandLeaseQueued {
        repo: Utf8PathBuf,
        holder: Option<LandLeaseHolder>,
    },
    LandLeaseGranted {
        repo: Utf8PathBuf,
    },
    LandStatus {
        repo: Utf8PathBuf,
        agent_id: Option<AgentId>,
        status: LandStatus,
    },
    McpAgentToolResult(McpAgentToolResponse),
    /// Reply to [`ClientMessage::IrohApprove`]: the enrolled client's
    /// endpoint id.
    IrohApproved {
        endpoint_id: String,
    },
    IrohRevoked {
        endpoint_id: String,
    },
    PrCommandResult {
        request_id: u64,
        output: String,
        data: Vec<u8>,
        is_error: bool,
    },
    /// Handshake reply on a zed-channel stream (see
    /// [`ClientMessage::ChannelOpen`]): the headless project is bound and the
    /// stream now speaks raw envelope frames. `root` is the project root the
    /// client should open worktrees under — the workspace checkout as the
    /// daemon sees it (a managed checkout path, or the repo root for user
    /// checkouts).
    ChannelOpened {
        root: Utf8PathBuf,
    },
    /// Handshake refusal on a zed-channel stream; the daemon closes the
    /// stream after sending it.
    ChannelClosed {
        reason: String,
    },
    /// First frame on a daemon-opened iroh unidirectional stream. Every later
    /// frame on that stream is [`ServerMessage::Agent`] for this agent.
    AgentStreamOpened {
        agent_id: AgentId,
    },
    /// Handshake reply on a terminal stream (see
    /// [`ClientMessage::TerminalCreate`] and
    /// [`ClientMessage::TerminalAttach`]). On an attached stream the first
    /// [`term::TermServerFrame`] after it is a snapshot of the current screen
    /// preceded by history.
    TerminalOpened {
        terminal_id: u64,
    },
    /// Handshake refusal on a terminal stream; the daemon closes the stream
    /// after sending it.
    TerminalRefused {
        reason: String,
    },
    /// Reply to [`ClientMessage::TerminalList`]: every running terminal
    /// (of one agent, if the request named one).
    TerminalList {
        terminals: Vec<term::TerminalInfo>,
    },
    /// Request fanned out to every registered GUI credential provider.
    GitTransportRequested {
        request_id: u64,
        provider_id: u64,
        request: GitTransportRequest,
    },
    /// Dedicated Git stream handshake succeeded; subsequent bytes are raw
    /// Git protocol data.
    GitTransportReady,
    GitTransportRefused {
        reason: String,
    },
    GitTransportPolicy {
        pat_available: bool,
    },
    /// An approval race completed or expired. Deliberately carries no result
    /// or winner information.
    GitTransportDone {
        request_id: u64,
    },
    /// Handshake reply on a Comint-style shell stream.
    ShellOpened,
    /// Main-control reply to [`ClientMessage::ShellStart`].
    ShellStarted {
        request_id: u64,
    },
    /// Main-control reply to [`ClientMessage::ShellList`].
    ShellList {
        request_id: u64,
        shells: Vec<shell::ShellInfo>,
    },
    /// Main-control reply after [`ClientMessage::ShellClose`] stops the shell.
    ShellClosed {
        request_id: u64,
    },
    /// Failed main-control lifecycle request.
    ShellRequestFailed {
        request_id: u64,
        reason: String,
    },
    /// Handshake refusal on a dedicated shell attachment stream.
    ShellAttachRefused {
        reason: String,
    },
    DiffSnapshot {
        snapshot: WorkspaceDiffSnapshot,
    },
    DiffUnchanged {
        commit_id: String,
    },
    DiffRefused {
        reason: String,
    },
    ChatGptUsage {
        used_percent: f64,
        reset_at_unix: i64,
    },
    QuotaUsage {
        summaries: Vec<QuotaSummary>,
    },
    QuotaHistory {
        series: Vec<QuotaSeries>,
    },
    AgentUsage {
        agent_id: AgentId,
        model: String,
        buckets: Vec<AgentUsageBucket>,
        total: AgentUsageBucket,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSummary {
    pub model: String,
    pub remaining_percent: u8,
    pub burn_10m: u16,
    pub burn_2h: u16,
    pub burn_1d: u16,
    pub burn_3d: u16,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSeries {
    pub model: String,
    pub points: Vec<QuotaPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaPoint {
    pub observed_at_ms: u64,
    pub remaining_percent: u8,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    pub approximate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiWorkstream {
    pub workstream_id: WorkstreamId,
    pub name: String,
    /// Free-form markers ("pin", "group:slack", …); semantics live in the
    /// client's view layer.
    pub labels: Vec<String>,
}

/// Enough about an agent to list and label it without loading it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiAgentSummary {
    pub agent_id: AgentId,
    /// The agent that spawned this one. The GUI uses parent edges to
    /// present delegated work inline beneath its parent.
    pub parent_agent: Option<AgentId>,
    pub display_name: Option<String>,
    pub created_at: rho_core::UnixMs,
    pub updated_at: rho_core::UnixMs,
    /// The opinionated configuration represented by this agent's pinned session
    /// profile.
    pub role: AgentRole,
    /// Where the agent works. Clients resolve start targets against this
    /// themselves: "on top of agent" is the revset `<ws-id>@`, and
    /// joining sends the info back verbatim.
    pub workspace: WorkspaceInfo,
    /// Attention level at summary time; kept current afterwards by
    /// [`ServerMessage::AgentAttention`].
    pub attention: UiAttention,
    /// When the agent last finished a turn (creation time if it never ran).
    /// Recency tiebreak for rail sorting; clients keep it current from
    /// Working broadcasts.
    pub last_active: rho_core::UnixMs,
    /// The user filed this agent away (`AgentDisposition::Hidden`): fold it
    /// immediately instead of waiting out the rail's idle window.
    pub hidden: bool,
    /// One-line snippet of the user's last message; empty if none yet.
    /// What the work is about, for summaries and naming.
    #[senax(default)]
    pub last_user_message_text: String,
    /// The workstream this agent belongs to (exactly one).
    pub workstream: WorkstreamId,
    /// Free-form markers ("pin", …); semantics live in the client's view
    /// layer.
    pub labels: Vec<String>,
}

/// How urgently an agent wants the user, in ascending order — the rail's
/// whole vocabulary for "which agent needs my focus". Derived by the daemon
/// from agent state × the persisted disposition; never sent finer-grained
/// than this (Streaming vs ToolCalling is transcript detail, not attention).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack,
)]
pub enum UiAttention {
    /// Done, snoozed, never finished a turn, or a sub-agent (whose turns are
    /// its parent's court, not the user's).
    #[default]
    Quiet,
    /// A turn is running; the agent's court.
    Working,
    /// A turn finished and awaits the user's disposition.
    Pending,
    /// Blocked on the user: the turn errored or stopped unfinished.
    NeedsInput,
}

/// A registered project available for agent routing, keyed by path.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiProject {
    pub path: Utf8PathBuf,
    pub name: String,
    pub description: String,
}

/// Encode and write one length-prefixed senax frame.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Packer,
{
    write_frame_counted(writer, value, None).await
}

/// Encode and write one length-prefixed senax frame, recording bytes on
/// successful completion when counters are supplied.
pub async fn write_frame_counted<W, T>(
    writer: &mut W,
    value: &T,
    counters: Option<&IoCounters>,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Packer,
{
    let payload = senax_encoder::pack(value).context("pack protocol frame")?;
    if payload.len() > MAX_FRAME_LEN {
        bail!(
            "protocol frame length {} exceeds {MAX_FRAME_LEN}",
            payload.len()
        );
    }
    let len: u32 = payload
        .len()
        .try_into()
        .context("protocol frame too large")?;
    writer
        .write_u32_le(len)
        .await
        .context("write frame length")?;
    writer
        .write_all(&payload)
        .await
        .context("write frame payload")?;
    writer.flush().await.context("flush frame")?;
    if let Some(counters) = counters {
        counters.record_sent(payload.len());
    }
    Ok(())
}

/// Read and decode one length-prefixed senax frame.
pub async fn read_frame<R, T>(reader: &mut R) -> anyhow::Result<T>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
{
    read_frame_counted(reader, None).await
}

/// Read and decode one length-prefixed senax frame, recording bytes on
/// successful completion when counters are supplied.
pub async fn read_frame_counted<R, T>(
    reader: &mut R,
    counters: Option<&IoCounters>,
) -> anyhow::Result<T>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
{
    let len = reader.read_u32_le().await.context("read frame length")? as usize;
    if len > MAX_FRAME_LEN {
        bail!("protocol frame length {len} exceeds {MAX_FRAME_LEN}");
    }

    let mut payload = vec![0; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read frame payload")?;
    let mut payload = payload.as_slice();
    let message = senax_encoder::unpack(&mut payload).context("unpack protocol frame")?;
    if let Some(counters) = counters {
        counters.record_received(len);
    }
    Ok(message)
}

fn frame_wire_len(payload_len: usize) -> u64 {
    FRAME_LEN_BYTES + payload_len as u64
}

/// Write one length-prefixed raw frame (no senax encoding): the framing used
/// by zed-channel streams after the [`ClientMessage::ChannelOpen`] handshake.
pub async fn write_raw_frame<W>(writer: &mut W, payload: &[u8]) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME_LEN {
        bail!("raw frame length {} exceeds {MAX_FRAME_LEN}", payload.len());
    }
    let len: u32 = payload.len().try_into().context("raw frame too large")?;
    writer
        .write_u32_le(len)
        .await
        .context("write raw frame length")?;
    writer
        .write_all(payload)
        .await
        .context("write raw frame payload")?;
    writer.flush().await.context("flush raw frame")?;
    Ok(())
}

/// Read one length-prefixed raw frame; `Ok(None)` on clean EOF at a frame
/// boundary (the peer closed the channel).
pub async fn read_raw_frame<R>(reader: &mut R) -> anyhow::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let len = match reader.read_u32_le().await {
        Ok(len) => len as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("read raw frame length"),
    };
    if len > MAX_FRAME_LEN {
        bail!("raw frame length {len} exceeds {MAX_FRAME_LEN}");
    }
    let mut payload = vec![0; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read raw frame payload")?;
    Ok(Some(payload))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolLogDirection {
    ClientToServer,
    ServerToClient,
}

impl ProtocolLogDirection {
    fn byte(self) -> u8 {
        match self {
            Self::ClientToServer => 0,
            Self::ServerToClient => 1,
        }
    }

    fn from_byte(byte: u8) -> anyhow::Result<Self> {
        match byte {
            0 => Ok(Self::ClientToServer),
            1 => Ok(Self::ServerToClient),
            _ => bail!("invalid protocol log direction {byte}"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClientToServer => "send",
            Self::ServerToClient => "recv",
        }
    }
}

pub fn protocol_frame_bytes<T>(message: &T) -> anyhow::Result<Vec<u8>>
where
    T: Packer,
{
    let payload = senax_encoder::pack(message).context("pack protocol log frame")?;
    let len: u32 = payload
        .len()
        .try_into()
        .context("protocol log frame too large")?;
    let mut frame = Vec::with_capacity(size_of::<u32>() + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn append_protocol_log_record(
    writer: &mut impl std::io::Write,
    unix_ms: u128,
    direction: ProtocolLogDirection,
    frame: &[u8],
) -> anyhow::Result<()> {
    let unix_ms: u64 = unix_ms
        .try_into()
        .context("protocol log timestamp overflow")?;
    let len: u32 = frame
        .len()
        .try_into()
        .context("protocol log frame too large")?;
    writer.write_all(PROTOCOL_LOG_MAGIC)?;
    writer.write_all(&unix_ms.to_le_bytes())?;
    writer.write_all(&[direction.byte()])?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(frame)?;
    Ok(())
}

pub fn print_protocol_log(
    path: impl AsRef<std::path::Path>,
    output: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let mut input = std::fs::File::open(path).context("open protocol log")?;
    loop {
        let Some((unix_ms, direction, frame)) = read_protocol_log_record(&mut input)? else {
            return Ok(());
        };
        if frame.len() < size_of::<u32>() {
            bail!("protocol log frame shorter than length prefix");
        }
        let payload_len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        let mut payload = frame
            .get(4..)
            .filter(|payload| payload.len() == payload_len)
            .context("protocol log frame length mismatch")?;
        match direction {
            ProtocolLogDirection::ClientToServer => {
                let message: ClientMessage =
                    senax_encoder::unpack(&mut payload).context("unpack client frame")?;
                writeln!(
                    output,
                    "{unix_ms} {} {}B {message:#?}",
                    direction.label(),
                    frame.len()
                )?;
            }
            ProtocolLogDirection::ServerToClient => {
                let message: ServerMessage =
                    senax_encoder::unpack(&mut payload).context("unpack server frame")?;
                writeln!(
                    output,
                    "{unix_ms} {} {}B {message:#?}",
                    direction.label(),
                    frame.len()
                )?;
            }
        }
    }
}

fn read_protocol_log_record(
    input: &mut impl std::io::Read,
) -> anyhow::Result<Option<(u64, ProtocolLogDirection, Vec<u8>)>> {
    let mut magic = [0; 4];
    match input.read_exact(&mut magic) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("read protocol log magic"),
    }
    if &magic != PROTOCOL_LOG_MAGIC {
        bail!("invalid protocol log magic");
    }
    let mut timestamp = [0; 8];
    input
        .read_exact(&mut timestamp)
        .context("read protocol log timestamp")?;
    let unix_ms = u64::from_le_bytes(timestamp);
    let mut direction = [0; 1];
    input
        .read_exact(&mut direction)
        .context("read protocol log direction")?;
    let direction = ProtocolLogDirection::from_byte(direction[0])?;
    let mut len = [0; 4];
    input
        .read_exact(&mut len)
        .context("read protocol log frame length")?;
    let len = u32::from_le_bytes(len) as usize;
    let mut frame = vec![0; len];
    input
        .read_exact(&mut frame)
        .context("read protocol log frame")?;
    Ok(Some((unix_ms, direction, frame)))
}

/// Marker tying this protocol layer to `rho-agent` without putting socket code
/// in the agent crate.
pub type Agent = rho_agent::Agent;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_wire_len_includes_length_prefix() {
        assert_eq!(frame_wire_len(0), 4);
        assert_eq!(frame_wire_len(12), 16);
    }

    #[test]
    fn protocol_log_records_full_length_prefixed_frame() {
        let frame = protocol_frame_bytes(&ClientMessage::Ping).unwrap();
        let mut log = Vec::new();
        append_protocol_log_record(&mut log, 123, ProtocolLogDirection::ClientToServer, &frame)
            .unwrap();

        let mut cursor = std::io::Cursor::new(log);
        let (unix_ms, direction, recorded_frame) =
            read_protocol_log_record(&mut cursor).unwrap().unwrap();
        assert_eq!(unix_ms, 123);
        assert_eq!(direction, ProtocolLogDirection::ClientToServer);
        assert_eq!(recorded_frame, frame);

        let mut payload = &recorded_frame[4..];
        let message: ClientMessage = senax_encoder::unpack(&mut payload).unwrap();
        assert_eq!(message, ClientMessage::Ping);
    }

    #[test]
    fn pr_command_round_trips() {
        let message = ClientMessage::PrCommand {
            request_id: 7,
            agent_id: Some("eng-abcd".into()),
            command: PrCommand::Comment {
                url: "https://github.com/acme/widgets/pull/1".into(),
                reply: Some("inline:9:v1".into()),
                body: "addressed".into(),
            },
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn agent_stream_control_messages_round_trip() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(7)).unwrap();
        for message in [
            ClientMessage::AgentStreamFocus {
                agent_id: Some(agent_id),
            },
            ClientMessage::AgentStreamFocus { agent_id: None },
        ] {
            let bytes = senax_encoder::pack(&message).unwrap();
            let mut slice: &[u8] = &bytes;
            let decoded = senax_encoder::unpack(&mut slice).unwrap();
            assert_eq!(message, decoded);
        }

        let message = ServerMessage::AgentStreamOpened { agent_id };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn shell_messages_round_trip() {
        let client = ClientMessage::ShellStart {
            request_id: 7,
            agent: "eng-test".to_owned(),
        };
        let bytes = senax_encoder::pack(&client).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(client, decoded);

        let frame = shell::ShellServerFrame::ExecutionOutput {
            execution: 3,
            start: 0,
            end: 0,
            text: "λ".to_owned(),
            styles: vec![shell::ShellStyleSpan {
                start: 0,
                end: 2,
                style: shell::ShellTextStyle {
                    foreground: Some(shell::ShellColor::Indexed(1)),
                    bold: true,
                    ..Default::default()
                },
            }],
        };
        let bytes = senax_encoder::pack(&frame).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(frame, decoded);

        assert!(shell::command_fits(&"x".repeat(shell::MAX_COMMAND_BYTES)));
        assert!(!shell::command_fits(
            &"x".repeat(shell::MAX_COMMAND_BYTES + 1)
        ));
    }

    #[test]
    fn git_transport_messages_round_trip() {
        let request = GitTransportRequest {
            host: "git.example".to_owned(),
            port: 2222,
            user: "deploy".to_owned(),
            repository: "team/repo.git".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec!["refs/heads/main".to_owned()]),
        };
        for message in [
            ClientMessage::GitTransportRegister,
            ClientMessage::GitTransportRequest {
                request: request.clone(),
            },
            ClientMessage::GitTransportProvide {
                request_id: 9,
                provider_id: 4,
                claim: true,
            },
            ClientMessage::GitTransportQuery {
                host: "github.com".to_owned(),
            },
        ] {
            let bytes = senax_encoder::pack(&message).unwrap();
            let mut slice: &[u8] = &bytes;
            let decoded = senax_encoder::unpack(&mut slice).unwrap();
            assert_eq!(message, decoded);
        }

        let message = ServerMessage::GitTransportPolicy {
            pat_available: true,
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);

        let message = ServerMessage::GitTransportDone { request_id: 9 };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn diff_manifest_messages_round_trip() {
        let workspace = WorkspaceInfo::UserCheckout {
            repo: Utf8PathBuf::from("/repo"),
        };
        let request = ClientMessage::DiffSnapshot {
            workspace,
            known_commit_id: Some("known".to_owned()),
            include_paths: vec![Utf8PathBuf::from("src/live.rs")],
        };
        let bytes = senax_encoder::pack(&request).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(request, decoded);

        let response = ServerMessage::DiffSnapshot {
            snapshot: WorkspaceDiffSnapshot {
                operation_id: "operation".to_owned(),
                commit_id: "commit".to_owned(),
                files: vec![WorkspaceDiffFile {
                    path: Utf8PathBuf::from("src/lib.rs"),
                    status: WorkspaceDiffStatus::Modified,
                    base: WorkspaceDiffContent::Text("old".to_owned()),
                    target: WorkspaceDiffTarget::Text { bytes: 3 },
                    base_executable: Some(false),
                    target_executable: Some(false),
                }],
                truncated: false,
            },
        };
        let bytes = senax_encoder::pack(&response).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(response, decoded);
    }
}
