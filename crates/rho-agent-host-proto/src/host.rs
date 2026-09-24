//! The machine itself, opened by [`crate::Open::Host`]: its session with a
//! GUI ([`crate::control`]), voice, desktops, Git transport, and one-shot
//! administration ([`Call`]).

use senax_encoder::{Decode, Encode, Pack, Packer, Unpack, Unpacker};

use crate::{GitTransportRequest, PrCommand};

/// What a host stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// A GUI's session: what the host pushes to it ([`crate::control`]).
    /// One per iroh connection.
    Control,
    /// One [`Call`], answered with one [`crate::Answer`]; then the stream
    /// closes.
    Request(Request),
    /// A voice session. Answered with [`crate::realtime::Opened`]; after
    /// the answer the stream carries [`crate::realtime::RealtimeClientFrame`]
    /// and [`crate::realtime::RealtimeServerFrame`].
    Realtime { offer_sdp: String },
    /// One live application over MoQ streams on this connection. Answered
    /// with [`crate::Opened`].
    Wayland {
        media_id: u64,
        agent: String,
        session: String,
    },
    /// A Git remote helper's transport, paired with a GUI that provides
    /// it. After [`crate::Opened::Ready`] the stream is raw Git data.
    GitTransport { request: GitTransportRequest },
    /// A GUI's answer to
    /// [`crate::control::ServerFrame::GitTransportRequested`].
    /// Answered with [`crate::GitProvided`]; after `Ready` the stream is raw
    /// Git data.
    GitProvide {
        request_id: u64,
        provider_id: u64,
        /// Whether this GUI claims the transport after approving the
        /// operation. The first claim selects the credential provider.
        claim: bool,
    },
}

/// A one-shot call on the machine: a stream of its own that opens with
/// [`Open::Request`] and is answered with one [`crate::Answer`] of its
/// reply.
pub trait Call: Into<Request> + Send + 'static {
    type Reply: Packer + Unpacker + std::fmt::Debug + Send + 'static;
    const PRIORITY: Option<i32> = Some(1);
}

calls! {
    /// Every call the machine answers, as it goes on the wire.
    pub enum Request {
        /// Answered with whether a GitHub PAT is available.
        GitTransportPolicy(GitTransportPolicy) -> bool;
        /// Answered with where the snapshot was stored.
        GuiTelemetryUpload(GuiTelemetryUpload) -> String, priority None;
        PlatformSecretsSet(PlatformSecretsSet) -> PlatformStatus;
        /// Answered with the enrolled client's endpoint id.
        IrohApprove(IrohApprove) -> String;
        IrohTrustInMemory(IrohTrustInMemory) -> ();
        /// Answered with the revoked endpoint id.
        IrohRevoke(IrohRevoke) -> String;
        /// Answered with where the copy is, in a directory of its own
        /// beside the database; the copy is the caller's to delete.
        Snapshot(Snapshot) -> camino::Utf8PathBuf;
        Pr(Pr) -> PrOutput;
    }
}

/// Makes one call on `stream`, a stream opened for it. A refusal is an
/// error.
pub async fn call<S, C>(stream: &mut S, call: C) -> anyhow::Result<C::Reply>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    C: Call,
{
    let open = crate::Open::Host(Open::Request(call.into()));
    crate::write_frame(stream, &open).await?;
    crate::read_frame::<_, crate::Answer<C::Reply>>(stream)
        .await?
        .into_result()
}

/// How the remote helper should reach `host`: PAT-backed GitHub HTTP or
/// client-held SSH.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GitTransportPolicy {
    pub host: String,
}

/// A bounded, client-produced performance snapshot, kept under the
/// daemon's state directory.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GuiTelemetryUpload {
    pub snapshot: Vec<u8>,
}

/// Installs platform secrets into the daemon's RAM-only store.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct PlatformSecretsSet {
    pub secrets: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct PlatformStatus {
    pub running: bool,
    pub detail: String,
}

/// Approves a pending iroh client enrollment by its displayed code,
/// trusting that client's endpoint key persistently.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct IrohApprove {
    pub code: String,
}

/// Trusts an iroh endpoint in daemon memory. A privileged local-control
/// operation intended to be invoked through SSH.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct IrohTrustInMemory {
    pub endpoint_id: String,
}

/// Revokes persistent trust for an iroh client endpoint.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct IrohRevoke {
    pub endpoint_id: String,
}

/// Copies the daemon's database for inspection, as of its latest commit
/// and ready to open without repair.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Snapshot;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Pr {
    pub agent_id: Option<String>,
    pub command: PrCommand,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct PrOutput {
    pub output: String,
    pub data: Vec<u8>,
    pub is_error: bool,
}
