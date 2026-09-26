//! The model's context: the log, rendered into a request.
//!
//! A step's call is answered by the report of the wake after it; messages
//! delivered at that wake follow as their own user items. Nothing else in
//! the log reaches the model: it wrote its messages and status itself, in
//! code it can already see.

use std::collections::HashMap;
use std::sync::Arc;

use rho_inference2::{CacheKey, Item, Request};

use crate::log::{Block, Entry, MessageId, Party};

pub fn request(instructions: Arc<str>, entries: &[Entry], cache_key: CacheKey) -> Request {
    let mut texts: HashMap<MessageId, String> = HashMap::new();
    let mut messages: HashMap<MessageId, (Party, Vec<Block>)> = HashMap::new();
    for entry in entries {
        match entry {
            Entry::Received { id, from, body, .. } => {
                messages.insert(*id, (from.clone(), body.clone()));
            }
            Entry::Sent { id, text, .. } => {
                texts.insert(*id, text.clone());
            }
            _ => {}
        }
    }
    for (id, (_, body)) in &messages {
        let plain = body
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                Block::Quote { .. } => None,
            })
            .collect::<String>();
        texts.insert(*id, plain);
    }

    let mut items = Vec::new();
    let mut open_call = None;
    for entry in entries {
        match entry {
            Entry::Step { call, carry, .. } => {
                items.push(Item::Step(carry.clone()));
                open_call = call.as_ref().map(|call| call.id.clone());
            }
            Entry::Woken {
                report,
                images,
                messages: delivered,
                ..
            } => {
                let text = report.clone();
                let images = images.clone();
                items.push(match open_call.take() {
                    Some(call_id) => Item::Result {
                        call_id,
                        text,
                        images,
                    },
                    None => Item::User { text, images },
                });
                for id in delivered {
                    if let Some((from, body)) = messages.get(id) {
                        items.push(Item::User {
                            text: render_message(from, body, &texts),
                            images: Vec::new(),
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

/// A message as the model reads it: who wrote it, then the body, with each
/// quote shown as the text it quotes.
pub fn render_message(from: &Party, body: &[Block], texts: &HashMap<MessageId, String>) -> String {
    let mut out = match from {
        Party::Human => "Message from the human:\n".to_owned(),
        Party::Agent(id) => format!("Message from agent {id}:\n"),
    };
    for block in body {
        match block {
            Block::Text(text) => out.push_str(text),
            Block::Quote { of, start, end } => {
                let quoted = texts
                    .get(of)
                    .and_then(|text| text.get(*start as usize..*end as usize))
                    .unwrap_or("[quoted text unavailable]");
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                for line in quoted.lines() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    out
}
