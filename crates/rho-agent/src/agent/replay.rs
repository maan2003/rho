//! Rebuilding an agent from its log: the history the next request sends,
//! the calls it owes an answer to, and what was queued when it stopped.
//!
//! Two generations of events share the log. The current loop's events say
//! what reached the model outright (`Sent`, `Replied`); the previous loop's
//! say what it observed (`InferenceResponse`, `ToolResult`, `Queued`,
//! `Dequeued`) and are folded with that loop's own rules, so an agent that
//! lived through both loops replays the same either way.

use std::sync::Arc;

use rho_core::{ContextBlock, InferenceResponseItem, MessageSender, ToolCall, ToolResult, UnixMs};

use super::MailItem;
use crate::{AgentEvent, InputKind, LegacyDelivery, QueuedInput, QueuedItem, QueuedItemKind};

#[derive(Default)]
pub(super) struct Replayed {
    pub history: Vec<Arc<ContextBlock>>,
    /// Calls history left hanging: every one of them gets a placeholder
    /// result in the next request (`SPEC-restart-recovery`).
    pub owed: Vec<ToolCall>,
    pub user: Vec<QueuedInput>,
    pub mail: Vec<MailItem>,
    pub context_used: Option<u64>,
}

/// The previous loop's open tool batch: results it had collected and not
/// yet committed into a block.
#[derive(Default)]
struct LegacyTurn {
    completed: Vec<ToolResult>,
}

