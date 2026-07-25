//! A small agent harness built around one question.
//!
//! The transcript is an append-only list of [`ContextBlock`]s. Several things
//! produce blocks — the model, tools, the user, peer agents — but only the
//! core appends, so the transcript has exactly one writer and a total order.
//!
//! Everything that produces blocks is a *source*. Sources accumulate on their
//! own and are **pulled** by the core at a moment the core picks, so several
//! sources merge into one request rather than waking one request each. That
//! makes the entire scheduler one question, asked after every event:
//!
//! > *Should the next request start now?*
//!
//! [`Agent::boundary`] answers it, and it is the only place in this crate that
//! exercises judgment. Everything else — spawning, persisting, draining,
//! publishing — is mechanism. The division of labour with tools follows from
//! it: **nothing outside the core decides *when*, and the core never decides
//! *what***. A tool reports what it has and how it is doing; the core weighs
//! that against everything else and picks the moment. The only thing the core
//! ever says back to a tool is [`ToolSession::cancel`] — wind down — and even
//! then it collects the tool's parting output, so the tool has the last word.
//!
//! There is deliberately no notion of a "turn", no foreground/background tool
//! distinction, and no stored status enum — each was a way of saying something
//! the question above already answers.

mod source;
mod store;
mod tool;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rho_core::{
    AgentId as PeerId, ContentPart, ContextBlock, InferenceEvent, InferenceRequest,
    InferenceResponseItem, PendingInferenceResponse, ProviderResponseId, ToolCall, ToolCallId,
    ToolName, ToolOutput, ToolOutputStatus, ToolResult, ToolSpec, UnixMs,
};
use rho_inference::config::{InferenceModel, InferenceProfile};
use rho_inference::{InferenceAuth, InferenceSession, PromptCacheKey};
use tokio::sync::{Notify, mpsc};

pub use crate::source::{
    Delivery, InputKind, InputSource, Preview, PreviewData, QueuePreview, QueuedInput, ToolPreview,
    UnknownPreviewData,
};
use crate::source::{MailSource, PendingSource, UserSource};
use crate::store::{AgentEvent, AgentRecord};
pub use crate::store::{AgentId, Store};
use crate::tool::{FinishedSession, RunningTool, ToolTake, lost_to_restart};
pub use crate::tool::{Rhythm, SourceWaker, Tool, ToolSession, ToolStatus, elide_middle};

/// Everything a UI needs, rebuilt whenever state changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentSnapshot {
    pub history: Vec<Arc<ContextBlock>>,
    /// Non-consuming view of what each source is holding, and since when.
    /// Readiness signals to the core carry no payload, so this is the only way
    /// to show pending content before it is pulled.
    pub previews: Vec<Preview>,
    /// Cancelled, and waiting for fresh input before doing anything else.
    pub stopped: bool,
    /// Model output for the in-flight request. Provisional: a failure drops it
    /// without touching history.
    pub streaming: Option<PendingInferenceResponse>,
    pub context_used: Option<u64>,
    pub last_error: Option<Arc<str>>,
    pub quota: Option<QuotaObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaObservation {
    pub observed_at: UnixMs,
    pub used_percent: u8,
    pub reset_at_unix: Option<i64>,
}

/// The tools an agent may call.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<ToolName, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: impl Tool) -> &mut Self {
        let spec = tool.spec();
        self.tools.insert(spec.name, Arc::new(tool));
        self
    }

    pub fn specs(&self) -> Arc<[ToolSpec]> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    fn get(&self, name: &ToolName) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Cheap clonable handle for observing and driving a running agent.
#[derive(Clone)]
pub struct AgentHandle {
    id: AgentId,
    control: mpsc::UnboundedSender<Control>,
    snapshot: Arc<RwLock<AgentSnapshot>>,
    notify: Arc<Notify>,
}

impl AgentHandle {
    pub async fn create(
        store: Store,
        auth: InferenceAuth,
        profile: InferenceProfile,
        model: InferenceModel,
        registry: ToolRegistry,
        instructions: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let record = AgentRecord {
            instructions: instructions.into(),
            profile,
            model: model.into(),
            prompt_cache_key: PromptCacheKey::generate(),
            next_event: 0,
        };
        let id = store.create_record(&record).await;
        Ok(Agent::start(
            store,
            auth,
            id,
            record,
            registry,
            Restored::default(),
        ))
    }

