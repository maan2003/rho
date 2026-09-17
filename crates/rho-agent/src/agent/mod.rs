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
mod persistence;
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
    ToolCallId, ToolName, ToolOutput, ToolOutputStatus, UnixMs,
};
#[cfg(test)]
use rho_db::RhoDb;
use rho_inference::config::{InferenceModel, InferenceProfile};
use rho_inference::{Inference, InferenceSession, PromptCacheKey};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::boundary::{
    Boundary, ModelAsked, ModelTurn, Observations, SourceKind, Standing, boundary,
};
use crate::db::{
    AgentHead, AgentRole, AgentRoleSessionProfile as _, AgentRuntime, AgentUsageBucket,
    AgentUsageModel, EngineerIntelligence, TurnEdge, TurnOutcome, UnixMillis,
};
#[cfg(test)]
use crate::db::{AgentProfileWriteTxnExt as _, AgentReadTxnExt as _, AgentWriteTxnExt as _};
use crate::lazy::Lazy;
use crate::multi_agent_tools::Team;
use crate::native::NativeEvent;
use crate::notebook::host_tools;
use crate::{
    AgentEvent, AgentStateKind, AgentStatus, FailedInferenceResponse, InputKind, QueuedInput,
    ToolPreview, View, final_answer_text, prompt,
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
    host: Option<Arc<crate::worker::Host>>,
}

struct Instructions {
    text: Arc<str>,
}

impl PromptInputs {
    async fn render(&self, role: AgentRole) -> anyhow::Result<Instructions> {
        let team = match &self.host {
            Some(host) => host.team().await?,
            None => None,
        };
        let text = prompt::prompt(&self.view, team.as_ref(), role);
        Ok(Instructions { text })
    }
}

/// Inputs consumed by the session's lazy tool initialization.
#[derive(Clone)]
struct SurfaceInputs {
    view: Arc<Lazy<Arc<View>>>,
    agent_id: AgentId,
    inference: Inference,
    host: Arc<crate::worker::Host>,
}

