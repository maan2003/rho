//! Streaming admission and settlement are in-memory execution state. Only
//! complete conversation boundaries are persisted. Provider EOF and transport
//! loss are deliberately different operations.
#[cfg(test)]
use rho_core::StreamingContextItem;
use rho_inference::exec::set_source;

use super::*;

pub(super) struct Stream {
    pub exec: Arc<rho_agent_tools::PythonExec>,
    item: InferenceResponseItem,
    index: usize,
    source: String,
    pub canonical: bool,
    pub interrupted: bool,
}

pub(super) fn progress_note(
    id: &ToolCallId,
    source: &str,
    completed: usize,
    admitted: usize,
    pending_status: &str,
) -> String {
    let id = id.as_str();
    format!(
        "Streaming Python call {id}: successfully evaluated UTF-8 source bytes 0..{completed}. \
         Bytes {completed}..{admitted}: {pending_status}. Do not replay admitted statements. \
         Source bytes after {admitted} were not admitted. Evaluation is not command completion: \
         existing command handles and their fresh output remain authoritative. Continue with new code.\n\
         Successfully evaluated source:\n```python\n{}\n```\n\
         Admitted but not confirmed successful:\n```python\n{}\n```",
        &source[..completed],
        &source[completed..admitted],
    )
}

impl Agent {
    pub(super) async fn update_stream(&mut self, now: UnixMs) -> anyhow::Result<()> {
        let Phase::Requesting(in_flight) = &self.phase else {
            return Ok(());
        };
        let Some((index, item, incoming)) =
            rho_inference::exec::stream(&in_flight.pending).map_err(anyhow::Error::msg)?
        else {
            return if in_flight.stream.is_some() {
                Err(anyhow::anyhow!(
                    "Provider removed an active streaming exec call"
                ))
            } else {
                Ok(())
            };
        };
        if incoming.source.len() > 1024 * 1024 {
            return Err(anyhow::anyhow!("Streaming Python source exceeds 1 MiB"));
        }
        if let Some(id) = &in_flight.stream {
            if id != &incoming.id {
                return Err(anyhow::anyhow!(
                    "Provider changed the streaming exec identity"
                ));
            }
            let stream = self.streams.get_mut(id).unwrap();
            let mut identity = item.clone();
            set_source(&mut identity, String::new());
            if index != stream.index
                || identity != stream.item
                || !incoming.source.starts_with(&stream.source)
            {
                return Err(anyhow::anyhow!(
                    "Provider changed previously received Python source or call metadata"
                ));
            }
            let fragment = incoming.source[stream.source.len()..].to_owned();
            stream.source = incoming.source;
            self.execs.get_mut(id).unwrap().call.source = stream.source.clone();
            if !fragment.is_empty() {
                stream
                    .exec
                    .feed(fragment, false)
                    .map_err(anyhow::Error::msg)?;
            }
            return Ok(());
        }
        if !self.admitted.insert(incoming.id.clone()) {
            return Err(anyhow::anyhow!(
                "Provider reused an earlier tool call identity"
            ));
        }
        let tool = &self.surface.get_if_ready().unwrap().notebook;
        let session = tool.start_stream(incoming.id.clone(), SourceWaker::new(self.wake.clone()));
        let exec = session.execution();
        let mut identity = item;
        set_source(&mut identity, String::new());
        self.persist(AgentEvent::ExecObserved {
            id: incoming.id.clone(),
            milestone: rho_core::ExecMilestone::FirstBlock,
            at: now,
        })
        .await?;
        self.execs.insert(
            incoming.id.clone(),
            RunningExec {
                call: rho_core::ExecCall {
                    id: incoming.id.clone(),
                    source: incoming.source.clone(),
                },
                first_block_at: now,
                session,
                answer: ReplyState::Owed,
            },
        );
        self.latest_python_exec = Some((incoming.id.clone(), exec.clone()));
        self.streams.insert(
            incoming.id.clone(),
            Stream {
                exec: exec.clone(),
                item: identity,
                index,
                source: incoming.source.clone(),
                canonical: false,
                interrupted: false,
            },
        );
        let Phase::Requesting(in_flight) = &mut self.phase else {
            unreachable!()
        };
        in_flight.stream = Some(incoming.id);
        exec.feed(incoming.source, false)
            .map_err(anyhow::Error::msg)
    }

