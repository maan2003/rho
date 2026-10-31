//! Host-owned transactions for one agent. Requests are bound to it: a
//! worker cannot name another agent, write another workset's log, or write
//! a row that is not about its agent.
use std::sync::Arc;

use anyhow::Context as _;
use rho_agent_types::AgentId;
use rho_db::RhoDb;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use crate::record::{ClaudeRow, Event, Record};
use crate::store::{StoreRead as _, StoreWrite as _};
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
    /// Whether the agent's workset process is stale, told in its status.
    pub stale: bool,
    pool: std::sync::Weak<crate::host::pool::AgentPool>,
}

impl Services {
    pub(crate) fn new(
        db: RhoDb,
        agent: AgentId,
        pool: std::sync::Weak<crate::host::pool::AgentPool>,
        next_control: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (ready, _) = tokio::sync::watch::channel(false);
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
            pool,
            status,
            stale: false,
        }
    }

    /// The worker went away without saying why: the agent's row says it
    /// stopped, and its status that it failed.
    pub(crate) async fn worker_failed(&self, error: String) {
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
        let workset = self.db.read().agent(self.agent).map(|head| head.workset);
        if let Some(workset) = workset {
            let mut write = self.db.write().await;
            let appended = write.append(
                workset,
                &Record {
                    at: rho_agent_types::UnixMs::now(),
                    event: Event::Failed {
                        agent: self.agent,
                        partial: Default::default(),
                        error: error.into(),
                        retrying: false,
                    },
                },
            );
            if appended.is_ok() {
                write.commit();
            }
        }
        self.status.send_replace(status);
        if let Some(pool) = self.pool.upgrade() {
            pool.flush_agent_usage(Some(self.agent)).await;
            pool.status_changed(self.agent, &self.status);
        }
    }

    /// The agent's head changed outside its loop: tell the loop.
    pub(crate) fn named(&self, head: crate::store::AgentHead) {
        let _ = self.commands.send(Message::Named(head));
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
                            // The worker sends a status only once the rows it
                            // reflects are committed, so a client that sends
                            // rows before statuses never shows one ahead of them.
                            Message::Status { mut status } => {
                                status.runtime.stale = self.stale;
                                self.status.send_replace(status);
                                if let Some(pool) = self.pool.upgrade() {
                                    pool.status_changed(self.agent, &self.status);
                                }
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
            result
        })
    }

    fn head(&self) -> anyhow::Result<crate::store::AgentHead> {
        self.db
            .read()
            .agent(self.agent)
            .context("agent was deleted or does not exist")
    }

    /// Send `rows` ahead of the reply in frames of about a megabyte.
    async fn batches(
        id: u64,
        rows: Vec<(rho_agent_types::Wpos, Record<'static>)>,
        outgoing: &mpsc::Sender<(Message<'static>, Option<Arc<()>>)>,
    ) -> anyhow::Result<()> {
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
        Ok(())
    }

    /// Append the agent's own rows, with the usage they bill, in one
    /// transaction.
    async fn append(&self, records: &[Record<'_>]) -> anyhow::Result<rho_agent_types::Wpos> {
        let workset = self.head()?.workset;
        for record in records {
            anyhow::ensure!(
                record.event.agent() == Some(self.agent)
                    && !matches!(record.event, Event::AgentCreated { .. }),
                "a worker writes only its own agent's rows"
            );
        }
        let mut write = self.db.write().await;
        let mut last = None;
        for record in records {
            last = Some(write.append(workset, record)?);
            if let Event::Native {
                entry:
                    crate::entry::Entry::Step {
                        usage: Some(usage), ..
                    },
                ..
            } = &record.event
            {
                write.add_agent_usage(
                    self.agent,
                    &crate::log::AgentUsageBucket {
                        bucket_start_ms: record.at.0 / crate::log::AGENT_USAGE_BUCKET_MS
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
        write.commit();
        last.context("no rows to append")
    }

    async fn call(
        &self,
        id: u64,
        request: Request<'static>,
        outgoing: &mpsc::Sender<(Message<'static>, Option<Arc<()>>)>,
    ) -> anyhow::Result<Reply> {
        let reply = match request {
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
            Request::Head => Reply::Head(self.head()?),
            Request::History => {
                let rows = {
                    let read = self.db.read();
                    let head = read.agent(self.agent).context("agent was deleted")?;
                    read.agent_rows(self.agent, head.created)
                };
                Self::batches(id, rows, outgoing).await?;
                Reply::History { rows: Vec::new() }
            }
            Request::NativeHistory(boundary) => {
                let (boundary, cursor, rows) = {
                    let read = self.db.read();
                    let head = read.agent(self.agent).context("agent was deleted")?;
                    let boundary = boundary.unwrap_or(crate::ipc::protocol::ContextBoundary {
                        from: head.native.from,
                        through: head.last,
                    });
                    let rows = read
                        .agent_rows(self.agent, boundary.from)
                        .into_iter()
                        .filter(|(pos, _)| *pos <= boundary.through)
                        .collect::<Vec<_>>();
                    (boundary, head.native, rows)
                };
                Self::batches(id, rows, outgoing).await?;
                Reply::NativeHistory {
                    boundary,
                    cursor,
                    rows: Vec::new(),
                }
            }
            Request::Append(record) => {
                Reply::Position(self.append(std::slice::from_ref(&record)).await?)
            }
            Request::AppendBatch(records) => {
                self.append(&records).await?;
                let head = self.head()?;
                Reply::Boundary(crate::ipc::protocol::ContextBoundary {
                    from: head.native.from,
                    through: head.last,
                })
            }
            Request::ClaudePendingOutput => Reply::ClaudePendingOutput(self.pending_output()?),
            Request::ClaudeAccount => Reply::ClaudeAccount(self.db.read().claude_account()),
        };
        Ok(reply)
    }

    /// The newest Claude output batch, unless it was handed off. Only the
    /// newest can still be pending; handoffs of older batches do not retire
    /// it, including across a rewind.
    fn pending_output(&self) -> anyhow::Result<Option<crate::ClaudeOutputBatch>> {
        let read = self.db.read();
        let head = read.agent(self.agent).context("agent was deleted")?;
        let rows = read.workset_rows(head.workset, head.created, usize::MAX);
        let mut handed_off = std::collections::BTreeSet::new();
        for (_, record) in rows.into_iter().rev() {
            match record.event {
                Event::Claude {
                    agent,
                    row: ClaudeRow::OutputHandedOff { id },
                } if agent == self.agent => {
                    handed_off.insert(id);
                }
                Event::Claude {
                    agent,
                    row: ClaudeRow::Output { batch },
                } if agent == self.agent => {
                    return Ok((!handed_off.contains(&batch.id)).then_some(batch));
                }
                _ => {}
            }
        }
        Ok(None)
    }
}
