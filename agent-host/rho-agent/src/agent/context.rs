//! The model's context: the log, rendered into a request.
//!
//! A step's calls are answered by the report of the wake after it; messages
//! delivered at that wake follow as their own user items. A result whose
//! call is not replayed (an older history kept it only to be read) is left
//! out, and a wake with no result left says its report instead. Nothing else in
//! the log reaches the model: it wrote its messages and status itself, in
//! code it can already see.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rho_inference2::{CacheKey, Item, Request};

use crate::entry::{Block, Entry, MessageId, Party};

/// `entries` is the visible branch, oldest first.
pub(crate) fn request(instructions: Arc<str>, entries: &[Entry], cache_key: CacheKey) -> Request {
    let messages: HashMap<MessageId, (Party, &[Block])> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Received { id, from, body, .. } => Some((*id, (*from, body.as_slice()))),
            _ => None,
        })
        .collect();
    let first = entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Step { carry, .. } if carry.has_compaction()))
        .unwrap_or(0);
    let replayed: HashSet<_> = entries[first..]
        .iter()
        .flat_map(|entry| match entry {
            Entry::Step { carry, .. } => carry.call_ids(),
            _ => Vec::new(),
        })
        .collect();
    let mut items = Vec::new();
    for entry in &entries[first..] {
        match entry {
            Entry::Step { carry, .. } => items.push(Item::Step(carry.clone())),
            Entry::CompactionTrigger { .. } => items.push(Item::CompactionTrigger),
            Entry::Woken {
                report,
                images,
                messages: delivered,
                results,
                ..
            } => {
                let results = results
                    .iter()
                    .filter(|result| replayed.contains(&result.id))
                    .collect::<Vec<_>>();
                if results.is_empty() && (!report.is_empty() || !images.is_empty()) {
                    items.push(Item::User {
                        text: report.clone(),
                        images: images.clone(),
                    });
                } else {
                    for result in results {
                        items.push(Item::Result {
                            call_id: result.id.clone(),
                            text: result.text.clone(),
                            images: result.images.clone(),
                        });
                    }
                }
                for id in delivered {
                    if let Some((from, body)) = messages.get(id) {
                        items.push(Item::User {
                            text: render_message(from, body),
                            images: body
                                .iter()
                                .filter_map(|block| match block {
                                    Block::Image(image) => Some(image.clone()),
                                    Block::Text(_) => None,
                                })
                                .collect(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Request {
        instructions,
        items,
        cache_key,
    }
}

/// A message as the model reads it: who wrote it, then the body.
pub(crate) fn render_message(from: &Party, body: &[Block]) -> String {
    let mut out = match from {
        Party::Human => "Message from the human:\n".to_owned(),
        Party::Agent(id) => format!("Message from agent {}:\n", id.encoded()),
    };
    for block in body {
        if let Block::Text(text) = block {
            out.push_str(text);
        }
    }
    out
}
