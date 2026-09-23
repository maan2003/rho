//! The files a daemon-owned workspace shows.
//!
//! [`zed_remote`] opens buffers over rho's bounded file protocol: the daemon
//! owns disk IO, the GUI owns unsaved edits.

pub mod zed_remote;

pub use zed_remote::*;

gpui::actions!(rho_files, [FileSave]);
