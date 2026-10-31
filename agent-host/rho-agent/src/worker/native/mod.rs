//! The native runtime: one agent's rows in its workset's log, its notebook,
//! and the loop that wakes the model.
//!
//! The loop does three things, over and over: record what arrives (inputs
//! from policy Python, and what the notebook hands back), ask
//! [`wake::decide`] whether the model should look, and if so wake it with a
//! report and run the cell it answers with. The model answers every wake
//! with one `exec` call; what it says to anyone goes through the tools
//! policy gave its notebook.

pub(crate) mod context;
mod persistence;

#[cfg(test)]
mod scripted;
#[cfg(test)]
mod tests;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rho_agent_types::{AgentId, ContentPart, InputWake, MailId, UnixMs, Wpos};
use rho_agents_client::protocol::transcript::{ArgumentsFormat, Item};
use rho_notebook::{CellHandle, Notebook};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::entry::{Entry, Notice, Report, RequestNotice, ResponseUsage, Wake};
use crate::inference::config::{InferenceModel, InferenceProfile};
use crate::inference::{
    CacheKey, Call, Carry, Event as Streamed, Inference, InferenceSession, PromptCacheKey,
    Request, Response, Step,
};
use crate::record::{Event, Record, Runtime};
use crate::store::AgentHead;
use crate::worker::host_client::{HostClient, StoreError};
use crate::worker::python::{self, TurnEnd};
use crate::worker::shared::outbox::{Outbound, Outbox};
use crate::worker::shared::wake::{Decision, Facts};
use crate::worker::shared::{Progress, tools, wake};
use crate::{AgentStatus, InferenceState, RuntimeState, StreamingResponse};

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
    /// The head as the loop keeps it.
    head: Arc<RwLock<AgentHead>>,
}

impl AgentHandle {
    pub fn status(&self) -> AgentStatus {
        self.status.read().expect("poison").clone()
    }

    /// The head as of the loop's last change to it.
    pub fn head(&self) -> AgentHead {
        self.head.read().expect("poison").clone()
    }

    /// Say the whole tail again: a client just started looking.
    pub fn tell_tail(&self) {
        let _ = self.control.send(Control::TellTail);
    }

    /// Queue an input and wait until the loop has durably logged it.
    pub async fn input(
        &self,
        content: Vec<ContentPart>,
        wake: InputWake,
        mail: Option<MailId>,
    ) -> anyhow::Result<()> {
        self.send(|done| Control::Input {
            content,
            wake,
            mail,
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

    pub(crate) async fn retire(&self) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(Control::Retire(reply))
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))?
    }

    /// A fresh prompt cache key, logged as a spec change.
    pub async fn new_cache_key(&self) -> anyhow::Result<()> {
        self.ask(|reply| Control::NewCacheKey { reply }).await
    }

