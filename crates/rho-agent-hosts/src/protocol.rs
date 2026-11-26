//! The machine itself, [`rho_rpc::protocol::Protocol::Host`]: Git transport and
//! one-shot administration ([`rho_rpc::protocol::Call`]).

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// What a host stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// This GUI holds SSH credentials and carries Git transport for the
    /// host's Git remote helpers. The host pushes [`GitProviderFrame`]s for
    /// as long as the stream is open, starting with every request that
    /// already waits.
    GitProvider,
    /// One [`rho_rpc::protocol::Call`], answered with one
    /// [`rho_rpc::protocol::Answer`]; then the stream closes.
    Request(Request),
    /// A Git remote helper's transport, paired with a GUI that provides
    /// it. After [`rho_rpc::protocol::Opened::Ready`] the stream is raw Git
    /// data.
    GitTransport { request: GitTransportRequest },
    /// A GUI's answer to [`GitProviderFrame::Requested`]: the user's word.
    /// Answered with [`GitProvided`]; after `Ready` the stream is raw Git
    /// data.
    GitProvide {
        request_id: u64,
        provider_id: u64,
        decision: GitDecision,
    },
}

/// What the user said to a request. The first answer from any GUI is the
/// answer: an approval makes that GUI the one that carries the transport,
/// and a denial refuses the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum GitDecision {
    Approve,
    Deny,
}

/// What a host pushes to a Git transport provider ([`Open::GitProvider`]).
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum GitProviderFrame {
    /// A Git remote helper wants a transport, and waits for the user for as
    /// long as it takes. Sent to every provider, and again to each that
    /// registers while it waits; one answers with [`Open::GitProvide`].
    Requested {
        request_id: u64,
        provider_id: u64,
        request: GitTransportRequest,
        /// When the helper asked, in milliseconds since the Unix epoch on
        /// the host's clock.
        requested_at_ms: u64,
    },
    /// The request no longer waits: answered, or its Git went away.
    /// Deliberately carries no result or winner information.
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
        /// Answered with every device the host trusts for good.
        IrohDevices(IrohDevices) -> Vec<IrohDevice>;
        /// Answered with a `rho://` pairing link, good for one client for
        /// ten minutes.
        IrohPair(IrohPair) -> String;
        /// Answered with where the copy is, in a directory of its own
        /// beside the database; the copy is the caller's to delete.
        Snapshot(Snapshot) -> camino::Utf8PathBuf;
        /// Answered with the ids of the savepoints dropped.
        ForgetSavepoints(ForgetSavepoints) -> Vec<u64>;
        /// Bytes before and after compaction of the open host database.
        CompactDatabase(CompactDatabase) -> (u64, u64);
        /// Answered with a file holding the report, in a directory of its
        /// own beside the database; it is the caller's to delete. A report
        /// can outgrow a frame.
        DebugReport(DebugReport) -> camino::Utf8PathBuf;
        GccpList(GccpList) -> GccpState;
        GccpApprove(GccpApprove) -> ();
        GccpRemove(GccpRemove) -> ();
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

/// Mints a pairing link: the client that opens it is trusted persistently,
/// as `IrohApprove` would trust it.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct IrohPair;

/// Revokes what an iroh endpoint was approved as: a client's trust, with
/// every connection it has open, or a GCCP host's approval with every
/// acceptance of an address on it.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct IrohRevoke {
    pub endpoint_id: String,
}

/// Lists the devices the host trusts for good, each by its key: those an
/// `IrohApprove` or a pairing link trusted. `IrohRevoke` takes one back.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct IrohDevices;

/// A device the host trusts, as [`IrohDevices`] lists it.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct IrohDevice {
    /// Its endpoint id: 52 of z-base-32.
    pub endpoint_id: String,
    /// When the host came to trust it, in seconds since the Unix epoch on
    /// the host's clock.
    pub trusted_at_unix_secs: u64,
    /// The call came over this key: it is the device that asks.
    pub asking: bool,
}

/// Lists whom the host talks to over GCCP: the harnesses the user
/// approved, and the addresses on them that agents accepted.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GccpList;

