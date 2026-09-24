//! The machine itself, opened by [`crate::Open::Host`]: its session with a
//! GUI ([`crate::control`]), voice, desktops, Git transport, and one-shot
//! administration ([`Request`]).

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::{GitTransportRequest, PrCommand};

/// What a host stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// A GUI's session: what the host pushes to it ([`crate::control`]).
    /// One per iroh connection.
    Control,
    /// One request, answered with one [`Reply`]; then the stream closes.
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

/// What a client can ask of the machine in one round trip.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Request {
    /// How the remote helper should reach `host`: PAT-backed GitHub HTTP
    /// or client-held SSH. Answered with [`Reply::GitTransportPolicy`].
    GitTransportPolicy { host: String },
    /// A bounded, client-produced performance snapshot, kept under the
    /// daemon's state directory. Answered with [`Reply::GuiTelemetryStored`].
    GuiTelemetryUpload { snapshot: Vec<u8> },
    /// Installs platform secrets into the daemon's RAM-only store.
    /// Answered with [`Reply::PlatformStatus`].
    PlatformSecretsSet { secrets: Vec<(String, String)> },
    /// Approves a pending iroh client enrollment by its displayed code,
    /// trusting that client's endpoint key persistently. Answered with
    /// [`Reply::IrohApproved`].
    IrohApprove { code: String },
    /// Trusts an iroh endpoint in daemon memory. A privileged
    /// local-control operation intended to be invoked through SSH.
    IrohTrustInMemory { endpoint_id: String },
    /// Revokes persistent trust for an iroh client endpoint.
    IrohRevoke { endpoint_id: String },
    /// Copies the daemon's database for inspection, as of its latest
    /// commit and ready to open without repair. Answered with
    /// [`Reply::Snapshotted`]; the copy is the caller's to delete.
    Snapshot,
    /// Answered with [`Reply::Pr`].
    Pr {
        agent_id: Option<String>,
        command: PrCommand,
    },
}

/// The answer to a [`Request`].
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Reply {
    /// Done, with nothing to say.
    Done,
    /// Not done, and why: the whole chain of causes.
    Failed {
        reason: String,
    },
    GitTransportPolicy {
        pat_available: bool,
    },
    GuiTelemetryStored {
        path: String,
    },
    PlatformStatus {
        running: bool,
        detail: String,
    },
    /// The enrolled client's endpoint id.
    IrohApproved {
        endpoint_id: String,
    },
    IrohRevoked {
        endpoint_id: String,
    },
    /// Where the copy is, in a directory of its own beside the database.
    Snapshotted {
        path: camino::Utf8PathBuf,
    },
    Pr {
        output: String,
        data: Vec<u8>,
        is_error: bool,
    },
}
