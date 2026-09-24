//! What a client and an agent host say to each other. The words they share
//! about agents live in `rho-agent-types`.
//!
//! Transport, authentication, compression, and generic Senax framing live in
//! `rho-rpc`; this crate owns message types, their limits, logical traffic
//! accounting, and protocol logs.

use anyhow::{Context as _, bail};
use camino::Utf8PathBuf;
use rho_agent_types::{
    AgentId, AgentRole, ContentPart, MessageDelivery, WorksetMode, WorkspaceInfo,
};
use senax_encoder::{Decode, Encode, Pack, Packer, Unpack, Unpacker};

/// Declares a part's one-shot calls. Each is a type of its own that names
/// its answer ([`Call::Reply`]); `Request` is what goes on the wire, one
/// variant per call, and the part's own `Open` in scope carries it as
/// `Open::Request`. A call is answered with one [`Answer`] of its reply
/// type, on a stream of its own.
#[macro_export]
macro_rules! calls {
    (
        $(#[$enum_meta:meta])*
        pub enum Request {
            $(
                $(#[$meta:meta])*
                $variant:ident($call:ty) -> $reply:ty $(, priority $priority:expr)?;
            )*
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(
            Clone,
            Debug,
            PartialEq,
            senax_encoder::Encode,
            senax_encoder::Decode,
            senax_encoder::Pack,
            senax_encoder::Unpack,
        )]
        pub enum Request {
            $($(#[$meta])* $variant($call),)*
        }

        impl Request {
            /// Its answer, read from `frame`, as a protocol log prints it.
            pub fn debug_answer(&self, frame: &[u8]) -> String {
                match self {
                    $(Self::$variant(_) => $crate::debug_frame::<$crate::Answer<$reply>>(frame),)*
                }
            }
        }

        $(
            impl From<$call> for Request {
                fn from(call: $call) -> Self {
                    Self::$variant(call)
                }
            }

            impl $crate::Call for $call {
                type Open = Open;
                type Reply = $reply;
                $(const PRIORITY: Option<i32> = $priority;)?

                fn open(self) -> Open {
                    Open::Request(self.into())
                }
            }
        )*
    };
}

pub mod agents;
pub mod client;
pub mod desk;
pub mod host;
pub mod realtime;
#[cfg(not(target_family = "wasm"))]
pub mod server;
pub mod transcript;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Maximum accepted frame payload size.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
/// Window represented by each point in the agent-cost distribution graph.
pub const AGENT_COST_WINDOW_DAYS: u64 = 7;
/// Maximum encoded GUI performance snapshot accepted by the daemon.
pub const MAX_GUI_TELEMETRY_BYTES: usize = 8 * 1024 * 1024;
/// ALPN identifying this protocol on iroh connections to the daemon.
pub const IROH_ALPN: &[u8] = b"rho/ui/21";
#[cfg(not(target_family = "wasm"))]
const PROTOCOL_LOG_MAGIC: &[u8; 5] = b"RUP21";

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

/// Which part of the host a stream is for. Each part's messages belong to
/// the crate that speaks it, so this names the parts and nothing more.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Part {
    /// The agents: their journal and what is asked of them
    /// (`rho-agents-client`).
    Agents,
    /// The desk ([`desk::stream`]).
    Desk,
    /// The machine itself: desktops, voice, Git transport and
    /// administration ([`host`]).
    Host,
    /// An agent's terminals (`rho-terminal`).
    Terminal,
    /// An agent's shell (`rho-shell-view`).
    Shell,
    /// An agent's workspace files (`rho-files`).
    Workspace,
}

/// The first frame on every stream: which part it is for, and that part's
/// own opening, packed. Everything after it is the part's own, so neither
/// side ever reads a frame meant for another part.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Open {
    pub part: Part,
    pub open: Vec<u8>,
}

/// A part's own opening frame.
pub trait PartOpen: Packer + Unpacker + std::fmt::Debug + Send + Sync {
    const PART: Part;

    /// A reply on a stream this opened, as a protocol log prints it; `None`
    /// for a stream frame, which the log does not read.
    fn debug_reply(&self, _frame: &[u8]) -> Option<String> {
        None
    }
}

impl Open {
    pub fn of<T: PartOpen>(open: &T) -> anyhow::Result<Self> {
        Ok(Self {
            part: T::PART,
            open: senax_encoder::pack(open)
                .context("pack part opening")?
                .to_vec(),
        })
    }

    /// The part's own opening. Fails if the stream is for another part.
    pub fn unpack<T: PartOpen>(&self) -> anyhow::Result<T> {
        anyhow::ensure!(
            self.part == T::PART,
            "{:?} stream opened as {:?}",
            T::PART,
            self.part
        );
        senax_encoder::unpack(&mut self.open.as_slice()).context("unpack part opening")
    }
}

/// Opens a stream for a part: the first frame on it.
pub async fn write_open<W, T>(writer: &mut W, open: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: PartOpen,
{
    write_frame(writer, &Open::of(open)?).await
}

/// A one-shot call: a stream of its own, opened with the part's
/// `Open::Request` ([`calls!`]) and answered with one [`Answer`] of its
/// reply.
pub trait Call: Send + 'static {
    type Open: PartOpen;
    type Reply: Packer + Unpacker + std::fmt::Debug + Send + 'static;
    /// The stream's priority: above the sessions unless the answer is bulk.
    const PRIORITY: Option<i32> = Some(1);

    fn open(self) -> Self::Open;
}

/// Makes one call on `stream`, a stream opened for it. A refusal is an
/// error.
pub async fn call<S, C>(stream: &mut S, call: C) -> anyhow::Result<C::Reply>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Call,
{
    write_open(stream, &call.open()).await?;
    read_frame::<_, Answer<C::Reply>>(stream)
        .await?
        .into_result()
}

/// A stream's opening as a protocol log prints it, read as part `T`, or
/// with `reply` a reply on it.
pub fn describe_as<T: PartOpen>(open: &Open, reply: Option<&[u8]>) -> String {
    let opened = match open.unpack::<T>() {
        Ok(opened) => opened,
        Err(error) => return format!("(undecodable: {error:#})"),
    };
    match reply {
        None => format!("{:?} {opened:#?}", open.part),
        Some(frame) => opened
            .debug_reply(frame)
            .unwrap_or_else(|| "(stream frame)".to_owned()),
    }
}

/// The answer to opening a stream the host can refuse.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Opened {
    Ready,
    /// The host closes the stream after sending it.
    Refused {
        reason: String,
    },
}

/// The answer to [`host::Open::GitProvide`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum GitProvided {
    /// This GUI carries the transport: raw Git data follows.
    Ready,
    /// The approval race completed or expired. Deliberately carries no
    /// result or winner.
    Done,
}

