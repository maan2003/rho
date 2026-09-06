//! The agents, as the client holds them.
//!
//! Today: the transcripts. A transcript is the fold of an agent's mirror
//! with the daemon's live tail on its end, and this crate owns every step
//! of that — reading the disk copy, folding rows into it, and answering
//! with what moved. What crosses out is a rendered state and a summary of
//! the change; nothing above needs to know there is a fold underneath.
//!
//! The rest of `GUI-CRATES-DESIGN.md`'s `rho-agents` (creation, Find, the
//! screens, the map and its indexes) follows here, one landed change at a
//! time.

pub mod transcript;

pub use transcript::{FrameChange, TranscriptFrame, Transcripts};