/// What [`GccpList`] is answered with.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GccpState {
    /// The host's own endpoint id, which another harness's owner approves;
    /// `None` when the host has no iroh endpoint and so speaks no GCCP.
    pub endpoint: Option<String>,
    pub hosts: Vec<GccpHost>,
    pub contacts: Vec<GccpContact>,
}

/// A harness the user approved.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GccpHost {
    /// Its endpoint id: 52 of z-base-32.
    pub endpoint: String,
    /// What the user named it.
    pub name: String,
}

/// One accepted GCCP address.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GccpContact {
    /// The agent that accepted it, by its id.
    pub agent: String,
    /// `<endpoint id>/<local id>`.
    pub address: String,
    /// What the agent named it.
    pub name: String,
}

/// Approves the harness at an endpoint as a GCCP host, under a name: the
/// host's agents may accept addresses on it, and it may open GCCP
/// connections. It is not a client of the host and gets nothing an
/// `IrohApprove`d one does. Approving it again renames it. `IrohRevoke`
/// takes the approval back.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GccpApprove {
    pub endpoint: String,
    pub name: String,
}

/// Takes back an agent's acceptance of a GCCP address: nothing more crosses
/// between the two.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GccpRemove {
    pub agent: String,
    pub address: String,
}

/// Copies the agent host's database for inspection, as of its latest commit
/// and ready to open without repair.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Snapshot;

/// Drops the savepoints the agent host's migrations recorded, once they are
/// verified: in its own write, so the agent host keeps running.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ForgetSavepoints;

/// Reclaim unused pages while the host remains online. Database access
/// pauses while pages move; recovery savepoints must be removed explicitly.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct CompactDatabase;

/// A read-only `rho debug` report, rendered from the agent host's live
/// database: copying a large database costs more than reading it.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum DebugReport {
    Agents,
    Context,
    Transcript { agent: String, max_chars: u64 },
}

/// Maximum encoded GUI performance snapshot accepted by the agent host.
pub const MAX_GUI_TELEMETRY_BYTES: usize = 8 * 1024 * 1024;

/// The answer to [`Open::GitProvide`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum GitProvided {
    /// This GUI carries the transport: raw Git data follows.
    Ready,
    /// The answer is taken: a denial, or a request that no longer waits.
    /// Deliberately carries no result or winner.
    Done,
}

/// The most commits a [`GitPush`] lists; [`GitPush::commit_count`] counts
/// them all.
pub const MAX_PUSH_COMMITS: usize = 50;
/// The most diff text a [`GitPush`] carries, over all its [`GitDiff`]s.
pub const MAX_PUSH_DIFF_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct GitTransportRequest {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub repository: String,
    pub intent: GitIntent,
    /// The agent whose git asks, as the helper found it in
    /// `$RHO_AGENT_ID`; `None` outside an agent's shell. Agents are not
    /// sandboxed, so this names the asker for the user; it proves nothing.
    pub agent: Option<rho_agent_types::AgentId>,
}

/// What the helper asks to do in the repository.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum GitIntent {
    /// Read it. Which refs a fetch wants goes over the transport after
    /// the answer, so the request cannot say.
    Fetch,
    Push(GitPush),
}

/// A push as the agent's repository plans it. Everything but `updates`
/// is what that repository says, for the user to read; it proves nothing.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct GitPush {
    /// Every ref it changes. A window carries the push only when Git's
    /// receive-pack commands set exactly these refs to exactly these
    /// objects.
    pub updates: Vec<GitRefUpdate>,
    /// The commits it carries that the remote lacks, as far as the agent's
    /// repository knows, newest first: at most [`MAX_PUSH_COMMITS`].
    pub commits: Vec<GitCommit>,
    /// How many such commits there are in all.
    pub commit_count: u64,
    /// What each update that changes files changes, in order.
    pub diffs: Vec<GitDiff>,
    /// Whether `diffs` stop short: their text is cut at
    /// [`MAX_PUSH_DIFF_BYTES`].
    pub diff_cut: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct GitRefUpdate {
    pub reference: String,
    pub change: GitRefChange,
}

