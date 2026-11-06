//! One projection for live input and replay, from the agent's visible rows.
//! Each RequestSent picks up every input before it and `compose` splits them
//! by role ([DECISION-roles](../../../specs/DECISION-roles.md)). Consecutive
//! requests are merged until a response closes the group; provider pairing is
//! derived from the preceding opaque Carry.

use std::sync::Arc;

use rho_agent_types::{ContentPart, Role};

use crate::entry::{Entry, RequestNotice, compose, output_content, plain_text};
use crate::inference::{CacheKey, Carry, Image, Item, Request};
use crate::record::Event;

#[derive(Default)]
pub(crate) struct Context {
    /// Inputs no request has picked up yet.
    queued: Vec<(Role, Vec<ContentPart>)>,
    pending: Option<Carry>,
    notices: Vec<RequestNotice>,
    /// Picked-up `tool` inputs. They wait for a call to answer: only a tool
    /// result reads as a tool's.
    tool: Vec<Vec<ContentPart>>,
    /// Picked-up `user` and `system` inputs.
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
            Event::Input { content, role, .. } => self.queued.push((*role, content.clone())),
            Event::Native { entry, .. } => self.observe_entry(entry),
            _ => {}
        }
    }

    fn open_call(&self) -> Option<&Carry> {
        self.pending.as_ref().filter(|carry| carry.has_call())
    }

    fn observe_entry(&mut self, entry: &Entry) {
        match entry {
            Entry::Step { carry, .. } => {
                if self.open_call().is_some() {
                    self.tool.clear();
                }
                self.notices.clear();
                self.pending = Some(carry.clone());
                self.delivered.clear();
                self.compact = false;
            }
            Entry::RequestSent {
                report, compact, ..
            } => {
                if !report.notebook.is_empty() {
                    self.tool.push(output_content(&report.notebook));
                }
                for (role, content) in self.queued.drain(..) {
                    match role {
                        Role::Tool => self.tool.push(content),
                        Role::User | Role::System => self.delivered.push(content),
                    }
                }
                for notice in &report.notices {
                    if !self.notices.contains(notice) {
                        self.notices.push(notice.clone());
                    }
                }
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
        let call = self.open_call();
        let composed = compose(call.is_some(), &self.notices, &self.tool, &self.delivered);
        let mut items = Vec::new();
        if let Some(call) = call
            && !composed.result.is_empty()
        {
            items.push(Item::Report {
                text: plain_text(&composed.result),
                images: images_of(&composed.result),
                reply_to: call.clone(),
            });
        }
        items.extend(composed.user.iter().map(|content| Item::User {
            text: rho_agent_types::transcript::text_content(content),
            images: images_of(content),
        }));
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

fn images_of(content: &[ContentPart]) -> Vec<Image> {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image { media_type, data } => Some(Image {
                media_type: media_type.clone(),
                data: data.clone(),
            }),
            ContentPart::Text { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentId, AgentIdDomain};

    use super::*;
    use crate::entry::{Notice, Report, WAITING, Wake};
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
            message: None,
            content: vec![ContentPart::Text { text: text.into() }],
            role: Role::User,
        }
    }
    /// A request as old rows have it, with the output in its report.
    fn attempt(text: &str, bytes: Vec<u8>) -> Event<'static> {
        native(Entry::RequestSent {
            why: Wake::Notify,
            compact: false,
            report: Report {
                notices: Vec::new(),
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
        let mut context =
            Context::restore(&[response("start"), received("first"), received("second")]);
        assert!(
            users(&context.input()).is_empty(),
            "nothing is delivered before a request"
        );
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

    fn input_as(role: Role, text: &str) -> Event<'static> {
        Event::Input {
            agent: agent(),
            message: None,
            content: vec![ContentPart::Text { text: text.into() }],
            role,
        }
    }
    fn sent(notebook: &str, notices: Vec<RequestNotice>) -> Event<'static> {
        native(Entry::RequestSent {
            why: Wake::Notify,
            compact: false,
            report: Report {
                notices,
                notebook: if notebook.is_empty() {
                    rho_notebook::Report::default()
                } else {
                    rho_notebook::Report::from_text(notebook.into(), vec![])
                },
            },
        })
    }
    fn prose() -> Event<'static> {
        native(Entry::Step {
            exec: None,
            prose: "words".into(),
            carry: crate::inference::Carry::new(serde_json::json!({}), vec![], false),
            usage: None,
        })
    }
    fn results(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::Report { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Each role reaches the model where that role reads: an answer holds
    /// `tool` text and no person's, and the person's text holds none of it.
    #[test]
    fn with_a_call_open_each_role_goes_where_it_reads() {
        let events = [
            response("call"),
            input_as(Role::Tool, "[message from peer]\nfrom the peer"),
            input_as(Role::User, "from the person"),
            input_as(Role::System, "from rho"),
            sent("printed", vec![]),
        ];
        let items = Context::restore(&events).input();
        let answer = results(&items);
        assert_eq!(answer.len(), 1);
        assert!(answer[0].contains("printed") && answer[0].contains("from the peer"));
        assert!(!answer[0].contains("from the person") && !answer[0].contains("from rho"));
        assert_eq!(users(&items), ["from the person", "from rho"]);
    }

    /// With no call to answer, the model's user turn says only that something
    /// waits. The text goes out in the answer to the first call after it, and
    /// once.
    #[test]
    fn with_no_call_open_tool_text_waits_for_the_next_call() {
        let mut events = vec![
            input_as(Role::Tool, "the parent's brief"),
            input_as(Role::User, "from the person"),
            sent("", vec![]),
        ];
        let first = Context::restore(&events).input();
        assert!(results(&first).is_empty());
        assert_eq!(users(&first), [WAITING, "from the person"]);

        // Prose has no call either: more text waits, the notice goes out once.
        events.push(prose());
        events.push(sent(
            "printed",
            vec![RequestNotice::PreviousResponseHadNoExec],
        ));
        let second = Context::restore(&events).input();
        assert!(results(&second).is_empty());
        let told = users(&second);
        assert_eq!(told.len(), 1);
        assert!(told[0].contains("no exec call") && told[0].contains(WAITING));
        assert!(!told[0].contains("brief") && !told[0].contains("printed"));

        events.push(response("call"));
        let mut context = Context::restore(&events);
        context.observe(&sent("later", vec![]));
        let answer = results(&context.input());
        assert_eq!(answer.len(), 1);
        for text in ["printed", "later", "the parent's brief"] {
            assert!(answer[0].contains(text), "{text} in {}", answer[0]);
        }
        assert!(!answer[0].contains("no exec call"));
        assert!(users(&context.input()).is_empty());

        context.observe(&response("next"));
        context.observe(&sent("", vec![RequestNotice::Checkin]));
        assert_eq!(results(&context.input()), ["Check-in: nothing new."]);

        // The whole conversation projects to the same delivery.
        let request = request("system".into(), &events, CacheKey::from_u128(9));
        let all = users(request.items());
        assert!(all.iter().all(|text| !text.contains("brief")), "{all:?}");
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
            reply_to: carry,
        } = &request.items()[1]
        else {
            panic!("one combined report");
        };
        assert_eq!(carry.display_calls()[0].display_id(), "first");
        assert_eq!(text, "alpha\n\nbeta");
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
                    reply_to: carry,
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
        // The compaction took the calls with it, so the output waits for the
        // next one.
        assert_eq!(built.items().len(), 3);
        let told = users(&built.items()[1..]);
        assert!(told[0].contains(WAITING), "{told:?}");
        assert_eq!(told[1], "queued-during-compaction");
    }
}
