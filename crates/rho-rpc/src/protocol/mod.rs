//! How a client and an agent host talk over [`Stream`](crate::Stream)s:
//! the opening every stream starts with ([`Open`]), one-shot calls
//! ([`Call`]), bounded frames, the agent host's Unix socket ([`client`],
//! [`server`]), and protocol logs. Each protocol's own words live with the
//! crate that speaks them ([`Protocol`]).

use anyhow::{Context as _, bail};
use senax_encoder::{Decode, Encode, Pack, Packer, Unpack, Unpacker};

/// Declares a protocol's one-shot calls. Each is a type of its own that names
/// its answer ([`Call::Reply`]); `Request` is what goes on the wire, one
/// variant per call, and the protocol's own `Open` in scope carries it as
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
                    $(Self::$variant(_) => $crate::protocol::debug_frame::<$crate::protocol::Answer<$reply>>(frame),)*
                }
            }
        }

        $(
            impl From<$call> for Request {
                fn from(call: $call) -> Self {
                    Self::$variant(call)
                }
            }

            impl $crate::protocol::Call for $call {
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

pub mod client;
#[cfg(not(target_family = "wasm"))]
pub mod server;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Maximum accepted frame payload size.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
/// ALPN identifying this protocol on iroh connections to the agent host.
pub const IROH_ALPN: &[u8] = b"rho/ui/23";
#[cfg(not(target_family = "wasm"))]
const PROTOCOL_LOG_MAGIC: &[u8; 5] = b"RUP23";

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

    pub fn host_lock(&self) -> std::path::PathBuf {
        self.directory.join(".rho-agent-host.lock")
    }
}

/// Fixed per-user agent host socket used by normal clients.
#[cfg(not(target_family = "wasm"))]
pub fn socket_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(RuntimePaths::new(None::<std::path::PathBuf>)?
        .socket()
        .to_owned())
}

/// Which protocol a stream speaks. Each protocol's messages belong to the
/// crate that speaks it, so this names the protocols and nothing more.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Protocol {
    /// The agents: their journal and what is asked of them
    /// (`rho-agents-client`).
    Agents,
    /// The desktops in the host's worksets, and a live view of one
    /// (`rho-desktop-client`).
    Desktop,
    /// The machine itself: Git transport and administration
    /// (`rho-agent-hosts`).
    Host,
    /// What the user put in, sealed, for the host to keep and pass between
    /// their devices (`rho-ledger`).
    Ledger,
    /// An agent's shell (`rho-shell-view`).
    Shell,
    /// An agent's terminals (`rho-terminal`).
    Terminal,
    /// A voice session (`rho-rtc`).
    Voice,
    /// An agent's workspace files (`rho-files`).
    Workspace,
}

/// The first frame on every stream: which protocol it speaks, and that
/// protocol's own opening, packed. Everything after it is the protocol's
/// own, so neither side ever reads a frame meant for another protocol.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Open {
    pub protocol: Protocol,
    pub open: Vec<u8>,
}

/// A protocol's own opening frame.
pub trait ProtocolOpen: Packer + Unpacker + std::fmt::Debug + Send + Sync {
    const PROTOCOL: Protocol;

    /// A reply on a stream this opened, as a protocol log prints it; `None`
    /// for a stream frame, which the log does not read.
    fn debug_reply(&self, _frame: &[u8]) -> Option<String> {
        None
    }
}

impl Open {
    pub fn of<T: ProtocolOpen>(open: &T) -> anyhow::Result<Self> {
        Ok(Self {
            protocol: T::PROTOCOL,
            open: senax_encoder::pack(open)
                .context("pack protocol opening")?
                .to_vec(),
        })
    }

    /// The protocol's own opening. Fails if the stream is for another protocol.
    pub fn unpack<T: ProtocolOpen>(&self) -> anyhow::Result<T> {
        anyhow::ensure!(
            self.protocol == T::PROTOCOL,
            "{:?} stream opened as {:?}",
            T::PROTOCOL,
            self.protocol
        );
        senax_encoder::unpack(&mut self.open.as_slice()).context("unpack protocol opening")
    }
}

