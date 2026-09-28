//! One projection for live input and replay. Each RequestSent records only new
//! contributions. Consecutive requests are merged until a response closes the
//! group; provider pairing is derived from the preceding opaque Carry.

use std::sync::Arc;

use crate::entry::{Block, Entry, MessageId, Party, Report};
use crate::inference::{CacheKey, Carry, Image, Item, Request};

#[derive(Default)]
pub(crate) struct Context {
    messages: Vec<(MessageId, Party, Vec<Block>)>,
    pending: Option<Carry>,
    report: Report,
    delivered: Vec<(Party, Vec<Block>)>,
    compact: bool,
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
                self.messages.push((*id, *from, body.clone()));
            }
            Entry::Step { carry, .. } => {
                self.pending = Some(carry.clone());
                self.report = Report::default();
                self.delivered.clear();
                self.compact = false;
            }
            Entry::RequestSent {
                report, compact, ..
            } => {
                for id in &report.messages {
                    let index = self
                        .messages
                        .iter()
                        .position(|(message, _, _)| message == id)
                        .expect("send references a queued message");
                    let (_, from, body) = self.messages.remove(index);
                    self.delivered.push((from, body));
                }
                self.messages
                    .retain(|(id, _, _)| !report.acknowledged.contains(id));
                self.report.merge(report.clone());
                self.compact |= compact;
            }
            _ => {}
        }
    }

    /// Snapshot the unanswered input group, not the whole conversation.
    pub fn input(&self) -> Vec<Item> {
        let rendered = self.report.render();
        let images: Vec<Image> = rendered
            .images
            .into_iter()
            .map(|image| Image {
                media_type: image.media_type,
                data: image.data,
            })
            .collect();
        let mut items = Vec::new();
        if !rendered.text.is_empty() || !images.is_empty() {
            items.push(Item::Report {
                text: rendered.text,
                images,
                reply_to: self.pending.clone(),
            });
        }
        items.extend(self.delivered.iter().map(message_input));
        if self.compact {
            items.push(Item::CompactionTrigger);
        }
        items
    }
}

pub(crate) fn request(instructions: Arc<str>, entries: &[Entry], cache_key: CacheKey) -> Request {
    let mut context = Context::default();
    let mut items = Vec::new();
    for entry in entries {
        if let Entry::Step { carry, exec, .. } = entry {
            // Compaction is an instruction for the current attempt, not a
            // historical conversation item to request again on every replay.
            items.extend(
                context
                    .input()
                    .into_iter()
                    .filter(|item| !matches!(item, Item::CompactionTrigger)),
            );
            if carry.has_compaction() {
                items.clear();
            }
            items.push(Item::Step {
                carry: carry.clone(),
                exec: exec.clone(),
            });
        }
        context.observe(entry);
    }
    items.extend(context.input());
    Request::new(instructions, items, cache_key)
}

