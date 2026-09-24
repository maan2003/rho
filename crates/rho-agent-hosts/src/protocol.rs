//! The machine itself, [`rho_rpc::protocol::Protocol::Host`]: Git transport and
//! one-shot administration ([`rho_rpc::protocol::Call`]).

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// What a host stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// This GUI holds SSH credentials and carries Git transport for the
    /// host's Git remote helpers. The host pushes [`GitProviderFrame`]s for
    /// as long as the stream is open.
    GitProvider,
    /// One [`rho_rpc::protocol::Call`], answered with one
    /// [`rho_rpc::protocol::Answer`]; then the stream closes.
    Request(Request),
    /// A Git remote helper's transport, paired with a GUI that provides
    /// it. After [`rho_rpc::protocol::Opened::Ready`] the stream is raw Git
    /// data.
    GitTransport { request: GitTransportRequest },
    /// A GUI's answer to [`GitProviderFrame::Requested`].
    /// Answered with [`GitProvided`]; after `Ready` the stream is raw
    /// Git data.
    GitProvide {
        request_id: u64,
        provider_id: u64,
        /// Whether this GUI claims the transport after approving the
        /// operation. The first claim selects the credential provider.
        claim: bool,
    },
}

/// What a host pushes to a Git transport provider ([`Open::GitProvider`]).
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum GitProviderFrame {
    /// A Git remote helper wants a transport. Sent to every provider; each
    /// answers with [`Open::GitProvide`].
    Requested {
        request_id: u64,
        provider_id: u64,
        request: GitTransportRequest,
    },
    /// An approval race completed or expired. Deliberately carries no
    /// result or winner information.
    Done { request_id: u64 },
}

rho_rpc::calls! {
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

impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Host;

    fn debug_reply(&self, frame: &[u8]) -> Option<String> {
        match self {
            Self::Request(request) => Some(request.debug_answer(frame)),
            _ => None,
        }
    }
}

/// How the remote helper should reach `host`: PAT-backed GitHub HTTP or
/// client-held SSH.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GitTransportPolicy {
    pub host: String,
}

/// A bounded, client-produced performance snapshot, kept under the
/// agent host's state directory.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GuiTelemetryUpload {
    pub snapshot: Vec<u8>,
}

/// Installs platform secrets into the agent host's RAM-only store.
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

/// Trusts an iroh endpoint in agent host memory. A privileged local-control
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

/// Copies the agent host's database for inspection, as of its latest commit
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

/// Maximum encoded GUI performance snapshot accepted by the agent host.
pub const MAX_GUI_TELEMETRY_BYTES: usize = 8 * 1024 * 1024;

/// The answer to [`Open::GitProvide`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum GitProvided {
    /// This GUI carries the transport: raw Git data follows.
    Ready,
    /// The approval race completed or expired. Deliberately carries no
    /// result or winner.
    Done,
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

#[cfg(test)]
mod tests {
    use rho_rpc::protocol::{self, Protocol, ProtocolOpen};

    use super::*;

    fn round_trips<T>(message: T)
    where
        T: senax_encoder::Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
    {
        let bytes = senax_encoder::pack(&message).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: T = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(message, decoded);
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            Pr {
                agent_id: Some("eng-abcd".into()),
                command: PrCommand::Edit {
                    url: "https://github.com/acme/widgets/pull/1".into(),
                    base: Some("release".into()),
                    title: Some("Better title".into()),
                    body: Some("Better summary".into()),
                },
            }
            .into(),
            GitTransportPolicy {
                host: "github.com".to_owned(),
            }
            .into(),
            GuiTelemetryUpload {
                snapshot: br#"{"version":1}"#.to_vec(),
            }
            .into(),
            Snapshot.into(),
        ] {
            round_trips(Open::Request(request));
        }
    }

    #[test]
    fn git_provider_frames_round_trip() {
        round_trips(GitProviderFrame::Done { request_id: 9 });
        round_trips(GitProvided::Done);
    }

    /// A protocol's opening survives the envelope, and reads as no other
    /// protocol.
    fn opens_as<T: ProtocolOpen + PartialEq>(open: T) {
        let envelope = protocol::Open::of(&open).unwrap();
        assert_eq!(envelope.unpack::<T>().unwrap(), open);
        let other = protocol::Open {
            protocol: Protocol::Agents,
            open: envelope.open.clone(),
        };
        assert!(other.unpack::<T>().is_err());
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
        opens_as(Open::GitProvider);
        opens_as(Open::GitTransport { request });
        opens_as(Open::GitProvide {
            request_id: 9,
            provider_id: 4,
            claim: true,
        });
    }
}