    pub fn load(
        store: Store,
        auth: InferenceAuth,
        registry: ToolRegistry,
        id: AgentId,
    ) -> anyhow::Result<Self> {
        let (record, events) = store
            .load(id)
            .ok_or_else(|| anyhow::anyhow!("rho-agent2 agent not found"))?;
        Ok(Agent::start(
            store,
            auth,
            id,
            record,
            registry,
            restore(events),
        ))
    }

    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn snapshot(&self) -> AgentSnapshot {
        self.snapshot.read().expect("poisoned snapshot").clone()
    }

    pub fn send_user_message(&self, text: impl Into<String>, delivery: Delivery) {
        self.send_input(
            InputKind::Message {
                content: vec![ContentPart::Text { text: text.into() }],
            },
            delivery,
        );
    }

    pub fn compact(&self) {
        self.send_input(InputKind::Compaction, Delivery::NextRequest);
    }

    fn send_input(&self, kind: InputKind, delivery: Delivery) {
        let _ = self.control.send(Control::User(QueuedInput {
            source: InputSource::User,
            kind,
            delivery,
            at: UnixMs::now(),
        }));
    }

    pub fn send_mail(&self, peer: PeerId, text: impl Into<String>) {
        let _ = self.control.send(Control::Mail {
            peer,
            content: vec![ContentPart::Text { text: text.into() }],
            at: UnixMs::now(),
        });
    }

    /// Abort the in-flight request and durably discard queued inputs.
    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }

    /// Retry after a failure, or resume a request interrupted by a restart.
    pub fn retry(&self) {
        let _ = self.control.send(Control::Retry);
    }

    /// An immediate snapshot, then every subsequent change.
    pub fn subscribe(&self) -> impl futures::Stream<Item = AgentSnapshot> + use<> {
        let snapshot = Arc::clone(&self.snapshot);
        let notify = Arc::clone(&self.notify);
        async_stream::stream! {
            loop {
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let current = snapshot.read().expect("poisoned snapshot").clone();
                yield current;
                notified.await;
            }
        }
    }
}

enum Control {
    User(QueuedInput),
    Mail {
        peer: PeerId,
        content: Vec<ContentPart>,
        at: UnixMs,
    },
    Cancel,
    Retry,
}

/// Everything that can move the agent. The `select!` normalises sources into
/// one of these and does nothing else; all judgment lives in [`Agent::handle`]
/// and [`Agent::boundary`].
enum Event {
    Control(Control),
    Inference(InferenceEvent),
    /// A tool says something about it changed. Deliberately carries no
    /// payload: the core asks the sources what they hold when it decides to.
    SourceChanged,
    /// A rhythm deadline expired; re-ask the question.
    Tick,
}

/// The in-flight request. Provisional until it finishes: a failure drops the
/// whole thing without touching history.
struct Inference {
    pending: PendingInferenceResponse,
    temporary_failures: u64,
}

/// A standing instruction that overrides the ordinary rhythm rules. The two
/// overrides are opposites, so they cannot both be in force.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Standing {
    /// Follow the rhythms.
    #[default]
    Normal,
    /// Send at the next opportunity even with nothing pending: a retry, a
    /// restart resume, or carrying on after a compaction.
    MustSend,
    /// Cancelled. Tool output still reaches history at the next boundary, but
    /// nothing may *start* a request until fresh user input arrives —
    /// otherwise a cancelled tool's dying words would wake the agent straight
    /// back up.
    Halted,
}

/// Whether the next request starts now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    /// Not yet. Wait for more events, or for a rhythm deadline.
    No,
    /// Drain every source and send.
    Now,
    /// Throw away the in-flight request first.
    AbortNow,
}

struct Agent {
    store: Store,
    id: AgentId,
    next_event: u64,
    instructions: Arc<str>,

    /// The transcript. Append-only, and this struct is its sole writer.
    history: Vec<Arc<ContextBlock>>,

    session: InferenceSession,
    inference: Option<Inference>,

    user: UserSource,
    mail: BTreeMap<PeerId, MailSource>,
    tools: BTreeMap<ToolCallId, RunningTool>,

    registry: ToolRegistry,

