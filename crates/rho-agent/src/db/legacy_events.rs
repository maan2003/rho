//! What the previous Rho loop wrote, decoded only by the `b1e40c93 ->
//! 50351c18` migration and rewritten into the current rows. Deleted with
//! the migration (`.agents/skills/temp-migration/SKILL.md`).
//!
//! `LegacyAgentEvent` is the log's enum as the old build had it: every
//! current variant, byte for byte the same (senax keys variants and fields
//! by name), plus the five the old loop wrote. The translator folds those
//! five with the rules `agent::replay` used to apply to them, so the
//! context an agent replays to is unchanged:
//!
//! - `Queued` becomes `Accepted` when it happened; what a later `Sent` would
//!   drop from the queue but the old loop still held is accepted again at the
//!   end of the lineage.
//! - `ToolResult` rows collect until the old loop committed them: a `Sent` with
//!   one `ToolResults` block before the next response, at a delivery, or at the
//!   lineage's end.
//! - `Dequeued` becomes the `Sent` that delivered the queue (a `NextRequest`
//!   delivery holds back `NextTurn` items).
//! - `InferenceResponse` becomes `Replied`, with the context usage the old loop
//!   would have carried (a compaction response clears it).
//! - `PresentationUpdated` becomes `Presented`.
//!
//! Old rows carry no time; rows made from them take the newest time seen
//! so far (tool results' `finished_at`, any row with a time, the agent's
//! creation).

use std::borrow::Cow;
use std::sync::Arc;

use bytes::BytesMut;
use rho_core::{
    ContentPart, ContextBlock, InferenceResponseItem, MessageDelivery, MessageSender,
    PendingInferenceResponse, ProviderResponseId, ToolResult, ToolUpdate, UnixMs,
};
use rho_db::RecordedTypeName;
use rho_workspaces::WorkspaceInfo;
use senax_encoder::{Decode, Decoder as _, Encode, Encoder as _};

use super::{
    AgentEventPos, AgentId, AgentPresentationUpdate, AgentRole, AgentRuntime, AgentSpawnedBy,
    AgentWant, PresentationField, SessionBinding, TurnEdge,
};
use crate::{
    AgentEvent, InputKind, InputSourceId, PresentationSpeaker, QueuedInput, RuntimeChange,
};

/// The name the old build wrote `agent_events` values under.
#[derive(Debug)]
pub(super) struct AgentEventName;

impl RecordedTypeName for AgentEventName {
    const NAME: &'static str = "rho-db::Sen<rho_agent::AgentEvent<'_>>";
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub(super) enum LegacyAgentEvent<'a> {
    /// An input entered a queue: user text, mail, or a `/compact`. It becomes
    /// context when a later `Sent` carries it.
    Accepted(QueuedInput),
    /// A boundary: every source was drained into `blocks`, they were appended
    /// to history, and a request went out carrying all of it.
    ///
    /// One event rather than an append and a start, because it was always one
    /// thing — and because the drain, the append and the send cannot come
    /// apart even in a crash. `blocks` can be empty: a retry or a resume
    /// sends with nothing pending, which is a fact worth being able to write
    /// down.
    Sent {
        blocks: Cow<'a, [ContextBlock]>,
        #[senax(default)]
        at: UnixMs,
    },
    /// The model answered, and the request is over.
    Replied {
        blocks: Cow<'a, [ContextBlock]>,
        /// Context-window occupancy after this response (all input plus
        /// output tokens), or `None` when it compacted or usage was missing.
        context_used: Option<u64>,
        /// What the response cost, as the provider reported it. Told
        /// here, at the response, so a reader can price the transcript
        /// without a usage table (`AGENT-LOG-DESIGN.md`).
        #[senax(default)]
        usage: Option<crate::db::AgentUsageBucket>,
        #[senax(default)]
        at: UnixMs,
    },
    /// All queued items were dropped (cancel). Written before the log
    /// carried times; `Cleared` is what is written now.
    QueueCleared,
    Cleared {
        at: UnixMs,
    },
    /// A turn started or stopped: the edge both runtimes cross.
    Turn {
        edge: TurnEdge,
        at: UnixMs,
    },
    /// The sidecar's title and activity, applied.
    Presented {
        title: PresentationField,
        activity: PresentationField,
        at: UnixMs,
    },
    /// What the last turn asks of the person.
    Wants {
        want: AgentWant,
        summary: Option<String>,
        at: UnixMs,
    },
    /// Everything from `to` up to this event is no longer the agent's
    /// history. Told, never undone: positions only grow.
    Rewound {
        to: AgentEventPos,
        at: UnixMs,
    },
    /// A request failed with this much of a response in. `retrying` when
    /// the loop makes the request again by itself; otherwise the turn
    /// ends in error right after. Never history: the next request does
    /// not carry it. Written so what the model said is not lost.
    Failed {
        partial: PendingInferenceResponse,
        error: Cow<'a, str>,
        retrying: bool,
        at: UnixMs,
    },

