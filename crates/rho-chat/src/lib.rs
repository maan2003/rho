//! Chat: a forest of messages per host
//! ([docs/rho2.md](../../../docs/rho2.md)), as a client holds it and as a
//! client and a host say it to each other.
//!
//! [`protocol`] is what a client and a host say about chat; the host uses
//! it alone, without the `client` feature.

pub mod protocol;
