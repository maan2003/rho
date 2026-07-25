//! Mail from peer agents.

use rho_core::{AgentId, ContentPart, ContextBlock, MessageSender, UnixMs};

use crate::preview::{MailPreview, PendingItem, PreviewData, text_of};
use crate::source::SourceKind;

/// One sender's mail. Several messages from the same sender collapse into a
/// single block, so a chatty peer costs one request rather than five.
#[derive(Clone, Debug, PartialEq)]
pub struct MailSource {
    sender: AgentId,
    items: Vec<MailItem>,
}

#[derive(Clone, Debug, PartialEq)]
struct MailItem {
    content: Vec<ContentPart>,
    at: UnixMs,
}

impl MailSource {
    pub fn new(sender: AgentId) -> Self {
        Self {
            sender,
            items: Vec::new(),
        }
    }

    pub fn push(&mut self, content: Vec<ContentPart>, at: UnixMs) {
        self.items.push(MailItem { content, at });
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn source(&self) -> SourceKind {
        SourceKind::Mail {
            oldest_at: self.items.first().map(|item| item.at),
            newest_at: self.items.last().map(|item| item.at),
        }
    }

    pub(crate) fn take(&mut self) -> Option<ContextBlock> {
        (!self.items.is_empty()).then(|| ContextBlock::UserMessage {
            sender: MessageSender::Agent { id: self.sender },
            content: std::mem::take(&mut self.items)
                .into_iter()
                .flat_map(|item| item.content)
                .collect(),
        })
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }

    pub(crate) fn preview(&self) -> Box<dyn PreviewData> {
        Box::new(MailPreview {
            sender: self.sender,
            items: self
                .items
                .iter()
                .map(|item| PendingItem {
                    at: item.at,
                    text: text_of(&item.content),
                })
                .collect(),
        })
    }
}
