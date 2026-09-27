//! The agents' screens.
//!
//! Every screen here is a buffer: conversation shows messages and a prompt,
//! while activity shows read-only execution records. Both are projections
//! of the same transcript, with independent cursors and scroll positions. They
//! draw what `rho-agents-client` holds, with `rho-window`'s primitives, and add
//! none of their own.

pub mod agent_view;
pub mod draft;
pub mod messages;
pub mod render;
pub mod transcript;
mod visualization;

pub use agent_view::{AgentModel, AgentModelEvent};
pub use render::TranscriptView;
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

/// The chip labels for a prompt's image attachments, e.g. `PNG · 12 KB`.
fn attachment_labels(attachments: &[rho_agent_types::ContentPart]) -> Vec<String> {
    use rho_agent_types::ContentPart;
    attachments
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image { media_type, data } => Some(format!(
                "{} · {} KB",
                media_type
                    .strip_prefix("image/")
                    .unwrap_or(media_type)
                    .to_ascii_uppercase(),
                data.len().div_ceil(1024)
            )),
            ContentPart::Text { .. } => None,
        })
        .collect()
}