    pub(super) fn advance_streams(&mut self, admit: bool) {
        if !admit {
            return;
        }
        for stream in self.streams.values() {
            if let Err(error) = stream.exec.admit_stream_unit() {
                self.recovery_notes.push(error);
            }
        }
    }

    pub(super) fn finish_stream(&mut self, items: &[InferenceResponseItem]) -> Result<(), String> {
        let Phase::Requesting(in_flight) = &self.phase else {
            return Ok(());
        };
        let Some(id) = &in_flight.stream else {
            return Ok(());
        };
        let stream = self.streams.get(id).unwrap();
        let calls = items
            .iter()
            .filter(|item| matches!(item, InferenceResponseItem::ToolCall { .. }))
            .collect::<Vec<_>>();
        if calls.len() != 1 {
            return Err(
                "Provider completed a different set of calls after streaming execution".into(),
            );
        }
        let mut expected = stream.item.clone();
        set_source(&mut expected, stream.source.clone());
        if calls[0] != &expected {
            return Err("Provider final exec differs from its streamed source".into());
        }
        // Only validated successful response completion closes the last unit.
        stream.exec.feed(String::new(), true)?;
        Ok(())
    }

    pub(super) async fn abandon_stream(&mut self, now: UnixMs) -> anyhow::Result<bool> {
        let Phase::Requesting(in_flight) = &mut self.phase else {
            return Ok(false);
        };
        let Some(id) = in_flight.stream.take() else {
            return Ok(false);
        };
        let stream = self.streams.get_mut(&id).unwrap();
        stream.interrupted = true;
        stream.exec.stop_stream();
        let progress = stream.exec.stream_progress();
        if progress.admitted == 0 {
            self.streams.remove(&id);
            self.execs.remove(&id);
            self.latest_python_exec = None;
            return Ok(false);
        }
        let mut item = stream.item.clone();
        set_source(&mut item, stream.source[..progress.admitted].to_owned());
        stream.canonical = true;
        self.context.replied(std::slice::from_ref(&item));
        // Give the admitted, syntactically complete prefix its one place in
        // history before the normal boundary drains its result.
        self.persist(AgentEvent::Native(NativeEvent::ResponseFinished {
            output: vec![ContextBlock::InferenceResponse {
                items: vec![item],
                provider_response_id: None,
            }],
            context_used: self.context_used,
            usage: None,
            at: now,
        }))
        .await?;
        Ok(true)
    }

    /// Retire live stream state after the boundary has accepted its output.
    pub(super) fn acknowledge_streams(
        &mut self,
        delivered: &std::collections::BTreeSet<ToolCallId>,
    ) {
        let ids = self
            .streams
            .iter()
            .filter(|(id, stream)| {
                delivered.contains(*id)
                    && stream.exec.stream_progress().returned
                    && stream.canonical
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.streams.remove(&id);
        }
    }

    pub(super) fn collect_stream_notes(
        &mut self,
        delivered: Option<&std::collections::BTreeSet<ToolCallId>>,
    ) {
        for (id, stream) in &mut self.streams {
            if delivered.is_some_and(|ids| !ids.contains(id)) {
                continue;
            }
            let progress = stream.exec.take_stream_report();
            if !stream.interrupted
                && (progress.recovery
                    || (progress.returned && progress.completed != stream.source.len()))
            {
                self.recovery_notes.push(progress_note(
                    id,
                    &stream.source,
                    progress.completed,
                    progress.admitted,
                    if progress.completed == progress.admitted {
                        "no outstanding admitted statement"
                    } else if progress.settled < progress.admitted && !progress.returned {
                        "admitted and still running or waiting to run"
                    } else {
                        "did not complete successfully; any side effects remain"
                    },
                ));
            }
        }
    }
}

#[cfg(test)]
pub(in crate::agent) mod tests {
    use rho_core::{AppendString, ContextItemEvent, ProviderResponseItemId, ToolType};
    use rho_inference::OpenAiResponsesProviderData;
    use rho_tool_shell::ShellTools;

    use super::*;

