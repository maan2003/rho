//! Temporary conversion of any native rows left in dc371fa2 histories.
//! Rows are rewritten in place, preserving positions, journal, and rewinds.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use rho_agent_types::transcript::{
    ContextBlock, InferenceResponseItem, MessageSender, OpenAiResponsesProviderData as Provider,
    PendingInferenceResponse, ProviderSpecificData, StreamingContextItemState, ToolResult,
};
use rho_agent_types::{AgentId, ContentPart, MessagePhase, UnixMs};
use rho_db::{SenValue, WriteTxn};
use serde_json::json;

use super::legacy::Entry;
use super::legacy::provider::{Call, CallResult, Carry};
use super::{AGENT_HEADS, AGENT_LOG, agent_range, fold_head, rows};
use crate::db::legacy::NativeEvent;
use crate::entry::{Block, MessageId, Notice, Party, Wake};
use crate::inference::{Image, Usage};
use crate::{AgentEvent, InputKind};

pub(super) fn migrate(write: &mut WriteTxn) -> BTreeMap<(AgentId, u64), Vec<Entry>> {
    let agents = write
        .open_table(AGENT_HEADS)
        .iter()
        .map(|(id, _)| id.value())
        .collect::<Vec<_>>();
    let mut synthetic = BTreeMap::new();
    for agent_id in agents {
        let has_native = write
            .open_table(AGENT_LOG)
            .range(agent_range(agent_id))
            .any(|(_, row)| matches!(row.value().into_owned(), AgentEvent::Native(_)));
        if !has_native {
            continue;
        }
        synthetic.extend(
            migrate_agent(write, agent_id)
                .into_iter()
                .map(|(pos, entries)| ((agent_id, pos), entries)),
        );
        // The rewrite folds the same, but the stored head must be the log's.
        let head = {
            let log = write.open_table(AGENT_LOG);
            fold_head(rows(log.range(agent_range(agent_id))))
                .expect("agent log starts with creation")
        };
        write
            .open_table(AGENT_HEADS)
            .insert(&agent_id, SenValue::borrowed(&head));
    }
    synthetic
}

/// How many delivered messages had no accepted row to become.
fn migrate_agent(write: &mut WriteTxn, agent_id: AgentId) -> BTreeMap<u64, Vec<Entry>> {
    let window = {
        let log = write.open_table(AGENT_LOG);
        Window::scan(rows(log.range(agent_range(agent_id))))
    };
    let targets = {
        let log = write.open_table(AGENT_LOG);
        rows(log.range(agent_range(agent_id)))
            .filter_map(|(_, event)| match event {
                AgentEvent::Rewound { to, .. } => Some(to.pos),
                _ => None,
            })
            .collect::<HashSet<_>>()
    };
    let mut log = write.open_table(AGENT_LOG);
    let mut convert = Convert::new(window, targets);
    for pos in convert.window.positions.clone() {
        let event = log
            .get(&(agent_id, pos))
            .expect("row seen by the scan")
            .value()
            .into_owned();
        if let Some(entry) = convert.row(pos, &event) {
            log.insert(
                &(agent_id, pos),
                SenValue::borrowed(&AgentEvent::LegacyEntry(entry)),
            );
        }
    }
    convert.synthetic.into_iter().collect()
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
    synthetic: HashMap<u64, Vec<Entry>>,
    // Snapshot before each physical row so rewinding restores pending messages
    // on the branch without losing conversion of the hidden branch's rows.
    before: HashMap<
        u64,
        (
            VecDeque<(MessageId, MessageSender, Vec<ContentPart>)>,
            HashSet<String>,
            bool,
        ),
    >,
    targets: HashSet<u64>,
    old_native_epoch: bool,
}

impl Convert {
    fn new(window: Window, targets: HashSet<u64>) -> Self {
        Self {
            window,
            pending: VecDeque::new(),
            calls: HashSet::new(),
            synthetic: HashMap::new(),
            before: HashMap::new(),
            targets,
            old_native_epoch: true,
        }
    }

