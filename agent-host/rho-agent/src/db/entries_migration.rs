//! Temporary one-hop migration (6bcd407c -> 9990d22e): a Rho-runtime
//! agent's native rows become the entries the loop now runs on. Rows are
//! rewritten in place, one for one, so positions, the journal and
//! `Rewound` targets stay valid. Remove once the developer's database has
//! opened with this build.

use std::collections::{HashMap, HashSet, VecDeque};

use rho_agent_types::{AgentId, ContentPart, MessagePhase, UnixMs};
use rho_db::{SenValue, WriteTxn};
use rho_inference::OpenAiResponsesProviderData as Provider;
use rho_inference::types::{ContextBlock, InferenceResponseItem, MessageSender, ToolResult};
use serde_json::json;

use super::{AGENT_HEADS, AGENT_LOG, AgentRuntime, agent_range, fold_head, rows};
use crate::entry::{Block, CallResult, Entry, MessageId, Notice, Party, Wake};
use crate::native::NativeEvent;
use crate::{AgentEvent, InputKind};

pub(super) fn migrate(write: &mut WriteTxn) {
    let rho = write
        .open_table(AGENT_HEADS)
        .iter()
        .filter(|(_, head)| {
            matches!(
                head.value().into_owned().config.runtime,
                AgentRuntime::Rho { .. }
            )
        })
        .map(|(id, _)| id.value())
        .collect::<Vec<_>>();
    for agent_id in rho {
        let unmatched = migrate_agent(write, agent_id);
        // The rewrite folds the same, but the stored head must be the log's.
        let head = {
            let log = write.open_table(AGENT_LOG);
            fold_head(rows(log.range(agent_range(agent_id))))
                .expect("agent log starts with creation")
        };
        write
            .open_table(AGENT_HEADS)
            .insert(&agent_id, SenValue::borrowed(&head));
        check(write, agent_id, unmatched);
    }
}

