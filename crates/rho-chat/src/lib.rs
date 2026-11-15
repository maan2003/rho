//! Chat: a forest of messages per host.
//!
//! [`protocol`] is what a client and a host say about the chat; the host
//! uses it alone. With the `client` feature the crate is also the chat as
//! a client holds it: each host's log folded into its forest ([`fold`]),
//! the disk copy a client starts from ([`cache`]), what the user wrote
//! that no host has taken yet ([`outbox`]), the session that carries a
//! host's log in ([`stream`]), and what the chat claims of the user's
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

#[cfg(feature = "client")]
pub use fold::{HostChat, Thread, Told};

/// A thread wherever it is: the log of the host that keeps it, named by
/// that host's machine seed, which is the same on every device, and the
/// message the thread hangs under. Its text form, `thread:<seed>/<id>`,
/// is what addresses and the user's facts say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId {
    pub machine_seed: u64,
    pub message: rho_agent_types::MessageId,
}

impl ThreadId {
    pub const KIND: &str = "thread";

    /// The thread as text after `thread:`.
    pub fn key(&self) -> String {
        format!("{:016x}/{}", self.machine_seed, self.message.0.0)
    }

    pub fn parse(rest: &str) -> Option<Self> {
        let (seed, message) = rest.split_once('/')?;
        Some(Self {
            machine_seed: u64::from_str_radix(seed, 16).ok()?,
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
            machine_seed: 0xabc,
            message: rho_agent_types::MessageId(rho_agent_types::ChatSeq(41)),
        };
        assert_eq!(id.to_string(), "thread:0000000000000abc/41");
        assert_eq!(ThreadId::parse(&id.key()), Some(id));
        assert_eq!(ThreadId::parse("abc"), None);
        assert_eq!(ThreadId::parse("zz/1"), None);
    }
}
