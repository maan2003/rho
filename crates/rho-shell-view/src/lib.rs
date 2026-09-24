//! Comint-style editor surface for a daemon-owned shell.
//!
//! The multibuffer keeps a read-only projection of daemon-owned structured
//! shell state beside the writable pending command. State deltas update the
//! projection without disturbing a draft while commands run.
//!
//! [`protocol`] is what a client and a host say about shells; the host and
//! the workset runtime use it alone, without the `client` feature.

pub mod protocol;

#[cfg(feature = "client")]
pub mod channel;
#[cfg(feature = "client")]
mod surface;

#[cfg(feature = "client")]
pub use surface::*;
