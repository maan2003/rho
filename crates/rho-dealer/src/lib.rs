//! How rho spends the user's attention.
//!
//! Everything in the user's world is a node: a Slack thread, an agent, a
//! note, a label. Most nodes are never stored; a source (Slack, the
//! agents, the notes) says they exist and whether they want the user. What
//! the user said about them — done, muted, snoozed, a todo date, a label —
//! is kept in the ledger as marks ([`marks`]), which the sources fold into
//! what they report.
//!
//! What a source reports is a [`Want`]: a node asking for the user, the
//! words for why, and the [`Curve`] its pressure follows over time. The
//! [`Dealer`] holds every want and ranks them into cards whenever it is
//! asked; nothing about ranking is stored. The curves are tuned by hand, so
//! their constants all sit in [`curve`].

pub mod curve;
mod dealer;
pub mod marks;
mod node;

pub use curve::{Curve, DateMark};
pub use dealer::{Card, CardKind, Dealer, Want};
pub use marks::Marks;
pub use node::{NodeId, SlackUnit};
