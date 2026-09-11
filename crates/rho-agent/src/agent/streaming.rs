//! Streaming admission belongs to the agent: write intent before permitting
//! Python, and write settlement before admitting another unit. Provider EOF
//! and transport loss are deliberately different operations.
use rho_core::{StreamingContextItem, StreamingContextItemState};

use super::*;
use crate::PythonStreamEvent;

pub(super) struct Stream {
    pub exec: Arc<rho_agent_tools::PythonExec>,
    item: InferenceResponseItem,
    index: usize,
    source: String,
    admitted: usize,
    settled: usize,
    completed: usize,
    stopped: bool,
    closed: bool,
    pub canonical: bool,
    recovery: bool,
    pub interrupted: bool,
}

fn call(item: &InferenceResponseItem) -> ToolCall {
    let InferenceResponseItem::ToolCall {
        id,
        name,
        tool_type,
        arguments,
        ..
    } = item
    else {
        unreachable!()
    };
    ToolCall {
        id: id.clone(),
        name: name.clone(),
        tool_type: *tool_type,
        arguments: arguments.clone(),
    }
}

fn set_source(item: &mut InferenceResponseItem, source: String) {
    let InferenceResponseItem::ToolCall { arguments, .. } = item else {
        unreachable!()
    };
    *arguments = source;
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
    pub(super) async fn update_stream(&mut self, now: UnixMs) -> Result<(), String> {
        if self.surface.get_if_ready().and_then(|s| s.code_mode)
            != Some(rho_agent_tools::CodeMode::Python)
        {
            return Ok(());
        }
        let Phase::Requesting(in_flight) = &self.phase else {
            return Ok(());
        };
        let mut calls = Vec::new();
        for (index, state) in in_flight.pending.items.iter().enumerate() {
            if let StreamingContextItemState::Pending(item)
            | StreamingContextItemState::Finished(item) = state
                && matches!(item, StreamingContextItem::ToolCall { .. })
            {
                calls.push((
                    index,
                    item.to_context_item().map_err(|error| error.to_string())?,
                ));
            }
        }
        if calls.len() > 1 {
            return Err("Python streaming permits only one exec call; previously admitted code is not undone".into());
        }
        let Some((index, item)) = calls.pop() else {
            return if in_flight.stream.is_some() {
                Err("Provider removed an active streaming exec call".into())
            } else {
                Ok(())
            };
        };
        let incoming = call(&item);
        if incoming.name.as_str() != "exec" || incoming.tool_type != rho_core::ToolType::Custom {
            return Err("Python streaming requires a custom exec call".into());
        }
        if incoming.arguments.len() > 1024 * 1024 {
            return Err("Streaming Python source exceeds 1 MiB".into());
        }
        if let Some(id) = &in_flight.stream {
            if id != &incoming.id {
                return Err("Provider changed the streaming exec identity".into());
            }
            let stream = self.streams.get_mut(id).unwrap();
            let mut identity = item.clone();
            set_source(&mut identity, String::new());
            if index != stream.index
                || identity != stream.item
                || !incoming.arguments.starts_with(&stream.source)
            {
                return Err(
                    "Provider changed previously received Python source or call metadata".into(),
                );
            }
            let fragment = incoming.arguments[stream.source.len()..].to_owned();
            stream.source = incoming.arguments;
            self.tools.get_mut(id).unwrap().call.arguments = stream.source.clone();
            if !stream.stopped && !stream.closed && !fragment.is_empty() {
                stream.exec.feed(fragment, false)?;
            }
            return Ok(());
        }
        if self.tools.contains_key(&incoming.id) || self.history.iter().any(|block| {
            matches!(&**block, ContextBlock::InferenceResponse { items, .. } if items.iter().any(|item| {
                matches!(item, InferenceResponseItem::ToolCall { id, .. } if id == &incoming.id)
            }))
        }) {
            return Err("Provider reused an earlier tool call identity".into());
        }
        let tool = self
            .surface
            .get_if_ready()
            .unwrap()
            .tools
            .get(&incoming.name)
            .unwrap();
        let session = tool
            .start_stream(SourceWaker::new(self.wake.clone()))
            .ok_or("Python tool does not support streaming")?;
        let exec = session
            .python_exec()
            .ok_or("Python stream failed to start")?;
        let mut identity = item;
        set_source(&mut identity, String::new());
        self.persist(AgentEvent::PythonStream {
            event: PythonStreamEvent::Opened {
                item: identity.clone(),
            },
            at: now,
        })
        .await;
        self.tools.insert(
            incoming.id.clone(),
            RunningTool {
                call: incoming.clone(),
                started_at: now,
                session,
                answer: ToolCallAnswer::Owed,
                answered_sources: Default::default(),
            },
        );
        self.latest_python_exec = Some((incoming.id.clone(), exec.clone()));
        self.streams.insert(
            incoming.id.clone(),
            Stream {
                exec: exec.clone(),
                item: identity,
                index,
                source: incoming.arguments.clone(),
                admitted: 0,
                settled: 0,
                completed: 0,
                stopped: false,
                closed: false,
                canonical: false,
                recovery: false,
                interrupted: false,
            },
        );
        let Phase::Requesting(in_flight) = &mut self.phase else {
            unreachable!()
        };
        in_flight.stream = Some(incoming.id);
        exec.feed(incoming.arguments, false)
    }

    pub(super) async fn advance_streams(&mut self, now: UnixMs, admit: bool) {
        let ids = self.streams.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let mut stream = self.streams.remove(&id).unwrap();
            let progress = stream.exec.stream_progress();
            if let Some((end, error)) = progress.settled
                && end > stream.settled
            {
                self.persist(AgentEvent::PythonStream {
                    event: PythonStreamEvent::Settled {
                        call_id: id.clone(),
                        end: end as u64,
                        error: error.clone(),
                    },
                    at: now,
                })
                .await;
                stream.settled = end;
                stream.recovery |= stream.stopped;
                if error.is_none() {
                    stream.completed = end;
                } else {
                    stream.stopped = true;
                    stream.exec.stop_stream();
                }
            }
            if progress.returned && !stream.closed {
                self.persist(AgentEvent::PythonStream {
                    event: PythonStreamEvent::Closed {
                        call_id: id.clone(),
                    },
                    at: now,
                })
                .await;
                stream.closed = true;
            }
            if admit
                && !stream.stopped
                && !stream.closed
                && stream.settled == stream.admitted
                && let Some(end) = progress.ready
                && end > stream.admitted
            {
                // Compiler offsets refer to the validated original UTF-8 source.
                let source = stream.source[stream.admitted..end].to_owned();
                self.persist(AgentEvent::PythonStream {
                    event: PythonStreamEvent::Admitted {
                        call_id: id.clone(),
                        source,
                    },
                    at: now,
                })
                .await;
                stream.admitted = end;
                if let Err(error) = stream.exec.permit(end) {
                    stream.stopped = true;
                    stream.recovery = true;
                    stream.exec.stop_stream();
                    self.recovery_notes.push(error);
                }
            }
            self.streams.insert(id, stream);
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
        if !stream.closed && !stream.stopped {
            // Only validated successful response completion closes the last unit.
            stream.exec.feed(String::new(), true)?;
        }
        Ok(())
    }

    pub(super) async fn abandon_stream(&mut self, now: UnixMs) -> bool {
        let Phase::Requesting(in_flight) = &mut self.phase else {
            return false;
        };
        let Some(id) = in_flight.stream.take() else {
            return false;
        };
        let stream = self.streams.get_mut(&id).unwrap();
        stream.stopped = true;
        stream.interrupted = true;
        stream.exec.stop_stream();
        if stream.admitted == 0 {
            self.streams.remove(&id);
            self.tools.remove(&id);
            self.latest_python_exec = None;
            self.persist(AgentEvent::PythonStream {
                event: PythonStreamEvent::Acknowledged { call_id: id },
                at: now,
            })
            .await;
            return false;
        }
        let mut item = stream.item.clone();
        set_source(&mut item, stream.source[..stream.admitted].to_owned());
        stream.canonical = true;
        let block = ContextBlock::InferenceResponse {
            items: vec![item],
            provider_response_id: None,
        };
        // Give the admitted, syntactically complete prefix its one place in
        // history before the normal boundary drains its result.
        self.persist(AgentEvent::Replied {
            blocks: Cow::Borrowed(std::slice::from_ref(&block)),
            context_used: self.context_used,
            usage: None,
            at: now,
        })
        .await;
        self.history.push(Arc::new(block));
        true
    }

    /// Called only after the boundary's Sent has committed its output and
    /// notes.
    pub(super) async fn acknowledge_streams(&mut self, now: UnixMs) {
        let mut ids = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.closed && stream.canonical)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        ids.extend(std::mem::take(&mut self.recovery_streams));
        for id in ids {
            self.persist(AgentEvent::PythonStream {
                event: PythonStreamEvent::Acknowledged {
                    call_id: id.clone(),
                },
                at: now,
            })
            .await;
            self.streams.remove(&id);
        }
    }

    pub(super) fn collect_stream_notes(&mut self) {
        for (id, stream) in &mut self.streams {
            if !stream.interrupted
                && (stream.recovery || (stream.closed && stream.completed != stream.source.len()))
            {
                self.recovery_notes.push(progress_note(
                    id,
                    &stream.source,
                    stream.completed,
                    stream.admitted,
                    if stream.completed == stream.admitted {
                        "no outstanding admitted statement"
                    } else if stream.settled < stream.admitted && !stream.closed {
                        "admitted and still running or waiting to run"
                    } else {
                        "did not complete successfully; any side effects remain"
                    },
                ));
                stream.recovery = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_core::{AppendString, ContextItemEvent, ProviderResponseItemId};
    use rho_inference::OpenAiResponsesProviderData;

    use super::*;

    async fn agent(directory: &std::path::Path) -> Agent {
        let db = RhoDb::open(directory.join("agent.redb"));
        let inference = Inference::new_with_config(
            db.clone(),
            rho_inference::InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        )
        .await
        .unwrap();
        let repo = Arc::new(
            rho_workspaces::Repo::open_plain_with_path_overrides(directory, Default::default())
                .unwrap(),
        );
        let workspace = repo.user_checkout().await.unwrap();
        let view = View::new(vec![workspace.clone()]).unwrap();
        let role = AgentRole::Engineer {
            intelligence: EngineerIntelligence::Python,
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
            vec![workspace.info().clone()],
            role,
            binding,
            AgentRuntime::Rho {
                prompt_cache_key: key,
            },
            None,
        );
        write.commit();
        let head = db.read().get_agent(id);
        let tool: Arc<dyn Tool> = Arc::new(
            rho_agent_tools::PythonTool::new(
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
            code_mode: Some(rho_agent_tools::CodeMode::Python),
            view: view.clone(),
            instructions: Arc::from("test"),
            tools: BTreeMap::from([(tool.spec().name, tool)]),
        };
        let (control, control_rx) = mpsc::unbounded_channel();
        Agent {
            db,
            agent_id: id,
            pool: Default::default(),
            surface: Arc::new(Lazy::ready(surface)),
            surface_inputs: SurfaceInputs {
                view: Arc::new(Lazy::ready(view)),
                agent_id: id,
                inference: inference.clone(),
                parent: None,
                pool: Default::default(),
            },
            model,
            history: Vec::new(),
            session: inference.deep_session(profile, model, key),
            phase: Phase::Requesting(InFlight::default()),
            user: Vec::new(),
            mail: Vec::new(),
            tools: BTreeMap::new(),
            wait_answers: Vec::new(),
            streams: BTreeMap::new(),
            recovery_notes: Vec::new(),
            recovery_blocks: Vec::new(),
            recovery_streams: Vec::new(),
            context_used: None,
            turn: None,
            latest_python_exec: None,
            total_usage: Default::default(),
            sidecar: Sidecar::new(inference, None),
            working: false,
            wake: Arc::new(Notify::new()),
            status: Arc::new(RwLock::new(AgentStatus {
                kind: AgentStateKind::Idle,
                queued: 0,
            })),
            head: Arc::new(RwLock::new(head)),
            teller: Default::default(),
            control: control.downgrade(),
            control_rx,
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
                agent.advance_streams(UnixMs::now(), true).await;
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
    async fn commands_start_before_response_end_and_final_response_does_not_replay() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent
            .handle(update(
                "one",
                "job = command(\"printf x >> marker; printf fresh-output\")\n",
            ))
            .await;
        until(&mut agent, |agent| {
            agent.streams.values().next().unwrap().completed > 0
        })
        .await;
        let cell = agent.latest_python_exec.as_ref().unwrap().1.sequence();
        // The response is still in flight, but the command is already running.
        assert!(matches!(agent.phase, Phase::Requesting(_)));
        until(&mut agent, |agent| agent.tools.values().next().unwrap().session.sources().iter().any(|(_, facts)|
            matches!(facts, rho_agent_tools::SourceFacts::PythonOperation(facts) if facts.finished.is_some())
        )).await;
        assert_eq!(
            std::fs::read_to_string(directory.path().join("marker")).unwrap(),
            "x"
        );
        agent
            .handle(Event::Inference(InferenceEvent::ContextItem {
                index: 0,
                event: ContextItemEvent::Finish,
            }))
            .await;
        agent
            .handle(Event::Inference(InferenceEvent::Finished {
                usage: None,
                provider_response_id: None,
            }))
            .await;
        until(&mut agent, |agent| {
            agent.streams.values().next().unwrap().closed
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
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let recovered = replay::replay(events);
        assert!(recovered.recovery_notes[0].contains("successfully evaluated"));
        agent.start_request(UnixMs::now()).await;
        agent.session.abort();
        assert!(agent.history.iter().any(|block| matches!(&**block,
            ContextBlock::ToolResults { results } if results.iter().any(|result| result.body.output.contains("fresh-output")))));
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        assert!(replay::replay(events).recovery_notes.is_empty());
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
            .await;
        until(&mut agent, |agent| {
            agent.streams.values().next().unwrap().completed == prefix.len()
        })
        .await;
        agent
            .handle(Event::Inference(InferenceEvent::TemporaryFailure {
                error: Arc::new(anyhow::anyhow!("stream disconnected")),
                retrying_at: std::time::Instant::now(),
            }))
            .await;
        assert!(matches!(
            agent.phase,
            Phase::Idle {
                standing: Standing::Nothing,
                ..
            }
        ));
        until(&mut agent, |agent| agent.streams.values().next().unwrap().closed
            && agent.tools.values().next().unwrap().session.sources().iter().any(|(_, facts)|
                matches!(facts, rho_agent_tools::SourceFacts::PythonOperation(facts) if facts.finished.is_some())
            )).await;
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let recovered = replay::replay(events);
        assert!(recovered.recovery_notes[0].contains(&format!("bytes 0..{}", prefix.len())));
        agent.start_request(UnixMs::now()).await;
        agent.session.abort();
        assert!(!directory.path().join("wrong").exists());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("marker")).unwrap(),
            "x"
        );
        assert!(agent.history.iter().any(|block| matches!(&**block,
            ContextBlock::ToolResults { results } if results.iter().any(|result| result.body.output.contains("fresh-output")))));
        assert!(agent.history.iter().any(|block| matches!(&**block,
            ContextBlock::ToolResults { results } if results.iter().any(|result|
                result.body.output.contains("Your response was interrupted while generating this tool call.")
                && result.body.output.contains("fresh-output")))));
        assert!(!agent.history.iter().any(|block| matches!(&**block,
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
            agent.handle(update("one", prefix)).await;
            until(&mut agent, |agent| {
                agent.streams.values().next().unwrap().completed == prefix.len()
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
            agent.handle(bad).await;
            assert!(matches!(
                agent.phase,
                Phase::Idle {
                    standing: Standing::Failed { .. },
                    ..
                }
            ));
            until(&mut agent, |agent| {
                agent.streams.values().next().unwrap().closed
            })
            .await;
            assert_eq!(
                std::fs::read_to_string(directory.path().join("marker")).unwrap(),
                "x"
            );
            assert!(!directory.path().join("wrong").exists());
            assert_eq!(agent.tools.len(), 1);
        }
    }

    #[tokio::test]
    async fn an_interrupt_boundary_does_not_admit_a_ready_statement() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent
            .handle(update("one", "Path('wrong').write_text('x')\n"))
            .await;
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
        let decision = boundary(
            &agent.sources(),
            agent.turn.as_ref(),
            &agent.phase,
            UnixMs::now(),
        );
        assert_eq!(decision, Boundary::AbortAndResend);
        agent
            .advance_streams(UnixMs::now(), decision != Boundary::AbortAndResend)
            .await;
        assert_eq!(agent.streams.values().next().unwrap().admitted, 0);
        let exec = agent.streams.values().next().unwrap().exec.clone();
        agent.abandon_stream(UnixMs::now()).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            while !exec.stream_progress().returned {
                agent.wake.notified().await;
            }
        })
        .await
        .unwrap();
        assert!(agent.streams.is_empty());
        assert!(agent.tools.is_empty());
        assert!(agent.history.is_empty());
        assert!(!directory.path().join("wrong").exists());
    }

    #[tokio::test]
    async fn successful_response_does_not_retire_a_still_awaiting_unit() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        let source = "import asyncio\ngate = asyncio.Event()\nawait gate.wait()\n";
        agent.handle(update("one", source)).await;
        until(&mut agent, |agent| {
            agent.streams.values().next().unwrap().admitted == source.len()
        })
        .await;
        agent
            .handle(Event::Inference(InferenceEvent::ContextItem {
                index: 0,
                event: ContextItemEvent::Finish,
            }))
            .await;
        agent
            .handle(Event::Inference(InferenceEvent::Finished {
                usage: None,
                provider_response_id: None,
            }))
            .await;
        agent.start_request(UnixMs::now()).await;
        agent.session.abort();
        assert_eq!(
            agent.streams.len(),
            1,
            "an outstanding await cannot be acknowledged as settled"
        );
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let recovered = replay::replay(events);
        assert!(
            recovered.owed.is_empty(),
            "its one provider result was already drained"
        );
        assert!(recovered.recovery_notes[0].contains("await gate.wait()"));
        // Another notebook cell can release it; the original execution still
        // carries the eventual settlement and must remain journaled.
        agent.phase = Phase::Requesting(InFlight::default());
        agent.handle(update("two", "gate.set()\n")).await;
        until(&mut agent, |agent| {
            agent
                .streams
                .get(&ToolCallId::try_from("one").unwrap())
                .unwrap()
                .closed
        })
        .await;
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        assert!(
            replay::replay(events)
                .recovery_notes
                .iter()
                .any(|note| note.contains(&format!("bytes 0..{}", source.len())))
        );
    }
    #[tokio::test]
    async fn rewind_rebuilds_recovery_state_instead_of_carrying_abandoned_notes() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent.phase = Phase::Idle {
            owed: Vec::new(),
            standing: Standing::Nothing,
        };
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
            .await;
        agent.recovery_notes.push("old failed attempt".into());
        agent
            .recovery_streams
            .push(ToolCallId::try_from("old").unwrap());
        agent.rewind(1).await.unwrap();
        assert!(agent.recovery_notes.is_empty());
        assert!(agent.recovery_streams.is_empty());
        assert!(agent.recovery_blocks.is_empty());
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
                    .await;
                until(&mut agent, |agent| {
                    agent.streams.values().next().unwrap().admitted == prefix.len()
                })
                .await;
                agent
                    .handle(Event::Inference(InferenceEvent::TemporaryFailure {
                        error: Arc::new(anyhow::anyhow!("stream disconnected")),
                        retrying_at: std::time::Instant::now(),
                    }))
                    .await;
                let turn = agent.turn.unwrap();
                assert_eq!(turn.asked, ModelAsked::Calls);
                assert!(matches!(
                    agent.phase,
                    Phase::Idle {
                        standing: Standing::Nothing,
                        ..
                    }
                ));
                let ContextBlock::InferenceResponse { items, .. } =
                    &**agent.history.last().unwrap()
                else {
                    unreachable!()
                };
                assert_eq!(call(&items[0]).arguments, prefix);
                assert_eq!(call(&items[0]).id.as_str(), "one");

                if !await_job {
                    until(&mut agent, |agent| {
                        agent.streams.values().next().unwrap().closed
                    })
                    .await;
                }
                // Transport retry used to force a request after one second.
                // Neither an unawaited command nor `await job` permits that.
                assert_eq!(
                    boundary(
                        &agent.sources(),
                        agent.turn.as_ref(),
                        &agent.phase,
                        turn.spoke_at + Duration::from_secs(1)
                    ),
                    Boundary::No {
                        recheck: Some(turn.spoke_at + Duration::from_secs(600))
                    },
                );
                if await_job {
                    agent.collect_stream_notes();
                    assert!(agent.recovery_notes.is_empty());
                }
                std::fs::write(directory.path().join("release"), "").unwrap();
                until(&mut agent, |agent| {
                    agent.streams.values().next().unwrap().closed
                        && agent.tools.values().next().unwrap().session.sources().iter().any(|(_, facts)| {
                            matches!(facts, rho_agent_tools::SourceFacts::PythonOperation(facts) if facts.finished.is_some())
                        })
                }).await;
                assert_eq!(
                    boundary(
                        &agent.sources(),
                        agent.turn.as_ref(),
                        &agent.phase,
                        UnixMs::now()
                    ),
                    if wake_on_tools {
                        Boundary::Now
                    } else {
                        Boundary::No {
                            recheck: Some(turn.spoke_at + Duration::from_secs(600)),
                        }
                    },
                );
            }
        }
    }

    #[tokio::test]
    async fn a_stream_failure_before_admission_keeps_transport_backoff() {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent.handle(update("one", "unfinished = (")).await;
        agent
            .handle(Event::Inference(InferenceEvent::TemporaryFailure {
                error: Arc::new(anyhow::anyhow!("stream disconnected")),
                retrying_at: std::time::Instant::now(),
            }))
            .await;
        assert!(matches!(
            agent.phase,
            Phase::Idle {
                standing: Standing::Retry { attempts: 1, .. },
                ..
            }
        ));
        assert!(agent.streams.is_empty());
        assert!(agent.tools.is_empty());
        assert!(agent.history.is_empty());
        assert!(agent.recovery_notes.is_empty());
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let recovered = replay::replay(events);
        assert!(recovered.history.is_empty());
        assert!(recovered.recovery_blocks.is_empty());
        assert!(recovered.recovery_notes.is_empty());
        let Phase::Idle {
            standing: Standing::Retry { failed_at, .. },
            ..
        } = agent.phase
        else {
            unreachable!()
        };
        assert_eq!(
            boundary(
                &agent.sources(),
                agent.turn.as_ref(),
                &agent.phase,
                failed_at
            ),
            Boundary::No {
                recheck: Some(failed_at + Duration::from_secs(1))
            },
        );
    }
}
