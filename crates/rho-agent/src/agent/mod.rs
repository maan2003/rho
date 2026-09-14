//! The Rho runtime: an agent loop built around one question, asked after
//! every event:
//!
//! > *Should the next request start now?*
//!
//! The `boundary` module answers it. Everything here is mechanism — spawning,
//! draining, persisting, publishing — and the transcript's sole writer. The
//! loop writes the agent's raw log and, beside it, the story a reader gets
//! (`AGENT-LOG-DESIGN.md`), records what each response cost, and keeps the
//! presentation sidecar fed.
//!
//! `specs/ARCH-rho-agent.md` has the shape and the invariants.

mod context;
mod notes;
pub(crate) mod replay;
#[cfg(test)]
mod rotation_tests;
mod streaming;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rho_agent_tools::{PythonCell, ReplyState, SourceWaker};
use rho_core::{
    AgentId, ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    MessageDelivery, MessageSender, PendingInferenceResponse, ProviderResponseId, ToolCall,
    ToolCallId, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, UnixMs,
};
use rho_db::RhoDb;
use rho_inference::config::{InferenceModel, InferenceProfile};
use rho_inference::{Inference, InferenceSession, PromptCacheKey};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::boundary::{
    Boundary, ModelAsked, ModelTurn, Observations, SourceKind, Standing, boundary,
};
use crate::db::{
    AgentEventPos, AgentHead, AgentProfileWriteTxnExt as _,
    AgentReadTxnExt as _, AgentRole, AgentRoleSessionProfile as _, AgentRuntime, AgentUsageBucket,
    AgentUsageModel, AgentWriteTxnExt as _, EngineerIntelligence, SessionBinding, TurnEdge,
    TurnOutcome, UnixMillis,
};
use crate::lazy::Lazy;
use crate::multi_agent_tools::MultiAgentTools;
use crate::native::NativeEvent;
use crate::notebook::host_tools;
use crate::pool::{AgentPool, AgentTurnCompleted};

use crate::{
    AgentEvent, AgentStateKind, AgentStatus, FailedInferenceResponse, InputKind, QueuedInput,
    StartPlace, ToolPreview, View, final_answer_text, prompt,
};

/// Whether the model's turn made a call it is waiting on. Only the notebook
/// says anything about pacing, through `set_checkin` inside the call.

// -- what is waiting to reach the model -------------------------------------
//
// A queue accumulates on its own and is *pulled* by the agent at a moment the
// agent chooses; nothing here starts a request.
// `DECISION-pull-based-sources`.

/// A piece of mail, once the queue has it. Who sent it lives on the message,
/// because that is where it varies: everyone's mail is one queue, since the
/// decision reads it as one — the oldest across every sender is the wait being
/// spent, and the newest across every sender is the burst that might still be
/// going.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MailItem {
    pub sender: AgentId,
    pub content: Vec<ContentPart>,
    pub at: UnixMs,
}

/// The session-lifetime tools and prompt ingredients, built from the
/// agent's workdirs once they are materialized. Lazy because a load must not
/// fail on a workdir that has gone: a reader still gets the transcript, and
/// only a turn needs the tools.
struct Surface {
    notebook: Arc<rho_agent_tools::PythonNotebook>,
    prompt: PromptInputs,
}

/// Role-independent ingredients; rendering instructions never rebuilds tools.
struct PromptInputs {
    view: Arc<View>,
    multi_agent: Option<MultiAgentTools>,
    host_specs: Vec<ToolSpec>,
    notes: Option<Lazy<std::path::PathBuf>>,
}

struct Instructions {
    text: Arc<str>,
    notes: Option<std::path::PathBuf>,
}

impl PromptInputs {
    async fn render(&self, role: AgentRole) -> anyhow::Result<Instructions> {
        let text = prompt::prompt(
            &self.view,
            self.multi_agent.as_ref(),
            role,
            &self.host_specs,
        );
        let notes = match &self.notes {
            Some(notes) if role.uses_notes_rotation() => Some(notes.get().await?.clone()),
            _ => None,
        };
        let text = match &notes {
            Some(path) => Arc::from(format!("{}{}", text, notes::instructions(path))),
            None => text,
        };
        Ok(Instructions { text, notes })
    }
}

/// Inputs consumed by the session's lazy tool initialization.
#[derive(Clone)]
struct SurfaceInputs {
    view: Arc<Lazy<Arc<View>>>,
    agent_id: AgentId,
    inference: Inference,
    parent: Option<AgentId>,
    pool: std::sync::Weak<AgentPool>,
}