/// Opens a stream for a protocol: the first frame on it.
pub async fn write_open<W, T>(writer: &mut W, open: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: ProtocolOpen,
{
    write_frame(writer, &Open::of(open)?).await
}

/// A one-shot call: a stream of its own, opened with the protocol's
/// `Open::Request` ([`calls!`]) and answered with one [`Answer`] of its
/// reply.
pub trait Call: Send + 'static {
    type Open: ProtocolOpen;
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

/// A stream's opening as a protocol log prints it, read as protocol `T`, or
/// with `reply` a reply on it.
pub fn describe_as<T: ProtocolOpen>(open: &Open, reply: Option<&[u8]>) -> String {
    let opened = match open.unpack::<T>() {
        Ok(opened) => opened,
        Err(error) => return format!("(undecodable: {error:#})"),
    };
    match reply {
        None => format!("{:?} {opened:#?}", open.protocol),
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
    crate::write_frame(writer, value, MAX_FRAME_LEN)
        .await
        .map(|_| ())
}

/// Read and decode one length-prefixed senax frame.
pub async fn read_frame<R, T>(reader: &mut R) -> anyhow::Result<T>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
{
    crate::read_frame(reader, MAX_FRAME_LEN)
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
    crate::read_frame_optional(reader, MAX_FRAME_LEN)
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
    crate::read_frame(reader, max_len)
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
    crate::write_frame(writer, value, max_len).await.map(|_| ())
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

/// Prints a protocol log. `describe` reads a protocol's frames, which this
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
            // What the host answers is the opened protocol's own; only a
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
    use super::*;

    /// A protocol's opening, for these tests alone.
    #[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
    pub enum Open {
        Session,
        Request(Request),
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
    pub struct Ping;

    crate::calls! {
        pub enum Request {
            Ping(Ping) -> bool;
        }
    }

    impl ProtocolOpen for Open {
        const PROTOCOL: Protocol = Protocol::Host;

        fn debug_reply(&self, frame: &[u8]) -> Option<String> {
            match self {
                Self::Request(request) => Some(request.debug_answer(frame)),
                Self::Session => None,
            }
        }
    }

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
            paths.host_lock(),
            paths.directory().join(".rho-agent-host.lock")
        );
    }

    fn round_trips<T>(message: T)
    where
        T: Packer + Unpacker + PartialEq + std::fmt::Debug,
    {
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: T = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn protocol_log_records_full_length_prefixed_frame() {
        let open = super::Open::of(&Open::Request(Ping.into())).unwrap();
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
        let message: super::Open = senax_encoder::unpack(&mut payload).unwrap();
        assert_eq!(message, open);
    }

    #[test]
    fn protocol_log_rejects_previous_wire_epoch() {
        // The previous epoch's magic followed by a record's worth of bytes.
        let mut old = &b"RUP21\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"[..];
        assert!(read_protocol_log_record(&mut old).is_err());
    }

    #[test]
    fn answers_round_trip() {
        round_trips(Answer::Done("stored".to_owned()));
        round_trips(Answer::Done(()));
        round_trips(Answer::Done(true));
        round_trips(Answer::<String>::Failed {
            reason: "no such repository".to_owned(),
        });
        round_trips(Opened::Refused {
            reason: "not running".to_owned(),
        });
    }

    /// A reply reads as its call's own type in a protocol log.
    #[test]
    fn protocol_log_prints_answers_by_their_call() {
        let request: Request = Ping.into();
        let answer = senax_encoder::pack(&Answer::Done(true)).unwrap();
        assert!(request.debug_answer(&answer).starts_with("Done("));
    }

    /// A protocol's opening survives the envelope, and reads as no other
    /// protocol.
    #[test]
    fn openings_read_only_as_their_part() {
        let envelope = super::Open::of(&Open::Session).unwrap();
        round_trips(envelope.clone());
        assert_eq!(envelope.unpack::<Open>().unwrap(), Open::Session);
        let other = super::Open {
            protocol: Protocol::Agents,
            open: envelope.open.clone(),
        };
        assert!(other.unpack::<Open>().is_err());
    }
}