    fn row(&mut self, pos: u64, event: &AgentEvent<'static>) -> Option<Entry> {
        if self.targets.contains(&pos) {
            self.before.insert(
                pos,
                (
                    self.pending.clone(),
                    self.calls.clone(),
                    self.old_native_epoch,
                ),
            );
        }
        let visible = self.window.visible.contains(&pos);
        Some(match event {
            AgentEvent::Accepted(input) if self.old_native_epoch => match &input.kind {
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
                InputKind::Compaction => Entry::Sent {
                    at: input.at,
                    id: MessageId::new(),
                    to: Party::Human,
                    text: "[Historical event: compaction requested.]".into(),
                },
            },
            AgentEvent::Cleared { at } if self.old_native_epoch => Entry::Woken {
                at: *at,
                why: Wake::Message,
                report: String::new(),
                images: Vec::new(),
                messages: Vec::new(),
                acknowledged: self.pending.drain(..).map(|(id, ..)| id).collect(),
                results: Vec::new(),
            },
            AgentEvent::Rewound { to, .. } => {
                let (pending, calls, old_native_epoch) = self
                    .before
                    .get(&to.pos)
                    .unwrap_or_else(|| panic!("rewind to absent row {}", to.pos))
                    .clone();
                self.pending = pending;
                self.calls = calls;
                self.old_native_epoch = old_native_epoch;
                return None;
            }
            AgentEvent::Entry(crate::entry::Entry::Received { id, from, body, .. }) => {
                self.old_native_epoch = false;
                let sender = match from {
                    Party::Human => MessageSender::User,
                    Party::Agent(id) => MessageSender::Agent { id: *id },
                };
                self.pending.push_back((
                    *id,
                    sender,
                    body.iter()
                        .map(|part| match part {
                            Block::Text(text) => ContentPart::Text { text: text.clone() },
                            Block::Image(image) => ContentPart::Image {
                                media_type: image.media_type.clone(),
                                data: image.data.clone(),
                            },
                        })
                        .collect(),
                ));
                return None;
            }
            AgentEvent::Entry(crate::entry::Entry::RequestSent { report, .. }) => {
                self.old_native_epoch = false;
                for id in report.messages.iter().chain(&report.acknowledged) {
                    if let Some(index) = self
                        .pending
                        .iter()
                        .position(|(candidate, ..)| candidate == id)
                    {
                        self.pending.remove(index);
                    }
                }
                return None;
            }
            AgentEvent::Failed { error, at, .. } => Entry::Notice {
                at: *at,
                notice: Notice::Error(error.to_string()),
            },
            AgentEvent::Native(NativeEvent::RequestFailed { partial, at, .. }) => {
                partial_step(partial, *at)
            }
            AgentEvent::Native(NativeEvent::RequestStarted { input, at, .. }) => {
                self.request(input, *at, pos, visible, false)
            }
            AgentEvent::Native(NativeEvent::ResponseFinished {
                output, usage, at, ..
            }) => {
                let (calls, carry, prose, evidence) = response(output, &|_| false, pos);
                self.calls.extend(carry_calls(&carry));
                Entry::Step {
                    at: *at,
                    calls,
                    prose,
                    carry: Carry::from_openai_items_with_evidence(carry, evidence),
                    usage: usage
                        .as_ref()
                        .map_or_else(Default::default, |bucket| Usage {
                            input_tokens: bucket
                                .input_tokens
                                .saturating_add(bucket.cache_read_tokens),
                            cached_tokens: bucket.cache_read_tokens,
                            output_tokens: bucket.output_tokens,
                        }),
                }
            }
            AgentEvent::Entry(_) | AgentEvent::LegacyEntry(_) => {
                self.old_native_epoch = false;
                return None;
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
        pos: u64,
        visible: bool,
        before: bool,
    ) -> Entry {
        let mut messages = Vec::new();
        let mut results: Vec<CallResult> = Vec::new();
        let mut images = Vec::new();
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
                            let id = MessageId::new();
                            self.synthetic
                                .entry(pos)
                                .or_default()
                                .push(Entry::Received {
                                    at,
                                    id,
                                    from: party(*sender),
                                    body: content.iter().map(block_of).collect(),
                                });
                            messages.push(id);
                        }
                    }
                }
                ContextBlock::ToolResults { results: answered } => {
                    why.get_or_insert(Wake::Returned);
                    for result in answered {
                        results.push(call_result(result));
                    }
                }
                ContextBlock::ToolUpdate(update) => {
                    why.get_or_insert(Wake::Notify);
                    extra.push(format!(
                        "Update from exec {}:\\n{}",
                        update.call_id.as_str(),
                        complete_output(
                            update.full_output.as_ref().map(|s| s.as_str()),
                            &update.output
                        )
                    ));
                    images.extend(update.images.iter().map(|image| Image {
                        media_type: image.media_type.clone(),
                        data: image.data.clone(),
                    }));
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
        let report = if before {
            String::new()
        } else {
            extra.join("\n\n")
        };
        let (messages, acknowledged) = if before {
            (Vec::new(), messages)
        } else {
            (messages, Vec::new())
        };
        Entry::Woken {
            at,
            why: why.unwrap_or(Wake::Notify),
            report,
            images,
            messages,
            acknowledged,
            results,
        }
    }
}

pub(super) fn party(sender: MessageSender) -> Party {
    match sender {
        MessageSender::User => Party::Human,
        MessageSender::Agent { id } => Party::Agent(id),
    }
}

pub(super) fn block_of(part: &ContentPart) -> Block {
    match part {
        ContentPart::Text { text } => Block::Text(text.clone()),
        ContentPart::Image { media_type, data } => Block::Image(Image {
            media_type: media_type.clone(),
            data: data.clone(),
        }),
    }
}

fn complete_output(full: Option<&str>, bounded: &str) -> String {
    match full {
        None => bounded.to_owned(),
        Some(full) if full.contains(bounded) => full.to_owned(),
        Some(full) => {
            format!("Full recorded output:\n{full}\n\nModel-visible output:\n{bounded}")
        }
    }
}

fn call_result(result: &ToolResult) -> CallResult {
    CallResult::from_legacy(
        result.call_id.as_str(),
        complete_output(
            result.body.full_output.as_ref().map(|s| s.as_str()),
            &result.body.output,
        ),
        result
            .body
            .images
            .iter()
            .map(|image| Image {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            })
            .collect(),
    )
}

fn carry_calls(carry: &[String]) -> Vec<String> {
    carry
        .iter()
        .filter_map(|item| serde_json::from_str::<serde_json::Value>(item).ok())
        .filter(|item| item["type"] == "custom_tool_call")
        .filter_map(|item| item["call_id"].as_str().map(str::to_owned))
        .collect()
}

pub(super) fn compaction(item: &InferenceResponseItem) -> Option<(&str, &str)> {
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
) -> (Vec<Call>, Vec<String>, String, Vec<String>) {
    let mut calls = Vec::new();
    let mut carry = Vec::new();
    let mut prose = String::new();
    let mut evidence = Vec::new();
    for block in output {
        match block {
            ContextBlock::InferenceResponse {
                provider_response_id: Some(id),
                ..
            } => evidence.push(json!({"provider_response_id": id.as_str()}).to_string()),
            ContextBlock::InferenceResponse { .. } => {}
            _ => evidence.push(json!({"unreplayed_block_senax_hex": exact_hex(block)}).to_string()),
        }
    }
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
                let text = rho_agent_types::transcript::text_content(content);
                if !matches!(
                    provider_specific.as_any().downcast_ref::<Provider>(),
                    Some(Provider::Message { .. })
                ) || content
                    .iter()
                    .any(|part| matches!(part, ContentPart::Image { .. }))
                {
                    evidence.push(
                        json!({"response_index": index, "item": evidence_of(item)}).to_string(),
                    );
                }
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
                name,
                tool_type,
                arguments,
            } => {
                if name.as_str() != "exec"
                    || *tool_type != rho_agent_types::transcript::ToolType::Custom
                    || !matches!(
                        provider_specific.as_any().downcast_ref::<Provider>(),
                        Some(Provider::FunctionCall { .. } | Provider::CustomToolCall { .. })
                    )
                {
                    evidence.push(
                        json!({"response_index": index, "item": evidence_of(item)}).to_string(),
                    );
                }
                calls.push(Call::from_legacy(id.as_str(), arguments.clone()));
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
                    "name": "exec",
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
                    evidence.push(
                        json!({"response_index": index, "item": evidence_of(item)}).to_string(),
                    );
                    continue;
                };
                if encrypted_content.is_empty() {
                    evidence.push(
                        json!({"response_index": index, "item": evidence_of(item)}).to_string(),
                    );
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
                    evidence.push(
                        json!({"response_index": index, "item": evidence_of(item)}).to_string(),
                    );
                    continue;
                };
                json!({ "type": "compaction", "id": id, "encrypted_content": encrypted_content })
            }
            // Raw reasoning was never provider replay; preserve it as opaque
            // evidence rather than showing previously private text to the model.
            InferenceResponseItem::RawReasoning { .. } => {
                evidence
                    .push(json!({"response_index": index, "item": evidence_of(item)}).to_string());
                continue;
            }
        };
        carry.push(value.to_string());
    }
    (calls, carry, prose, evidence)
}

