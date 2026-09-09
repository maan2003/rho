//! UI wire vocabulary shared by Rho clients and the daemon.
//!
//! Transport, authentication, compression, and generic Senax framing live in
//! `rho-rpc`; this crate owns UI message types, UI-specific limits, logical
//! traffic accounting, and protocol logs.

use anyhow::{Context as _, bail};
use camino::Utf8PathBuf;
use rho_core::ContentPart;
pub use rho_core::{
    AdvisorIntelligence, AgentId, AgentIdDomain, AgentRole, EngineerIntelligence, MessageDelivery,
};
pub use rho_workspaces_types::{
    WorkspaceDiffBaseContent, WorkspaceDiffContent, WorkspaceDiffFile, WorkspaceDiffSnapshot,
    WorkspaceDiffStatus, WorkspaceDiffTarget, WorkspaceId, WorkspaceIdDomain, WorkspaceInfo,
};
use senax_encoder::{Decode, Encode, Pack, Packer, Unpack, Unpacker};

#[cfg(not(target_family = "wasm"))]
pub mod client;
#[doc(hidden)]
pub use rho_desk as desk_tree;
pub mod mirror;
pub mod realtime;
#[cfg(not(target_family = "wasm"))]
pub mod server;
pub mod shell;
pub mod term;
pub mod workspace;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
pub use workspace::{FileReadResult, FileSaveResult, WorkspaceClientFrame, WorkspaceServerFrame};

/// Maximum accepted frame payload size.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
/// Window represented by each point in the agent-cost distribution graph.
pub const AGENT_COST_WINDOW_DAYS: u64 = 7;
/// Maximum encoded GUI performance snapshot accepted by the daemon.
pub const MAX_GUI_TELEMETRY_BYTES: usize = 8 * 1024 * 1024;
/// ALPN identifying this protocol on iroh connections to the daemon.
pub const IROH_ALPN: &[u8] = b"rho/ui/12";
#[cfg(not(target_family = "wasm"))]
const PROTOCOL_LOG_MAGIC: &[u8; 5] = b"RUP12";

#[cfg(not(target_family = "wasm"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimePaths {
    socket: std::path::PathBuf,
    directory: std::path::PathBuf,
}

#[cfg(not(target_family = "wasm"))]
impl RuntimePaths {
    pub const SOCKET_ENV: &'static str = "RHO_SOCKET_PATH";

    pub fn new(socket: Option<impl Into<std::path::PathBuf>>) -> anyhow::Result<Self> {
        let socket = match socket {
            Some(socket) => {
                let socket = socket.into();
                if socket.is_absolute() {
                    socket
                } else {
                    std::env::current_dir()
                        .context("resolve current directory for relative socket path")?
                        .join(socket)
                }
            }
            None => {
                let base = dirs::runtime_dir()
                    .ok_or_else(|| anyhow::anyhow!("runtime directory not available"))?;
                base.join("rho").join("rho.sock")
            }
        };
        let directory = socket
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned();
        Ok(Self { socket, directory })
    }

    pub fn from_env() -> anyhow::Result<Self> {
        Self::new(std::env::var_os(Self::SOCKET_ENV).map(std::path::PathBuf::from))
    }

    pub fn resolve(socket: Option<impl Into<std::path::PathBuf>>) -> anyhow::Result<Self> {
        match socket {
            Some(socket) => Self::new(Some(socket)),
            None => Self::from_env(),
        }
    }

    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    pub fn directory(&self) -> &std::path::Path {
        &self.directory
    }

    pub fn octo_socket(&self) -> std::path::PathBuf {
        self.directory.join("octo.sock")
    }

    pub fn browser_socket(&self) -> std::path::PathBuf {
        self.directory.join("rho-browser.sock")
    }

    pub fn pr_logs(&self) -> std::path::PathBuf {
        self.directory.join("pr-logs")
    }

    pub fn daemon_lock(&self) -> std::path::PathBuf {
        self.directory.join(".rho-daemon.lock")
    }
}

/// Fixed per-user daemon socket used by normal clients.
#[cfg(not(target_family = "wasm"))]
pub fn socket_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(RuntimePaths::new(None::<std::path::PathBuf>)?
        .socket()
        .to_owned())
}

