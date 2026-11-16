//! How rho spends the user's attention.
//!
//! Everything in the user's world has an [`Address`]: a Slack thread, a
//! chat thread, a note. Most are never stored; a source (Slack, the chat)
//! says they exist and what they have done. What the user did about them,
//! what they named them, and the notes they wrote,
//! is kept in the ledger as facts ([`facts`]). [`marks`] reads them into
//! what each address comes to.
//!
//! [`rank`] reads what the sources claim and the user's facts as they were
//! said, and deals the hand; nothing about ranking is stored. The curves
//! are tuned by hand, so their constants all sit in [`curve`].

pub mod address;
pub mod curve;
pub mod facts;
pub mod marks;
pub mod rank;
pub mod until;

pub use address::{Address, Addressed};
pub use marks::Marks;
pub use rank::{Card, Claim, Hand, Skips, rank};
pub use until::Until;