fn exact_hex<T: senax_encoder::Encoder>(value: &T) -> String {
    senax_encoder::encode(value)
        .expect("persisted native item encodes")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn provider_evidence(
    item: &InferenceResponseItem,
    provider: &dyn ProviderSpecificData,
) -> serde_json::Value {
    match provider.as_any().downcast_ref::<Provider>() {
        Some(Provider::Message { item_id }) => {
            json!({"type":"message", "item_id":item_id.as_str()})
        }
        Some(Provider::FunctionCall { item_id }) => {
            json!({"type":"function_call", "item_id":item_id.as_str()})
        }
        Some(Provider::CustomToolCall { item_id }) => {
            json!({"type":"custom_tool_call", "item_id":item_id.as_str()})
        }
        Some(Provider::EncryptedReasoning {
            item_id,
            encrypted_content,
        }) => {
            json!({"type":"encrypted_reasoning", "item_id":item_id.as_str(), "encrypted_content":encrypted_content})
        }
        Some(Provider::Compaction {
            item_id,
            encrypted_content,
        }) => {
            json!({"type":"compaction", "item_id":item_id.as_str(), "encrypted_content":encrypted_content})
        }
        None => json!({"original_item_senax_hex": exact_hex(item)}),
    }
}

fn evidence_of(item: &InferenceResponseItem) -> serde_json::Value {
    match item {
        InferenceResponseItem::AssistantMessage {
            provider_specific,
            content,
            phase,
        } => {
            let content = content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => json!({"type":"text", "text":text}),
                    ContentPart::Image { media_type, data } => {
                        json!({"type":"image", "media_type":media_type, "data":data})
                    }
                })
                .collect::<Vec<_>>();
            json!({"type":"assistant_message", "content":content, "phase":format!("{phase:?}"),
                "provider":provider_evidence(item, provider_specific.as_ref())})
        }
        InferenceResponseItem::ToolCall {
            provider_specific,
            id,
            name,
            tool_type,
            arguments,
        } => json!({"type":"tool_call", "id":id.as_str(), "name":name.as_str(),
                "tool_type":format!("{tool_type:?}"), "arguments":arguments,
                "provider":provider_evidence(item, provider_specific.as_ref())}),
        InferenceResponseItem::RawReasoning {
            provider_specific,
            content,
            summary,
        } => json!({"type":"raw_reasoning", "content":content, "summary":summary,
                "provider":provider_evidence(item, provider_specific.as_ref())}),
        InferenceResponseItem::EncryptedReasoning {
            provider_specific,
            summary,
        } => json!({"type":"encrypted_reasoning", "summary":summary,
                "provider":provider_evidence(item, provider_specific.as_ref())}),
        InferenceResponseItem::Compaction { provider_specific } => {
            json!({"type":"compaction", "provider":provider_evidence(item, provider_specific.as_ref())})
        }
        InferenceResponseItem::Unknown { provider_specific } => {
            json!({"type":"unknown", "provider":provider_evidence(item, provider_specific.as_ref())})
        }
    }
}

