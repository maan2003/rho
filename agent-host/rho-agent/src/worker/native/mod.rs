//! The Rho runtime: one agent's log, its notebook, and the loop that wakes
//! the model.
//!
//! The loop does three things, over and over: record what arrives (messages
//! from outside, and what the notebook sends out), ask [`wake::decide`]
//! whether the model should look, and if so wake it with a report and run
//! the cell it answers with. The model answers every wake with one `exec`
//! call and speaks to the person only through `human.send`.

pub(crate) mod context;
pub(crate) mod notebook;
mod persistence;
pub mod process;

#[cfg(test)]
mod scripted;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use notebook::{CellSide, NotebookSide};
use rho_agent_types::{
    AgentId, AgentRole, ContentPart, EngineerIntelligence, TurnEdge, TurnOutcome, UnixMs,
};
use rho_agents_client::protocol::transcript::{ArgumentsFormat, Item};
use rho_notebook::Notebook;
use rho_notebook::process::{Event as NotebookEvent, ServiceKind, ServiceRequest};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::entry::{
    Block, Entry, MessageId, Notice, Party, Report, RequestNotice, ResponseUsage, Wake,
};
use crate::inference::config::{InferenceModel, InferenceProfile};
use crate::inference::{
    CacheKey, Call, Carry, Event, Image, Inference, InferenceSession, Request, Response, Step,
};
use crate::log::{AgentHead, AgentRoleSessionProfile as _, AgentRuntime};
use crate::worker::host_client::{HostClient, StoreError};
use crate::worker::shared::mailroom::{Mailroom, Outbound};
use crate::worker::shared::wake::{Decision, Facts};
use crate::worker::shared::{Progress, tools, wake};
use crate::{AgentEvent, AgentStatus, InferenceState, RuntimeState, StreamingResponse, prompt};

/// Retries are volatile: restarting never resumes work without fresh input.
struct Backoff {
    since: tokio::time::Instant,
    previous: u64,
    delay: u64,
    at: UnixMs,
    error: String,
}

impl Backoff {
    const WINDOW: Duration = Duration::from_secs(8 * 60 * 60);

    fn failed(previous: Option<Self>, error: String) -> Self {
        let (since, previous, delay) =
            previous.map_or((tokio::time::Instant::now(), 0, 1), |last| {
                (
                    last.since,
                    last.delay,
                    (last.previous + last.delay).min(30 * 60),
                )
            });
        let remaining = Self::WINDOW.saturating_sub(since.elapsed());
        Self {
            since,
            previous,
            delay,
            at: UnixMs::now() + Duration::from_secs(delay).min(remaining),
            error,
        }
    }
}
/// Responses Lite's automatic provider-compaction threshold for the GPT-6
/// models.
const AUTO_COMPACT_TOKENS: u64 = 232_560;

/// Cheap clonable handle for observing and driving a running agent.
#[derive(Clone)]
pub struct AgentHandle {
    control: mpsc::UnboundedSender<Control>,
    status: Arc<RwLock<AgentStatus>>,
    /// The record as the loop keeps it: read here instead of folding the
    /// log again for every mail, tool call, or shell.
    head: Arc<RwLock<AgentHead>>,
}

impl AgentHandle {
    pub fn status(&self) -> AgentStatus {
        self.status.read().expect("poison").clone()
    }

    /// The record as of the loop's last change to it.
    pub fn head(&self) -> AgentHead {
        self.head.read().expect("poison").clone()
    }

    /// A user message carried the pending notice: it is not said again.
    /// The log agrees once the message's row is in it.
    pub fn notice_carried(&self) {
        self.head.write().expect("poison").pending_notice = None;
    }

    /// Say the whole tail again: a client just started looking.
    pub fn tell_tail(&self) {
        let _ = self.control.send(Control::TellTail);
    }

    pub fn send_user_message(&self, text: impl Into<String>) {
        self.send_user_content(vec![ContentPart::Text { text: text.into() }]);
    }

    pub fn send_user_content(&self, content: Vec<ContentPart>) {
        let _ = self.control.send(Control::Received {
            id: MessageId::new(),
            from: Party::Human,
            content,
            done: None,
        });
    }

    /// Send user input and wait until the loop has durably logged it. An
    /// `id` already logged is acknowledged without a second row.
    pub async fn send_user_content_accepted(
        &self,
        id: MessageId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<()> {
        self.send(|done| Control::Received {
            id,
            from: Party::Human,
            content,
            done: Some(done),
        })
        .await
    }

    /// Deliver mail from a peer agent.
    pub fn send_agent_message(&self, sender: AgentId, text: impl Into<String>) {
        let _ = self.control.send(Control::Received {
            id: MessageId::new(),
            from: Party::Agent(sender),
            content: vec![ContentPart::Text { text: text.into() }],
            done: None,
        });
    }

    /// Deliver mail and wait until the loop has durably logged it.
    pub async fn send_agent_message_accepted(
        &self,
        sender: AgentId,
        text: impl Into<String>,
    ) -> anyhow::Result<()> {
        let content = vec![ContentPart::Text { text: text.into() }];
        self.send(|done| Control::Received {
            id: MessageId::new(),
            from: Party::Agent(sender),
            content,
            done: Some(done),
        })
        .await
    }

    /// Ask the provider to compact the context on the next request.
    pub fn compact(&self) {
        let _ = self.control.send(Control::Compact);
    }

    /// Cut off the response in flight and cancel the notebook's running
    /// work; the agent stays quiet until someone writes.
    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }

    /// Explicitly resume after failure, cancellation, or restart.
    pub fn retry(&self) {
        let _ = self.control.send(Control::Retry);
    }

    pub async fn change_role(&self, role: AgentRole) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(Control::ChangeRole { role, reply })
            .map_err(|_| anyhow::anyhow!("agent loop has stopped"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("agent loop has stopped"))?
    }

    /// Waits for the response in flight, if any, to end, then freezes the
    /// loop with its log flushed. Work still running is left to die with
    /// the process.
    pub(crate) async fn drain(&self) -> anyhow::Result<()> {
        let (reply, drained) = oneshot::channel();
        self.control
            .send(Control::Drain(reply))
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))?;
        drained
            .await
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))?
    }

    pub(crate) async fn retire(&self) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(Control::Retire(reply))
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))?
    }

    pub fn change_prompt_cache_key(&self) {
        let _ = self.control.send(Control::ChangePromptCacheKey(
            crate::inference::PromptCacheKey::generate(),
        ));
    }

    /// Branch before the `turns`-th last human message; the notebook stays.
    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(Control::Rewind { turns, reply })
            .map_err(|_| anyhow::anyhow!("agent loop has stopped"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("agent loop has stopped"))?
    }

    /// Hand a command to the loop and wait for it to land. An error means
    /// the agent stopped before it got there.
    async fn send(
        &self,
        command: impl FnOnce(oneshot::Sender<()>) -> Control,
    ) -> anyhow::Result<()> {
        let (done, landed) = oneshot::channel();
        self.control
            .send(command(done))
            .map_err(|_| anyhow::anyhow!("agent loop has stopped"))?;
        landed
            .await
            .map_err(|_| anyhow::anyhow!("agent loop stopped before accepting the input"))
    }
}

