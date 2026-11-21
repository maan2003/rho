//! The words everything about agents shares: who an agent is, what a
//! message holds, where an agent works, and when things happened. The
//! runtime, the host and the clients all mean the same thing by these, so
//! they live below all of them. Anything shaped for one protocol belongs
//! to that protocol's owner instead.

mod activity;
mod chat;
mod gccp;
mod log;
mod place;
mod project;
pub mod transcript;
mod vocab;
mod workset;

pub use activity::*;
pub use chat::*;
pub use gccp::*;
pub use log::*;
pub use place::*;
pub use project::*;
pub use vocab::*;
pub use workset::*;