impl SurfaceInputs {
    fn lazy(&self, role: AgentRole) -> Arc<Lazy<Surface>> {
        let inputs = self.clone();
        Arc::new(Lazy::new(move || {
            let inputs = inputs.clone();
            async move {
                let view = Arc::clone(inputs.view.get().await?);
                let team = inputs.host.team().await?;
                surface(
                    view,
                    role,
                    inputs.agent_id,
                    Some(&inputs.inference),
                    team.as_ref(),
                    Some(&inputs.host),
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
    #[allow(clippy::too_many_arguments)]
    fn start(
        host: Arc<crate::worker::Host>,
        inference: Inference,
        profile: InferenceProfile,
        model: InferenceModel,
        role: AgentRole,
        prompt_cache_key: PromptCacheKey,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<View>>>,
        replayed: replay::Replayed,
        head: AgentHead,
        total_usage: AgentUsageBucket,
        admitted: std::collections::HashSet<ToolCallId>,
    ) -> (Self, Agent) {
        let session = inference.deep_session(profile, model, prompt_cache_key);
        let name_updates = host.names();
        let surface_inputs = SurfaceInputs {
            view: Arc::clone(&view),
            agent_id,
            inference,
            host: host.clone(),
        };
        let status = Arc::new(RwLock::new(AgentStatus {
            kind: AgentStateKind::Idle,
            queued: 0,
        }));
        let head = Arc::new(RwLock::new(head));
        let (control, control_rx) = mpsc::unbounded_channel();
        host.observe(&status);
        let mut agent = Agent {
            writer: persistence::Writer::new(host.clone()),
            pending_events: Vec::new(),
            admitted,
            host,
            surface: surface_inputs.lazy(role),
            model,
            context: replayed.context,
            provider_history: Some(replayed.history),
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
            context_used: replayed.context_used,
            turn: None,
            latest_python_exec: None,
            total_usage,
            name_updates,
            working: false,
            wake: Arc::new(Notify::new()),
            status: Arc::clone(&status),
            head: Arc::clone(&head),
            control_rx,
        };
        // Published before the loop's first decision, so a subscriber that
        // arrives immediately sees the replayed transcript.
        agent.publish_sync(None);
        (
            Self {
                control,
                status,
                head,
                view,
            },
            agent,
        )
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
    Retire(oneshot::Sender<anyhow::Result<()>>),
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
    /// Tell the live tail whole, for a client that just started looking.
    TellTail,
}

/// Everything that can move the agent. The `select!` normalises sources into
/// one of these and does nothing else; all judgment lives in [`Agent::handle`]
/// and [`Agent::boundary`].
enum Event {
    Named(AgentHead),
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
/// live work; abandoning the response preserves
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

pub(crate) struct Agent {
    host: Arc<crate::worker::Host>,
    writer: persistence::Writer,
    pending_events: Vec<AgentEvent<'static>>,
    admitted: std::collections::HashSet<ToolCallId>,
    surface: Arc<Lazy<Surface>>,
    model: InferenceModel,

    /// Live window policy; provider context itself is derived from the event
    /// log.
    context: context::Window,
    // Live projection, including the ordered tail queued for replication.
    provider_history: Option<Vec<Arc<ContextBlock>>>,

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

    context_used: Option<u64>,
    /// What the model's latest turn settled about being looked in on. `None`
    /// until it has spoken once — after a restart included, which is safe
    /// because no tool survives one.
    turn: Option<ModelTurn>,
    latest_python_exec: Option<(ToolCallId, Arc<rho_agent_tools::PythonExec>)>,
    /// Cumulative provider-reported usage across this agent's requests.
    total_usage: AgentUsageBucket,
    name_updates: tokio::sync::watch::Receiver<Option<AgentHead>>,
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
    control_rx: mpsc::UnboundedReceiver<Control>,
}

impl Agent {
    /// Construct a worker-owned runtime from daemon services, without opening
    /// a database or retaining the pool.
    pub(crate) async fn load(
        agent_id: AgentId,
        host: Arc<crate::worker::Host>,
        inference: Inference,
        view: Arc<Lazy<Arc<View>>>,
    ) -> anyhow::Result<(AgentHandle, Self)> {
        let head = host.head().await?;
        anyhow::ensure!(
            !matches!(
                head.config.binding,
                crate::db::SessionBinding::LegacyGemini(_)
            ),
            "Legacy Gemini agents are unsupported; create an agent with a supported role"
        );

        let AgentRuntime::Rho { prompt_cache_key } = head.config.runtime else {
            anyhow::bail!("agent does not use the Rho runtime");
        };
        let profile = head
            .config
            .binding
            .deep_config()
            .ok_or_else(|| anyhow::anyhow!("Rho runtime stored with a Claude mode"))?;
        let model = head
            .config
            .binding
            .deep_model()
            .expect("deep profile has a model");
        host.team().await?;
        let admitted = host.admitted_ids().await?.into_iter().collect();
        let total_usage = host.usage_total().await?;
        let (_, rows) = host.history().await?;
        let replayed = replay::recover(rows.into_iter().map(|(_, event)| event).collect());
        Ok(AgentHandle::start(
            host,
            inference,
            profile,
            model,
            head.config.role,
            prompt_cache_key,
            agent_id,
            view,
            replayed,
            head,
            total_usage,
            admitted,
        ))
    }

    /// Answer the one question after every event, act on the answer, and wait
    /// for the next one, until the last handle is dropped.
    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.session.abort();
        for stream in self.streams.values() {
            stream.exec.stop_stream();
        }
        for exec in self.execs.values_mut() {
            exec.session.cancel();
        }
        if let Some(surface) = self.surface.get_if_ready() {
            surface
                .notebook
                .shutdown()
                .await
                .map_err(anyhow::Error::msg)?;
        }
        self.streams.clear();
        self.execs.clear();
        self.latest_python_exec = None;
        self.flush_events().await?;
        Ok(())
    }

    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            self.writer.check()?;
            // One question per event. Either it says to wait and hands over the
            // timer — so the timer and the rule behind it cannot drift apart —
            // or it says to send, and a request in flight is never waited for.
            let now = UnixMs::now();
            // An idle agent settles its streams first, so the cell facts the
            // decision reads are the ones an admitted statement produced.
            // In flight, admission waits on the decision: a statement is not
            // admitted into a request about to be thrown away.
            if matches!(self.phase, Phase::Idle { .. }) {
                self.advance_streams(true);
            }
            let decision = self.decide(now);
            if matches!(self.phase, Phase::Requesting(_)) {
                self.advance_streams(decision != Boundary::AbortAndResend);
            }
            let deadline = match decision {
                Boundary::No { recheck } => recheck,
                Boundary::AbortAndResend => {
                    // Stop admission and preserve any executed call before
                    // draining fresh input into the replacement request.
                    self.abandon_stream(now).await?;
                    self.session.abort();
                    self.start_request(now, Some(crate::WakeFacts::interrupt()))
                        .await?;
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
                        .await?;
                    None
                }
                Boundary::Now { wake } => {
                    self.start_request(now, Some(wake)).await?;
                    None
                }
            };
            self.publish(deadline).await?;
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
                    name_updates,
                    writer,
                    session,
                    wake,
                    ..
                } = &mut *self;
                // Normalising sources into one Event is all that happens here;
                // no policy, because policy is `boundary` and nowhere else.
                tokio::select! {
                    biased;
                    error = writer.failed() => return Err(error.into()),
                    named = name_updates.changed() => {
                        named.map_err(|_| anyhow::anyhow!("agent services disconnected"))?;
                        Some(Event::Named(name_updates.borrow_and_update().clone().expect("name update")))
                    }
                    control = control_rx.recv() => control.map(Event::Control),
                    event = session.run() => Some(Event::Inference(event)),
                    _ = wake.notified() => Some(Event::SourceChanged),
                    _ = tokio::time::sleep(sleep), if deadline.is_some() => Some(Event::Tick),
                }
            };
            let Some(event) = event else { return Ok(()) };
            self.handle(event).await?;
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
        sources
    }

    /// The single funnel. Every event lands here and does nothing but update
    /// state; what to do about it is asked once, by the caller.
    async fn handle(&mut self, event: Event) -> anyhow::Result<()> {
        let now = UnixMs::now();
        match event {
            Event::Named(stored) => {
                let mut head = self.head.write().expect("poison");
                head.generated_title = stored.generated_title;
                head.title_attempted = stored.title_attempted;
            }
            Event::Control(control) => self.handle_control(control, now).await?,
            // Anything the model says outside a request of ours is somebody
            // else's, or the tail of one already abandoned.
            Event::Inference(event) => {
                let Phase::Requesting(in_flight) = &mut self.phase else {
                    return Ok(());
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
                            .await?;
                        }
                    }
                    InferenceEvent::StreamingStarted => {}
                    InferenceEvent::ExecArgumentsFinished { id } => {
                        self.persist(AgentEvent::ExecObserved {
                            id,
                            milestone: rho_core::ExecMilestone::ArgumentsFinished,
                            at: now,
                        })
                        .await?;
                    }
                    InferenceEvent::ContextItem { index, event } => {
                        in_flight.pending.apply(index, event);
                        if let Err(error) = self.update_stream(now).await {
                            if error.is::<crate::worker::StoreError>() {
                                return Err(error);
                            }
                            let Phase::Requesting(in_flight) = &mut self.phase else {
                                unreachable!()
                            };
                            let partial = std::mem::take(&mut in_flight.pending);
                            self.session.abort();
                            self.fail(now, partial, error.to_string()).await?;
                        }
                    }
                    InferenceEvent::TemporaryFailure { error, .. } => {
                        let (since, attempts) = in_flight.retry.unwrap_or((now, 0));
                        let compaction_owes_reply = in_flight.compaction_owes_reply;
                        let partial = std::mem::take(&mut in_flight.pending);
                        let error = error.to_string();
                        let has_execution = self.abandon_stream(now).await?;
                        self.persist(AgentEvent::Native(NativeEvent::RequestFailed {
                            partial,
                            error: error.clone(),
                            retrying: true,
                            at: now,
                        }))
                        .await?;
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
                        self.fail(now, partial, error.to_string()).await?;
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
                            .await?;
                        }
                        match finished {
                            // Finished streaming, but what arrived does not
                            // assemble into a response.
                            Err(error) => {
                                let Phase::Requesting(in_flight) = &mut self.phase else {
                                    unreachable!()
                                };
                                let partial = std::mem::take(&mut in_flight.pending);
                                self.fail(now, partial, error.to_string()).await?
                            }
                            Ok(items) => {
                                self.finish_request(items, provider_response_id, usage, now)
                                    .await?
                            }
                        }
                    }
                }
            }
            // Both are pure prompts to re-ask the question; what a source
            // reports is read live, so there is nothing to record here.
            Event::SourceChanged | Event::Tick => {}
        }
        Ok(())
    }

    async fn handle_control(&mut self, control: Control, now: UnixMs) -> anyhow::Result<()> {
        match control {
            Control::Retire(reply) => {
                if match self.decide(now) {
                    Boundary::No { recheck } => self.status(recheck).settled(),
                    _ => false,
                } {
                    let _ = reply.send(Ok(()));
                    // Freeze scheduling and admission at this serialized boundary.
                    // The outer driver cancels this future on daemon disconnect.
                    std::future::pending::<()>().await;
                } else {
                    let _ = reply.send(Err(anyhow::anyhow!("agent still has work")));
                }
            }

            Control::TellTail => self.host.tell_tail(),
            Control::User(input, done) => {
                self.persist(AgentEvent::Accepted(input.clone())).await?;
                if let InputKind::Message { content } = &input.kind
                    && !rho_core::text_content(content).trim().is_empty()
                {
                    self.name(&rho_core::text_content(content)).await?;
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
                self.persist(AgentEvent::Accepted(QueuedInput {
                    source: MessageSender::Agent { id: sender },
                    kind: InputKind::Message {
                        content: content.clone(),
                    },
                    delivery: MessageDelivery::NextRequest,
                    at,
                }))
                .await?;
                if !rho_core::text_content(&content).trim().is_empty() {
                    self.name(&rho_core::text_content(&content)).await?;
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
                self.abandon_stream(now).await?;
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
                    self.persist(AgentEvent::Cleared { at: now }).await?;
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
                self.session.set_prompt_cache_key(key);
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

    /// The request is over and the agent stops. What the model had said
    /// goes to the log first, so the reader keeps it and the turn's end
    /// follows its row.
    async fn fail(
        &mut self,
        now: UnixMs,
        partial: PendingInferenceResponse,
        error: String,
    ) -> anyhow::Result<()> {
        self.abandon_stream(now).await?;
        self.persist(AgentEvent::Native(NativeEvent::RequestFailed {
            partial,
            error: error.clone(),
            retrying: false,
            at: now,
        }))
        .await?;
        self.phase = Phase::Idle {
            owed: Vec::new(),
            standing: Standing::Failed {
                at: now,
                error: Arc::from(error.as_str()),
            },
        };
        self.flush_events().await?;
        self.host.failed(error).await?;
        Ok(())
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
        self.flush_events().await?;
        self.host.profile(role, binding).await?;
        self.provider_history = None;
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
        self.flush_events().await?;
        let cursor = {
            let (_, records) = self.host.history().await?;
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
        self.host.rewind(UnixMillis::now(), cursor).await?;
        let (_, records) = self.host.history().await?;
        let replayed = replay::replay(records.into_iter().map(|(_, event)| event).collect());
        self.context = replayed.context;
        self.provider_history = Some(replayed.history);
        self.recovery_notes = replayed.recovery_notes;
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

    async fn name(&mut self, input: &str) -> anyhow::Result<()> {
        self.flush_events().await?;
        let stored = self.host.name(input).await?;
        let mut head = self.head.write().expect("poison");
        head.generated_title = stored.generated_title;
        head.title_attempted = stored.title_attempted;
        Ok(())
    }

    // -- acting on it -------------------------------------------------------

    async fn start_request(
        &mut self,
        now: UnixMs,
        wake: Option<crate::WakeFacts>,
    ) -> anyhow::Result<()> {
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
                .await?;
                return Ok(());
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
                .await?;
                return Ok(());
            }
        };
        let notes_rotation = role.uses_notes_rotation();
        let instructions = Arc::clone(&prompt.text);
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
        if self.provider_history.is_none() {
            self.provider_input().await?;
        }
        let history = self.provider_history.as_ref().unwrap();
        let pending_compaction = history
            .iter()
            .skip(rho_core::context_window_start(history))
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
        let cancel_rotation = self.context.marker.is_some();
        if manual || cancel_rotation {
            self.context.rotated();
        }
        // Explicit compaction always uses the provider, including transport retries.
        let notes_rotation = notes_rotation && !manual;
        // Tool eviction is a first pass; provider compaction remains the fallback.
        self.session.set_context_rotation(notes_rotation);
        self.context.preparation = None;
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
        let limit = self.session.auto_compact_token_limit();
        let owed = match &mut self.phase {
            Phase::Idle { owed, .. } => std::mem::take(owed),
            Phase::Requesting(_) => Vec::new(),
        };
        let delivered = self
            .execs
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut blocks: Vec<ContextBlock> = Vec::new();
        if cancel_rotation {
            blocks.push(ContextBlock::DeveloperMessage {
                text: if manual {
                    context::MANUAL_COMPACTION
                } else {
                    context::POLICY_CHANGED
                }
                .into(),
            });
        }
        self.collect_stream_notes(Some(&delivered));
        let history = self.provider_history.as_ref().unwrap();
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
                           and background alike — and their external side effects may remain. The empty tool results above are placeholders, not output. Recent execution may be absent from this conversation. Do not automatically replay interrupted work; inspect current state before continuing."
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
                    let body = tool.session.first_output();
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
        {
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

            let mut inputs = std::mem::take(&mut self.user);
            inputs.sort_by_key(|input| matches!(input.kind, InputKind::Compaction));
            blocks.extend(inputs.into_iter().map(|input| match input.kind {
                InputKind::Message { content } => ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content,
                },
                InputKind::Compaction => ContextBlock::CompactionTrigger,
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

        let mut evicted = false;
        let mut used = self.context_used;
        if notes_rotation
            && let Some((limit, occupancy)) = limit.zip(used)
            && occupancy >= limit
        {
            let active = self.execs.keys().cloned().collect();
            let eviction = context::evict_tools(history, &active, occupancy, limit);
            if !eviction.call_ids.is_empty() {
                used = Some(
                    occupancy.saturating_sub(eviction.freed_tokens)
                        + context::estimate(&ContextBlock::DeveloperMessage {
                            text: context::EVICTED.into(),
                        }),
                );
                blocks.push(ContextBlock::ToolHistoryEvicted {
                    call_ids: eviction.call_ids,
                });
                blocks.push(ContextBlock::DeveloperMessage {
                    text: context::EVICTED.into(),
                });
                evicted = true;
            }
        }
        let compacting_already =
            pending_compaction || blocks.contains(&ContextBlock::CompactionTrigger);
        let compact =
            !compacting_already && limit.zip(used).is_some_and(|(limit, used)| used >= limit);
        if compact {
            blocks.push(ContextBlock::CompactionTrigger);
        }
        let compaction_owes_reply = retry_owes_reply || compact
                || blocks
                    .iter()
                    .any(|block| !matches!(block, ContextBlock::CompactionTrigger)
                        && !matches!(block, ContextBlock::DeveloperMessage { text } if text == context::MANUAL_COMPACTION));

        let handoff = blocks
            .iter()
            .filter_map(|block| match block {
                ContextBlock::ToolResults { results } => Some(results),
                _ => None,
            })
            .flatten()
            .map(|result| result.call_id.clone())
            .collect::<Vec<_>>();
        for id in &handoff {
            self.persist(AgentEvent::ExecObserved {
                id: id.clone(),
                milestone: rho_core::ExecMilestone::Boundary,
                at: now,
            })
            .await?;
        }

        // The drain, the append and the send are one event because they are one
        // thing: a crash between them would leave a transcript nobody drained
        // into and a queue nobody emptied.
        self.persist(AgentEvent::Native(NativeEvent::RequestStarted {
            input: blocks,
            context: None,
            wake,
            at: now,
        }))
        .await?;
        if evicted {
            self.context.rotated();
            self.context_used = used;
            self.session.abort();
        }
        for (id, exec) in &mut self.execs {
            if delivered.contains(id) && exec.session.acknowledge_output() {
                exec.answer = ReplyState::Sent;
            }
        }
        self.execs
            .retain(|id, exec| !delivered.contains(id) || !exec.session.done());
        self.acknowledge_streams(&delivered);
        let input = self.provider_input().await?;
        self.surface
            .get_if_ready()
            .expect("surface initialized")
            .notebook
            .set_history(input.clone());
        self.session.request(InferenceRequest {
            instructions,
            input,

            agent_id_labels: Default::default(),
        });
        self.phase = Phase::Requesting(InFlight {
            handoff,
            retry,
            previous_failure,
            compaction_owes_reply,
            ..InFlight::default()
        });
        Ok(())
    }

    // -- inference ----------------------------------------------------------

    async fn finish_request(
        &mut self,
        items: Vec<InferenceResponseItem>,
        provider_response_id: Option<ProviderResponseId>,
        usage: Option<rho_core::TokenUsage>,
        now: UnixMs,
    ) -> anyhow::Result<()> {
        if let Err(error) = self.finish_stream(&items) {
            self.fail(now, PendingInferenceResponse::default(), error)
                .await?;
            return Ok(());
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
                    .await?;
                return Ok(());
            }
        };
        let final_text = call.is_none().then(|| final_answer_text(&items));

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
        self.persist(AgentEvent::Native(NativeEvent::ResponseFinished {
            output: vec![ContextBlock::InferenceResponse {
                items,
                provider_response_id,
            }],
            context_used,
            usage: turn_usage.clone(),
            at: now,
        }))
        .await?;

        if let Some(turn_usage) = turn_usage {
            self.total_usage.add(&turn_usage);
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
        {
            self.flush_events().await?;
            self.host.completed(final_text).await?;
        }
        Ok(())
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

    /// Live provider context includes the ordered, not-yet-durable tail.
    async fn provider_input(&mut self) -> anyhow::Result<Vec<Arc<ContextBlock>>> {
        if self.provider_history.is_none() {
            self.flush_events().await?;
            let (_, records) = self.host.history().await?;
            self.provider_history =
                Some(replay::replay(records.into_iter().map(|(_, event)| event).collect()).history);
        }
        Ok(self.provider_history.as_ref().unwrap().clone())
    }

    /// Timing travels with the next conversation boundary. The bounded writer
    /// preserves order without making healthy inference wait for disk.
    async fn persist(&mut self, event: AgentEvent<'static>) -> anyhow::Result<()> {
        if matches!(event, AgentEvent::ExecObserved { .. }) {
            self.pending_events.push(event);
            return Ok(());
        }
        let blocks = event.native_event().map(|event| {
            event
                .blocks()
                .iter()
                .cloned()
                .map(Arc::new)
                .collect::<Vec<_>>()
        });
        if let Some(NativeEvent::ResponseFinished { output, .. }) = event.native_event() {
            for block in output {
                if let ContextBlock::InferenceResponse { items, .. } = block {
                    self.admitted
                        .extend(items.iter().filter_map(|item| match item {
                            InferenceResponseItem::ToolCall { id, .. } => Some(id.clone()),
                            _ => None,
                        }));
                }
            }
        }
        self.pending_events.push(event);
        self.writer
            .append(std::mem::take(&mut self.pending_events))
            .await?;
        if let (Some(history), Some(blocks)) = (&mut self.provider_history, blocks) {
            history.extend(blocks);
            if let Some(surface) = self.surface.get_if_ready() {
                surface.notebook.set_history(history.clone());
            }
        }
        Ok(())
    }

    async fn flush_events(&mut self) -> anyhow::Result<()> {
        if !self.pending_events.is_empty() {
            self.writer
                .append(std::mem::take(&mut self.pending_events))
                .await?;
        }
        Ok(self.writer.flush().await?)
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
        *self.status.write().expect("poison") = status;
        self.host.published();
    }

    /// Publish, and tell the log when the turn's edge moved: started when
    /// the agent begins working, ended when it hands back.
    async fn publish(&mut self, deadline: Option<UnixMs>) -> anyhow::Result<()> {
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
            if !working {
                self.flush_events().await?;
            }
            self.host.turn(now, edge).await?;
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
    team: Option<&Team>,
    host: Option<&Arc<crate::worker::Host>>,
) -> anyhow::Result<Surface> {
    let (shell, others) = host_tools(&view, role, agent_id, inference, team, host);
    let notebook = Arc::new(
        rho_agent_tools::PythonNotebook::new(shell, others)
            .map_err(|error| anyhow::anyhow!("the Python notebook failed to start: {error}"))?,
    );
    Ok(Surface {
        notebook,
        prompt: PromptInputs {
            view,
            host: host.cloned(),
        },
    })
}

/// The model-facing surface of a role, for a reader: the prompt and the tool
/// entry point a new agent of that role would get, without constructing a
/// notebook.
pub fn render_agent_surface(
    view: Arc<View>,
    role: AgentRole,
) -> anyhow::Result<crate::RenderedAgentSurface> {
    let binding = role.session_profile()?;
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
