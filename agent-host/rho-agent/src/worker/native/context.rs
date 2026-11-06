//! One projection for live input and replay, from the agent's visible rows.
//! Each `Request` holds what it carried and `compose` splits it by role
//! ([DECISION-roles](../../../specs/DECISION-roles.md)). Consecutive requests
//! are merged until a response closes the group; provider pairing is derived
//! from the preceding opaque Carry.

use std::sync::Arc;

use rho_agent_types::ContentPart;

use crate::entry::{Entry, RequestNotice, add_output, compose, plain_text};
use crate::inference::{CacheKey, Carry, Image, Item, Request};
use crate::record::{Event, Input};

#[derive(Default)]
pub(crate) struct Context {
    pending: Option<Carry>,
    notices: Vec<RequestNotice>,
    output: Vec<ContentPart>,
    inputs: Vec<Input>,
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
            Event::Request {
                output,
                inputs,
                notices,
                compact,
                ..
            } => {
                add_output(&mut self.output, output.clone());
                self.inputs.extend(inputs.iter().cloned());
                for notice in notices {
                    if !self.notices.contains(notice) {
                        self.notices.push(notice.clone());
                    }
                }
                self.compact |= compact;
            }
            Event::Native {
                entry: Entry::Step { carry, .. },
                ..
            } => {
                self.notices.clear();
                self.pending = Some(carry.clone());
                self.output.clear();
                self.inputs.clear();
                self.compact = false;
            }
            _ => {}
        }
    }

    fn open_call(&self) -> Option<&Carry> {
        self.pending.as_ref().filter(|carry| carry.has_call())
    }

    /// The newest step left a call for a request to answer.
    pub fn call_open(&self) -> bool {
        self.open_call().is_some()
    }

    /// Snapshot the unanswered input group, not the whole conversation.
    pub fn input(&self) -> Vec<Item> {
        let call = self.open_call();
        let composed = compose(call.is_some(), &self.notices, &self.output, &self.inputs);
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
    use rho_agent_types::{AgentId, AgentIdDomain, Role};

    use super::*;
    use crate::entry::{Notice, Wake};
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
    fn image_part(bytes: Vec<u8>) -> ContentPart {
        ContentPart::Image {
            media_type: "image/png".into(),
            data: bytes,
        }
    }
    fn output(text: &str, bytes: Vec<u8>) -> Vec<ContentPart> {
        let mut content = vec![ContentPart::Text { text: text.into() }];
        if !bytes.is_empty() {
            content.push(image_part(bytes));
        }
        content
    }
    fn input_as(role: Role, text: &str) -> Input {
        Input {
            role,
            message: None,
            content: vec![ContentPart::Text { text: text.into() }],
        }
    }
    fn person(text: &str) -> Input {
        input_as(Role::User, text)
    }
    fn carrying(
        output: Vec<ContentPart>,
        inputs: Vec<Input>,
        notices: Vec<RequestNotice>,
    ) -> Event<'static> {
        Event::Request {
            agent: agent(),
            why: Wake::Notify,
            output,
            inputs,
            notices,
            compact: false,
        }
    }
    fn sent(notices: Vec<RequestNotice>) -> Event<'static> {
        carrying(Vec::new(), Vec::new(), notices)
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

    /// What a request carried stays until a response closes the group, and
    /// the request after it carries only its own.
    #[test]
    fn a_response_closes_what_a_request_carried() {
        let mut context = Context::restore(&[response("start")]);
        assert!(users(&context.input()).is_empty());
        context.observe(&carrying(
            Vec::new(),
            vec![person("first"), person("second")],
            vec![],
        ));
        assert_eq!(users(&context.input()), ["first", "second"]);
        context.observe(&response("next"));
        assert!(users(&context.input()).is_empty());
        context.observe(&carrying(Vec::new(), vec![person("late")], vec![]));
        assert_eq!(users(&context.input()), ["late"]);
        context.observe(&response("done"));
        context.observe(&sent(vec![]));
        assert!(users(&context.input()).is_empty());
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
            carrying(
                output("printed", vec![]),
                vec![
                    input_as(Role::Tool, "[message from peer]\nfrom the peer"),
                    input_as(Role::User, "from the person"),
                    input_as(Role::System, "from rho"),
                ],
                vec![RequestNotice::Restarted],
            ),
        ];
        let items = Context::restore(&events).input();
        let answer = results(&items);
        assert_eq!(answer.len(), 1);
        assert_eq!(answer[0], "printed\n\n[message from peer]\nfrom the peer");
        let told = users(&items);
        assert_eq!(told.len(), 3);
        assert!(told[0].starts_with("rho restarted."), "{told:?}");
        assert_eq!(told[1..], ["from the person", "from rho"]);
    }

    /// A check-in with an open call answers it and opens no user text. With
    /// no call open, the same notice is user text and answers nothing.
    #[test]
    fn a_notice_of_nothing_goes_where_the_model_can_read_it() {
        let open = Context::restore(&[response("call"), sent(vec![RequestNotice::Checkin])]);
        assert_eq!(results(&open.input()), ["Check-in: nothing new."]);
        assert!(users(&open.input()).is_empty());

        let closed = Context::restore(&[prose(), sent(vec![RequestNotice::Checkin])]);
        assert!(results(&closed.input()).is_empty());
        assert_eq!(users(&closed.input()), ["Check-in: nothing new."]);
    }

    /// Only old rows have output or a `tool` input with no call to answer. The
    /// model read those as user text, and a replay stays true to that.
    #[test]
    fn an_old_tool_input_with_no_call_replays_as_it_was_read() {
        let events = [
            prose(),
            carrying(
                output("old output", vec![]),
                vec![input_as(Role::Tool, "old peer message")],
                vec![],
            ),
        ];
        let items = Context::restore(&events).input();
        assert!(results(&items).is_empty());
        assert_eq!(users(&items), ["old output", "old peer message"]);
    }

    #[test]
    fn failed_attempts_merge_once_and_keep_new_inputs_and_images() {
        let events = vec![
            response("first"),
            carrying(
                output("alpha", vec![1, 7]),
                vec![person("steer-left")],
                vec![RequestNotice::Restarted],
            ),
            native(Entry::Notice {
                notice: Notice::Error("retry".into()),
            }),
            carrying(
                output("beta", vec![9, 2, 8]),
                vec![person("steer-right")],
                vec![RequestNotice::Restarted],
            ),
        ];
        let request = request("system".into(), &events, CacheKey::from_u128(1));
        assert_eq!(request.items().len(), 5);
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
        let told = users(&request.items()[2..]);
        assert_eq!(told[0].matches("rho restarted.").count(), 1);
        assert_eq!(told[1..], ["steer-left", "steer-right"]);

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
            carrying(output("before-partial", vec![]), vec![], vec![]),
            response("partial"),
            carrying(output("after-partial", vec![]), vec![], vec![]),
        ];
        let built = request("system".into(), &events, CacheKey::from_u128(2));
        let found = built
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
        assert_eq!(
            found,
            [
                ("first".to_owned(), "before-partial"),
                ("partial".to_owned(), "after-partial")
            ]
        );

        events.push(native(Entry::Step {
            exec: None,
            prose: String::new(),
            usage: None,
            carry: Carry::new(serde_json::json!("script-compaction"), vec![], true),
        }));
        events.push(carrying(
            Vec::new(),
            vec![person("after-compaction")],
            vec![RequestNotice::Waiting],
        ));
        let built = request("system".into(), &events, CacheKey::from_u128(2));
        // The compaction took the calls with it: what follows opens no result.
        assert_eq!(built.items().len(), 3);
        let told = users(&built.items()[1..]);
        assert!(told[0].contains("are waiting"), "{told:?}");
        assert_eq!(told[1], "after-compaction");
    }
}