    // -- what the previous Rho loop wrote; decoded, never written ------------
    InferenceResponse {
        items: Cow<'a, [InferenceResponseItem]>,
        provider_response_id: Option<ProviderResponseId>,
        context_used: Option<u64>,
    },
    ToolResult {
        result: Cow<'a, ToolResult>,
    },
    Queued(QueuedItem),
    Dequeued {
        boundary: LegacyDelivery,
    },
    /// The Claude runtime's presentation record before `Presented`.
    PresentationUpdated {
        update: AgentPresentationUpdate,
    },

    // -- the runtimes' shared config log --------------------------------------
    /// A text-only message confirmed in Claude Code's external transcript.
    /// It gives the shared presentation sidecar a durable, rewindable
    /// source without treating Claude's protocol state as native inference.
    ClaudePresentationSource {
        source_id: uuid::Uuid,
        speaker: PresentationSpeaker,
        /// The message whole (rows from before the mirror existed hold
        /// the first kilobyte only).
        text: Cow<'a, str>,
        #[senax(default)]
        at: UnixMs,
    },
    /// The agent coming into being: the first event of every agent's log,
    /// and the base the head's config is folded from. A spawn name given
    /// here is why no title is generated for that agent.
    Created {
        role: AgentRole,
        binding: SessionBinding,
        runtime: AgentRuntime,
        workdirs: Vec<WorkspaceInfo>,
        spawned_by: AgentSpawnedBy,
        spawn_name: Option<String>,
        created_at: rho_core::UnixMs,
        /// The agent that spawned this one.
        #[senax(default)]
        parent: Option<AgentId>,
    },
    RoleChanged {
        role: AgentRole,
        /// `None` when only the role moved and the session binding stands.
        binding: Option<SessionBinding>,
        #[senax(default)]
        at: UnixMs,
    },
    WorkdirAdded {
        workdir: WorkspaceInfo,
        #[senax(default)]
        at: UnixMs,
    },
    /// The runtime itself changing under the agent: a Claude rewind before
    /// and after its destination transcript is verified, or a new prompt
    /// cache key for the Rho runtime.
    RuntimeRebound {
        change: RuntimeChange,
        #[senax(default)]
        at: UnixMs,
    },
}

/// What the previous loop queued. Decoded from old logs only.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub(super) struct QueuedItem {
    pub kind: QueuedItemKind,
    pub delivery: LegacyDelivery,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub(super) enum QueuedItemKind {
    UserMessage {
        sender: MessageSender,
        content: Arc<Vec<ContentPart>>,
        #[senax(default)]
        source_id: Option<InputSourceId>,
    },
    Compaction,
    ToolUpdate(ToolUpdate),
}

/// The delivery lanes the previous loop had. `NextTurn` no longer exists
/// live; old rows that name it still have to decode, and replay reads it
/// as `NextRequest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub(super) enum LegacyDelivery {
    Immediate,
    NextRequest,
    NextTurn,
}

