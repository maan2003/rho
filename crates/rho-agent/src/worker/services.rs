//! Daemon-owned transactions and policy. Requests are bound to one agent;
//! callers cannot choose another agent id or send arbitrary database writes.
use std::sync::Arc;

use anyhow::Context as _;
use rho_core::AgentId;
use rho_db::RhoDb;
use rho_inference::Inference;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::ipc::{self, Message, Reply, Request};
use crate::db::{AgentProfileWriteTxnExt as _, AgentReadTxnExt as _, AgentWriteTxnExt as _};

struct Controls {
    closed: bool,
    pending: std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<(), String>>>,
}

pub(super) struct Services {
    pub stopped: std::sync::atomic::AtomicBool,
    commands: mpsc::UnboundedSender<Message<'static>>,
    command_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Message<'static>>>>,
    controls: std::sync::Mutex<Controls>,
    next_control: Arc<std::sync::atomic::AtomicU64>,
    pub ready: tokio::sync::watch::Sender<bool>,
    pub db: RhoDb,
    pub agent: AgentId,
    pub status: tokio::sync::watch::Sender<crate::AgentStatus>,
    pool: std::sync::Weak<crate::pool::AgentPool>,
    title: tokio::sync::Mutex<crate::title::Task>,
}

impl Services {
    pub(super) fn new(
        db: RhoDb,
        inference: Inference,
        agent: AgentId,
        pool: std::sync::Weak<crate::pool::AgentPool>,
        next_control: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (ready, _) = tokio::sync::watch::channel(false);
        let title = tokio::sync::Mutex::new(crate::title::Task::new(inference));
        let (status, _) = tokio::sync::watch::channel(crate::AgentStatus {
            kind: crate::AgentStateKind::Idle,
            queued: 0,
        });
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

    pub(super) async fn worker_failed(&self, error: String) {
        use crate::db::{AgentWriteTxnExt as _, TurnEdge, TurnOutcome};
        let status = crate::AgentStatus {
            kind: crate::AgentStateKind::Error(crate::FailedInferenceResponse {
                partial_response: Default::default(),
                attempt_count: std::num::NonZeroU64::MIN,
                error: Arc::new(error.clone()),
            }),
            queued: 0,
        };
        // The supervisor knows the process ended, not which unrecorded Python
        // statements ran. Record only that coarse lifecycle fact.
        let mut write = self.db.write().await;
        write.tell_turn(
            rho_core::UnixMs::now(),
            self.agent,
            TurnEdge::Ended(TurnOutcome::Errored { message: error }),
        );
        write.commit();
        if let Some(pool) = self.pool.upgrade() {
            pool.settle_turn(self.agent).await;
            if pool.is_live(self.agent) {
                for live in crate::live::Teller::default().tell(&status.kind) {
                    crate::mirror::tell_live(&self.db, self.agent, live);
                }
            }
        }
        self.status.send_replace(status);
    }

    pub(super) async fn publish_failure(&self, error: String) {
        if let Some(pool) = self.pool.upgrade() {
            pool.publish_failed_turn(self.agent, error).await;
        }
    }

    pub(super) fn control(
        &self,
        body: super::ipc::Control,
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

    pub(super) fn serve(
        self: Arc<Self>,
        writer: super::transport::Sender,
        port: super::transport::Port,
        mut incoming: mpsc::UnboundedReceiver<super::transport::Packet>,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            let mut commands = self
                .command_rx
                .lock()
                .expect("poison")
                .take()
                .expect("one daemon connection");
            let (outgoing, mut messages) = mpsc::channel::<Message<'static>>(32);
            let mut calls = JoinSet::new();
            let mut teller = crate::live::Teller::default();
            let result = tokio::select! {
                result = async {
                    loop {
                        // Keep an in-progress frame alive while reaping completed
                        // services: cancelling read_exact between its header and
                        // payload would corrupt the channel.
                        let frame = async {
                            let bytes = incoming.recv().await.context("agent port closed")?;
                            Ok::<_, anyhow::Error>(ipc::decode(&bytes.bytes)?)
                        };
                        tokio::pin!(frame);
                        let message = loop {
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
                            Message::Status { status, queue, reset } => {
                                if reset { teller.reset(); }
                                if self.pool.upgrade().is_some_and(|pool| pool.is_live(self.agent)) {
                                    if let Some(queue) = queue
                                        && let Some(live) = teller.tell_queue(&queue)
                                    {
                                        crate::mirror::tell_live(&self.db, self.agent, live);
                                    }
                                    for live in teller.tell(&status.kind) {
                                        crate::mirror::tell_live(&self.db, self.agent, live);
                                    }
                                } else {
                                    teller.reset();
                                }
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
                            outgoing.send(Message::Reply { id, body: reply }).await
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
                            message = commands.recv() => message,
                        };
                        let Some(message) = message else { return Ok(()); };
                        writer.send(port, ipc::encode(&message)?).await?;
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
        outgoing: &mpsc::Sender<Message<'static>>,
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
                    super::ipc::SharedCall::Papercut(args) => crate::papercut::PapercutTool {
                        db: self.db.clone(),
                        agent_id: self.agent,
                    }
                    .record(args)
                    .await
                    .map(|id| format!("Saved papercut #{id}.")),
                    super::ipc::SharedCall::Agent(call) => {
                        let pool = self.pool.upgrade().context("agent pool is shutting down")?;
                        let head = self.db.read().get_agent(self.agent);
                        anyhow::ensure!(
                            call.allowed(head.config.role),
                            "not an available daemon-owned tool"
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
                    Ok(text) => super::ipc::SharedReply::Ok(text),
                    Err(error) => super::ipc::SharedReply::Err(error.to_string()),
                })
            }
            Request::Usage(usage) => {
                if let Some(pool) = self.pool.upgrade() {
                    pool.record_agent_usage(self.agent, usage).await;
                }
                Reply::Done
            }
            Request::Completed(final_answer) => {
                if let Some(pool) = self.pool.upgrade() {
                    pool.publish_completed_turn(crate::pool::AgentTurnCompleted {
                        agent_id: self.agent,
                        final_answer,
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
                        let _ = outgoing.send(Message::Named(head)).await;
                    })
                    .await;
                Reply::Head(self.db.read().get_agent(agent))
            }
            Request::Head => Reply::Head(self.db.read().get_agent(self.agent)),
            Request::History => {
                let (next, rows) = self.db.read().agent_event_records(self.agent);
                let mut batch = Vec::new();
                let mut bytes = 0;
                for row in rows {
                    let size = senax_encoder::encode(&row)?.len();
                    if bytes + size > 1024 * 1024 && !batch.is_empty() {
                        outgoing
                            .send(Message::HistoryBatch {
                                id,
                                rows: std::mem::take(&mut batch),
                            })
                            .await?;
                        bytes = 0;
                    }
                    bytes += size;
                    batch.push(row);
                }
                if !batch.is_empty() {
                    outgoing
                        .send(Message::HistoryBatch { id, rows: batch })
                        .await?;
                }
                Reply::History {
                    next,
                    rows: Vec::new(),
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
                    if let Some(crate::native::NativeEvent::ResponseFinished {
                        usage: Some(usage),
                        at,
                        ..
                    }) = event.native_event()
                    {
                        let mut usage = usage.clone();
                        usage.bucket_start_ms = at.0 / crate::db::AGENT_USAGE_BUCKET_MS
                            * crate::db::AGENT_USAGE_BUCKET_MS;
                        write.add_agent_usage(self.agent, &usage);
                    }
                }
                write.commit();
                Reply::Done
            }
            Request::AdmittedIds => {
                Reply::AdmittedIds(self.db.read().agent_admitted_ids(self.agent))
            }
            Request::ExecAdmitted(id) => {
                Reply::Admitted(self.db.read().agent_exec_was_admitted(self.agent, &id))
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
    use rho_core::UnixMs;

    use super::*;
    use crate::AgentEvent;
    use crate::db::{AgentRole, AgentRoleSessionProfile as _, AgentRuntime};

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
            role.session_profile().unwrap(),
            AgentRuntime::Rho {
                prompt_cache_key: rho_inference::PromptCacheKey::generate(),
            },
            None,
        );
        for _ in 0..3 {
            write.append_agent_event(
                agent,
                &AgentEvent::Notice {
                    text: "x".repeat(700_000).into(),
                    at: UnixMs(2),
                },
            );
        }
        write.commit();
        let inference = Inference::new_with_config(
            db.clone(),
            rho_inference::InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        )
        .await
        .unwrap();
        let services = Arc::new(Services::new(
            db.clone(),
            inference,
            agent,
            Default::default(),
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
        ));
        let (client, server) = crate::worker::testing::pair();
        let server = tokio::spawn(services.clone().serve(
            server.sender,
            server.port,
            server.incoming,
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

        // The daemon committed, but the worker never received its reply.
        // Neither transport nor store client is allowed to resend the append.
        let (client, mut server) = crate::worker::testing::pair();
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
