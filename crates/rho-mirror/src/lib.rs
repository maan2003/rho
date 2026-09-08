//! The client's mirror of its hosts' agents and the model that reads it.
//!
//! [`mirror`] is the cache: agent events as the client last saw them, in the
//! client's own database, so a cold start has something to show before a host
//! answers. [`model`] turns connection events into the events a screen
//! reacts to, on its own thread, feeding the mirror as it goes.

pub mod desk;
pub mod mirror;
pub mod model;
