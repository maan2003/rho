//! A small agent harness built around one question, asked after every event:
//!
//! > *Should the next request start now?*
//!
//! The `boundary` module answers it. Everything here is mechanism — spawning,
//! draining, persisting, publishing — and the transcript's sole writer.
//!
//! `specs/ARCH-rho-agent2.md` has the shape and the invariants.

mod boundary;
mod db;
mod preview;
mod source;
mod tool;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rho_core::{
    AgentId, ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    MessageSender, PendingInferenceResponse, ProviderResponseId, ToolCall, ToolCallId, ToolName,
    ToolOutput, ToolOutputStatus, ToolResult, ToolSpec, UnixMs,
};
use rho_inference::config::{InferenceModel, InferenceProfile};
use rho_inference::{Inference, InferenceSession, PromptCacheKey};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::boundary::{Boundary, ModelAsked, ModelTurn, boundary};
pub use crate::db::Store;
use crate::db::{AgentEvent, AgentRecord, EventPos};
pub use crate::preview::{PendingItem, Preview};
pub use crate::source::{Delivery, InputKind, InputSource, QueuedInput};
use crate::source::{MailSource, UserSource};
use crate::tool::{FinishedSession, RunningTool, ToolTake};
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
    pub previews: Vec<Preview>,
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
    /// Cancelled, or failed: nothing will start a request until fresh user
    /// input arrives. Which of the two it was is `last_error`.
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
    id: AgentId,
    control: mpsc::UnboundedSender<Control>,
    snapshot: Arc<RwLock<AgentSnapshot>>,
    published: Arc<Notify>,
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
        let (id, next_event, record) = store
            .create_agent(profile, model.into(), PromptCacheKey::generate())
            .await;
        Ok(Agent::start(
            store,
            inference,
            id,
            record,
            instructions.into(),
            next_event,
            registry,
            Restored::default(),
        ))
    }

    /// Instructions come from the caller here exactly as they do for a new
    /// agent: `DECISION-instructions-are-code`.
    pub fn load(
        store: Store,
        inference: Inference,
        registry: ToolRegistry,
        id: AgentId,
        instructions: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let (record, next_event, events) = store
            .load(id)
            .ok_or_else(|| anyhow::anyhow!("rho-agent2 agent not found"))?;
        Ok(Agent::start(
            store,
            inference,
            id,
            record,
            instructions.into(),
            next_event,
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

    pub async fn send_user_message(&self, text: impl Into<String>, delivery: Delivery) -> bool {
        self.send_input(
            InputKind::Message {
                content: vec![ContentPart::Text { text: text.into() }],
            },
            delivery,
        )
        .await
    }

    pub async fn compact(&self) -> bool {
        self.send_input(InputKind::Compaction, Delivery::NextRequest)
            .await
    }

    async fn send_input(&self, kind: InputKind, delivery: Delivery) -> bool {
        self.send(|done| {
            Control::User(
                QueuedInput {
                    source: InputSource::User,
                    kind,
                    delivery,
                    at: UnixMs::now(),
                },
                done,
            )
        })
        .await
    }

    /// Deliver mail from a peer agent.
    pub async fn send_mail(&self, sender: AgentId, text: impl Into<String>) -> bool {
        self.send(|done| Control::Mail {
            sender,
            content: vec![ContentPart::Text { text: text.into() }],
            at: UnixMs::now(),
            done,
        })
        .await
    }

    /// Abort the in-flight request and durably discard queued inputs.
    pub async fn cancel(&self) -> bool {
        self.send(Control::Cancel).await
    }

    /// Retry after a failure, or resume a request interrupted by a restart.
    pub async fn retry(&self) -> bool {
        self.send(Control::Retry).await
    }

    /// Hand a command to the loop and wait for it to land.
    ///
    /// `false` means the agent stopped before it got there — either the loop
    /// had already ended when the command was sent, or it ended while the
    /// command was in the queue. A caller cannot tell those apart and has no
    /// reason to: neither one happened.
    async fn send(&self, command: impl FnOnce(oneshot::Sender<()>) -> Control) -> bool {
        let (done, landed) = oneshot::channel();
        if self.control.send(command(done)).is_err() {
            return false;
        }
        landed.await.is_ok()
    }

    /// An immediate snapshot, then every subsequent change.
    pub fn subscribe(&self) -> impl futures::Stream<Item = AgentSnapshot> + use<> {
        let snapshot = Arc::clone(&self.snapshot);
        let published = Arc::clone(&self.published);
        async_stream::stream! {
            loop {
                let notified = published.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let current = snapshot.read().expect("poisoned snapshot").clone();
                yield current;
                notified.await;
            }
        }
    }
}

/// A command, and the ack that says it landed.
///
/// Every one carries the ack, not just the ones that write: what a caller wants
/// to know is that the loop has *acted*, and "the queue is durable" and "the
/// cancel has taken effect" are the same promise from the outside. The ack
/// fires after the command's own handling, so anything it persisted is on disk
/// by then.
enum Control {
    User(QueuedInput, oneshot::Sender<()>),
    Mail {
        sender: AgentId,
        content: Vec<ContentPart>,
        at: UnixMs,
        done: oneshot::Sender<()>,
    },
    Cancel(oneshot::Sender<()>),
    Retry(oneshot::Sender<()>),
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

/// What the agent is up to, and the first thing the decision asks about.
///
/// Either a request is in flight or it is not, and there is nothing else to be.
#[derive(Clone)]
enum Phase {
    /// No request in flight. `owed` is what the next one has to open with —
    /// calls nothing is going to answer, which after a restart is every call
    /// history left hanging (`SPEC-restart-recovery`). Ordinarily empty.
    Idle {
        owed: Vec<ToolCall>,
        standing: Standing,
    },
    /// A request is in flight, and nothing but an interrupt may disturb it.
    Requesting(InFlight),
}

/// The last thing to happen to an idle agent that bears on whether it should
/// speak, and when it happened.
///
/// Facts rather than a verdict: what any of them is worth is `boundary`'s to
/// say. Nothing here survives a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Standing {
    /// Nothing either way; the sources decide. The ordinary case, and what a
    /// loaded agent always comes back as.
    Nothing,
    /// Somebody asked for a request the sources would not have made: a retry,
    /// or a compaction the core performed on work that still owes a reply.
    /// Never set by loading: `DECISION-a-restart-does-not-resume-by-itself`.
    Asked,
    /// The user pressed stop. What is `owed` outlives it, because a cancel is
    /// not an answer.
    Cancelled { at: UnixMs },
    /// The request in flight then failed for good, saying `error`.
    Failed { at: UnixMs, error: Arc<str> },
}

impl Standing {
    /// Whether the agent is stopped, given the oldest thing the user has
    /// queued.
    ///
    /// A stop waits on a person, so only user input that arrived after it lifts
    /// it: `DECISION-stopped-agents-wait-for-a-person`. Derived from the queue
    /// rather than recorded, because the queue already says it.
    fn stopped(&self, user_oldest_at: Option<UnixMs>) -> bool {
        match self {
            Self::Nothing | Self::Asked => false,
            // A cancel empties the queues and a failed request had already
            // drained them, so anything dated at or after the stop is somebody
            // typing since. Anything older was already on its way.
            Self::Cancelled { at } | Self::Failed { at, .. } => {
                !user_oldest_at.is_some_and(|oldest| oldest >= *at)
            }
        }
    }
}

/// The in-flight request. Provisional until it finishes: a failure drops the
/// whole thing without touching history, and everything here goes with it.
#[derive(Clone, Default)]
struct InFlight {
    pending: PendingInferenceResponse,
    /// What each temporary failure said, latest last — so the count is how deep
    /// into retrying this request is, and the last one is what it is retrying
    /// *from*. Both belong to the request rather than to the agent: a retry
    /// that eventually works leaves nothing behind to explain.
    temporary_failures: Vec<Arc<str>>,
    /// This request compacted on behalf of work that still owes a reply, so a
    /// compaction must not be where the agent stops. A fact about *this*
    /// request, so a request that never finishes never has to unset it.
    compaction_owes_reply: bool,
}

struct Agent {
    store: Store,
    /// Where this agent's next event goes. It is the sole writer of its own
    /// log, so this is all it needs to carry on from.
    next_event: EventPos,
    instructions: Arc<str>,

    /// The transcript. Append-only, and this struct is its sole writer.
    history: Vec<Arc<ContextBlock>>,

    session: InferenceSession,
    phase: Phase,

    // review: inlilne ToolRegister, UserSource, MailSource
    user: UserSource,
    mail: MailSource,
    tools: BTreeMap<ToolCallId, RunningTool>,

    registry: ToolRegistry,

    context_used: Option<u64>,
    /// What the model's latest turn settled about being looked in on. `None`
    /// until it has spoken once — after a restart included, which is safe
    /// because no tool survives one.
    // review: why turn not part of Phase
    turn: Option<ModelTurn>,
    /// Content-free signal that some tool changed. Tools hold a
    /// [`SourceWaker`] over this; the core rescans rather than being told.
    wake: Arc<Notify>,

    snapshot: Arc<RwLock<AgentSnapshot>>,
    /// Poked whenever the published snapshot changes; `subscribe` waits on it.
    published: Arc<Notify>,
    control_rx: mpsc::UnboundedReceiver<Control>,
}

impl Agent {
    #[expect(
        clippy::too_many_arguments,
        reason = "everything the loop needs, and a struct to hold it would be \
                  these same arguments with a name"
    )]
    fn start(
        store: Store,
        inference: Inference,
        id: AgentId,
        record: AgentRecord,
        instructions: String,
        next_event: EventPos,
        registry: ToolRegistry,
        restored: Restored,
    ) -> AgentHandle {
        let session =
            inference.deep_session(record.profile, record.model.into(), record.prompt_cache_key);
        let snapshot = Arc::new(RwLock::new(AgentSnapshot::default()));
        let published = Arc::new(Notify::new());
        let (control, control_rx) = mpsc::unbounded_channel();

        let agent = Self {
            store,
            next_event,
            instructions: Arc::from(instructions),
            history: restored.history,
            session,
            // A fresh agent lands here too — being loaded from a log is not
            // its own kind of state, and coming up is never by itself a reason
            // to send: `DECISION-a-restart-does-not-resume-by-itself`.
            phase: Phase::Idle {
                owed: restored.owed,
                standing: Standing::Nothing,
            },
            user: restored.user,
            mail: restored.mail,
            tools: BTreeMap::new(),
            registry,
            context_used: restored.context_used,
            turn: None,
            wake: Arc::new(Notify::new()),
            snapshot: Arc::clone(&snapshot),
            published: Arc::clone(&published),
            control_rx,
        };

        tokio::spawn(agent.run());

        AgentHandle {
            id,
            control,
            snapshot,
            published,
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
            // Every source, in whatever state it is in — nothing is filtered
            // out for having nothing to say, because deciding that is the
            // decision's job, and an empty queue is a fact it reads.
            let mut sources = vec![self.user.source(), self.mail.source()];
            sources.extend(self.tools.values().map(RunningTool::source));

            let deadline = match boundary(&sources, self.turn.as_ref(), &self.phase, now) {
                Boundary::No { recheck } => recheck,
                Boundary::AbortAndResend => {
                    // Nothing to undo: what the model had said was provisional
                    // and never reached history.
                    self.session.abort();
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
            Event::Control(control) => {
                let done = match control {
                    Control::User(input, done) => {
                        self.persist(AgentEvent::Queued(input.clone())).await;
                        // Queueing it is the whole of it. Whether this revives
                        // an agent that had stopped is `Standing::stopped`'s
                        // reading of the very queue this pushes onto, so there
                        // is no second place for it to be written down and go
                        // stale.
                        self.user.push(input);
                        done
                    }
                    Control::Mail {
                        sender,
                        content,
                        at,
                        done,
                    } => {
                        let input = QueuedInput {
                            source: InputSource::Mail { sender },
                            kind: InputKind::Message {
                                content: content.clone(),
                            },
                            delivery: Delivery::NextRequest,
                            at,
                        };
                        self.persist(AgentEvent::Queued(input)).await;
                        self.mail.push(sender, content, at);
                        done
                    }
                    Control::Cancel(done) => {
                        // Ask every tool to wind down, then keep reading it: the
                        // core does not kill tools, so a tool still chooses its
                        // own last words.
                        for tool in self.tools.values_mut() {
                            tool.cancel();
                        }
                        // A cancel is not an answer, so what is owed outlives
                        // it.
                        let owed = match &mut self.phase {
                            Phase::Idle { owed, .. } => std::mem::take(owed),
                            Phase::Requesting(_) => Vec::new(),
                        };
                        self.session.abort();
                        self.phase = Phase::Idle {
                            owed,
                            standing: Standing::Cancelled { at: now },
                        };
                        if !self.user.is_empty() || !self.mail.is_empty() {
                            self.persist(AgentEvent::QueueCleared).await;
                            self.user.clear();
                            self.mail.clear();
                        }
                        done
                    }
                    Control::Retry(done) => {
                        // Hurries the next request rather than changing what has
                        // to be in it; nothing to hurry while one is in flight.
                        if let Phase::Idle { standing, .. } = &mut self.phase {
                            *standing = Standing::Asked;
                        }
                        done
                    }
                };
                // After the handling rather than before, so a caller that waited
                // knows the thing happened — and for the commands that persist,
                // that it is on disk. Not that the model has *seen* it: that
                // waits for a boundary the caller does not control.
                let _ = done.send(());
            }
            // Anything the model says outside a request of ours is somebody
            // else's, or the tail of one already abandoned.
            Event::Inference(event) => {
                let Phase::Requesting(in_flight) = &mut self.phase else {
                    return;
                };
                match event {
                    // Quota is account-wide and published by rho-inference
                    // itself, so there is nothing here to carry.
                    InferenceEvent::RequestSent
                    | InferenceEvent::StreamingStarted
                    | InferenceEvent::Quota { .. } => {}
                    InferenceEvent::ContextItem { index, event } => {
                        in_flight.pending.apply(index, event)
                    }
                    InferenceEvent::TemporaryFailure { error, .. } => {
                        in_flight
                            .temporary_failures
                            .push(Arc::from(error.to_string()));
                        // The retry starts a fresh response; drop the partial
                        // one.
                        in_flight.pending = PendingInferenceResponse::default();
                    }
                    // Nothing to abort, and nothing to record: the request is
                    // already over, it is the agent that stops here rather than
                    // the request, and everything the request had was
                    // provisional.
                    InferenceEvent::Failed { error } => {
                        self.phase = Phase::Idle {
                            owed: Vec::new(),
                            standing: Standing::Failed {
                                at: now,
                                error: Arc::from(error.to_string()),
                            },
                        };
                    }
                    InferenceEvent::Finished {
                        usage,
                        provider_response_id,
                    } => match in_flight.pending.finish() {
                        // Finished streaming, but what arrived does not
                        // assemble into a response.
                        Err(error) => {
                            self.phase = Phase::Idle {
                                owed: Vec::new(),
                                standing: Standing::Failed {
                                    at: now,
                                    error: Arc::from(error.to_string()),
                                },
                            };
                        }
                        Ok(items) => {
                            self.finish_request(items, provider_response_id, usage, now)
                                .await
                        }
                    },
                }
            }
            // Both are pure prompts to re-ask the question; what a source
            // reports is read live, so there is nothing to record here.
            Event::SourceChanged | Event::Tick => {}
        }
    }

    // -- acting on it -------------------------------------------------------

    async fn start_request(&mut self, now: UnixMs) {
        // What is owed is settled here and nowhere earlier:
        // `SPEC-restart-recovery`.
        let owed = match &mut self.phase {
            Phase::Idle { owed, .. } => std::mem::take(owed),
            Phase::Requesting(_) => Vec::new(),
        };
        let mut blocks: Vec<ContextBlock> = Vec::new();
        if !owed.is_empty() {
            blocks.push(ContextBlock::ToolResults {
                results: owed
                    .iter()
                    .map(|call| ToolResult {
                        call_id: call.id.clone(),
                        tool_type: call.tool_type,
                        body: ToolOutput {
                            output: Arc::new(String::new()),
                            status: ToolOutputStatus::Cancelled,
                        },
                        started_at: now,
                        finished_at: now,
                        metadata: None,
                    })
                    .collect(),
            });
            // What the empty results cannot say themselves.
            blocks.push(ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: vec![ContentPart::Text {
                    // The prose half of what the request owes the model;
                    // the empty results above are the other half.
                    text: "note: rho restarted. Every tool that was running is gone — foreground \
                           and background alike — and nothing was recorded about what any of them \
                           did. The empty tool results above are placeholders, not output. Re-run \
                           anything you still need."
                        .to_owned(),
                }],
            });
        }
        // Every source, not just whichever one triggered the boundary:
        // `DECISION-pull-based-sources`. The order is protocol-constrained
        // rather than chronological, so tool output leads:
        // `REQ-provider-transcript-protocol`.
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
        blocks.extend(self.mail.take());
        blocks.extend(self.user.take());

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
        // because it failed — which is what reading back as far as the latest
        // response finds.
        let compacting_already = blocks.contains(&ContextBlock::CompactionTrigger)
            || self
                .history
                .iter()
                .rev()
                .find_map(|block| match &**block {
                    ContextBlock::CompactionTrigger => Some(true),
                    ContextBlock::InferenceResponse { .. } => Some(false),
                    ContextBlock::UserMessage { .. }
                    | ContextBlock::ToolResults { .. }
                    | ContextBlock::ToolUpdate(_) => None,
                })
                .unwrap_or(false);
        let compact = over_limit && !compacting_already;
        if compact {
            blocks.push(ContextBlock::CompactionTrigger);
        }
        // Once it compacts, is the agent still owed a reply? A compaction is a
        // means, not an end: whatever else rode in the request is inside the
        // summary now rather than answered, and one the core asked for displaced
        // a request that had its own purpose. Only a bare `/compact` asks for
        // nothing further, and that is where the agent stops.
        let owes_reply = compact
            || blocks
                .iter()
                .any(|block| *block != ContextBlock::CompactionTrigger);

        // Forget tools that exited and whose output the model has seen, so a
        // long session does not accumulate them forever. Nothing to record: a
        // reaped tool is one the transcript has already finished talking about.
        self.tools.retain(|_, tool| !tool.reapable());

        // The drain, the append and the send are one event because they are one
        // thing: a crash between them would leave a transcript nobody drained
        // into and a queue nobody emptied.
        self.persist(AgentEvent::Sent {
            blocks: Cow::Borrowed(&blocks),
        })
        .await;
        self.history.extend(blocks.into_iter().map(Arc::new));
        self.session.request(InferenceRequest {
            instructions: Arc::clone(&self.instructions),
            input: self.history.clone(),
            agent_id_labels: Default::default(),
            tools: self.registry.specs(),
        });
        self.phase = Phase::Requesting(InFlight {
            compaction_owes_reply: owes_reply,
            ..InFlight::default()
        });
    }

    // -- inference ----------------------------------------------------------

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
        self.persist(AgentEvent::Replied {
            blocks: Cow::Borrowed(std::slice::from_ref(&block)),
            context_used,
        })
        .await;
        self.history.push(Arc::new(block));

        // Everything the request carried goes with it, except whether it still
        // owes a reply.
        let owed_a_reply = match &self.phase {
            Phase::Requesting(in_flight) => in_flight.compaction_owes_reply,
            // Only a request in flight can finish.
            Phase::Idle { .. } => false,
        };

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
            self.spawn_tool(call, now);
        }

        self.phase = Phase::Idle {
            owed: Vec::new(),
            standing: match compacted && owed_a_reply {
                // The compaction ate the turn the model owed a reply to, so ask
                // for it again.
                true => Standing::Asked,
                false => Standing::Nothing,
            },
        };
    }

    // -- tools --------------------------------------------------------------

    fn spawn_tool(&mut self, call: ToolCall, now: UnixMs) {
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
        self.next_event = self.store.append(self.next_event, &event).await;
    }

    fn publish(&self) {
        // Each preview names itself, so the list needs no parallel labelling
        // that could drift from the data.
        let mut previews = Vec::new();
        if !self.user.is_empty() {
            previews.push(self.user.preview());
        }
        previews.extend(self.mail.previews());
        previews.extend(self.tools.values().map(RunningTool::preview));

        *self.snapshot.write().expect("poisoned snapshot") = AgentSnapshot {
            history: self.history.clone(),
            previews,
            activity: match &self.phase {
                Phase::Idle { standing, .. } if standing.stopped(self.user.oldest_at()) => {
                    AgentActivity::Stopped
                }
                Phase::Idle { .. } | Phase::Requesting(_) => AgentActivity::Live,
            },
            streaming: match &self.phase {
                Phase::Requesting(in_flight) => Some(in_flight.pending.clone()),
                Phase::Idle { .. } => None,
            },
            context_used: self.context_used,
            // What stopped the agent, or — while it is still retrying inside one
            // request — what it is retrying from.
            last_error: match &self.phase {
                Phase::Idle {
                    standing: Standing::Failed { error, .. },
                    ..
                } => Some(Arc::clone(error)),
                Phase::Requesting(in_flight) => in_flight.temporary_failures.last().cloned(),
                Phase::Idle { .. } => None,
            },
        };
        self.published.notify_waiters();
    }
}

