//! Rebuilding an agent from its log: the history the next request sends,
//! the calls it owes an answer to, and what was queued when it stopped.

use std::sync::Arc;

use rho_inference::types::{ContextBlock, InferenceResponseItem, MessageSender};

use super::MailItem;
use crate::{AgentEvent, InputKind, QueuedInput};

#[derive(Default)]
pub(crate) struct Replayed {
    pub history: Vec<Arc<ContextBlock>>,
    pub(super) usage_caps: super::context::UsageCaps,
    pub(super) context: super::context::Window,
    pub recovery_notes: Vec<String>,
    /// Calls history left hanging: every one of them gets a placeholder
    /// result in the next request (`SPEC-restart-recovery`).
    pub owed: Vec<rho_inference::types::ExecId>,
    pub user: Vec<QueuedInput>,
    pub mail: Vec<MailItem>,
    pub context_used: Option<u64>,
}

/// Process restart, unlike live rewind or provider-input projection, discards
/// the notebook and all command handles.
pub(crate) fn recover(events: Vec<AgentEvent<'static>>) -> Replayed {
    let mut replayed = replay(events);
    if !replayed.history.is_empty() && replayed.owed.is_empty() {
        replayed.recovery_notes.push(
            "Rho restarted: Python state and command handles are gone. Recent execution may be \
             absent from this conversation, and external side effects may remain. Do not \
             automatically replay interrupted work; inspect current state and continue with new code."
                .into(),
        );
    }
    replayed
}