    /// When the model last spoke. `max_hold` is measured from here, because
    /// impatience is a property of the conversation's cadence rather than of
    /// any single pending item.
    last_response_at: UnixMs,
    context_used: Option<u64>,
    standing: Standing,
    auto_compaction_in_flight: bool,
    last_error: Option<Arc<str>>,
    quota: Option<QuotaObservation>,
    /// Content-free signal that some tool changed. Tools hold a
    /// [`SourceWaker`] over this; the core rescans rather than being told.
    wake: Arc<Notify>,

    snapshot: Arc<RwLock<AgentSnapshot>>,
    notify: Arc<Notify>,
    control_rx: mpsc::UnboundedReceiver<Control>,
}

impl Agent {
    fn start(
        store: Store,
        auth: InferenceAuth,
        id: AgentId,
        record: AgentRecord,
        registry: ToolRegistry,
        restored: Restored,
    ) -> AgentHandle {
        let session = InferenceSession::new_deep(
            auth,
            record.profile,
            record.model.into(),
            record.prompt_cache_key,
        );
        let snapshot = Arc::new(RwLock::new(AgentSnapshot::default()));
        let notify = Arc::new(Notify::new());
        let (control, control_rx) = mpsc::unbounded_channel();

        let mut agent = Self {
            store,
            id,
            next_event: record.next_event,
            instructions: Arc::from(record.instructions),
            history: restored.history,
            session,
            inference: None,
            user: restored.user,
            mail: restored.mail,
            tools: BTreeMap::new(),
            registry,
            last_response_at: UnixMs::now(),
            context_used: restored.context_used,
            standing: if restored.request_active {
                Standing::MustSend
            } else {
                Standing::Normal
            },
            auto_compaction_in_flight: false,
            last_error: None,
            quota: None,
            wake: Arc::new(Notify::new()),
            snapshot: Arc::clone(&snapshot),
            notify: Arc::clone(&notify),
            control_rx,
        };

        tokio::spawn(async move {
            agent.recover_lost_tools(restored.orphan_tools).await;
            agent.publish();
            agent.run().await;
        });

        AgentHandle {
            id,
            control,
            snapshot,
            notify,
        }
    }

    async fn run(mut self) {
        loop {
            let deadline = self.next_deadline(UnixMs::now());
            let event = {
                let Self {
                    control_rx,
                    session,
                    wake,
                    ..
                } = &mut self;
                next_event(control_rx, session, wake, deadline).await
            };
            let Some(event) = event else { return };
            self.handle(event).await;
            self.publish();
        }
    }

    /// The single funnel. Every event updates state, then asks the one
    /// question.
    async fn handle(&mut self, event: Event) {
        let now = UnixMs::now();
        match event {
            Event::Control(Control::User(input)) => {
                self.persist(AgentEvent::Queued(input.clone())).await;
                self.user.push(input);
                // Fresh instructions revive a cancelled agent; peer mail alone
                // does not, so a cancel stays a cancel.
                if self.standing == Standing::Halted {
                    self.standing = Standing::Normal;
                }
            }
            Event::Control(Control::Mail { peer, content, at }) => {
                let input = QueuedInput {
                    source: InputSource::Mail { peer },
                    kind: InputKind::Message {
                        content: content.clone(),
                    },
                    delivery: Delivery::NextRequest,
                    at,
                };
                self.persist(AgentEvent::Queued(input)).await;
                self.mail
                    .entry(peer)
                    .or_insert_with(|| MailSource::new(peer, at))
                    .push(content, at);
            }
            Event::Control(Control::Cancel) => self.cancel(now).await,
            Event::Control(Control::Retry) => {
                self.last_error = None;
                self.standing = Standing::MustSend;
            }
            Event::Inference(event) => self.on_inference(event, now).await,
            // Both are pure prompts to re-ask the question; what a source
            // reports is read live, so there is nothing to record here.
            Event::SourceChanged | Event::Tick => {}
        }
        self.maybe_request().await;
    }

    // -- the one decision ---------------------------------------------------

    fn schedule(&self) -> Schedule {
        Schedule {
            pending: self.pending_sources(),
            inference_active: self.inference.is_some(),
            wants_interrupt: self.user.wants_interrupt(),
            standing: self.standing,
            last_response_at: self.last_response_at,
        }
    }

    fn boundary(&self, now: UnixMs) -> Boundary {
        self.schedule().boundary(now)
    }

    fn next_deadline(&self, now: UnixMs) -> Option<UnixMs> {
        self.schedule().next_deadline(now)
    }

