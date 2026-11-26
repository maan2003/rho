//! The agents' screens.
//!
//! An agent's screen is a buffer: its transcript, everything it read and
//! did, in order, drawn from what `rho-agents-client` holds with
//! `rho-window`'s primitives. [`system`] is how the agents plug into the
//! frame: the `agent:` address, the surface over the transcript, and the
//! rows that start and steer agents. [`usage`] is what the agents have
//! spent, as charts the system opens from `usage:` addresses. [`git`] is
//! the user's word on an agent's push or fetch that needs their ssh login,
//! each a card the window deals.

pub mod agent_view;
pub mod git;
pub mod render;
pub mod system;
pub mod transcript;
pub mod usage;
mod visualization;

pub use agent_view::{AgentModel, AgentModelEvent};
pub use git::GitApprovals;
pub use system::AgentsSystem;
pub use transcript::{FrameChange, TranscriptFrame, Transcripts};
