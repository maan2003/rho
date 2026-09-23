//! Keeping a client in sync with its agent hosts.
//!
//! [`transcripts`] and [`desk`] are caches: what the client last saw, in its
//! own database, so a cold start has something to show before a host
//! answers. [`model`] turns connection events into the events a screen
//! reacts to, on its own thread, feeding the caches as it goes.

pub mod desk;
pub mod model;
pub mod transcripts;
