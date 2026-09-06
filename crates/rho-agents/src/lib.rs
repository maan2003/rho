//! The agents, as the client holds them.
//!
//! Today: the transcripts and the screens over them. A transcript is the
//! fold of an agent's mirror with the daemon's live tail on its end, and
//! this crate owns every step of that — reading the disk copy, folding rows
//! into it, and answering with what moved. What crosses out is a rendered state
//! and a summary of the change; nothing above needs to know there is a fold
//! underneath.
//!
//! Every screen here is a buffer: the transcript is a multibuffer of
//! per-turn excerpts with the point in it, and the agent screen is that
//! buffer with a prompt buffer under it. They draw with `rho-window`'s
//! primitives and add none of their own.
//!
//! The rest of `GUI-CRATES-DESIGN.md`'s `rho-agents` (the map and its
//! indexes) follows here, one landed change at a time.

pub mod agent_view;
pub mod create;
pub mod find;
pub mod render;
pub mod transcript;

pub use agent_view::{AgentModel, AgentModelEvent};
pub use create::{StartBase, StartFieldMode};
pub use find::AgentHit;
pub use transcript::{FrameChange, TranscriptFrame, Transcripts};
