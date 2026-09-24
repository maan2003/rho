//! The agents, as the client holds them.
//!
//! A transcript is the fold of an agent's log with the host's live tail on
//! its end, and this crate owns every step of that: the host frames that
//! carry it, the fold on its own thread ([`model`]) and the disk copy
//! ([`cache`]). What crosses out is agents as they now stand; nothing above
//! needs to know there is a fold underneath. Nothing here draws: the
//! screens are `rho-agents-view`'s.

pub mod cache;
pub mod create;
pub mod elision;
pub mod find;
pub mod fold;
pub mod map;
pub mod model;
pub mod remote;
pub mod session;
pub mod state;
pub mod store;
pub mod stream;
pub mod usage;

pub use create::{StartBase, StartFieldMode};
pub use find::AgentHit;
pub use fold::{
    AgentIdentity, Attention, AttentionFacts, DIGEST_VERSION, Digest, MirroredAgent,
    TranscriptFold, Verdict, Wants, attention, one_line, transcript,
};
pub use map::{AgentFacts, AgentFiling, AgentLife, AgentMap};
pub use rho_hosts::HostId;

/// Now, in Unix milliseconds, saturating rather than panicking on a clock
/// that says something impossible.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}