/// A new agent for a host to start.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct NewAgent {
    pub role: AgentRole,
    /// Where the agent's working copy starts (including which repo, for
    /// the modes that need one).
    pub start: StartMode,
    /// How the agent sees the filesystem around its workset: a minimal
    /// generated root, or the host.
    pub mode: WorksetMode,
    pub content: Option<Vec<ContentPart>>,
}

/// What a client tells a host to do to one of its agents.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum AgentCommand {
    Send {
        agent_id: AgentId,
        content: Vec<ContentPart>,
        delivery: MessageDelivery,
    },
    Compact {
        agent_id: AgentId,
        delivery: MessageDelivery,
    },
    ChangeRole {
        agent_id: AgentId,
        role: AgentRole,
    },
    /// How the agent sees the filesystem from now on. Its loop restarts
    /// in the new view, so the Python notebook's state is lost.
    ChangeMode {
        agent_id: AgentId,
        mode: WorksetMode,
    },
    Cancel {
        agent_id: AgentId,
    },
    Rewind {
        agent_id: AgentId,
        turns: u32,
    },
    Continue {
        agent_id: AgentId,
    },
    /// Gives a Rho-runtime agent a fresh key for subsequent provider
    /// requests.
    ChangePromptCacheKey {
        agent_id: AgentId,
    },
}