    pub(in crate::agent) struct TestAgent {
        pub agent: Agent,
        pub db: RhoDb,
        pub agent_id: AgentId,
    }
    impl std::ops::Deref for TestAgent {
        type Target = Agent;
        fn deref(&self) -> &Agent {
            &self.agent
        }
    }
    impl std::ops::DerefMut for TestAgent {
        fn deref_mut(&mut self) -> &mut Agent {
            &mut self.agent
        }
    }

    pub(in crate::agent) async fn agent(directory: &std::path::Path) -> TestAgent {
        let db = RhoDb::open(directory.join("agent.redb"));
        let inference = Inference::new_with_config(
            db.clone(),
            rho_inference::InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        )
        .await
        .unwrap();
        let worksets = rho_fs_view::Worksets::open(
            directory.join("state"),
            Default::default(),
            Default::default(),
            rho_fs_view::StoreService::None,
        )
        .await
        .unwrap();
        let view = worksets
            .adopt(directory)
            .unwrap()
            .enter(
                rho_fs_view::Mode::View {
                    home_skeleton: None,
                },
                camino::Utf8Path::new(rho_fs_view::MOUNT_ROOT),
            )
            .unwrap();
        let role = AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
        };
        let binding = role.session_profile().unwrap();
        let profile = binding.deep_config().unwrap();
        let model = binding.deep_model().unwrap();
        let key = PromptCacheKey::generate();
        let mut write = db.write().await;
        write.init_agent_tables();
        let id = write.alloc_agent_id();
        write.create_agent(
            UnixMillis::now(),
            id,
            None,
            crate::StartPlace::new(Arc::clone(&view), None).place,
            role,
            binding,
            AgentRuntime::Rho {
                prompt_cache_key: key,
            },
            None,
        );
        write.commit();
        let head = db.read().get_agent(id);
        let notebook = Arc::new(
            rho_agent_tools::PythonNotebook::new(
                ShellTools::in_directory(
                    Duration::from_secs(5),
                    directory.to_str().unwrap().into(),
                    Default::default(),
                ),
                Vec::new(),
            )
            .unwrap(),
        );
        let surface = Surface {
            prompt: PromptInputs {
                view,
                host: None,
                host_specs: Vec::new(),
                notes: Some(Lazy::ready(directory.join("notes"))),
            },
            notebook,
        };
        let (_control, control_rx) = mpsc::unbounded_channel();
        let host =
            crate::worker::local_services(db.clone(), inference.clone(), id, Default::default());
        let status = Arc::new(RwLock::new(AgentStatus {
            kind: AgentStateKind::Idle,
            queued: 0,
        }));
        host.observe(&status);
        TestAgent {
            db,
            agent_id: id,
            agent: Agent {
                writer: super::super::persistence::Writer::new(host.clone()),
                pending_events: Vec::new(),
                admitted: Default::default(),
                provider_history: None,
                name_updates: host.names(),
                host,
                surface: Arc::new(Lazy::ready(surface)),
                model,
                context: Default::default(),
                session: inference.deep_session(profile, model, key),
                phase: Phase::Requesting(InFlight::default()),
                user: Vec::new(),
                mail: Vec::new(),
                execs: BTreeMap::new(),
                observations: Observations::default(),
                streams: BTreeMap::new(),
                recovery_notes: Vec::new(),
                context_used: None,
                turn: None,
                latest_python_exec: None,
                total_usage: Default::default(),
                working: false,
                wake: Arc::new(Notify::new()),
                status,
                head: Arc::new(RwLock::new(head)),
                control_rx,
            },
        }
    }

    fn update(id: &str, source: &str) -> Event {
        let mut arguments = AppendString::new();
        arguments.push_str(source);
        Event::Inference(InferenceEvent::ContextItem {
            index: 0,
            event: ContextItemEvent::Update(StreamingContextItem::ToolCall {
                provider_specific: Box::new(OpenAiResponsesProviderData::CustomToolCall {
                    item_id: ProviderResponseItemId::try_from(format!("item-{id}")).unwrap(),
                }),
                id: ToolCallId::try_from(id).unwrap(),
                name: ToolName::try_from("exec").unwrap(),
                tool_type: ToolType::Custom,
                arguments: arguments.snapshot(),
            }),
        })
    }

