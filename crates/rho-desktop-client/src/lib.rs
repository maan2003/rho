//! The host's desktops, as a client sees them: which ones there are
//! ([`stream`]) and a live view of one ([`viewer`]).
//!
//! [`protocol`] is what a client and a host say about desktops; the host
//! and the workset runtime use it alone, without the `client` feature.

pub mod protocol;

#[cfg(feature = "client")]
pub mod stream;
#[cfg(feature = "client")]
pub mod viewer;