/// Says what a person checking the migrated log should look at: messages
/// found only in model input, messages the next wake will deliver, and
/// replayed calls nothing answers.
fn check(write: &mut WriteTxn, agent_id: AgentId, unmatched: usize) {
    let entries = {
        let log = write.open_table(AGENT_LOG);
        super::visible_rows(rows(log.range(agent_range(agent_id))))
            .1
            .into_iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Entry(entry) => Some(entry),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let delivered = entries
        .iter()
        .flat_map(|entry| match entry {
            Entry::Woken {
                messages,
                acknowledged,
                ..
            } => messages.iter().chain(acknowledged).copied().collect(),
            _ => Vec::new(),
        })
        .collect::<HashSet<_>>();
    let unread = entries
        .iter()
        .filter(|entry| matches!(entry, Entry::Received { id, .. } if !delivered.contains(id)))
        .count();
    let request =
        crate::agent::context::request("".into(), &entries, rho_inference2::CacheKey::from_u128(0));
    let answered = request
        .items
        .iter()
        .filter_map(|item| match item {
            rho_inference2::Item::Result { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut unanswered = request
        .items
        .iter()
        .flat_map(|item| match item {
            rho_inference2::Item::Step(carry) => carry.call_ids(),
            _ => Vec::new(),
        })
        .filter(|id| !answered.contains(id))
        .collect::<Vec<_>>();
    // The last step's calls are answered by the wake still to come.
    if let Some(Entry::Step { calls, .. }) = entries
        .iter()
        .rev()
        .find(|entry| matches!(entry, Entry::Step { .. } | Entry::Woken { .. }))
    {
        unanswered.retain(|id| !calls.iter().any(|call| call.id == *id));
    }
    if unmatched + unread + unanswered.len() > 0 {
        eprintln!(
            "agent {}: {unmatched} messages only in model input, {unread} unread, \
             unanswered calls {unanswered:?}",
            agent_id.encoded()
        );
    }
}

/// How many delivered messages had no accepted row to become.
fn migrate_agent(write: &mut WriteTxn, agent_id: AgentId) -> usize {
    let window = {
        let log = write.open_table(AGENT_LOG);
        Window::scan(rows(log.range(agent_range(agent_id))))
    };
    let mut log = write.open_table(AGENT_LOG);
    let mut convert = Convert::new(window);
    for pos in convert.window.positions.clone() {
        let event = log
            .get(&(agent_id, pos))
            .expect("row seen by the scan")
            .value()
            .into_owned();
        if let Some(entry) = convert.row(pos, &event) {
            log.insert(
                &(agent_id, pos),
                SenValue::borrowed(&AgentEvent::Entry(entry)),
            );
        }
    }
    convert.unmatched
}

/// Where the old provider's view began: its latest rotation, then its latest
/// compaction after that, over the visible branch's blocks in order.
struct Window {
    positions: Vec<u64>,
    visible: HashSet<u64>,
    /// First visible block index of each visible native row.
    first_block: HashMap<(u64, bool), usize>,
    start: usize,
    evicted: HashSet<String>,
    /// Calls some visible request answered.
    answered: HashSet<String>,
    /// The latest visible response, if no request followed it: the next
    /// wake answers its calls.
    last_response: Option<u64>,
}

impl Window {
    fn scan(all: impl Iterator<Item = (super::AgentEventPos, AgentEvent<'static>)>) -> Self {
        let mut positions = Vec::new();
        let mut visible: Vec<(u64, Option<Native>)> = Vec::new();
        for (pos, event) in all {
            positions.push(pos.pos);
            if let AgentEvent::Rewound { to, .. } = &event {
                visible.retain(|(kept, _)| *kept < to.pos);
            }
            let native = match &event {
                AgentEvent::Native(NativeEvent::RequestStarted { input, .. }) => {
                    Some((false, input.iter().map(Summary::of).collect()))
                }
                AgentEvent::Native(NativeEvent::ResponseFinished { output, .. }) => {
                    Some((true, output.iter().map(Summary::of).collect()))
                }
                // Becomes a wake, so the response before it is not the last.
                AgentEvent::Cleared { .. } => Some((false, Vec::new())),
                _ => None,
            };
            visible.push((pos.pos, native));
        }
        let mut first_block = HashMap::new();
        let mut start = 0;
        let mut evicted = HashSet::new();
        let mut compactions = Vec::new();
        let mut answered = HashSet::new();
        let mut last_response = None;
        let mut index = 0;
        for (pos, native) in &visible {
            let Some((output, blocks)) = native else {
                continue;
            };
            last_response = output.then_some(*pos);
            first_block.insert((*pos, *output), index);
            for block in blocks {
                match block {
                    Summary::Rotation(retain_from) => start = start.max(*retain_from as usize),
                    Summary::Evicted(ids) => evicted.extend(ids.iter().cloned()),
                    Summary::Compaction => compactions.push(index),
                    Summary::Answered(ids) => answered.extend(ids.iter().cloned()),
                    Summary::Other => {}
                }
                index += 1;
            }
        }
        if let Some(compaction) = compactions.into_iter().rfind(|at| *at >= start) {
            start = compaction;
        }
        Self {
            positions,
            visible: visible.into_iter().map(|(pos, _)| pos).collect(),
            first_block,
            start,
            evicted,
            answered,
            last_response,
        }
    }

    /// Whether a visible response's call is left out of replay: evicted by
    /// the old runtime, or cut off with nothing ever answering it.
    fn dropped(&self, pos: u64, call: &str) -> bool {
        self.evicted.contains(call)
            || (Some(pos) != self.last_response && !self.answered.contains(call))
    }

    /// Whether a visible native row's blocks precede the provider's view.
    /// Hidden rows keep everything: they are only ever read.
    fn before_start(&self, pos: u64, output: bool) -> bool {
        self.first_block
            .get(&(pos, output))
            .is_some_and(|first| *first < self.start)
    }
}

/// A visible native row: whether it is a response, and its blocks.
type Native = (bool, Vec<Summary>);

enum Summary {
    Rotation(u64),
    Evicted(Vec<String>),
    Compaction,
    Answered(Vec<String>),
    Other,
}

impl Summary {
    fn of(block: &ContextBlock) -> Self {
        match block {
            ContextBlock::ContextRotation { retain_from } => Self::Rotation(*retain_from),
            ContextBlock::ToolHistoryEvicted { call_ids } => {
                Self::Evicted(call_ids.iter().map(|id| id.as_str().to_owned()).collect())
            }
            ContextBlock::ToolResults { results } => Self::Answered(
                results
                    .iter()
                    .map(|result| result.call_id.as_str().to_owned())
                    .collect(),
            ),
            ContextBlock::InferenceResponse { items, .. }
                if items.iter().any(|item| compaction(item).is_some()) =>
            {
                Self::Compaction
            }
            _ => Self::Other,
        }
    }
}

struct Convert {
    window: Window,
    /// Accepted messages not yet delivered, oldest first.
    pending: VecDeque<(MessageId, MessageSender, Vec<ContentPart>)>,
    /// Calls replayed in the provider's view so far.
    calls: HashSet<String>,
    unmatched: usize,
}

impl Convert {
    fn new(window: Window) -> Self {
        Self {
            window,
            pending: VecDeque::new(),
            calls: HashSet::new(),
            unmatched: 0,
        }
    }

    fn row(&mut self, pos: u64, event: &AgentEvent<'static>) -> Option<Entry> {
        let visible = self.window.visible.contains(&pos);
        Some(match event {
            AgentEvent::Accepted(input) => match &input.kind {
                InputKind::Message { content } => {
                    let id = MessageId(pos);
                    self.pending.push_back((id, input.source, content.clone()));
                    Entry::Received {
                        at: input.at,
                        id,
                        from: party(input.source),
                        body: content.iter().map(block_of).collect(),
                    }
                }
                // A request the provider already answered, or asked for
                // before a later rotation, is spent.
                InputKind::Compaction if visible && self.accepted_before_start(pos) => {
                    Entry::Woken {
                        at: input.at,
                        why: Wake::Compaction,
                        report: String::new(),
                        images: Vec::new(),
                        messages: Vec::new(),
                        acknowledged: Vec::new(),
                        results: Vec::new(),
                    }
                }
                InputKind::Compaction => Entry::CompactionTrigger {
                    at: input.at,
                    manual: true,
                },
            },
            AgentEvent::Cleared { at } => Entry::Woken {
                at: *at,
                why: Wake::Message,
                report: String::new(),
                images: Vec::new(),
                messages: Vec::new(),
                acknowledged: self.pending.drain(..).map(|(id, ..)| id).collect(),
                results: Vec::new(),
            },
            AgentEvent::Rewound { to, .. } => {
                self.pending.retain(|(id, ..)| id.0 < to.pos);
                return None;
            }
            AgentEvent::Failed { error, at, .. } => Entry::Notice {
                at: *at,
                notice: Notice::Error(error.to_string()),
            },
            AgentEvent::Native(NativeEvent::RequestFailed { error, at, .. }) => Entry::Notice {
                at: *at,
                notice: Notice::Error(error.clone()),
            },
            AgentEvent::Native(NativeEvent::RequestStarted { input, at, .. }) => {
                let before = visible && self.window.before_start(pos, false);
                self.request(input, *at, visible, before)
            }
            AgentEvent::Native(NativeEvent::ResponseFinished {
                output, usage, at, ..
            }) => {
                let before = visible && self.window.before_start(pos, true);
                let window = &self.window;
                let (calls, carry, prose) =
                    response(output, &|call| visible && window.dropped(pos, call), pos);
                if !before {
                    self.calls.extend(carry_calls(&carry));
                }
                Entry::Step {
                    at: *at,
                    calls,
                    prose,
                    carry: rho_inference2::Carry::from_openai_items(if before {
                        Vec::new()
                    } else {
                        carry
                    }),
                    usage: usage.as_ref().map_or_else(Default::default, |bucket| {
                        rho_inference2::Usage {
                            input_tokens: bucket.input_tokens,
                            cached_tokens: bucket.cache_read_tokens,
                            output_tokens: bucket.output_tokens,
                        }
                    }),
                }
            }
            _ => return None,
        })
    }

    /// The pending messages one delivered message was made of: the old
    /// runtime joined a sender's queued messages into one, in order.
    fn take(&mut self, sender: MessageSender, content: &[ContentPart]) -> Option<Vec<MessageId>> {
        let mut rest = content;
        let mut taken = Vec::new();
        for (index, (_, from, parts)) in self.pending.iter().enumerate() {
            if rest.is_empty() {
                break;
            }
            if *from == sender && rest.starts_with(parts) {
                rest = &rest[parts.len()..];
                taken.push(index);
            }
        }
        if !rest.is_empty() || taken.is_empty() {
            return None;
        }
        let mut ids = Vec::new();
        for index in taken.into_iter().rev() {
            ids.push(self.pending.remove(index).expect("taken").0);
        }
        ids.reverse();
        Some(ids)
    }

    /// A manual compaction accepted before the provider's view began.
    fn accepted_before_start(&self, pos: u64) -> bool {
        // The first visible native row after it decides.
        self.window
            .positions
            .iter()
            .filter(|later| **later > pos)
            .find_map(|later| {
                [false, true]
                    .into_iter()
                    .find_map(|output| self.window.first_block.get(&(*later, output)))
            })
            .is_some_and(|first| *first < self.window.start)
    }

    /// One request's input as one wake. What the old provider saw beyond
    /// results and messages (updates, harness notes, results for calls it
    /// no longer replayed) rides along as text.
    fn request(
        &mut self,
        input: &[ContextBlock],
        at: UnixMs,
        visible: bool,
        before: bool,
    ) -> Entry {
        let mut messages = Vec::new();
        let mut results: Vec<CallResult> = Vec::new();
        let mut extra = Vec::new();
        let mut why = None;
        for block in input {
            match block {
                ContextBlock::UserMessage { sender, content } => {
                    why.get_or_insert(match sender {
                        MessageSender::User => Wake::Message,
                        MessageSender::Agent { .. } => Wake::AgentMessage,
                    });
                    match self.take(*sender, content) {
                        Some(ids) => messages.extend(ids),
                        None => {
                            self.unmatched += 1;
                            extra.push(crate::agent::context::render_message(
                                &party(*sender),
                                &content.iter().map(block_of).collect::<Vec<_>>(),
                            ))
                        }
                    }
                }
                ContextBlock::ToolResults { results: answered } => {
                    why.get_or_insert(Wake::Returned);
                    for result in answered {
                        let id = result.call_id.as_str();
                        let evicted = visible && self.window.evicted.contains(id);
                        if !visible || before || evicted || self.calls.contains(id) {
                            results.push(call_result(result));
                        } else {
                            extra.push(format!(
                                "Output of earlier exec {id}:\n{}",
                                result.body.output
                            ));
                        }
                    }
                }
                ContextBlock::ToolUpdate(update) => {
                    why.get_or_insert(Wake::Notify);
                    extra.push(format!(
                        "Update from exec {}:\n{}",
                        update.call_id.as_str(),
                        update.output
                    ));
                }
                ContextBlock::DeveloperMessage { text } => extra.push(text.clone()),
                ContextBlock::CompactionTrigger => {
                    why.get_or_insert(Wake::Compaction);
                }
                ContextBlock::InferenceResponse { .. }
                | ContextBlock::ContextRotation { .. }
                | ContextBlock::ToolHistoryEvicted { .. } => {}
            }
        }
        let mut report = String::new();
        if !before && !extra.is_empty() {
            let text = extra.join("\n\n");
            match results
                .iter_mut()
                .find(|result| self.calls.contains(result.id.as_str()))
            {
                Some(result) => {
                    result.text.push_str("\n\n");
                    result.text.push_str(&text);
                }
                None => report = text,
            }
        }
        let (messages, acknowledged) = if before {
            (Vec::new(), messages)
        } else {
            (messages, Vec::new())
        };
        Entry::Woken {
            at,
            why: why.unwrap_or(Wake::Notify),
            report,
            images: Vec::new(),
            messages,
            acknowledged,
            results,
        }
    }
}

fn party(sender: MessageSender) -> Party {
    match sender {
        MessageSender::User => Party::Human,
        MessageSender::Agent { id } => Party::Agent(id),
    }
}

fn block_of(part: &ContentPart) -> Block {
    match part {
        ContentPart::Text { text } => Block::Text(text.clone()),
        ContentPart::Image { media_type, data } => Block::Image(rho_inference2::Image {
            media_type: media_type.clone(),
            data: data.clone(),
        }),
    }
}

fn call_result(result: &ToolResult) -> CallResult {
    CallResult {
        id: rho_inference2::CallId::new(result.call_id.as_str()),
        text: (*result.body.output).clone(),
        images: result
            .body
            .images
            .iter()
            .map(|image| rho_inference2::Image {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            })
            .collect(),
    }
}

fn carry_calls(carry: &[String]) -> Vec<String> {
    carry
        .iter()
        .filter_map(|item| serde_json::from_str::<serde_json::Value>(item).ok())
        .filter(|item| item["type"] == "custom_tool_call")
        .filter_map(|item| item["call_id"].as_str().map(str::to_owned))
        .collect()
}

fn compaction(item: &InferenceResponseItem) -> Option<(&str, &str)> {
    let (InferenceResponseItem::Compaction { provider_specific }
    | InferenceResponseItem::Unknown { provider_specific }) = item
    else {
        return None;
    };
    match provider_specific.as_any().downcast_ref::<Provider>() {
        Some(Provider::Compaction {
            item_id,
            encrypted_content,
        }) if !encrypted_content.is_empty() => Some((item_id.as_str(), encrypted_content)),
        _ => None,
    }
}

/// A response's calls (all of them, for reading), the items the provider
/// replays (normalized as inference2 stores them, dropped calls left out)
/// and its prose.
fn response(
    output: &[ContextBlock],
    dropped: &dyn Fn(&str) -> bool,
    pos: u64,
) -> (Vec<rho_inference2::Call>, Vec<String>, String) {
    let mut calls = Vec::new();
    let mut carry = Vec::new();
    let mut prose = String::new();
    let items = output.iter().flat_map(|block| match block {
        ContextBlock::InferenceResponse { items, .. } => items.as_slice(),
        _ => &[],
    });
    for (index, item) in items.enumerate() {
        let value = match item {
            InferenceResponseItem::AssistantMessage {
                provider_specific,
                content,
                phase,
            } => {
                let text = rho_inference::types::text_content(content);
                prose.push_str(&text);
                let id = match provider_specific.as_any().downcast_ref::<Provider>() {
                    Some(Provider::Message { item_id }) => item_id.as_str().to_owned(),
                    _ => format!("msg_migrated_{pos}_{index}"),
                };
                let mut message = json!({
                    "type": "message",
                    "role": "assistant",
                    "id": id,
                    "content": [{ "type": "output_text", "text": text }],
                });
                if let Some(phase) = phase {
                    message["phase"] = json!(match phase {
                        MessagePhase::Commentary => "commentary",
                        MessagePhase::FinalAnswer => "final_answer",
                    });
                }
                message
            }
            InferenceResponseItem::ToolCall {
                provider_specific,
                id,
                arguments,
                ..
            } => {
                calls.push(rho_inference2::Call {
                    id: rho_inference2::CallId::new(id.as_str()),
                    code: arguments.clone(),
                });
                if dropped(id.as_str()) {
                    continue;
                }
                let item_id = match provider_specific.as_any().downcast_ref::<Provider>() {
                    Some(
                        Provider::FunctionCall { item_id } | Provider::CustomToolCall { item_id },
                    ) => item_id.as_str().to_owned(),
                    _ => format!("ctc_migrated_{pos}_{index}"),
                };
                json!({
                    "type": "custom_tool_call",
                    "id": item_id,
                    "call_id": id.as_str(),
                    "name": rho_inference2::EXEC,
                    "input": arguments,
                })
            }
            InferenceResponseItem::EncryptedReasoning {
                provider_specific,
                summary,
            } => {
                let Some(Provider::EncryptedReasoning {
                    item_id,
                    encrypted_content,
                }) = provider_specific.as_any().downcast_ref::<Provider>()
                else {
                    continue;
                };
                if encrypted_content.is_empty() {
                    continue;
                }
                json!({
                    "type": "reasoning",
                    "id": item_id.as_str(),
                    "encrypted_content": encrypted_content,
                    "summary": summary
                        .iter()
                        .map(|text| json!({ "type": "summary_text", "text": text }))
                        .collect::<Vec<_>>(),
                })
            }
            InferenceResponseItem::Compaction { .. } | InferenceResponseItem::Unknown { .. } => {
                let Some((id, encrypted_content)) = compaction(item) else {
                    continue;
                };
                json!({ "type": "compaction", "id": id, "encrypted_content": encrypted_content })
            }
            // Old replay never sent raw reasoning.
            InferenceResponseItem::RawReasoning { .. } => continue,
        };
        carry.push(value.to_string());
    }
    (calls, carry, prose)
}

#[cfg(test)]
mod tests {
    use rho_agent_types::MessageDelivery;
    use rho_inference::types::{
        ProviderResponseItemId, ToolCallId, ToolName, ToolOutput, ToolType,
    };
    use rho_inference2::Item;

    use super::super::tests::{test_agent_runtime, test_workspace};
    use super::super::{
        AgentProfileWriteTxnExt, AgentWriteTxnExt, InferenceProfile, SessionBinding,
    };
    use super::*;
    use crate::QueuedInput;

    fn accepted(source: MessageSender, text: &str) -> AgentEvent<'static> {
        AgentEvent::Accepted(QueuedInput {
            source,
            kind: InputKind::Message {
                content: vec![ContentPart::Text { text: text.into() }],
            },
            delivery: MessageDelivery::Immediate,
            at: UnixMs(1),
        })
    }

    fn request(input: Vec<ContextBlock>) -> AgentEvent<'static> {
        AgentEvent::Native(NativeEvent::RequestStarted {
            input,
            context: None,
            wake: None,
            at: UnixMs(2),
        })
    }

    fn response(items: Vec<InferenceResponseItem>) -> AgentEvent<'static> {
        AgentEvent::Native(NativeEvent::ResponseFinished {
            output: vec![ContextBlock::InferenceResponse {
                items,
                provider_response_id: None,
            }],
            context_used: None,
            usage: None,
            at: UnixMs(3),
        })
    }

    fn call(id: &str) -> InferenceResponseItem {
        InferenceResponseItem::ToolCall {
            provider_specific: Box::new(Provider::CustomToolCall {
                item_id: ProviderResponseItemId::try_from(format!("ctc_{id}").as_str()).unwrap(),
            }),
            id: ToolCallId::try_from(id).unwrap(),
            name: ToolName::try_from("exec").unwrap(),
            tool_type: ToolType::Custom,
            arguments: format!("print('{id}')"),
        }
    }

    fn result(id: &str, output: &str) -> ToolResult {
        ToolResult {
            call_id: ToolCallId::try_from(id).unwrap(),
            tool_type: ToolType::Custom,
            body: ToolOutput {
                output: std::sync::Arc::new(output.into()),
                full_output: None,
                images: Default::default(),
                status: rho_agent_types::ToolOutputStatus::Success,
            },
            started_at: UnixMs(2),
            finished_at: UnixMs(2),
            metadata: None,
        }
    }

    #[tokio::test]
    async fn native_rows_become_entries_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(temp.path().join("rho.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent_id = write.alloc_agent_id();
        write.create_agent(
            UnixMs(0),
            agent_id,
            None,
            test_workspace(),
            Default::default(),
            SessionBinding::ResponsesSol(InferenceProfile::default()),
            test_agent_runtime(),
            super::super::AgentOrigin::User,
        );
        let peer = write.alloc_agent_id();
        for event in [
            accepted(MessageSender::User, "hi"),
            request(vec![ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: vec![ContentPart::Text { text: "hi".into() }],
            }]),
            response(vec![call("c1")]),
            request(vec![ContextBlock::ToolResults {
                results: vec![result("c1", "one")],
            }]),
            response(vec![call("c2")]),
            request(vec![
                ContextBlock::ToolResults {
                    results: vec![result("c2", "two")],
                },
                ContextBlock::ToolHistoryEvicted {
                    call_ids: vec![ToolCallId::try_from("c1").unwrap()],
                },
                ContextBlock::DeveloperMessage {
                    text: "note".into(),
                },
            ]),
            response(vec![InferenceResponseItem::AssistantMessage {
                provider_specific: Box::new(Provider::Message {
                    item_id: ProviderResponseItemId::try_from("msg_1").unwrap(),
                }),
                content: vec![ContentPart::Text {
                    text: "done".into(),
                }],
                phase: None,
            }]),
            accepted(MessageSender::Agent { id: peer }, "later"),
        ] {
            write.append_agent_event(agent_id, &event);
        }
        migrate(&mut write);

        let rows = {
            let log = write.open_table(AGENT_LOG);
            rows(log.range(agent_range(agent_id))).collect::<Vec<_>>()
        };
        assert_eq!(rows.len(), 9, "rewritten in place, nothing added");
        assert!(matches!(rows[0].1, AgentEvent::Created { .. }));
        let entries = rows[1..]
            .iter()
            .map(|(_, event)| match event {
                AgentEvent::Entry(entry) => entry.clone(),
                other => panic!("not converted: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(
                &entries[2],
                Entry::Step { calls, .. } if calls[0].id == *"c1"
            ),
            "the evicted call is still there to read"
        );
        assert!(
            matches!(
                &entries[3],
                Entry::Woken { results, .. } if results[0].text == "one"
            ),
            "and so is its result"
        );
        let unread = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Received { id, .. } => Some(*id),
                _ => None,
            })
            .filter(|id| {
                !entries.iter().any(|entry| {
                    matches!(entry, Entry::Woken { messages, acknowledged, .. }
                        if messages.contains(id) || acknowledged.contains(id))
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(unread, vec![MessageId(rows[8].0.pos)]);

        let request =
            crate::agent::context::request("".into(), &entries, rho_inference2::CacheKey::new());
        let shown = request
            .items
            .iter()
            .map(|item| match item {
                Item::Step(carry) => format!("step {:?}", carry.call_ids()),
                Item::Result { call_id, text, .. } => format!("result {call_id}: {text}"),
                Item::User { text, .. } => format!("user {text}"),
                Item::CompactionTrigger => "compact".into(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [
                "user Message from the human:\nhi",
                "step []",
                "step [CallId(\"c2\")]",
                "result c2: two\n\nnote",
                "step []",
            ]
        );
    }
}