/// What a push does to one ref. `old` is the ref on the remote as the
/// agent's repository last saw it; the remote may have moved since.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum GitRefChange {
    /// The remote has no such ref.
    Create { new: String },
    /// `new` contains `old`.
    FastForward { old: String, new: String },
    /// `new` does not contain `old`: the ref loses `dropped` commits, or
    /// an unknown number when the agent's repository lacks `old`.
    Force {
        old: String,
        new: String,
        dropped: Option<u64>,
    },
    /// `old` is `None` when the agent's repository has not seen the ref
    /// on the remote.
    Delete { old: Option<String> },
}

impl GitRefChange {
    /// The object the ref names after the push; `None` once deleted.
    pub fn new_object(&self) -> Option<&str> {
        match self {
            Self::Create { new } | Self::FastForward { new, .. } | Self::Force { new, .. } => {
                Some(new)
            }
            Self::Delete { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct GitCommit {
    /// The abbreviated id, as `git log` prints it.
    pub short_id: String,
    pub author: String,
    /// The author's date, in seconds since the Unix epoch.
    pub authored_at: i64,
    pub subject: String,
}

/// What one update changes: `git diff --stat --summary --patch` from what
/// the remote has to the new object.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct GitDiff {
    pub reference: String,
    pub text: String,
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
            GitTransportPolicy {
                host: "github.com".to_owned(),
            }
            .into(),
            GuiTelemetryUpload {
                snapshot: br#"{"version":1}"#.to_vec(),
            }
            .into(),
            Snapshot.into(),
            ForgetSavepoints.into(),
            CompactDatabase.into(),
            DebugReport::Transcript {
                agent: "00img8eiae3n".to_owned(),
                max_chars: 200,
            }
            .into(),
        ] {
            round_trips(Open::Request(request));
        }
    }

    #[test]
    fn git_provider_frames_round_trip() {
        round_trips(GitProviderFrame::Done { request_id: 9 });
        round_trips(GitProviderFrame::Requested {
            request_id: 9,
            provider_id: 4,
            request: GitTransportRequest {
                host: "github.com".to_owned(),
                port: 22,
                user: "git".to_owned(),
                repository: "acme/repo".to_owned(),
                intent: GitIntent::Fetch,
                agent: None,
            },
            requested_at_ms: 1_764_000_000_000,
        });
        round_trips(GitProvided::Done);
    }

    /// A protocol's opening survives the envelope, and reads as no other
    /// protocol.
    fn opens_as<T: ProtocolOpen + PartialEq>(open: T) {
        let envelope = protocol::Open::of(&open).unwrap();
        assert_eq!(envelope.unpack::<T>().unwrap(), open);
        let other = protocol::Open {
            protocol: Protocol::Shell,
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
            intent: GitIntent::Push(GitPush {
                updates: vec![
                    GitRefUpdate {
                        reference: "refs/heads/main".to_owned(),
                        change: GitRefChange::Force {
                            old: "a".repeat(40),
                            new: "b".repeat(40),
                            dropped: Some(2),
                        },
                    },
                    GitRefUpdate {
                        reference: "refs/tags/v1".to_owned(),
                        change: GitRefChange::Delete { old: None },
                    },
                ],
                commits: vec![GitCommit {
                    short_id: "bbbbbbb".to_owned(),
                    author: "Ada".to_owned(),
                    authored_at: 1_764_000_000,
                    subject: "Fix the clock".to_owned(),
                }],
                commit_count: 3,
                diffs: vec![GitDiff {
                    reference: "refs/heads/main".to_owned(),
                    text: " clock.rs | 2 +-\n".to_owned(),
                }],
                diff_cut: true,
            }),
            agent: None,
        };
        opens_as(Open::GitProvider);
        opens_as(Open::GitTransport { request });
        for decision in [GitDecision::Approve, GitDecision::Deny] {
            opens_as(Open::GitProvide {
                request_id: 9,
                provider_id: 4,
                decision,
            });
        }
    }
}