    /// Branch before the `turns`-th last input from the user; the notebook
    /// stays.
    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        self.ask(|reply| Control::Rewind { turns, reply }).await
    }

    async fn ask(
        &self,
        command: impl FnOnce(oneshot::Sender<anyhow::Result<()>>) -> Control,
    ) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(command(reply))
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
/// landed: after its row is durable, not after the model has seen it.
enum Control {
    Retire(oneshot::Sender<anyhow::Result<()>>),
    Input {
        content: Vec<ContentPart>,
        wake: InputWake,
        mail: Option<MailId>,
        done: Option<oneshot::Sender<()>>,
    },
    Compact,
    Cancel,
    Retry,
    NewCacheKey {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    Rewind {
        turns: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Tell the live tail whole, for a client that just started looking.
    TellTail,
}

/// The latest cell, the call that wrote it, and when it started.
struct Latest {
    cell: CellHandle,
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
    boundary: oneshot::Receiver<crate::ipc::protocol::ContextBoundary>,
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
        let (_, _, rows) = host.native_history(Some(boundary)).await?;
        Ok(context::request(
            self.instructions.clone(),
            rows.iter().map(|(_, record)| &record.event),
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
    /// Where a new notebook starts, before `hooks.on_notebook` moves it.
    root: camino::Utf8PathBuf,
    context: context::Context,
    continuation: Option<crate::inference::Continuation>,
    notebook: Option<Notebook>,
    outbox_handle: Arc<Outbox>,
    outbox: mpsc::UnboundedReceiver<Outbound>,
    control_rx: mpsc::UnboundedReceiver<Control>,
    wake: Arc<Notify>,
    status: Arc<RwLock<AgentStatus>>,
    head: Arc<RwLock<AgentHead>>,
    name_updates: tokio::sync::watch::Receiver<Option<AgentHead>>,
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
    /// Inputs that wake the model and it has not seen, oldest first.
    unread: Vec<(InputWake, UnixMs)>,
    progress: Progress,
    awaiting: bool,
    /// The notebook went with a restart since the model's last wake: tell
    /// it at the next. Coming up is never itself a wake.
    restarted: bool,
    rewound: bool,
    retry: bool,
    backoff: Option<Backoff>,
    stopped: Option<Stopped>,
    cache_key: CacheKey,
    compaction: crate::entry::CompactionState,
    /// The context was compacted and no request has followed: the next one
    /// first logs what `hooks.on_compaction` gives back.
    compaction_owed: bool,
}

impl Agent {
    /// Construct a worker-owned runtime from agent host services, without
    /// opening a database or retaining the pool.
    pub(crate) async fn load(
        agent_id: AgentId,
        host: Arc<HostClient>,
        inference: Inference,
        root: camino::Utf8PathBuf,
    ) -> anyhow::Result<(AgentHandle, Self)> {
        let head = host.head().await?;
        let Runtime::Native {
            model,
            profile,
            cache_key: key,
        } = head.spec.runtime
        else {
            anyhow::bail!("agent does not use the native runtime");
        };
        let (session, model_name) = inference_session(&inference, model, profile);
        let (boundary, cursor, rows) = host.native_history(None).await?;
        let context = context::Context::restore(rows.iter().map(|(_, record)| &record.event));
        let (outbox_handle, outbox) = Outbox::new();
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
            root,
            context,
            continuation: None,
            notebook: None,
            outbox_handle,
            outbox,
            control_rx,
            wake: Arc::new(Notify::new()),
            status: Arc::clone(&status),
            head: Arc::clone(&head),
            working: false,
            archived: cursor.archived,
            fresh: false,
            responding: false,
            writing: None,
            response_id: String::new(),
            cell: None,
            interrupted: false,
            unread: Vec::new(),
            progress: Progress::default(),
            awaiting: false,
            restarted: false,
            rewound: false,
            retry: false,
            backoff: None,
            stopped: None,
            cache_key: cache_key(key),
            compaction: cursor.compaction,
            compaction_owed: false,
        };
        agent.resume(&rows, cursor.woken);
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

    /// Answer from `script` instead of the spec's provider.
    #[cfg(test)]
    fn script(&mut self, script: Arc<scripted::Scripted>) {
        self.session = Arc::new(script);
    }

    /// Pick up from the log: what the model has not seen, and whether there
    /// was a notebook that is now gone.
    fn resume(&mut self, rows: &[(Wpos, Record<'static>)], woken: bool) {
        self.restore_unread(rows);
        if woken && !self.archived {
            self.restarted = true;
        }
    }

    /// Every input after the newest request is unread: delivery is
    /// positional. A compaction with no request after it is owed its
    /// texts.
    fn restore_unread(&mut self, rows: &[(Wpos, Record<'static>)]) {
        self.unread.clear();
        self.compaction_owed = false;
        for (_, record) in rows {
            match &record.event {
                Event::Input { wake, .. } if *wake != InputWake::Quiet => {
                    self.unread.push((*wake, record.at));
                }
                Event::Native { entry, .. } => match entry {
                    Entry::RequestSent { .. } => {
                        self.unread.clear();
                        self.compaction_owed = false;
                    }
                    Entry::Step { carry, .. } if carry.has_compaction() => {
                        self.compaction_owed = true;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    /// Answer the one question after every event, act on the answer, and
    /// wait for the next one, until the last handle is dropped.
    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            self.writer.check()?;
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
            let decision = if let Some(backoff) = &self.backoff {
                if backoff.at <= UnixMs::now() {
                    Decision::Now(Wake::Prose)
                } else {
                    Decision::Later(Some(backoff.at))
                }
            } else {
                wake::decide(&self.facts(), UnixMs::now())
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
                () = self.wake.notified() => {}
                () = sleep => {}
            }
        }
    }

    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        if let Some(notebook) = self.notebook.take() {
            notebook.cancel();
            notebook.shutdown().await.map_err(anyhow::Error::msg)?;
        }
        self.flush().await?;
        Ok(())
    }

    /// The head changed outside the loop: a new spec takes effect at the
    /// next request.
    fn named(&mut self) {
        let stored = self.name_updates.borrow_and_update().clone();
        let Some(stored) = stored else { return };
        if let Runtime::Native {
            model,
            profile,
            cache_key: key,
        } = stored.spec.runtime
        {
            let (session, model_name) = inference_session(&self.inference, model, profile);
            self.session = session;
            self.model_name = model_name;
            self.cache_key = cache_key(key);
        }
        *self.head.write().expect("poison") = stored;
    }

    async fn control(&mut self, control: Control) -> anyhow::Result<()> {
        match control {
            Control::Retire(reply) => {
                if self.settled() {
                    let _ = reply.send(Ok(()));
                    // Freeze scheduling and admission at this serialized
                    // boundary. The driver cancels this future on agent host
                    // disconnect.
                    std::future::pending::<()>().await;
                } else {
                    let _ = reply.send(Err(anyhow::anyhow!("agent still has work")));
                }
            }
            Control::TellTail => self.host.tell_tail(),
            Control::Input {
                content,
                wake,
                mail,
                done,
            } => {
                self.receive(content, wake, mail).await?;
                if let Some(done) = done {
                    self.flush().await?;
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
            Control::NewCacheKey { reply } => {
                let result = self.new_cache_key().await;
                if result.as_ref().is_err_and(|error| error.is::<StoreError>()) {
                    return result;
                }
                let _ = reply.send(result);
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
    fn settled(&self) -> bool {
        !self.responding
            && self.backoff.is_none()
            && self.unread.is_empty()
            && self.notebook.as_ref().is_none_or(|notebook| {
                notebook
                    .facts()
                    .iter()
                    .all(|source| source.finished.is_some())
            })
            && (self.archived
                || self.stopped.is_some()
                || self.progress.ended
                || self.progress.last_response.is_none())
    }

    async fn new_cache_key(&mut self) -> anyhow::Result<()> {
        let mut spec = self.head.read().expect("poison").spec.clone();
        let Runtime::Native { cache_key: key, .. } = &mut spec.runtime else {
            anyhow::bail!("prompt cache keys are only available for native agents");
        };
        *key = PromptCacheKey::generate();
        self.cache_key = cache_key(*key);
        self.log(Event::AgentChanged {
            agent: self.agent_id,
            spec: spec.clone(),
        })
        .await?;
        self.flush().await?;
        self.head.write().expect("poison").spec = spec;
        Ok(())
    }

    /// Branch history before the `turns`-th last input from the user. What
    /// the agent walked away from stays in the log
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
        let rows = self.host.history().await?;
        let people = rows
            .iter()
            .filter(|(_, record)| {
                matches!(
                    record.event,
                    Event::Input {
                        wake: InputWake::Person,
                        ..
                    }
                )
            })
            .map(|(pos, _)| *pos)
            .collect::<Vec<_>>();
        anyhow::ensure!(!people.is_empty(), "nothing to rewind");
        let to = people[people.len().saturating_sub(turns as usize)];
        self.host
            .append(Event::Rewound {
                agent: self.agent_id,
                to,
            })
            .await?;
        let (boundary, cursor, rows) = self.host.native_history(None).await?;
        self.writer = persistence::Writer::new(self.host.clone(), boundary);
        self.context = context::Context::restore(rows.iter().map(|(_, record)| &record.event));
        self.continuation = None;
        self.restore_unread(&rows);
        self.progress.last_response = None;
        self.cell = None;
        self.progress.told_returned = false;
        self.interrupted = false;
        self.awaiting = false;
        self.progress.prose = 0;
        self.stopped = None;
        self.restarted = false;
        self.rewound = true;
        self.compaction = cursor.compaction;
        Ok(())
    }

    async fn compact(&mut self) -> anyhow::Result<()> {
        if !self.archived && !self.compaction.pending {
            self.append(Entry::CompactionTrigger { manual: true }).await?;
        }
        Ok(())
    }

    async fn append(&mut self, entry: Entry) -> anyhow::Result<()> {
        self.log(Event::Native {
            agent: self.agent_id,
            entry,
        })
        .await
    }

    /// Log one of this agent's rows and fold it into the live context.
    async fn log(&mut self, event: Event<'static>) -> anyhow::Result<()> {
        self.writer
            .append(vec![Record {
                at: UnixMs::now(),
                event: event.clone(),
            }])
            .await?;
        self.context.observe(&event);
        if let Event::Native { entry, .. } = &event {
            match entry {
                Entry::Step { carry, .. } => {
                    self.continuation = None;
                    if carry.has_compaction() {
                        self.compaction_owed = true;
                    }
                }
                Entry::RequestSent { .. } => self.compaction_owed = false,
                _ => {}
            }
            self.compaction.observe(entry);
        }
        Ok(())
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        Ok(self.writer.flush().await?)
    }

    async fn receive(
        &mut self,
        content: Vec<ContentPart>,
        wake: InputWake,
        mail: Option<MailId>,
    ) -> anyhow::Result<()> {
        let at = UnixMs::now();
        if wake == InputWake::Person {
            if self.archived && !self.responding {
                self.fresh_notebook().await?;
            }
            // The user's message ends a wait it answers.
            self.awaiting = false;
            self.stopped = None;
        }
        if wake != InputWake::Quiet {
            if let Some(backoff) = &mut self.backoff {
                backoff.at = at;
            }
            self.unread.push((wake, at));
        }
        self.log(Event::Input {
            agent: self.agent_id,
            mail,
            content,
            wake,
        })
        .await
    }

    async fn fresh_notebook(&mut self) -> anyhow::Result<()> {
        if let Some(notebook) = self.notebook.take() {
            notebook.cancel();
            let _ = notebook.shutdown().await;
        }
        self.archived = false;
        self.fresh = true;
        self.cell = None;
        self.progress = Progress::default();
        self.append(Entry::Notice {
            notice: Notice::FreshNotebook,
        })
        .await
    }

    async fn outbound(&mut self, outbound: Outbound) -> anyhow::Result<()> {
        match outbound {
            Outbound::Archive => {
                self.archived = true;
                self.backoff = None;
                if let Some(notebook) = self.notebook.take() {
                    notebook.cancel();
                    notebook.shutdown().await.map_err(anyhow::Error::msg)?;
                }
                self.cell = None;
                self.awaiting = false;
                self.append(Entry::Notice {
                    notice: Notice::Archived,
                })
                .await?;
                self.turn_ended(TurnEnd::Archived, None);
                Ok(())
            }
            Outbound::EndTurn => {
                self.progress.ended = true;
                self.awaiting = true;
                self.turn_ended(TurnEnd::EndTurn, None);
                Ok(())
            }
        }
    }

    /// Tell policy, without holding up the loop.
    fn turn_ended(&self, end: TurnEnd, error: Option<String>) {
        tokio::spawn(python::on_turn_end(self.agent_id, end, error));
    }

    async fn drain_outbox(&mut self) -> anyhow::Result<()> {
        while let Ok(outbound) = self.outbox.try_recv() {
            self.outbound(outbound).await?;
        }
        Ok(())
    }

    fn facts(&self) -> Facts {
        let notebook = self.progress.facts(
            self.notebook.as_ref(),
            self.cell.as_ref().map(|latest| &latest.cell),
        );
        let first = |wake| {
            self.unread
                .iter()
                .find(|(from, _)| *from == wake)
                .map(|(_, at)| *at)
        };
        Facts {
            // The cell that ended the turn returns to nobody.
            finished: notebook.finished.filter(|_| !self.progress.ended),
            human: first(InputWake::Person),
            agent: first(InputWake::Agent),
            prose: self.progress.prose > 0 || self.retry,
            rewound: self.rewound,
            compaction: self.compaction.pending,
            compaction_reply: self.compaction.reply,
            archived: self.archived,
            prose_silenced: self.stopped.is_some(),
            ..notebook
        }
    }

    /// Start the notebook on first use.
    async fn notebook(&mut self) -> anyhow::Result<&Notebook> {
        if self.notebook.is_none() {
            let (shell, exports) = tools::host_tools(
                &self.root,
                self.agent_id,
                Some(&self.inference),
                Some(&self.host),
                Some(&self.outbox_handle),
                true,
            );
            let notebook = Notebook::new(
                shell,
                exports,
                Arc::clone(&self.wake),
                Some(python::on_notebook(self.agent_id)),
            )
            .map_err(|error| anyhow::anyhow!("the notebook failed to start: {error}"))?;
            self.notebook = Some(notebook);
        }
        Ok(self.notebook.as_ref().expect("started above"))
    }

    async fn wake_model(&mut self, why: Wake) -> anyhow::Result<()> {
        self.drain_outbox().await?;
        if self.archived {
            return Ok(());
        }
        self.retry = false;
        let agent = self.agent_id;
        let owed = self.compaction_owed;
        let prepared = async {
            let instructions = Arc::<str>::from(python::instructions(agent).await?);
            let restored = if owed {
                python::on_compaction(agent).await?
            } else {
                Vec::new()
            };
            self.notebook().await?;
            anyhow::Ok((instructions, restored))
        }
        .await;
        let (instructions, restored) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return self.fail(format!("{error:#}")).await,
        };
        for text in restored {
            self.log(Event::Input {
                agent,
                mail: None,
                content: vec![ContentPart::Text { text }],
                wake: InputWake::Quiet,
            })
            .await?;
        }
        let mut report = Report::default();
        if self.restarted {
            self.append(Entry::Notice {
                notice: Notice::Restarted,
            })
            .await?;
            report.notices.push(RequestNotice::Restarted);
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
            .and_then(|notebook| notebook.report())
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
        self.unread.clear();
        let manual_only =
            why == Wake::Compaction && report.is_empty() && !self.context.has_queued();
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
            self.append(Entry::CompactionTrigger { manual: false })
                .await?;
        }
        self.append(Entry::RequestSent {
            why,
            report,
            compact: self.compaction.pending,
        })
        .await?;
        self.restarted = false;
        self.rewound = false;
        self.progress.ended = false;
        if let Some(notebook) = &self.notebook {
            notebook.reset_checkin();
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
            // The socket actor emits ordered owned events; the agent alone
            // admits Python.
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
                                Some(Streamed::Call { carry }) => {
                                    self.stream(&mut streaming, (Some(carry), String::new()));
                                    pending_stream = true;
                                    stream_needs_runtime = true;
                                }
                                Some(Streamed::Code(code)) => {
                                    self.stream(&mut streaming, (None, code));
                                    pending_stream = true;
                                }
                                Some(Streamed::Completed(step)) => break 'exchange Ok(step),
                                Some(Streamed::NeedsContext) => {
                                    break; // The consumed continuation becomes a full replay at the same cutoff.
                                }
                                Some(Streamed::Failed(error)) => break 'exchange Err(error),
                                None => break 'exchange Err(anyhow::anyhow!("model session stopped")),
                            }
                        },
                        _ = stream_frame.tick(), if pending_stream => {
                            self.publish_stream(streaming.as_ref(), stream_needs_runtime);
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
                        () = self.wake.notified() => self.publish().await?,
                    }
                }
            };
            if pending_stream {
                self.publish_stream(streaming.as_ref(), stream_needs_runtime);
            }
            match result {
                Ok(step) => Ok((step, streaming)),
                Err(error) => {
                    let retryable = crate::inference::is_retryable(&error);
                    let error = format!("{error:#}");
                    self.append(Entry::Notice {
                        notice: Notice::Error(error.clone()),
                    })
                    .await?;
                    // Code that already ran cannot be taken back: it stands
                    // as the step, and the model hears it was cut off.
                    if let (Some(latest), Some(streaming)) = (&self.cell, streaming)
                        && let Some(ran) = latest.cell.interrupt()
                    {
                        Err((
                            streaming.carry,
                            streaming.code[..ran.min(streaming.code.len())].to_owned(),
                        ))
                    } else {
                        self.responding = false;
                        self.writing = None;
                        if self.archived {
                            return self.revive_if_written().await;
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
                    exec: Some(code),
                    prose: String::new(),
                    carry,
                    usage: None,
                })
                .await?;
                return self.revive_if_written().await;
            }
        };
        self.finish_turn(step, streaming).await
    }

    async fn finish_turn(
        &mut self,
        step: Step,
        streaming: Option<Streaming>,
    ) -> anyhow::Result<()> {
        let continuation = step.continuation;
        let compacted = step.carry.has_compaction();
        let usage = step.usage;
        self.append(Entry::Step {
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
                    let _ = latest.cell.feed(rest.to_owned(), true);
                    latest.call = call;
                }
            }
            (Some(call), None) => {
                self.progress.prose = 0;
                let cell = self.notebook().await?.run(call.code.clone());
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
                    latest.cell.stop();
                }
                self.progress.prose += 1;
                if self.progress.prose >= Progress::MAX_PROSE {
                    self.progress.prose = 0;
                    self.stopped = Some(Stopped::Quiet);
                }
            }
        }
        self.revive_if_written().await
    }

    /// The user wrote to an archived agent while it was responding.
    async fn revive_if_written(&mut self) -> anyhow::Result<()> {
        if self.archived && self.unread.iter().any(|(wake, _)| *wake == InputWake::Person) {
            self.fresh_notebook().await?;
        }
        Ok(())
    }

    /// Model requests kept failing: stop until someone writes or retries.
    async fn fail(&mut self, error: String) -> anyhow::Result<()> {
        self.backoff = None;
        self.stopped = Some(Stopped::Failed(Arc::from(error.as_str())));
        self.flush().await?;
        self.turn_ended(TurnEnd::Failed, Some(error));
        Ok(())
    }

    async fn interrupt(&mut self, streaming: Option<Streaming>) -> anyhow::Result<()> {
        self.backoff = None;
        if let Some(notebook) = &self.notebook {
            notebook.cancel();
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
    ) {
        if let Some(carry) = carry
            && let Some(notebook) = &self.notebook
        {
            self.cell = Some(Latest {
                cell: notebook.stream(),
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
            let _ = latest.cell.feed(code, false);
        }
    }

    fn publish_stream(&mut self, streaming: Option<&Streaming>, refresh_runtime: bool) {
        self.writing = streaming.map(|stream| stream.carry.with_code(stream.code.clone()));
        if refresh_runtime {
            // Starting a cell changes occupancy; subsequent fragments do not.
            *self.status.write().expect("poison") = self.status();
        } else {
            // Notebook and mail events publish runtime independently. A code
            // frame changes the response and its derived draft, not runtime.
            let response = self.response();
            let draft = self.draft();
            let mut status = self.status.write().expect("poison");
            status.response = response;
            status.draft = draft;
        }
        self.host.published();
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

    fn draft(&self) -> Option<String> {
        self.cell
            .as_ref()
            .filter(|latest| {
                !latest.published && (self.responding || latest.cell.facts().finished.is_none())
            })
            .and_then(|latest| {
                super::shared::python_preview::tool_preview(
                    "exec",
                    &latest.call.code,
                    ArgumentsFormat::Text,
                )
            })
    }

    /// What a reader sees, built from the loop's own state.
    fn status(&self) -> AgentStatus {
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
        AgentStatus {
            runtime: RuntimeState {
                inference,
                running_tasks: self.notebook.as_ref().map_or(0, |notebook| {
                    notebook
                        .facts()
                        .iter()
                        .filter(|source| {
                            matches!(
                                source.kind,
                                rho_notebook::Kind::Cell | rho_notebook::Kind::Task
                            ) && source.finished.is_none()
                        })
                        .count() as u32
                }),
                awaiting_human: self.awaiting,
                checkin_at: if self.archived || self.stopped.is_some() {
                    None
                } else {
                    self.facts().checkin
                },
                archived: self.archived,
                stale: false,
            },
            response: self.response(),
            draft: self.draft(),
            queued: self.unread.len(),
        }
    }

    /// Durable history is committed before replacing the live response.
    /// Current occupancy is published directly, never appended to history.
    async fn publish(&mut self) -> anyhow::Result<()> {
        self.flush().await?;
        let status = self.status();
        let working = status.runtime.is_working();
        if working != self.working {
            if !working && let Some(Stopped::Failed(error)) = &self.stopped {
                let notice = Notice::Stopped(error.to_string());
                self.append(Entry::Notice { notice }).await?;
            }
            self.flush().await?;
            self.working = working;
        }
        *self.status.write().expect("poison") = status;
        self.host.published();
        Ok(())
    }
}

/// The inference session for a spec's model and profile, and the name its
/// usage is billed under. Credentials come from the agent host's account
/// selection.
fn inference_session(
    inference: &Inference,
    model: InferenceModel,
    profile: InferenceProfile,
) -> (InferenceSession, String) {
    let billed = match model {
        InferenceModel::Gpt6Astra => crate::log::AgentUsageModel::ASTRA,
        InferenceModel::Gpt61Terra => crate::log::AgentUsageModel::TERRA,
        InferenceModel::Gpt61Sol => crate::log::AgentUsageModel::GPT,
    };
    (inference.session(profile, model), billed.name().to_owned())
}

fn cache_key(key: PromptCacheKey) -> CacheKey {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&key.to_bytes());
    CacheKey::from_u128(u128::from_le_bytes(bytes))
}

fn until(at: UnixMs) -> Duration {
    Duration::from_millis(at.0.saturating_sub(UnixMs::now().0))
}
