//! Rebuilding an agent from its log: the history the next request sends,
//! the calls it owes an answer to, and what was queued when it stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rho_core::{ContextBlock, InferenceResponseItem, MessageSender};

use super::MailItem;
use crate::{AgentEvent, InputKind, QueuedInput};

#[derive(Default)]
pub(crate) struct Replayed {
    pub history: Vec<Arc<ContextBlock>>,
    pub(super) context: super::context::Window,
    pub recovery_notes: Vec<String>,
    pub recovery_blocks: Vec<ContextBlock>,
    pub recovery_streams: Vec<rho_core::ToolCallId>,
    /// Calls history left hanging: every one of them gets a placeholder
    /// result in the next request (`SPEC-restart-recovery`).
    pub owed: Vec<rho_core::ExecId>,
    pub user: Vec<QueuedInput>,
    pub mail: Vec<MailItem>,
    pub context_used: Option<u64>,
}

pub(crate) fn replay(events: Vec<AgentEvent<'static>>) -> Replayed {
    let mut history: Vec<Arc<ContextBlock>> = Vec::new();
    let mut context_used = None;
    let mut context = super::context::Window::default();
    let mut streams =
        BTreeMap::<rho_core::ToolCallId, (rho_core::InferenceResponseItem, String, usize)>::new();
    let mut canonical = BTreeSet::new();
    let mut recovery_notes = Vec::new();
    let mut recovery_blocks = Vec::new();
    let mut recovery_streams = Vec::new();
    let mut user = Vec::new();
    let mut mail = Vec::new();

    let mut notes_rotation = false;
    for event in events {
        if let Some(native) = event.native_event() {
            use crate::native::NativeEvent;
            let blocks = match native {
                NativeEvent::RequestStarted {
                    input,
                    context: change,
                    ..
                } => {
                    let blocks = input.clone();
                    if !matches!(change, Some(crate::ContextChange::Preparing { .. })) {
                        user.clear();
                        mail.clear();
                    }
                    if let Some(change) = change {
                        context.sent(change);
                    } else {
                        if blocks.contains(&ContextBlock::CompactionTrigger) {
                            context.rotated();
                        }
                        if blocks
                            .iter()
                            .any(|block| matches!(block, ContextBlock::ContextRotation { .. }))
                        {
                            context.rotated();
                            context_used = None;
                        }
                    }
                    blocks
                }
                NativeEvent::ResponseFinished {
                    output,
                    context_used: replied,
                    ..
                } => {
                    for block in output {
                        if let ContextBlock::InferenceResponse { items, .. } = block {
                            context.replied(items);
                        }
                    }
                    context_used = *replied;
                    output.clone()
                }
                NativeEvent::RequestFailed { .. } => Vec::new(),
                NativeEvent::PythonStream { event, .. } => {
                    use crate::PythonStreamEvent;
                    match event {
                        PythonStreamEvent::Opened { item } => {
                            if let InferenceResponseItem::ToolCall { id, .. } = item {
                                streams.insert(id.clone(), (item.clone(), String::new(), 0));
                            }
                        }
                        PythonStreamEvent::Admitted { call_id, source } => {
                            if let Some((_, admitted, _)) = streams.get_mut(call_id) {
                                admitted.push_str(source);
                            }
                        }
                        PythonStreamEvent::Settled {
                            call_id,
                            end,
                            error,
                        } => {
                            if let Some((_, _, completed)) = streams.get_mut(call_id)
                                && error.is_none()
                            {
                                *completed = *end as usize;
                            }
                        }
                        PythonStreamEvent::Closed { .. } => {}
                        PythonStreamEvent::Acknowledged { call_id } => {
                            streams.remove(call_id);
                        }
                    }
                    Vec::new()
                }
            };
            for block in &blocks {
                if let ContextBlock::InferenceResponse { items, .. } = block {
                    for item in items {
                        if let InferenceResponseItem::ToolCall { id, .. } = item {
                            canonical.insert(id.clone());
                        }
                    }
                }
            }
            history.extend(blocks.into_iter().map(Arc::new));
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
    for (id, (mut item, source, completed)) in streams {
        recovery_streams.push(id.clone());
        if source.is_empty() {
            continue;
        }
        if !canonical.contains(&id) {
            let InferenceResponseItem::ToolCall { arguments, .. } = &mut item else {
                unreachable!()
            };
            *arguments = source.clone();
            recovery_blocks.push(ContextBlock::InferenceResponse {
                items: vec![item],
                provider_response_id: None,
            });
        }
        recovery_notes.push(format!(
            "Rho restarted: Python state and command handles are gone. {}",
            super::streaming::progress_note(
                &id, &source, completed, source.len(),
                "execution was interrupted by restart; these statements may have executed partially",
            ).replace(
                "existing command handles and their fresh output remain authoritative",
                "external side effects may remain, but old command handles cannot be used"
            ),
        ));
    }
    if context.preparation.take().is_some() {
        recovery_notes.push(
            "Rho restarted during context-rotation preparation. Notes may already have been \
             written; inspect them rather than replaying preparation code. The old context \
             has not been removed."
                .into(),
        );
    }
    let mut recovery_history = history.clone();
    recovery_history.extend(recovery_blocks.iter().cloned().map(Arc::new));
    let owed = owed_calls(&recovery_history);
    Replayed {
        history,
        context,
        recovery_notes,
        recovery_blocks,
        recovery_streams,
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
pub(crate) fn owed_calls(history: &[Arc<ContextBlock>]) -> Vec<rho_core::ExecId> {
    let mut unanswered: Vec<rho_core::ExecId> = Vec::new();
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
            | ContextBlock::ContextRotation { .. } => {}
        }
    }
    unanswered
}

#[cfg(test)]
mod tests {

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
    fn native_records_project_the_same_protocol_history_without_a_second_authority() {
        let legacy = vec![
            AgentEvent::Native(crate::native::NativeEvent::RequestStarted { input: Vec::from(vec![ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content: text_parts("go"),
                }]), at: UnixMs(1), wake: None, context: None }),
            AgentEvent::Native(crate::native::NativeEvent::ResponseFinished { output: Vec::from(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("same-id")],
                    provider_response_id: Some("response-1".try_into().unwrap()),
                }]), context_used: Some(42), usage: None, at: UnixMs(2) }),
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
            AgentEvent::Native(crate::native::NativeEvent::RequestStarted { input: Vec::from(vec![ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content: text_parts("go"),
                }]), at: rho_core::UnixMs(0), wake: None, context: None }),
            AgentEvent::Native(crate::native::NativeEvent::ResponseFinished { output: Vec::from(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("c")],
                    provider_response_id: None,
                }]), context_used: Some(40), usage: None, at: rho_core::UnixMs(0) }),
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
    fn streaming_crash_cuts_preserve_admission_and_recovery_is_idempotent() {
        use crate::PythonStreamEvent;
        let id = rho_core::ToolCallId::try_from("c").unwrap();
        let source = "command('touch marker')\n";
        let journal = [
            PythonStreamEvent::Opened {
                item: tool_call("c"),
            },
            PythonStreamEvent::Admitted {
                call_id: id.clone(),
                source: source.into(),
            },
            PythonStreamEvent::Settled {
                call_id: id.clone(),
                end: source.len() as u64,
                error: None,
            },
        ];
        for cut in 1..=journal.len() {
            let events: Vec<_> = journal[..cut]
                .iter()
                .cloned()
                .map(|event| AgentEvent::Native(crate::native::NativeEvent::PythonStream { event, at: rho_core::UnixMs(0) }))
                .collect();
            let replayed = replay(events.clone());
            if cut == 1 {
                assert!(replayed.owed.is_empty());
                assert!(replayed.recovery_blocks.is_empty());
                assert!(replayed.recovery_notes.is_empty());
                continue;
            }
            assert_eq!(replayed.owed.len(), 1);
            assert_eq!(replayed.recovery_blocks.len(), 1);
            assert!(
                replayed.history.is_empty(),
                "load must not invent already-persisted history"
            );
            let completed = if cut == 3 { source.len() } else { 0 };
            assert!(replayed.recovery_notes[0].contains(&format!("bytes 0..{completed}")));
            let mut blocks = replayed.recovery_blocks;
            blocks.push(ContextBlock::ToolResults {
                results: vec![rho_core::ToolResult {
                    call_id: id.clone(),
                    tool_type: rho_core::ToolType::Function,
                    body: rho_core::ToolOutput {
                        output: Arc::new(String::new()),
                        full_output: None,
                        images: Arc::new(Vec::new()),
                        status: rho_core::ToolOutputStatus::Cancelled,
                    },
                    started_at: rho_core::UnixMs(1),
                    finished_at: rho_core::UnixMs(1),
                    metadata: None,
                }],
            });
            let mut events = events;
            events.push(AgentEvent::Native(crate::native::NativeEvent::RequestStarted { input: Vec::from(blocks), at: rho_core::UnixMs(1), wake: None, context: None }));
            let twice = replay(events);
            assert!(twice.owed.is_empty());
            assert!(
                twice.recovery_blocks.is_empty(),
                "the recovery call must not be inserted twice"
            );
        }
    }

    #[test]
    fn a_later_success_does_not_erase_an_earlier_unsettled_stream() {
        use crate::PythonStreamEvent;
        let replayed = replay(vec![
            AgentEvent::Native(crate::native::NativeEvent::PythonStream { event: PythonStreamEvent::Opened {
                    item: tool_call("first"),
                }, at: rho_core::UnixMs(0) }),
            AgentEvent::Native(crate::native::NativeEvent::PythonStream { event: PythonStreamEvent::Admitted {
                    call_id: rho_core::ToolCallId::try_from("first").unwrap(),
                    source: "await work()\n".into(),
                }, at: rho_core::UnixMs(0) }),
            AgentEvent::Native(crate::native::NativeEvent::ResponseFinished { output: Vec::from(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("first")],
                    provider_response_id: None,
                }]), context_used: None, usage: None, at: rho_core::UnixMs(1) }),
            AgentEvent::Native(crate::native::NativeEvent::ResponseFinished { output: Vec::from(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("second")],
                    provider_response_id: None,
                }]), context_used: None, usage: None, at: rho_core::UnixMs(2) }),
        ]);
        assert_eq!(replayed.owed.len(), 2);
        assert!(replayed.recovery_blocks.is_empty());
        assert!(replayed.recovery_notes[0].contains("await work()"));
    }

    #[test]
    fn closed_and_replied_do_not_retire_progress_before_durable_acknowledgement() {
        use crate::PythonStreamEvent;
        let id = rho_core::ToolCallId::try_from("c").unwrap();
        let journal = |event| AgentEvent::Native(crate::native::NativeEvent::PythonStream { event, at: rho_core::UnixMs(0) });
        for close_first in [false, true] {
            let mut events = vec![
                journal(PythonStreamEvent::Opened {
                    item: tool_call("c"),
                }),
                journal(PythonStreamEvent::Admitted {
                    call_id: id.clone(),
                    source: "work()\n".into(),
                }),
                journal(PythonStreamEvent::Settled {
                    call_id: id.clone(),
                    end: 7,
                    error: None,
                }),
            ];
            let close = journal(PythonStreamEvent::Closed {
                call_id: id.clone(),
            });
            let reply = AgentEvent::Native(crate::native::NativeEvent::ResponseFinished { output: Vec::from(vec![ContextBlock::InferenceResponse {
                    items: vec![tool_call("c")],
                    provider_response_id: None,
                }]), context_used: None, usage: None, at: rho_core::UnixMs(0) });
            events.extend(if close_first {
                vec![close, reply]
            } else {
                vec![reply, close]
            });
            let before = replay(events.clone());
            assert!(before.recovery_notes[0].contains("bytes 0..7"));
            assert!(before.recovery_blocks.is_empty());
            events.push(journal(PythonStreamEvent::Acknowledged {
                call_id: id.clone(),
            }));
            assert!(replay(events).recovery_notes.is_empty());
        }
    }
}