pub(crate) fn replay(events: Vec<AgentEvent<'static>>) -> Replayed {
    let mut history: Vec<Arc<ContextBlock>> = Vec::new();
    let mut context_used = None;
    let mut usage_caps = super::context::UsageCaps::default();
    let mut context = super::context::Window::default();
    let mut recovery_notes = Vec::new();
    let mut user = Vec::new();
    let mut mail = Vec::new();

    let mut notes_rotation = false;
    for event in events {
        if let Some(native) = event.native_event() {
            usage_caps.observe(native, &history);
            use crate::native::NativeEvent;
            match native {
                NativeEvent::RequestStarted {
                    input,
                    context: change,
                    ..
                } => {
                    let blocks = input;
                    if !matches!(change, Some(crate::ContextChange::Preparing { .. })) {
                        user.clear();
                        mail.clear();
                    }
                    if let Some(change) = change {
                        context.sent(change);
                    } else {
                        if blocks.contains(&ContextBlock::CompactionTrigger)
                            || blocks.iter().any(|block| matches!(block, ContextBlock::DeveloperMessage { text } if text == super::context::POLICY_CHANGED))
                        {
                            context.rotated();
                        }
                        if blocks.iter().any(|block| {
                            matches!(
                                block,
                                ContextBlock::ContextRotation { .. }
                                    | ContextBlock::ToolHistoryEvicted { .. }
                            )
                        }) {
                            context.rotated();
                            context_used = None;
                        }
                    }
                }
                NativeEvent::ResponseFinished {
                    context_used: replied,
                    ..
                } => {
                    context_used = *replied;
                }
                NativeEvent::RequestFailed { .. } => {}
            };
            history.extend(native.blocks().iter().cloned().map(Arc::new));
            continue;
        }
        match event {
            AgentEvent::Accepted(input) => queue(input, &mut user, &mut mail),
            AgentEvent::Cleared { .. } => {
                user.clear();
                mail.clear();
            }
            AgentEvent::Created { role, .. } => notes_rotation = role.uses_notes_rotation(),
            AgentEvent::RoleChanged { role, .. } => {
                usage_caps.reset();
                let enabled = role.uses_notes_rotation();
                if enabled != notes_rotation {
                    context.rotated();
                    history.push(Arc::new(ContextBlock::DeveloperMessage {
                        text: super::context::POLICY_CHANGED.into(),
                    }));
                }
                notes_rotation = enabled;
            }
            // Config, creation and what a reader is told are the head's
            // business, never context. A `Rewound` never reaches replay:
            // the read that hands over the visible log has applied it.
            AgentEvent::Native(_)
            | AgentEvent::ClaudeOutput { .. }
            | AgentEvent::ClaudeOutputHandedOff { .. }
            | AgentEvent::ClaudeExecAdmitted { .. }
            | AgentEvent::ExecObserved { .. }
            | AgentEvent::Transcript { .. }
            | AgentEvent::Turn { .. }
            | AgentEvent::TitleAttempted { .. }
            | AgentEvent::Titled { .. }
            | AgentEvent::Wants { .. }
            | AgentEvent::Rewound { .. }
            | AgentEvent::Failed { .. }
            | AgentEvent::ModeChanged { .. }
            | AgentEvent::Notice { .. }
            | AgentEvent::RuntimeRebound { .. } => {}
        }
    }
    if context.preparation.take().is_some() {
        recovery_notes.push(
            "Rho restarted during context-rotation preparation. Notes may already have been \
             written; inspect them rather than replaying preparation code. The old context \
             has not been removed."
                .into(),
        );
    }
    let owed = owed_calls(&history);
    Replayed {
        history,
        usage_caps,
        context,
        recovery_notes,
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
pub(crate) fn owed_calls(history: &[Arc<ContextBlock>]) -> Vec<rho_inference::types::ExecId> {
    let mut unanswered: Vec<rho_inference::types::ExecId> = Vec::new();
    for block in history {
        match &**block {
            ContextBlock::InferenceResponse { items, .. } => {
                unanswered.extend(items.iter().filter_map(|item| match item {
                    InferenceResponseItem::ToolCall { id, .. } => Some(id.clone()),
                    _ => None,
                }));
            }
            ContextBlock::ToolResults { results } => {
                unanswered.retain(|call| !results.iter().any(|result| &result.call_id == call));
            }
            ContextBlock::UserMessage { .. }
            | ContextBlock::ToolUpdate(_)
            | ContextBlock::CompactionTrigger
            | ContextBlock::DeveloperMessage { .. }
            | ContextBlock::ContextRotation { .. }
            | ContextBlock::ToolHistoryEvicted { .. } => {}
        }
    }
    unanswered
}

#[cfg(test)]
mod tests {

    use rho_agent_types::{ContentPart, MessageDelivery, UnixMs};
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

    fn provider_data() -> Box<dyn rho_inference::types::ProviderSpecificData> {
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
            id: rho_inference::types::ToolCallId::try_from(id).unwrap(),
            name: rho_inference::types::ToolName::try_from("shell_command").unwrap(),
            tool_type: rho_inference::types::ToolType::Function,
            arguments: String::new(),
        }
    }

    #[test]
    fn native_records_project_the_same_protocol_history_without_a_second_authority() {
        let legacy = vec![
            AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
                input: Vec::from(vec![ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content: text_parts("go"),
                }]),
                at: UnixMs(1),
                wake: None,
                context: None,
            }),
            AgentEvent::Native(crate::native::NativeEvent::ResponseFinished {
                output: Vec::from(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("same-id")],
                    provider_response_id: Some("response-1".try_into().unwrap()),
                }]),
                context_used: Some(42),
                usage: None,
                at: UnixMs(2),
            }),
        ];
        let current = legacy
            .iter()
            .map(|event| AgentEvent::Native(event.native_event().unwrap().clone()))
            .collect();
        let old = replay(legacy);
        let new = replay(current);
        assert_eq!(new.history, old.history);
        assert_eq!(new.owed, old.owed);
        assert_eq!(new.context_used, old.context_used);
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
            AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
                input: Vec::from(vec![ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content: text_parts("go"),
                }]),
                at: rho_agent_types::UnixMs(0),
                wake: None,
                context: None,
            }),
            AgentEvent::Native(crate::native::NativeEvent::ResponseFinished {
                output: Vec::from(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("c")],
                    provider_response_id: None,
                }]),
                context_used: Some(40),
                usage: None,
                at: rho_agent_types::UnixMs(0),
            }),
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
            AgentEvent::Cleared { at: UnixMs(0) },
        ]);
        assert!(replayed.user.is_empty());
        assert!(replayed.history.is_empty());
    }
    #[test]
    fn only_process_recovery_reports_lost_notebook_state() {
        let events = vec![AgentEvent::Native(
            crate::native::NativeEvent::RequestStarted {
                input: vec![ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content: vec![rho_agent_types::ContentPart::Text {
                        text: "saved conversation".into(),
                    }],
                }],
                context: None,
                wake: None,
                at: rho_agent_types::UnixMs(1),
            },
        )];
        assert!(replay(events.clone()).recovery_notes.is_empty());
        let recovered = recover(events);
        assert_eq!(recovered.recovery_notes.len(), 1);
        assert!(recovered.recovery_notes[0].contains("command handles are gone"));
        assert!(recovered.recovery_notes[0].contains("Recent execution may be absent"));
        assert!(recovered.owed.is_empty());
        assert!(recovered.user.is_empty());
        assert!(recovered.mail.is_empty());
        assert!(recover(Vec::new()).recovery_notes.is_empty());
    }

    #[test]
    fn recovery_uses_canonical_calls_without_interpreter_progress() {
        use crate::native::NativeEvent;
        let id = rho_inference::types::ExecId::try_from("c").unwrap();
        let mut events = Vec::new();
        events.push(AgentEvent::Native(NativeEvent::ResponseFinished {
            output: vec![ContextBlock::InferenceResponse {
                items: vec![tool_call("c")],
                provider_response_id: None,
            }],
            context_used: None,
            usage: None,
            at: rho_agent_types::UnixMs(1),
        }));
        let recovered = replay(events.clone());
        assert_eq!(recovered.history.len(), 1);
        assert_eq!(recovered.owed, vec![id.clone()]);

        events.push(AgentEvent::Native(NativeEvent::RequestStarted {
            input: vec![rho_inference::exec::output(
                &rho_inference::types::ExecOutput::Reply {
                    id: id.clone(),
                    body: rho_inference::types::ToolOutput {
                        output: Arc::new(String::new()),
                        full_output: None,
                        images: Default::default(),
                        status: rho_agent_types::ToolOutputStatus::Cancelled,
                    },
                    first_block_at: rho_agent_types::UnixMs(2),
                    at: rho_agent_types::UnixMs(2),
                },
            )],
            context: None,
            wake: None,
            at: rho_agent_types::UnixMs(2),
        }));
        let recovered = replay(events);
        assert!(recovered.owed.is_empty());
        assert_eq!(recovered.history.len(), 2);
        assert!(recovered.recovery_notes.is_empty());
    }
}
