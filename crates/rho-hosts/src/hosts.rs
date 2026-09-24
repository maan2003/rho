//! The attached daemons. Each host is one [`Connection`] plus the liveness
//! the chrome reports; all of them feed a single tagged event stream, so the
//! workspace handles one ordered sequence instead of polling per host.
//!
//! Hosts are addressed by [`HostId`], assigned here in attachment order.
//! Agent ids are already unique across machines, so the id is
//! for routing — which socket a command goes down — not for disambiguation.

use std::sync::Arc;
use std::time::Duration;

use camino::Utf8PathBuf;

use crate::connection::Connection;
use crate::{AttachTarget, HostId, HostSink, HostStream};

/// Where a host is in its connection lifecycle. Only `Online` accepts
/// commands; the rest exist so the chrome can say which host is unwell
/// without implying the others are.
#[derive(Clone, Debug, PartialEq)]
pub enum HostStatus {
    /// Dialing, or connected but still waiting for the first `Ready`.
    Connecting,
    Online,
    /// The transport has gone quiet but the same connection may still
    /// recover.
    Recovering(Duration),
    Disconnected(String),
}

impl HostStatus {
    pub fn is_online(&self) -> bool {
        matches!(self, Self::Online | Self::Recovering(_))
    }

    /// One word for listings, where the host's name carries the subject.
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "connecting".to_owned(),
            Self::Online => "online".to_owned(),
            Self::Recovering(elapsed) => format!("recovering {}s", elapsed.as_secs()),
            Self::Disconnected(reason) => format!("disconnected · {reason}"),
        }
    }
}

pub struct Host {
    pub id: HostId,
    /// The short user-facing name: what the config called it, and what
    /// qualifies its agents' labels once more than one host is attached.
    pub name: String,
    pub target: AttachTarget,
    pub status: HostStatus,
    connection: Connection,
}

impl Host {
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

/// Every attached daemon, in attachment order, and the shared event stream
/// they write to.
/// A working directory on a specific daemon. Two machines can both offer
/// `/home/you/src/rho`, so a bare path never identifies a project once more
/// than one host is attached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPath {
    pub host: HostId,
    pub path: Utf8PathBuf,
}

/// A workdir one daemon offers, under the name its store gave it. What
/// makes it a project is a fact in the store, which is not this crate's;
/// what this crate knows is which machine it is on and what to call it.
#[derive(Clone, Debug)]
pub struct HostWorkdir {
    pub host: HostId,
    pub name: String,
    pub path: Utf8PathBuf,
}

pub struct Hosts {
    hosts: Vec<Host>,
    next_id: u32,
    events: Arc<dyn HostSink>,
    /// Registered workdirs from every attached daemon. Fed by whoever reads
    /// the store; named and qualified here, because what a workdir is
    /// called depends on how many machines are attached.
    workdirs: Vec<HostWorkdir>,
}

impl Hosts {
    /// Nothing is attached yet. Every connection's control events go to
    /// `events`, whatever the reader behind it makes of them.
    pub fn new(events: Arc<dyn HostSink>) -> Self {
        Self {
            hosts: Vec::new(),
            next_id: 0,
            events,
            workdirs: Vec::new(),
        }
    }

    /// Dials a daemon and starts feeding its events into the shared stream.
    /// Attaching is fire-and-forget: the host appears immediately as
    /// `Connecting` and reports its own progress through the stream.
    /// `streams` is handed the new id and the host's [`crate::Link`], and
    /// returns the streams the host carries, opened again on every
    /// reconnect.
    pub fn attach(
        &mut self,
        name: String,
        target: AttachTarget,
        streams: impl FnOnce(HostId, crate::Link) -> Vec<Arc<dyn HostStream>>,
        runtime: &tokio::runtime::Handle,
    ) -> HostId {
        let id = HostId(self.next_id);
        self.next_id += 1;
        let connection = crate::connection::spawn(
            id,
            target.clone(),
            self.events.clone(),
            |link| streams(id, link),
            runtime,
        );
        self.hosts.push(Host {
            id,
            name,
            target,
            status: HostStatus::Connecting,
            connection,
        });
        id
    }

    /// Drops a host and tears its connection down. Surfaces and transcripts
    /// belonging to it are the workspace's to clean up.
    pub fn detach(&mut self, host: HostId) -> Option<Host> {
        let index = self.hosts.iter().position(|entry| entry.id == host)?;
        self.workdirs.retain(|workdir| workdir.host != host);
        Some(self.hosts.remove(index))
    }

    pub fn get(&self, host: HostId) -> Option<&Host> {
        self.hosts.iter().find(|entry| entry.id == host)
    }

    pub fn get_mut(&mut self, host: HostId) -> Option<&mut Host> {
        self.hosts.iter_mut().find(|entry| entry.id == host)
    }

