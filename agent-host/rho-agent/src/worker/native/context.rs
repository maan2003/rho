//! One projection for live input and replay, from the agent's visible rows.
//! Each RequestSent records only new contributions and delivers every input
//! before it. Consecutive requests are merged until a response closes the
//! group; provider pairing is derived from the preceding opaque Carry.

use std::sync::Arc;

use rho_agent_types::ContentPart;

use crate::entry::{Entry, Report};
use crate::inference::{CacheKey, Carry, Image, Item, Request};
use crate::record::Event;

#[derive(Default)]
pub(crate) struct Context {
    /// Inputs no request has delivered yet.
    queued: Vec<Vec<ContentPart>>,
    pending: Option<Carry>,
    report: Report,
    delivered: Vec<Vec<ContentPart>>,
    compact: bool,
}

impl Context {
    pub fn restore<'a>(events: impl IntoIterator<Item = &'a Event<'a>>) -> Self {
        let mut context = Self::default();
        for event in events {
            context.observe(event);
        }
        context
    }

    pub fn observe(&mut self, event: &Event<'_>) {
        match event {
            Event::Input { content, .. } => self.queued.push(content.clone()),
            Event::Native { entry, .. } => self.observe_entry(entry),
            _ => {}
        }
    }

    fn observe_entry(&mut self, entry: &Entry) {
        match entry {
            Entry::Step { carry, .. } => {
                self.pending = Some(carry.clone());
                self.report = Report::default();
                self.delivered.clear();
                self.compact = false;
            }
            Entry::RequestSent {
                report, compact, ..
            } => {
                self.delivered.append(&mut self.queued);
                self.report.merge(report.clone());
                self.compact |= compact;
            }
            Entry::Notice { .. } | Entry::CompactionTrigger { .. } => {}
        }
    }

    /// Inputs wait for the next request.
    pub fn has_queued(&self) -> bool {
        !self.queued.is_empty()
    }

    /// Snapshot the unanswered input group, not the whole conversation.
    pub fn input(&self) -> Vec<Item> {
        let rendered = self.report.render(!self.delivered.is_empty());
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
        items.extend(self.delivered.iter().map(|content| input(content)));
        if self.compact {
            items.push(Item::CompactionTrigger);
        }
        items
    }
}

pub(crate) fn request<'a>(
    instructions: Arc<str>,
    events: impl IntoIterator<Item = &'a Event<'a>>,
    cache_key: CacheKey,
) -> Request {
    let mut context = Context::default();
    let mut items = Vec::new();
    for event in events {
        if let Event::Native {
            entry: Entry::Step { carry, exec, .. },
            ..
        } = event
        {
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
        context.observe(event);
    }
    items.extend(context.input());
    Request::new(instructions, items, cache_key)
}

/// An input as the model reads it: its text as policy wrote it, and its
/// images.
fn input(content: &[ContentPart]) -> Item {
    Item::User {
        text: rho_agent_types::transcript::text_content(content),
        images: content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Image { media_type, data } => Some(Image {
                    media_type: media_type.clone(),
                    data: data.clone(),
                }),
                ContentPart::Text { .. } => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentId, AgentIdDomain, InputWake};

    use super::*;
    use crate::entry::{Notice, RequestNotice, Wake};
    use crate::inference::Call;

    fn agent() -> AgentId {
        AgentId::from_counter(1, &AgentIdDomain(7)).unwrap()
    }
    fn native(entry: Entry) -> Event<'static> {
        Event::Native {
            agent: agent(),
            entry,
        }
    }
    fn response(id: &str) -> Event<'static> {
        native(Entry::Step {
            exec: Some("pass".into()),
            prose: String::new(),
            carry: crate::worker::native::scripted::carry(Call::new(id, "pass".into())),
            usage: None,
        })
    }
    fn received(text: &str) -> Event<'static> {
        Event::Input {
            agent: agent(),
            mail: None,
            content: vec![ContentPart::Text { text: text.into() }],
            wake: InputWake::Person,
        }
    }
    fn attempt(text: &str, bytes: Vec<u8>) -> Event<'static> {
        native(Entry::RequestSent {
            why: Wake::Notify,
            compact: false,
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
            },
        })
    }
    fn users(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_request_delivers_every_input_before_it_and_none_after() {
        let mut context = Context::restore(&[received("first"), received("second")]);
        assert!(users(&context.input()).is_empty(), "nothing is delivered before a request");
        context.observe(&attempt("old", vec![]));
        assert_eq!(users(&context.input()), ["first", "second"]);
        // An input after the request waits for the next one, even before a
        // response closes the group.
        context.observe(&received("late"));
        assert_eq!(users(&context.input()), ["first", "second"]);
        context.observe(&response("next"));
        assert!(users(&context.input()).is_empty());
        context.observe(&attempt("new", vec![]));
        assert_eq!(users(&context.input()), ["late"]);
        context.observe(&response("done"));
        context.observe(&attempt("empty", vec![]));
        assert!(users(&context.input()).is_empty());
    }

    #[test]
    fn failed_attempts_merge_once_and_keep_new_inputs_and_images() {
        let events = vec![
            response("first"),
            received("steer-left"),
            attempt("alpha", vec![1, 7]),
            native(Entry::Notice {
                notice: Notice::Error("retry".into()),
            }),
            received("steer-right"),
            attempt("beta", vec![9, 2, 8]),
        ];
        let request = request("system".into(), &events, CacheKey::from_u128(1));
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
        assert_eq!(users(&request.items()[2..]), ["steer-left", "steer-right"]);

        let restored = Context::restore(&events);
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
        let mut events = vec![
            response("first"),
            attempt("before-partial", vec![]),
            response("partial"),
            attempt("after-partial", vec![]),
        ];
        let built = request("system".into(), &events, CacheKey::from_u128(2));
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

        events.push(received("queued-during-compaction"));
        events.push(native(Entry::Step {
            exec: None,
            prose: String::new(),
            usage: None,
            carry: Carry::new(serde_json::json!("script-compaction"), vec![], true),
        }));
        events.push(attempt("after-compact", vec![]));
        let built = request("system".into(), &events, CacheKey::from_u128(2));
        assert_eq!(built.items().len(), 3);
        assert!(
            matches!(&built.items()[1], Item::Report {text,..} if text.ends_with("after-compact"))
        );
        assert_eq!(users(&built.items()[2..]), ["queued-during-compaction"]);
    }
}