impl SurfaceInputs {
    fn lazy(&self, role: AgentRole) -> Arc<Lazy<Surface>> {
        let inputs = self.clone();
        Arc::new(Lazy::new(move || {
            let inputs = inputs.clone();
            async move {
                let view = Arc::clone(inputs.view.get().await?);
                surface(
                    view,
                    role,
                    inputs.agent_id,
                    Some(&inputs.inference),
                    inputs.parent,
                    &inputs.pool,
                )
            }
        }))
    }
}

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
    /// A new agent: its record, its workdirs, and a loop with nothing in it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create(
        db: RhoDb,
        inference: Inference,
        mode: SessionBinding,
        role: AgentRole,
        display_name: Option<String>,
        start: StartPlace,
        parent: Option<AgentId>,
        // A dead Weak (e.g. `Weak::default()`) means no pool: the
        // multi-agent tools are not offered.
        pool: std::sync::Weak<AgentPool>,
    ) -> anyhow::Result<(AgentId, Self)> {
        let profile = mode
            .deep_config()
            .ok_or_else(|| anyhow::anyhow!("cannot create a Rho runtime for a Claude mode"))?;
        let model = mode.deep_model().expect("deep config implies a deep model");
        let prompt_cache_key = PromptCacheKey::generate();
        // One transaction spans agent id allocation and the record write.
        let mut write = db.write().await;
        let agent_id = write.alloc_agent_id();
        let StartPlace { view, place, .. } = start;
        write.create_agent(
            UnixMillis::now(),
            agent_id,
            display_name,
            place,
            role,
            mode,
            AgentRuntime::Rho { prompt_cache_key },
            parent,
        );
        write.commit();
        // Two rows old: folding it is nothing.
        let head = db.read().get_agent(agent_id);
        let handle = Self::start(
            db,
            inference,
            profile,
            model,
            role,
            prompt_cache_key,
            agent_id,
            view,
            parent,
            pool,
            replay::Replayed::default(),
            head,
        );
        Ok((agent_id, handle))
    }

    /// Replaying the log is the whole of recovery, and a loaded agent starts
    /// in the same phase a fresh one does: coming up is never by itself a
    /// reason to send. `SPEC-restart-recovery`.
    pub(crate) async fn load(
        db: RhoDb,
        inference: Inference,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<View>>>,
        pool: std::sync::Weak<AgentPool>,
    ) -> anyhow::Result<Self> {
        let record = db.read().get_agent(agent_id);
        let head = record.clone();
        let AgentRuntime::Rho { prompt_cache_key } = record.config.runtime else {
            anyhow::bail!("agent {agent_id:?} does not use the Rho runtime");
        };
        let profile = record
            .config
            .binding
            .deep_config()
            .ok_or_else(|| anyhow::anyhow!("Rho runtime stored with a Claude mode"))?;
        let model = record
            .config
            .binding
            .deep_model()
            .expect("deep config implies a deep model");
        let parent = record.parent;
        let (_, events) = db.read().agent_events(agent_id);
        let replayed = replay::replay(events);
        Ok(Self::start(
            db,
            inference,
            profile,
            model,
            record.config.role,
            prompt_cache_key,
            agent_id,
            view,
            parent,
            pool,
            replayed,
            head,
        ))
    }

    /// Shared tail of [`Self::create`] and [`Self::load`]: what a loaded
    /// agent starts as sits beside what a fresh one starts as.
    #[allow(clippy::too_many_arguments)]
    fn start(
        db: RhoDb,
        inference: Inference,
        profile: InferenceProfile,
        model: InferenceModel,
        role: AgentRole,
        prompt_cache_key: PromptCacheKey,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<View>>>,
        parent: Option<AgentId>,
        pool: std::sync::Weak<AgentPool>,
        replayed: replay::Replayed,
        head: AgentHead,
    ) -> Self {
        let session = inference.deep_session(profile, model, prompt_cache_key);
        let total_usage = db.read().agent_usage_total(agent_id);
        let title = crate::title::Task::new(inference.clone());
        let surface_inputs = SurfaceInputs {
            view: Arc::clone(&view),
            agent_id,
            inference,
            parent,
            pool: pool.clone(),
        };
        let status = Arc::new(RwLock::new(AgentStatus {
            kind: AgentStateKind::Idle,
            queued: 0,
        }));
        let head = Arc::new(RwLock::new(head));
        let (control, control_rx) = mpsc::unbounded_channel();
        let mut agent = Agent {
            db,
            agent_id,
            pool,
            surface: surface_inputs.lazy(role),
            model,
            context: replayed.context,
            session,
            // The same phase a fresh agent starts in. Being loaded from a
            // log is not its own kind of state, and coming up is never by
            // itself a reason to send:
            // `DECISION-a-restart-does-not-resume-by-itself`.
            phase: Phase::Idle {
                owed: replayed.owed,
                standing: Standing::Nothing,
            },
            user: replayed.user,
            mail: replayed.mail,
            execs: BTreeMap::new(),
            observations: Observations::default(),
            streams: BTreeMap::new(),
            recovery_notes: replayed.recovery_notes,
            recovery_blocks: replayed.recovery_blocks,
            recovery_streams: replayed.recovery_streams,
            context_used: replayed.context_used,
            turn: None,
            latest_python_exec: None,
            total_usage,
            title,
            working: false,
            wake: Arc::new(Notify::new()),
            status: Arc::clone(&status),
            head: Arc::clone(&head),
            teller: crate::live::Teller::default(),
            control: control.downgrade(),
            control_rx,
        };
        // Published before the loop's first decision, so a subscriber that
        // arrives immediately sees the replayed transcript.
        agent.publish_sync(None);
        tokio::spawn(agent.run());
        Self {
            control,
            status,
            head,
            view,
        }
    }

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

    pub fn send_user_content(&self, content: Vec<ContentPart>, delivery: MessageDelivery) {
        let _ = self.control.send(Control::User(
            QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Message { content },
                delivery,
                at: UnixMs::now(),
            },
            None,
        ));
    }

    /// Send user input and wait until the loop has durably queued it.
    pub async fn send_user_content_accepted(
        &self,
        content: Vec<ContentPart>,
        delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        self.send(|done| {
            Control::User(
                QueuedInput {
                    source: MessageSender::User,
                    kind: InputKind::Message { content },
                    delivery,
                    at: UnixMs::now(),
                },
                Some(done),
            )
        })
        .await
    }

    /// Deliver mail from a peer agent.
    pub fn send_agent_message(&self, sender: AgentId, text: impl Into<String>) {
        let _ = self.control.send(Control::Mail {
            sender,
            content: vec![ContentPart::Text { text: text.into() }],
            at: UnixMs::now(),
            done: None,
        });
    }

    /// Deliver mail and wait until the loop has durably queued it.
    pub async fn send_agent_message_accepted(
        &self,
        sender: AgentId,
        text: impl Into<String>,
    ) -> anyhow::Result<()> {
        let content = vec![ContentPart::Text { text: text.into() }];
        self.send(|done| Control::Mail {
            sender,
            content,
            at: UnixMs::now(),
            done: Some(done),
        })
        .await
    }

    /// The user explicitly asked to compact. Automatic compaction is not an
    /// input at all — it happens while building a request.
    pub fn compact(&self) {
        let _ = self.control.send(Control::User(
            QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Compaction,
                delivery: MessageDelivery::NextRequest,
                at: UnixMs::now(),
            },
            None,
        ));
    }

    /// Abort the in-flight request and durably discard queued inputs.
    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }

    /// Retry after a failure, or resume a request interrupted by a restart.
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

    pub fn change_prompt_cache_key(&self) {
        let _ = self
            .control
            .send(Control::ChangePromptCacheKey(PromptCacheKey::generate()));
    }

    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(Control::Rewind { turns, reply })
            .map_err(|_| anyhow::anyhow!("agent loop has stopped"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("agent loop has stopped"))?
    }

    /// Whether anyone is looking at this agent; titles and activity are
    /// made only then.

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
/// landed. The ack fires after the command's own handling, so anything it
/// persisted is on disk by then — not that the model has *seen* it: that
/// waits for a boundary the caller does not control.
enum Control {
    User(QueuedInput, Option<oneshot::Sender<()>>),
    Mail {
        sender: AgentId,
        content: Vec<ContentPart>,
        at: UnixMs,
        done: Option<oneshot::Sender<()>>,
    },
    Cancel,
    Retry,
    ChangeRole {
        role: AgentRole,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    ChangePromptCacheKey(PromptCacheKey),
    Rewind {
        turns: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    TitleFinished(Result<String, String>),
    /// Tell the live tail whole, for a client that just started looking.
    TellTail,
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
pub(crate) enum Phase {
    /// No request in flight. `owed` is what the next one has to open with —
    /// calls nothing is going to answer, which after a restart is every call
    /// history left hanging (`SPEC-restart-recovery`). Ordinarily empty.
    Idle {
        owed: Vec<rho_core::ExecId>,
        standing: Standing,
    },
    /// A request is in flight, and nothing but an interrupt may disturb it.
    Requesting(InFlight),
}

/// The notebook takes one cell per model response, and there is nothing else
/// to call.
/// The core's bookkeeping for one call: which tool, how much of its story the
/// model has, and since when it has been holding something. The output itself
/// lives in the session, which is asked for it at every boundary.
struct RunningExec {
    call: rho_core::ExecCall,
    first_block_at: UnixMs,
    session: Box<PythonCell>,
    answer: ReplyState,
}

impl RunningExec {
    fn output_order(&self, latest: Option<&ToolCallId>) -> (bool, u64) {
        (Some(&self.call.id) != latest, self.session.sequence())
    }

    fn sources(&self) -> impl Iterator<Item = SourceKind> + '_ {
        self.session
            .sources()
            .into_iter()
            .map(|(_, facts)| match facts {
                rho_agent_tools::SourceFacts::Cell(facts) => SourceKind::Cell {
                    facts,
                    latest: false,
                },
                rho_agent_tools::SourceFacts::Job(facts) => SourceKind::Job { facts },
            })
    }
}

/// The in-flight provider response. Its streamed Python call may already have
/// durable admission records and live work; abandoning the response preserves
/// that call before the next boundary drains its output.
#[derive(Clone, Default)]
pub(crate) struct InFlight {
    handoff: Vec<rho_core::ExecId>,
    pending: PendingInferenceResponse,
    /// The prior attempt's failure, displayed while its fresh continuation
    /// runs.
    previous_failure: Option<Arc<str>>,
    retry: Option<(UnixMs, u32)>,
    stream: Option<ToolCallId>,
    /// This request compacted on behalf of work that still owes a reply, so a
    /// compaction must not be where the agent stops. A fact about *this*
    /// request, so a request that never finishes never has to unset it.
    compaction_owes_reply: bool,
}

struct Agent {
    db: RhoDb,
    agent_id: AgentId,
    pool: std::sync::Weak<AgentPool>,
    surface: Arc<Lazy<Surface>>,
    model: InferenceModel,

    /// Live window policy; provider context itself is derived from the event
    /// log.
    context: context::Window,

    session: InferenceSession,
    phase: Phase,

    /// Typed input, in arrival order: discrete, never merged or summarised,
    /// and always drained in that order.
    user: Vec<QueuedInput>,
    /// Everyone's mail, in arrival order.
    mail: Vec<MailItem>,
    /// One entry per call the model has made and nothing has answered.
    execs: BTreeMap<ToolCallId, RunningExec>,
    /// When each pending event was first seen: the clocks the boundary's
    /// patiences run on.
    observations: Observations,
    streams: BTreeMap<ToolCallId, streaming::Stream>,
    recovery_notes: Vec<String>,
    recovery_blocks: Vec<ContextBlock>,
    recovery_streams: Vec<ToolCallId>,

    context_used: Option<u64>,
    /// What the model's latest turn settled about being looked in on. `None`
    /// until it has spoken once — after a restart included, which is safe
    /// because no tool survives one.
    turn: Option<ModelTurn>,
    latest_python_exec: Option<(ToolCallId, Arc<rho_agent_tools::PythonExec>)>,
    /// Cumulative provider-reported usage across this agent's requests.
    total_usage: AgentUsageBucket,
    title: crate::title::Task,
    /// Whether the last published state counted as a running turn, so the
    /// story is told once per edge.
    working: bool,

    /// Content-free signal that some tool changed. Tools hold a
    /// [`SourceWaker`] over this; the core rescans rather than being told.
    wake: Arc<Notify>,

    status: Arc<RwLock<AgentStatus>>,
    head: Arc<RwLock<AgentHead>>,
    /// What clients have been told of the tail, so each publish says only
    /// what changed.
    teller: crate::live::Teller,
    control: mpsc::WeakUnboundedSender<Control>,
    control_rx: mpsc::UnboundedReceiver<Control>,
}

impl Drop for Agent {
    fn drop(&mut self) {
    }
}

impl Agent {
    /// Answer the one question after every event, act on the answer, and wait
    /// for the next one, until the last handle is dropped.
    async fn run(mut self) {
        loop {
            // One question per event. Either it says to wait and hands over the
            // timer — so the timer and the rule behind it cannot drift apart —
            // or it says to send, and a request in flight is never waited for.
            let now = UnixMs::now();
            // An idle agent settles its streams first, so the cell facts the
            // decision reads are the ones an admitted statement produced.
            // In flight, admission waits on the decision: a statement is not
            // admitted into a request about to be thrown away.
            if matches!(self.phase, Phase::Idle { .. }) {
                self.advance_streams(now, true).await;
            }
            let decision = self.decide(now);
            if matches!(self.phase, Phase::Requesting(_)) {
                self.advance_streams(now, decision != Boundary::AbortAndResend)
                    .await;
            }
            let deadline = match decision {
                Boundary::No { recheck } => recheck,
                Boundary::AbortAndResend => {
                    // Stop admission and preserve any executed call before
                    // draining fresh input into the replacement request.
                    self.abandon_stream(now).await;
                    self.session.abort();
                    self.start_request(now, Some(crate::WakeFacts::interrupt()))
                        .await;
                    None
                }
                Boundary::RetryExhausted => {
                    let Phase::Idle {
                        standing: Standing::Retry { error, .. },
                        ..
                    } = &self.phase
                    else {
                        unreachable!()
                    };
                    let error = format!("Provider retry window exhausted: {error}");
                    self.fail(now, PendingInferenceResponse::default(), error)
                        .await;
                    None
                }
                Boundary::Now { wake } => {
                    self.start_request(now, Some(wake)).await;
                    None
                }
            };
            self.publish(deadline).await;
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

    /// The one question, asked of everything the loop knows.
    fn decide(&mut self, now: UnixMs) -> Boundary {
        let sources = self.sources();
        boundary(
            &sources,
            self.turn.as_ref(),
            match &self.phase {
                Phase::Idle { standing, .. } => Some(standing),
                Phase::Requesting(_) => None,
            },
            &mut self.observations,
            now,
        )
    }

    /// Every source, in whatever state it is in — nothing is filtered out for
    /// having nothing to say, because deciding that is the decision's job, and
    /// an empty queue is a fact it reads.
    fn sources(&self) -> Vec<SourceKind> {
        let mut sources = vec![
            SourceKind::User {
                interrupt: self
                    .user
                    .iter()
                    .any(|input| input.delivery == MessageDelivery::Immediate),
                // Arrival order, so the first is the one that has waited
                // longest — the one whose patience is being spent.
                oldest_at: self.user.first().map(|input| input.at),
            },
            SourceKind::Mail {
                oldest_at: self.mail.first().map(|item| item.at),
                newest_at: self.mail.last().map(|item| item.at),
            },
        ];
        // What each call is, with nothing decided about it: every one of
        // these is something the tool observed, and what any of them is
        // worth is `boundary`'s business.
        for tool in self.execs.values() {
            let latest = self
                .latest_python_exec
                .as_ref()
                .is_some_and(|(id, _)| *id == tool.call.id);
            sources.extend(tool.sources().map(|source| match source {
                SourceKind::Cell { facts, .. } => SourceKind::Cell { facts, latest },
                other => other,
            }));
        }
        // A quiet completed exec may leave the transcript session, but its
        // check-in remains authoritative for this model turn.
        if let Some((id, exec)) = &self.latest_python_exec
            && !self.execs.contains_key(id)
        {
            sources.push(SourceKind::Cell {
                facts: exec.facts(),
                latest: true,
            });
        }
        if self
            .head
            .read()
            .expect("poison")
            .config
            .role
            .uses_notes_rotation()
            && let Some(preparation) = &self.context.preparation
        {
            let returned = preparation.call.as_ref().is_none_or(|id| {
                self.execs.get(id).is_none_or(|tool| {
                    tool.session
                        .sources()
                        .iter()
                        .all(|(_, source)| match source {
                            rho_agent_tools::SourceFacts::Cell(facts) => facts.returned.is_some(),
                            rho_agent_tools::SourceFacts::Job(_) => true,
                        })
                })
            });
            sources.push(SourceKind::Preparation {
                replied: preparation.replied,
                returned,
            });
        }
        sources
    }

    /// The single funnel. Every event lands here and does nothing but update
    /// state; what to do about it is asked once, by the caller.
    async fn handle(&mut self, event: Event) {
        let now = UnixMs::now();
        match event {
            Event::Control(control) => self.handle_control(control, now).await,
            // Anything the model says outside a request of ours is somebody
            // else's, or the tail of one already abandoned.
            Event::Inference(event) => {
                let Phase::Requesting(in_flight) = &mut self.phase else {
                    return;
                };
                match event {
                    InferenceEvent::RequestSent => {
                        let handed_off = std::mem::take(&mut in_flight.handoff);
                        for id in handed_off {
                            self.persist(AgentEvent::ExecObserved {
                                id,
                                milestone: rho_core::ExecMilestone::HandedOff,
                                at: now,
                            })
                            .await;
                        }
                    }
                    InferenceEvent::StreamingStarted => {}
                    InferenceEvent::ExecArgumentsFinished { id } => {
                        self.persist(AgentEvent::ExecObserved {
                            id,
                            milestone: rho_core::ExecMilestone::ArgumentsFinished,
                            at: now,
                        })
                        .await;
                    }
                    InferenceEvent::ContextItem { index, event } => {
                        in_flight.pending.apply(index, event);
                        if let Err(error) = self.update_stream(now).await {
                            let Phase::Requesting(in_flight) = &mut self.phase else {
                                unreachable!()
                            };
                            let partial = std::mem::take(&mut in_flight.pending);
                            self.session.abort();
                            self.fail(now, partial, error).await;
                        }
                    }
                    InferenceEvent::TemporaryFailure { error, .. } => {
                        let (since, attempts) = in_flight.retry.unwrap_or((now, 0));
                        let compaction_owes_reply = in_flight.compaction_owes_reply;
                        let partial = std::mem::take(&mut in_flight.pending);
                        let error = error.to_string();
                        let has_execution = self.abandon_stream(now).await;
                        self.persist(AgentEvent::Native(NativeEvent::RequestFailed {
                            partial,
                            error: error.clone(),
                            retrying: true,
                            at: now,
                        }))
                        .await;
                        self.session.abort();
                        if has_execution {
                            // Accepted Python is an ordinary model-issued exec,
                            // not a transport retry that may bypass its sources.
                            self.turn = Some(ModelTurn {
                                spoke_at: now,
                                asked: ModelAsked::Calls,
                            });
                        }
                        self.phase = Phase::Idle {
                            owed: Vec::new(),
                            standing: if has_execution {
                                Standing::Nothing
                            } else {
                                Standing::Retry {
                                    since,
                                    failed_at: now,
                                    attempts: attempts + 1,
                                    compaction_owes_reply,
                                    error: Arc::from(error),
                                }
                            },
                        };
                    }
                    // Nothing to abort: the request is already over, and it
                    // is the agent that stops here rather than the request.
                    InferenceEvent::Failed { error } => {
                        let partial = std::mem::take(&mut in_flight.pending);
                        self.fail(now, partial, error.to_string()).await;
                    }
                    InferenceEvent::Finished {
                        usage,
                        provider_response_id,
                    } => {
                        let finished = in_flight.pending.finish();
                        let exec = in_flight.stream.clone();
                        if let Some(id) = exec {
                            self.persist(AgentEvent::ExecObserved {
                                id,
                                milestone: rho_core::ExecMilestone::ResponseFinished,
                                at: now,
                            })
                            .await;
                        }
                        match finished {
                            // Finished streaming, but what arrived does not
                            // assemble into a response.
                            Err(error) => {
                                let Phase::Requesting(in_flight) = &mut self.phase else {
                                    unreachable!()
                                };
                                let partial = std::mem::take(&mut in_flight.pending);
                                self.fail(now, partial, error.to_string()).await
                            }
                            Ok(items) => {
                                self.finish_request(items, provider_response_id, usage, now)
                                    .await
                            }
                        }
                    }
                }
            }
            // Both are pure prompts to re-ask the question; what a source
            // reports is read live, so there is nothing to record here.
            Event::SourceChanged | Event::Tick => {}
        }
    }

    async fn handle_control(&mut self, control: Control, now: UnixMs) {
        match control {
            Control::TellTail => {
                self.teller.reset();
                let kind = self.status.read().expect("poison").kind.clone();
                self.tell(&kind);
            }
            Control::User(input, done) => {
                self.persist(AgentEvent::Accepted(input.clone())).await;
                if let InputKind::Message { content } = &input.kind
                    && !rho_core::text_content(content).trim().is_empty()
                {
                    self.name(&rho_core::text_content(content)).await;
                }
                // Queueing it is the whole of it. Whether this revives an
                // agent that had stopped is `Standing::stopped`'s reading of
                // the very queue this pushes onto, so there is no second
                // place for it to be written down and go stale.
                self.user.push(input);
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            Control::Mail {
                sender,
                content,
                at,
                done,
            } => {
                self
                    .persist(AgentEvent::Accepted(QueuedInput {
                        source: MessageSender::Agent { id: sender },
                        kind: InputKind::Message {
                            content: content.clone(),
                        },
                        delivery: MessageDelivery::NextRequest,
                        at,
                    }))
                    .await;
                if !rho_core::text_content(&content).trim().is_empty() {
                    self.name(&rho_core::text_content(&content)).await;
                }
                self.mail.push(MailItem {
                    sender,
                    content,
                    at,
                });
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            Control::Cancel => {
                self.context.preparation = None;
                // Ask every tool to wind down, then keep reading it: the core
                // does not kill tools, so a tool still chooses its own last
                // words.
                self.abandon_stream(now).await;
                for tool in self.execs.values_mut() {
                    tool.session.cancel();
                }
                // A cancel is not an answer, so what is owed outlives it.
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
                    self.persist(AgentEvent::Cleared { at: now }).await;
                    self.user.clear();
                    self.mail.clear();
                }
            }
            Control::Retry => {
                // Hurries the next request rather than changing what has to
                // be in it; nothing to hurry while one is in flight.
                if let Phase::Idle { standing, .. } = &mut self.phase {
                    *standing = Standing::Asked;
                }
            }
            Control::ChangeRole { role, reply } => {
                let _ = reply.send(self.change_role(role).await);
            }
            Control::ChangePromptCacheKey(key) => {
                let mut write = self.db.write().await;
                write.set_agent_prompt_cache_key(self.agent_id, key);
                write.commit();
                self.session.set_prompt_cache_key(key);
            }
            Control::Rewind { turns, reply } => {
                let _ = reply.send(self.rewind(turns).await);
            }
            Control::TitleFinished(result) => crate::title::finish(&self.db, self.agent_id, result).await,
        }
    }

    /// The request is over and the agent stops. What the model had said
    /// goes to the log first, so the reader keeps it and the turn's end
    /// follows its row.
    async fn fail(&mut self, now: UnixMs, partial: PendingInferenceResponse, error: String) {
        self.abandon_stream(now).await;
        // A terminal failure must not leave an unreplied preparation gate
        // blocking fresh input or an explicit retry.
        self.context.preparation = None;
        self.persist(AgentEvent::Native(NativeEvent::RequestFailed {
            partial,
            error: error.clone(),
            retrying: false,
            at: now,
        }))
        .await;
        self.phase = Phase::Idle {
            owed: Vec::new(),
            standing: Standing::Failed {
                at: now,
                error: Arc::from(error.as_str()),
            },
        };
        if let Some(pool) = self.pool.upgrade() {
            pool.publish_failed_turn(self.agent_id, error).await;
        }
    }

    // -- the user's commands that reach into the transcript -----------------

    /// Nothing may be in motion: a change of transcript or of surface under
    /// a running request or a live tool would leave one of them lying.
    fn ensure_settled(&self, what: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.phase, Phase::Idle { .. }),
            "{what} is only available while idle or errored; cancel the turn first"
        );
        anyhow::ensure!(
            self.user.is_empty() && self.mail.is_empty(),
            "{what} is not available with queued inputs"
        );
        anyhow::ensure!(
            self.execs.is_empty() && !self.session.has_active_request(),
            "{what} is not available while work is running"
        );
        Ok(())
    }

    async fn change_role(&mut self, requested: AgentRole) -> anyhow::Result<()> {
        self.ensure_settled("a role change")?;
        let requested = match requested {
            AgentRole::Engineer { intelligence } => intelligence,
            _ => anyhow::bail!("role changes currently support only engineer roles"),
        };
        let switchable = |intelligence| {
            matches!(
                intelligence,
                EngineerIntelligence::Low
                    | EngineerIntelligence::Cheap
                    | EngineerIntelligence::Medium
                    | EngineerIntelligence::High
                    | EngineerIntelligence::HighNotes
            )
        };
        anyhow::ensure!(
            switchable(requested),
            "this agent can switch only between eng-low, eng-cheap, eng, eng-high, and eng-high-notes"
        );
        let current = self.head.read().expect("poison").config.role;
        let role = match current {
            AgentRole::Engineer { intelligence } if switchable(intelligence) => {
                AgentRole::Engineer {
                    intelligence: requested,
                }
            }
            _ => anyhow::bail!(
                "this agent can switch only between eng-low, eng-cheap, eng, eng-high, and eng-high-notes"
            ),
        };
        if role == current {
            return Ok(());
        }
        let binding = role.session_profile()?;
        let profile = binding
            .deep_config()
            .ok_or_else(|| anyhow::anyhow!("role change would leave the Rho runtime"))?;
        let model = binding
            .deep_model()
            .ok_or_else(|| anyhow::anyhow!("role change has no Rho model"))?;
        anyhow::ensure!(
            self.session.set_deep_config(profile, model),
            "agent does not have a configurable inference session"
        );
        let mut write = self.db.write().await;
        write.set_agent_profile(self.agent_id, role, binding);
        write.commit();
        {
            let mut head = self.head.write().expect("poison");
            head.config.role = role;
            head.config.binding = binding;
        }
        if current.uses_notes_rotation() != role.uses_notes_rotation() {
            self.context.rotated();
            // Same-model roles still change the developer instructions.
            self.session.abort();
        }
        self.model = model;
        // Instructions are rendered from the current role on each request.
        // The session's tool surface (including Python globals) stays alive.
        Ok(())
    }

    /// Branch history before the `turns`-th last user message and start
    /// again from there. `DECISION-history-only-branches`: what the agent
    /// walked away from stays in the log.
    async fn rewind(&mut self, turns: u32) -> anyhow::Result<()> {
        anyhow::ensure!(turns > 0, ":rewind turns must be greater than zero");
        self.ensure_settled(":rewind")?;
        let cursor = {
            let (_, records) = self.db.read().agent_event_records(self.agent_id);
            let user_positions = records
                .iter()
                .filter(|(_, event)| event.is_user_message())
                .map(|(pos, _)| *pos)
                .collect::<Vec<_>>();
            if user_positions.is_empty() {
                None
            } else {
                let index = user_positions.len().saturating_sub(turns as usize);
                Some(user_positions[index])
            }
        };
        let Some(cursor) = cursor else {
            anyhow::bail!("nothing to rewind");
        };
        {
            let mut write = self.db.write().await;
            write.rewind_agent(UnixMillis::now(), self.agent_id, cursor);
            write.commit();
        }
        let (_, events) = self.db.read().agent_events(self.agent_id);
        let replayed = replay::replay(events);
        self.context = replayed.context;
        self.recovery_notes = replayed.recovery_notes;
        self.recovery_blocks = replayed.recovery_blocks;
        self.recovery_streams = replayed.recovery_streams;
        self.streams.clear();
        self.latest_python_exec = None;
        self.user = replayed.user;
        self.mail = replayed.mail;
        self.context_used = replayed.context_used;
        self.phase = Phase::Idle {
            owed: replayed.owed,
            standing: Standing::Nothing,
        };
        self.turn = None;
        self.observations.clear();
        self.session.abort();
        // A rewind is told, not undone: the last title and activity still
        // stand.
        Ok(())
    }

    async fn name(&mut self, input: &str) {
        let control = self.control.clone();
        self.title.start(&self.db, self.agent_id, input, move |result| {
            if let Some(control) = control.upgrade() {
                let _ = control.send(Control::TitleFinished(result));
            }
        }).await;
    }

    // -- acting on it -------------------------------------------------------

    async fn start_request(&mut self, now: UnixMs, wake: Option<crate::WakeFacts>) {
        // Tools and instructions come from the workdirs, which are only
        // opened now: a load never fails on them, a turn may.
        let surface = match self.surface.get().await {
            Ok(surface) => surface,
            Err(error) => {
                self.fail(
                    now,
                    PendingInferenceResponse::default(),
                    format!("{error:#}"),
                )
                .await;
                return;
            }
        };
        let role = self.head.read().expect("poison").config.role;
        let prompt = match surface.prompt.render(role).await {
            Ok(prompt) => prompt,
            Err(error) => {
                self.fail(
                    now,
                    PendingInferenceResponse::default(),
                    format!("{error:#}"),
                )
                .await;
                return;
            }
        };
        let notes_rotation = role.uses_notes_rotation();
        let instructions = if !notes_rotation && self.context.marker.is_some() {
            // Older experimental builds emitted rotation notices for ordinary roles.
            Arc::from(format!(
                "{}\n\nThis role uses standard provider compaction, not notes rotation. Earlier retention and preparation notices are canceled; continue the user's task.",
                prompt.text
            ))
        } else {
            Arc::clone(&prompt.text)
        };
        let notes_path = prompt.notes;
        // What is owed is settled here and nowhere earlier:
        // `SPEC-restart-recovery`.
        let previous_failure = match &self.phase {
            Phase::Idle {
                standing: Standing::Retry { error, .. },
                ..
            } => Some(error.clone()),
            _ => None,
        };
        let retry = match &self.phase {
            Phase::Idle {
                standing:
                    Standing::Retry {
                        since, attempts, ..
                    },
                ..
            } => Some((*since, *attempts)),
            _ => None,
        };
        let history = self.provider_input();
        let pending_compaction = history
            .iter()
            .skip(rho_core::context_window_start(&history))
            .rev()
            .find_map(|block| match &**block {
                ContextBlock::CompactionTrigger => Some(true),
                ContextBlock::InferenceResponse { .. } | ContextBlock::ContextRotation { .. } => {
                    Some(false)
                }
                _ => None,
            })
            .unwrap_or(false);
        let manual = pending_compaction
            || self
                .user
                .iter()
                .any(|input| matches!(input.kind, InputKind::Compaction));
        let cancel_rotation = manual && self.context.marker.is_some();
        if manual {
            self.context.rotated();
        }
        // Explicit compaction always uses the provider, including transport retries.
        let notes_rotation = notes_rotation && !manual;
        self.session.set_context_rotation(notes_rotation);
        if !notes_rotation {
            self.context.preparation = None;
        }
        let retry_owes_reply = matches!(
            &self.phase,
            Phase::Idle {
                standing: Standing::Retry {
                    compaction_owes_reply: true,
                    ..
                },
                ..
            }
        );
        let prior_preparation = self.context.preparation.clone();
        let limit = self.session.auto_compact_token_limit();
        let mut rotate = None;
        let mut change = if !notes_rotation {
            None
        } else if let Some(preparation) = &prior_preparation {
            if !preparation.replied {
                Some(crate::ContextChange::Preparing {
                    retain_from: preparation.retain_from as u64,
                    repair: preparation.repair,
                })
            } else {
                let failed = preparation.call.as_ref().is_some_and(|id| {
                    self.execs.get(id).is_some_and(|tool| {
                        tool.session.sources().iter().any(|(_, source)| {
                        matches!(source, rho_agent_tools::SourceFacts::Cell(facts) if facts.failed)
                    })
                    })
                });
                let headroom = self
                    .session
                    .context_window()
                    .zip(self.context_used)
                    .is_some_and(|(window, used)| {
                        window.saturating_sub(used) >= context::REPAIR_HEADROOM
                    });
                if failed && !preparation.repair && headroom {
                    Some(crate::ContextChange::Preparing {
                        retain_from: preparation.retain_from as u64,
                        repair: true,
                    })
                } else {
                    rotate = Some(preparation.retain_from);
                    None
                }
            }
        } else if limit
            .zip(self.context_used)
            .is_some_and(|(limit, used)| used >= limit)
        {
            Some(crate::ContextChange::Preparing {
                retain_from: self
                    .context
                    .marker
                    .unwrap_or_else(|| self.context.fallback_start(&history))
                    as u64,
                repair: false,
            })
        } else {
            None
        };
        let preparing = matches!(change, Some(crate::ContextChange::Preparing { .. }));
        let preparation_call = prior_preparation
            .as_ref()
            .and_then(|preparation| preparation.call.as_ref());
        let owed = match &mut self.phase {
            Phase::Idle { owed, .. } => std::mem::take(owed),
            Phase::Requesting(_) => Vec::new(),
        };
        let delivered = self
            .execs
            .iter()
            .filter(|(id, tool)| {
                !preparing || tool.answer == ReplyState::Owed || preparation_call == Some(*id)
            })
            .map(|(id, _)| id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut blocks: Vec<ContextBlock> = std::mem::take(&mut self.recovery_blocks)
            .into_iter()
            .collect();
        if cancel_rotation {
            blocks.push(ContextBlock::DeveloperMessage {
                text: context::MANUAL_COMPACTION.into(),
            });
        }
        self.collect_stream_notes(Some(&delivered));
        if !owed.is_empty() {
            blocks.extend(owed.iter().map(|id| {
                rho_inference::exec::output(&rho_core::ExecOutput::Reply {
                    id: id.clone(),
                    body: ToolOutput {
                        full_output: None,
                        images: Default::default(),
                        output: Arc::new(String::new()),
                        status: ToolOutputStatus::Cancelled,
                    },
                    first_block_at: now,
                    at: now,
                })
            }));
            // What the empty results cannot say themselves.
            blocks.push(ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: vec![ContentPart::Text {
                    // The prose half of what the request owes the model;
                    // the empty results above are the other half.
                    text: "note: rho restarted. Every tool that was running is gone — foreground \
                           and background alike — and their external side effects may remain. The empty tool results above are placeholders, not output. Consult streaming progress notes before deciding what is safe to repeat."
                        .to_owned(),
                }],
            });
        }
        // Every source, not just whichever one triggered the boundary:
        // `DECISION-pull-based-sources`. The order is protocol-constrained
        // rather than chronological, so tool output leads, and each call's
        // first contribution becomes its `ToolResult` and every later one a
        // `ToolUpdate`, because a provider accepts exactly one result per call
        // id: `REQ-provider-transcript-protocol`.
        let latest = self.latest_python_exec.as_ref().map(|(id, _)| id);
        let mut tools = self.execs.values_mut().collect::<Vec<_>>();
        tools.sort_by_key(|tool| tool.output_order(latest));
        for tool in tools {
            if !delivered.contains(&tool.call.id) {
                continue;
            }
            // Whatever the tool is reporting: a request that leaves one call
            // unanswered is rejected whole, so the first drain after a call is
            // made answers it and the tool says what it has, even if that is
            // nothing yet. The facts are for `boundary` and are not read
            // here, nor is `done`, which is asked below.
            match tool.answer {
                ReplyState::Owed => {
                    let mut body = tool.session.first_output();
                    if self
                        .streams
                        .get(&tool.call.id)
                        .is_some_and(|stream| stream.interrupted)
                    {
                        let execution = if body.status == ToolOutputStatus::Cancelled {
                            "Execution was cancelled."
                        } else {
                            "Execution was not cancelled."
                        };
                        body.output = Arc::new(format!(
                            "Your response was interrupted while generating this tool call. {execution} Continue from the existing state without replaying this call.\n\n{}",
                            body.output,
                        ));
                    }
                    blocks.push(rho_inference::exec::output(&rho_core::ExecOutput::Reply {
                        id: tool.call.id.clone(),
                        body,
                        first_block_at: tool.first_block_at,
                        at: now,
                    }));
                }
                ReplyState::Sent => {
                    if let Some(output) = tool.session.more_output() {
                        blocks.push(rho_inference::exec::output(&rho_core::ExecOutput::Report {
                            id: tool.call.id.clone(),
                            body: output,
                            at: now,
                        }));
                    }
                }
            }
        }
        // Everything pending went into this request; the next event's clock
        // starts fresh.
        self.observations.clear();
        if !preparing {
            // One block per sender: several messages from the same peer collapse,
            // so a chatty one costs the model one block rather than five.
            let mut by_sender: BTreeMap<AgentId, Vec<ContentPart>> = BTreeMap::new();
            for item in std::mem::take(&mut self.mail) {
                by_sender
                    .entry(item.sender)
                    .or_default()
                    .extend(item.content);
            }
            blocks.extend(by_sender.into_iter().map(|(sender, content)| {
                ContextBlock::UserMessage {
                    sender: MessageSender::Agent { id: sender },
                    content,
                }
            }));

            // Ordinary input enters only outside the dedicated preparation exchange.
            let mut inputs = std::mem::take(&mut self.user);
            inputs.sort_by_key(|input| matches!(input.kind, InputKind::Compaction));
            blocks.extend(inputs.into_iter().filter_map(|input| match input.kind {
                InputKind::Message { content } => Some(ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content,
                }),
                InputKind::Compaction if notes_rotation => None,
                InputKind::Compaction => Some(ContextBlock::CompactionTrigger),
            }));
        }

        for text in std::mem::take(&mut self.recovery_notes) {
            let index = blocks
                .iter()
                .position(|block| matches!(block, ContextBlock::CompactionTrigger))
                .unwrap_or(blocks.len());
            blocks.insert(
                index,
                ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content: vec![ContentPart::Text { text }],
                },
            );
        }

        if let Some(crate::ContextChange::Preparing {
            retain_from,
            repair,
        }) = &change
        {
            let text = if *repair {
                context::REPAIR.to_owned()
            } else if self.context.marker.is_some() {
                context::PREPARE.to_owned()
            } else {
                context::fallback_notice(&history, *retain_from as usize)
            };
            blocks.push(ContextBlock::DeveloperMessage { text });
        } else if let Some(retain_from) = rotate {
            blocks.push(ContextBlock::ContextRotation {
                retain_from: retain_from as u64,
            });
            let inventory = if let Some(path) = notes_path {
                tokio::task::spawn_blocking(move || notes::inventory(&path))
                    .await
                    .unwrap_or_else(|_| "Notes inventory unavailable.".into())
            } else {
                "Notes inventory unavailable.".into()
            };
            blocks.push(ContextBlock::DeveloperMessage {
                text: format!(
                    "Context has rotated. Older conversation before the announced boundary is no \
                     longer in context; the retained conversation and preparation exchange remain. \
                     The earlier retention and preparation notices are now fulfilled; wait for a new \
                     notice before preparing for another rotation. Python state and running jobs were \
                     preserved. Read relevant notes and use the retained conversation and tool state \
                     to build on the work already done and avoid duplicating work. Continue the user's task.\n\n{inventory}"
                ),
            });
        } else if notes_rotation
            && self.context.marker.is_none()
            && limit
                .zip(self.context_used)
                .is_some_and(|(limit, used)| used >= limit.saturating_sub(context::RETAIN_TOKENS))
        {
            change = Some(crate::ContextChange::Marked {
                retain_from: (history.len() + blocks.len()) as u64,
            });
            blocks.push(ContextBlock::DeveloperMessage {
                text: context::MARKER.to_owned(),
            });
        }

        // Standard roles retain the existing explicit provider-compaction path.
        let compacting_already =
            pending_compaction || blocks.contains(&ContextBlock::CompactionTrigger);
        let compact = !notes_rotation
            && !compacting_already
            && limit
                .zip(self.context_used)
                .is_some_and(|(limit, used)| used >= limit);
        if compact {
            blocks.push(ContextBlock::CompactionTrigger);
        }
        let compaction_owes_reply = !preparing
            && (retry_owes_reply || compact
                || blocks
                    .iter()
                    .any(|block| !matches!(block, ContextBlock::CompactionTrigger)
                        && !matches!(block, ContextBlock::DeveloperMessage { text } if text == context::MANUAL_COMPACTION)));

        let handoff = blocks
            .iter()
            .filter_map(|block| match block {
                ContextBlock::ToolResults { results } => Some(results),
                _ => None,
            })
            .flatten().map(|result| result.call_id.clone())
            .collect::<Vec<_>>();
        for id in &handoff {
            self.persist(AgentEvent::ExecObserved {
                id: id.clone(),
                milestone: rho_core::ExecMilestone::Boundary,
                at: now,
            })
            .await;
        }

        // The drain, the append and the send are one event because they are one
        // thing: a crash between them would leave a transcript nobody drained
        // into and a queue nobody emptied.
        self.persist(AgentEvent::Native(NativeEvent::RequestStarted {
            input: blocks,
            context: change.clone(),
            wake,
            at: now,
        }))
        .await;
        if let Some(change) = &change {
            self.context.sent(change);
        }
        if rotate.is_some() {
            self.context.rotated();
            self.context_used = None;
            self.session.abort();
        }
        for (id, exec) in &mut self.execs {
            if delivered.contains(id) && exec.session.acknowledge_output() {
                exec.answer = ReplyState::Sent;
            }
        }
        self.execs
            .retain(|id, exec| !delivered.contains(id) || !exec.session.done());
        self.acknowledge_streams(now, &delivered).await;
        self.session.request(InferenceRequest {
            instructions,
            input: self.provider_input(),

            agent_id_labels: Default::default(),
        });
        self.phase = Phase::Requesting(InFlight {
            handoff,
            retry,
            previous_failure,
            compaction_owes_reply,
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
        if let Err(error) = self.finish_stream(&items) {
            self.fail(now, PendingInferenceResponse::default(), error)
                .await;
            return;
        }
        let compacted = items
            .iter()
            .any(|item| matches!(item, InferenceResponseItem::Compaction { .. }));
        let context_used = if compacted {
            None
        } else {
            usage
                .as_ref()
                .map(|usage| usage.input_tokens + usage.output_tokens)
                .or(self.context_used)
        };
        self.context_used = context_used;

        let call = match rho_inference::exec::call(&items) {
            Ok(call) => call,
            Err(error) => {
                self.fail(now, PendingInferenceResponse::default(), error.into())
                    .await;
                return;
            }
        };
        let final_text = call.is_none().then(|| final_answer_text(&items));

        let preparing = self.context.preparation.is_some();
        self.context.replied(&items);
        // What the response cost rides on the reply itself, so a reader
        // can price the transcript from the log alone.
        let turn_usage = usage.as_ref().map(|usage| AgentUsageBucket {
            model: usage_model(self.model),
            input_tokens: usage
                .input_tokens
                .saturating_sub(usage.cached_input_tokens)
                .saturating_sub(usage.cache_write_input_tokens),
            cache_read_tokens: usage.cached_input_tokens,
            cache_write_tokens: usage.cache_write_input_tokens,
            output_tokens: usage.output_tokens,
            requests: 1,
            ..AgentUsageBucket::default()
        });
        self
            .persist(AgentEvent::Native(NativeEvent::ResponseFinished {
                output: vec![ContextBlock::InferenceResponse { items, provider_response_id }],
                context_used,
                usage: turn_usage.clone(),
                at: now,
            }))
            .await;

        if let Some(turn_usage) = turn_usage {
            self.total_usage.add(&turn_usage);
            if let Some(pool) = self.pool.upgrade() {
                pool.record_agent_usage(self.agent_id, turn_usage).await;
            }
        }

        // Everything the request carried goes with it, except whether it still
        // owes a reply.
        let owed_a_reply = match &self.phase {
            Phase::Requesting(in_flight) => in_flight.compaction_owes_reply,
            // Only a request in flight can finish.
            Phase::Idle { .. } => false,
        };

        // A turn that issues no calls buys no further look-in: whatever is
        // still running speaks for itself.
        self.latest_python_exec = None;
        self.turn = Some(ModelTurn {
            spoke_at: now,
            asked: if call.is_some() {
                ModelAsked::Calls
            } else {
                ModelAsked::Nothing
            },
        });
        if let Some(call) = call {
            if let Some(stream) = self.streams.get_mut(&call.id) {
                stream.canonical = true;
                self.latest_python_exec = Some((call.id.clone(), stream.exec.clone()));
            } else {
                self.start_exec(call, now);
            }
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

        // A reply with no calls is the model handing back: whoever is
        // subscribed to this agent's answers gets it as mail and the sidecar
        // classifies it.
        if let Some(final_text) = final_text
            && !compacted
            && !preparing
        {
            if let Some(pool) = self.pool.upgrade() {
                pool.publish_completed_turn(AgentTurnCompleted {
                    agent_id: self.agent_id,
                    final_answer: final_text.clone(),
                })
                .await;
            }
        }
    }

    // -- tools --------------------------------------------------------------

    fn start_exec(&mut self, call: rho_core::ExecCall, now: UnixMs) {
        let session = self
            .surface
            .get_if_ready()
            .expect("the notebook is initialized before inference")
            .notebook
            .exec(call.clone(), SourceWaker::new(Arc::clone(&self.wake)));
        self.latest_python_exec = Some((call.id.clone(), session.execution()));
        self.execs.insert(
            call.id.clone(),
            RunningExec {
                first_block_at: now,
                call,
                session,
                answer: ReplyState::Owed,
            },
        );
    }

    // -- plumbing -----------------------------------------------------------

    /// Provider context is a disposable projection of the committed event log.
    /// Neither execution nor streaming maintains a second authoritative
    /// transcript.
    fn provider_input(&self) -> Vec<Arc<ContextBlock>> {
        let (_, events) = self.db.read().agent_events(self.agent_id);
        replay::replay(events).history
    }

    /// Append to the raw log; the journal and every mirror follow from it.
    async fn persist(&mut self, event: AgentEvent<'_>) -> AgentEventPos {
        let mut write = self.db.write().await;
        let at = write.append_agent_event(self.agent_id, &event);
        write.commit();
        at
    }

    /// What a reader sees, built from the loop's own state. `deadline` is
    /// when the decision said to look again, if it can change by itself.
    fn status(&self, deadline: Option<UnixMs>) -> AgentStatus {
        let kind = match &self.phase {
            Phase::Requesting(in_flight) => AgentStateKind::ApiStreaming {
                pending_response: in_flight.pending.clone(),
                previous_attempt: in_flight.previous_failure.as_ref().map(|error| {
                    FailedInferenceResponse {
                        partial_response: PendingInferenceResponse::default(),
                        attempt_count: NonZeroU64::new(
                            in_flight
                                .retry
                                .map_or(0, |(_, attempts)| u64::from(attempts)),
                        )
                        .unwrap_or(NonZeroU64::MIN),
                        error: Arc::new(error.to_string()),
                    }
                }),
            },
            Phase::Idle { standing, .. }
                if standing.stopped(self.user.first().map(|input| input.at)) =>
            {
                match standing {
                    Standing::Failed { error, .. } => {
                        AgentStateKind::Error(FailedInferenceResponse {
                            partial_response: PendingInferenceResponse::default(),
                            attempt_count: NonZeroU64::MIN,
                            error: Arc::new(error.to_string()),
                        })
                    }
                    _ => AgentStateKind::Idle,
                }
            }
            Phase::Idle { owed, .. } if !owed.is_empty() => AgentStateKind::UnfinishedTurn {
                outstanding_calls: owed.clone().into(),
            },
            // The model is waiting on a call, or asked to be woken, or
            // something queued is about to go: a turn is running.
            Phase::Idle { .. }
                if self
                    .execs
                    .values()
                    .any(|tool| tool.answer == ReplyState::Owed)
                    || self.context.preparation.is_some()
                    || deadline.is_some() =>
            {
                AgentStateKind::ToolCalling {
                    previews: self
                        .execs
                        .iter()
                        .map(|(id, tool)| {
                            (
                                id.clone(),
                                ToolPreview {
                                    call: ToolCall {
                                        id: tool.call.id.clone(),
                                        name: ToolName::try_from("exec").unwrap(),
                                        tool_type: rho_core::ToolType::Custom,
                                        arguments: tool.call.source.clone(),
                                    },
                                    started_at: tool.first_block_at,
                                    metadata: None,
                                },
                            )
                        })
                        .collect(),
                    results: Vec::new(),
                    // Nothing names an interval any more; the check-in is the
                    // notebook's and is not shown here.
                    waiting: None,
                }
            }
            Phase::Idle { .. } => AgentStateKind::Idle,
        };
        AgentStatus {
            kind,
            queued: self.user.len() + self.mail.len(),
        }
    }

    fn publish_sync(&mut self, deadline: Option<UnixMs>) {
        let status = self.status(deadline);
        self.tell(&status.kind);
        *self.status.write().expect("poison") = status;
    }

    /// Say what changed in the tail, if anyone is looking. Sent after the
    /// row this publish follows, from this task, which is the ordering a
    /// client relies on. When nobody is looking the teller forgets, so
    /// the first tell after someone starts is the whole tail.
    fn tell(&mut self, kind: &AgentStateKind) {
        let live = self
            .pool
            .upgrade()
            .is_some_and(|pool| pool.is_live(self.agent_id));
        if !live {
            self.teller.reset();
            return;
        }
        for live in self.teller.tell(kind) {
            crate::mirror::tell_live(&self.db, self.agent_id, live);
        }
    }

    /// Publish, and tell the log when the turn's edge moved: started when
    /// the agent begins working, ended when it hands back.
    async fn publish(&mut self, deadline: Option<UnixMs>) {
        let status = self.status(deadline);
        let working = status.kind.is_working();
        if working != self.working {
            let now = UnixMillis::now();
            let edge = if working {
                TurnEdge::Started
            } else {
                TurnEdge::Ended(match &self.phase {
                    Phase::Idle {
                        standing: Standing::Failed { error, .. },
                        ..
                    } => TurnOutcome::Errored {
                        message: error.to_string(),
                    },
                    Phase::Idle {
                        standing: Standing::Cancelled { .. },
                        ..
                    } => TurnOutcome::Cancelled,
                    _ => TurnOutcome::Completed,
                })
            };
            {
                let mut write = self.db.write().await;
                write.tell_turn(now, self.agent_id, edge);
                write.commit();
            }
            if !working {
                if let Some(pool) = self.pool.upgrade() {
                    pool.settle_turn(self.agent_id).await;
                }
            }
            self.working = working;
        }
        self.tell(&status.kind);
        *self.status.write().expect("poison") = status;
    }
}

fn usage_model(model: InferenceModel) -> AgentUsageModel {
    match model {
        InferenceModel::Gpt6Astra => AgentUsageModel::ASTRA,
        InferenceModel::Gpt56Terra => AgentUsageModel::TERRA,
        InferenceModel::Gpt56Luna => AgentUsageModel::LUNA,
        _ => AgentUsageModel::GPT,
    }
}

// -- the tool surface -------------------------------------------------------

/// Build one session-lifetime notebook. Allowed role switches preserve its
/// capabilities.
#[allow(clippy::too_many_arguments)]
fn surface(
    view: Arc<View>,
    role: AgentRole,
    agent_id: AgentId,
    inference: Option<&Inference>,
    parent: Option<AgentId>,
    pool: &std::sync::Weak<AgentPool>,
) -> anyhow::Result<Surface> {
    let multi_agent = pool
        .upgrade()
        .map(|_| MultiAgentTools::new(pool.clone(), agent_id, parent));
    let (shell, others) = host_tools(&view, role, agent_id, inference, multi_agent.as_ref(), pool);
    let host_specs = others.iter().map(|tool| tool.spec()).collect();
    let notes = inference.map(|_| {
        let view = Arc::clone(&view);
        Lazy::new(move || {
            let view = Arc::clone(&view);
            async move { notes::directory(view.workset()) }
        })
    });
    let notebook = Arc::new(
        rho_agent_tools::PythonNotebook::new(shell, others)
            .map_err(|error| anyhow::anyhow!("the Python notebook failed to start: {error}"))?,
    );
    Ok(Surface {
        notebook,
        prompt: PromptInputs {
            view,
            multi_agent,
            host_specs,
            notes,
        },
    })
}

/// The model-facing surface of a role, for a reader: the prompt and the tool
/// specs a new agent of that role would get, without a pool behind them.
pub fn render_agent_surface(
    view: Arc<View>,
    role: AgentRole,
) -> anyhow::Result<crate::RenderedAgentSurface> {
    let binding = role.session_profile()?;
    if binding.claude_model().is_some() {
        let placeholder = AgentId::from_counter(1, &crate::db::AgentIdDomain(0))
            .expect("counter 1 is within prefix-id capacity");
        let (_, others) = host_tools(
            &view,
            role,
            placeholder,
            None,
            None,
            &std::sync::Weak::new(),
        );
        let specs = others.iter().map(|tool| tool.spec()).collect::<Vec<_>>();
        return Ok(crate::RenderedAgentSurface {
            system_prompt: prompt::claude_prompt(Some(view.as_ref()), None, role, Some(&specs)),
            tools: Arc::from([rho_claude::mcp::exec_spec()]),
        });
    }
    binding
        .deep_config()
        .ok_or_else(|| anyhow::anyhow!("role has no inference profile"))?;
    let placeholder = AgentId::from_counter(1, &crate::db::AgentIdDomain(0))
        .expect("counter 1 is within prefix-id capacity");
    let surface = surface(view, role, placeholder, None, None, &std::sync::Weak::new())?;
    Ok(crate::RenderedAgentSurface {
        system_prompt: prompt::prompt(
            &surface.prompt.view,
            surface.prompt.multi_agent.as_ref(),
            role,
            &surface.prompt.host_specs,
        ),
        tools: Arc::from([rho_inference::exec::spec()]),
    })
}
