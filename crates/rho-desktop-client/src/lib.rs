#![recursion_limit = "256"]

//! The host's desktops, as a client sees them: which ones there are
//! ([`stream`]) and a live view of one ([`viewer`]). With `ui`, [`ui`] is
//! the desktops as the frame shows them: the surface over one.
//!
//! [`protocol`] is what a client and a host say about desktops; the host
//! and the workset runtime use it alone, without the `client` feature.

pub mod protocol;

#[cfg(feature = "client")]
pub mod stream;
#[cfg(feature = "ui")]
pub mod ui;
#[cfg(feature = "client")]
pub mod viewer;
