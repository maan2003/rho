//! One format hop from b4e2c7a1. Remove after the migrated build has opened
//! active developer databases. Never normalize legacy rows in live readers.
use redb::TableDefinition;
use rho_core::{ExecOutput, ProviderResponseId};
use rho_db::{RecordedTypeName, SenAs, SenValue, WriteTxn};

use crate::*;

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Event<'a> {
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
        /// Why the request went out when it did; absent on rows from before
        /// the scheduler recorded it.
        #[senax(default)]
        wake: Option<WakeFacts>,
    },
    /// A request boundary that also advances context rotation. Preparation
    /// preserves queued input; other context boundaries drain it normally.
    ContextSent {
        blocks: Cow<'a, [ContextBlock]>,
        change: ContextChange,
        at: UnixMs,
        wake: Option<WakeFacts>,
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

    /// One line of the conversation, as the Claude runtime's stream told
    /// it: a finished content block, a person's message (Claude's echo
    /// of a send), a call's results, a compaction. Rows before 7 Sep
    /// were copied from Claude Code's session file instead and carried
    /// their offset in it, a field a decoder now skips.
    Transcript {
        /// The line's uuid, the same Claude Code's session file gives it
        /// (a rewind forks the session there).
        uuid: uuid::Uuid,
        line: TranscriptLine,
        at: UnixMs,
        /// On a row the notebook produced (an exec call's results, or a
        /// message of output injected into an idle model): why the notebook
        /// spoke when it did.
        #[senax(default)]
        wake: Option<WakeFacts>,
    },

    // -- the runtimes' shared config log --------------------------------------
    /// The agent coming into being: the first event of every agent's log,
    /// and the base the head's config is folded from. A spawn name given
    /// here is why no title is generated for that agent.
    Created {
        role: AgentRole,
        binding: SessionBinding,
        runtime: AgentRuntime,
        place: Place,
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
    /// The agent now sees the filesystem this way: the same workset and
    /// directory, entered in the other mode at its next load.
    ModeChanged {
        mode: WorksetMode,
        #[senax(default)]
        at: UnixMs,
    },
    /// Something Rho has to tell the agent, carried ahead of its next user
    /// message and then done: what a migration did to its place, say.
    Notice {
        text: Cow<'a, str>,
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
    /// Durable admission and settlement of streaming Python units.
    PythonStream {
        event: PythonStreamEvent,
        at: UnixMs,
    },
    /// A provider/host lifecycle observation shared only for presentation.
    /// It neither changes native context nor claims ownership of Claude
    /// history.
    ExecObserved {
        id: rho_core::ExecId,
        milestone: rho_core::ExecMilestone,
        at: UnixMs,
    },
    /// Claude owns its conversation; this records only Rho's permission to
    /// execute a cell, committed before the notebook can perform side effects.
    ClaudeExecAdmitted {
        call: rho_core::ExecCall,
        at: UnixMs,
    },
    /// Canonical native conversation records; legacy block rows are read-only.
    Native(NativeEvent),
    ClaudeOutput {
        batch: ClaudeOutputBatch,
    },
    ClaudeOutputHandedOff {
        id: uuid::Uuid,
        at: UnixMs,
    },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Input {
    UserMessage {
        sender: MessageSender,
        content: Vec<ContentPart>,
    },
    ExecOutput(ExecOutput),
    /// Read compatibility only; new notebook output never uses this shape.
    Historical(ContextBlock),
    DeveloperMessage {
        text: String,
    },
    CompactionTrigger,
    ContextRotation {
        retain_from: u64,
    },
    /// A crash-recovered admitted prefix, not a new model response.
    RecoveredResponse {
        items: Vec<InferenceResponseItem>,
        provider_response_id: Option<ProviderResponseId>,
    },
}

impl Input {
    /// Materialize the provider's replay vocabulary at the adapter boundary.
    pub fn context(&self) -> ContextBlock {
        match self {
            Self::UserMessage { sender, content } => ContextBlock::UserMessage {
                sender: sender.clone(),
                content: content.clone(),
            },
            Self::ExecOutput(output) => rho_inference::exec::output(output),
            Self::Historical(block) => block.clone(),
            Self::DeveloperMessage { text } => {
                ContextBlock::DeveloperMessage { text: text.clone() }
            }
            Self::CompactionTrigger => ContextBlock::CompactionTrigger,
            Self::ContextRotation { retain_from } => ContextBlock::ContextRotation {
                retain_from: *retain_from,
            },
            Self::RecoveredResponse {
                items,
                provider_response_id,
            } => ContextBlock::InferenceResponse {
                items: items.clone(),
                provider_response_id: provider_response_id.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum NativeEvent {
    RequestStarted {
        input: Vec<Input>,
        context: Option<ContextChange>,
        wake: Option<WakeFacts>,
        at: UnixMs,
    },
    ResponseFinished {
        items: Vec<InferenceResponseItem>,
        provider_response_id: Option<ProviderResponseId>,
        context_used: Option<u64>,
        usage: Option<crate::db::AgentUsageBucket>,
        at: UnixMs,
    },
    RequestFailed {
        partial: PendingInferenceResponse,
        error: String,
        retrying: bool,
        at: UnixMs,
    },
    PythonStream {
        event: PythonStreamEvent,
        at: UnixMs,
    },
}

impl Event<'static> {
    pub(super) fn current(self, native: bool) -> AgentEvent<'static> {
        use crate::native::NativeEvent as Current;
        let event = match self {
            Self::Sent { blocks, at, wake } => Current::RequestStarted {
                input: blocks.into_owned(),
                context: None,
                wake,
                at,
            },
            Self::ContextSent {
                blocks,
                change,
                at,
                wake,
            } => Current::RequestStarted {
                input: blocks.into_owned(),
                context: Some(change),
                wake,
                at,
            },
            Self::Replied {
                blocks,
                context_used,
                usage,
                at,
            } => Current::ResponseFinished {
                output: blocks.into_owned(),
                context_used,
                usage,
                at,
            },
            Self::Failed {
                partial,
                error,
                retrying,
                at,
            } if native => Current::RequestFailed {
                partial,
                error: error.into_owned(),
                retrying,
                at,
            },
            Self::PythonStream { event, at } => Current::PythonStream { event, at },
            Self::Native(event) => match event {
                NativeEvent::RequestStarted {
                    input,
                    context,
                    wake,
                    at,
                } => Current::RequestStarted {
                    input: input.iter().map(Input::context).collect(),
                    context,
                    wake,
                    at,
                },
                NativeEvent::ResponseFinished {
                    items,
                    provider_response_id,
                    context_used,
                    usage,
                    at,
                } => Current::ResponseFinished {
                    output: vec![ContextBlock::InferenceResponse {
                        items,
                        provider_response_id,
                    }],
                    context_used,
                    usage,
                    at,
                },
                NativeEvent::RequestFailed {
                    partial,
                    error,
                    retrying,
                    at,
                } => Current::RequestFailed {
                    partial,
                    error,
                    retrying,
                    at,
                },
                NativeEvent::PythonStream { event, at } => Current::PythonStream { event, at },
            },
            Self::QueueCleared => return AgentEvent::Cleared { at: UnixMs(0) },
            Self::Presented { title, at, .. } => {
                return match title {
                    PresentationField::Set(title) => AgentEvent::Titled {
                        title: Some(title),
                        at,
                    },
                    PresentationField::Clear => AgentEvent::Titled { title: None, at },
                    PresentationField::Unchanged => AgentEvent::TitleAttempted { at },
                };
            }
            // Unchanged observation/configuration shapes. Tagged provider data
            // is decoded/re-encoded by its owning codec, not flattened to text.
            other => {
                let bytes = senax_encoder::encode(&other).expect("encode migration row");
                return senax_encoder::decode(&mut bytes.as_ref())
                    .expect("decode unchanged migration row");
            }
        };
        AgentEvent::Native(event)
    }
}

#[derive(Debug)]
struct LogName;
impl RecordedTypeName for LogName {
    const NAME: &'static str = "rho-db::Sen<rho_agent::AgentEvent<'_>>";
}
const OLD_LOG: TableDefinition<(AgentId, u64), SenAs<Event<'static>, LogName>> =
    TableDefinition::new("agent_log");

pub(super) fn migrate(write: &mut WriteTxn) {
    use std::ops::Bound::{Excluded, Unbounded};
    let mut after = None;
    let mut native = true;
    // Bounded batches, in RAW key order, including branches hidden by rewind.
    // Every row keeps its key and every context block keeps its grouping.
    loop {
        let rows = {
            let log = write.open_table(OLD_LOG);
            log.range((after.map_or(Unbounded, Excluded), Unbounded))
                .take(128)
                .map(|(key, value)| (key.value(), value.value().into_owned()))
                .collect::<Vec<_>>()
        };
        if rows.is_empty() {
            break;
        }
        let mut log = write.open_table(super::AGENT_LOG);
        for (key, event) in rows {
            if let Event::Created { runtime, .. } = &event {
                native = matches!(runtime, crate::db::AgentRuntime::Rho { .. });
            }
            let current = event.current(native);
            log.insert(&key, SenValue::borrowed(&current));
            after = Some(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_inference::OpenAiResponsesProviderData as Data;

    use super::*;
    use crate::db::AgentReadTxnExt as _;

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    struct FutureData {
        payload: Vec<u8>,
    }
    impl senax_encoder::TaggedSenax for FutureData {
        const TAG: &'static str = "migration-test.future-provider";
    }

    // Exact former payload shape, deliberately not registered as a provider.
    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    enum SignatureAttachment {
        Standalone,
        NextPart,
    }
    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    struct RetiredThinking {
        signature: String,
        attachment: SignatureAttachment,
    }
    impl senax_encoder::TaggedSenax for RetiredThinking {
        const TAG: &'static str = "google.antigravity.thought-signature";
    }

    fn response(id: &str, items: Vec<InferenceResponseItem>) -> ContextBlock {
        ContextBlock::InferenceResponse {
            items,
            provider_response_id: Some(id.try_into().unwrap()),
        }
    }

    fn old_call(id: &str) -> InferenceResponseItem {
        InferenceResponseItem::ToolCall {
            provider_specific: Box::new(Data::FunctionCall {
                item_id: "fc-old".try_into().unwrap(),
            }),
            id: id.try_into().unwrap(),
            name: "old_shell".try_into().unwrap(),
            tool_type: rho_core::ToolType::Function,
            arguments: "{\"command\":\"pwd\"}".into(),
        }
    }

    #[tokio::test]
    async fn rewrites_raw_rows_without_losing_response_boundaries_or_provider_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.redb");
        let reasoning = vec![
            InferenceResponseItem::EncryptedReasoning {
                provider_specific: Box::new(Data::EncryptedReasoning {
                    item_id: "rs-a".try_into().unwrap(),
                    encrypted_content: "encrypted-thinking".into(),
                }),
                summary: vec!["summary one".into(), "summary two".into()],
            },
            InferenceResponseItem::RawReasoning {
                provider_specific: Box::new(Data::Message {
                    item_id: "raw-a".try_into().unwrap(),
                }),
                content: "raw thinking, preserved durably".into(),
                summary: vec!["raw summary".into()],
            },
            InferenceResponseItem::Unknown {
                provider_specific: Box::new(RetiredThinking {
                    signature: "opaque retired signature".into(),
                    attachment: SignatureAttachment::NextPart,
                }),
            },
            InferenceResponseItem::Unknown {
                provider_specific: Box::new(FutureData {
                    payload: vec![0, 1, 255],
                }),
            },
        ];
        let output = vec![
            response("resp-a", reasoning),
            ContextBlock::DeveloperMessage {
                text: "between two responses".into(),
            },
            response("resp-b", vec![old_call("call-a"), old_call("call-b")]),
        ];
        let body = rho_core::ToolOutput {
            output: Arc::new("bounded result".into()),
            full_output: Some(Arc::new("full result".into())),
            status: rho_core::ToolOutputStatus::Error,
            images: Arc::new(vec![rho_core::ImageContent {
                media_type: "image/png".into(),
                data: vec![1, 2, 3],
                detail: rho_core::ImageDetail::Original,
            }]),
        };
        let input = vec![
            ContextBlock::ToolResults {
                results: ["call-a", "call-b"]
                    .into_iter()
                    .map(|id| ToolResult {
                        call_id: id.try_into().unwrap(),
                        tool_type: rho_core::ToolType::Function,
                        body: body.clone(),
                        started_at: UnixMs(1),
                        finished_at: UnixMs(2),
                        metadata: None,
                    })
                    .collect(),
            },
            ContextBlock::ContextRotation { retain_from: 2 },
        ];
        let report = ExecOutput::Report {
            id: "call-b".try_into().unwrap(),
            body,
            at: UnixMs(7),
        };
        let expected_output = senax_encoder::encode(&output).unwrap();
        let expected_input = senax_encoder::encode(&input).unwrap();
        let (agent, journal) = {
            let db = rho_db::RhoDb::open(&path);
            let mut write = db.write().await;
            write.init_agent_tables();
            let agent = super::super::tests::create(&mut write, None, None);
            let legacy = vec![
                Event::Sent {
                    blocks: Cow::Owned(vec![ContextBlock::UserMessage {
                        sender: MessageSender::User,
                        content: vec![ContentPart::Text {
                            text: "task".into(),
                        }],
                    }]),
                    at: UnixMs(1),
                    wake: None,
                },
                Event::Replied {
                    blocks: Cow::Owned(output),
                    context_used: Some(17),
                    usage: None,
                    at: UnixMs(2),
                },
                Event::ContextSent {
                    blocks: Cow::Owned(input),
                    change: ContextChange::Marked { retain_from: 2 },
                    at: UnixMs(3),
                    wake: None,
                },
                Event::PythonStream {
                    event: PythonStreamEvent::Opened {
                        item: old_call("hidden"),
                    },
                    at: UnixMs(4),
                },
                Event::PythonStream {
                    event: PythonStreamEvent::Admitted {
                        call_id: "hidden".try_into().unwrap(),
                        source: "side_effect()".into(),
                    },
                    at: UnixMs(5),
                },
                Event::Rewound {
                    to: crate::db::AgentEventPos::new(4),
                    at: UnixMs(6),
                },
                Event::Native(NativeEvent::RequestStarted {
                    input: vec![Input::ExecOutput(report.clone())],
                    context: None,
                    wake: None,
                    at: UnixMs(7),
                }),
                Event::Native(NativeEvent::ResponseFinished {
                    items: vec![InferenceResponseItem::Compaction {
                        provider_specific: Box::new(Data::Compaction {
                            item_id: "compact-a".try_into().unwrap(),
                            encrypted_content: "compaction".into(),
                        }),
                    }],
                    provider_response_id: Some("resp-c".try_into().unwrap()),
                    context_used: None,
                    usage: None,
                    at: UnixMs(8),
                }),
            ];
            for (index, event) in legacy.iter().enumerate() {
                let pos = index as u64 + 1;
                write
                    .open_table(OLD_LOG)
                    .insert(&(agent, pos), SenValue::borrowed(event));
                write
                    .open_table(super::super::JOURNAL)
                    .insert(&(pos + 1), &(agent, pos));
            }
            write
                .open_table(super::super::FORMAT)
                .insert(&(), &"b4e2c7a1".to_owned());
            write.commit();
            let journal = db.read().journal_head();
            let mut write = db.write().await;
            write.init_agent_tables();
            write.commit();
            let read = db.read();
            assert_eq!(read.journal_head(), journal);
            assert_eq!(
                read.open_table(super::super::FORMAT)
                    .get(&())
                    .unwrap()
                    .value(),
                "d8f63a20"
            );
            let AgentEvent::Native(crate::native::NativeEvent::ResponseFinished { output, .. }) =
                read.agent_event(agent, crate::db::AgentEventPos::new(2))
                    .unwrap()
            else {
                panic!("response")
            };
            assert_eq!(senax_encoder::encode(&output).unwrap(), expected_output);
            let AgentEvent::Native(crate::native::NativeEvent::RequestStarted { input, .. }) = read
                .agent_event(agent, crate::db::AgentEventPos::new(3))
                .unwrap()
            else {
                panic!("request")
            };
            assert_eq!(senax_encoder::encode(&input).unwrap(), expected_input);
            assert!(read.agent_exec_was_admitted(agent, &"hidden".try_into().unwrap()));
            let (_, visible) = read.agent_event_records(agent);
            assert!(!visible.iter().any(|(pos, _)| matches!(pos.pos, 4 | 5)));
            let AgentEvent::Native(crate::native::NativeEvent::RequestStarted { input, .. }) = read
                .agent_event(agent, crate::db::AgentEventPos::new(7))
                .unwrap()
            else {
                panic!("report")
            };
            assert_eq!(input, vec![rho_inference::exec::output(&report)]);
            let ContextBlock::ToolUpdate(report) = &input[0] else {
                panic!("report")
            };
            assert_eq!(report.status, Some(rho_core::ToolOutputStatus::Error));
            let replayed = crate::agent::replay::replay(read.agent_events(agent).1);
            assert_eq!(rho_core::context_window_start(&replayed.history), 2);
            assert_eq!(replayed.history.len(), 8, "logical block grouping changed");
            (agent, journal)
        };
        let db = rho_db::RhoDb::open(&path);
        let mut write = db.write().await;
        write.init_agent_tables(); // Already current: no legacy reader needed.
        write.commit();
        assert_eq!(db.read().journal_head(), journal);
        assert_eq!(db.read().get_agent(agent).next.pos, 9);
        assert!(
            db.read()
                .agent_exec_was_admitted(agent, &"hidden".try_into().unwrap())
        );
    }

    #[tokio::test]
    async fn malformed_later_batch_rolls_back_earlier_rewrites_and_format() {
        // An actual failed process also verifies redb recovery. The workspace's
        // Cranelift build does not reliably catch nested Rust unwinds.
        if let Some(path) = std::env::var_os("RHO_MIGRATION_ROLLBACK_TEST") {
            let db = rho_db::RhoDb::open(path);
            let mut write = db.write().await;
            write.init_agent_tables();
            write.commit();
            return;
        }
        #[derive(Debug, Encode, Decode)]
        enum Unrecognized {
            FutureEvent,
        }
        const BAD_LOG: TableDefinition<(AgentId, u64), SenAs<Unrecognized, LogName>> =
            TableDefinition::new("agent_log");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.redb");
        let db = rho_db::RhoDb::open(&path);
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent = super::super::tests::create(&mut write, None, None);
        for pos in 1..=129 {
            write.open_table(OLD_LOG).insert(
                &(agent, pos),
                SenValue::borrowed(&Event::Sent {
                    blocks: Cow::Owned(Vec::new()),
                    at: UnixMs(pos),
                    wake: None,
                }),
            );
        }
        write.open_table(BAD_LOG).insert(
            &(agent, 130),
            SenValue::borrowed(&Unrecognized::FutureEvent),
        );
        write
            .open_table(super::super::FORMAT)
            .insert(&(), &"b4e2c7a1".to_owned());
        write.commit();
        drop(db);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "db::migration::tests::malformed_later_batch_rolls_back_earlier_rewrites_and_format", "--nocapture"])
            .env("RHO_MIGRATION_ROLLBACK_TEST", &path)
            .output().unwrap();
        assert!(
            !child.status.success(),
            "corrupt migration unexpectedly committed"
        );
        assert!(String::from_utf8_lossy(&child.stderr).contains("UnknownVariantId"));
        let db = rho_db::RhoDb::open(&path);
        let read = db.read();
        assert_eq!(
            read.open_table(super::super::FORMAT)
                .get(&())
                .unwrap()
                .value(),
            "b4e2c7a1"
        );
        assert!(matches!(
            read.open_table(OLD_LOG)
                .get(&(agent, 1))
                .unwrap()
                .value()
                .into_owned(),
            Event::Sent { .. }
        ));
    }
    #[test]
    fn retired_provider_configuration_rows_migrate_to_the_default_engineer() {
        #[derive(Encode)]
        enum Intelligence {
            Gemini,
        }
        #[derive(Encode)]
        enum Workflow {
            PrFriendly,
        }
        #[derive(Encode)]
        enum Role {
            Engineer {
                intelligence: Intelligence,
            },
            WorkflowEngineer {
                intelligence: Intelligence,
                workflow: Workflow,
            },
        }
        #[derive(Encode)]
        enum Binding {
            AntigravityFlashLow(rho_inference::config::InferenceProfile),
        }
        #[derive(Encode)]
        enum Row {
            Created {
                role: Role,
                binding: Binding,
                runtime: AgentRuntime,
                place: Place,
                spawned_by: AgentSpawnedBy,
                spawn_name: Option<String>,
                created_at: UnixMs,
            },
            RoleChanged {
                role: Role,
                binding: Option<Binding>,
                at: UnixMs,
            },
        }
        let binding = || {
            Binding::AntigravityFlashLow(rho_inference::config::InferenceProfile {
                effort: rho_inference::config::ReasoningEffort::Xhigh,
                fast_mode: true,
            })
        };
        let mut rows = vec![Row::Created {
            role: Role::Engineer {
                intelligence: Intelligence::Gemini,
            },
            binding: binding(),
            runtime: super::super::tests::test_agent_runtime(),
            place: super::super::tests::test_workspace(),
            spawned_by: AgentSpawnedBy::Direct,
            spawn_name: Some("keep-this-name".into()),
            created_at: UnixMs(1),
        }];
        for bound in [None, Some(binding())] {
            rows.push(Row::RoleChanged {
                role: Role::WorkflowEngineer {
                    intelligence: Intelligence::Gemini,
                    workflow: Workflow::PrFriendly,
                },
                binding: bound,
                at: UnixMs(2),
            });
        }
        use crate::db::AgentRoleSessionProfile as _;
        let expected = AgentRole::default().session_profile().unwrap();
        for (index, row) in rows.iter().enumerate() {
            let mut bytes = senax_encoder::encode(row).unwrap();
            let old: Event<'static> = senax_encoder::decode(&mut bytes).unwrap();
            let current = old.current(true);
            match &current {
                AgentEvent::Created {
                    role,
                    binding,
                    spawn_name,
                    ..
                } => {
                    assert_eq!(*role, AgentRole::default());
                    assert_eq!(*binding, expected);
                    assert_eq!(spawn_name.as_deref(), Some("keep-this-name"));
                }
                AgentEvent::RoleChanged { role, binding, .. } => {
                    assert_eq!(*role, AgentRole::default());
                    assert_eq!(*binding, (index == 2).then_some(expected));
                }
                _ => panic!("configuration changed event kind"),
            }
            let mut encoded = senax_encoder::encode(&current).unwrap();
            assert_eq!(
                senax_encoder::decode::<AgentEvent<'static>>(&mut encoded).unwrap(),
                current
            );
        }
    }
}
