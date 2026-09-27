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

use rho_inference::step::{CacheKey, Item, Request};

use crate::entry::{Block, Entry, MessageId, Party};

/// Domain inputs waiting for a wake, and the inference-owned active context.
/// Historical Sent/Status/Usage entries never enter the request path.
#[derive(Default)]
pub(crate) struct Context {
    model: rho_inference::step::Context,
    messages: HashMap<MessageId, (Party, Vec<Block>)>,
    replayed: HashSet<rho_inference::step::CallId>,
    pending: Vec<rho_inference::step::CallId>,
}
impl Context {
    pub fn restore(entries: &[Entry]) -> Self {
        let mut context = Self::default();
        for entry in entries {
            context.observe(entry);
        }
        context
    }

    pub fn observe(&mut self, entry: &Entry) {
        match entry {
            Entry::Received { id, from, body, .. } => {
                self.messages.insert(*id, (*from, body.clone()));
            }
            Entry::Step { carry, calls, .. } => {
                self.pending = calls.iter().map(|call| call.id.clone()).collect();
                if carry.has_compaction() {
                    self.replayed.clear();
                }
                self.replayed.extend(carry.call_ids());
                self.model.push(Item::Step(carry.clone()));
            }
            Entry::CompactionTrigger { .. } => self.model.push(Item::CompactionTrigger),
            Entry::Woken {
                report,
                images,
                messages,
                acknowledged,
                results,
                ..
            } => {
                self.pending.clear();
                let mut answered = false;
                for result in results.iter().filter(|r| self.replayed.contains(&r.id)) {
                    answered = true;
                    self.model.push(Item::Result {
                        call_id: result.id.clone(),
                        text: result.text.clone(),
                        images: result.images.clone(),
                    });
                }
                if !answered && (!report.is_empty() || !images.is_empty()) {
                    self.model.push(Item::User {
                        text: report.clone(),
                        images: images.clone(),
                    });
                }
                for id in messages {
                    if let Some((from, body)) = self.messages.remove(id) {
                        self.model.push(Item::User {
                            text: render_message(&from, &body),
                            images: body
                                .into_iter()
                                .filter_map(|block| match block {
                                    Block::Image(image) => Some(image),
                                    Block::Text(_) => None,
                                })
                                .collect(),
                        });
                    }
                }
                for id in acknowledged {
                    self.messages.remove(id);
                }
            }
            _ => {}
        }
    }

    pub fn pending_calls(&self) -> &[rho_inference::step::CallId] {
        &self.pending
    }

    pub fn request(&self, instructions: Arc<str>, cache_key: CacheKey) -> Request {
        self.model.request(instructions, cache_key)
    }
}

/// Offline migration checks build once from the visible branch.
pub(crate) fn request(instructions: Arc<str>, entries: &[Entry], cache_key: CacheKey) -> Request {
    Context::restore(entries).request(instructions, cache_key)
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

#[cfg(test)]
mod tests {
    use rho_agent_types::UnixMs;
    use rho_inference::step::{Call, CallId, Carry, Usage};

    use super::*;
    use crate::entry::{CallResult, Wake};

    #[test]
    fn pending_input_survives_compaction_and_wakes_resolve_it_once() {
        let mut context = Context::default();
        let at = UnixMs(1);
        context.observe(&Entry::Received {
            at,
            id: MessageId(7),
            from: Party::Human,
            body: vec![Block::Text("queued-before-compact".into())],
        });
        let call = Call {
            id: CallId::new("old"),
            code: "pass".into(),
        };
        context.observe(&Entry::Step {
            at,
            calls: vec![call.clone()],
            prose: String::new(),
            carry: Carry::bare(call),
            usage: Usage::default(),
        });
        context.observe(&Entry::Step {
            at,
            calls: vec![],
            prose: String::new(),
            carry: Carry::from_openai_items(vec![
                r#"{"type":"compaction","id":"compact","encrypted_content":"summary"}"#.into(),
            ]),
            usage: Usage::default(),
        });
        context.observe(&Entry::Woken {
            at,
            why: Wake::Message,
            report: "new report".into(),
            images: vec![],
            messages: vec![MessageId(7)],
            acknowledged: vec![],
            results: vec![CallResult {
                id: CallId::new("old"),
                text: "must not replay".into(),
                images: vec![],
            }],
        });
        for n in 0..1000 {
            context.observe(&Entry::Status {
                at,
                text: format!("display-only-{n}"),
            });
        }
        let request = context.request("system".into(), CacheKey::from_u128(1));
        assert_eq!(request.items().len(), 3);
        assert!(matches!(&request.items()[0],Item::Step(carry) if carry.has_compaction()));
        assert!(matches!(&request.items()[1],Item::User{text,..} if text=="new report"));
        assert!(
            matches!(&request.items()[2],Item::User{text,..} if text=="Message from the human:\nqueued-before-compact")
        );
        assert!(context.messages.is_empty());
        assert!(context.pending_calls().is_empty());
    }

    #[test]
    fn acknowledged_input_is_not_kept_in_the_request_index() {
        let mut context = Context::default();
        let at = UnixMs(2);
        context.observe(&Entry::Received {
            at,
            id: MessageId(8),
            from: Party::Human,
            body: vec![Block::Text("already handled".into())],
        });
        context.observe(&Entry::Woken {
            at,
            why: Wake::Message,
            report: String::new(),
            images: vec![],
            messages: vec![],
            acknowledged: vec![MessageId(8)],
            results: vec![],
        });
        assert!(context.messages.is_empty());
        assert!(
            context
                .request("system".into(), CacheKey::from_u128(1))
                .items()
                .is_empty()
        );
    }
}
