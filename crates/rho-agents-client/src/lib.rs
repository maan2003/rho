//! The agents, as the client holds them.
//!
//! A transcript is the fold of an agent's log with the host's live tail on
//! its end, and this crate owns every step of that: the host frames that
//! carry it, the fold on its own thread ([`model`]) and the disk copy
//! ([`cache`]). What crosses out is agents as they now stand; nothing above
//! needs to know there is a fold underneath. Nothing here draws: the
//! screens are `rho-agents-view`'s.
//!
//! [`protocol`] is what a client and a host say about agents; the host uses
//! it alone, without the `client` feature.

pub mod protocol;

#[cfg(feature = "client")]
pub mod cache;
#[cfg(feature = "client")]
pub mod create;
#[cfg(feature = "client")]
pub mod elision;
#[cfg(feature = "client")]
pub mod find;
#[cfg(feature = "client")]
pub mod fold;
#[cfg(feature = "client")]
pub mod map;
#[cfg(feature = "client")]
pub mod model;
#[cfg(feature = "client")]
pub mod quota;
#[cfg(feature = "client")]
pub mod remote;
#[cfg(feature = "client")]
pub mod session;
#[cfg(feature = "client")]
pub mod state;
#[cfg(feature = "client")]
pub mod store;
#[cfg(feature = "client")]
pub mod stream;
#[cfg(feature = "client")]
pub mod usage;

#[cfg(feature = "client")]
pub use create::{StartBase, StartFieldMode};
#[cfg(feature = "client")]
pub use find::AgentHit;
#[cfg(feature = "client")]
pub use fold::{
    AgentIdentity, Attention, AttentionFacts, DIGEST_VERSION, Digest, MirroredAgent,
    TranscriptFold, Verdict, Wants, attention, one_line, transcript,
};
#[cfg(feature = "client")]
pub use map::{AgentFacts, AgentFiling, AgentLife, AgentMap};
#[cfg(feature = "client")]
pub use rho_hosts::HostId;

/// Now, in Unix milliseconds, saturating rather than panicking on a clock
/// that says something impossible.
#[cfg(feature = "client")]
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}
