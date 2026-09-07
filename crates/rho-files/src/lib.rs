//! The files a daemon-owned workspace shows.
//!
//! [`zed_remote`] opens buffers over rho's bounded file protocol: the daemon
//! owns disk IO, the GUI owns unsaved edits. [`diff_view`] shows a
//! workspace's diff against its base and opens the dirty files in it.

pub mod diff_view;
pub mod zed_remote;

pub use diff_view::*;
pub use zed_remote::*;

gpui::actions!(rho_files, [FileSave]);
