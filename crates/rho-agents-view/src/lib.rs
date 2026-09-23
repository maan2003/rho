//! The agents' screens.
//!
//! Every screen here is a buffer: the transcript is a multibuffer of
//! per-turn excerpts with the point in it, and the agent screen is that
//! buffer with a prompt buffer under it. They draw what `rho-agents-client`
//! holds, with `rho-window`'s primitives, and add none of their own.

pub mod agent_view;
pub mod draft;
pub mod messages;
pub mod render;
pub mod transcript;

pub use agent_view::{AgentModel, AgentModelEvent};
pub use transcript::{FrameChange, TranscriptFrame, Transcripts};

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