/// Message sent from a UI client to the rho daemon.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum ClientMessage {
    Ping,
    Subscribe,
    DeskSync {
        device: desk_tree::cells::DeviceId,
        known: desk_tree::cells::Version,
        /// Which store the client counted `known` in, when it holds a
        /// replica at all. A version is a count of writes per device inside
        /// one store; carried to another store the same numbers name writes
        /// that never happened. So a client that comes back holding one
        /// says whose numbers these are, and a daemon that does not
        /// recognise the name answers with the whole store rather than a
        /// difference from a number that was never its own.
        store: Option<desk_tree::cells::DeviceId>,
    },
    /// The client's half of a sync: the cells it holds that the daemon's
    /// frontier does not cover. The store is the client's, so the daemon
    /// catches up from it the same way it is caught up from.
    DeskCellsApply {
        cells: desk_tree::cells::Snapshot,
    },
    DeskMutationApply {
        mutation: desk_tree::cells::CellMutation,
    },
    /// An edit to a note's body, which is the only text the store holds.
    DeskTextApply {
        id: desk_tree::cells::Id,
        operation: desk_tree::TextOperation,
        transaction: Option<desk_tree::TextTransaction>,
    },
    NewAgent {
        role: AgentRole,
        /// Where the agent's working copy starts (including which repo, for
        /// the modes that need one).
        start: StartMode,
        content: Option<Vec<ContentPart>>,
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
    ChangeAgentRole {
        agent_id: AgentId,
        role: AgentRole,
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
    /// Enables or disables one provider account namespace on this host.
    SetAuthAccountEnabled {
        name: String,
        enabled: bool,
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
    /// Install platform secrets into the daemon's RAM-only store.
    PlatformSecretsSet {
        secrets: Vec<(String, String)>,
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
    /// Dedicates this whole stream to workspace file access: sent as the
    /// *first* message on a fresh stream (a new iroh bi-stream or Unix
    /// connection), never inside a UI session. The daemon binds a headless
    /// workspace and replies [`ServerMessage::ChannelOpened`]; after that
    /// handshake the stream carries [`workspace::WorkspaceClientFrame`] and
    /// [`workspace::WorkspaceServerFrame`] values. Closing the stream closes
    /// the channel and its filesystem watcher.
    ChannelOpen {
        workspace: WorkspaceInfo,
    },
    /// Opens a dedicated realtime stream. After
    /// [`ServerMessage::RealtimeOpened`] the stream carries
    /// [`realtime::RealtimeClientFrame`] and
    /// [`realtime::RealtimeServerFrame`] values until either side closes it.
    RealtimeOpen {
        offer_sdp: String,
    },
    /// The agents whose live frames this connection wants: the ones on
    /// screen. Everything durable arrives on the journal regardless, so
    /// this only decides who streams partial text and tools in flight.
    /// Replaces the set wholesale; an empty set asks for none.
    AgentStreamFocus {
        agent_ids: Vec<AgentId>,
    },
    /// Sent once after [`ServerMessage::Ready`]: the last journal entry
    /// this client holds for this host (zero for none). The daemon answers
    /// [`ServerMessage::Log`] pages for everything past it, then follows:
    /// every later append on any agent is pushed on this connection.
    Follow {
        since: mirror::Seq,
    },
    /// The bodies of raw events: tool output, a response whole.
    ///
    /// One request per chunk of transcript rather than one per call: a
    /// chunk's tool calls are one `Sent` each (measured at 1.01 results per
    /// `Sent` over the whole corpus), so asking per call would ask the same
    /// events over again. The daemon answers one [`ServerMessage::Detail`]
    /// per position, each naming its own `pos`, so the answers need no order
    /// and no correlation id.
    ///
    /// `pos` is the first position and `more` the rest. A daemon older than
    /// `more` skips the field it does not know and answers `pos` alone; the
    /// client draws the bodies it is given and leaves the rest folded.
    Detail {
        agent_id: AgentId,
        pos: mirror::AgentPos,
        #[senax(default)]
        more: Vec<mirror::AgentPos>,
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
    /// One-shot request on a fresh stream. The daemon persists this bounded,
    /// client-produced performance snapshot under its state directory.
    GuiTelemetryUpload {
        snapshot: Vec<u8>,
    },
    /// Requests the daemon account's weekly ChatGPT Codex allowance.
    ChatGptUsage,
    QuotaHistory,
    GlobalUsage {
        since_ms: u64,
    },
    /// Raw per-agent usage needed to form cost distributions beginning at
    /// `since_ms`. The daemon includes the fixed trailing-window lookback.
    AgentCostDistribution {
        since_ms: u64,
    },
    /// Asks which Claude accounts exist and which one agents run on, and
    /// replies with [`ServerMessage::ClaudeAccounts`].
    ClaudeAccounts,
    /// Puts every agent on `name` from its next turn, replying with
    /// [`ServerMessage::ClaudeAccounts`] as it stands after the switch.
    SetClaudeAccount {
        name: String,
    },
    /// Stores an immutable visualization snapshot and replies with
    /// [`ServerMessage::VisualizationRecorded`].
    RecordVisualization {
        mime_type: String,
        content: Vec<u8>,
    },
    /// One-shot request on a fresh stream. The daemon returns only the
    /// requested artifact, if it exists, then closes the stream.
    VisualizationGet {
        id: String,
    },
    /// Loads parent-side contents from the immutable operation returned by
    /// [`ClientMessage::DiffSnapshot`]. Replies are bounded and never
    /// snapshot the live working copy.
    DiffBaseContents {
        workspace: WorkspaceInfo,
        operation_id: String,
        commit_id: String,
        paths: Vec<Utf8PathBuf>,
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
        reply_comment: Option<u64>,
        body: String,
    },
    Comments {
        url: String,
    },
    Checks {
        url: String,
    },
    Rerun {
        url: String,
        run_id: u64,
    },
    Logs {
        url: String,
        run_id: u64,
    },
    Edit {
        url: String,
        base: Option<String>,
        title: Option<String>,
        body: Option<String>,
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
    /// (workspace names arrive on the mirror's `Created` event).
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
    /// A known workspace, sent back verbatim from the mirror's `Created`.
    Workspace(WorkspaceInfo),
    /// The user's own checkout of `repo`.
    User { repo: Utf8PathBuf },
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
    DeskSynced {
        /// The store this delta was counted in, so a client holding a
        /// replica can tell whether what it kept is behind this store or
        /// about a different one. When it does not match what the client
        /// holds, `delta` is the whole store, not a difference.
        store: desk_tree::cells::DeviceId,
        node_namespace: u16,
        delta: desk_tree::cells::Snapshot,
        bodies: Vec<desk_tree::cells::BodySnapshot>,
    },
    DeskCellsAvailable {
        frontier: desk_tree::cells::Version,
    },
    DeskTextApplied {
        id: desk_tree::cells::Id,
        operation: desk_tree::TextOperation,
        transaction: Option<desk_tree::TextTransaction>,
    },
    DeskResyncRequired,
    Ready {
        auth: AuthState,
        /// The daemon database's machine seed; clients need it to encode
        /// agent IDs (see [`AgentIdDomain`]).
        machine_seed: u64,
        /// Last allocated agent-id counter; clients use it for uniform
        /// short-prefix rendering.
        agent_counter: u64,
        /// How far this host's journal runs, so a client knows how far
        /// behind it is before it follows.
        journal_head: mirror::Seq,
    },
    Error {
        message: String,
    },
    /// The host's active/default auth changed, or its available namespaces
    /// were refreshed.
    AuthState {
        auth: AuthState,
    },
    PlatformStatus {
        running: bool,
        detail: String,
    },
    /// What a runtime has past the log, as it changes, for every agent
    /// any client is looking at.
    Live {
        agent_id: AgentId,
        live: mirror::Live,
    },
    AgentCreated {
        agent_id: AgentId,
    },
    TurnCancelled {
        agent_id: AgentId,
    },
    /// A run of the host's journal in order: the answer to
    /// [`ClientMessage::Follow`], paged, and afterwards every append as it
    /// lands. Entries never repeat and never skip within one connection.
    Log {
        entries: Vec<mirror::LogEntry>,
    },
    /// The answer to [`ClientMessage::Detail`].
    Detail {
        agent_id: AgentId,
        pos: mirror::AgentPos,
        body: mirror::DetailBody,
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
    /// Handshake reply on a workspace-channel stream (see
    /// [`ClientMessage::ChannelOpen`]).
    ChannelOpened,
    /// Handshake refusal on a workspace-channel stream; the daemon closes the
    /// stream after sending it.
    ChannelClosed {
        reason: String,
    },
    RealtimeOpened {
        answer_sdp: String,
    },
    RealtimeRefused {
        reason: String,
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
    GuiTelemetryStored {
        path: String,
    },
    GuiTelemetryRefused {
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
    GlobalUsage {
        series: Vec<AgentUsageSeries>,
    },
    AgentCostDistribution {
        series: Vec<AgentCostSeries>,
    },
    ClaudeAccounts {
        accounts: Vec<String>,
        current: String,
    },
    VisualizationRecorded {
        id: String,
    },
    VisualizationContent {
        id: String,
        mime_type: String,
        content: Vec<u8>,
    },
    VisualizationRefused {
        reason: String,
    },
    DiffBaseContents {
        contents: Vec<WorkspaceDiffBaseContent>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSummary {
    pub model: String,
    /// Daemon-local ChatGPT OAuth namespace; absent for Claude.
    pub auth_namespace: Option<String>,
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
    /// Daemon-local ChatGPT OAuth namespace; absent for Claude.
    pub auth_namespace: Option<String>,
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
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    pub approximate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentUsageSeries {
    pub model: String,
    pub buckets: Vec<AgentUsageBucket>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentCostSeries {
    /// Host-local identity. Clients combining hosts must keep the host in the
    /// distribution key rather than merging equal counters.
    pub agent_id: AgentId,
    pub model: String,
    pub buckets: Vec<AgentUsageBucket>,
}

/// Daemon-wide authentication settings presented by a GUI host.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AuthState {
    pub namespaces: Vec<String>,
    pub disabled_namespaces: Vec<String>,
    pub active_namespace: Option<String>,
}

/// Encode and write one length-prefixed senax frame.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Packer,
{
    rho_rpc::write_frame(writer, value, MAX_FRAME_LEN)
        .await
        .map(|_| ())
}

/// Read and decode one length-prefixed senax frame.
pub async fn read_frame<R, T>(reader: &mut R) -> anyhow::Result<T>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
{
    rho_rpc::read_frame(reader, MAX_FRAME_LEN)
        .await
        .map(|(value, _)| value)
}

/// Read and decode one frame, returning `None` after a cleanly finished
/// compressed stream at a frame boundary.
pub async fn read_frame_optional<R, T>(reader: &mut R) -> anyhow::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
{
    rho_rpc::read_frame_optional(reader, MAX_FRAME_LEN)
        .await
        .map(|frame| frame.map(|(value, _)| value))
}

/// Read and decode one frame with a protocol-specific bound smaller than the
/// global UI-frame ceiling.
pub async fn read_frame_limited<R, T>(reader: &mut R, max_len: usize) -> anyhow::Result<T>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
{
    rho_rpc::read_frame(reader, max_len)
        .await
        .map(|(value, _)| value)
}

/// Encode and write one frame with a protocol-specific bound smaller than the
/// global UI-frame ceiling.
pub async fn write_frame_limited<W, T>(
    writer: &mut W,
    value: &T,
    max_len: usize,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Packer,
{
    rho_rpc::write_frame(writer, value, max_len)
        .await
        .map(|_| ())
}

/// Write one length-prefixed raw frame (no senax encoding).
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
/// boundary.
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

#[cfg(not(target_family = "wasm"))]
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

#[cfg(not(target_family = "wasm"))]
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

#[cfg(not(target_family = "wasm"))]
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

#[cfg(not(target_family = "wasm"))]
fn read_protocol_log_record(
    input: &mut impl std::io::Read,
) -> anyhow::Result<Option<(u64, ProtocolLogDirection, Vec<u8>)>> {
    let mut magic = [0; 5];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_runtime_paths_share_an_absolute_directory() {
        let paths = RuntimePaths::new(Some("qa/rho.sock")).unwrap();

        assert!(paths.socket().is_absolute());
        assert_eq!(paths.socket().parent(), Some(paths.directory()));
        assert_eq!(paths.octo_socket(), paths.directory().join("octo.sock"));
        assert_eq!(
            paths.browser_socket(),
            paths.directory().join("rho-browser.sock")
        );
        assert_eq!(paths.pr_logs(), paths.directory().join("pr-logs"));
        assert_eq!(
            paths.daemon_lock(),
            paths.directory().join(".rho-daemon.lock")
        );
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
    fn protocol_log_rejects_previous_wire_epoch() {
        // The previous epoch's magic followed by a record's worth of bytes.
        let mut old = &b"RUP9\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"[..];
        assert!(read_protocol_log_record(&mut old).is_err());
    }

    #[test]
    fn desk_cells_messages_round_trip() {
        use desk_tree::cells::{
            CellMutation, CellWrite, DeviceId, Id, Property, Stamp, Uuid, Version,
        };

        let device = DeviceId([7; 16]);
        let id = Id::Note(Uuid([9; 16]));
        let mutation = CellMutation {
            stamp: Stamp {
                device,
                version: 12,
            },
            writes: vec![CellWrite {
                id: id.clone(),
                property: Property::Labeled {
                    label: Id::Label(Uuid([3; 16])),
                    present: true,
                },
            }],
            verdict: None,
        };
        let text_operation = desk_tree::TextOperation::Edit {
            timestamp: desk_tree::TreeClock {
                value: 1,
                replica_id: 4,
            },
            version: Vec::new(),
            ranges: vec![(0, 0)],
            new_text: vec!["note".into()],
        };
        for message in [
            ClientMessage::DeskSync {
                device,
                known: Version::from([(device, 11)]),
                store: Some(device),
            },
            ClientMessage::DeskMutationApply { mutation },
            ClientMessage::DeskTextApply {
                id: id.clone(),
                operation: text_operation.clone(),
                transaction: None,
            },
        ] {
            let bytes = senax_encoder::pack(&message).unwrap();
            let mut slice: &[u8] = &bytes;
            let decoded: ClientMessage = senax_encoder::unpack(&mut slice).unwrap();
            assert_eq!(decoded, message);
        }
        let message = ServerMessage::DeskSynced {
            store: device,
            node_namespace: 4,
            delta: desk_tree::cells::Snapshot::default(),
            bodies: Vec::new(),
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: ServerMessage = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(decoded, message);

        let message = ServerMessage::DeskTextApplied {
            id,
            operation: text_operation,
            transaction: None,
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: ServerMessage = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn pr_command_round_trips() {
        let message = ClientMessage::PrCommand {
            request_id: 7,
            agent_id: Some("eng-abcd".into()),
            command: PrCommand::Edit {
                url: "https://github.com/acme/widgets/pull/1".into(),
                base: Some("release".into()),
                title: Some("Better title".into()),
                body: Some("Better summary".into()),
            },
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn auth_settings_round_trip() {
        let message = ClientMessage::SetAuthAccountEnabled {
            name: "work".to_owned(),
            enabled: false,
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: ClientMessage = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(decoded, message);

        let message = ServerMessage::AuthState {
            auth: AuthState {
                namespaces: vec!["default".to_owned(), "work".to_owned()],
                disabled_namespaces: vec!["work".to_owned()],
                active_namespace: Some("default".to_owned()),
            },
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: ServerMessage = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn global_usage_response_round_trips() {
        let message = ServerMessage::GlobalUsage {
            series: vec![AgentUsageSeries {
                model: "fable".to_owned(),
                buckets: vec![AgentUsageBucket {
                    bucket_start_ms: 300_000,
                    input_tokens: 10,
                    ..AgentUsageBucket::default()
                }],
            }],
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn agent_cost_distribution_response_round_trips() {
        let request = ClientMessage::AgentCostDistribution { since_ms: 42 };
        let bytes = senax_encoder::pack(&request).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(request, decoded);

        let agent_id = AgentId::from_counter(7, &AgentIdDomain(1)).unwrap();
        let message = ServerMessage::AgentCostDistribution {
            series: vec![AgentCostSeries {
                agent_id,
                model: "gpt".to_owned(),
                buckets: vec![AgentUsageBucket {
                    bucket_start_ms: 3_600_000,
                    output_tokens: 10,
                    requests: 1,
                    ..AgentUsageBucket::default()
                }],
            }],
        };
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn visualization_messages_round_trip() {
        let request = ClientMessage::RecordVisualization {
            mime_type: "image/svg+xml".to_owned(),
            content: b"<svg viewBox=\"0 0 1 1\"/>".to_vec(),
        };
        let bytes = senax_encoder::pack(&request).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(request, decoded);

        let response = ServerMessage::VisualizationContent {
            id: "0123456789abcdef0123456789abcdef".to_owned(),
            mime_type: "image/svg+xml".to_owned(),
            content: b"<svg viewBox=\"0 0 1 1\"/>".to_vec(),
        };
        let bytes = senax_encoder::pack(&response).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(response, decoded);
    }

    #[test]
    fn agent_stream_control_messages_round_trip() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(7)).unwrap();
        for message in [
            ClientMessage::AgentStreamFocus {
                agent_ids: vec![agent_id],
            },
            ClientMessage::AgentStreamFocus { agent_ids: vec![] },
            ClientMessage::Follow {
                since: mirror::Seq(9),
            },
            ClientMessage::Detail {
                agent_id,
                pos: mirror::AgentPos(3),
                more: vec![mirror::AgentPos(4), mirror::AgentPos(9)],
            },
        ] {
            let bytes = senax_encoder::pack(&message).unwrap();
            let mut slice: &[u8] = &bytes;
            let decoded = senax_encoder::unpack(&mut slice).unwrap();
            assert_eq!(message, decoded);
        }

        for live in [
            mirror::Live::Requesting,
            mirror::Live::Item {
                index: 0,
                item: mirror::Item::Text {
                    text: "hel".to_owned(),
                    phase: Some(mirror::TextPhase::FinalAnswer),
                },
            },
            mirror::Live::Appended {
                index: 0,
                text: "lo".to_owned(),
            },
            mirror::Live::Waiting {
                until: Some(rho_core::UnixMs(5)),
            },
            mirror::Live::Idle,
        ] {
            let message = ServerMessage::Live { agent_id, live };
            let bytes = senax_encoder::pack(&message).unwrap();
            let mut slice: &[u8] = &bytes;
            let decoded = senax_encoder::unpack(&mut slice).unwrap();
            assert_eq!(message, decoded);
        }
    }

    #[test]
    fn image_user_message_round_trips() {
        let message = ClientMessage::SendUserMessage {
            agent_id: AgentId::from_counter(1, &AgentIdDomain(7)).unwrap(),
            content: vec![
                ContentPart::Text {
                    text: "inspect".to_owned(),
                },
                ContentPart::Image {
                    media_type: "image/gif".to_owned(),
                    data: vec![1, 2, 3],
                },
            ],
            delivery: MessageDelivery::NextRequest,
        };
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

        let request = ClientMessage::DiffBaseContents {
            workspace: WorkspaceInfo::UserCheckout {
                repo: Utf8PathBuf::from("/repo"),
            },
            operation_id: "operation".to_owned(),
            commit_id: "commit".to_owned(),
            paths: vec![Utf8PathBuf::from("src/lib.rs")],
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
                    base: WorkspaceDiffContent::Deferred,
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

    #[test]
    fn gui_telemetry_messages_round_trip() {
        let request = ClientMessage::GuiTelemetryUpload {
            snapshot: br#"{"version":1}"#.to_vec(),
        };
        let bytes = senax_encoder::pack(&request).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(request, decoded);

        let response = ServerMessage::GuiTelemetryStored {
            path: "/state/rho/gui-telemetry/snapshot.json".to_owned(),
        };
        let bytes = senax_encoder::pack(&response).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(response, decoded);
    }
}
