//! The desk, as the client holds it.
//!
//! The desk is the user's: cells and verdicts are made here and the agent
//! hosts only keep and relay them between devices. [`desk`] is the desk
//! itself: the replica of each host, its handshake, the map and the writes.
//! [`cache`] is the client's own copy on disk, so a cold start has a desk
//! before a host answers. [`stream`] is each host's desk stream.
//!
//! [`protocol`] is what a client and a host say about the desk; the host
//! uses it alone, without the `client` feature.

pub mod protocol;

#[cfg(feature = "client")]
pub mod cache;
#[cfg(feature = "client")]
pub mod desk;
#[cfg(feature = "client")]
pub mod stream;

#[cfg(feature = "client")]
pub use desk::Desk;