    fn pending_sources(&self) -> Vec<PendingSource> {
        let mut sources: Vec<PendingSource> = Vec::new();
        sources.extend(self.user.pending_source());
        sources.extend(self.mail.values().filter_map(MailSource::pending_source));
        sources.extend(
            self.tools
                .values()
                .filter(|tool| tool.pending())
                .map(|tool| {
                    let status = tool.status();
                    if status.exited {
                        PendingSource::done(tool.rhythm)
                    } else {
                        PendingSource::talking(tool.rhythm, status.last_output_at)
                    }
                }),
        );
        sources
    }

    // -- acting on it -------------------------------------------------------

    async fn maybe_request(&mut self) {
        let now = UnixMs::now();
        if self.boundary(now) == Boundary::AbortNow {
            self.session.abort();
            self.inference = None;
            self.auto_compaction_in_flight = false;
            self.persist(AgentEvent::RequestEnded { context_used: None })
                .await;
        }
        if self.boundary(now) == Boundary::Now {
            self.start_request(now).await;
        }
    }

    async fn start_request(&mut self, now: UnixMs) {
        let mut blocks = self.drain(now);

        // Automatic compaction is not an input — it is something the core does
        // while assembling a request. (A user-requested compaction *is* an
        // input, and arrives through the user queue.)
        let over_limit = self
            .session
            .auto_compact_token_limit()
            .zip(self.context_used)
            .is_some_and(|(limit, used)| used >= limit);
        let compact = over_limit
            && !latest_request_has_compaction_trigger(&self.history)
            && !blocks.contains(&ContextBlock::CompactionTrigger);
        if compact {
            blocks.push(ContextBlock::CompactionTrigger);
        }
        self.auto_compaction_in_flight = compact;

        if !blocks.is_empty() {
            self.persist(AgentEvent::Appended {
                blocks: Cow::Borrowed(&blocks),
                drained: true,
            })
            .await;
            self.history.extend(blocks.into_iter().map(Arc::new));
        }
        self.reap_tools().await;

        self.persist(AgentEvent::RequestStarted).await;
        self.session.request(InferenceRequest {
            instructions: Arc::clone(&self.instructions),
            input: self.history.clone(),
            agent_id_labels: Default::default(),
            tools: self.registry.specs(),
        });
        self.inference = Some(Inference {
            pending: PendingInferenceResponse::default(),
            temporary_failures: 0,
        });
        self.standing = Standing::Normal;
    }

    /// Pull from *every* source, not just whichever one triggered the
    /// boundary. Batching them into one request is the whole point of pulling.
    ///
    /// Order is protocol-constrained rather than chronological: a provider
    /// wants each `ToolCall` answered adjacent to the call, so tool output
    /// leads.
    fn drain(&mut self, now: UnixMs) -> Vec<ContextBlock> {
        let mut blocks = Vec::new();

        let mut results: Vec<ToolResult> = Vec::new();
        let mut updates = Vec::new();
        for tool in self.tools.values_mut() {
            match tool.take(now) {
                Some(ToolTake::Result(result)) => results.push(result),
                Some(ToolTake::Update(update)) => updates.push(ContextBlock::ToolUpdate(update)),
                None => {}
            }
        }
        if !results.is_empty() {
            blocks.push(ContextBlock::ToolResults { results });
        }
        blocks.extend(updates);

        blocks.extend(self.mail.values_mut().filter_map(MailSource::take));
        blocks.extend(self.user.take());
        blocks
    }

    /// Forget tools that exited and whose output the model has seen, so a long
    /// session does not accumulate them forever.
    async fn reap_tools(&mut self) {
        let done: Vec<ToolCallId> = self
            .tools
            .iter()
            .filter(|(_, tool)| tool.reapable())
            .map(|(id, _)| id.clone())
            .collect();
        for call_id in done {
            self.persist(AgentEvent::ToolReaped {
                call_id: call_id.clone(),
            })
            .await;
            self.tools.remove(&call_id);
        }
    }

