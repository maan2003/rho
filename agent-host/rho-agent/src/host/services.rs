//! Host-owned transactions and policy. Requests are bound to one agent;
//! callers cannot choose another agent id or send arbitrary database writes.
use std::sync::Arc;

use anyhow::Context as _;
use rho_agent_types::AgentId;
use rho_db::RhoDb;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::db::{AgentProfileWriteTxnExt as _, AgentReadTxnExt as _, AgentWriteTxnExt as _};
use crate::inference::Accounts;
use crate::ipc::protocol::{self, Message, Reply, Request};

struct Controls {
    closed: bool,
    pending: std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<(), String>>>,
}

pub(crate) struct Services {
    pub stopped: std::sync::atomic::AtomicBool,
    commands: mpsc::UnboundedSender<Message<'static>>,
    command_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Message<'static>>>>,
    controls: std::sync::Mutex<Controls>,
    next_control: Arc<std::sync::atomic::AtomicU64>,
    pub ready: tokio::sync::watch::Sender<bool>,
    pub db: RhoDb,
    pub agent: AgentId,
    pub status: tokio::sync::watch::Sender<crate::AgentStatus>,
    pool: std::sync::Weak<crate::host::pool::AgentPool>,
    title: tokio::sync::Mutex<crate::title::Task>,
}

impl Services {
    pub(crate) fn new(
        db: RhoDb,
        inference: Accounts,
        agent: AgentId,
        pool: std::sync::Weak<crate::host::pool::AgentPool>,
        next_control: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (ready, _) = tokio::sync::watch::channel(false);
        let title = tokio::sync::Mutex::new(crate::title::Task::new(inference.client()));
        let (status, _) = tokio::sync::watch::channel(crate::AgentStatus::default());
        Self {
            stopped: std::sync::atomic::AtomicBool::new(false),
            commands,
            command_rx: std::sync::Mutex::new(Some(command_rx)),
            controls: std::sync::Mutex::new(Controls {
                closed: false,
                pending: Default::default(),
            }),
            next_control,
            ready,
            db,
            agent,
            title,
            pool,
            status,
        }
    }

    pub(crate) async fn worker_failed(&self, error: String) {
        use rho_agent_types::{TurnEdge, TurnOutcome};

        // A stopping agent host lets its workers go; that is no failure.
        if self.pool.upgrade().is_some_and(|pool| pool.is_draining()) {
            return;
        }

        use crate::db::AgentWriteTxnExt as _;
        let status = crate::AgentStatus {
            runtime: crate::RuntimeState {
                inference: crate::InferenceState::Failed {
                    error: error.clone(),
                },
                ..Default::default()
            },
            ..Default::default()
        };
        // The supervisor knows the process ended, not which unrecorded Python
        // statements ran. Record only that coarse lifecycle fact.
        let mut write = self.db.write().await;
        write.tell_turn(
            rho_agent_types::UnixMs::now(),
            self.agent,
            TurnEdge::Ended(TurnOutcome::Errored { message: error }),
        );
        write.commit();
        if let Some(pool) = self.pool.upgrade() {
            pool.settle_turn(self.agent).await;
            crate::journal::tell_status(
                &self.db,
                self.agent,
                Arc::new(status.clone()),
                Some(Arc::from([])),
            );
        }
        self.status.send_replace(status);
    }

    pub(crate) async fn publish_failure(&self, error: String) {
        if let Some(pool) = self.pool.upgrade() {
            pool.publish_failed_turn(self.agent, error).await;
        }
    }

    pub(crate) fn control(
        &self,
        body: crate::ipc::protocol::Control,
    ) -> anyhow::Result<tokio::sync::oneshot::Receiver<Result<(), String>>> {
        let mut controls = self.controls.lock().expect("poison");
        anyhow::ensure!(!controls.closed, "agent worker connection closed");
        let id = self
            .next_control
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (send, receive) = tokio::sync::oneshot::channel();
        controls.pending.insert(id, send);
        if self.commands.send(Message::Control { id, body }).is_err() {
            controls.pending.remove(&id);
            anyhow::bail!("agent worker connection closed");
        }
        Ok(receive)
    }

