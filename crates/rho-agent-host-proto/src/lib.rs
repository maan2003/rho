//! The host and desk parts of how a client and an agent host talk. The
//! plumbing every part shares is `rho_rpc::parts`.

use senax_encoder::{Decode, Encode, Pack, Unpack};

pub mod desk;
pub mod host;
pub mod realtime;

/// Maximum encoded GUI performance snapshot accepted by the daemon.
pub const MAX_GUI_TELEMETRY_BYTES: usize = 8 * 1024 * 1024;

/// The answer to [`host::Open::GitProvide`].
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub struct DesktopSession {
    pub agent: String,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use rho_rpc::parts::{Open, Part, PartOpen};

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
    }

    #[test]
    fn git_provider_frames_round_trip() {
        round_trips(host::GitProviderFrame::Done { request_id: 9 });
        round_trips(GitProvided::Done);
    }

    /// A part's opening survives the envelope, and reads as no other part.
    fn opens_as<T: PartOpen + PartialEq>(open: T) {
        let envelope = Open::of(&open).unwrap();
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
        opens_as(desk::Open);
    }
}
