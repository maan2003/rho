//! How rho spends the user's attention.
//!
//! Everything in the user's world is a node: a Slack thread, an agent, a
//! note, a label. Most nodes are never stored; a source (Slack, the
//! agents) says they exist and what they have done. What the user did
//! about them — done, mute, snooze, todo — is kept in the ledger as facts
//! ([`facts`]), and what they called and filed them as marks ([`marks`]).
//!
//! [`rank`] reads all of it, the sources as they are and the user's facts
//! as they were said, and deals the hand; nothing about ranking is stored.
//! The curves are tuned by hand, so their constants all sit in [`curve`].

pub mod curve;
pub mod facts;
pub mod marks;
mod node;
pub mod rank;
pub mod until;

pub use marks::Marks;
pub use node::{NodeId, SlackUnit};
pub use rank::{Card, CardKind, Hand, Skips, rank};
pub use until::Until;
