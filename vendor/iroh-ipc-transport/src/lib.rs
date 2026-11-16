//! Portable local datagrams as a native iroh QUIC path.
//!
//! macOS uses IPv4 loopback UDP, Linux uses Unix datagrams, and Windows uses
//! local message-mode named pipes. Callers use the same [`LocalDiscovery`] and
//! [`IpcTransport`] API on every platform. QUIC provides authentication, stream
//! multiplexing, reliability and congestion control on each backend.
//!
//! Prepare the transport asynchronously, add it to the iroh builder, and install
//! [`PreferIpcTransport`]. Resolve co-located peers through [`LocalDiscovery`].
//! After `iroh::Endpoint::close().await`, call [`IpcTransport::close`] to release
//! the transport and its discovery lease. Dropping the final handle also
//! schedules cleanup; a process crash leaves an unlocked, undiscoverable record
//! that the next owner can reclaim without probing any socket.

#![deny(missing_docs)]

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
compile_error!("iroh-ipc-transport supports macOS, Linux, and Windows");

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

/// Custom transport identifier shared by the platform backends.
pub const IPC_TRANSPORT_ID: u64 = 0x69_70_63_5f_74_72_00_00;