    pub(crate) fn serve(
        self: Arc<Self>,
        writer: crate::ipc::transport::Sender,
        requests: crate::ipc::transport::Sender,
        port: crate::ipc::transport::Port,
        mut incoming: mpsc::UnboundedReceiver<(crate::ipc::transport::Packet, Arc<()>)>,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            let mut commands = self
                .command_rx
                .lock()
                .expect("poison")
                .take()
                .expect("one agent host connection");
            // A reply carries its request's in-flight token until written.
            let (outgoing, mut messages) = mpsc::channel::<(Message<'static>, Option<Arc<()>>)>(32);
            let mut calls = JoinSet::new();
            let result = tokio::select! {
                result = async {
                    loop {
                        // Keep an in-progress frame alive while reaping completed
                        // services: cancelling read_exact between its header and
                        // payload would corrupt the channel.
                        let frame = async {
                            let (packet, inflight) = incoming.recv().await.context("agent port closed")?;
                            Ok::<_, anyhow::Error>((protocol::decode(&packet.bytes)?, inflight))
                        };
                        tokio::pin!(frame);
                        let (message, inflight) = loop {
                            tokio::select! {
                                biased;
                                Some(completed) = calls.join_next(), if !calls.is_empty() => {
                                    completed.context("agent service task failed")??;
                                }
                                message = &mut frame, if calls.len() < 32 => break message?,
                            }
                        };
                        let (id, body) = match message {
                            Message::Stopped { error } => {
                                self.stopped.store(true, std::sync::atomic::Ordering::Release);
                                if let Some(error) = error { anyhow::bail!(error); }
                                return Ok(());
                            }
                            Message::Ready { status } => {
                                anyhow::ensure!(!*self.ready.borrow(), "duplicate worker ready frame");
                                self.status.send_replace(status);
                                self.ready.send_replace(true);
                                continue;
                            }
                            Message::Controlled { id, error } => {
                                if let Some(reply) = self.controls.lock().expect("poison").pending.remove(&id) {
                                    let _ = reply.send(error.map_or(Ok(()), Err));
                                }
                                continue;
                            }
                            Message::Status { status, queue } => {
                                // Only focused clients receive the response body. Avoid
                                // cloning its growing text for an unfocused publication.
                                let snapshot = if self.pool.upgrade().is_some_and(|pool| pool.is_live(self.agent)) {
                                    status.clone()
                                } else {
                                    crate::AgentStatus {
                                        runtime: status.runtime.clone(),
                                        response: None,
                                        draft: None,
                                        queued: status.queued,
                                    }
                                };
                                crate::journal::tell_status(
                                    &self.db, self.agent, Arc::new(snapshot), queue.map(Arc::from),
                                );
                                self.status.send_replace(status);
                                continue;
                            }
                            Message::Request { id, body } => (id, body),
                            _ => anyhow::bail!("unexpected agent service message"),
                        };
                        let service = self.clone();
                        let outgoing = outgoing.clone();
                        calls.spawn(async move {
                            let reply = match service.call(id, body, &outgoing).await {
                                Ok(reply) => reply,
                                Err(error) => Reply::Error(error.to_string()),
                            };
                            outgoing.send((Message::Reply { id, body: reply }, Some(inflight))).await
                                .map_err(|_| anyhow::anyhow!("agent connection closed"))
                        });
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), anyhow::Error>(())
                } => result,
                result = async {
                    loop {
                        let message = tokio::select! {
                            biased;
                            message = messages.recv() => message,
                            message = commands.recv() => message.map(|message| (message, None)),
                        };
                        let Some((message, _inflight)) = message else { return Ok(()); };
                        // Answers to the worker's requests return on their connection.
                        let writer = match message {
                            Message::Reply { .. } | Message::HistoryBatch { .. } => &requests,
                            _ => &writer,
                        };
                        writer.send(port, protocol::encode(&message)?).await?;
                    }
                } => result,
            };
            {
                let mut controls = self.controls.lock().expect("poison");
                controls.closed = true;
                controls.pending.clear();
            }
            drop(messages);
            calls.shutdown().await;
            self.title.lock().await.stop().await;
            result
        })
    }

    async fn call(
        &self,
        id: u64,
        request: Request<'static>,
        outgoing: &mpsc::Sender<(Message<'static>, Option<Arc<()>>)>,
    ) -> anyhow::Result<Reply> {
        let reply = match request {
            Request::Team => {
                let team = self
                    .pool
                    .upgrade()
                    .map(|pool| {
                        let parent = self.db.read().get_agent(self.agent).parent;
                        crate::multi_agent_tools::MultiAgentTools::new(
                            Arc::downgrade(&pool),
                            self.agent,
                            parent,
                        )
                        .team()
                    })
                    .transpose()?;
                Reply::Team(team)
            }
            Request::SharedTool(call) => {
                let result = match call {
                    crate::ipc::protocol::SharedCall::Papercut(args) => {
                        crate::papercut::PapercutTool {
                            db: self.db.clone(),
                            agent_id: self.agent,
                        }
                        .record(args)
                        .await
                        .map(|id| format!("Saved papercut #{id}."))
                    }
                    crate::ipc::protocol::SharedCall::Agent(call) => {
                        let pool = self.pool.upgrade().context("agent pool is shutting down")?;
                        let head = self.db.read().get_agent(self.agent);
                        anyhow::ensure!(
                            call.allowed(head.config.role),
                            "not an available host-owned tool"
                        );
                        let tools = crate::multi_agent_tools::MultiAgentTools::new(
                            Arc::downgrade(&pool),
                            self.agent,
                            head.parent,
                        );
                        crate::multi_agent_tools::call_agent_tool(tools, call).await
                    }
                };
                Reply::Shared(match result {
                    Ok(text) => crate::ipc::protocol::SharedReply::Ok(text),
                    Err(error) => crate::ipc::protocol::SharedReply::Err(error.to_string()),
                })
            }
            Request::Usage(usage) => {
                if let Some(pool) = self.pool.upgrade() {
                    pool.record_agent_usage(self.agent, usage).await;
                }
                Reply::Done
            }
            Request::MessageSent(text) => {
                if let Some(pool) = self.pool.upgrade() {
                    pool.publish_message(crate::host::pool::AgentMessage {
                        agent_id: self.agent,
                        text,
                    })
                    .await;
                }
                Reply::Done
            }
            Request::Failed(error) => {
                if let Some(pool) = self.pool.upgrade() {
                    pool.publish_failed_turn(self.agent, error).await;
                }
                Reply::Done
            }
            Request::Settled => {
                if let Some(pool) = self.pool.upgrade() {
                    pool.settle_turn(self.agent).await;
                }
                Reply::Done
            }
            Request::Name(input) => {
                let db = self.db.clone();
                let agent = self.agent;
                let outgoing = outgoing.clone();
                self.title
                    .lock()
                    .await
                    .start(&self.db, agent, &input, move |result| async move {
                        crate::title::finish(&db, agent, result).await;
                        let head = db.read().get_agent(agent);
                        let _ = outgoing.send((Message::Named(head), None)).await;
                    })
                    .await;
                Reply::Head(self.db.read().get_agent(agent))
            }
            Request::Head => Reply::Head(self.db.read().get_agent(self.agent)),
            Request::History | Request::NativeHistory(_) => {
                let read = self.db.read();
                let native = if let Request::NativeHistory(boundary) = request {
                    Some((
                        boundary.unwrap_or_else(|| read.agent_context_boundary(self.agent)),
                        read.agent_native_recovery(self.agent),
                    ))
                } else {
                    None
                };
                let (next, rows) = match &native {
                    Some((boundary, _)) => (
                        boundary.through,
                        read.agent_context_records(self.agent, *boundary),
                    ),
                    None => read.agent_event_records(self.agent),
                };
                drop(read);
                let mut batch = Vec::new();
                let mut bytes = 0;
                for row in rows {
                    let size = senax_encoder::encode(&row)?.len();
                    if bytes + size > 1024 * 1024 && !batch.is_empty() {
                        outgoing
                            .send((
                                Message::HistoryBatch {
                                    id,
                                    rows: std::mem::take(&mut batch),
                                },
                                None,
                            ))
                            .await?;
                        bytes = 0;
                    }
                    bytes += size;
                    batch.push(row);
                }
                if !batch.is_empty() {
                    outgoing
                        .send((Message::HistoryBatch { id, rows: batch }, None))
                        .await?;
                }
                match native {
                    Some((boundary, recovery)) => Reply::NativeHistory {
                        boundary,
                        recovery,
                        rows: Vec::new(),
                    },
                    None => Reply::History {
                        next,
                        rows: Vec::new(),
                    },
                }
            }
            Request::Append(event) => {
                let mut write = self.db.write().await;
                let position = write.append_agent_event(self.agent, &event);
                write.commit();
                Reply::Position(position)
            }
            Request::AppendBatch(events) => {
                let mut write = self.db.write().await;
                for event in &events {
                    write.append_agent_event(self.agent, event);
                    if let crate::AgentEvent::Entry(crate::entry::Entry::Step {
                        usage: Some(usage),
                        at,
                        ..
                    }) = event
                    {
                        write.add_agent_usage(
                            self.agent,
                            &crate::log::AgentUsageBucket {
                                bucket_start_ms: at.0 / crate::log::AGENT_USAGE_BUCKET_MS
                                    * crate::log::AGENT_USAGE_BUCKET_MS,
                                model: crate::log::AgentUsageModel::named(&usage.model),
                                input_tokens: usage.input_tokens,
                                cache_read_tokens: usage.cache_read_tokens,
                                cache_write_tokens: usage.cache_write_tokens,
                                cache_write_1h_tokens: usage.cache_write_1h_tokens,
                                output_tokens: usage.output_tokens,
                                requests: 1,
                                approximate: false,
                            },
                        );
                    }
                }
                let boundary = write.agent_context_boundary(self.agent);
                write.commit();
                Reply::Boundary(boundary)
            }
            Request::Profile { role, binding } => {
                let mut write = self.db.write().await;
                write.set_agent_profile(self.agent, role, binding);
                write.commit();
                Reply::Done
            }
            Request::CacheKey(key) => {
                let mut write = self.db.write().await;
                write.set_agent_prompt_cache_key(self.agent, key);
                write.commit();
                Reply::Done
            }
            Request::Rewind { at, to } => {
                let mut write = self.db.write().await;
                write.rewind_agent(at, self.agent, to);
                write.commit();
                Reply::Done
            }
            Request::ClaudeRewind { at, to, rewind } => {
                let mut write = self.db.write().await;
                if let Some(to) = to {
                    write.rewind_agent(at, self.agent, to);
                }
                write.set_agent_claude_rewind(self.agent, rewind);
                write.commit();
                Reply::Done
            }
            Request::CompleteClaudeRewind(session) => {
                let mut write = self.db.write().await;
                write.complete_agent_claude_rewind(self.agent, session);
                write.commit();
                Reply::Done
            }
            Request::ClaudePendingOutput => {
                Reply::ClaudePendingOutput(self.db.read().agent_pending_claude_output(self.agent))
            }
            Request::ClaudeAccount => Reply::ClaudeAccount(self.db.read().claude_account()),
            Request::UsageTotal => Reply::Usage(self.db.read().agent_usage_total(self.agent)),
            Request::Turn { at, edge } => {
                let mut write = self.db.write().await;
                write.tell_turn(at, self.agent, edge);
                write.commit();
                Reply::Done
            }
        };
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentRole, UnixMs};

    use super::*;
    use crate::AgentEvent;
    use crate::log::{AgentRoleSessionProfile as _, AgentRuntime};

    #[tokio::test]
    async fn blocked_append_does_not_block_reads_and_history_crosses_multiple_frames() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("rho.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent = write.alloc_agent_id();
        let role = AgentRole::default();
        write.create_agent(
            UnixMs(1),
            agent,
            None,
            crate::db::tests::test_workspace(),
            role,
            role.session_profile(),
            AgentRuntime::Rho {
                prompt_cache_key: crate::inference::PromptCacheKey::generate(),
            },
            crate::log::AgentOrigin::User,
        );
        for _ in 0..3 {
            write.append_agent_event(
                agent,
                &AgentEvent::Entry(crate::entry::Entry::Status {
                    text: "x".repeat(700_000),
                    at: UnixMs(2),
                }),
            );
        }
        write.commit();
        let inference = crate::inference::testing::accounts();
        let services = Arc::new(Services::new(
            db.clone(),
            inference,
            agent,
            Default::default(),
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
        ));
        let (client, server) = crate::testing::pair();
        let server = tokio::spawn(services.clone().serve(
            server.sender.clone(),
            server.sender,
            server.port,
            crate::testing::route(server.incoming),
        ));
        let host = client.host();
        let locked = db.write().await;
        let append = tokio::spawn({
            let host = host.clone();
            async move {
                host.append(AgentEvent::Notice {
                    text: "last".into(),
                    at: UnixMs(3),
                })
                .await
            }
        });
        let head = tokio::time::timeout(std::time::Duration::from_secs(2), host.head())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(head, db.read().get_agent(agent));
        assert!(!append.is_finished());
        drop(locked);
        let appended = append.await.unwrap().unwrap();
        let (next, rows) = host.history().await.unwrap();
        assert_eq!(next, appended.next());
        assert_eq!((next, rows), db.read().agent_event_records(agent));
        let (boundary, _, entries) = host.native_history(None).await.unwrap();
        assert_eq!(boundary.through, next);
        assert_eq!(entries.len(), 3);
        assert!(
            entries.iter().all(|entry| matches!(
                entry, crate::entry::Entry::Status { text, .. } if text.len() == 700_000
            )),
            "all native history frames must reach the caller"
        );

        // More than the service concurrency bound must wait, never return
        // "too many pending agent services" or lose an append.
        let locked = db.write().await;
        let mut pending = JoinSet::new();
        for index in 0..80 {
            let host = host.clone();
            pending.spawn(async move {
                host.append(AgentEvent::Notice {
                    text: format!("queued-{index}").into(),
                    at: UnixMs(4),
                })
                .await
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            pending.try_join_next().is_none(),
            "a busy store rejected work"
        );
        drop(locked);
        let mut positions = std::collections::BTreeSet::new();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(result) = pending.join_next().await {
                positions.insert(result.unwrap().unwrap());
            }
        })
        .await
        .unwrap();
        assert_eq!(positions.len(), 80);
        let (after, rows) = host.history().await.unwrap();
        assert_eq!(after.pos, next.pos + 80);
        let labels = rows
            .iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Notice { text, .. } if text.starts_with("queued-") => {
                    Some(text.to_string())
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            labels,
            (0..80).map(|index| format!("queued-{index}")).collect()
        );
        drop(host);
        assert!(server.await.unwrap().is_err());

        // The agent host committed, but the worker never received its reply.
        // Neither transport nor store client is allowed to resend the append.
        let (client, mut server) = crate::testing::pair();
        let host = client.host();
        let append = tokio::spawn({
            let host = host.clone();
            async move {
                host.append(AgentEvent::Notice {
                    text: "lost-ack".into(),
                    at: UnixMs(4),
                })
                .await
            }
        });
        let Message::Request { id, body } = server.read().await.unwrap() else {
            panic!()
        };
        let (outgoing, _incoming) = mpsc::channel(1);
        assert!(matches!(
            services.call(id, body, &outgoing).await.unwrap(),
            Reply::Position(_)
        ));
        drop(server);
        assert!(append.await.unwrap().is_err());
        let (_, records) = db.read().agent_event_records(agent);
        assert_eq!(
            records
                .iter()
                .filter(|(_, event)| matches!(
                    event, AgentEvent::Notice { text, .. } if text == "lost-ack"
                ))
                .count(),
            1
        );
    }
}