/// A command, and for the ones a caller may wait on, the ack that says it
/// landed: after its row is on disk, not after the model has seen it.
enum Control {
    Retire(oneshot::Sender<anyhow::Result<()>>),
    Drain(oneshot::Sender<anyhow::Result<()>>),
    Received {
        id: MessageId,
        from: Party,
        content: Vec<ContentPart>,
        done: Option<oneshot::Sender<()>>,
    },
    Compact,
    Cancel,
    Retry,
    ChangeRole {
        role: AgentRole,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    ChangePromptCacheKey(crate::inference::PromptCacheKey),
    Rewind {
        turns: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Tell the live tail whole, for a client that just started looking.
    TellTail,
}

/// The latest cell, the call that wrote it, and when it started.
struct Latest {
    cell: CellSide,
    call: Call,
    published: bool,
}

/// The call of the step in progress, as its code arrives.
struct Streaming {
    carry: Carry,
    code: String,
}

/// Why the loop stopped waking the model, when it has.
#[derive(Clone)]
enum Stopped {
    /// Three steps in a row without a call, or a cancel: only the human
    /// wakes it.
    Quiet,
    /// Model requests kept failing: the human or a retry wakes it.
    Failed(Arc<str>),
}

/// Owned request boundary. Concurrent controls update the live context, never
/// here.
struct PreparedTurn {
    instructions: Arc<str>,
    input: Vec<crate::inference::Item>,
    previous: Option<crate::inference::Continuation>,
    boundary: oneshot::Receiver<crate::log::ContextBoundary>,
    cache_key: CacheKey,
}
impl PreparedTurn {
    async fn request(&mut self, host: &HostClient) -> anyhow::Result<Request> {
        if let Some(previous) = self.previous.take() {
            return Ok(Request::continuation(
                self.instructions.clone(),
                std::mem::take(&mut self.input),
                self.cache_key,
                previous,
            ));
        }
        let boundary = (&mut self.boundary)
            .await
            .map_err(|_| anyhow::anyhow!("native event replication stopped"))?;
        let (_, _, entries) = host.native_history(Some(boundary)).await?;
        Ok(context::request(
            self.instructions.clone(),
            &entries,
            self.cache_key,
        ))
    }
}

pub(crate) struct Agent {
    agent_id: AgentId,
    host: Arc<HostClient>,
    writer: persistence::Writer,
    inference: Inference,
    session: InferenceSession,
    model_name: String,
    cwd: camino::Utf8PathBuf,
    context: context::Context,
    continuation: Option<crate::inference::Continuation>,
    notebook: Option<NotebookSide>,
    checkpoint_dir: Option<PathBuf>,
    notebook_process: Option<process::Process>,
    notebook_events_tx: mpsc::UnboundedSender<(u64, NotebookEvent)>,
    notebook_events: mpsc::UnboundedReceiver<(u64, NotebookEvent)>,
    checkpointed: bool,
    restored: bool,
    mailroom: Arc<Mailroom>,
    outbox: mpsc::UnboundedReceiver<Outbound>,
    control_rx: mpsc::UnboundedReceiver<Control>,
    wake: Arc<Notify>,
    status: Arc<RwLock<AgentStatus>>,
    head: Arc<RwLock<AgentHead>>,
    name_updates: tokio::sync::watch::Receiver<Option<AgentHead>>,
    draining: Option<oneshot::Sender<anyhow::Result<()>>>,
    /// Whether the last published state counted as a running turn, so the
    /// turn's edges are told once each.
    working: bool,

    archived: bool,
    /// The next wake says the notebook is new.
    fresh: bool,
    responding: bool,
    writing: Option<Call>,
    response_id: String,
    /// The latest cell and its call. Older ones live on in the notebook's
    /// sources.
    cell: Option<Latest>,
    /// The latest step was cut off part-way through its cell.
    interrupted: bool,
    /// Messages the model has not seen, oldest first.
    unread: Vec<(MessageId, Party, UnixMs)>,
    /// Every message id the loaded log holds, so a message sent again
    /// after its answer was lost is not logged twice.
    received: HashSet<MessageId>,
    progress: Progress,
    awaiting: bool,
    /// The worker restarted since the last model wake; tell it whether
    /// Python was restored at the next request. Loading is not a wake.
    restarted: bool,
    rewound: bool,
    retry: bool,
    backoff: Option<Backoff>,
    stopped: Option<Stopped>,
    cache_key: CacheKey,
    compaction: crate::entry::CompactionState,
}

impl Agent {
    /// Construct a worker-owned runtime without a notebook subprocess in tests.
    #[cfg(test)]
    pub(crate) async fn load(
        agent_id: AgentId,
        host: Arc<HostClient>,
        inference: Inference,
        cwd: camino::Utf8PathBuf,
    ) -> anyhow::Result<(AgentHandle, Self)> {
        Self::load_with_checkpoint(agent_id, host, inference, cwd, None).await
    }

    pub(crate) async fn load_with_checkpoint(
        agent_id: AgentId,
        host: Arc<HostClient>,
        inference: Inference,
        cwd: camino::Utf8PathBuf,
        checkpoint_dir: Option<PathBuf>,
    ) -> anyhow::Result<(AgentHandle, Self)> {
        let head = host.head().await?;
        let AgentRuntime::Rho { prompt_cache_key } = head.config.runtime else {
            anyhow::bail!("agent does not use the Rho runtime");
        };
        let (session, model_name) = inference_session(&inference, head.config.binding)?;
        let (boundary, recovery, entries) = host.native_history(None).await?;
        let context = context::Context::restore(&entries);
        let (mailroom, outbox) = Mailroom::new();
        let (notebook_events_tx, notebook_events) = mpsc::unbounded_channel();
        let status = Arc::new(RwLock::new(AgentStatus::default()));
        let head = Arc::new(RwLock::new(head));
        let (control, control_rx) = mpsc::unbounded_channel();
        host.observe(&status);
        let mut agent = Self {
            agent_id,
            writer: persistence::Writer::new(host.clone(), boundary),
            name_updates: host.names(),
            host,
            inference,
            session,
            model_name,
            cwd,
            context,
            continuation: None,
            notebook: None,
            checkpoint_dir,
            notebook_process: None,
            notebook_events_tx,
            notebook_events,
            checkpointed: false,
            restored: false,
            mailroom,
            outbox,
            control_rx,
            wake: Arc::new(Notify::new()),
            status: Arc::clone(&status),
            head: Arc::clone(&head),
            draining: None,
            working: false,
            archived: recovery.archived,
            fresh: false,
            responding: false,
            writing: None,
            response_id: String::new(),
            cell: None,
            interrupted: false,
            unread: Vec::new(),
            received: entries
                .iter()
                .filter_map(|entry| match entry {
                    Entry::Received { id, .. } => Some(*id),
                    _ => None,
                })
                .collect(),
            progress: Progress::default(),
            awaiting: false,
            restarted: false,
            rewound: false,
            retry: false,
            backoff: None,
            stopped: None,
            cache_key: cache_key(prompt_cache_key),
            compaction: recovery.compaction,
        };
        agent.resume(&entries, recovery.woken);
        if agent
            .checkpoint_dir
            .as_ref()
            .is_some_and(|dir| dir.join("current").exists())
        {
            agent.start_process().await?;
        }
        agent.publish().await?;
        Ok((
            AgentHandle {
                control,
                status,
                head,
            },
            agent,
        ))
    }

    /// Start the process on first notebook use, or attach a saved one at load.
    async fn start_process(&mut self) -> anyhow::Result<()> {
        if let Some(dir) = self.checkpoint_dir.clone() {
            let team = self.host.team().await?;
            let role = self.head.read().expect("poison").config.role;
            let (services_tx, mut services) = mpsc::unbounded_channel::<ServiceRequest>();
            let (child, restored) = tokio::task::block_in_place(|| {
                process::Process::start(
                    &dir,
                    &self.cwd,
                    role,
                    self.agent_id,
                    team.is_some(),
                    Arc::clone(&self.wake),
                    self.notebook_events_tx.clone(),
                    services_tx,
                )
            })?;
            let client = Arc::clone(&child.client);
            let service_client = Arc::clone(&client);
            let host = Arc::clone(&self.host);
            let inference = self.inference.clone();
            tokio::spawn(async move {
                while let Some(request) = services.recv().await {
                    let reply = match request.kind {
                        ServiceKind::HostCall => {
                            let mut bytes = request.payload.as_slice();
                            match senax_encoder::decode::<crate::ipc::protocol::SharedCall>(
                                &mut bytes,
                            ) {
                                Ok(call) if bytes.is_empty() => {
                                    host.shared_tool(call).await.and_then(|text| {
                                        senax_encoder::encode(&text)
                                            .map(|bytes| bytes.to_vec())
                                            .map_err(|e| e.to_string())
                                    })
                                }
                                _ => Err("invalid notebook host call".into()),
                            }
                        }
                        ServiceKind::WebCredentials => inference
                            .web_credentials()
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|value| {
                                senax_encoder::encode(&process::CredentialsWire {
                                    bearer_token: value.bearer_token,
                                    account_id: value.account_id,
                                })
                                .map(|bytes| bytes.to_vec())
                                .map_err(|e| e.to_string())
                            }),
                    };
                    if service_client.service_reply(request.id, reply).is_err() {
                        break;
                    }
                }
            });
            if restored {
                client.resume().map_err(anyhow::Error::msg)?;
                if let Some((id, _)) = client.latest_cell().map_err(anyhow::Error::msg)? {
                    self.cell = Some(Latest {
                        cell: CellSide::Process {
                            client: Arc::clone(&client),
                            id,
                        },
                        // A restored cell is never an in-flight provider stream.
                        call: Call::new("restored", String::new()),
                        published: true,
                    });
                }
                let (_, _, entries) = self.host.native_history(None).await?;
                self.progress.last_response = entries.iter().rev().find_map(|entry| match entry {
                    Entry::Step { at, .. } => Some(*at),
                    _ => None,
                });
                for entry in &entries {
                    match entry {
                        Entry::AwaitingHuman { .. } => {
                            self.awaiting = true;
                            self.progress.ended = true;
                        }
                        Entry::StoppedAwaitingHuman { .. }
                        | Entry::Received {
                            from: Party::Human, ..
                        } => self.awaiting = false,
                        Entry::RequestSent { .. } => self.progress.ended = false,
                        _ => {}
                    }
                }
            }
            self.restored = restored;
            self.notebook = Some(NotebookSide::Process(client));
            self.notebook_process = Some(child);
        }
        Ok(())
    }

    /// Answer from `script` instead of the role's provider.
    #[cfg(test)]
    fn script(&mut self, script: Arc<scripted::Scripted>) {
        self.session = Arc::new(script);
    }

    /// Pick up from the log: what the model has not seen, and whether there
    /// was a notebook that is now gone.
    fn resume(&mut self, entries: &[Entry], woken: bool) {
        self.restore_unread(entries);
        if woken && !self.archived {
            self.restarted = true;
        }
    }

    fn restore_unread(&mut self, entries: &[Entry]) {
        self.unread.clear();
        for entry in entries {
            match entry {
                Entry::Received { at, id, from, .. } => self.unread.push((*id, *from, *at)),
                Entry::RequestSent { report, .. } => self.unread.retain(|(id, _, _)| {
                    !report.messages.contains(id) && !report.acknowledged.contains(id)
                }),
                _ => {}
            }
        }
    }

    /// Answer the one question after every event, act on the answer, and
    /// wait for the next one, until the last handle is dropped.
    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            self.writer.check()?;
            if self.draining.is_some() && !self.responding {
                self.drain_outbox().await?;
                self.publish().await?;
                self.flush().await?;
                if let Some(process) = &self.notebook_process {
                    if process.client.prepare().is_err() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    let result = tokio::task::block_in_place(|| process.checkpoint());
                    self.checkpointed = result.is_ok();
                    if let Err(error) = result {
                        // The log is durable even when CRIU cannot save the
                        // interpreter. A restart will use a fresh notebook.
                        eprintln!("notebook checkpoint failed; restarting fresh: {error:#}");
                    }
                    let _ = self.draining.take().expect("checked above").send(Ok(()));
                } else {
                    let _ = self.draining.take().expect("checked above").send(Ok(()));
                }
                // Frozen like a retired loop; the driver cancels this future
                // when the agent host lets go.
                std::future::pending::<()>().await;
            }
            // A cell can archive itself as it completes. Apply what it sent
            // before deciding whether its completion warrants a wake.
            self.drain_outbox().await?;
            if let Some(backoff) = &self.backoff
                && backoff.since.elapsed() >= Backoff::WINDOW
            {
                self.fail(format!(
                    "Provider retry window exhausted: {}",
                    backoff.error
                ))
                .await?;
            }
            let decision = if self.draining.is_some() {
                Decision::Later(None)
            } else if let Some(backoff) = &self.backoff {
                if backoff.at <= UnixMs::now() {
                    Decision::Now(Wake::Prose)
                } else {
                    Decision::Later(Some(backoff.at))
                }
            } else {
                wake::decide(&self.facts()?, UnixMs::now())
            };
            let recheck = match decision {
                Decision::Now(why) => {
                    self.wake_model(why).await?;
                    continue;
                }
                Decision::Later(recheck) => recheck,
            };
            self.publish().await?;
            let sleep = async {
                match recheck {
                    Some(at) => tokio::time::sleep(until(at)).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                error = self.writer.failed() => return Err(error.into()),
                named = self.name_updates.changed() => {
                    named.map_err(|_| anyhow::anyhow!("agent services disconnected"))?;
                    self.named();
                }
                control = self.control_rx.recv() => match control {
                    Some(control) => self.control(control).await?,
                    None => return Ok(()),
                },
                Some(outbound) = self.outbox.recv() => self.outbound(outbound).await?,
                Some((id, event)) = self.notebook_events.recv() => self.notebook_event(id, event).await?,
                () = self.wake.notified() => {}
                () = sleep => {}
            }
        }
    }

    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        if !self.checkpointed {
            if let Some(process) = self.notebook_process.take() {
                tokio::task::block_in_place(|| process.shutdown())?;
            } else if let Some(notebook) = self.notebook.take() {
                notebook.cancel()?;
                notebook.shutdown().await?;
            }
        }
        self.flush().await?;
        Ok(())
    }

    fn named(&mut self) {
        let stored = self.name_updates.borrow_and_update().clone();
        if let Some(stored) = stored {
            let mut head = self.head.write().expect("poison");
            head.generated_title = stored.generated_title;
            head.title_attempted = stored.title_attempted;
        }
    }

    async fn control(&mut self, control: Control) -> anyhow::Result<()> {
        match control {
            Control::Retire(reply) => {
                if self.settled()? {
                    self.drain_outbox().await?;
                    self.publish().await?;
                    self.flush().await?;
                    if let Some(process) = &self.notebook_process {
                        if let Err(error) = process.client.prepare() {
                            let _ = reply.send(Err(anyhow::Error::msg(error)));
                            return Ok(());
                        }
                        if let Err(error) = tokio::task::block_in_place(|| process.checkpoint()) {
                            let message = format!("notebook checkpoint failed: {error:#}");
                            let _ = reply.send(Err(anyhow::anyhow!(message.clone())));
                            // A failed CRIU dump can leave its target stopped.
                            // Stop this runtime rather than retaining a prepared,
                            // potentially unusable interpreter after failed eviction.
                            anyhow::bail!(message);
                        }
                        self.checkpointed = true;
                    }
                    let _ = reply.send(Ok(()));
                    // The driver cancels this future when the host disconnects.
                    std::future::pending::<()>().await;
                } else {
                    let _ = reply.send(Err(anyhow::anyhow!("agent still has work")));
                }
            }
            Control::Drain(reply) => self.draining = Some(reply),
            Control::TellTail => self.host.tell_tail(),
            Control::Received {
                id,
                from,
                content,
                done,
            } => {
                if !self.received.insert(id) {
                    if let Some(done) = done {
                        let _ = done.send(());
                    }
                    return Ok(());
                }
                let text = rho_agent_types::transcript::text_content(&content);
                self.receive(id, from, blocks(content)).await?;
                if !text.trim().is_empty() {
                    self.name(&text).await?;
                }
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            Control::Compact => self.compact().await?,
            Control::Cancel => self.interrupt(None).await?,
            Control::Retry => {
                if !self.responding
                    && !self.archived
                    && (self.restarted || self.stopped.is_some() || self.backoff.is_some())
                {
                    self.stopped = None;
                    self.backoff = None;
                    self.retry = true;
                }
            }
            Control::ChangeRole { role, reply } => {
                let result = self.change_role(role).await;
                if result.as_ref().is_err_and(|error| error.is::<StoreError>()) {
                    return result;
                }
                let _ = reply.send(result);
            }
            Control::ChangePromptCacheKey(key) => {
                self.host.cache_key(key).await?;
                self.cache_key = cache_key(key);
            }
            Control::Rewind { turns, reply } => {
                let result = self.rewind(turns).await;
                if result.as_ref().is_err_and(|error| error.is::<StoreError>()) {
                    return result;
                }
                let _ = reply.send(result);
            }
        }
        Ok(())
    }

    /// Nothing in motion, nothing waiting to be seen.
    fn settled(&self) -> anyhow::Result<bool> {
        Ok(!self.responding
            && self.backoff.is_none()
            && self.unread.is_empty()
            && self
                .notebook
                .as_ref()
                .map(NotebookSide::facts)
                .transpose()?
                .is_none_or(|sources| sources.iter().all(|source| source.finished.is_some()))
            && (self.archived || self.stopped.is_some() || self.progress.last_response.is_none()))
    }

    fn cell_running(&self) -> anyhow::Result<bool> {
        Ok(self
            .cell
            .as_ref()
            .map(|latest| latest.cell.facts())
            .transpose()?
            .is_some_and(|facts| facts.finished.is_none()))
    }

    async fn name(&mut self, input: &str) -> anyhow::Result<()> {
        self.flush().await?;
        let stored = self.host.name(input).await?;
        let mut head = self.head.write().expect("poison");
        head.generated_title = stored.generated_title;
        head.title_attempted = stored.title_attempted;
        Ok(())
    }

    async fn change_role(&mut self, requested: AgentRole) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.responding && !self.cell_running()?,
            "a role change is only available while idle; cancel the turn first"
        );
        let requested = match requested {
            AgentRole::Engineer { intelligence } => intelligence,
            _ => anyhow::bail!("role changes currently support only engineer roles"),
        };
        let switchable = |intelligence| {
            matches!(
                intelligence,
                EngineerIntelligence::Mini
                    | EngineerIntelligence::Medium
                    | EngineerIntelligence::High
            )
        };
        anyhow::ensure!(
            switchable(requested),
            "this agent can switch only between mini-eng, med-eng, and high-eng"
        );
        let current = self.head.read().expect("poison").config.role;
        let role = match current {
            AgentRole::Engineer { intelligence } if switchable(intelligence) => {
                AgentRole::Engineer {
                    intelligence: requested,
                }
            }
            _ => {
                anyhow::bail!("this agent can switch only between mini-eng, med-eng, and high-eng")
            }
        };
        if role == current {
            return Ok(());
        }
        let binding = role.session_profile();
        let (session, model_name) = inference_session(&self.inference, binding)?;
        self.flush().await?;
        self.host.profile(role, binding).await?;
        {
            let mut head = self.head.write().expect("poison");
            head.config.role = role;
            head.config.binding = binding;
        }
        self.session = session;
        self.model_name = model_name;
        // Instructions are rendered from the current role on each wake. The
        // notebook, and its Python globals, stay alive.
        Ok(())
    }

    /// Branch history before the `turns`-th last human message. What the
    /// agent walked away from stays in the log
    /// (`DECISION-history-only-branches`); the notebook, its globals and
    /// anything it did are not rewound.
    async fn rewind(&mut self, turns: u32) -> anyhow::Result<()> {
        anyhow::ensure!(turns > 0, ":rewind turns must be greater than zero");
        anyhow::ensure!(
            !self.responding,
            ":rewind is not available while the model is responding"
        );
        self.drain_outbox().await?;
        self.flush().await?;
        let (_, rows) = self.host.history().await?;
        let humans = rows
            .iter()
            .filter(|(_, event)| {
                matches!(
                    event,
                    AgentEvent::Entry(Entry::Received {
                        from: Party::Human,
                        ..
                    })
                )
            })
            .map(|(pos, _)| *pos)
            .collect::<Vec<_>>();
        anyhow::ensure!(!humans.is_empty(), "nothing to rewind");
        let to = humans[humans.len().saturating_sub(turns as usize)];
        self.host.rewind(rho_agent_types::UnixMs::now(), to).await?;
        let (_, recovery, entries) = self.host.native_history(None).await?;
        self.context = context::Context::restore(&entries);
        self.continuation = None;
        self.restore_unread(&entries);
        self.progress.last_response = None;
        self.cell = None;
        self.progress.told_returned = false;
        self.interrupted = false;
        self.awaiting = false;
        self.progress.prose = 0;
        self.stopped = None;
        self.restarted = false;
        self.rewound = true;
        self.compaction = recovery.compaction;
        Ok(())
    }

    async fn compact(&mut self) -> anyhow::Result<()> {
        if !self.archived && !self.compaction.pending {
            self.append(Entry::CompactionTrigger {
                at: UnixMs::now(),
                manual: true,
            })
            .await?;
        }
        Ok(())
    }

    async fn append(&mut self, entry: Entry) -> anyhow::Result<()> {
        self.writer
            .append(vec![AgentEvent::Entry(entry.clone())])
            .await?;
        self.context.observe(&entry);
        if matches!(entry, Entry::Step { .. }) {
            self.continuation = None;
        }
        self.compaction.observe(&entry);
        Ok(())
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        Ok(self.writer.flush().await?)
    }

    async fn receive(
        &mut self,
        id: MessageId,
        from: Party,
        body: Vec<Block>,
    ) -> anyhow::Result<()> {
        let at = UnixMs::now();
        if from == Party::Human {
            if self.archived && !self.responding {
                self.fresh_notebook(at).await?;
            }
            // The human's message ends a wait it answers.
            self.awaiting = false;
            self.stopped = None;
        }
        if let Some(backoff) = &mut self.backoff {
            backoff.at = at;
        }
        self.unread.push((id, from, at));
        self.append(Entry::Received { at, id, from, body }).await
    }

    async fn fresh_notebook(&mut self, at: UnixMs) -> anyhow::Result<()> {
        if self.notebook_process.is_none() {
            if let Some(notebook) = self.notebook.take() {
                notebook.cancel()?;
                let _ = notebook.shutdown().await;
            }
        }
        self.archived = false;
        self.fresh = true;
        self.cell = None;
        self.progress = Progress::default();
        self.append(Entry::Notice {
            at,
            notice: Notice::FreshNotebook,
        })
        .await
    }

    async fn outbound(&mut self, outbound: Outbound) -> anyhow::Result<()> {
        let at = UnixMs::now();
        match outbound {
            Outbound::Send { cell, text } => {
                if let Some(latest) = &mut self.cell
                    && latest.cell.source_id() == cell
                {
                    latest.published = true;
                }
                let to = self
                    .head
                    .read()
                    .expect("poison")
                    .parent
                    .map_or(Party::Human, Party::Agent);
                self.append(Entry::Sent {
                    at,
                    id: MessageId::new(),
                    to,
                    text: text.clone(),
                })
                .await?;
                // Whoever is subscribed to this agent's answers gets it as
                // mail, and the sidecar reads what it asks of the person.
                self.flush().await?;
                self.host.message_sent(text).await?;
                Ok(())
            }
            Outbound::Status(text) => self.append(Entry::Status { at, text }).await,
            Outbound::Archive => {
                self.archived = true;
                self.backoff = None;
                if let Some(notebook) = self.notebook.take() {
                    notebook.cancel()?;
                    notebook.shutdown().await?;
                }
                self.cell = None;
                if self.awaiting {
                    self.awaiting = false;
                    self.append(Entry::StoppedAwaitingHuman { at }).await?;
                }
                self.append(Entry::Notice {
                    at,
                    notice: Notice::Archived,
                })
                .await
            }
            Outbound::EndTurn => {
                self.progress.ended = true;
                if self.awaiting {
                    return Ok(());
                }
                self.awaiting = true;
                self.append(Entry::AwaitingHuman { at }).await
            }
        }
    }

    async fn notebook_event(&mut self, id: u64, event: NotebookEvent) -> anyhow::Result<()> {
        let archive = matches!(event, NotebookEvent::Archive);
        if archive {
            let at = UnixMs::now();
            self.archived = true;
            self.backoff = None;
            if self.awaiting {
                self.awaiting = false;
                self.append(Entry::StoppedAwaitingHuman { at }).await?;
            }
            self.append(Entry::Notice {
                at,
                notice: Notice::Archived,
            })
            .await?;
        } else {
            let outbound = match event {
                NotebookEvent::Send { cell, text } => Outbound::Send { cell, text },
                NotebookEvent::Status(text) => Outbound::Status(text),
                NotebookEvent::EndTurn => Outbound::EndTurn,
                NotebookEvent::Archive => unreachable!(),
            };
            self.outbound(outbound).await?;
        }
        self.flush().await?;
        if let Some(process) = &self.notebook_process {
            process.client.ack(id).map_err(anyhow::Error::msg)?;
        }
        if archive {
            if let Some(process) = &self.notebook_process {
                process.client.fresh().map_err(anyhow::Error::msg)?;
            }
            self.cell = None;
        }
        Ok(())
    }

    async fn drain_outbox(&mut self) -> anyhow::Result<()> {
        while let Ok(outbound) = self.outbox.try_recv() {
            self.outbound(outbound).await?;
        }
        while let Ok((id, event)) = self.notebook_events.try_recv() {
            self.notebook_event(id, event).await?;
        }
        Ok(())
    }

    fn facts(&self) -> anyhow::Result<Facts> {
        let notebook = self.progress.process_facts(
            self.notebook.as_ref(),
            self.cell.as_ref().map(|latest| &latest.cell),
        )?;
        Ok(Facts {
            // The cell that ended the turn returns to nobody.
            finished: notebook.finished.filter(|_| !self.progress.ended),
            human: self
                .unread
                .iter()
                .find(|(_, from, _)| *from == Party::Human)
                .map(|(_, _, at)| *at),
            agent: self
                .unread
                .iter()
                .find(|(_, from, _)| *from != Party::Human)
                .map(|(_, _, at)| *at),
            prose: self.progress.prose > 0 || self.retry,
            rewound: self.rewound,
            compaction: self.compaction.pending,
            compaction_reply: self.compaction.reply,
            archived: self.archived,
            prose_silenced: self.stopped.is_some(),
            ..notebook
        })
    }

    /// Start the notebook on first use; workset setup has already completed.
    async fn notebook(&mut self) -> anyhow::Result<&NotebookSide> {
        if self.notebook.is_none() && self.checkpoint_dir.is_some() {
            self.start_process().await?;
        }
        if self.notebook.is_none() {
            let team = self.host.team().await?;
            let role = self.head.read().expect("poison").config.role;
            let (shell, exports) = tools::host_tools(
                &self.cwd,
                role,
                self.agent_id,
                Some(&self.inference),
                team.as_ref(),
                Some(&self.host),
                Some(&self.mailroom),
                true,
            );
            let notebook = Notebook::new(shell, exports, Arc::clone(&self.wake))
                .map_err(|error| anyhow::anyhow!("the notebook failed to start: {error}"))?;
            self.notebook = Some(NotebookSide::Local(notebook));
        }
        Ok(self.notebook.as_ref().expect("started above"))
    }

    async fn instructions(&mut self) -> anyhow::Result<Arc<str>> {
        let team = self.host.team().await?;
        let role = self.head.read().expect("poison").config.role;
        Ok(prompt::prompt(
            &prompt::WorksetPrompt::new(&self.cwd),
            team.as_ref(),
            role,
        ))
    }

    async fn wake_model(&mut self, why: Wake) -> anyhow::Result<()> {
        self.drain_outbox().await?;
        if self.archived {
            return Ok(());
        }
        self.retry = false;
        let prepared = async {
            let instructions = self.instructions().await?;
            self.notebook().await?;
            anyhow::Ok(instructions)
        }
        .await;
        let instructions = match prepared {
            Ok(instructions) => instructions,
            Err(error) => return self.fail(format!("{error:#}")).await,
        };
        let mut report = Report::default();
        if self.restarted {
            self.append(Entry::Notice {
                at: UnixMs::now(),
                notice: Notice::Restarted,
            })
            .await?;
            report.notices.push(if self.restored {
                RequestNotice::Restored
            } else {
                RequestNotice::Restarted
            });
        }
        match why {
            Wake::Rewound => report.notices.push(RequestNotice::Rewound),
            Wake::Prose if self.progress.prose > 0 => report
                .notices
                .push(RequestNotice::PreviousResponseHadNoExec),
            _ => {}
        }
        if std::mem::take(&mut self.fresh) {
            report.notices.push(RequestNotice::FreshNotebook);
        }
        if std::mem::take(&mut self.interrupted) {
            report.notices.push(RequestNotice::InterruptedExecution);
        }
        if let Some(output) = self
            .notebook
            .as_ref()
            .map(NotebookSide::report)
            .transpose()?
            .flatten()
        {
            report.notebook = output;
        }
        self.wake_with(why, instructions, report).await
    }

    async fn wake_with(
        &mut self,
        why: Wake,
        instructions: Arc<str>,
        mut report: Report,
    ) -> anyhow::Result<()> {
        if self.cell.is_some() {
            self.progress.told_returned = true;
        }
        let messages = std::mem::take(&mut self.unread);
        report.messages = messages.into_iter().map(|(id, _, _)| id).collect();
        let manual_only = why == Wake::Compaction && report.is_empty();
        if report.is_empty() && !manual_only {
            report.notices.push(if why == Wake::Checkin {
                RequestNotice::Checkin
            } else {
                RequestNotice::NothingNew
            });
        }
        if self
            .compaction
            .context_used
            .is_some_and(|used| used >= AUTO_COMPACT_TOKENS)
            && !self.compaction.pending
        {
            self.append(Entry::CompactionTrigger {
                at: UnixMs::now(),
                manual: false,
            })
            .await?;
        }
        self.append(Entry::RequestSent {
            at: UnixMs::now(),
            why,
            report,
            compact: self.compaction.pending,
        })
        .await?;
        self.restarted = false;
        self.rewound = false;
        self.progress.ended = false;
        if let Some(notebook) = &self.notebook {
            notebook.reset_checkin()?;
        }
        let turn = self.prepare_turn(instructions).await?;
        self.respond(turn).await
    }

    async fn prepare_turn(&mut self, instructions: Arc<str>) -> anyhow::Result<PreparedTurn> {
        Ok(PreparedTurn {
            instructions,
            input: self.context.input(),
            previous: self.continuation.take(),
            boundary: self.writer.checkpoint().await?,
            cache_key: self.cache_key,
        })
    }

    async fn respond(&mut self, mut turn: PreparedTurn) -> anyhow::Result<()> {
        let previous_backoff = self.backoff.take();
        self.responding = true;
        self.response_id = uuid::Uuid::new_v4().to_string();
        self.writing = None;
        self.publish().await?;
        let step = {
            // The socket actor emits ordered owned events; the agent alone admits Python.
            let mut streaming = None;
            // Full replaceable snapshots copy the accumulated code. Coalesce
            // provider fragments into 50 ms display frames rather than copying
            // on every fragment (the last fragment is published before commit).
            // Each frame still copies/serializes O(current response bytes),
            // including once per focused listener; long streams can remain
            // quadratic in bytes over time, bounded by frame count, not chunks.
            let mut stream_frame = tokio::time::interval(Duration::from_millis(50));
            stream_frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut pending_stream = false;
            let mut stream_needs_runtime = false;
            let session = self.session.clone();
            let result = 'exchange: loop {
                let host = self.host.clone();
                let starting = async {
                    let request = turn.request(&host).await?;
                    anyhow::Ok(session.start(request))
                };
                tokio::pin!(starting);
                let mut response: Option<Response> = None;
                loop {
                    tokio::select! {
                        biased;
                        error = self.writer.failed() => return Err(error.into()),
                        result = &mut starting, if response.is_none() => match result {
                            Ok(events) => {
                                response = Some(events);
                            }
                            Err(error) => break 'exchange Err(error),
                        },
                        event = async { response.as_mut().unwrap().recv().await }, if response.is_some() => {
                            match event {
                                Some(Event::Call { carry }) => {
                                    self.stream(&mut streaming, (Some(carry), String::new()))?;
                                    pending_stream = true;
                                    stream_needs_runtime = true;
                                }
                                Some(Event::Code(code)) => {
                                    self.stream(&mut streaming, (None, code))?;
                                    pending_stream = true;
                                }
                                Some(Event::Completed(step)) => break 'exchange Ok(step),
                                Some(Event::NeedsContext) => {
                                    break; // The consumed continuation becomes a full replay at the same cutoff.
                                }
                                Some(Event::Failed(error)) => break 'exchange Err(error),
                                None => break 'exchange Err(anyhow::anyhow!("model session stopped")),
                            }
                        },
                        _ = stream_frame.tick(), if pending_stream => {
                            self.publish_stream(streaming.as_ref(), stream_needs_runtime)?;
                            pending_stream = false;
                            stream_needs_runtime = false;
                        },
                        control = self.control_rx.recv() => match control {
                            Some(Control::Cancel) => {
                                // Drop the response receiver before touching notebook state.
                                drop(response);
                                self.interrupt(streaming).await?;
                                return Ok(());
                            }
                            Some(control) => self.control(control).await?,
                            None => return Ok(()),
                        },
                        Some(outbound) = self.outbox.recv() => {
                            self.outbound(outbound).await?;
                            self.publish().await?;
                        },
                        Some((id, event)) = self.notebook_events.recv() => {
                            self.notebook_event(id, event).await?;
                            self.publish().await?;
                        },
                        () = self.wake.notified() => self.publish().await?,
                    }
                }
            };
            if pending_stream {
                self.publish_stream(streaming.as_ref(), stream_needs_runtime)?;
            }
            match result {
                Ok(step) => Ok((step, streaming)),
                Err(error) => {
                    let retryable = crate::inference::is_retryable(&error);
                    let error = format!("{error:#}");
                    self.append(Entry::Notice {
                        at: UnixMs::now(),
                        notice: Notice::Error(error.clone()),
                    })
                    .await?;
                    // Code that already ran cannot be taken back: it stands
                    // as the step, and the model hears it was cut off.
                    if let (Some(latest), Some(streaming)) = (&self.cell, streaming)
                        && let Some(ran) = latest.cell.interrupt()?
                    {
                        Err((
                            streaming.carry,
                            streaming.code[..ran.min(streaming.code.len())].to_owned(),
                        ))
                    } else {
                        self.responding = false;
                        self.writing = None;
                        if self.archived {
                            return self.revive_if_written(UnixMs::now()).await;
                        }
                        if retryable {
                            self.backoff = Some(Backoff::failed(previous_backoff, error));
                            return Ok(());
                        }
                        return self.fail(error).await;
                    }
                }
            }
        };
        self.responding = false;
        let at = UnixMs::now();
        self.progress.last_response = Some(at);
        let (step, streaming) = match step {
            Ok(step) => step,
            Err((mut carry, code)) => {
                carry.set_exec(&code);
                self.interrupted = true;
                self.progress.prose = 0;
                self.append(Entry::Step {
                    at,
                    exec: Some(code),
                    prose: String::new(),
                    carry,
                    usage: None,
                })
                .await?;
                return self.revive_if_written(at).await;
            }
        };
        self.finish_turn(step, streaming).await
    }

    async fn finish_turn(
        &mut self,
        step: Step,
        streaming: Option<Streaming>,
    ) -> anyhow::Result<()> {
        let at = UnixMs::now();
        let continuation = step.continuation;
        let compacted = step.carry.has_compaction();
        let usage = step.usage;
        self.append(Entry::Step {
            at,
            exec: step.call.as_ref().map(|call| call.code.clone()),
            prose: step.prose,
            carry: step.carry,
            usage: Some(ResponseUsage::rho(self.model_name.clone(), usage)),
        })
        .await?;
        self.continuation = continuation;
        match (step.call, streaming) {
            (Some(call), Some(streaming)) => {
                self.progress.prose = 0;
                if let Some(latest) = &mut self.cell {
                    // Whatever the stream missed, then the end.
                    let rest = call.code.strip_prefix(&streaming.code).unwrap_or_default();
                    latest.cell.feed(rest.to_owned(), true)?;
                    latest.call = call;
                }
            }
            (Some(call), None) => {
                self.progress.prose = 0;
                let cell = self.notebook().await?.run(call.code.clone())?;
                self.cell = Some(Latest {
                    cell,
                    call,
                    published: false,
                });
                self.progress.told_returned = false;
            }
            (None, streaming) => {
                if compacted {
                    self.progress.prose = 0;
                    return Ok(());
                }
                if streaming.is_some()
                    && let Some(latest) = &self.cell
                {
                    latest.cell.stop()?;
                }
                self.progress.prose += 1;
                if self.progress.prose >= Progress::MAX_PROSE {
                    self.progress.prose = 0;
                    self.stopped = Some(Stopped::Quiet);
                }
            }
        }
        self.revive_if_written(at).await
    }

    /// The human wrote to an archived agent while it was responding.
    async fn revive_if_written(&mut self, at: UnixMs) -> anyhow::Result<()> {
        if self.archived && self.unread.iter().any(|(_, from, _)| *from == Party::Human) {
            self.fresh_notebook(at).await?;
        }
        Ok(())
    }

    /// Model requests kept failing: stop until someone writes or retries.
    async fn fail(&mut self, error: String) -> anyhow::Result<()> {
        self.backoff = None;
        self.stopped = Some(Stopped::Failed(Arc::from(error.as_str())));
        self.flush().await?;
        self.host.failed(error).await?;
        Ok(())
    }

    async fn interrupt(&mut self, streaming: Option<Streaming>) -> anyhow::Result<()> {
        self.backoff = None;
        if let Some(notebook) = &self.notebook {
            notebook.cancel()?;
        }
        self.cell = None;
        self.responding = false;
        self.stopped = Some(Stopped::Quiet);
        if let Some(streaming) = streaming {
            self.interrupted = true;
            let mut carry = streaming.carry;
            let code = streaming.code;
            carry.set_exec(&code);
            self.append(Entry::Step {
                at: UnixMs::now(),
                exec: Some(code),
                prose: String::new(),
                carry,
                usage: None,
            })
            .await?;
        }
        Ok(())
    }

    /// A piece of the call being written: its start opens a cell, its code
    /// feeds it.
    fn stream(
        &mut self,
        streaming: &mut Option<Streaming>,
        (carry, code): (Option<Carry>, String),
    ) -> anyhow::Result<()> {
        if let Some(carry) = carry
            && let Some(notebook) = &self.notebook
        {
            self.cell = Some(Latest {
                cell: notebook.stream()?,
                call: carry.with_code(String::new()),
                published: false,
            });
            self.progress.told_returned = false;
            *streaming = Some(Streaming {
                carry,
                code: String::new(),
            });
        }
        if let (Some(streaming), Some(latest)) = (streaming.as_mut(), &mut self.cell)
            && !code.is_empty()
        {
            streaming.code.push_str(&code);
            latest.call.code.push_str(&code);
            latest.cell.feed(code, false)?;
        }
        Ok(())
    }

    fn publish_stream(
        &mut self,
        streaming: Option<&Streaming>,
        refresh_runtime: bool,
    ) -> anyhow::Result<()> {
        self.writing = streaming.map(|stream| stream.carry.with_code(stream.code.clone()));
        if refresh_runtime {
            // Starting a cell changes occupancy; subsequent fragments do not.
            *self.status.write().expect("poison") = self.status()?;
        } else {
            // Notebook and mail events publish runtime independently. A code
            // frame changes the response and its derived draft, not runtime.
            let response = self.response();
            let draft = self.draft()?;
            let mut status = self.status.write().expect("poison");
            status.response = response;
            status.draft = draft;
        }
        self.host.published();
        Ok(())
    }

    fn response(&self) -> Option<StreamingResponse> {
        self.responding.then(|| StreamingResponse {
            id: self.response_id.clone(),
            items: self
                .writing
                .iter()
                .map(|call| Item::ToolCall {
                    id: call.display_id().to_owned(),
                    name: "exec".to_owned(),
                    arguments: call.code.clone(),
                    format: ArgumentsFormat::Text,
                })
                .collect(),
        })
    }
    fn draft(&self) -> anyhow::Result<Option<String>> {
        let Some(latest) = self.cell.as_ref().filter(|latest| !latest.published) else {
            return Ok(None);
        };
        if !self.responding && latest.cell.facts()?.finished.is_some() {
            return Ok(None);
        }
        Ok(super::shared::python_preview::tool_preview(
            "exec",
            &latest.call.code,
            ArgumentsFormat::Text,
        ))
    }

    /// What a reader sees, built from the loop's own state.
    fn status(&self) -> anyhow::Result<AgentStatus> {
        let inference = if let Some(backoff) = &self.backoff {
            InferenceState::Retrying {
                at: backoff.at,
                error: backoff.error.clone(),
            }
        } else if self.responding {
            InferenceState::Responding
        } else if let Some(Stopped::Failed(error)) = &self.stopped {
            InferenceState::Failed {
                error: error.to_string(),
            }
        } else {
            InferenceState::Idle
        };
        let running_tasks = self
            .notebook
            .as_ref()
            .map(NotebookSide::facts)
            .transpose()?
            .unwrap_or_default()
            .iter()
            .filter(|source| {
                matches!(
                    source.kind,
                    rho_notebook::Kind::Cell | rho_notebook::Kind::Task
                ) && source.finished.is_none()
            })
            .count() as u32;
        Ok(AgentStatus {
            runtime: RuntimeState {
                inference,
                running_tasks,
                awaiting_human: self.awaiting,
                checkin_at: if self.archived || self.stopped.is_some() {
                    None
                } else {
                    self.facts()?.checkin
                },
                archived: self.archived,
            },
            response: self.response(),
            draft: self.draft()?,
            queued: self.unread.len(),
        })
    }

    /// Durable history is committed before replacing the live response.
    /// Current occupancy is published directly, never appended to history.
    async fn publish(&mut self) -> anyhow::Result<()> {
        self.flush().await?;
        let status = self.status()?;
        let working = status.runtime.is_working();
        if working != self.working {
            let edge = if working {
                TurnEdge::Started
            } else {
                TurnEdge::Ended(match &self.stopped {
                    Some(Stopped::Failed(error)) => TurnOutcome::Errored {
                        message: error.to_string(),
                    },
                    _ => TurnOutcome::Completed,
                })
            };
            self.flush().await?;
            self.host.turn(rho_agent_types::UnixMs::now(), edge).await?;
            if !working {
                self.host.settled().await?;
            }
            self.working = working;
        }
        *self.status.write().expect("poison") = status;
        self.host.published();
        Ok(())
    }
}

