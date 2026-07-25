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
//! There is deliberately no stored status enum, and no foreground/background
//! tool distinction to declare — at the moment of the call `npm test` and
//! `npm run dev` are the same call, and nothing could tell them apart. So the
//! core does not try. A running tool has no urgency of its own; the pace at
//! which its output reaches the model is the model's own, stated by what its
//! last turn did and revised every turn. What a tool *can* say is that it
//! ended, or that what it holds stands alone, and those are worth waking
//! somebody for whatever else is going on.

mod preview;
mod source;
mod store;
mod tool;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rho_core::{
    AgentId, ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    PendingInferenceResponse, ProviderResponseId, ToolCall, ToolCallId, ToolName, ToolOutput,
    ToolOutputStatus, ToolResult, ToolSpec, UnixMs,
};
use rho_inference::config::{InferenceModel, InferenceProfile};
use rho_inference::{Inference, InferenceSession, PromptCacheKey};
use tokio::sync::{Notify, mpsc, oneshot};

pub use crate::preview::{
    MailPreview, PendingItem, PreviewData, ToolPreview, UnknownPreviewData, UserPreview,
};
pub use crate::source::{Delivery, InputKind, InputSource, QueuedInput};
use crate::source::{MailSource, SourceKind, UserSource};
use crate::store::{AgentEvent, AgentRecord};
pub use crate::store::{AgentKey, Store};
use crate::tool::{FinishedSession, RunningTool, Told, ToolTake, lost_to_restart};
pub use crate::tool::{
    SourceWaker, Tool, ToolActivity, ToolSession, ToolStatus, Unsent, elide_middle,
};

/// Everything a UI needs, rebuilt whenever state changes.
///
/// Quota is deliberately absent: it is an account-wide fact the agent neither
/// uses nor owns, so a UI reads it from [`rho_inference::quota`] directly.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentSnapshot {
    pub history: Vec<Arc<ContextBlock>>,
    /// Non-consuming view of what each source is holding, and since when.
    /// Readiness signals to the core carry no payload, so this is the only way
    /// to show pending content before it is pulled.
    pub previews: Vec<Box<dyn PreviewData>>,
    pub activity: AgentActivity,
    /// Model output for the in-flight request. Provisional: a failure drops it
    /// without touching history.
    pub streaming: Option<PendingInferenceResponse>,
    pub context_used: Option<u64>,
    pub last_error: Option<Arc<str>>,
}

/// Whether the agent will act on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentActivity {
    /// Working, or waiting on something that is coming.
    #[default]
    Live,
    /// Cancelled: nothing will start a request until fresh user input arrives.
    Stopped,
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
    id: AgentKey,
    control: mpsc::UnboundedSender<Control>,
    snapshot: Arc<RwLock<AgentSnapshot>>,
    notify: Arc<Notify>,
}

