//! Non-consuming views of what each source is holding, for UIs.
//!
//! Signals to the core carry no payload, so this is the only way to show
//! pending content before it is pulled. The payload is open the same way
//! provider data is: a shell tool shows a terminal buffer, a search shows match
//! counts, and neither has to be describable as one summary string.
//!
//! Each source has its own preview type carrying its own identity — the sender
//! of a piece of mail, the call a tool is answering — so nothing here needs a
//! separate label that could drift from the data it describes.

use rho_core::{AgentId, ContentPart, ToolCallId, UnixMs};
use senax_encoder::{Decode, Encode};

use crate::tool::{ToolActivity, Unsent};

senax_encoder::declare_senax_tagged_trait!(
    pub trait PreviewData,
    unknown = UnknownPreviewData,
);

/// One input waiting in a queue.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
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

/// Messages the user has typed that the model has not seen yet.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct UserPreview {
    pub items: Vec<PendingItem>,
}

senax_encoder::register_senax_tagged!(
    trait = PreviewData,
    type = UserPreview,
    tag = "rho-agent2.preview.user",
);

/// Mail from one sender, waiting to be collapsed into a single block.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct MailPreview {
    pub sender: AgentId,
    pub items: Vec<PendingItem>,
}

senax_encoder::register_senax_tagged!(
    trait = PreviewData,
    type = MailPreview,
    tag = "rho-agent2.preview.mail",
);

/// Default preview for a tool with nothing richer to show.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ToolPreview {
    pub call_id: ToolCallId,
    pub activity: ToolActivity,
    pub unsent: Unsent,
    pub last_output_at: UnixMs,
}

senax_encoder::register_senax_tagged!(
    trait = PreviewData,
    type = ToolPreview,
    tag = "rho-agent2.preview.tool",
);