/// The inference session for a role's binding, and the name its usage is
/// billed under. Credentials come from the agent host's account selection.
fn inference_session(
    inference: &Inference,
    binding: crate::log::SessionBinding,
) -> anyhow::Result<(InferenceSession, String)> {
    let profile: InferenceProfile = binding
        .deep_config()
        .ok_or_else(|| anyhow::anyhow!("Rho runtime stored with a Claude mode"))?;
    let model: InferenceModel = binding
        .deep_model()
        .ok_or_else(|| anyhow::anyhow!("Rho runtime stored without a model"))?;
    let billed = match model {
        InferenceModel::Gpt6Astra => crate::log::AgentUsageModel::ASTRA,
        InferenceModel::Gpt6Luna => crate::log::AgentUsageModel::LUNA,
        InferenceModel::Gpt6Sol => crate::log::AgentUsageModel::GPT,
    };
    Ok((inference.session(profile, model), billed.name().to_owned()))
}

fn cache_key(key: crate::inference::PromptCacheKey) -> CacheKey {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&key.to_bytes());
    CacheKey::from_u128(u128::from_le_bytes(bytes))
}

fn blocks(content: Vec<ContentPart>) -> Vec<Block> {
    content
        .into_iter()
        .map(|part| match part {
            ContentPart::Text { text } => Block::Text(text),
            ContentPart::Image { media_type, data } => Block::Image(Image { media_type, data }),
        })
        .collect()
}