pub(super) fn replay(events: Vec<AgentEvent<'static>>) -> Replayed {
    let mut history: Vec<Arc<ContextBlock>> = Vec::new();
    let mut context_used = None;
    let mut user = Vec::new();
    let mut mail = Vec::new();
    let mut legacy_queue: Vec<QueuedItem> = Vec::new();
    let mut legacy_turn: Option<LegacyTurn> = None;

    let flush = |turn: &mut Option<LegacyTurn>, history: &mut Vec<Arc<ContextBlock>>| {
        if let Some(turn) = turn.take()
            && !turn.completed.is_empty()
        {
            history.push(Arc::new(ContextBlock::ToolResults {
                results: turn.completed,
            }));
        }
    };

    for event in events {
        match event {
            AgentEvent::Accepted(input) => queue(input, &mut user, &mut mail),
            AgentEvent::QueueCleared => {
                user.clear();
                mail.clear();
                legacy_queue.clear();
            }
            // A drain is total: whatever was queued rode in these blocks.
            AgentEvent::Sent { blocks } => {
                flush(&mut legacy_turn, &mut history);
                history.extend(blocks.into_owned().into_iter().map(Arc::new));
                user.clear();
                mail.clear();
                legacy_queue.clear();
            }
            AgentEvent::Replied {
                blocks,
                context_used: replied,
            } => {
                history.extend(blocks.into_owned().into_iter().map(Arc::new));
                context_used = replied;
            }
            AgentEvent::InferenceResponse {
                items,
                provider_response_id,
                context_used: response_context_used,
            } => {
                let compacted = items
                    .iter()
                    .any(|item| matches!(item, InferenceResponseItem::Compaction { .. }));
                if compacted {
                    // Compaction response usage describes the old, full
                    // input, not the newly compacted context.
                    context_used = None;
                } else if response_context_used.is_some() {
                    context_used = response_context_used;
                }
                flush(&mut legacy_turn, &mut history);
                let has_calls = items
                    .iter()
                    .any(|item| matches!(item, InferenceResponseItem::ToolCall { .. }));
                if has_calls {
                    legacy_turn = Some(LegacyTurn::default());
                }
                history.push(Arc::new(ContextBlock::InferenceResponse {
                    items: items.into_owned(),
                    provider_response_id,
                }));
            }
            AgentEvent::ToolResult { result } => legacy_turn
                .get_or_insert_default()
                .completed
                .push(result.into_owned()),
            AgentEvent::Queued(item) => legacy_queue.push(item),
            AgentEvent::Dequeued { boundary } => {
                // A mid-turn delivery committed the batch so far and kept
                // the turn open; a turn boundary closed it.
                let keep_mid_turn =
                    boundary == LegacyDelivery::NextRequest && legacy_turn.is_some();
                flush(&mut legacy_turn, &mut history);
                if keep_mid_turn {
                    legacy_turn = Some(LegacyTurn::default());
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
            // Config and creation are the head's business, never context.
            AgentEvent::PresentationUpdated { .. }
            | AgentEvent::ClaudePresentationSource { .. }
            | AgentEvent::Created { .. }
            | AgentEvent::RoleChanged { .. }
            | AgentEvent::WorkdirAdded { .. }
            | AgentEvent::RuntimeRebound { .. } => {}
        }
    }
    flush(&mut legacy_turn, &mut history);
    // What the previous loop still had queued when it stopped, dated at the
    // epoch: it has waited long enough that every patience has run out.
    for item in legacy_queue {
        match item.kind {
            QueuedItemKind::UserMessage {
                sender, content, ..
            } => queue(
                QueuedInput {
                    source: sender,
                    kind: InputKind::Message {
                        content: Arc::try_unwrap(content)
                            .unwrap_or_else(|content| (*content).clone()),
                    },
                    delivery: item.delivery.into(),
                    at: UnixMs(0),
                },
                &mut user,
                &mut mail,
            ),
            QueuedItemKind::Compaction => user.push(QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Compaction,
                delivery: item.delivery.into(),
                at: UnixMs(0),
            }),
            // Progress of a tool that no longer exists.
            QueuedItemKind::ToolUpdate(_) => {}
        }
    }
    let owed = owed_calls(&history);
    Replayed {
        history,
        owed,
        user,
        mail,
        context_used,
    }
}

fn queue(input: QueuedInput, user: &mut Vec<QueuedInput>, mail: &mut Vec<MailItem>) {
    match (input.source, input.kind) {
        (MessageSender::Agent { id }, InputKind::Message { content }) => mail.push(MailItem {
            sender: id,
            content,
            at: input.at,
        }),
        // Only the user compacts; a peer asking for it is not a thing.
        (MessageSender::Agent { .. }, InputKind::Compaction) => {}
        (MessageSender::User, kind) => user.push(QueuedInput {
            source: MessageSender::User,
            kind,
            delivery: input.delivery,
            at: input.at,
        }),
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

/// Every call in history that no result answers, in the order made.
fn owed_calls(history: &[Arc<ContextBlock>]) -> Vec<ToolCall> {
    let mut unanswered: Vec<ToolCall> = Vec::new();
    for block in history {
        match &**block {
            ContextBlock::InferenceResponse { items, .. } => {
                unanswered.extend(items.iter().filter_map(|item| match item {
                    InferenceResponseItem::ToolCall {
                        id,
                        name,
                        tool_type,
                        arguments,
                        ..
                    } => Some(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        tool_type: *tool_type,
                        arguments: arguments.clone(),
                    }),
                    _ => None,
                }));
            }
            ContextBlock::ToolResults { results } => {
                unanswered.retain(|call| !results.iter().any(|result| result.call_id == call.id));
            }
            ContextBlock::UserMessage { .. }
            | ContextBlock::ToolUpdate(_)
            | ContextBlock::CompactionTrigger => {}
        }
    }
    unanswered
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use rho_core::{
        ContentPart, MessageDelivery, ToolCallId, ToolName, ToolOutput, ToolOutputStatus,
    };
    use senax_encoder::{Decode, Encode};

    use super::*;
    use crate::db::{AgentId, AgentIdDomain};

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct TestProviderData {
        item_id: String,
    }

    impl senax_encoder::TaggedSenax for TestProviderData {
        const TAG: &'static str = "rho-agent-replay-test.provider-data";
    }

    fn provider_data() -> Box<dyn rho_core::ProviderSpecificData> {
        Box::new(TestProviderData {
            item_id: "item".to_owned(),
        })
    }

    fn agent_id(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &AgentIdDomain(7)).expect("counter fits")
    }

    fn text_parts(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text {
            text: text.to_owned(),
        }]
    }

    fn queued_event(
        sender: MessageSender,
        text: &str,
        delivery: LegacyDelivery,
    ) -> AgentEvent<'static> {
        AgentEvent::Queued(QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender,
                content: Arc::new(text_parts(text)),
                source_id: None,
            },
            delivery,
        })
    }

    fn response_event(items: Vec<InferenceResponseItem>) -> AgentEvent<'static> {
        AgentEvent::InferenceResponse {
            items: Cow::Owned(items),
            provider_response_id: None,
            context_used: None,
        }
    }

    fn tool_call(id: &str) -> InferenceResponseItem {
        InferenceResponseItem::ToolCall {
            provider_specific: provider_data(),
            id: ToolCallId::try_from(id).unwrap(),
            name: ToolName::try_from("shell_command").unwrap(),
            tool_type: rho_core::ToolType::Function,
            arguments: String::new(),
        }
    }

    fn tool_result(id: &str) -> AgentEvent<'static> {
        AgentEvent::ToolResult {
            result: Cow::Owned(ToolResult {
                call_id: ToolCallId::try_from(id).unwrap(),
                tool_type: rho_core::ToolType::Function,
                body: ToolOutput {
                    images: Arc::new(Vec::new()),
                    output: Arc::new("ok".to_owned()),
                    status: ToolOutputStatus::Success,
                },
                started_at: UnixMs(0),
                finished_at: UnixMs(0),
                metadata: None,
            }),
        }
    }

    #[test]
    fn legacy_dequeue_at_turn_end_delivers_user_and_mail() {
        let replayed = replay(vec![
            queued_event(MessageSender::User, "hi", LegacyDelivery::Immediate),
            queued_event(
                MessageSender::Agent { id: agent_id(1) },
                "done",
                LegacyDelivery::NextRequest,
            ),
            AgentEvent::Dequeued {
                boundary: LegacyDelivery::NextTurn,
            },
        ]);
        assert_eq!(replayed.history.len(), 2);
        assert_eq!(
            *replayed.history[1],
            ContextBlock::UserMessage {
                sender: MessageSender::Agent { id: agent_id(1) },
                content: text_parts("done")
            }
        );
        assert!(replayed.user.is_empty() && replayed.mail.is_empty());
        assert!(replayed.owed.is_empty());
    }

    #[test]
    fn legacy_next_turn_lane_held_mid_turn_then_queued_as_next_request() {
        let replayed = replay(vec![
            queued_event(MessageSender::User, "steer", LegacyDelivery::NextRequest),
            queued_event(MessageSender::User, "later", LegacyDelivery::NextTurn),
            AgentEvent::Dequeued {
                boundary: LegacyDelivery::NextRequest,
            },
        ]);
        assert_eq!(replayed.history.len(), 1);
        assert_eq!(replayed.user.len(), 1);
        assert_eq!(replayed.user[0].delivery, MessageDelivery::NextRequest);
        assert_eq!(replayed.user[0].at, UnixMs(0));
    }

    #[test]
    fn legacy_unanswered_calls_are_owed_and_answered_ones_are_committed() {
        let replayed = replay(vec![
            response_event(vec![tool_call("a"), tool_call("b")]),
            tool_result("a"),
        ]);
        assert_eq!(
            replayed.history.len(),
            2,
            "the answered call's result is a block"
        );
        assert_eq!(replayed.owed.len(), 1);
        assert_eq!(replayed.owed[0].id.as_str(), "b");
    }

    #[test]
    fn current_events_replay_verbatim_and_a_send_empties_the_queues() {
        let input = QueuedInput {
            source: MessageSender::User,
            kind: InputKind::Message {
                content: text_parts("go"),
            },
            delivery: MessageDelivery::NextRequest,
            at: UnixMs(5),
        };
        let replayed = replay(vec![
            AgentEvent::Accepted(input.clone()),
            AgentEvent::Accepted(QueuedInput {
                source: MessageSender::Agent { id: agent_id(2) },
                ..input.clone()
            }),
        ]);
        assert_eq!(replayed.user, vec![input.clone()]);
        assert_eq!(replayed.mail.len(), 1);

        let replayed = replay(vec![
            AgentEvent::Accepted(input.clone()),
            AgentEvent::Sent {
                blocks: Cow::Owned(vec![ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content: text_parts("go"),
                }]),
            },
            AgentEvent::Replied {
                blocks: Cow::Owned(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("c")],
                    provider_response_id: None,
                }]),
                context_used: Some(40),
            },
        ]);
        assert!(replayed.user.is_empty());
        assert_eq!(replayed.history.len(), 2);
        assert_eq!(replayed.context_used, Some(40));
        assert_eq!(replayed.owed.len(), 1, "the call the log ends on is owed");
    }

    #[test]
    fn a_cleared_queue_stays_cleared() {
        let replayed = replay(vec![
            queued_event(MessageSender::User, "old", LegacyDelivery::NextRequest),
            AgentEvent::Accepted(QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Compaction,
                delivery: MessageDelivery::NextRequest,
                at: UnixMs(1),
            }),
            AgentEvent::QueueCleared,
        ]);
        assert!(replayed.user.is_empty());
        assert!(replayed.history.is_empty());
    }
}
