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
//! The `boundary` module answers it, and is the only place in this crate that
//! exercises judgment. Everything else — spawning, persisting, draining,
//! publishing — is mechanism, and lives here. The division of labour with
//! tools follows from the same split: **nothing outside the core decides
//! *when*, and the core never decides *what***. A tool reports what it has and
//! how it is doing; the core weighs that against everything else and picks the
//! moment. The only thing the core ever says back to a tool is
//! [`ToolSession::cancel`] — wind down — and even then it collects the tool's
//! parting output, so the tool has the last word.

mod boundary;
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

use crate::boundary::{Boundary, ModelAsked, ModelTurn, Standing, boundary};
pub use crate::preview::{
    MailPreview, PendingItem, PreviewData, ToolPreview, UnknownPreviewData, UserPreview,
};
pub use crate::source::{Delivery, InputKind, InputSource, QueuedInput};
use crate::source::{MailSource, SourceKind, UserSource};
use crate::store::{AgentEvent, AgentRecord};
pub use crate::store::{AgentKey, Store};
use crate::tool::{FinishedSession, RunningTool, ToolTake, lost_to_restart};
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
    turn: Option<ModelTurn>,
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

    /// Answer the one question after every event, act on the answer, and wait
    /// for the next one, until the last handle is dropped.
    async fn run(mut self) {
        loop {
            // One question per event. Either it says to wait and hands over the
            // timer — so the timer and the rule behind it cannot drift apart —
            // or it says to send, and a request in flight is never waited for.
            let now = UnixMs::now();
            let deadline = match self.boundary(now) {
                Boundary::No { recheck } => recheck,
                Boundary::AbortAndResend => {
                    self.abort_in_flight().await;
                    self.start_request(now).await;
                    None
                }
                Boundary::Now => {
                    self.start_request(now).await;
                    None
                }
            };
            self.publish();
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
        }
    }

    /// The single funnel. Every event lands here and does nothing but update
    /// state; what to do about it is asked once, by the caller.
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
    }

    // -- the one decision ---------------------------------------------------

    fn boundary(&self, now: UnixMs) -> Boundary {
        boundary(
            &self.sources(),
            self.turn.as_ref(),
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

    /// Throw away the in-flight request. Whatever the model had said so far was
    /// provisional and never reached history, so there is nothing to undo.
    async fn abort_in_flight(&mut self) {
        self.session.abort();
        self.in_flight = None;
        self.compaction_owes_reply = false;
        self.persist(AgentEvent::RequestEnded { context_used: None })
            .await;
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
        self.abort_in_flight().await;
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

        // A turn that issues no calls asks for nothing and changes nothing: it
        // neither buys another look-in nor moves what the model is waiting on.
        // `ModelAsked::Wait` arrives with the tool that names an interval.
        let waiting_on = match calls.is_empty() {
            false => calls.iter().map(|call| call.id.clone()).collect(),
            true => self
                .turn
                .take()
                .map(|turn| turn.waiting_on)
                .unwrap_or_default(),
        };
        self.turn = Some(ModelTurn {
            spoke_at: now,
            asked: match calls.is_empty() {
                false => ModelAsked::Calls,
                true => ModelAsked::Nothing,
            },
            waiting_on,
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