impl From<LegacyDelivery> for MessageDelivery {
    fn from(delivery: LegacyDelivery) -> Self {
        match delivery {
            LegacyDelivery::Immediate => Self::Immediate,
            LegacyDelivery::NextRequest | LegacyDelivery::NextTurn => Self::NextRequest,
        }
    }
}

/// A row the old loop wrote, as the current log says it.
///
/// Current variants are the same bytes under both enums, so they cross
/// through the encoder rather than through a match that would have to
/// spell every field of every variant.
fn current(event: LegacyAgentEvent<'static>) -> AgentEvent<'static> {
    let mut bytes = BytesMut::new();
    event
        .encode(&mut bytes)
        .expect("encode a legacy agent event");
    AgentEvent::decode(&mut &bytes[..]).expect("a current variant decodes under the current enum")
}

/// A queued item as the current queue would hold it; `None` for a tool
/// update, which the current queue has no room for (the old loop dropped
/// undelivered ones at replay too).
fn accepted(item: &QueuedItem, at: UnixMs) -> Option<QueuedInput> {
    match &item.kind {
        QueuedItemKind::UserMessage {
            sender, content, ..
        } => Some(QueuedInput {
            source: *sender,
            kind: InputKind::Message {
                content: (**content).clone(),
            },
            delivery: item.delivery.into(),
            at,
        }),
        QueuedItemKind::Compaction => Some(QueuedInput {
            source: MessageSender::User,
            kind: InputKind::Compaction,
            delivery: item.delivery.into(),
            at,
        }),
        QueuedItemKind::ToolUpdate(_) => None,
    }
}

/// The context block a legacy queued input became at delivery.
fn delivered_block(item: QueuedItem) -> ContextBlock {
    match item.kind {
        QueuedItemKind::UserMessage {
            sender, content, ..
        } => ContextBlock::UserMessage {
            sender,
            content: Arc::try_unwrap(content).unwrap_or_else(|content| (*content).clone()),
        },
        QueuedItemKind::Compaction => ContextBlock::CompactionTrigger,
        QueuedItemKind::ToolUpdate(update) => ContextBlock::ToolUpdate(update),
    }
}

/// When a current row says it happened.
fn time_of(event: &AgentEvent<'_>) -> Option<UnixMs> {
    Some(match event {
        AgentEvent::Accepted(input) => input.at,
        AgentEvent::Sent { at, .. }
        | AgentEvent::Replied { at, .. }
        | AgentEvent::Cleared { at }
        | AgentEvent::Turn { at, .. }
        | AgentEvent::Presented { at, .. }
        | AgentEvent::Wants { at, .. }
        | AgentEvent::Rewound { at, .. }
        | AgentEvent::Failed { at, .. }
        | AgentEvent::ClaudePresentationSource { at, .. }
        | AgentEvent::RoleChanged { at, .. }
        | AgentEvent::WorkdirAdded { at, .. }
        | AgentEvent::RuntimeRebound { at, .. } => *at,
        AgentEvent::Created { created_at, .. } => *created_at,
        AgentEvent::QueueCleared => return None,
    })
}

#[derive(Clone)]
struct Held {
    item: QueuedItem,
    /// A `Sent` was told after this item's `Accepted`, so a reader's queue
    /// no longer has it while the old loop's still did.
    cleared: bool,
}

/// The old loop's state as it moved through one lineage, and the current
/// rows each old row becomes. Cloned at a fork point so the fork resumes
/// from what the parent had in flight there.
#[derive(Clone)]
pub(super) struct Translator {
    pending: Vec<ToolResult>,
    queue: Vec<Held>,
    context_used: Option<u64>,
    clock: UnixMs,
}

impl Translator {
    pub(super) fn new(created_at: UnixMs) -> Self {
        Self {
            pending: Vec::new(),
            queue: Vec::new(),
            context_used: None,
            clock: created_at,
        }
    }

    pub(super) fn translate(
        &mut self,
        event: LegacyAgentEvent<'static>,
    ) -> Vec<AgentEvent<'static>> {
        let mut out = Vec::new();
        match event {
            LegacyAgentEvent::Queued(item) => {
                if let Some(input) = accepted(&item, self.clock) {
                    out.push(AgentEvent::Accepted(input));
                }
                self.queue.push(Held {
                    item,
                    cleared: false,
                });
            }
            LegacyAgentEvent::ToolResult { result } => {
                let result = result.into_owned();
                self.clock = self.clock.max(result.finished_at);
                self.pending.push(result);
            }
            LegacyAgentEvent::InferenceResponse {
                items,
                provider_response_id,
                context_used,
            } => {
                let items = items.into_owned();
                let compacted = items
                    .iter()
                    .any(|item| matches!(item, InferenceResponseItem::Compaction { .. }));
                if compacted {
                    // Compaction response usage describes the old, full
                    // input, not the newly compacted context.
                    self.context_used = None;
                } else if context_used.is_some() {
                    self.context_used = context_used;
                }
                self.flush_results(&mut out);
                out.push(AgentEvent::Replied {
                    blocks: Cow::Owned(vec![ContextBlock::InferenceResponse {
                        items,
                        provider_response_id,
                    }]),
                    context_used: self.context_used,
                    usage: None,
                    at: self.clock,
                });
            }
            LegacyAgentEvent::Dequeued { boundary } => {
                let mut blocks = Vec::new();
                if !self.pending.is_empty() {
                    blocks.push(ContextBlock::ToolResults {
                        results: std::mem::take(&mut self.pending),
                    });
                }
                let (delivered, held): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queue)
                    .into_iter()
                    .partition(|held| {
                        boundary == LegacyDelivery::NextTurn
                            || held.item.delivery != LegacyDelivery::NextTurn
                    });
                self.queue = held;
                blocks.extend(delivered.into_iter().map(|held| delivered_block(held.item)));
                if !blocks.is_empty() {
                    self.sent(blocks, &mut out);
                }
            }
            LegacyAgentEvent::PresentationUpdated { update } => out.push(AgentEvent::Presented {
                title: update.generated_title,
                activity: update.activity,
                at: self.clock,
            }),
            other => {
                let event = current(other);
                match &event {
                    AgentEvent::Sent { .. } => {
                        self.flush_results(&mut out);
                        self.queue.clear();
                    }
                    AgentEvent::QueueCleared | AgentEvent::Cleared { .. } => self.queue.clear(),
                    _ => {}
                }
                if let Some(at) = time_of(&event) {
                    self.clock = self.clock.max(at);
                }
                out.push(event);
            }
        }
        out
    }

    /// The rows that close the lineage: results still uncommitted, and
    /// the queue a reader would have lost to a `Sent` the old loop never
    /// made. Dated at the epoch, as replay dated what the old loop left.
    pub(super) fn finish(&mut self) -> Vec<AgentEvent<'static>> {
        let mut out = Vec::new();
        self.flush_results(&mut out);
        for held in std::mem::take(&mut self.queue) {
            if held.cleared
                && let Some(input) = accepted(&held.item, UnixMs(0))
            {
                out.push(AgentEvent::Accepted(input));
            }
        }
        out
    }

    fn flush_results(&mut self, out: &mut Vec<AgentEvent<'static>>) {
        if self.pending.is_empty() {
            return;
        }
        let blocks = vec![ContextBlock::ToolResults {
            results: std::mem::take(&mut self.pending),
        }];
        self.sent(blocks, out);
    }

    fn sent(&mut self, blocks: Vec<ContextBlock>, out: &mut Vec<AgentEvent<'static>>) {
        out.push(AgentEvent::Sent {
            blocks: Cow::Owned(blocks),
            at: self.clock,
        });
        for held in &mut self.queue {
            held.cleared = true;
        }
    }
}

