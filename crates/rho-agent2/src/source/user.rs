//! Messages typed by the user.

use rho_core::{ContextBlock, MessageSender, UnixMs};

use crate::preview::{PendingItem, Preview, text_of};
use crate::source::{Delivery, InputKind, QueuedInput, SourceKind};

/// Discrete, never merged or summarised, and always drained in arrival order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserSource {
    items: Vec<QueuedInput>,
}

impl UserSource {
    pub fn push(&mut self, input: QueuedInput) {
        self.items.push(input);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Items are kept in arrival order, so the first is the one that has been
    /// waiting longest — the one whose patience the core is spending.
    pub(crate) fn oldest_at(&self) -> Option<UnixMs> {
        self.items.first().map(|item| item.at)
    }

    pub(crate) fn source(&self) -> SourceKind {
        SourceKind::User {
            interrupt: self
                .items
                .iter()
                .any(|item| item.delivery == Delivery::Interrupt),
            oldest_at: self.oldest_at(),
        }
    }

    /// Every queued item is eligible at every boundary, so a drain is total.
    ///
    /// Compaction is stable-sorted to the back, because the trigger has to be
    /// the final input item and history would otherwise disagree with the
    /// request it produced: `REQ-provider-transcript-protocol`. Messages keep
    /// their relative order among themselves.
    pub(crate) fn take(&mut self) -> Vec<ContextBlock> {
        let mut items = std::mem::take(&mut self.items);
        items.sort_by_key(|item| matches!(item.kind, InputKind::Compaction));
        items
            .into_iter()
            .map(|item| match item.kind {
                InputKind::Message { content } => ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content,
                },
                InputKind::Compaction => ContextBlock::CompactionTrigger,
            })
            .collect()
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }

    pub(crate) fn preview(&self) -> Preview {
        Preview::User {
            items: self.items.iter().map(pending_item).collect(),
        }
    }
}

fn pending_item(input: &QueuedInput) -> PendingItem {
    let text = match &input.kind {
        InputKind::Message { content } => text_of(content),
        InputKind::Compaction => "/compact".to_owned(),
    };
    PendingItem { at: input.at, text }
}
