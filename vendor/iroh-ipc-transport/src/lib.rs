//! Local datagrams as a native iroh QUIC path.
//!
//! rho's copy of `iroh-ipc-transport`, cut to what rho runs on: Linux Unix
//! datagrams, in a directory private to the user, with every file in it
//! private too. The upstream macOS and Windows backends and the shared
//! directory mode are gone. QUIC provides authentication, stream
//! multiplexing, reliability and congestion control over the socket.
//!
//! Prepare the transport asynchronously, add it to the iroh builder, and install
//! [`PreferIpcTransport`]. Resolve co-located peers through [`LocalDiscovery`].
//! After `iroh::Endpoint::close().await`, call [`IpcTransport::close`] to release
//! the transport and its discovery lease. Dropping the final handle also
//! schedules cleanup; a process crash leaves an unlocked, undiscoverable record
//! that the next owner can reclaim without probing any socket.

#![deny(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!("rho's iroh-ipc-transport is Linux only");

mod backend;
mod directory_access;
mod discovery;
mod endpoint;
mod locking;
mod lookup;
mod selector;
mod transport;

pub use discovery::LocalDiscovery;
pub use selector::PreferIpcTransport;
pub use transport::IpcTransport;

/// Custom transport identifier of the IPC path.
pub const IPC_TRANSPORT_ID: u64 = 0x69_70_63_5f_74_72_00_00;
