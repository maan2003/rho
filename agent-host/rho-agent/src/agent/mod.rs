//! The Rho runtime: one agent's log, its notebook, and the loop that wakes
//! the model.
//!
//! The loop does three things, over and over: record what arrives (messages
//! from outside, and what the notebook sends out), ask [`wake::decide`]
//! whether the model should look, and if so wake it with a report and run
//! the cell it answers with. The model answers every wake with one `exec`
//! call and speaks to the person only through `human.send`.

pub(crate) mod context;
pub(crate) mod mailroom;
mod persistence;
pub(crate) mod tools;
pub(crate) mod wake;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rho_agent_types::{
    AgentId, AgentRole, ContentPart, EngineerIntelligence, MessageDelivery, TurnEdge, TurnOutcome,
    UnixMs,
};
use rho_inference::Inference;
use rho_inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use rho_inference::types::{PendingInferenceResponse, ToolCall, ToolName, ToolType};
use rho_inference2::{CacheKey, Call, CallId, Carry, Image, Model, Stream, Usage};
use rho_notebook2::{CellHandle, Notebook};
use tokio::sync::{Notify, mpsc, oneshot};

use self::mailroom::{Mailroom, Outbound};
use self::wake::{Decision, Facts};
use crate::db::{AgentHead, AgentRoleSessionProfile as _, AgentRuntime, UnixMillis};
use crate::entry::{Block, CallResult, Entry, MessageId, Notice, Party, ResponseUsage, Wake};
use crate::lazy::Lazy;
use crate::{
    AgentEvent, AgentStateKind, AgentStatus, FailedInferenceResponse, ToolPreview, View, prompt,
};

/// Failed model requests in a row before the agent stops until someone
/// writes or retries.
const MAX_FAILURES: u32 = 3;
/// Steps in a row without a call before the same.
const MAX_PROSE: u32 = 3;
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
    /// The agent's place, materialized on first use: a new agent's clone
    /// may still be in flight when a terminal or shell asks for it.
    view: Arc<Lazy<Arc<View>>>,
}

impl AgentHandle {
    /// The agent's view, ready once its place is.
    pub async fn view(&self) -> anyhow::Result<Arc<View>> {
        Ok(Arc::clone(self.view.get().await?))
    }

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

    pub fn send_user_message(&self, text: impl Into<String>, delivery: MessageDelivery) {
        self.send_user_content(vec![ContentPart::Text { text: text.into() }], delivery);
    }

    pub fn send_user_content(&self, content: Vec<ContentPart>, _delivery: MessageDelivery) {
        let _ = self.control.send(Control::Received {
            from: Party::Human,
            content,
            done: None,
        });
    }

    /// Send user input and wait until the loop has durably logged it.
    pub async fn send_user_content_accepted(
        &self,
        content: Vec<ContentPart>,
        _delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        self.send(|done| Control::Received {
            from: Party::Human,
            content,
            done: Some(done),
        })
        .await
    }