impl AgentHandle {
    pub async fn create(
        store: Store,
        inference: Inference,
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
            inference,
            id,
            record,
            registry,
            Restored::default(),
        ))
    }

    pub fn load(
        store: Store,
        inference: Inference,
        registry: ToolRegistry,
        id: AgentKey,
    ) -> anyhow::Result<Self> {
        let (record, events) = store
            .load(id)
            .ok_or_else(|| anyhow::anyhow!("rho-agent2 agent not found"))?;
        Ok(Agent::start(
            store,
            inference,
            id,
            record,
            registry,
            restore(events),
        ))
    }

    pub fn id(&self) -> AgentKey {
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

    /// Deliver mail from a peer agent.
    ///
    /// The returned [`Queued`] resolves once the message is durably recorded,
    /// so a sender can wait for that rather than assume it. Dropping it sends
    /// the mail all the same.
    pub fn send_mail(&self, sender: AgentId, text: impl Into<String>) -> Queued {
        let (queued, receiver) = oneshot::channel();
        let _ = self.control.send(Control::Mail {
            sender,
            content: vec![ContentPart::Text { text: text.into() }],
            at: UnixMs::now(),
            queued,
        });
        Queued(receiver)
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

/// Resolves once a message has been durably recorded.
pub struct Queued(oneshot::Receiver<()>);

impl Queued {
    /// `false` if the agent stopped before the message could be recorded.
    pub async fn recorded(self) -> bool {
        self.0.await.is_ok()
    }
}

enum Control {
    User(QueuedInput),
    Mail {
        sender: AgentId,
        content: Vec<ContentPart>,
        at: UnixMs,
        queued: oneshot::Sender<()>,
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
struct InFlight {
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
    /// Not yet. `recheck` is the earliest instant at which the answer could
    /// change by itself; `None` means only a new event can change it. Carrying
    /// it here means the loop cannot arm a timer that disagrees with the
    /// decision, because the decision handed it the timer.
    No { recheck: Option<UnixMs> },
    /// Drain every source and send.
    Now,
    /// Throw away the in-flight request first.
    AbortNow,
}

struct Agent {
    store: Store,
    id: AgentKey,
    next_event: u64,
    instructions: Arc<str>,

    /// The transcript. Append-only, and this struct is its sole writer.
    history: Vec<Arc<ContextBlock>>,

    session: InferenceSession,
    in_flight: Option<InFlight>,

    user: UserSource,
    mail: BTreeMap<AgentId, MailSource>,
    tools: BTreeMap<ToolCallId, RunningTool>,
    /// Calls left unanswered by a restart, waiting for the next request to
    /// admit that their tools are gone.
    lost_tools: Vec<ToolCall>,

    registry: ToolRegistry,

    context_used: Option<u64>,
    /// What the model's latest turn settled about being looked in on. `None`
    /// until it has spoken once — after a restart included, which is safe
    /// because no tool survives one.
    turn: Option<Turn>,
    standing: Standing,
    /// The request in flight was compacting on behalf of work that still owes
    /// a reply, so a compaction must not be where the agent stops.
    compaction_owes_reply: bool,
    last_error: Option<Arc<str>>,
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
        inference: Inference,
        id: AgentKey,
        record: AgentRecord,
        registry: ToolRegistry,
        restored: Restored,
    ) -> AgentHandle {
        let session =
            inference.deep_session(record.profile, record.model.into(), record.prompt_cache_key);
        let snapshot = Arc::new(RwLock::new(AgentSnapshot::default()));
        let notify = Arc::new(Notify::new());
        let (control, control_rx) = mpsc::unbounded_channel();

        let agent = Self {
            store,
            id,
            next_event: record.next_event,
            instructions: Arc::from(record.instructions),
            history: restored.history,
            session,
            in_flight: None,
            user: restored.user,
            mail: restored.mail,
            tools: BTreeMap::new(),
            lost_tools: restored.orphan_tools,
            registry,
            context_used: restored.context_used,
            turn: None,
            standing: if restored.request_active {
                Standing::MustSend
            } else {
                Standing::Normal
            },
            compaction_owes_reply: false,
            last_error: None,
            wake: Arc::new(Notify::new()),
            snapshot: Arc::clone(&snapshot),
            notify: Arc::clone(&notify),
            control_rx,
        };

        tokio::spawn(agent.run());

        AgentHandle {
            id,
            control,
            snapshot,
            notify,
        }
    }

    /// Publish the starting state, then answer the one question after every
    /// event until the last handle is dropped.
    async fn run(mut self) {
        self.publish();
        loop {
            // The decision itself says when it might change, so the timer and
            // the rule behind it cannot drift apart.
            let deadline = match self.boundary(UnixMs::now()) {
                Boundary::No { recheck } => recheck,
                Boundary::Now | Boundary::AbortNow => None,
            };
            // Disabled `select!` arms still evaluate their expression, so give
            // the timer a zero duration when nothing is armed; the guard keeps
            // it unpolled.
            let sleep = Duration::from_millis(
                deadline
                    .map(|deadline| deadline.0.saturating_sub(UnixMs::now().0))
                    .unwrap_or(0),
            );
            let event = {
                let Self {
                    control_rx,
                    session,
                    wake,
                    ..
                } = &mut self;
                // Normalising sources into one Event is all that happens here;
                // no policy, because policy is `boundary` and nowhere else.
                tokio::select! {
                    biased;
                    control = control_rx.recv() => control.map(Event::Control),
                    event = session.run() => Some(Event::Inference(event)),
                    _ = wake.notified() => Some(Event::SourceChanged),
                    _ = tokio::time::sleep(sleep), if deadline.is_some() => Some(Event::Tick),
                }
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
            Event::Control(Control::Mail {
                sender,
                content,
                at,
                queued,
            }) => {
                let input = QueuedInput {
                    source: InputSource::Mail { sender },
                    kind: InputKind::Message {
                        content: content.clone(),
                    },
                    delivery: Delivery::NextRequest,
                    at,
                };
                self.persist(AgentEvent::Queued(input)).await;
                // Answered once the record is durable, not once it is read: a
                // sender wants to know the message cannot be lost, and waiting
                // for the model to see it would be waiting on a boundary the
                // sender does not control.
                let _ = queued.send(());
                self.mail
                    .entry(sender)
                    .or_insert_with(|| MailSource::new(sender))
                    .push(content, at);
            }
            Event::Control(Control::Cancel) => self.cancel().await,
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

    fn boundary(&self, now: UnixMs) -> Boundary {
        boundary(
            &self.sources(),
            self.turn,
            self.in_flight.is_some(),
            self.standing,
            now,
        )
    }

    /// Every source, in whatever state it is in — nothing is filtered out for
    /// having nothing to say, because deciding that is the decision's job.
    fn sources(&self) -> Vec<SourceKind> {
        let mut sources = vec![self.user.source()];
        sources.extend(self.mail.values().map(MailSource::source));
        sources.extend(self.tools.values().map(RunningTool::source));
        sources
    }

    // -- acting on it -------------------------------------------------------

    async fn maybe_request(&mut self) {
        let now = UnixMs::now();
        if self.boundary(now) == Boundary::AbortNow {
            self.session.abort();
            self.in_flight = None;
            self.compaction_owes_reply = false;
            self.persist(AgentEvent::RequestEnded { context_used: None })
                .await;
        }
        if self.boundary(now) == Boundary::Now {
            self.start_request(now).await;
        }
    }

    async fn start_request(&mut self, now: UnixMs) {
        // Tools do not survive a restart and their calls are still unanswered,
        // so the first request after one has to say so. Saying it here rather
        // than at load means an agent that is only opened and read is never
        // written to, and the note lands next to the request it explains.
        let lost = std::mem::take(&mut self.lost_tools);
        let mut blocks: Vec<ContextBlock> = lost
            .iter()
            .map(|call| lost_to_restart(call, result_sent(&self.history, &call.id), now))
            .collect();
        blocks.extend(self.drain(now));

        // Automatic compaction is not an input — it is something the core does
        // while assembling a request. (A user-requested compaction *is* an
        // input, and arrives through the user queue.)
        let over_limit = self
            .session
            .auto_compact_token_limit()
            .zip(self.context_used)
            .is_some_and(|(limit, used)| used >= limit);
        // A trigger can already be on the table two ways: this drain carries a
        // `/compact`, or an earlier request pushed one and never got its answer
        // because it failed.
        let compacting_already = blocks.contains(&ContextBlock::CompactionTrigger)
            || latest_request_has_compaction_trigger(&self.history);
        let compact = over_limit && !compacting_already;
        if compact {
            blocks.push(ContextBlock::CompactionTrigger);
        }
        self.compaction_owes_reply = compaction_owes_reply(compact, &blocks);

        if !blocks.is_empty() {
            self.persist(AgentEvent::Appended {
                blocks: Cow::Borrowed(&blocks),
                drained: true,
            })
            .await;
            self.history.extend(blocks.into_iter().map(Arc::new));
        }
        // Only once the note is durable: a crash in between must leave the call
        // still looking lost, not silently answered.
        for call in lost {
            self.persist(AgentEvent::ToolReaped { call_id: call.id })
                .await;
        }
        self.reap_tools().await;

        // review: might smart to vendor a AgentEvent if it is better
        self.persist(AgentEvent::RequestStarted).await;
        self.session.request(InferenceRequest {
            instructions: Arc::clone(&self.instructions),
            input: self.history.clone(),
            agent_id_labels: Default::default(),
            tools: self.registry.specs(),
        });
        self.in_flight = Some(InFlight {
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

    async fn cancel(&mut self) {
        // Ask every tool to wind down, then keep reading it: the core does not
        // kill tools, so a tool still chooses its own last words. Those words
        // reach history at the next boundary, but `stopped` makes sure they
        // cannot themselves *cause* a boundary.
        for tool in self.tools.values_mut() {
            tool.cancel();
        }
        self.standing = Standing::Halted;
        self.session.abort();
        self.in_flight = None;
        self.compaction_owes_reply = false;
        self.persist(AgentEvent::RequestEnded { context_used: None })
            .await;
        if !self.user.is_empty() || self.mail.values().any(|mail| !mail.is_empty()) {
            self.persist(AgentEvent::QueueCleared).await;
            self.user.clear();
            for mail in self.mail.values_mut() {
                mail.clear();
            }
        }
    }

    // -- inference ----------------------------------------------------------

    async fn on_inference(&mut self, event: InferenceEvent, now: UnixMs) {
        let Some(in_flight) = self.in_flight.as_mut() else {
            return;
        };
        match event {
            // Quota is account-wide and published by rho-inference itself, so
            // there is nothing here to carry.
            InferenceEvent::RequestSent
            | InferenceEvent::StreamingStarted
            | InferenceEvent::Quota { .. } => {}
            InferenceEvent::ContextItem { index, event } => in_flight.pending.apply(index, event),
            InferenceEvent::TemporaryFailure { error, .. } => {
                in_flight.temporary_failures += 1;
                self.last_error = Some(Arc::from(error.to_string()));
                // The retry starts a fresh response; drop the partial one.
                in_flight.pending = PendingInferenceResponse::default();
            }
            InferenceEvent::Failed { error } => {
                self.last_error = Some(Arc::from(error.to_string()));
                self.fail_request().await;
            }
            InferenceEvent::Finished {
                usage,
                provider_response_id,
            } => {
                let items = in_flight.pending.finish();
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
        self.in_flight = None;
        self.compaction_owes_reply = false;
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

        self.in_flight = None;
        self.last_error = None;
        self.persist(AgentEvent::RequestEnded { context_used })
            .await;

        // A turn that issues no calls still resets the check-in — the model has
        // just seen everything and is entitled to another interval before being
        // shown more — but it does not change what the model is waiting on.
        self.turn = Some(Turn {
            spoke_at: now,
            wait: None,
            calls_at: match calls.is_empty() {
                false => now,
                true => self.turn.map_or(now, |turn| turn.calls_at),
            },
        });

        for call in calls {
            self.spawn_tool(call, now).await;
        }

        if compacted && std::mem::take(&mut self.compaction_owes_reply) {
            self.standing = Standing::MustSend;
        }
    }

    // -- tools --------------------------------------------------------------

    async fn spawn_tool(&mut self, call: ToolCall, now: UnixMs) {
        self.persist(AgentEvent::ToolSpawned {
            call: Cow::Borrowed(&call),
        })
        .await;

        let session = match self.registry.get(&call.name) {
            Some(tool) => tool.run(call.clone(), SourceWaker::new(Arc::clone(&self.wake))),
            // Born exited: the error reaches the model at the next boundary
            // through exactly the same path as any other tool output.
            None => FinishedSession::boxed(
                ToolOutput {
                    output: Arc::new(format!("unknown tool: {}", call.name.as_str())),
                    status: ToolOutputStatus::Error,
                },
                now,
            ),
        };
        self.tools
            .insert(call.id.clone(), RunningTool::new(call, session, now));
    }

    // -- plumbing -----------------------------------------------------------

    async fn persist(&mut self, event: AgentEvent<'_>) {
        self.store.append(self.id, self.next_event, &event).await;
        self.next_event += 1;
    }

    fn publish(&self) {
        // Each preview names itself, so the list needs no parallel labelling
        // that could drift from the data.
        let mut previews: Vec<Box<dyn PreviewData>> = Vec::new();
        if !self.user.is_empty() {
            previews.push(self.user.preview());
        }
        previews.extend(
            self.mail
                .values()
                .filter(|mail| !mail.is_empty())
                .map(MailSource::preview),
        );
        previews.extend(self.tools.values().map(RunningTool::preview));

        *self.snapshot.write().expect("poisoned snapshot") = AgentSnapshot {
            history: self.history.clone(),
            previews,
            activity: if self.standing == Standing::Halted {
                AgentActivity::Stopped
            } else {
                AgentActivity::Live
            },
            streaming: self
                .in_flight
                .as_ref()
                .map(|in_flight| in_flight.pending.clone()),
            context_used: self.context_used,
            last_error: self.last_error.clone(),
        };
        self.notify.notify_waiters();
    }
}

/// How long a person's message waits for the machines around it to settle. Long
/// enough that a tool about to finish rides along with it, short enough not to
/// be felt.
const USER_PATIENCE: Duration = Duration::from_millis(500);
/// A peer usually sends several lines in a row; expect a beat more so they
/// collapse into one request instead of waking one apiece.
const MAIL_BURST: Duration = Duration::from_secs(1);
/// ...and how long a peer's mail waits for anything else, which also flushes a
/// peer that never stops.
const MAIL_PATIENCE: Duration = Duration::from_secs(2);
/// How long something a tool has finished with waits for a call that is still
/// working, so that a round of parallel calls arrives as one request rather
/// than one apiece.
const TOOL_PATIENCE: Duration = Duration::from_secs(10);
/// How long output a tool is still in the middle of sits unsent, once nobody is
/// waiting for it. Not a patience: it never shortens anybody else's wait, and
/// nothing about it is urgent. A build log is worth more whole than in pieces,
/// and a log nobody asked for is worth very little — but neither is worth
/// leaving unsent forever.
const PROGRESS_PATIENCE: Duration = Duration::from_secs(60);
/// How long the model waits to be shown what its calls have, when it did not
/// say.
///
/// Every other number here is a patience — how long something already worth
/// sending waits for company. This one is the opposite and the only one of its
/// kind: it is the model asking to be woken, and it is honoured whether or not
/// anything arrived, because an empty request is how the model learns to ask
/// for a longer interval next time.
const CHECKIN: Duration = Duration::from_secs(10);

/// What the model's latest turn settled: when it wants to be looked in on, and
/// which calls it is waiting on.
///
/// Two instants rather than one, because a turn can move one without the other.
/// Replying in prose while a build runs buys everyone another interval without
/// changing what the model is blocked on — and if a single instant did both, a
/// person typing during a five minute test run would end up demoting it, which
/// is the shape of bug this whole design keeps walking into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Turn {
    /// The end of the model's latest turn, whatever it contained.
    pub spoke_at: UnixMs,
    /// How long it asked to be left alone for. `None` is the ordinary case:
    /// it made some calls and said nothing about when to look at them.
    pub wait: Option<Duration>,
    /// The end of the latest turn that issued calls. Calls made at or after
    /// this are the ones it is waiting on; anything older it has moved past.
    pub calls_at: UnixMs,
}

/// Whether anything more is coming.
///
/// The one thing that can make a source impatient sooner than its own patience:
/// if nothing is due, waiting cannot improve the request, so whoever is holding
/// something goes now rather than sitting out a wait for company that is not
/// coming. Without it every finished tool call would cost [`TOOL_PATIENCE`] and
/// every typed message [`USER_PATIENCE`], however quiet the rest of the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    Nothing,
    /// A peer may be mid-burst until this instant. The only guess in here, and
    /// the only expectation that has to be waited out on a clock, because a
    /// quiet peer and a finished peer look exactly alike.
    Until(UnixMs),
    /// A call the model is waiting on. It wakes the core when it ends, so there
    /// is nothing to guess and no timer to arm.
    UntilItEnds,
}

/// Should the next request start now?
///
/// The one decision in this crate, and a plain function of what every source
/// reports — so it can be exercised against a fabricated clock and fabricated
/// sources, with no provider, no database, and no tasks. Every rule and every
/// duration lives here rather than on the sources, because a number chosen by
/// one source in isolation is a number chosen without seeing what else is
/// waiting.
///
/// Every source is here, including the ones with nothing to say, because
/// "this one is not worth sending" is a decision too and this is where
/// decisions live.
///
/// Everything holding something says how long it will wait for company, and
/// [`Due`] says whether company is coming. Four rules, and nothing else in this
/// crate decides anything:
///
/// 1. An `Interrupt` message is worth throwing away an in-flight request.
/// 2. The model is looked in on when it asked to be, whether or not anything
///    arrived.
/// 3. Otherwise go once nothing more is due — waiting cannot improve the
///    request.
/// 4. Otherwise go when the least patient holder runs out of patience, and
///    everyone yields into that same request.
///
/// Note what tools do *not* get: a running tool has no patience of its own
/// worth speaking of, because there is no way to tell `npm test` from
/// `npm run dev` by looking at either one. The pace at which a tool's output
/// reaches the model is set by the model, in rule 2, and revised every turn.
/// Only the two things a tool can say that are true regardless — it ended, or
/// what it holds stands on its own — buy it any urgency.
///
/// Every wait is measured from a moment that has already happened, never from
/// the last thing a source did, so no source can extend a wait by continuing to
/// talk — which is what stops one chatty tool from pinning everybody else.
fn boundary(
    sources: &[SourceKind],
    turn: Option<Turn>,
    inference_active: bool,
    standing: Standing,
    now: UnixMs,
) -> Boundary {
    // Cases where only a fresh event can change the answer, so there is nothing
    // to wake up for.
    let never = Boundary::No { recheck: None };
    if standing == Standing::Halted {
        return never;
    }
    if inference_active {
        let interrupt = sources.iter().any(|source| match source {
            SourceKind::User { interrupt, .. } => *interrupt,
            // However loud a peer or a tool is, the model finishes what it is
            // saying.
            SourceKind::Mail { .. } | SourceKind::Tool { .. } => false,
        });
        return if interrupt { Boundary::AbortNow } else { never };
    }
    if standing == Standing::MustSend {
        return Boundary::Now;
    }
    let mut due = Due::Nothing;
    // Something worth sending on its own, and when it stops waiting for company
    // — a person, a peer, a call that ended or flagged what it holds.
    let mut urgent: Option<UnixMs> = None;
    // When to collect whatever exists, whether or not any of it stands alone.
    // These are the opposite of a patience: nothing here is impatient, it is
    // just not worth leaving unsent forever.
    let mut sweep: Option<UnixMs> = None;
    // Rule 2, and it applies only while something the model is actually waiting
    // on is still running. An agent that has finished everything asked of it
    // must not wake itself up forever to say so — and a dev server it started
    // an hour ago is not a reason to keep waking it either, or an agent could
    // never be idle again until every long-lived tool it ever ran had died.
    if let Some(turn) = turn.filter(|turn| {
        sources.iter().any(|source| {
            matches!(
                source,
                SourceKind::Tool { called_at, told, activity: ToolActivity::Running, .. }
                    if *told != Told::Exit && *called_at >= turn.calls_at
            )
        })
    }) {
        sweep = Some(turn.spoke_at + turn.wait.unwrap_or(CHECKIN));
    }
    for source in sources {
        let (stands_alone, collect_by, source_due) = match *source {
            // Typed input is whole on arrival, so nothing more is ever due from
            // it. An empty queue has no patience, which is what makes it nothing
            // to send rather than a special case.
            SourceKind::User { oldest_at, .. } => {
                (oldest_at.map(|at| at + USER_PATIENCE), None, Due::Nothing)
            }
            SourceKind::Mail {
                oldest_at,
                newest_at,
            } => (
                oldest_at.map(|at| at + MAIL_PATIENCE),
                None,
                match newest_at.map(|at| at + MAIL_BURST) {
                    Some(until) if until > now => Due::Until(until),
                    _ => Due::Nothing,
                },
            ),
            SourceKind::Tool {
                called_at,
                told,
                activity,
                unsent,
            } => {
                // When what it is holding stops waiting for anybody else. An
                // ending and a flag are the same kind of news and wait the
                // same; anything else is mid-thought and waits far longer.
                // Every one of these dates from the moment it happened, so a
                // tool that ends after an hour of output gets its siblings'
                // full attention rather than looking an hour overdue.
                let (stands_alone, collect_by) = match (activity, unsent) {
                    (ToolActivity::Exited { at }, Unsent::Settled { since }) => {
                        (Some(at.min(since) + TOOL_PATIENCE), None)
                    }
                    (ToolActivity::Exited { at }, _) => (Some(at + TOOL_PATIENCE), None),
                    (ToolActivity::Running, Unsent::Settled { since }) => {
                        (Some(since + TOOL_PATIENCE), None)
                    }
                    // Mid-thought, so nobody asked for it and it is nobody's
                    // reason to make a request — it is only not worth leaving
                    // unsent forever. While the model is being looked in on it
                    // never gets this far, because the check-in is sooner; this
                    // is what half a build log is worth to an agent that has
                    // moved on, or asked for a long quiet.
                    (ToolActivity::Running, Unsent::Waiting { since }) => {
                        (None, Some(since + PROGRESS_PATIENCE))
                    }
                    (ToolActivity::Running, Unsent::Nothing) => (None, None),
                };
                match told {
                    // Everything there was to say has been said.
                    Told::Exit => (None, None, Due::Nothing),
                    _ => (
                        stands_alone,
                        collect_by,
                        match (activity, unsent) {
                            // It says what it holds stands on its own, which is
                            // it saying not to wait for the rest of the call.
                            (ToolActivity::Running, Unsent::Settled { .. }) => Due::Nothing,
                            // A call the model went on to ask for something
                            // else after is one it has stopped waiting on. If
                            // it still counted, a dev server that never
                            // finishes would be a wait nobody could ever end.
                            (ToolActivity::Running, _)
                                if turn.is_some_and(|turn| called_at >= turn.calls_at) =>
                            {
                                Due::UntilItEnds
                            }
                            _ => Due::Nothing,
                        },
                    ),
                }
            }
        };
        if let Some(ends) = stands_alone {
            urgent = Some(urgent.unwrap_or(ends).min(ends));
        }
        if let Some(by) = collect_by {
            sweep = Some(sweep.unwrap_or(by).min(by));
        }
        due = match (due, source_due) {
            (Due::UntilItEnds, _) | (_, Due::UntilItEnds) => Due::UntilItEnds,
            (Due::Until(one), Due::Until(other)) => Due::Until(one.max(other)),
            (Due::Nothing, other) | (other, Due::Nothing) => other,
        };
    }

    // Rule 3. Only what stands on its own is cut short this way: a patience is
    // a wait for company, and there is no company coming. A sweep is not a
    // patience — half a build log is no more worth sending because the room is
    // empty — so it is left to run.
    if urgent.is_some() && due == Due::Nothing {
        return Boundary::Now;
    }
    // Nothing worth a request, whoever is still busy.
    let deadline = match (urgent, sweep) {
        (Some(urgent), Some(sweep)) => urgent.min(sweep),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => return never,
    };
    match due {
        // Rule 4.
        _ if now >= deadline => Boundary::Now,
        // A peer going quiet is not an event, so the one expectation that
        // lapses on a clock has to be waited out. Everything else announces
        // itself, so the deadline is the only thing to wake up for.
        Due::Until(until) => Boundary::No {
            recheck: Some(deadline.min(until)),
        },
        Due::Nothing | Due::UntilItEnds => Boundary::No {
            recheck: Some(deadline),
        },
    }
}

/// Once a request compacts, is the agent still owed a reply?
///
/// A compaction is a means, not an end: whatever else rode in the request is
/// inside the summary now rather than answered, and a compaction the core asked
/// for displaced a request that had its own purpose. Only a bare `/compact`
/// asks for nothing further, and that is where the agent stops.
fn compaction_owes_reply(core_asked: bool, blocks: &[ContextBlock]) -> bool {
    core_asked
        || blocks
            .iter()
            .any(|block| *block != ContextBlock::CompactionTrigger)
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

/// Whether a call already has its result, for a tool the core no longer has.
fn result_sent(history: &[Arc<ContextBlock>], call_id: &ToolCallId) -> bool {
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
    mail: BTreeMap<AgentId, MailSource>,
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
                InputSource::Mail { sender } => {
                    let InputKind::Message { content } = input.kind else {
                        continue;
                    };
                    restored
                        .mail
                        .entry(sender)
                        .or_insert_with(|| MailSource::new(sender))
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
