//! The machines this client can reach.
//!
//! One connection per attached daemon, the handshake that brings it up, and
//! the events it produces, fanned out to whoever asked for that kind. The
//! crate holds no agent state, no desk state and no window state: what it
//! knows is which machines exist, whether they are answering, and how to say
//! something to one of them. Everything that has an opinion about what was
//! said goes through here rather than reaching a socket itself.

pub mod connection;
pub mod hosts;
pub mod realtime_client;

pub use connection::{ChannelTask, ConnEvent, Connection, HostEvent, spawn};
pub use hosts::{Host, HostPath, HostStatus, HostWorkdir, Hosts};

/// Which attached daemon. Assigned in attachment order; agent ids are
/// already unique across machines, so this says which socket a command goes
/// down rather than telling two things apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostId(pub u32);

impl std::fmt::Display for HostId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "host{}", self.0)
    }
}

/// How to reach the daemon. Deliberately holds no client-local paths: the
/// socket may be forwarded from another machine, so this client's own cwd
/// and home mean nothing to the daemon and must never leak into agent
/// working directories.
#[derive(Clone)]
pub enum AttachTarget {
    Unix(std::path::PathBuf),
    Iroh {
        endpoint_id: iroh::EndpointId,
        ssh_destination: String,
        remote_rho: String,
    },
}

impl AttachTarget {
    /// How the host reads in chrome and error text.
    pub fn describe(&self) -> String {
        match self {
            Self::Unix(path) => path.display().to_string(),
            Self::Iroh {
                ssh_destination, ..
            } => format!("iroh via {ssh_destination}"),
        }
    }
}

/// One daemon to attach: the short name it is known by in this client, and
/// how to reach it.
#[derive(Clone)]
pub struct HostSpec {
    pub name: String,
    pub target: AttachTarget,
}

impl HostSpec {
    /// Parses the one-line host form used both on the command line and in
    /// the attach prompt: `<name>=unix:<socket>` or
    /// `<name>=iroh:<endpoint-id>@<ssh-destination>`.
    pub fn parse(text: &str, remote_rho: &str) -> Result<Self, String> {
        let (name, target) = text
            .trim()
            .split_once('=')
            .ok_or("expected <name>=unix:<socket> or <name>=iroh:<endpoint-id>@<ssh-dest>")?;
        if name.is_empty() {
            return Err("host name is empty".to_owned());
        }
        let target = match target.split_once(':') {
            Some(("unix", path)) => AttachTarget::Unix(std::path::PathBuf::from(path)),
            Some(("iroh", rest)) => {
                let (endpoint_id, ssh_destination) = rest
                    .split_once('@')
                    .ok_or("iroh targets are <endpoint-id>@<ssh-dest>")?;
                AttachTarget::Iroh {
                    endpoint_id: endpoint_id
                        .parse()
                        .map_err(|error| format!("invalid iroh endpoint id: {error}"))?,
                    ssh_destination: ssh_destination.to_owned(),
                    remote_rho: remote_rho.to_owned(),
                }
            }
            _ => return Err(format!("unknown host target scheme in `{target}`")),
        };
        Ok(Self {
            name: name.to_owned(),
            target,
        })
    }
}

/// Where a host's events go. The crate does not know what a reader makes of
/// them, only that one is listening: this is what keeps the connection from
/// depending on the crates that consume it.
pub trait HostSink: Send + Sync + 'static {
    fn send(&self, event: HostEvent) -> Result<(), ()>;
    /// Whether the reader has gone. A connection that finds nobody
    /// listening stops rather than dialling again for nothing.
    fn is_closed(&self) -> bool;
}