    async fn cancel(&mut self, now: UnixMs) {
        // Ask every tool to wind down, then keep reading it: the core does not
        // kill tools, so a tool still chooses its own last words. Those words
        // reach history at the next boundary, but `stopped` makes sure they
        // cannot themselves *cause* a boundary.
        for tool in self.tools.values_mut() {
            tool.cancel();
        }
        self.standing = Standing::Halted;
        self.session.abort();
        self.inference = None;
        self.auto_compaction_in_flight = false;
        self.persist(AgentEvent::RequestEnded { context_used: None })
            .await;
        if !self.user.is_empty() || self.mail.values().any(|mail| !mail.is_empty()) {
            self.persist(AgentEvent::QueueCleared).await;
            self.user.clear();
            for mail in self.mail.values_mut() {
                mail.clear();
            }
        }
        let _ = now;
    }

    // -- inference ----------------------------------------------------------

    async fn on_inference(&mut self, event: InferenceEvent, now: UnixMs) {
        let Some(inference) = self.inference.as_mut() else {
            return;
        };
        match event {
            InferenceEvent::RequestSent | InferenceEvent::StreamingStarted => {}
            InferenceEvent::Quota {
                used_percent,
                reset_at_unix,
            } => {
                self.quota = Some(QuotaObservation {
                    observed_at: now,
                    used_percent,
                    reset_at_unix,
                });
            }
            InferenceEvent::ContextItem { index, event } => inference.pending.apply(index, event),
            InferenceEvent::TemporaryFailure { error, .. } => {
                inference.temporary_failures += 1;
                self.last_error = Some(Arc::from(error.to_string()));
                // The retry starts a fresh response; drop the partial one.
                inference.pending = PendingInferenceResponse::default();
            }
            InferenceEvent::Failed { error } => {
                self.last_error = Some(Arc::from(error.to_string()));
                self.fail_request().await;
            }
            InferenceEvent::Finished {
                usage,
                provider_response_id,
            } => {
                let items = inference.pending.finish();
                match items {
                    Err(error) => {
                        self.last_error = Some(Arc::from(error.to_string()));
                        self.fail_request().await;
                    }
                    Ok(items) => {
                        self.finish_request(items, provider_response_id, usage, now)
                            .await
                    }
                }
            }
        }
    }

    async fn fail_request(&mut self) {
        self.inference = None;
        self.auto_compaction_in_flight = false;
        self.persist(AgentEvent::RequestEnded { context_used: None })
            .await;
    }

