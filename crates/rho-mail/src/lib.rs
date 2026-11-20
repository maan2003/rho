//! Mail: a forest of messages per host.
//!
//! [`protocol`] is what a client and a host say about the mail; the host
//! uses it alone. With the `client` feature the crate is also the mail as
//! a client holds it: each host's log folded into its forest ([`fold`]),
//! the disk copy a client starts from ([`cache`]), what the user wrote
//! that no host has taken yet ([`outbox`]), the session that carries a
//! host's log in ([`stream`]), and what the mail claims of the user's
//! attention ([`cards`]). Nothing here draws.

pub mod protocol;

#[cfg(feature = "client")]
pub mod cache;
#[cfg(feature = "client")]
pub mod cards;
#[cfg(feature = "client")]
pub mod fold;
#[cfg(feature = "client")]
pub mod outbox;
#[cfg(feature = "client")]
pub mod stream;
#[cfg(feature = "ui")]
pub mod ui;

#[cfg(feature = "client")]
pub use fold::{HostMail, Thread, Told};

/// An agent as an address, `agent:<encoded id>`: what a thread's
/// participants are named by, and what the agents system opens. The id
/// is the host's, unique across hosts, so the address carries no host.
#[cfg(feature = "client")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AgentRef(pub rho_agent_types::AgentId);

/// A thread wherever it is: the log of the host that keeps it, named by
/// that host's iroh identity, which is the same on every device, and the
/// message the thread hangs under. Its text form,
/// `thread:<endpoint>/<id>`, is what addresses and the user's facts say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId {
    pub host: rho_agent_types::EndpointId,
    pub message: rho_agent_types::MessageId,
}

impl ThreadId {
    pub const KIND: &str = "thread";

    /// The thread as text after `thread:`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.host, self.message.0.0)
    }

    pub fn parse(rest: &str) -> Option<Self> {
        let (host, message) = rest.split_once('/')?;
        Some(Self {
            host: host.parse().ok()?,
            message: rho_agent_types::MessageId(rho_agent_types::ChatSeq(message.parse().ok()?)),
        })
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", Self::KIND, self.key())
    }
}

#[cfg(test)]
mod thread_id_tests {
    use super::*;

    #[test]
    fn a_thread_id_reads_its_own_text() {
        let id = ThreadId {
            host: rho_agent_types::EndpointId([0; 32]),
            message: rho_agent_types::MessageId(rho_agent_types::ChatSeq(41)),
        };
        assert_eq!(
            id.to_string(),
            "thread:yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy/41"
        );
        assert_eq!(ThreadId::parse(&id.key()), Some(id));
        assert_eq!(ThreadId::parse("abc"), None);
        assert_eq!(ThreadId::parse("zz/1"), None);
    }
}