fn until(at: UnixMs) -> Duration {
    Duration::from_millis(at.0.saturating_sub(UnixMs::now().0))
}

/// The model-facing surface of a role, for a reader: the prompt and the tool
/// entry point a new agent of that role would get, without constructing a
/// notebook.
pub fn render_agent_surface(
    workset: &rho_fs_view::Workset,
    place: &rho_agent_types::Place,
    role: AgentRole,
) -> anyhow::Result<crate::RenderedAgentSurface> {
    let place = prompt::WorksetPrompt::for_host(workset, place);
    let binding = role.session_profile();
    if binding.claude_model().is_some() {
        return Ok(crate::RenderedAgentSurface {
            system_prompt: prompt::claude_prompt(Some(&place), None, role),
            tools: Arc::from([rho_claude::mcp::exec_spec()]),
        });
    }
    binding
        .deep_config()
        .ok_or_else(|| anyhow::anyhow!("role has no inference profile"))?;
    Ok(crate::RenderedAgentSurface {
        system_prompt: prompt::prompt(&place, None, role),
        tools: Arc::from([rho_agent_types::transcript::ToolSpec {
            name: rho_agent_types::transcript::ToolName::try_from("exec").unwrap(),
            tool_type: rho_agent_types::transcript::ToolType::Custom,
            description: "Execute Python in the persistent notebook.".into(),
            input_schema: serde_json::Value::Null,
            format: Some(rho_agent_types::transcript::ToolFormat::Text),
        }]),
    })
}