    async fn finish_request(
        &mut self,
        items: Vec<InferenceResponseItem>,
        provider_response_id: Option<ProviderResponseId>,
        usage: Option<rho_core::TokenUsage>,
        now: UnixMs,
    ) {
        let compacted = items
            .iter()
            .any(|item| matches!(item, InferenceResponseItem::Compaction { .. }));
        let context_used = if compacted {
            None
        } else {
            usage
                .map(|usage| usage.input_tokens + usage.output_tokens)
                .or(self.context_used)
        };
        self.context_used = context_used;

        let calls: Vec<ToolCall> = items
            .iter()
            .filter_map(|item| match item {
                InferenceResponseItem::ToolCall {
                    id,
                    name,
                    tool_type,
                    arguments,
                    ..
                } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    tool_type: *tool_type,
                    arguments: arguments.clone(),
                }),
                _ => None,
            })
            .collect();

        let block = ContextBlock::InferenceResponse {
            items,
            provider_response_id,
        };
        self.persist(AgentEvent::Appended {
            blocks: Cow::Borrowed(std::slice::from_ref(&block)),
            drained: false,
        })
        .await;
        self.history.push(Arc::new(block));

        self.inference = None;
        self.last_error = None;
        self.last_response_at = now;
        self.persist(AgentEvent::RequestEnded { context_used })
            .await;

        for call in calls {
            self.spawn_tool(call, now).await;
        }

        // A compaction the core asked for is a means, not an end: carry on
        // with the work that triggered it.
        if compacted && std::mem::take(&mut self.auto_compaction_in_flight) {
            self.standing = Standing::MustSend;
        }
    }

    // -- tools --------------------------------------------------------------

    async fn spawn_tool(&mut self, call: ToolCall, now: UnixMs) {
        self.persist(AgentEvent::ToolSpawned {
            call: Cow::Borrowed(&call),
        })
        .await;

        let (rhythm, session) = match self.registry.get(&call.name) {
            Some(tool) => (
                tool.rhythm(),
                tool.run(call.clone(), SourceWaker::new(Arc::clone(&self.wake))),
            ),
            // Born exited: the error reaches the model at the next boundary
            // through exactly the same path as any other tool output.
            None => (
                Rhythm::TOOL,
                FinishedSession::boxed(
                    ToolOutput {
                        output: Arc::new(format!("unknown tool: {}", call.name.as_str())),
                        status: ToolOutputStatus::Error,
                    },
                    now,
                ),
            ),
        };
        self.tools.insert(
            call.id.clone(),
            RunningTool::new(call, rhythm, session, now),
        );
    }

    /// Tools do not survive a restart. Say so, rather than leaving their calls
    /// unanswered.
    async fn recover_lost_tools(&mut self, orphans: Vec<ToolCall>) {
        if orphans.is_empty() {
            return;
        }
        let now = UnixMs::now();
        let blocks: Vec<ContextBlock> = orphans
            .iter()
            .map(|call| lost_to_restart(call, answered(&self.history, &call.id), now))
            .collect();
        self.persist(AgentEvent::Appended {
            blocks: Cow::Borrowed(&blocks),
            drained: false,
        })
        .await;
        self.history.extend(blocks.into_iter().map(Arc::new));
        for call in orphans {
            self.persist(AgentEvent::ToolReaped { call_id: call.id })
                .await;
        }
    }

    // -- plumbing -----------------------------------------------------------

    async fn persist(&mut self, event: AgentEvent<'_>) {
        self.store.append(self.id, self.next_event, &event).await;
        self.next_event += 1;
    }

    fn publish(&self) {
        let mut previews = Vec::new();
        if let Some(since) = self.user.oldest() {
            previews.push(Preview {
                label: Cow::Borrowed("user"),
                data: Box::new(QueuePreview {
                    pending: self.user.len() as u32,
                    since,
                }),
            });
        }
        for (peer, mail) in &self.mail {
            if !mail.is_empty() {
                previews.push(Preview {
                    label: Cow::Owned(format!("mail:{}", peer.encoded())),
                    data: Box::new(mail.preview()),
                });
            }
        }
        for (call_id, tool) in &self.tools {
            previews.push(Preview {
                label: Cow::Owned(format!("tool:{}", call_id.as_str())),
                data: tool.session.preview(),
            });
        }

        *self.snapshot.write().expect("poisoned snapshot") = AgentSnapshot {
            history: self.history.clone(),
            previews,
            stopped: self.standing == Standing::Halted,
            streaming: self
                .inference
                .as_ref()
                .map(|inference| inference.pending.clone()),
            context_used: self.context_used,
            last_error: self.last_error.clone(),
            quota: self.quota.clone(),
        };
        self.notify.notify_waiters();
    }
}

/// Everything [`Schedule::boundary`] is allowed to look at.
///
/// Split out from [`Agent`] so the one decision in this crate can be exercised
/// against a fabricated clock and fabricated sources, with no provider, no
/// database, and no tasks.
struct Schedule {
    pending: Vec<PendingSource>,
    inference_active: bool,
    wants_interrupt: bool,
    standing: Standing,
    last_response_at: UnixMs,
}

impl Schedule {
    /// Should the next request start now?
    ///
    /// Three rules, and nothing else in this crate decides anything:
    ///
    /// 1. An `Interrupt` message is worth throwing away an in-flight request.
    /// 2. Otherwise go once every pending source has settled — nobody has more
    ///    to say, so waiting cannot improve the request.
    /// 3. Otherwise go at the earliest `max_hold` among pending sources. The
    ///    most impatient source sets the deadline, and every source yields into
    ///    that same request.
    fn boundary(&self, now: UnixMs) -> Boundary {
        if self.standing == Standing::Halted {
            return Boundary::No;
        }
        if self.inference_active {
            return if self.wants_interrupt {
                Boundary::AbortNow
            } else {
                Boundary::No
            };
        }
        if self.standing == Standing::MustSend {
            return Boundary::Now;
        }
        if self.pending.is_empty() {
            return Boundary::No;
        }
        if self.pending.iter().all(|source| source.settled(now)) {
            return Boundary::Now;
        }
        if now >= self.hold_deadline().expect("pending is non-empty") {
            Boundary::Now
        } else {
            Boundary::No
        }
    }

