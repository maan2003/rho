//! Mail from peer agents.

use std::collections::BTreeMap;

use rho_core::{AgentId, ContentPart, ContextBlock, MessageSender, UnixMs};

use crate::preview::{PendingItem, Preview, text_of};
use crate::source::SourceKind;

/// Everyone's mail, in arrival order.
///
/// One source rather than one per peer, because the decision reads it as one:
/// the oldest message across every sender is the wait being spent, and the
/// newest across every sender is the burst that might still be going. Splitting
/// by sender and folding it back would compute the same two instants the long
/// way round, and leave an empty queue behind for every peer that ever wrote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MailSource {
    items: Vec<MailItem>,
}

/// Who sent it lives on the message, because that is where it varies.
#[derive(Clone, Debug, PartialEq)]
struct MailItem {
    sender: AgentId,
    content: Vec<ContentPart>,
    at: UnixMs,
}

impl MailSource {
    pub fn push(&mut self, sender: AgentId, content: Vec<ContentPart>, at: UnixMs) {
        self.items.push(MailItem {
            sender,
            content,
            at,
        });
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

    /// One block per sender: several messages from the same peer collapse, so a
    /// chatty one costs the model one block rather than five.
    pub(crate) fn take(&mut self) -> Vec<ContextBlock> {
        let mut by_sender: BTreeMap<AgentId, Vec<ContentPart>> = BTreeMap::new();
        for item in std::mem::take(&mut self.items) {
            by_sender
                .entry(item.sender)
                .or_default()
                .extend(item.content);
        }
        by_sender
            .into_iter()
            .map(|(sender, content)| ContextBlock::UserMessage {
                sender: MessageSender::Agent { id: sender },
                content,
            })
            .collect()
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }

    /// Grouped the same way a drain would group it, so what a UI shows is what
    /// the next request will carry.
    pub(crate) fn previews(&self) -> Vec<Preview> {
        let mut by_sender: BTreeMap<AgentId, Vec<PendingItem>> = BTreeMap::new();
        for item in &self.items {
            by_sender.entry(item.sender).or_default().push(PendingItem {
                at: item.at,
                text: text_of(&item.content),
            });
        }
        by_sender
            .into_iter()
            .map(|(sender, items)| Preview::Mail { sender, items })
            .collect()
    }
}