    async fn until(agent: &mut Agent, predicate: impl Fn(&Agent) -> bool) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                agent.advance_streams(true);
                if predicate(agent) {
                    return;
                }
                agent.wake.notified().await;
            }
        })
        .await
        .expect("stream did not settle");
    }

    #[tokio::test]
    async fn provider_projection_tracks_the_queued_tail_and_converges_after_flush() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        assert!(agent.provider_input().await.unwrap().is_empty());
        for index in 0..3 {
            agent
                .persist(AgentEvent::Native(
                    crate::native::NativeEvent::RequestStarted {
                        input: vec![rho_core::ContextBlock::DeveloperMessage {
                            text: format!("round {index}"),
                        }],
                        context: None,
                        wake: None,
                        at: UnixMs::now(),
                    },
                ))
                .await
                .unwrap();
            agent.flush_events().await.unwrap();
            let (_, events) = agent.db.read().agent_events(agent.agent_id);
            assert_eq!(
                agent.provider_input().await.unwrap(),
                replay::replay(events).history
            );
        }
    }

    #[tokio::test]
    async fn blocked_database_does_not_block_live_history_and_usage_commits_with_response() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        assert!(agent.provider_input().await.unwrap().is_empty());
        let db = agent.db.clone();
        let blocked = db.write().await;
        let before = db.read().agent_events(agent.agent_id).1.len();
        tokio::time::timeout(Duration::from_secs(1), async {
            agent
                .persist(AgentEvent::Native(
                    crate::native::NativeEvent::RequestStarted {
                        input: vec![rho_core::ContextBlock::DeveloperMessage {
                            text: "queued".into(),
                        }],
                        context: None,
                        wake: None,
                        at: UnixMs(300_000),
                    },
                ))
                .await
                .unwrap();
            agent
                .persist(AgentEvent::Native(
                    crate::native::NativeEvent::ResponseFinished {
                        output: vec![rho_core::ContextBlock::InferenceResponse {
                            items: Vec::new(),
                            provider_response_id: None,
                        }],
                        context_used: None,
                        usage: Some(crate::db::AgentUsageBucket {
                            requests: 1,
                            output_tokens: 7,
                            ..Default::default()
                        }),
                        at: UnixMs(300_001),
                    },
                ))
                .await
                .unwrap();
            assert_eq!(agent.provider_input().await.unwrap().len(), 2);
        })
        .await
        .expect("submissions must not wait for the database lock");
        assert_eq!(db.read().agent_events(agent.agent_id).1.len(), before);
        assert_eq!(db.read().agent_usage_total(agent.agent_id).requests, 0);
        drop(blocked);
        agent.flush_events().await.unwrap();
        let read = db.read();
        let events = read.agent_events(agent.agent_id).1;
        assert_eq!(events.len(), before + 2);
        assert_eq!(read.agent_usage_total(agent.agent_id).requests, 1);
        assert_eq!(read.agent_usage_total(agent.agent_id).output_tokens, 7);
        drop(read);
        assert_eq!(
            agent.provider_input().await.unwrap(),
            replay::replay(events).history
        );
        agent.flush_events().await.unwrap();
        assert_eq!(db.read().agent_usage_total(agent.agent_id).requests, 1);
    }

    #[tokio::test]
    async fn replication_disconnect_fails_barriers_and_stops_the_loop() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        let (client, server) = crate::worker::testing::pair();
        let host = client.host();
        agent.writer = super::super::persistence::Writer::new(host.clone());
        drop(server);
        host.closed().await;
        // Submission may succeed before the writer observes the failure.
        let _ = agent
            .persist(AgentEvent::Native(
                crate::native::NativeEvent::RequestStarted {
                    input: Vec::new(),
                    context: None,
                    wake: None,
                    at: UnixMs::now(),
                },
            ))
            .await;
        assert!(
            agent
                .flush_events()
                .await
                .unwrap_err()
                .is::<crate::worker::StoreError>()
        );
        let error = agent.run().await.unwrap_err();
        assert!(error.is::<crate::worker::StoreError>());
    }

    #[tokio::test]
    async fn units_execute_without_database_access_and_restart_drops_unsaved_source() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        let source = "Path('marker').write_text('first')\nPath('marker').write_text('second')\n";
        agent.handle(update("one", source)).await.unwrap();
        let db = agent.db.clone();
        let (_, before) = db.read().agent_events(agent.agent_id);
        // The first-block observation is coarse metadata; unit admission and
        // settlement must never need the writer, including the very first unit.
        let writer = db.write().await;
        until(&mut agent, |agent| {
            agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .completed
                == source.len()
        })
        .await;
        drop(writer);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("marker")).unwrap(),
            "second"
        );
        let (_, events) = db.read().agent_events(agent.agent_id);
        assert_eq!(
            events, before,
            "executing units must not journal interpreter progress"
        );
        drop(agent);
        let recovered = replay::replay(events);
        assert!(
            recovered.history.is_empty(),
            "no response boundary was committed"
        );
        assert!(
            recovered.owed.is_empty(),
            "do not invent or replay an unsaved call"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("marker")).unwrap(),
            "second"
        );
    }

    #[tokio::test]
    async fn commands_start_before_response_end_and_final_response_does_not_replay() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent
            .handle(update(
                "one",
                "job = command(\"printf x >> marker; printf fresh-output\")\n",
            ))
            .await
            .unwrap();
        until(&mut agent, |agent| {
            agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .completed
                > 0
        })
        .await;
        let cell = agent.latest_python_exec.as_ref().unwrap().1.sequence();
        // The response is still in flight, but the command is already running.
        assert!(matches!(agent.phase, Phase::Requesting(_)));
        until(&mut agent, |agent| {
            agent.execs.values().next().unwrap().session.sources().iter().any(|(_, facts)|
            matches!(facts, rho_agent_tools::SourceFacts::Job(facts) if facts.finished.is_some())
        )
        })
        .await;
        assert_eq!(
            std::fs::read_to_string(directory.path().join("marker")).unwrap(),
            "x"
        );
        agent
            .handle(Event::Inference(InferenceEvent::ContextItem {
                index: 0,
                event: ContextItemEvent::Finish,
            }))
            .await
            .unwrap();
        agent
            .handle(Event::Inference(InferenceEvent::Finished {
                usage: None,
                provider_response_id: None,
            }))
            .await
            .unwrap();
        until(&mut agent, |agent| {
            agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .returned
        })
        .await;
        assert_eq!(
            agent.latest_python_exec.as_ref().unwrap().1.sequence(),
            cell
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("marker")).unwrap(),
            "x"
        );
        agent.flush_events().await.unwrap();
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let recovered = replay::replay(events);
        assert_eq!(recovered.owed.len(), 1);
        assert!(recovered.recovery_notes.is_empty());
        agent.start_request(UnixMs::now(), None).await.unwrap();
        agent.session.abort();
        assert!(agent.provider_input().await.unwrap().iter().any(|block| matches!(&**block,
            ContextBlock::ToolResults { results } if results.iter().any(|result| result.body.output.contains("fresh-output")))));
        agent.flush_events().await.unwrap();
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        assert!(replay::replay(events).owed.is_empty());
    }

    #[tokio::test]
    async fn failure_discards_suffix_and_retry_boundary_drains_fresh_command_output() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        let prefix = "job = command(\"printf fresh-output; printf x >> marker\")\n";
        agent
            .handle(update(
                "one",
                &format!("{prefix}if True:\n    command('touch wrong')\n"),
            ))
            .await
            .unwrap();
        until(&mut agent, |agent| {
            agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .completed
                == prefix.len()
        })
        .await;
        agent
            .handle(Event::Inference(InferenceEvent::TemporaryFailure {
                error: Arc::new(anyhow::anyhow!("stream disconnected")),
                retrying_at: std::time::Instant::now(),
            }))
            .await
            .unwrap();
        assert!(matches!(
            agent.phase,
            Phase::Idle {
                standing: Standing::Nothing,
                ..
            }
        ));
        until(&mut agent, |agent| agent.streams.values().next().unwrap().exec.stream_progress().returned
            && agent.execs.values().next().unwrap().session.sources().iter().any(|(_, facts)|
                matches!(facts, rho_agent_tools::SourceFacts::Job(facts) if facts.finished.is_some())
            )).await;
        agent.flush_events().await.unwrap();
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let recovered = replay::replay(events);
        assert_eq!(recovered.owed.len(), 1);
        assert!(recovered.recovery_notes.is_empty());
        agent.start_request(UnixMs::now(), None).await.unwrap();
        agent.session.abort();
        assert!(!directory.path().join("wrong").exists());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("marker")).unwrap(),
            "x"
        );
        assert!(agent.provider_input().await.unwrap().iter().any(|block| matches!(&**block,
            ContextBlock::ToolResults { results } if results.iter().any(|result| result.body.output.contains("fresh-output")))));
        assert!(agent.provider_input().await.unwrap().iter().any(|block| matches!(&**block,
            ContextBlock::ToolResults { results } if results.iter().any(|result|
                result.body.output.contains("Your response was interrupted while generating this tool call.")
                && result.body.output.contains("fresh-output")))));
        assert!(!agent.provider_input().await.unwrap().iter().any(|block| matches!(&**block,
            ContextBlock::UserMessage { content, .. } if rho_core::text_content(content).contains("stream disconnected"))));
        assert!(agent.recovery_notes.is_empty());
        assert!(matches!(&agent.phase, Phase::Requesting(in_flight) if in_flight.retry.is_none()));
    }
    #[tokio::test]
    async fn changed_source_second_calls_and_oversized_streams_stop_without_replaying() {
        for fault in 0..4 {
            let directory = tempfile::tempdir().unwrap();
            let mut agent = agent(directory.path()).await;
            let prefix = "Path('marker').write_text('x')\n";
            agent.handle(update("one", prefix)).await.unwrap();
            until(&mut agent, |agent| {
                agent
                    .streams
                    .values()
                    .next()
                    .unwrap()
                    .exec
                    .stream_progress()
                    .completed
                    == prefix.len()
            })
            .await;
            let bad = match fault {
                0 => update("one", "Path('wrong').write_text('x')\n"),
                1 => update("changed-id", prefix),
                2 => {
                    let Event::Inference(InferenceEvent::ContextItem { event, .. }) =
                        update("two", "pass\n")
                    else {
                        unreachable!()
                    };
                    Event::Inference(InferenceEvent::ContextItem { index: 1, event })
                }
                _ => update("one", &format!("{prefix}{}", "x".repeat(1024 * 1024))),
            };
            agent.handle(bad).await.unwrap();
            assert!(matches!(
                agent.phase,
                Phase::Idle {
                    standing: Standing::Failed { .. },
                    ..
                }
            ));
            until(&mut agent, |agent| {
                agent
                    .streams
                    .values()
                    .next()
                    .unwrap()
                    .exec
                    .stream_progress()
                    .returned
            })
            .await;
            assert_eq!(
                std::fs::read_to_string(directory.path().join("marker")).unwrap(),
                "x"
            );
            assert!(!directory.path().join("wrong").exists());
            assert_eq!(agent.execs.len(), 1);
        }
    }

    #[tokio::test]
    async fn an_interrupt_boundary_does_not_admit_a_ready_statement() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent
            .handle(update("one", "Path('wrong').write_text('x')\n"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .ready
                .is_none()
            {
                agent.wake.notified().await;
            }
        })
        .await
        .unwrap();
        agent.user.push(QueuedInput {
            source: MessageSender::User,
            kind: InputKind::Message {
                content: vec![ContentPart::Text {
                    text: "stop".into(),
                }],
            },
            delivery: MessageDelivery::Immediate,
            at: UnixMs::now(),
        });
        let decision = agent.decide(UnixMs::now());
        assert_eq!(decision, Boundary::AbortAndResend);
        agent.advance_streams(decision != Boundary::AbortAndResend);
        assert_eq!(
            agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .admitted,
            0
        );
        let exec = agent.streams.values().next().unwrap().exec.clone();
        agent.abandon_stream(UnixMs::now()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while !exec.stream_progress().returned {
                agent.wake.notified().await;
            }
        })
        .await
        .unwrap();
        assert!(agent.streams.is_empty());
        assert!(agent.execs.is_empty());
        assert!(agent.provider_input().await.unwrap().is_empty());
        assert!(!directory.path().join("wrong").exists());
    }

    #[tokio::test]
    async fn successful_response_does_not_retire_a_still_awaiting_unit() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        let source = "import asyncio\ngate = asyncio.Event()\nawait gate.wait()\n";
        agent.handle(update("one", source)).await.unwrap();
        until(&mut agent, |agent| {
            agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .admitted
                == source.len()
        })
        .await;
        agent
            .handle(Event::Inference(InferenceEvent::ContextItem {
                index: 0,
                event: ContextItemEvent::Finish,
            }))
            .await
            .unwrap();
        agent
            .handle(Event::Inference(InferenceEvent::Finished {
                usage: None,
                provider_response_id: None,
            }))
            .await
            .unwrap();
        agent.start_request(UnixMs::now(), None).await.unwrap();
        agent.session.abort();
        assert_eq!(
            agent.streams.len(),
            1,
            "an outstanding await cannot be acknowledged as settled"
        );
        agent.flush_events().await.unwrap();
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let recovered = replay::replay(events);
        assert!(
            recovered.owed.is_empty(),
            "its one provider result was already drained"
        );
        assert!(recovered.recovery_notes.is_empty());
        // Another notebook cell can release it; the original execution still
        // carries the eventual settlement in memory.
        agent.phase = Phase::Requesting(InFlight::default());
        agent.handle(update("two", "gate.set()\n")).await.unwrap();
        until(&mut agent, |agent| {
            agent
                .streams
                .get(&ToolCallId::try_from("one").unwrap())
                .unwrap()
                .exec
                .stream_progress()
                .returned
        })
        .await;
        agent.flush_events().await.unwrap();
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        assert!(
            replay::replay(events)
                .recovery_notes
                .iter()
                .all(|note| !note.contains("bytes 0.."))
        );
        let old = ToolCallId::try_from("one").unwrap();
        agent.acknowledge_streams(&Default::default());
        assert!(
            agent.streams.contains_key(&old),
            "held stream evidence must survive preparation"
        );
        agent.acknowledge_streams(&std::collections::BTreeSet::from([old.clone()]));
        assert!(
            !agent.streams.contains_key(&old),
            "delivered settled evidence may be retired"
        );
    }
    #[tokio::test]
    async fn rewind_rebuilds_recovery_state_instead_of_carrying_abandoned_notes() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent
            .handle(update("one", "survives_rewind = 42\n"))
            .await
            .unwrap();
        agent
            .handle(Event::Inference(InferenceEvent::ContextItem {
                index: 0,
                event: ContextItemEvent::Finish,
            }))
            .await
            .unwrap();
        agent
            .handle(Event::Inference(InferenceEvent::Finished {
                usage: None,
                provider_response_id: None,
            }))
            .await
            .unwrap();
        until(&mut agent, |agent| {
            agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .returned
        })
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !agent.execs.is_empty() {
                agent.start_request(UnixMs::now(), None).await.unwrap();
                agent.session.abort();
                agent.phase = Phase::Idle {
                    owed: Vec::new(),
                    standing: Standing::Nothing,
                };
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("drain the settled cell before rewinding");
        agent
            .persist(AgentEvent::Accepted(QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Message {
                    content: vec![ContentPart::Text {
                        text: "old turn".into(),
                    }],
                },
                delivery: MessageDelivery::NextRequest,
                at: UnixMs::now(),
            }))
            .await
            .unwrap();
        agent.recovery_notes.push("old failed attempt".into());
        agent.rewind(1).await.unwrap();
        assert!(agent.recovery_notes.is_empty());
        assert!(
            !agent.provider_input().await.unwrap().is_empty(),
            "rewind retained the earlier conversation"
        );
        agent.phase = Phase::Requesting(InFlight::default());
        let source = "assert survives_rewind == 42\nPath('survived').write_text('yes')\n";
        agent.handle(update("two", source)).await.unwrap();
        until(&mut agent, |agent| {
            agent
                .streams
                .values()
                .next()
                .unwrap()
                .exec
                .stream_progress()
                .completed
                == source.len()
        })
        .await;
        assert_eq!(
            std::fs::read_to_string(directory.path().join("survived")).unwrap(),
            "yes"
        );
    }
    #[tokio::test]
    async fn partial_exec_failure_uses_normal_command_waiting_and_checkin() {
        for await_job in [false, true] {
            for wake_on_tools in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let mut agent = agent(directory.path()).await;
                let prefix = format!(
                    "set_checkin(after_seconds=600, wake_on_tools={})\njob = command(\"while [ ! -e release ]; do sleep 0.01; done; printf released\")\n{}",
                    if wake_on_tools { "True" } else { "False" },
                    if await_job { "await job\n" } else { "" },
                );
                agent
                    .handle(update("one", &format!("{prefix}unfinished = (")))
                    .await
                    .unwrap();
                until(&mut agent, |agent| {
                    agent
                        .streams
                        .values()
                        .next()
                        .unwrap()
                        .exec
                        .stream_progress()
                        .admitted
                        == prefix.len()
                })
                .await;
                agent
                    .handle(Event::Inference(InferenceEvent::TemporaryFailure {
                        error: Arc::new(anyhow::anyhow!("stream disconnected")),
                        retrying_at: std::time::Instant::now(),
                    }))
                    .await
                    .unwrap();
                let turn = agent.turn.unwrap();
                assert_eq!(turn.asked, ModelAsked::Calls);
                assert!(matches!(
                    agent.phase,
                    Phase::Idle {
                        standing: Standing::Nothing,
                        ..
                    }
                ));
                let history = agent.provider_input().await.unwrap();
                let ContextBlock::InferenceResponse { items, .. } = &**history.last().unwrap()
                else {
                    unreachable!()
                };
                assert_eq!(
                    rho_inference::exec::call(&items[..1])
                        .unwrap()
                        .unwrap()
                        .source,
                    prefix
                );
                assert_eq!(
                    rho_inference::exec::call(&items[..1])
                        .unwrap()
                        .unwrap()
                        .id
                        .as_str(),
                    "one"
                );

                if !await_job {
                    until(&mut agent, |agent| {
                        agent
                            .streams
                            .values()
                            .next()
                            .unwrap()
                            .exec
                            .stream_progress()
                            .returned
                    })
                    .await;
                }
                // Transport retry used to force a request after one second.
                // Neither an unawaited command nor `await job` permits that.
                assert_eq!(
                    agent.decide(turn.spoke_at + Duration::from_secs(1)),
                    Boundary::No {
                        recheck: Some(turn.spoke_at + Duration::from_secs(600))
                    },
                );
                if await_job {
                    agent.collect_stream_notes(None);
                    assert!(agent.recovery_notes.is_empty());
                }
                std::fs::write(directory.path().join("release"), "").unwrap();
                until(&mut agent, |agent| {
                    agent.streams.values().next().unwrap().exec.stream_progress().returned
                        && agent.execs.values().next().unwrap().session.sources().iter().any(|(_, facts)| {
                            matches!(facts, rho_agent_tools::SourceFacts::Job(facts) if facts.finished.is_some())
                        })
                }).await;
                let decision = agent.decide(UnixMs::now());
                if wake_on_tools {
                    assert!(decision.is_now(), "{decision:?}");
                } else {
                    assert_eq!(
                        decision,
                        Boundary::No {
                            recheck: Some(turn.spoke_at + Duration::from_secs(600)),
                        }
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn a_stream_failure_before_admission_keeps_transport_backoff() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent.handle(update("one", "unfinished = (")).await.unwrap();
        agent
            .handle(Event::Inference(InferenceEvent::TemporaryFailure {
                error: Arc::new(anyhow::anyhow!("stream disconnected")),
                retrying_at: std::time::Instant::now(),
            }))
            .await
            .unwrap();
        assert!(matches!(
            agent.phase,
            Phase::Idle {
                standing: Standing::Retry { attempts: 1, .. },
                ..
            }
        ));
        assert!(agent.streams.is_empty());
        assert!(agent.execs.is_empty());
        assert!(agent.provider_input().await.unwrap().is_empty());
        assert!(agent.recovery_notes.is_empty());
        agent.flush_events().await.unwrap();
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let recovered = replay::replay(events);
        assert!(recovered.history.is_empty());
        assert!(recovered.recovery_notes.is_empty());
        let Phase::Idle {
            standing: Standing::Retry { failed_at, .. },
            ..
        } = agent.phase
        else {
            unreachable!()
        };
        assert_eq!(
            agent.decide(failed_at),
            Boundary::No {
                recheck: Some(failed_at + Duration::from_secs(1))
            },
        );
    }
}
