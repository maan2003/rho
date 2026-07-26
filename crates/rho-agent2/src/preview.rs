//! Non-consuming views of what each source is holding, for UIs.
//!
//! Signals to the core carry no payload, so this is the only way to show
//! pending content before it is pulled.
//!
//! Every variant names what it belongs to — the sender of a piece of mail, the
//! call a tool is answering — so a flat list needs no parallel labelling that
//! could drift from the data it describes.

use rho_core::{AgentId, ContentPart, ToolCallId, UnixMs};

use crate::tool::ToolHaste;

/// What one source is holding, as much as a UI can show without taking it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preview {
    /// Messages the user has typed that the model has not seen yet.
    User { items: Vec<PendingItem> },
    /// Mail from one sender, waiting to be collapsed into a single block.
    Mail {
        sender: AgentId,
        items: Vec<PendingItem>,
    },
    /// A call, and how much of a hurry its tool says it is in. Deliberately the
    /// same hint the decision reads and no summary line: a tool describes its
    /// own output better than the core could, and it does that when *asked*, at
    /// a request boundary — not on every repaint.
    Tool {
        call_id: ToolCallId,
        haste: ToolHaste,
    },
}

/// One input waiting in a queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingItem {
    /// When this particular item arrived, so a UI can age each one rather than
    /// only the queue as a whole.
    pub at: UnixMs,
    pub text: String,
}

/// The renderable text of a queued message. A preview is for reading, so
/// anything without a textual form is simply left out.
pub(crate) fn text_of(content: &[ContentPart]) -> String {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