fn partial_step(partial: &PendingInferenceResponse, at: UnixMs) -> Entry {
    let mut calls = Vec::new();
    let mut prose = String::new();
    let mut evidence = Vec::new();
    for (index, state) in partial.items.iter().enumerate() {
        let (kind, item) = match state {
            StreamingContextItemState::Empty => {
                evidence.push(json!({"response_index": index, "state":"empty"}).to_string());
                continue;
            }
            StreamingContextItemState::Pending(item) => ("pending", item),
            StreamingContextItemState::Finished(item) => ("finished", item),
        };
        let item = item.to_context_item().expect("persisted partial item");
        if let InferenceResponseItem::AssistantMessage { content, .. } = &item {
            prose.push_str(&rho_agent_types::transcript::text_content(content));
        }
        if let InferenceResponseItem::ToolCall { id, arguments, .. } = &item {
            calls.push(Call::from_legacy(id.as_str(), arguments.clone()));
        }
        evidence.push(
            json!({"response_index":index, "state":kind, "item": evidence_of(&item)}).to_string(),
        );
    }
    Entry::Step {
        at,
        calls,
        prose,
        carry: Carry::from_openai_items_with_evidence(vec![], evidence),
        usage: Usage::default(),
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::MessageDelivery;
    use rho_agent_types::transcript::{
        ProviderResponseItemId, ToolCallId, ToolName, ToolOutput, ToolType,
    };

    use super::super::tests::{test_agent_runtime, test_workspace};
    use super::super::{
        AgentProfileWriteTxnExt, AgentWriteTxnExt, InferenceProfile, SessionBinding,
    };
    use super::*;
    use crate::QueuedInput;
    use crate::db::legacy::Item;

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
                AgentEvent::LegacyEntry(entry) => entry.clone(),
                other => panic!("not converted: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(
                &entries[2],
                Entry::Step { calls, .. } if calls[0].display_id() == "c1"
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

        assert!(entries.iter().any(|entry|
            matches!(entry, Entry::Woken { report, .. } if report == "note")));
        let request = crate::db::legacy::request(&entries);
        let shown = request
            .iter()
            .map(|item| match item {
                Item::Step(calls) => format!("step {calls:?}"),
                Item::Result(result) => format!("result {}: {}", result.display_id(), result.text),
                Item::User { text, .. } => format!("user {text}"),
                Item::CompactionTrigger => "compact".into(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [
                "user Message from the human:\nhi",
                "step [\"c1\"]",
                "result c1: one",
                "step [\"c2\"]",
                "result c2: two",
                "step []",
            ]
        );
    }
}
