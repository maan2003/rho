//! The files a daemon-owned workspace shows.
//!
//! [`protocol`] is what a client and a host say over a workspace file
//! channel; the host serves it from this crate's protocol alone, without
//! the `client` feature. [`zed_remote`] opens buffers over it: the daemon
//! owns disk IO, the GUI owns unsaved edits.

pub mod protocol;

#[cfg(feature = "client")]
pub mod channel;
#[cfg(feature = "client")]
pub mod zed_remote;

#[cfg(feature = "client")]
pub use zed_remote::*;

#[cfg(feature = "client")]
gpui::actions!(rho_files, [FileSave]);