impl AgentCommand {
    /// The agent the command is for.
    pub fn agent_id(&self) -> AgentId {
        match self {
            Self::Send { agent_id, .. }
            | Self::Compact { agent_id, .. }
            | Self::ChangeRole { agent_id, .. }
            | Self::ChangeMode { agent_id, .. }
            | Self::Cancel { agent_id }
            | Self::Rewind { agent_id, .. }
            | Self::Continue { agent_id }
            | Self::ChangePromptCacheKey { agent_id } => *agent_id,
        }
    }
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

/// Where a new agent works. Each mode carries exactly the data it needs.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum StartMode {
    /// A fresh workset holding a clone of `repo` (a URL or a daemon-side
    /// path), with a new change on top of the revset.
    NewOn { repo: Utf8PathBuf, revset: String },
    /// The SAME place as the target: the new agent works in the target
    /// agent's directory, seeing its edits instantly.
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub struct DesktopSession {
    pub agent: String,
    pub name: String,
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

/// The answer to a one-shot call: what it asked for, or why the host
/// would not.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum Answer<T: Packer + Unpacker> {
    Done(T),
    /// Not done, and why: the whole chain of causes.
    Failed {
        reason: String,
    },
}

impl<T: Packer + Unpacker> Answer<T> {
    pub fn into_result(self) -> anyhow::Result<T> {
        match self {
            Self::Done(reply) => Ok(reply),
            Self::Failed { reason } => Err(anyhow::anyhow!(reason)),
        }
    }
}

impl<T: Packer + Unpacker> From<anyhow::Result<T>> for Answer<T> {
    fn from(result: anyhow::Result<T>) -> Self {
        match result {
            Ok(reply) => Self::Done(reply),
            // The whole chain, not just the outermost context: a new agent
            // that failed said "create managed workspace" and kept the
            // reason to itself, which is not something a reader can act on.
            Err(error) => Self::Failed {
                reason: format!("{error:#}"),
            },
        }
    }
}

#[doc(hidden)]
pub fn debug_frame<T: Unpacker + std::fmt::Debug>(mut frame: &[u8]) -> String {
    match senax_encoder::unpack::<T>(&mut frame) {
        Ok(value) => format!("{value:#?}"),
        Err(error) => format!("(undecodable: {error})"),
    }
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

/// Prints a protocol log. `describe` reads a part's frames, which this
/// crate does not know: a stream's opening with `None`, a reply on it with
/// the reply's frame ([`describe_as`]).
#[cfg(not(target_family = "wasm"))]
pub fn print_protocol_log(
    path: impl AsRef<std::path::Path>,
    output: &mut impl std::io::Write,
    describe: impl Fn(&Open, Option<&[u8]>) -> String,
) -> anyhow::Result<()> {
    let mut input = std::fs::File::open(path).context("open protocol log")?;
    let mut opened = None;
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
        let message = match direction {
            ProtocolLogDirection::ClientToServer => {
                let open: Open =
                    senax_encoder::unpack(&mut payload).context("unpack client frame")?;
                let message = describe(&open, None);
                opened = Some(open);
                message
            }
            // What the host answers is the opened part's own; only a
            // request's reply is read.
            ProtocolLogDirection::ServerToClient => match &opened {
                Some(open) => describe(open, Some(payload)),
                None => "(stream frame)".to_owned(),
            },
        };
        writeln!(
            output,
            "{unix_ms} {} {}B {message}",
            direction.label(),
            frame.len()
        )?;
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
    use rho_agent_types::AgentIdDomain;

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

    fn round_trips<T>(message: T)
    where
        T: Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
    {
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: T = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn protocol_log_records_full_length_prefixed_frame() {
        let open = Open::of(&host::Open::Request(host::Snapshot.into())).unwrap();
        let frame = protocol_frame_bytes(&open).unwrap();
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
        let message: Open = senax_encoder::unpack(&mut payload).unwrap();
        assert_eq!(message, open);
    }

    #[test]
    fn protocol_log_rejects_previous_wire_epoch() {
        // The previous epoch's magic followed by a record's worth of bytes.
        let mut old = &b"RUP20\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"[..];
        assert!(read_protocol_log_record(&mut old).is_err());
    }

    #[test]
    fn requests_and_replies_round_trip() {
        let agent_id = AgentId::from_counter(7, &AgentIdDomain(1)).unwrap();
        for request in [
            host::Pr {
                agent_id: Some("eng-abcd".into()),
                command: PrCommand::Edit {
                    url: "https://github.com/acme/widgets/pull/1".into(),
                    base: Some("release".into()),
                    title: Some("Better title".into()),
                    body: Some("Better summary".into()),
                },
            }
            .into(),
            host::GitTransportPolicy {
                host: "github.com".to_owned(),
            }
            .into(),
            host::GuiTelemetryUpload {
                snapshot: br#"{"version":1}"#.to_vec(),
            }
            .into(),
            host::Snapshot.into(),
        ] {
            round_trips(host::Open::Request(request));
        }
        for request in [
            agents::SetAuthAccountEnabled {
                name: "work".to_owned(),
                enabled: false,
            }
            .into(),
            agents::AgentCostDistribution { since_ms: 42 }.into(),
            agents::RecordVisualization {
                mime_type: "image/svg+xml".to_owned(),
                content: b"<svg viewBox=\"0 0 1 1\"/>".to_vec(),
            }
            .into(),
            agents::QuotaUsage.into(),
            AgentCommand::Send {
                agent_id,
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
            }
            .into(),
        ] {
            round_trips(agents::Open::Request(request));
        }
        round_trips(Answer::Done(vec![AgentUsageSeries {
            model: "fable".to_owned(),
            buckets: vec![AgentUsageBucket {
                bucket_start_ms: 300_000,
                input_tokens: 10,
                ..AgentUsageBucket::default()
            }],
        }]));
        round_trips(Answer::Done(vec![AgentCostSeries {
            agent_id,
            model: "gpt".to_owned(),
            buckets: vec![AgentUsageBucket {
                bucket_start_ms: 3_600_000,
                output_tokens: 10,
                requests: 1,
                ..AgentUsageBucket::default()
            }],
        }]));
        round_trips(Answer::Done(agents::VisualizationContent {
            mime_type: "image/svg+xml".to_owned(),
            content: b"<svg viewBox=\"0 0 1 1\"/>".to_vec(),
        }));
        round_trips(Answer::Done(agent_id));
        round_trips(Answer::Done(()));
        round_trips(Answer::Done(true));
        round_trips(Answer::<String>::Failed {
            reason: "no such repository".to_owned(),
        });
    }

    /// A reply reads as its call's own type in a protocol log.
    #[test]
    fn protocol_log_prints_answers_by_their_call() {
        let request: agents::Request = agents::Visualization { id: "v".to_owned() }.into();
        let answer = senax_encoder::pack(&Answer::Done(agents::VisualizationContent {
            mime_type: "image/svg+xml".to_owned(),
            content: Vec::new(),
        }))
        .unwrap();
        assert!(request.debug_answer(&answer).starts_with("Done("));
    }

    #[test]
    fn git_provider_frames_round_trip() {
        round_trips(host::GitProviderFrame::Done { request_id: 9 });
    }

    /// A part's opening survives the envelope, and reads as no other part.
    fn opens_as<T: PartOpen + PartialEq>(open: T) {
        let envelope = Open::of(&open).unwrap();
        round_trips(envelope.clone());
        assert_eq!(envelope.unpack::<T>().unwrap(), open);
        let other = match T::PART {
            Part::Desk => envelope.unpack::<host::Open>().err(),
            _ => envelope.unpack::<desk::Open>().err(),
        };
        assert!(other.is_some());
    }

    #[test]
    fn stream_openings_round_trip() {
        let request = GitTransportRequest {
            host: "git.example".to_owned(),
            port: 2222,
            user: "deploy".to_owned(),
            repository: "team/repo.git".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec!["refs/heads/main".to_owned()]),
        };
        opens_as(host::Open::Desktops);
        opens_as(host::Open::GitProvider);
        opens_as(host::Open::GitTransport { request });
        opens_as(host::Open::GitProvide {
            request_id: 9,
            provider_id: 4,
            claim: true,
        });
        opens_as(agents::Open::Session);
        opens_as(desk::Open);
        round_trips(Opened::Refused {
            reason: "not running".to_owned(),
        });
        round_trips(GitProvided::Done);
    }
}
