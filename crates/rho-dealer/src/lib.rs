//! How rho spends the user's attention.
//!
//! Everything in the user's world is a node: a Slack thread, an agent, a
//! note, a label. Most nodes are never stored; a source (Slack, the
//! agents) says they exist and what they have done. What the user did
//! about them, and how they named and filed them, is kept in the ledger
//! as facts ([`facts`]); notes sync as revisions of their own ([`notes`]).
//! [`marks`] reads both into what each node comes to.
//!
//! [`rank`] reads all of it, the sources as they are and the user's facts
//! as they were said, and deals the hand; nothing about ranking is stored.
//! The curves are tuned by hand, so their constants all sit in [`curve`].

pub mod curve;
pub mod facts;
pub mod marks;
mod node;
pub mod notes;
pub mod rank;
pub mod until;

pub use marks::Marks;
pub use node::{NodeId, SlackUnit};
pub use rank::{Card, CardKind, Hand, Skips, rank};
pub use until::Until;