    pub fn by_name(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|entry| entry.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Host> {
        self.hosts.iter()
    }

    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// The host commands fall back to when nothing in the UI names one: the
    /// first attached host that is answering, else the first attached.
    pub fn primary(&self) -> Option<HostId> {
        self.hosts
            .iter()
            .find(|host| host.status.is_online())
            .or_else(|| self.hosts.first())
            .map(|host| host.id)
    }

    /// The host that owns a thing with exactly one home, a Slack unit above
    /// all: the first host configured, answering or not. Unlike `primary`
    /// this never moves when a host goes quiet, so a unit's cells stay on
    /// one host instead of splitting across two, and a write made while the
    /// owner is away is refused where the user can see it.
    pub fn owner(&self) -> Option<HostId> {
        self.hosts.first().map(|host| host.id)
    }

    pub fn connection(&self, host: HostId) -> Option<&Connection> {
        self.get(host).map(Host::connection)
    }

    pub fn is_online(&self, host: HostId) -> bool {
        self.get(host).is_some_and(|entry| entry.status.is_online())
    }

    /// Any host answering at all: the weakest precondition, for actions that
    /// pick their own host later.
    pub fn any_online(&self) -> bool {
        self.hosts.iter().any(|host| host.status.is_online())
    }

    /// Every host id, in the order they were added.
    pub fn ids(&self) -> Vec<HostId> {
        self.hosts.iter().map(|host| host.id).collect()
    }

    pub fn set_status(&mut self, host: HostId, status: HostStatus) {
        if let Some(entry) = self.get_mut(host) {
            entry.status = status;
        }
    }

    /// The status line for the bottom strip: the unhealthiest host, with its
    /// name when there is more than one to tell apart.
    pub fn worst_status(&self) -> Option<(String, HostStatus)> {
        let rank = |status: &HostStatus| match status {
            HostStatus::Disconnected(_) => 3,
            HostStatus::Recovering(_) => 2,
            HostStatus::Connecting => 1,
            HostStatus::Online => 0,
        };
        let host = self
            .hosts
            .iter()
            .filter(|host| rank(&host.status) > 0)
            .max_by_key(|host| rank(&host.status))?;
        Some((host.name.clone(), host.status.clone()))
    }

    /// What this host is called where a name has to say which machine.
    pub fn host_label(&self, host: HostId) -> String {
        self.get(host)
            .map(|entry| entry.name.clone())
            .unwrap_or_default()
    }

    /// Qualifies a daemon-side name with its host, but only when there is
    /// more than one host for it to be confused with.
    pub fn qualify(&self, host: HostId, name: &str) -> String {
        if self.len() > 1 {
            format!("{}/{name}", self.host_label(host))
        } else {
            name.to_owned()
        }
    }

    /// The workdirs one daemon offers. Whoever reads the store hands them
    /// over; what they are called once more than one machine is attached is
    /// this crate's answer.
    pub fn set_workdirs(&mut self, host: HostId, workdirs: Vec<(String, Utf8PathBuf)>) {
        self.workdirs.retain(|workdir| workdir.host != host);
        self.workdirs
            .extend(
                workdirs
                    .into_iter()
                    .map(|(name, path)| HostWorkdir { host, name, path }),
            );
    }

    pub fn workdirs(&self) -> &[HostWorkdir] {
        &self.workdirs
    }

    pub fn workdir_label(&self, workdir: &HostPath) -> String {
        let name = self
            .workdirs
            .iter()
            .find(|candidate| candidate.host == workdir.host && candidate.path == workdir.path)
            .map(|candidate| candidate.name.clone());
        match name {
            Some(name) => self.qualify(workdir.host, &name),
            None if self.len() > 1 => {
                format!("{}:{}", self.host_label(workdir.host), workdir.path)
            }
            None => workdir.path.to_string(),
        }
    }

    /// Registered workdirs as the `(name, description)` table the shared
    /// command layer expects. Names carry their host once more than one is
    /// attached, since two machines can register the same project name.
    pub fn workdir_table(&self) -> Vec<(String, String)> {
        self.workdirs
            .iter()
            .map(|workdir| {
                (
                    self.qualify(workdir.host, &workdir.name),
                    workdir.path.to_string(),
                )
            })
            .collect()
    }

    /// A workdir argument that names a registered project, if exactly one
    /// does. An ambiguous bare name resolves to nothing rather than to a
    /// guess about which machine was meant.
    pub fn registered_workdir(&self, argument: &str) -> Option<HostPath> {
        let workdir = |candidate: &HostWorkdir| HostPath {
            host: candidate.host,
            path: candidate.path.clone(),
        };
        if let Some(exact) = self.workdirs.iter().find(|candidate| {
            self.qualify(candidate.host, &candidate.name) == argument || candidate.path == argument
        }) {
            return Some(workdir(exact));
        }
        let mut bare = self
            .workdirs
            .iter()
            .filter(|candidate| candidate.name == argument);
        let first = bare.next()?;
        bare.next().is_none().then(|| workdir(first))
    }
}
