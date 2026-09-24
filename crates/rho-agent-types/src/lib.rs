//! The words everything about agents shares: who an agent is, what a
//! message holds, where an agent works, and when things happened. The
//! runtime, the host and the clients all mean the same thing by these, so
//! they live below all of them. Anything shaped for one protocol belongs
//! to that protocol's owner instead.

mod place;
mod vocab;

pub use place::*;
pub use vocab::*;