    /// The earliest instant at which [`Schedule::boundary`] could change
    /// answer, so the loop knows how long it may sleep.
    fn next_deadline(&self, now: UnixMs) -> Option<UnixMs> {
        // Nothing to wait for: the answer is already settled either way.
        if self.inference_active || self.standing != Standing::Normal || self.pending.is_empty() {
            return None;
        }
        self.pending
            .iter()
            .filter_map(|source| source.quiet_deadline())
            .chain(self.hold_deadline())
            .filter(|deadline| *deadline > now)
            .min()
            // Every deadline has already passed: wake immediately rather than
            // sleeping through a boundary.
            .or(Some(now))
    }

    fn hold_deadline(&self) -> Option<UnixMs> {
        self.pending
            .iter()
            .map(|source| source.hold_deadline(self.last_response_at))
            .min()
    }
}

/// Normalise every source into one [`Event`]. No policy lives here.
async fn next_event(
    control_rx: &mut mpsc::UnboundedReceiver<Control>,
    session: &mut InferenceSession,
    wake: &Notify,
    deadline: Option<UnixMs>,
) -> Option<Event> {
    // Disabled `select!` arms still evaluate their expression, so give the
    // timer a zero duration when nothing is armed; the guard keeps it unpolled.
    let sleep = Duration::from_millis(
        deadline
            .map(|deadline| deadline.0.saturating_sub(UnixMs::now().0))
            .unwrap_or(0),
    );
    tokio::select! {
        biased;
        control = control_rx.recv() => control.map(Event::Control),
        event = session.run() => Some(Event::Inference(event)),
        _ = wake.notified() => Some(Event::SourceChanged),
        _ = tokio::time::sleep(sleep), if deadline.is_some() => Some(Event::Tick),
    }
}

fn latest_request_has_compaction_trigger(history: &[Arc<ContextBlock>]) -> bool {
    history
        .iter()
        .rev()
        .find_map(|block| match &**block {
            ContextBlock::CompactionTrigger => Some(true),
            ContextBlock::InferenceResponse { .. } => Some(false),
            ContextBlock::UserMessage { .. }
            | ContextBlock::ToolResults { .. }
            | ContextBlock::ToolUpdate(_) => None,
        })
        .unwrap_or(false)
}

fn answered(history: &[Arc<ContextBlock>], call_id: &ToolCallId) -> bool {
    history.iter().any(|block| match &**block {
        ContextBlock::ToolResults { results } => {
            results.iter().any(|result| result.call_id == *call_id)
        }
        _ => false,
    })
}

#[derive(Default)]
struct Restored {
    history: Vec<Arc<ContextBlock>>,
    user: UserSource,
    mail: BTreeMap<PeerId, MailSource>,
    request_active: bool,
    context_used: Option<u64>,
    /// Tools that were running when the process stopped.
    orphan_tools: Vec<ToolCall>,
}

impl Restored {
    fn clear_queues(&mut self) {
        self.user.clear();
        for mail in self.mail.values_mut() {
            mail.clear();
        }
    }
}

fn restore(events: Vec<AgentEvent<'static>>) -> Restored {
    let mut restored = Restored::default();
    let mut live_tools: BTreeMap<ToolCallId, ToolCall> = BTreeMap::new();
    for event in events {
        match event {
            AgentEvent::Queued(input) => match input.source {
                InputSource::User => restored.user.push(input),
                InputSource::Mail { peer } => {
                    let InputKind::Message { content } = input.kind else {
                        continue;
                    };
                    restored
                        .mail
                        .entry(peer)
                        .or_insert_with(|| MailSource::new(peer, input.at))
                        .push(content, input.at);
                }
            },
            AgentEvent::Appended { blocks, drained } => {
                restored
                    .history
                    .extend(blocks.into_owned().into_iter().map(Arc::new));
                if drained {
                    restored.clear_queues();
                }
            }
            AgentEvent::QueueCleared => restored.clear_queues(),
            AgentEvent::RequestStarted => restored.request_active = true,
            AgentEvent::RequestEnded { context_used } => {
                restored.request_active = false;
                if context_used.is_some() {
                    restored.context_used = context_used;
                }
            }
            AgentEvent::ToolSpawned { call } => {
                let call = call.into_owned();
                live_tools.insert(call.id.clone(), call);
            }
            AgentEvent::ToolReaped { call_id } => {
                live_tools.remove(&call_id);
            }
        }
    }
    restored.orphan_tools = live_tools.into_values().collect();
    restored
}

#[cfg(test)]
mod tests;