fn message_input((from, body): &(Party, Vec<Block>)) -> Item {
    Item::User {
        text: render_message(from, body),
        images: body
            .iter()
            .filter_map(|block| match block {
                Block::Image(image) => Some(image.clone()),
                Block::Text(_) => None,
            })
            .collect(),
    }
}

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

    use super::*;
    use crate::entry::{Notice, RequestNotice, Wake};
    use crate::inference::Call;

    fn response(id: &str) -> Entry {
        Entry::Step {
            at: UnixMs(1),
            exec: Some("pass".into()),
            prose: String::new(),
            carry: crate::worker::native::scripted::carry(Call::new(id, "pass".into())),
            usage: None,
        }
    }
    fn received(id: u64, text: &str) -> Entry {
        Entry::Received {
            at: UnixMs(id),
            id: MessageId(id),
            from: Party::Human,
            body: vec![Block::Text(text.into())],
        }
    }
    fn attempt(text: &str, messages: Vec<MessageId>, bytes: Vec<u8>) -> Entry {
        Entry::RequestSent {
            at: UnixMs(9),
            why: Wake::Notify,
            compact: false,
            imported: None,
            report: Report {
                notices: vec![RequestNotice::Restarted],
                notebook: rho_notebook::Report::from_text(
                    text.into(),
                    if bytes.is_empty() {
                        vec![]
                    } else {
                        vec![rho_notebook::Image {
                            media_type: "image/png".into(),
                            data: bytes,
                        }]
                    },
                ),
                messages,
                acknowledged: vec![],
            },
        }
    }

    #[test]
    fn delivery_preserves_order_acknowledgment_and_deferred_messages() {
        let mut entries = vec![
            received(1, "first"),
            received(2, "acknowledged"),
            received(3, "third"),
            received(4, "still-pending"),
        ];
        let mut sent = attempt("old", vec![MessageId(3), MessageId(1)], vec![]);
        if let Entry::RequestSent { report, .. } = &mut sent {
            report.acknowledged.push(MessageId(2));
        }
        entries.push(sent);
        let mut context = Context::restore(&entries);
        let messages = |context: &Context| {
            context
                .input()
                .into_iter()
                .filter_map(|item| match item {
                    Item::User { text, .. } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            messages(&context),
            [
                "Message from the human:\nthird",
                "Message from the human:\nfirst",
            ]
        );
        context.observe(&response("next"));
        context.observe(&attempt("new", vec![MessageId(4)], vec![]));
        assert_eq!(
            messages(&context),
            ["Message from the human:\nstill-pending"]
        );
        context.observe(&response("done"));
        context.observe(&attempt("empty", vec![], vec![]));
        assert!(messages(&context).is_empty());
    }

    #[test]
    fn failed_attempts_merge_once_and_keep_new_messages_and_images() {
        let entries = vec![
            response("first"),
            received(3, "steer-left"),
            attempt("alpha", vec![MessageId(3)], vec![1, 7]),
            Entry::Notice {
                at: UnixMs(10),
                notice: Notice::Error("retry".into()),
            },
            received(4, "steer-right"),
            attempt("beta", vec![MessageId(4)], vec![9, 2, 8]),
        ];
        let request = request("system".into(), &entries, CacheKey::from_u128(1));
        assert_eq!(request.items().len(), 4);
        let Item::Report {
            text,
            images,
            reply_to: Some(carry),
        } = &request.items()[1]
        else {
            panic!("one combined report");
        };
        assert_eq!(carry.display_calls()[0].display_id(), "first");
        assert!(text.ends_with("alpha\n\nbeta"));
        assert_eq!(text.matches("rho restarted.").count(), 1);
        assert_eq!(
            images.iter().map(|i| i.data.clone()).collect::<Vec<_>>(),
            vec![vec![1, 7], vec![9, 2, 8]]
        );
        assert!(
            matches!(&request.items()[2], Item::User {text,..} if text.ends_with("steer-left"))
        );
        assert!(
            matches!(&request.items()[3], Item::User {text,..} if text.ends_with("steer-right"))
        );

        let restored = Context::restore(&entries);
        let rebuilt = Request::continuation(
            "system".into(),
            restored.input(),
            CacheKey::from_u128(1),
            crate::inference::Continuation::new("previous".into()),
        );
        assert!(matches!(&rebuilt.items()[0], Item::Report {text:t,..} if t == text));
    }

    #[test]
    fn partial_step_closes_the_group_and_compaction_drops_old_pairings() {
        let mut entries = vec![
            response("first"),
            attempt("before-partial", vec![], vec![]),
            response("partial"),
            attempt("after-partial", vec![], vec![]),
        ];
        let built = request("system".into(), &entries, CacheKey::from_u128(2));
        let results = built
            .items()
            .iter()
            .filter_map(|i| match i {
                Item::Report {
                    text,
                    reply_to: Some(carry),
                    ..
                } => Some((
                    carry.display_calls()[0].display_id().to_owned(),
                    text.as_str(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "first");
        assert!(results[0].1.ends_with("before-partial"));
        assert_eq!(results[1].0, "partial");
        assert!(results[1].1.ends_with("after-partial"));
        assert!(!results[1].1.contains("before-partial"));

        entries.push(received(22, "queued-during-compaction"));
        entries.push(Entry::Step {
            at: UnixMs(30),
            exec: None,
            prose: String::new(),
            usage: None,
            carry: Carry::new(serde_json::json!("script-compaction"), vec![], true),
        });
        entries.push(attempt("after-compact", vec![MessageId(22)], vec![]));
        let built = request("system".into(), &entries, CacheKey::from_u128(2));
        assert_eq!(built.items().len(), 3);
        assert!(
            matches!(&built.items()[1], Item::Report {text,..} if text.ends_with("after-compact"))
        );
        assert!(
            matches!(&built.items()[2], Item::User {text,..} if text.ends_with("queued-during-compaction"))
        );
    }
}
