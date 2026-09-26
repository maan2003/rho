//! rho-agent2: an agent that is a chat participant, not a turn-taker.
//!
//! The model answers every wake with one cell of Python and speaks only
//! through `human.send`. The host keeps one append-only [`log`] per agent;
//! the model's context ([`context`]) and the chat the GUI syncs ([`chat`])
//! are both projections of it. What wakes the model is [`wake`]'s decision.

pub mod agent;
pub mod chat;
pub mod context;
pub mod human;
pub mod log;
pub mod prompt;
pub mod wake;

pub use agent::{Agent, AgentHandle, Config, Inbound, Trace};

#[cfg(test)]
mod tests;