    /// Deliver mail from a peer agent.
    pub fn send_agent_message(&self, sender: AgentId, text: impl Into<String>) {
        let _ = self.control.send(Control::Received {
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

    /// Wake an agent that stopped after failing.
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
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))
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
            rho_inference::PromptCacheKey::generate(),
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
    Drain(oneshot::Sender<()>),
    Received {
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
    ChangePromptCacheKey(rho_inference::PromptCacheKey),
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
    started_at: UnixMs,
}

/// The call of the step in progress, as its code arrives.
struct Streaming {
    id: CallId,
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

pub(crate) struct Agent {
    agent_id: AgentId,
    host: Arc<crate::worker::Host>,
    writer: persistence::Writer,
    inference: Inference,
    model: Arc<Model>,
    model_name: String,
    view: Arc<Lazy<Arc<View>>>,
    /// The visible branch of this runtime's rows, oldest first.
    entries: Vec<Entry>,
    notebook: Option<Notebook>,
    mailroom: Arc<Mailroom>,
    outbox: mpsc::UnboundedReceiver<Outbound>,
    control_rx: mpsc::UnboundedReceiver<Control>,
    wake: Arc<Notify>,
    status: Arc<RwLock<AgentStatus>>,
    head: Arc<RwLock<AgentHead>>,
    name_updates: tokio::sync::watch::Receiver<Option<AgentHead>>,
    draining: Option<oneshot::Sender<()>>,
    /// Whether the last published state counted as a running turn, so the
    /// turn's edges are told once each.
    working: bool,

    archived: bool,
    /// The next wake says the notebook is new.
    fresh: bool,
    responding: bool,
    /// The latest cell and its call. Older ones live on in the notebook's
    /// sources.
    cell: Option<Latest>,
    /// The latest step was cut off part-way through its cell.
    interrupted: bool,
    /// The model has been told the latest cell finished.
    told_returned: bool,
    /// Messages the model has not seen, oldest first.
    unread: Vec<(MessageId, Party, UnixMs)>,
    last_step: Option<UnixMs>,
    awaiting: bool,
    prose: u32,
    /// The notebook went with a restart since the model's last wake: tell
    /// it at the next. Coming up is never itself a wake.
    restarted: bool,
    rewound: bool,
    retry: bool,
    stopped: Option<Stopped>,
    cache_key: CacheKey,
    context_used: Option<u64>,
    compaction_pending: bool,
    compaction_reply: bool,
}

impl Agent {
    /// Construct a worker-owned runtime from agent host services, without
    /// opening a database or retaining the pool.
    pub(crate) async fn load(
        agent_id: AgentId,
        host: Arc<crate::worker::Host>,
        inference: Inference,
        view: Arc<Lazy<Arc<View>>>,
    ) -> anyhow::Result<(AgentHandle, Self)> {
        let head = host.head().await?;
        let AgentRuntime::Rho { prompt_cache_key } = head.config.runtime else {
            anyhow::bail!("agent does not use the Rho runtime");
        };
        let (model, model_name) = model(&inference, head.config.binding)?;
        let (_, rows) = host.history().await?;
        let entries = rows
            .into_iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Entry(entry) => Some(entry),
                _ => None,
            })
            .collect();
        let (mailroom, outbox) = Mailroom::new();
        let status = Arc::new(RwLock::new(AgentStatus {
            kind: AgentStateKind::Idle,
            queued: 0,
        }));
        let head = Arc::new(RwLock::new(head));
        let (control, control_rx) = mpsc::unbounded_channel();
        host.observe(&status);
        let mut agent = Self {
            agent_id,
            writer: persistence::Writer::new(host.clone()),
            name_updates: host.names(),
            host,
            inference,
            model: Arc::new(model),
            model_name,
            view: Arc::clone(&view),
            entries,
            notebook: None,
            mailroom,
            outbox,
            control_rx,
            wake: Arc::new(Notify::new()),
            status: Arc::clone(&status),
            head: Arc::clone(&head),
            draining: None,
            working: false,
            archived: false,
            fresh: false,
            responding: false,
            cell: None,
            interrupted: false,
            told_returned: false,
            unread: Vec::new(),
            last_step: None,
            awaiting: false,
            prose: 0,
            restarted: false,
            rewound: false,
            retry: false,
            stopped: None,
            cache_key: cache_key(prompt_cache_key),
            context_used: None,
            compaction_pending: false,
            compaction_reply: false,
        };
        agent.resume().await?;
        agent.publish_sync();
        Ok((
            AgentHandle {
                control,
                status,
                head,
                view,
            },
            agent,
        ))
    }

    /// Answer from `script` instead of the role's provider.
    #[cfg(test)]
    fn script(&mut self, script: Arc<rho_inference2::scripted::Scripted>) {
        self.model = Arc::new(Model::Scripted(script));
    }

    /// Pick up from the log: what the model has not seen, and whether there
    /// was a notebook that is now gone.
    async fn resume(&mut self) -> anyhow::Result<()> {
        let mut delivered = std::collections::HashSet::new();
        // Any wake may have run code: streamed code runs before its step is
        // logged.
        let mut woken = false;
        let mut awaiting = false;
        for entry in &self.entries {
            match entry {
                Entry::Woken {
                    messages,
                    acknowledged,
                    ..
                } => {
                    woken = true;
                    delivered.extend(messages.iter().chain(acknowledged).copied());
                }
                Entry::Awaiting { since, .. } => awaiting = since.is_some(),
                Entry::Notice {
                    notice: Notice::Archived,
                    ..
                } => self.archived = true,
                Entry::Notice {
                    notice: Notice::FreshNotebook,
                    ..
                } => self.archived = false,
                _ => {}
            }
        }
        for entry in &self.entries {
            if let Entry::Received { at, id, from, .. } = entry
                && !delivered.contains(id)
            {
                self.unread.push((*id, *from, *at));
                if *from == Party::Human {
                    self.mailroom.received();
                }
            }
        }
        if woken && !self.archived {
            self.restarted = true;
        }
        self.refresh_compaction_state();
        if awaiting {
            // Whatever awaited the human went with the old notebook.
            self.append(Entry::Awaiting {
                at: UnixMs::now(),
                since: None,
            })
            .await?;
        }
        Ok(())
    }

    /// Answer the one question after every event, act on the answer, and
    /// wait for the next one, until the last handle is dropped.
    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            self.writer.check()?;
            if self.draining.is_some() && !self.responding {
                self.publish(None).await?;
                self.flush().await?;
                let _ = self.draining.take().expect("checked above").send(());
                // Frozen like a retired loop; the driver cancels this future
                // when the agent host lets go.
                std::future::pending::<()>().await;
            }
            // A cell can archive itself as it completes. Apply what it sent
            // before deciding whether its completion warrants a wake.
            self.drain_outbox().await?;
            let decision = if self.draining.is_some() {
                Decision::Later(None)
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
            self.publish(recheck).await?;
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
            Control::Drain(reply) => self.draining = Some(reply),
            Control::TellTail => self.host.tell_tail(),
            Control::Received {
                from,
                content,
                done,
            } => {
                let text = rho_inference::types::text_content(&content);
                self.receive(from, blocks(content)).await?;
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
                if matches!(self.stopped, Some(Stopped::Failed(_))) {
                    self.stopped = None;
                    self.retry = true;
                }
            }
            Control::ChangeRole { role, reply } => {
                let result = self.change_role(role).await;
                if result
                    .as_ref()
                    .is_err_and(|error| error.is::<crate::worker::StoreError>())
                {
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
                if result
                    .as_ref()
                    .is_err_and(|error| error.is::<crate::worker::StoreError>())
                {
                    return result;
                }
                let _ = reply.send(result);
            }
        }
        Ok(())
    }

    /// Nothing in motion, nothing waiting to be seen.
    fn settled(&self) -> bool {
        !self.responding && self.unread.is_empty() && !self.cell_running()
    }

    fn cell_running(&self) -> bool {
        self.cell
            .as_ref()
            .is_some_and(|latest| latest.cell.facts().finished.is_none())
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
            !self.responding && !self.cell_running(),
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
        let (model, model_name) = model(&self.inference, binding)?;
        self.flush().await?;
        self.host.profile(role, binding).await?;
        {
            let mut head = self.head.write().expect("poison");
            head.config.role = role;
            head.config.binding = binding;
        }
        self.model = Arc::new(model);
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
        self.host.rewind(UnixMillis::now(), to).await?;
        let (_, rows) = self.host.history().await?;
        self.entries = rows
            .into_iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Entry(entry) => Some(entry),
                _ => None,
            })
            .collect();
        self.unread.clear();
        self.last_step = None;
        self.cell = None;
        self.told_returned = false;
        self.interrupted = false;
        self.awaiting = false;
        self.prose = 0;
        self.stopped = None;
        self.restarted = false;
        self.rewound = true;
        self.refresh_compaction_state();
        Ok(())
    }

    /// Occupancy and an unfinished compaction, from the current branch
    /// only: a rewind can restore a pre-compaction context.
    fn refresh_compaction_state(&mut self) {
        let mut used = None;
        let mut pending = false;
        let mut owes_reply = false;
        let mut reply = false;
        for entry in &self.entries {
            match entry {
                Entry::CompactionTrigger { manual, .. } => {
                    pending = true;
                    owes_reply = !manual;
                }
                Entry::Woken { .. } if pending => owes_reply = true,
                Entry::Woken { .. } => reply = false,
                Entry::Step { carry, usage, .. } => {
                    if carry.has_compaction() {
                        used = None;
                    } else if usage.input_tokens > 0 {
                        used = Some(usage.input_tokens.saturating_add(usage.output_tokens));
                    }
                    if pending {
                        reply = carry.has_compaction() && owes_reply;
                        pending = false;
                        owes_reply = false;
                    }
                }
                _ => {}
            }
        }
        self.context_used = used;
        self.compaction_pending = pending;
        self.compaction_reply = reply;
    }

    async fn compact(&mut self) -> anyhow::Result<()> {
        if !self.archived && !self.compaction_pending {
            self.append(Entry::CompactionTrigger {
                at: UnixMs::now(),
                manual: true,
            })
            .await?;
            self.refresh_compaction_state();
        }
        Ok(())
    }

    async fn append(&mut self, entry: Entry) -> anyhow::Result<()> {
        self.writer
            .append(vec![AgentEvent::Entry(entry.clone())])
            .await?;
        self.entries.push(entry);
        Ok(())
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        Ok(self.writer.flush().await?)
    }

    async fn receive(&mut self, from: Party, body: Vec<Block>) -> anyhow::Result<()> {
        let at = UnixMs::now();
        let id = MessageId::new();
        if from == Party::Human {
            if self.archived && !self.responding {
                self.fresh_notebook(at).await?;
            }
            self.mailroom.received();
            self.stopped = None;
        } else {
            self.mailroom.agent_received();
        }
        self.unread.push((id, from, at));
        self.append(Entry::Received { at, id, from, body }).await
    }

    async fn fresh_notebook(&mut self, at: UnixMs) -> anyhow::Result<()> {
        if let Some(notebook) = self.notebook.take() {
            notebook.cancel();
            let _ = notebook.shutdown().await;
        }
        self.archived = false;
        self.fresh = true;
        self.cell = None;
        self.told_returned = false;
        self.append(Entry::Notice {
            at,
            notice: Notice::FreshNotebook,
        })
        .await
    }

    async fn outbound(&mut self, outbound: Outbound) -> anyhow::Result<()> {
        let at = UnixMs::now();
        match outbound {
            Outbound::Send(text) => {
                self.append(Entry::Sent {
                    at,
                    id: MessageId::new(),
                    to: Party::Human,
                    text: text.clone(),
                })
                .await?;
                // Whoever is subscribed to this agent's answers gets it as
                // mail, and the sidecar reads what it asks of the person.
                self.flush().await?;
                self.host.completed(text).await?;
                Ok(())
            }
            Outbound::Status(text) => self.append(Entry::Status { at, text }).await,
            Outbound::Archive => {
                self.archived = true;
                if let Some(notebook) = &self.notebook {
                    notebook.cancel();
                }
                self.append(Entry::Notice {
                    at,
                    notice: Notice::Archived,
                })
                .await
            }
            Outbound::Awaiting(awaiting) if awaiting != self.awaiting => {
                self.awaiting = awaiting;
                self.append(Entry::Awaiting {
                    at,
                    since: awaiting.then_some(at),
                })
                .await
            }
            Outbound::Awaiting(_) => Ok(()),
        }
    }

    async fn drain_outbox(&mut self) -> anyhow::Result<()> {
        while let Ok(outbound) = self.outbox.try_recv() {
            self.outbound(outbound).await?;
        }
        Ok(())
    }

    fn facts(&self) -> Facts {
        let sources = self
            .notebook
            .as_ref()
            .map(|notebook| notebook.facts())
            .unwrap_or_default();
        let latest = self.cell.as_ref().and_then(|latest| {
            sources
                .iter()
                .find(|source| source.session_id == latest.cell.session_id())
        });
        let (wait, wake_on_tools) = self
            .notebook
            .as_ref()
            .map(|notebook| notebook.checkin())
            .unwrap_or((wake::DEFAULT_CHECKIN, true));
        let finished = latest
            .and_then(|facts| facts.finished)
            .filter(|end| !end.failed && !self.told_returned)
            .map(|end| end.at);
        Facts {
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
            finished,
            notified: sources
                .iter()
                .filter_map(|facts| facts.notified_at.into_iter().chain(facts.paged_at).min())
                .min(),
            failure: sources
                .iter()
                .filter(|facts| !facts.delivered && facts.finished.is_some_and(|end| end.failed))
                .filter_map(|facts| facts.finished.map(|end| end.at))
                .min(),
            checkin: self.last_step.map(|at| at + wait),
            response_finished: self.last_step,
            wake_on_tools,
            prose: self.prose > 0 || self.retry,
            rewound: self.rewound,
            compaction: self.compaction_pending,
            compaction_reply: self.compaction_reply,
            archived: self.archived,
            prose_silenced: self.stopped.is_some(),
        }
    }

    /// The notebook, started on first use: a load never fails on a place
    /// that has gone, a wake may.
    async fn notebook(&mut self) -> anyhow::Result<&Notebook> {
        if self.notebook.is_none() {
            let view = Arc::clone(self.view.get().await?);
            let team = self.host.team().await?;
            let role = self.head.read().expect("poison").config.role;
            let (shell, exports) = tools::host_tools(
                &view,
                role,
                self.agent_id,
                Some(&self.inference),
                team.as_ref(),
                Some(&self.host),
                Some(&self.mailroom),
            );
            let notebook = Notebook::new(shell, exports, Arc::clone(&self.wake))
                .map_err(|error| anyhow::anyhow!("the notebook failed to start: {error}"))?;
            self.notebook = Some(notebook);
        }
        Ok(self.notebook.as_ref().expect("started above"))
    }

    async fn instructions(&mut self) -> anyhow::Result<Arc<str>> {
        let view = Arc::clone(self.view.get().await?);
        let team = self.host.team().await?;
        let role = self.head.read().expect("poison").config.role;
        Ok(prompt::prompt(&view, team.as_ref(), role))
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
        let mut lines = Vec::new();
        if self.restarted {
            self.append(Entry::Notice {
                at: UnixMs::now(),
                notice: Notice::Restarted,
            })
            .await?;
            lines.push(
                "rho restarted. Your notebook and everything running in it are gone, and \
                 their side effects may remain. Check the current state before carrying on."
                    .to_owned(),
            );
        }
        match why {
            Wake::Rewound => lines.push(
                "The human rewound your visible history. Your Python notebook, running work, \
                 and side effects were not rewound. Check the current state before continuing."
                    .to_owned(),
            ),
            Wake::Prose if self.prose > 0 => lines.push(
                "Your last response had no exec call. Text outside a call reaches nobody: \
                 speak with human.send()."
                    .to_owned(),
            ),
            _ => {}
        }
        if std::mem::take(&mut self.fresh) {
            lines.push(
                "This agent was archived. You have a fresh notebook; earlier Python state and \
                 running work are gone."
                    .to_owned(),
            );
        }
        if std::mem::take(&mut self.interrupted) {
            lines.push(
                "Your response was cut off while you were writing its cell; only the code \
                 shown ran. Carry on from the notebook's state without replaying it."
                    .to_owned(),
            );
        }
        let mut images = Vec::new();
        if let Some(report) = self
            .notebook
            .as_ref()
            .and_then(|notebook| notebook.report())
        {
            lines.push(report.text);
            images.extend(report.images.into_iter().map(|image| Image {
                media_type: image.media_type,
                data: image.data,
            }));
        }
        self.wake_with(why, instructions, lines, images).await
    }

    async fn wake_with(
        &mut self,
        why: Wake,
        instructions: Arc<str>,
        mut lines: Vec<String>,
        images: Vec<Image>,
    ) -> anyhow::Result<()> {
        if self.cell.is_some() {
            self.told_returned = true;
        }
        let messages = std::mem::take(&mut self.unread);
        let humans = messages
            .iter()
            .filter(|(_, from, _)| *from == Party::Human)
            .count();
        self.mailroom.read(humans as u64);
        if lines.is_empty() {
            lines.push(
                if !messages.is_empty() {
                    "New messages below."
                } else if why == Wake::Checkin {
                    "Check-in: nothing new."
                } else {
                    "Nothing new."
                }
                .to_owned(),
            );
        }
        let manual_only = why == Wake::Compaction
            && messages.is_empty()
            && images.is_empty()
            && lines == ["Nothing new."];
        let report = lines.join("\n\n");
        if !manual_only {
            let pending = self
                .entries
                .iter()
                .rev()
                .find_map(|entry| match entry {
                    Entry::Step { calls, .. } => Some(calls.clone()),
                    Entry::Woken { .. } => Some(Vec::new()),
                    _ => None,
                })
                .unwrap_or_default();
            let results = pending
                .into_iter()
                .map(|call| CallResult {
                    id: call.id,
                    text: report.clone(),
                    images: images.clone(),
                })
                .collect();
            self.append(Entry::Woken {
                at: UnixMs::now(),
                why,
                report,
                images,
                messages: messages.into_iter().map(|(id, _, _)| id).collect(),
                acknowledged: Vec::new(),
                results,
            })
            .await?;
            self.restarted = false;
            self.rewound = false;
        }
        if let Some(notebook) = &self.notebook {
            notebook.reset_checkin();
        }
        if self
            .context_used
            .is_some_and(|used| used >= AUTO_COMPACT_TOKENS)
            && !self.compaction_pending
        {
            self.append(Entry::CompactionTrigger {
                at: UnixMs::now(),
                manual: false,
            })
            .await?;
            self.refresh_compaction_state();
        }
        let request = context::request(instructions, &self.entries, self.cache_key);

        self.responding = true;
        self.publish(None).await?;
        let mut failures = 0;
        let step = loop {
            let model = Arc::clone(&self.model);
            // The call's code, as it arrives, runs as it arrives.
            let (code_tx, mut code_rx) = mpsc::unbounded_channel();
            let mut streaming = None;
            let result = {
                let mut forward = move |piece: Stream<'_>| {
                    let _ = code_tx.send(match piece {
                        Stream::Call { id } => (Some(id.clone()), String::new()),
                        Stream::Code(code) => (None, code.to_owned()),
                    });
                };
                let step = model.step(&request, &mut forward);
                tokio::pin!(step);
                // Keep recording what arrives while the model writes.
                loop {
                    tokio::select! {
                        biased;
                        error = self.writer.failed() => return Err(error.into()),
                        result = &mut step => break result,
                        Some(piece) = code_rx.recv() => self.stream(&mut streaming, piece),
                        control = self.control_rx.recv() => match control {
                            Some(Control::Cancel) => {
                                self.interrupt(streaming).await?;
                                return Ok(());
                            }
                            Some(control) => self.control(control).await?,
                            None => return Ok(()),
                        },
                        Some(outbound) = self.outbox.recv() => self.outbound(outbound).await?,
                    }
                }
            };
            while let Ok(piece) = code_rx.try_recv() {
                self.stream(&mut streaming, piece);
            }
            match result {
                Ok(step) => break Ok((step, streaming)),
                Err(error) => {
                    let error = format!("{error:#}");
                    self.append(Entry::Notice {
                        at: UnixMs::now(),
                        notice: Notice::Error(error.clone()),
                    })
                    .await?;
                    // Code that already ran cannot be taken back: it stands
                    // as the step, and the model hears it was cut off.
                    if let (Some(latest), Some(streaming)) = (&self.cell, streaming)
                        && let Some(ran) = latest.cell.interrupt()
                    {
                        break Err(Call {
                            id: streaming.id,
                            code: streaming.code[..ran.min(streaming.code.len())].to_owned(),
                        });
                    }
                    failures += 1;
                    if failures >= MAX_FAILURES {
                        self.responding = false;
                        return self.fail(error).await;
                    }
                    tokio::time::sleep(Duration::from_secs(2u64.pow(failures))).await;
                }
            }
        };
        self.responding = false;
        let at = UnixMs::now();
        self.last_step = Some(at);
        let (step, streaming) = match step {
            Ok(step) => step,
            Err(call) => {
                self.interrupted = true;
                self.prose = 0;
                self.append(Entry::Step {
                    at,
                    calls: vec![call.clone()],
                    prose: String::new(),
                    carry: Carry::bare(call),
                    usage: Usage::default(),
                })
                .await?;
                return self.revive_if_written(at).await;
            }
        };
        let compacted = step.carry.has_compaction();
        let usage = step.usage;
        self.append(Entry::Step {
            at,
            calls: step.call.clone().into_iter().collect(),
            prose: step.prose,
            carry: step.carry,
            usage,
        })
        .await?;
        self.append(Entry::Usage {
            at,
            usage: ResponseUsage::rho(self.model_name.clone(), usage),
        })
        .await?;
        self.refresh_compaction_state();
        match (step.call, streaming) {
            (Some(call), Some(streaming)) => {
                self.prose = 0;
                if let Some(latest) = &mut self.cell {
                    // Whatever the stream missed, then the end.
                    let rest = call.code.strip_prefix(&streaming.code).unwrap_or_default();
                    let _ = latest.cell.feed(rest.to_owned(), true);
                    latest.call = call;
                }
            }
            (Some(call), None) => {
                self.prose = 0;
                let cell = self.notebook().await?.run(call.code.clone());
                self.cell = Some(Latest {
                    cell,
                    call,
                    started_at: at,
                });
                self.told_returned = false;
            }
            (None, streaming) => {
                if compacted {
                    self.prose = 0;
                    return Ok(());
                }
                if streaming.is_some()
                    && let Some(latest) = &self.cell
                {
                    latest.cell.stop();
                }
                self.prose += 1;
                if self.prose >= MAX_PROSE {
                    self.prose = 0;
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
        self.stopped = Some(Stopped::Failed(Arc::from(error.as_str())));
        self.flush().await?;
        self.host.failed(error).await?;
        Ok(())
    }

    async fn interrupt(&mut self, streaming: Option<Streaming>) -> anyhow::Result<()> {
        if let Some(notebook) = &self.notebook {
            notebook.cancel();
        }
        self.cell = None;
        self.responding = false;
        self.stopped = Some(Stopped::Quiet);
        if let Some(streaming) = streaming {
            self.interrupted = true;
            let call = Call {
                id: streaming.id,
                code: streaming.code,
            };
            self.append(Entry::Step {
                at: UnixMs::now(),
                calls: vec![call.clone()],
                prose: String::new(),
                carry: Carry::bare(call),
                usage: Usage::default(),
            })
            .await?;
        }
        Ok(())
    }

    /// A piece of the call being written: its start opens a cell, its code
    /// feeds it.
    fn stream(&mut self, streaming: &mut Option<Streaming>, (id, code): (Option<CallId>, String)) {
        if let Some(id) = id
            && let Some(notebook) = &self.notebook
        {
            self.cell = Some(Latest {
                cell: notebook.stream(),
                call: Call {
                    id: id.clone(),
                    code: String::new(),
                },
                started_at: UnixMs::now(),
            });
            self.told_returned = false;
            *streaming = Some(Streaming {
                id,
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

    /// What a reader sees, built from the loop's own state.
    fn status(&self) -> AgentStatus {
        let kind = if self.responding {
            AgentStateKind::ApiStreaming {
                pending_response: PendingInferenceResponse::default(),
                previous_attempt: None,
            }
        } else if let Some(Stopped::Failed(error)) = &self.stopped {
            AgentStateKind::Error(FailedInferenceResponse {
                partial_response: PendingInferenceResponse::default(),
                attempt_count: NonZeroU64::MIN,
                error: Arc::new(error.to_string()),
            })
        } else if self.cell_running() && !self.awaiting && !self.archived {
            let latest = self.cell.as_ref().expect("running");
            let id = rho_inference::types::ExecId::try_from(latest.call.id.as_str().to_owned())
                .unwrap_or_else(|_| "exec".try_into().expect("valid id"));
            let preview = ToolPreview {
                call: ToolCall {
                    id: id.clone(),
                    name: ToolName::try_from("exec").expect("valid tool name"),
                    tool_type: ToolType::Custom,
                    arguments: latest.call.code.clone(),
                },
                started_at: latest.started_at,
                metadata: None,
            };
            AgentStateKind::ToolCalling {
                previews: BTreeMap::from([(id, preview)]),
                results: Vec::new(),
                waiting: None,
            }
        } else {
            AgentStateKind::Idle
        };
        AgentStatus {
            kind,
            queued: self.unread.len(),
        }
    }

    fn publish_sync(&mut self) {
        let status = self.status();
        self.working = status.kind.is_working();
        *self.status.write().expect("poison") = status;
        self.host.published();
    }

    /// Publish, and tell the log when the turn's edge moved: started when
    /// the agent begins working, ended when it hands back.
    async fn publish(&mut self, _recheck: Option<UnixMs>) -> anyhow::Result<()> {
        let status = self.status();
        let working = status.kind.is_working();
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
            self.host.turn(UnixMillis::now(), edge).await?;
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

/// The inference2 model a role's binding names, and the name its usage is
/// billed under. Credentials come from the agent host's account selection.
fn model(
    inference: &Inference,
    binding: crate::db::SessionBinding,
) -> anyhow::Result<(Model, String)> {
    let profile: InferenceProfile = binding
        .deep_config()
        .ok_or_else(|| anyhow::anyhow!("Rho runtime stored with a Claude mode"))?;
    let model: InferenceModel = binding
        .deep_model()
        .ok_or_else(|| anyhow::anyhow!("Rho runtime stored without a model"))?;
    let effort = match profile.effort {
        ReasoningEffort::Low => rho_inference2::openai::Effort::Low,
        ReasoningEffort::Medium => rho_inference2::openai::Effort::Medium,
        ReasoningEffort::High => rho_inference2::openai::Effort::High,
        ReasoningEffort::Xhigh => rho_inference2::openai::Effort::XHigh,
    };
    let accounts = inference.clone();
    let resolve_auth: rho_inference2::openai::AuthResolver = Arc::new(move |_| {
        let accounts = accounts.clone();
        Box::pin(async move {
            let auth = accounts.auth().await?;
            let resolved = accounts.resolve_auth(auth).await?;
            Ok(rho_inference::ResolvedOAuth {
                bearer_token: resolved.bearer_token,
                account_id: resolved.account_id,
            })
        })
    });
    let billed = match model {
        InferenceModel::Gpt6Astra => crate::db::AgentUsageModel::ASTRA,
        InferenceModel::Gpt6Luna => crate::db::AgentUsageModel::LUNA,
        InferenceModel::Gpt6Sol => crate::db::AgentUsageModel::GPT,
    };
    Ok((
        Model::OpenAiWithAuth {
            model: rho_inference2::openai::OpenAi {
                base_url: inference.responses_base_url().to_owned(),
                model: model.as_str().to_owned(),
                effort,
                fast: profile.fast_mode,
                auth: String::new(),
            },
            resolve_auth,
        },
        billed.name().to_owned(),
    ))
}

fn cache_key(key: rho_inference::PromptCacheKey) -> CacheKey {
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
    view: Arc<View>,
    role: AgentRole,
) -> anyhow::Result<crate::RenderedAgentSurface> {
    let binding = role.session_profile();
    if binding.claude_model().is_some() {
        return Ok(crate::RenderedAgentSurface {
            system_prompt: prompt::claude_prompt(Some(view.as_ref()), None, role),
            tools: Arc::from([rho_claude::mcp::exec_spec()]),
        });
    }
    binding
        .deep_config()
        .ok_or_else(|| anyhow::anyhow!("role has no inference profile"))?;
    Ok(crate::RenderedAgentSurface {
        system_prompt: prompt::prompt(&view, None, role),
        tools: Arc::from([rho_inference::exec::spec()]),
    })
}
