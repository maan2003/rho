//! Rebuilding an agent from its log: the history the next request sends,
//! the calls it owes an answer to, and what was queued when it stopped.

use std::sync::Arc;

use rho_core::{ContextBlock, InferenceResponseItem, MessageSender, ToolCall};

use super::MailItem;
use crate::{AgentEvent, InputKind, QueuedInput};

#[derive(Default)]
pub(crate) struct Replayed {
    pub history: Vec<Arc<ContextBlock>>,
    /// Calls history left hanging: every one of them gets a placeholder
    /// result in the next request (`SPEC-restart-recovery`).
    pub owed: Vec<ToolCall>,
    pub user: Vec<QueuedInput>,
    pub mail: Vec<MailItem>,
    pub context_used: Option<u64>,
}

pub(crate) fn replay(events: Vec<AgentEvent<'static>>) -> Replayed {
    let mut history: Vec<Arc<ContextBlock>> = Vec::new();
    let mut context_used = None;
    let mut user = Vec::new();
    let mut mail = Vec::new();

    for event in events {
        match event {
            AgentEvent::Accepted(input) => queue(input, &mut user, &mut mail),
            AgentEvent::QueueCleared | AgentEvent::Cleared { .. } => {
                user.clear();
                mail.clear();
            }
            // A drain is total: whatever was queued rode in these blocks.
            AgentEvent::Sent { blocks, .. } => {
                history.extend(blocks.into_owned().into_iter().map(Arc::new));
                user.clear();
                mail.clear();
            }
            AgentEvent::Replied {
                blocks,
                context_used: replied,
                ..
            } => {
                history.extend(blocks.into_owned().into_iter().map(Arc::new));
                context_used = replied;
            }
            // Config, creation and what a reader is told are the head's
            // business, never context. A `Rewound` never reaches replay:
            // the read that hands over the visible log has applied it.
            AgentEvent::ClaudePresentationSource { .. }
            | AgentEvent::Transcript { .. }
            | AgentEvent::Turn { .. }
            | AgentEvent::Presented { .. }
            | AgentEvent::Wants { .. }
            | AgentEvent::Rewound { .. }
            | AgentEvent::Failed { .. }
            | AgentEvent::Created { .. }
            | AgentEvent::RoleChanged { .. }
            | AgentEvent::WorkdirAdded { .. }
            | AgentEvent::RuntimeRebound { .. } => {}
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

/// Every call in history that no result answers, in the order made.
pub(crate) fn owed_calls(history: &[Arc<ContextBlock>]) -> Vec<ToolCall> {
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

    use rho_core::{ContentPart, MessageDelivery, UnixMs};
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

    fn tool_call(id: &str) -> InferenceResponseItem {
        InferenceResponseItem::ToolCall {
            provider_specific: provider_data(),
            id: rho_core::ToolCallId::try_from(id).unwrap(),
            name: rho_core::ToolName::try_from("shell_command").unwrap(),
            tool_type: rho_core::ToolType::Function,
            arguments: String::new(),
        }
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
                at: rho_core::UnixMs(0),
            },
            AgentEvent::Replied {
                blocks: Cow::Owned(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("c")],
                    provider_response_id: None,
                }]),
                context_used: Some(40),
                usage: None,
                at: rho_core::UnixMs(0),
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
