//! The attached daemons. Each host is one [`Connection`] plus the liveness
//! the chrome reports; all of them feed a single tagged event stream, so the
//! workspace handles one ordered sequence instead of polling per host.
//!
//! Hosts are addressed by [`HostId`], assigned here in attachment order.
//! Agent ids are already unique across machines, so the id is
//! for routing — which socket a command goes down — not for disambiguation.

use std::time::Duration;

use futures::channel::mpsc as futures_mpsc;
use gpui::App;
use rho_ui_proto::ClientMessage;

use crate::connection::{Connection, HostEvent};
use crate::registry::HostId;
use crate::workspace::AttachTarget;

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
    pub auth: Option<rho_ui_proto::AuthState>,
    connection: Connection,
}

impl Host {
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

/// Every attached daemon, in attachment order, and the shared event stream
/// they write to.
pub struct Hosts {
    hosts: Vec<Host>,
    next_id: u32,
    events: futures_mpsc::UnboundedSender<HostEvent>,
}

impl Hosts {
    /// Opens the shared event stream. Nothing is attached yet; the receiver
    /// stays live for the workspace's lifetime because `Hosts` keeps the
    /// sender.
    pub fn new() -> (Self, futures_mpsc::UnboundedReceiver<HostEvent>) {
        let (events, receiver) = futures_mpsc::unbounded();
        (
            Self {
                hosts: Vec::new(),
                next_id: 0,
                events,
            },
            receiver,
        )
    }

    /// Dials a daemon and starts feeding its events into the shared stream.
    /// Attaching is fire-and-forget: the host appears immediately as
    /// `Connecting` and reports its own progress through the stream.
    pub fn attach(&mut self, name: String, target: AttachTarget, cx: &App) -> HostId {
        let id = HostId(self.next_id);
        self.next_id += 1;
        let connection = crate::connection::spawn(id, target.clone(), self.events.clone(), cx);
        self.hosts.push(Host {
            id,
            name,
            target,
            status: HostStatus::Connecting,
            auth: None,
            connection,
        });
        id
    }

    /// Drops a host and tears its connection down. Surfaces and transcripts
    /// belonging to it are the workspace's to clean up.
    pub fn detach(&mut self, host: HostId) -> Option<Host> {
        let index = self.hosts.iter().position(|entry| entry.id == host)?;
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

    pub fn send(&self, host: HostId, message: ClientMessage) {
        if let Some(connection) = self.connection(host) {
            connection.send(message);
        }
    }

    /// Sends the same command to every attached host. Used only for queries
    /// whose answers the workspace merges, never for mutations.
    pub fn broadcast(&self, message: impl Fn() -> ClientMessage) {
        for host in &self.hosts {
            host.connection.send(message());
        }
    }

    /// Points every host's transport priority at the focused agent: its own
    /// host promotes that stream, the others drop back to background
    /// weights.
    pub fn focus_agent(&self, focused: Option<(HostId, rho_ui_proto::AgentId)>) {
        for host in &self.hosts {
            let agent_id = focused
                .filter(|(owner, _)| *owner == host.id)
                .map(|(_, agent_id)| agent_id);
            host.connection.focus_agent(agent_id);
        }
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
}
