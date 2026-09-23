//! The agents, as the client holds them.
//!
//! A transcript is the fold of an agent's log with the host's live tail on
//! its end, and this crate owns every step of that: the host frames that
//! carry it, the fold on its own thread ([`model`]), the disk copy
//! ([`cache`]) and the screens over it. What crosses out is agents as they
//! now stand and screens to draw; nothing above needs to know there is a
//! fold underneath.
//!
//! Every screen here is a buffer: the transcript is a multibuffer of
//! per-turn excerpts with the point in it, and the agent screen is that
//! buffer with a prompt buffer under it. They draw with `rho-window`'s
//! primitives and add none of their own.

pub mod agent_view;
pub mod cache;
pub mod create;
pub mod draft;
pub mod find;
pub mod fold;
pub mod map;
pub mod messages;
pub mod model;
pub mod render;
pub mod session;
pub mod state;
pub mod store;
pub mod transcript;
pub mod usage;

pub use agent_view::{AgentModel, AgentModelEvent};
pub use create::{StartBase, StartFieldMode};
pub use find::AgentHit;
pub use fold::{
    AgentIdentity, Attention, AttentionFacts, DIGEST_VERSION, Digest, MirroredAgent,
    TranscriptFold, Verdict, Wants, attention, one_line, transcript,
};
pub use map::{AgentFacts, AgentFiling, AgentLife, AgentMap};
pub use rho_hosts::HostId;

// What the composer answers to. Declared here because the composer is
// here: the fields, what submitting or clearing one means, and what
// cycling a value does are this crate's, and a host that wants them on a
// key binds these rather than inventing its own. (The macro documents each
// action itself, so this cannot be a doc comment.)
gpui::actions!(
    rho_agents,
    [
        DraftFieldSubmit,
        DraftFieldClear,
        DraftValueCycle,
        RoleCycle,
        RoleCycleGroup,
    ]
);

/// Now, in Unix milliseconds, saturating rather than panicking on a clock
/// that says something impossible.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub use transcript::{FrameChange, TranscriptFrame, Transcripts};