/// A current row under the old enum, for tests that write an old store.
#[cfg(test)]
pub(super) fn legacy_of(event: AgentEvent<'static>) -> LegacyAgentEvent<'static> {
    let mut bytes = BytesMut::new();
    event.encode(&mut bytes).expect("encode an agent event");
    LegacyAgentEvent::decode(&mut &bytes[..]).expect("a current variant decodes under the old enum")
}

/// The replay the old build did over its own rows, kept so a proof can
/// hold the translation to it.
#[cfg(test)]
pub(super) fn legacy_replay(
    events: Vec<LegacyAgentEvent<'static>>,
) -> crate::agent::replay::Replayed {
    use crate::agent::replay::{Replayed, replay};
    let mut history: Vec<Arc<ContextBlock>> = Vec::new();
    let mut context_used = None;
    let mut user = Vec::new();
    let mut mail = Vec::new();
    let mut legacy_queue: Vec<QueuedItem> = Vec::new();
    let mut legacy_turn: Option<Vec<ToolResult>> = None;
    let flush = |turn: &mut Option<Vec<ToolResult>>, history: &mut Vec<Arc<ContextBlock>>| {
        if let Some(results) = turn.take()
            && !results.is_empty()
        {
            history.push(Arc::new(ContextBlock::ToolResults { results }));
        }
    };
    // Current rows fold as `replay` folds them; only the queues need the
    // old rules alongside.
    let fold_current = |event: AgentEvent<'static>,
                        history: &mut Vec<Arc<ContextBlock>>,
                        user: &mut Vec<QueuedInput>,
                        mail: &mut Vec<crate::agent::MailItem>,
                        context_used: &mut Option<u64>| {
        let clears = matches!(
            event,
            AgentEvent::Sent { .. } | AgentEvent::QueueCleared | AgentEvent::Cleared { .. }
        );
        let replied = matches!(event, AgentEvent::Replied { .. });
        let one = replay(vec![event]);
        history.extend(one.history);
        if replied {
            *context_used = one.context_used;
        }
        if clears {
            user.clear();
            mail.clear();
        }
        user.extend(one.user);
        mail.extend(one.mail);
    };
    for event in events {
        match event {
            LegacyAgentEvent::Sent { .. } => {
                flush(&mut legacy_turn, &mut history);
                legacy_queue.clear();
                fold_current(
                    current(event),
                    &mut history,
                    &mut user,
                    &mut mail,
                    &mut context_used,
                );
            }
            LegacyAgentEvent::QueueCleared | LegacyAgentEvent::Cleared { .. } => {
                legacy_queue.clear();
                fold_current(
                    current(event),
                    &mut history,
                    &mut user,
                    &mut mail,
                    &mut context_used,
                );
            }
            LegacyAgentEvent::InferenceResponse {
                items,
                provider_response_id,
                context_used: response_context_used,
            } => {
                let compacted = items
                    .iter()
                    .any(|item| matches!(item, InferenceResponseItem::Compaction { .. }));
                if compacted {
                    context_used = None;
                } else if response_context_used.is_some() {
                    context_used = response_context_used;
                }
                flush(&mut legacy_turn, &mut history);
                let has_calls = items
                    .iter()
                    .any(|item| matches!(item, InferenceResponseItem::ToolCall { .. }));
                if has_calls {
                    legacy_turn = Some(Vec::new());
                }
                history.push(Arc::new(ContextBlock::InferenceResponse {
                    items: items.into_owned(),
                    provider_response_id,
                }));
            }
            LegacyAgentEvent::ToolResult { result } => legacy_turn
                .get_or_insert_default()
                .push(result.into_owned()),
            LegacyAgentEvent::Queued(item) => legacy_queue.push(item),
            LegacyAgentEvent::Dequeued { boundary } => {
                let keep_mid_turn =
                    boundary == LegacyDelivery::NextRequest && legacy_turn.is_some();
                flush(&mut legacy_turn, &mut history);
                if keep_mid_turn {
                    legacy_turn = Some(Vec::new());
                }
                let (delivered, held): (Vec<_>, Vec<_>) = std::mem::take(&mut legacy_queue)
                    .into_iter()
                    .partition(|item| {
                        boundary == LegacyDelivery::NextTurn
                            || item.delivery != LegacyDelivery::NextTurn
                    });
                legacy_queue = held;
                history.extend(
                    delivered
                        .into_iter()
                        .map(|item| Arc::new(delivered_block(item))),
                );
            }
            LegacyAgentEvent::PresentationUpdated { .. } => {}
            other => {
                fold_current(
                    current(other),
                    &mut history,
                    &mut user,
                    &mut mail,
                    &mut context_used,
                );
            }
        }
    }
    flush(&mut legacy_turn, &mut history);
    for item in legacy_queue {
        if let Some(input) = accepted(&item, UnixMs(0)) {
            let one = replay(vec![AgentEvent::Accepted(input)]);
            user.extend(one.user);
            mail.extend(one.mail);
        }
    }
    let mut replayed = Replayed {
        history,
        owed: Vec::new(),
        user,
        mail,
        context_used,
    };
    replayed.owed = crate::agent::replay::owed_calls(&replayed.history);
    replayed
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use rho_core::{ContentPart, ToolCallId, ToolName, ToolOutput, ToolOutputStatus, ToolType};
    use senax_encoder::{Decode, Encode};

    use super::*;
    use crate::agent::replay::replay;
    use crate::db::AgentIdDomain;

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct TestProviderData {
        item_id: String,
    }

    impl senax_encoder::TaggedSenax for TestProviderData {
        const TAG: &'static str = "rho-agent-legacy-test.provider-data";
    }

    fn provider_data() -> Box<dyn rho_core::ProviderSpecificData> {
        Box::new(TestProviderData {
            item_id: "item".to_owned(),
        })
    }

    fn text_parts(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text {
            text: text.to_owned(),
        }]
    }

    fn queued(
        sender: MessageSender,
        text: &str,
        delivery: LegacyDelivery,
    ) -> LegacyAgentEvent<'static> {
        LegacyAgentEvent::Queued(QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender,
                content: Arc::new(text_parts(text)),
                source_id: None,
            },
            delivery,
        })
    }

    fn response(
        items: Vec<InferenceResponseItem>,
        context_used: Option<u64>,
    ) -> LegacyAgentEvent<'static> {
        LegacyAgentEvent::InferenceResponse {
            items: Cow::Owned(items),
            provider_response_id: None,
            context_used,
        }
    }

    fn tool_call(id: &str) -> InferenceResponseItem {
        InferenceResponseItem::ToolCall {
            provider_specific: provider_data(),
            id: ToolCallId::try_from(id).unwrap(),
            name: ToolName::try_from("shell_command").unwrap(),
            tool_type: ToolType::Function,
            arguments: String::new(),
        }
    }

    fn tool_result(id: &str, at: u64) -> LegacyAgentEvent<'static> {
        LegacyAgentEvent::ToolResult {
            result: Cow::Owned(ToolResult {
                call_id: ToolCallId::try_from(id).unwrap(),
                tool_type: ToolType::Function,
                body: ToolOutput {
                    images: Arc::new(Vec::new()),
                    output: Arc::new("ok".to_owned()),
                    status: ToolOutputStatus::Success,
                },
                started_at: UnixMs(at),
                finished_at: UnixMs(at),
                metadata: None,
            }),
        }
    }

    fn translated(events: Vec<LegacyAgentEvent<'static>>) -> Vec<AgentEvent<'static>> {
        let mut translator = Translator::new(UnixMs(1));
        let mut out = Vec::new();
        for event in events {
            out.extend(translator.translate(event));
        }
        out.extend(translator.finish());
        out
    }

    /// What a reader rebuilds from the translation is what the old build
    /// rebuilt from its rows, times aside.
    fn same_replay(events: Vec<LegacyAgentEvent<'static>>) {
        let old = legacy_replay(events.clone());
        let new = replay(translated(events));
        assert_eq!(new.history, old.history);
        assert_eq!(new.owed, old.owed);
        assert_eq!(new.context_used, old.context_used);
        let dated = |inputs: &[QueuedInput]| {
            inputs
                .iter()
                .cloned()
                .map(|input| QueuedInput {
                    at: UnixMs(0),
                    ..input
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(dated(&new.user), dated(&old.user));
        assert_eq!(
            new.mail
                .iter()
                .map(|m| (m.sender, m.content.clone()))
                .collect::<Vec<_>>(),
            old.mail
                .iter()
                .map(|m| (m.sender, m.content.clone()))
                .collect::<Vec<_>>()
        );
    }

    fn agent(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &AgentIdDomain(7)).expect("counter fits")
    }

    #[test]
    fn a_delivery_at_turn_end_becomes_the_send() {
        let events = vec![
            queued(MessageSender::User, "hi", LegacyDelivery::Immediate),
            queued(
                MessageSender::Agent { id: agent(1) },
                "done",
                LegacyDelivery::NextRequest,
            ),
            LegacyAgentEvent::Dequeued {
                boundary: LegacyDelivery::NextTurn,
            },
        ];
        let rows = translated(events.clone());
        assert!(matches!(rows[0], AgentEvent::Accepted(_)));
        assert!(matches!(rows[1], AgentEvent::Accepted(_)));
        assert!(matches!(&rows[2], AgentEvent::Sent { blocks, .. } if blocks.len() == 2));
        assert_eq!(rows.len(), 3);
        same_replay(events);
    }

    #[test]
    fn a_next_turn_item_held_mid_turn_is_accepted_again_at_the_end() {
        let events = vec![
            queued(MessageSender::User, "steer", LegacyDelivery::NextRequest),
            queued(MessageSender::User, "later", LegacyDelivery::NextTurn),
            LegacyAgentEvent::Dequeued {
                boundary: LegacyDelivery::NextRequest,
            },
        ];
        let rows = translated(events.clone());
        assert_eq!(rows.len(), 4, "two accepted, the send, the held one again");
        assert!(matches!(
            &rows[3],
            AgentEvent::Accepted(QueuedInput { at: UnixMs(0), .. })
        ));
        same_replay(events);
    }

    #[test]
    fn results_ride_one_send_before_the_next_reply() {
        let events = vec![
            response(vec![tool_call("a"), tool_call("b")], Some(40)),
            tool_result("a", 5),
            tool_result("b", 6),
            response(vec![], None),
            response(vec![tool_call("c")], None),
            tool_result("c", 9),
        ];
        let rows = translated(events.clone());
        assert!(matches!(
            &rows[0],
            AgentEvent::Replied {
                context_used: Some(40),
                at: UnixMs(1),
                ..
            }
        ));
        assert!(
            matches!(&rows[1], AgentEvent::Sent { blocks, at: UnixMs(6) } if blocks.len() == 1)
        );
        assert!(matches!(
            &rows[2],
            AgentEvent::Replied {
                context_used: Some(40),
                ..
            }
        ));
        assert!(matches!(&rows[4], AgentEvent::Sent { at: UnixMs(9), .. }));
        assert_eq!(rows.len(), 5);
        same_replay(events);
    }

    #[test]
    fn a_queue_cleared_by_a_send_and_never_delivered_is_accepted_again() {
        let events = vec![
            response(vec![tool_call("a")], None),
            queued(
                MessageSender::User,
                "while it runs",
                LegacyDelivery::NextRequest,
            ),
            tool_result("a", 5),
            response(vec![], None),
        ];
        same_replay(events);
    }

    #[test]
    fn current_rows_cross_unchanged() {
        let event = legacy_of(AgentEvent::QueueCleared);
        assert_eq!(current(event), AgentEvent::QueueCleared);
    }
}