#[derive(Default)]
struct Restored {
    history: Vec<Arc<ContextBlock>>,
    user: UserSource,
    mail: MailSource,
    context_used: Option<u64>,
    /// Calls nothing is going to answer, because no tool survived.
    owed: Vec<ToolCall>,
}

impl Restored {
    fn clear_queues(&mut self) {
        self.user.clear();
        self.mail.clear();
    }
}

fn restore(events: Vec<AgentEvent<'static>>) -> Restored {
    let mut restored = Restored::default();
    for event in events {
        match event {
            AgentEvent::Queued(input) => match input.source {
                InputSource::User => restored.user.push(input),
                InputSource::Mail { sender } => {
                    let InputKind::Message { content } = input.kind else {
                        continue;
                    };
                    restored.mail.push(sender, content, input.at);
                }
            },
            AgentEvent::QueueCleared => restored.clear_queues(),
            AgentEvent::Sent { blocks } => {
                restored
                    .history
                    .extend(blocks.into_owned().into_iter().map(Arc::new));
                // A send drains everything, so nothing that was pending when it
                // went out is pending after it.
                restored.clear_queues();
            }
            AgentEvent::Replied {
                blocks,
                context_used,
            } => {
                restored
                    .history
                    .extend(blocks.into_owned().into_iter().map(Arc::new));
                if context_used.is_some() {
                    restored.context_used = context_used;
                }
            }
        }
    }

    // Every call the model has made, minus the ones the transcript already
    // answers. Read off history rather than remembered, because history already
    // says it. `SPEC-restart-recovery`.
    let mut unanswered: BTreeMap<ToolCallId, ToolCall> = BTreeMap::new();
    for block in &restored.history {
        match &**block {
            ContextBlock::InferenceResponse { items, .. } => {
                for item in items {
                    let InferenceResponseItem::ToolCall {
                        id,
                        name,
                        tool_type,
                        arguments,
                        ..
                    } = item
                    else {
                        continue;
                    };
                    unanswered.insert(
                        id.clone(),
                        ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            tool_type: *tool_type,
                            arguments: arguments.clone(),
                        },
                    );
                }
            }
            ContextBlock::ToolResults { results } => {
                for result in results {
                    unanswered.remove(&result.call_id);
                }
            }
            ContextBlock::UserMessage { .. }
            | ContextBlock::ToolUpdate(_)
            | ContextBlock::CompactionTrigger => {}
        }
    }
    restored.owed = unanswered.into_values().collect();
    restored
}

#[cfg(test)]
mod tests;
