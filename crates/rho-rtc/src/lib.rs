//! Native realtime audio sessions.
//!
//! This crate owns WebRTC and local audio devices. OpenAI provider
//! control traffic is handled out-of-band by `rho-openai-realtime`.
//!
//! [`protocol`] is what a client and a host say about voice; the host uses
//! it alone, without the `client` feature.

pub mod protocol;

#[cfg(feature = "client")]
mod session;

#[cfg(feature = "client")]
pub use session::*;
