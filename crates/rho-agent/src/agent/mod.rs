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

mod boundary;
pub(crate) mod replay;
#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use rho_agent_tools::{FutureTool, SourceWaker, Tool, ToolHaste, ToolSession};
use rho_core::{
    AgentId, ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    MessageDelivery, MessageSender, PendingInferenceResponse, ProviderResponseId, ToolCall,
    ToolCallId, ToolName, ToolOutput, ToolOutputStatus, ToolResult, ToolSpec, ToolType, ToolUpdate,
    UnixMs,
};
use rho_db::RhoDb;
use rho_inference::config::{InferenceModel, InferenceProfile};
use rho_inference::{Inference, InferenceSession, PromptCacheKey};
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use rho_web_search::WebSearchTools;
use rho_workspaces::View;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Notify, mpsc, oneshot};

use self::boundary::{Boundary, ModelAsked, ModelTurn, SourceKind, boundary};
use crate::db::{
    AgentEventPos, AgentHead, AgentPresentationUpdate, AgentProfileWriteTxnExt as _,
    AgentReadTxnExt as _, AgentRole, AgentRoleSessionProfile as _, AgentRuntime, AgentUsageBucket,
    AgentUsageModel, AgentWriteTxnExt as _, EngineerIntelligence, SessionBinding, TurnEdge,
    TurnOutcome, UnixMillis,
};
use crate::lazy::Lazy;
use crate::multi_agent_tools::{self, MultiAgentTools};
use crate::pool::{AgentPool, AgentTurnCompleted};
use crate::presentation::{self, Sidecar, SidecarMessage};
use crate::{
    AgentEvent, AgentStateKind, AgentStatus, FailedInferenceResponse, InputKind, QueuedInput,
    StartWorkdir, ToolPreview, assistant_text, final_answer_text, materialize_workdirs,
    system_prompt,
};

// -- the one tool the core answers itself -----------------------------------

/// The model's way of naming how long to be left alone. It is the only call
/// whose argument the core reads, because `boundary` is the only thing it is
/// for: `DECISION-model-sets-the-pace`. No session is spawned for it; the
/// answer is written at the next drain, whenever the boundary comes.
pub const WAIT_TOOL_NAME: &str = "wait";
/// Longer than this and the model has stopped pacing and started sleeping.
const MAX_WAIT: Duration = Duration::from_secs(3600);

pub(crate) fn wait_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(WAIT_TOOL_NAME).expect("a valid tool name"),
        tool_type: ToolType::Function,
        description: "Ask to be left alone for a while. Anything a running call ends with, a \
                      user message, or mail wakes you sooner, so a long interval costs nothing \
                      and a short one costs a request: prefer this over polling. Returns with \
                      whatever your other calls have to show by then."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["seconds"],
            "properties": {
                "seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_WAIT.as_secs(),
                    "description": "How long to wait if nothing happens first."
                }
            }
        }),
        format: None,
    }
}

/// What a `wait` call asked for, and what to tell the model in reply. No
/// interval means the call could not be read, and the reply says so.
fn read_wait(arguments: &str) -> (Option<Duration>, ToolOutput) {
    #[derive(Deserialize)]
    struct Args {
        seconds: u64,
    }
    let reply = |status, text: String| ToolOutput {
        images: Arc::new(Vec::new()),
        output: Arc::new(text),
        status,
    };
    match serde_json::from_str::<Args>(arguments) {
        Ok(Args { seconds }) if seconds > 0 => {
            let interval = Duration::from_secs(seconds).min(MAX_WAIT);
            (
                Some(interval),
                reply(
                    ToolOutputStatus::Success,
                    format!("Waited up to {}s.", interval.as_secs()),
                ),
            )
        }
        _ => (
            None,
            reply(
                ToolOutputStatus::Error,
                "wait takes {\"seconds\": N} with N at least 1".to_owned(),
            ),
        ),
    }
}

/// What the model's calls say about being looked in on: the longest interval
/// any `wait` among them named, or only that there were calls.
fn asked_of(calls: &[ToolCall]) -> ModelAsked {
    let longest = calls
        .iter()
        .filter(|call| call.name.as_str() == WAIT_TOOL_NAME)
        .filter_map(|call| read_wait(&call.arguments).0)
        .max();
    match longest {
        Some(interval) => ModelAsked::Wait(interval),
        None if calls.is_empty() => ModelAsked::Nothing,
        None => ModelAsked::Calls,
    }
}

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

/// The tools the model may call and the instructions it gets, built from the
/// agent's workdirs once they are materialized. Lazy because a load must not
/// fail on a workdir that has gone: a reader still gets the transcript, and
/// only a turn needs the tools.
struct Surface {
    view: Arc<View>,
    tools: BTreeMap<ToolName, Arc<dyn Tool>>,
    instructions: Arc<str>,
}

/// What a surface is built from; kept so a role change can build another.
#[derive(Clone)]
struct SurfaceInputs {
    view: Arc<Lazy<Arc<View>>>,
    agent_id: AgentId,
    inference: Inference,
    parent: Option<AgentId>,
    pool: std::sync::Weak<AgentPool>,
}

impl SurfaceInputs {
    fn lazy(&self, role: AgentRole, profile: InferenceProfile) -> Arc<Lazy<Surface>> {
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
                    profile,
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
        start: Vec<StartWorkdir>,
        parent: Option<AgentId>,
        // A dead Weak (e.g. `Weak::default()`) means no pool: the
        // multi-agent tools are not offered.
        pool: std::sync::Weak<AgentPool>,
    ) -> anyhow::Result<(AgentId, Self)> {
        let profile = mode
            .deep_config()
            .ok_or_else(|| anyhow::anyhow!("cannot create a Rho runtime for a Claude mode"))?;
        let model = mode.deep_model().expect("deep config implies a deep model");
        anyhow::ensure!(
            model != InferenceModel::Gemini37FlashLow,
            "the Rho runtime does not support the reduced Antigravity transcript protocol"
        );
        let prompt_cache_key = PromptCacheKey::generate();
        // One transaction spans agent id allocation and the record write.
        // jj owns repository-local managed workspace id allocation; a failed
        // multi-repo creation may leave an unreachable checkout for jj GC.
        let mut write = db.write().await;
        let agent_id = write.alloc_agent_id();
        let materialized = materialize_workdirs(start).await?;
        let entries = materialized.entries.clone();
        let view = match View::new(entries.clone()) {
            Ok(view) => view,
            Err(error) => {
                drop(entries);
                materialized.discard();
                return Err(error);
            }
        };
        write.create_agent(
            UnixMillis::now(),
            agent_id,
            display_name,
            entries
                .iter()
                .map(|workspace| workspace.info().clone())
                .collect(),
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
            Arc::new(Lazy::ready(view)),
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
        anyhow::ensure!(
            model != InferenceModel::Gemini37FlashLow,
            "the Rho runtime does not support the reduced Antigravity transcript protocol"
        );
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
        let last_source = {
            let records = db
                .read()
                .agent_presentation_source_tail(agent_id, crate::PRESENTATION_SOURCE_TAIL_BYTES);
            crate::presentation_sources(agent_id, &records)
                .last()
                .map(|source| source.through)
        };
        let sidecar = Sidecar::new(inference.clone(), last_source);
        let surface_inputs = SurfaceInputs {
            view,
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
            surface: surface_inputs.lazy(role, profile),
            surface_inputs,
            model,
            history: replayed.history,
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
            tools: BTreeMap::new(),
            wait_answers: Vec::new(),
            context_used: replayed.context_used,
            turn: None,
            total_usage,
            sidecar,
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
        }
    }

    pub fn status(&self) -> AgentStatus {
        self.status.read().expect("poison").clone()
    }

    /// The record as of the loop's last change to it.
    pub fn head(&self) -> AgentHead {
        self.head.read().expect("poison").clone()
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
    pub(crate) fn set_watched(&self, watching: bool) {
        let _ = self
            .control
            .send(Control::Presentation(SidecarMessage::Watch { watching }));
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
    Presentation(SidecarMessage),
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
pub(crate) enum Standing {
    /// Nothing either way; the sources decide. The ordinary case, and what a
    /// loaded agent always comes back as.
    Nothing,
    /// Somebody asked for a request the sources would not have made: a retry,
    /// or a compaction that ate the turn the model still owed a reply to.
    Asked,
    /// The user cancelled at `at`, and nothing has been asked since.
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

/// Whether this call's one answer has gone out. The core's own bookkeeping,
/// and the only thing about a call it remembers: what the call is *doing* is
/// the tool's to report, every time it is asked.
///
/// It is also what "the model is waiting on this call" means, there being no
/// other definition of it: the model waits on a call until the call answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolCallAnswer {
    /// Still owed. The next thing this call produces is its one
    /// [`ToolResult`].
    Owed,
    /// Sent, so everything after it arrives as a [`ToolUpdate`].
    Sent,
}

/// The core's bookkeeping for one call: which tool, how much of its story the
/// model has, and since when it has been holding something. The output itself
/// lives in the session, which is asked for it at every boundary.
struct RunningTool {
    call: ToolCall,
    started_at: UnixMs,
    session: Box<dyn ToolSession>,
    answer: ToolCallAnswer,
}

/// The in-flight request. Provisional until it finishes: a failure drops the
/// whole thing without touching history, and everything here goes with it.
#[derive(Clone, Default)]
pub(crate) struct InFlight {
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
    db: RhoDb,
    agent_id: AgentId,
    pool: std::sync::Weak<AgentPool>,
    surface: Arc<Lazy<Surface>>,
    surface_inputs: SurfaceInputs,
    model: InferenceModel,

    /// The transcript. Append-only, and this struct is its sole writer.
    history: Vec<Arc<ContextBlock>>,

    session: InferenceSession,
    phase: Phase,

    /// Typed input, in arrival order: discrete, never merged or summarised,
    /// and always drained in that order.
    user: Vec<QueuedInput>,
    /// Everyone's mail, in arrival order.
    mail: Vec<MailItem>,
    /// One entry per call the model has made and nothing has answered.
    tools: BTreeMap<ToolCallId, RunningTool>,
    /// Replies to `wait` calls, written when the call is made and delivered
    /// with the next drain like any other result.
    wait_answers: Vec<ToolResult>,

    context_used: Option<u64>,
    /// What the model's latest turn settled about being looked in on. `None`
    /// until it has spoken once — after a restart included, which is safe
    /// because no tool survives one.
    turn: Option<ModelTurn>,
    /// Cumulative provider-reported usage across this agent's requests.
    total_usage: AgentUsageBucket,
    sidecar: Sidecar,
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
        self.sidecar.abort();
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
            let deadline = match boundary(&self.sources(), self.turn.as_ref(), &self.phase, now) {
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
        sources.extend(self.tools.values().map(|tool| SourceKind::Tool {
            answer: tool.answer,
            haste: tool.session.haste(),
        }));
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
                    InferenceEvent::RequestSent | InferenceEvent::StreamingStarted => {}
                    InferenceEvent::ContextItem { index, event } => {
                        in_flight.pending.apply(index, event)
                    }
                    InferenceEvent::TemporaryFailure { error, .. } => {
                        let error = error.to_string();
                        in_flight.temporary_failures.push(Arc::from(error.as_str()));
                        // The retry starts a fresh response; the partial one
                        // goes to the log rather than nowhere.
                        let partial = std::mem::take(&mut in_flight.pending);
                        self.persist(AgentEvent::Failed {
                            partial,
                            error: std::borrow::Cow::Borrowed(error.as_str()),
                            retrying: true,
                            at: now,
                        })
                        .await;
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
                        match finished {
                            // Finished streaming, but what arrived does not
                            // assemble into a response.
                            Err(error) => {
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
                let pos = self.persist(AgentEvent::Accepted(input.clone())).await;
                if let InputKind::Message { content } = &input.kind
                    && !rho_core::text_content(content).trim().is_empty()
                {
                    self.source_committed(pos);
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
                let pos = self
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
                    self.source_committed(pos);
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
                // Ask every tool to wind down, then keep reading it: the core
                // does not kill tools, so a tool still chooses its own last
                // words.
                for tool in self.tools.values_mut() {
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
            Control::Presentation(message) => self.handle_presentation(message).await,
        }
    }

    /// The request is over and the agent stops. What the model had said
    /// goes to the log first, so the reader keeps it and the turn's end
    /// follows its row.
    async fn fail(&mut self, now: UnixMs, partial: PendingInferenceResponse, error: String) {
        self.persist(AgentEvent::Failed {
            partial,
            error: std::borrow::Cow::Borrowed(error.as_str()),
            retrying: false,
            at: now,
        })
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
            self.tools.is_empty() && !self.session.has_active_request(),
            "{what} is not available while work is running"
        );
        Ok(())
    }

    async fn change_role(&mut self, requested: AgentRole) -> anyhow::Result<()> {
        self.ensure_settled("a role change")?;
        let requested = match requested {
            AgentRole::Engineer { intelligence }
            | AgentRole::WorkflowEngineer { intelligence, .. } => intelligence,
            _ => anyhow::bail!("role changes currently support only engineer roles"),
        };
        let switchable = |intelligence| {
            matches!(
                intelligence,
                EngineerIntelligence::Low
                    | EngineerIntelligence::Cheap
                    | EngineerIntelligence::Medium
                    | EngineerIntelligence::High
            )
        };
        anyhow::ensure!(
            switchable(requested),
            "this agent can switch only between eng-low, eng-cheap, eng, and eng-high"
        );
        let current = self.head.read().expect("poison").config.role;
        let role = match current {
            AgentRole::Engineer { intelligence } if switchable(intelligence) => {
                AgentRole::Engineer {
                    intelligence: requested,
                }
            }
            AgentRole::WorkflowEngineer {
                intelligence,
                workflow,
            } if switchable(intelligence) => AgentRole::WorkflowEngineer {
                intelligence: requested,
                workflow,
            },
            _ => anyhow::bail!(
                "this agent can switch only between eng-low, eng-cheap, eng, and eng-high"
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
        self.model = model;
        // The prompt and the tool surface follow the role, so the next turn
        // builds them again.
        self.surface = self.surface_inputs.lazy(role, profile);
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
        self.history = replayed.history;
        self.user = replayed.user;
        self.mail = replayed.mail;
        self.context_used = replayed.context_used;
        self.phase = Phase::Idle {
            owed: replayed.owed,
            standing: Standing::Nothing,
        };
        self.turn = None;
        self.wait_answers.clear();
        self.session.abort();
        // A rewind is told, not undone: the last title and activity still
        // stand.
        let last_source = {
            let records = self.db.read().agent_presentation_source_tail(
                self.agent_id,
                crate::PRESENTATION_SOURCE_TAIL_BYTES,
            );
            crate::presentation_sources(self.agent_id, &records)
                .last()
                .map(|source| source.through)
        };
        self.sidecar.reset(last_source);
        self.schedule_presentation();
        if let Some(pool) = self.pool.upgrade() {
            let head = self.db.read().get_agent(self.agent_id);
            pool.publish_presentation_changed(self.agent_id, head.generated_title, head.activity);
        }
        Ok(())
    }

    // -- the presentation sidecar -------------------------------------------

    fn source_committed(&mut self, through: AgentEventPos) {
        self.sidecar.source_committed(through);
        self.schedule_presentation();
    }

    fn schedule_presentation(&mut self) {
        let control = self.control.clone();
        self.sidecar
            .schedule(self.db.clone(), self.agent_id, move |message| {
                control
                    .upgrade()
                    .is_some_and(|control| control.send(Control::Presentation(message)).is_ok())
            });
    }

    async fn handle_presentation(&mut self, message: SidecarMessage) {
        match message {
            SidecarMessage::Watch { watching } => {
                if self.sidecar.watch(watching) {
                    self.schedule_presentation();
                }
            }
            SidecarMessage::Started {
                generation,
                acknowledged,
            } => {
                let _ = acknowledged.send(self.sidecar.started(generation));
            }
            SidecarMessage::Finished { generation, result } => {
                let Some(update) = self.sidecar.finished(generation, result) else {
                    return;
                };
                if let Some(update) = update {
                    self.persist_presentation(update).await;
                }
                self.schedule_presentation();
            }
        }
    }

    async fn persist_presentation(&mut self, update: AgentPresentationUpdate) {
        let cache = {
            let mut write = self.db.write().await;
            let cache = write.apply_agent_presentation(UnixMillis::now(), self.agent_id, &update);
            write.commit();
            cache
        };
        if let (Some(cache), Some(pool)) = (cache, self.pool.upgrade()) {
            pool.publish_presentation_changed(self.agent_id, cache.generated_title, cache.activity);
        }
    }

    // -- acting on it -------------------------------------------------------

    async fn start_request(&mut self, now: UnixMs) {
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
        let instructions = Arc::clone(&surface.instructions);
        let tool_specs = surface
            .tools
            .values()
            .map(|tool| tool.spec())
            .chain(std::iter::once(wait_tool_spec()))
            .collect::<Arc<[ToolSpec]>>();
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
                            images: std::sync::Arc::new(Vec::new()),
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
        // rather than chronological, so tool output leads, and each call's
        // first contribution becomes its `ToolResult` and every later one a
        // `ToolUpdate`, because a provider accepts exactly one result per call
        // id: `REQ-provider-transcript-protocol`.
        let mut results: Vec<ToolResult> = std::mem::take(&mut self.wait_answers);
        let mut updates = Vec::new();
        for tool in self.tools.values_mut() {
            // Whatever the tool is reporting: a request that leaves one call
            // unanswered is rejected whole, so the first drain after a call is
            // made answers it and the tool says what it has, even if that is
            // nothing yet. `ToolHaste` is a hint for `boundary` and is not
            // read here, nor is `done`, which is asked below.
            match tool.answer {
                ToolCallAnswer::Owed => {
                    tool.answer = ToolCallAnswer::Sent;
                    results.push(ToolResult {
                        call_id: tool.call.id.clone(),
                        tool_type: tool.call.tool_type,
                        body: tool.session.first_output(),
                        started_at: tool.started_at,
                        // A result carries `finished_at`, so answering a call
                        // that has already ended says both things at once.
                        finished_at: now,
                        metadata: None,
                    });
                }
                ToolCallAnswer::Sent => {
                    if let Some(output) = tool.session.more_output() {
                        updates.push(ContextBlock::ToolUpdate(ToolUpdate {
                            call_id: tool.call.id.clone(),
                            tool_type: tool.call.tool_type,
                            output: output.output,
                            at: now,
                        }));
                    }
                }
            }
        }
        // Asked after the drain, so whatever a tool said last has been taken:
        // a tool that answers `true` here has had its last chance to speak and
        // is choosing not to want another. Nothing to record — a reaped call is
        // one the transcript has finished talking about.
        self.tools.retain(|_, tool| !tool.session.done());
        if !results.is_empty() {
            blocks.push(ContextBlock::ToolResults { results });
        }
        blocks.extend(updates);
        // One block per sender: several messages from the same peer collapse,
        // so a chatty one costs the model one block rather than five.
        let mut by_sender: BTreeMap<AgentId, Vec<ContentPart>> = BTreeMap::new();
        for item in std::mem::take(&mut self.mail) {
            by_sender
                .entry(item.sender)
                .or_default()
                .extend(item.content);
        }
        blocks.extend(
            by_sender
                .into_iter()
                .map(|(sender, content)| ContextBlock::UserMessage {
                    sender: MessageSender::Agent { id: sender },
                    content,
                }),
        );

        // Every queued item is eligible at every boundary, so the drain is
        // total. Compaction is stable-sorted to the back, because the trigger
        // has to be the final input item and history would otherwise disagree
        // with the request it produced: `REQ-provider-transcript-protocol`.
        let mut inputs = std::mem::take(&mut self.user);
        inputs.sort_by_key(|input| matches!(input.kind, InputKind::Compaction));
        blocks.extend(inputs.into_iter().map(|input| match input.kind {
            InputKind::Message { content } => ContextBlock::UserMessage {
                sender: MessageSender::User,
                content,
            },
            InputKind::Compaction => ContextBlock::CompactionTrigger,
        }));

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

        // The drain, the append and the send are one event because they are one
        // thing: a crash between them would leave a transcript nobody drained
        // into and a queue nobody emptied.
        self.persist(AgentEvent::Sent {
            blocks: Cow::Borrowed(&blocks),
            at: now,
        })
        .await;
        self.history.extend(blocks.into_iter().map(Arc::new));
        self.session.request(InferenceRequest {
            instructions,
            input: self.history.clone(),
            agent_id_labels: Default::default(),
            tools: tool_specs,
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
                .as_ref()
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
        let final_text = calls.is_empty().then(|| final_answer_text(&items));

        let block = ContextBlock::InferenceResponse {
            items,
            provider_response_id,
        };
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
        let pos = self
            .persist(AgentEvent::Replied {
                blocks: Cow::Borrowed(std::slice::from_ref(&block)),
                context_used,
                usage: turn_usage.clone(),
                at: now,
            })
            .await;
        let spoke = match &block {
            ContextBlock::InferenceResponse { items, .. } => {
                !assistant_text(items).trim().is_empty()
            }
            _ => false,
        };
        if spoke {
            self.source_committed(pos);
        }
        self.history.push(Arc::new(block));

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
        // still running speaks for itself. A `wait` among the calls names the
        // interval instead.
        self.turn = Some(ModelTurn {
            spoke_at: now,
            asked: asked_of(&calls),
        });
        for call in calls {
            if call.name.as_str() == WAIT_TOOL_NAME {
                // Answered here, not run: there is nothing to run. The reply
                // reaches the model with the next drain, which is when the
                // wait is over by definition.
                let (_, body) = read_wait(&call.arguments);
                self.wait_answers.push(ToolResult {
                    call_id: call.id,
                    tool_type: call.tool_type,
                    body,
                    started_at: now,
                    finished_at: now,
                    metadata: None,
                });
            } else {
                self.spawn_tool(call, now);
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
        // subscribed to this agent's answers gets it as mail, the sidecar
        // classifies it, and the checkout's state is committed so the user's
        // jj view follows the agent's work.
        if let Some(final_text) = final_text
            && !compacted
        {
            if let Some(pool) = self.pool.upgrade() {
                pool.publish_completed_turn(AgentTurnCompleted {
                    agent_id: self.agent_id,
                    final_answer: final_text.clone(),
                })
                .await;
            }
            presentation::spawn_turn_report(
                self.db.clone(),
                self.pool.clone(),
                self.sidecar.session(),
                self.agent_id,
                &final_text,
            );
            if let Some(surface) = self.surface.get_if_ready() {
                let view = Arc::clone(&surface.view);
                tokio::spawn(async move {
                    if let Err(error) = view.snapshot().await {
                        eprintln!("rho-agent: snapshot failed: {error:#}");
                    }
                });
            }
        }
    }

    // -- tools --------------------------------------------------------------

    fn spawn_tool(&mut self, call: ToolCall, now: UnixMs) {
        /// A session that is already over, so a call that fails before any work
        /// starts reaches the model through exactly the same path as any other
        /// tool output.
        struct BornExited {
            output: ToolOutput,
            at: UnixMs,
        }

        impl ToolSession for BornExited {
            fn haste(&self) -> ToolHaste {
                ToolHaste::Ended { at: self.at }
            }
            fn done(&self) -> bool {
                true
            }

            fn first_output(&mut self) -> ToolOutput {
                self.output.clone()
            }

            fn more_output(&mut self) -> Option<ToolOutput> {
                None
            }

            fn cancel(&mut self) {}
        }

        let tool = self
            .surface
            .get_if_ready()
            .and_then(|surface| surface.tools.get(&call.name));
        let session: Box<dyn ToolSession> = match tool {
            Some(tool) => tool.run(call.clone(), SourceWaker::new(Arc::clone(&self.wake))),
            None => Box::new(BornExited {
                output: ToolOutput {
                    images: std::sync::Arc::new(Vec::new()),
                    output: Arc::new(format!("unknown tool: {}", call.name.as_str())),
                    status: ToolOutputStatus::Error,
                },
                at: now,
            }),
        };
        self.tools.insert(
            call.id.clone(),
            RunningTool {
                started_at: now,
                call,
                session,
                answer: ToolCallAnswer::Owed,
            },
        );
    }

    // -- plumbing -----------------------------------------------------------

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
        let waiting = match self.turn {
            Some(ModelTurn {
                spoke_at,
                asked: ModelAsked::Wait(interval),
            }) => Some(UnixMs(spoke_at.0 + interval.as_millis() as u64)),
            _ => None,
        };
        let kind = match &self.phase {
            Phase::Requesting(in_flight) => AgentStateKind::ApiStreaming {
                pending_response: in_flight.pending.clone(),
                previous_attempt: in_flight.temporary_failures.last().map(|error| {
                    FailedInferenceResponse {
                        partial_response: PendingInferenceResponse::default(),
                        attempt_count: NonZeroU64::new(in_flight.temporary_failures.len() as u64)
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
                    .tools
                    .values()
                    .any(|tool| tool.answer == ToolCallAnswer::Owed)
                    || waiting.is_some_and(|until| deadline.is_some_and(|at| at <= until))
                    || deadline.is_some() =>
            {
                AgentStateKind::ToolCalling {
                    previews: self
                        .tools
                        .iter()
                        .map(|(id, tool)| {
                            (
                                id.clone(),
                                ToolPreview {
                                    call: tool.call.clone(),
                                    started_at: tool.started_at,
                                    metadata: None,
                                },
                            )
                        })
                        .collect(),
                    results: Vec::new(),
                    waiting,
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
                // The activity throttle coalesces within a turn; the next
                // turn's first update should not inherit this one's spacing.
                self.sidecar.turn_settled();
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
        InferenceModel::Gpt56Terra => AgentUsageModel::TERRA,
        InferenceModel::Gpt56Luna => AgentUsageModel::LUNA,
        InferenceModel::Gemini37FlashLow => AgentUsageModel::GEMINI,
        _ => AgentUsageModel::GPT,
    }
}

// -- the tool surface -------------------------------------------------------

/// The tools and instructions of one agent.
#[allow(clippy::too_many_arguments)]
fn surface(
    view: Arc<View>,
    role: AgentRole,
    agent_id: AgentId,
    inference: Option<&Inference>,
    profile: InferenceProfile,
    parent: Option<AgentId>,
    pool: &std::sync::Weak<AgentPool>,
) -> anyhow::Result<Surface> {
    let shell = ShellTools::new(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        Arc::clone(&view),
    )
    .with_env("RHO_AGENT_ID", agent_id.encoded());
    let multi_agent = pool
        .upgrade()
        .map(|_| MultiAgentTools::new(pool.clone(), agent_id, parent));
    let mut others: Vec<Arc<dyn FutureTool>> = vec![Arc::new(ImageTool(
        crate::image_tool::ImageTools::new(Arc::clone(&view)),
    ))];
    if let Some(multi_agent) = &multi_agent {
        others.extend(
            multi_agent_tools::agent_tool_specs(role)
                .into_iter()
                .map(|spec| {
                    Arc::new(AgentTool {
                        tools: multi_agent.clone(),
                        spec,
                    }) as Arc<dyn FutureTool>
                }),
        );
    }
    others.push(match inference {
        Some(inference) => Arc::new(WebSearchTools::new(
            inference.clone(),
            agent_id.encoded().to_owned(),
        )),
        // A rendering has no provider behind it; the spec is what it is for.
        None => Arc::new(SpecOnly(rho_web_search::web_search_spec())),
    });
    let code_mode = cfg!(feature = "code-mode") && profile.code_mode;
    let tools = rho_agent_tools::tools(shell, others, code_mode)
        .map_err(|error| anyhow::anyhow!("code mode failed to start: {error}"))?;
    let instructions = system_prompt::prompt(view.as_ref(), multi_agent.as_ref(), code_mode, role);
    Ok(Surface {
        view,
        tools: tools
            .into_iter()
            .map(|tool| (tool.spec().name, tool))
            .collect(),
        instructions,
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
        return Ok(crate::RenderedAgentSurface {
            system_prompt: system_prompt::claude_prompt(Some(view.as_ref()), None, role),
            tools: Arc::from([]),
        });
    }
    let profile = binding
        .deep_config()
        .ok_or_else(|| anyhow::anyhow!("role has no inference profile"))?;
    let placeholder = AgentId::from_counter(1, &crate::db::AgentIdDomain(0))
        .expect("counter 1 is within prefix-id capacity");
    let surface = surface(
        view,
        role,
        placeholder,
        None,
        profile,
        None,
        &std::sync::Weak::new(),
    )?;
    Ok(crate::RenderedAgentSurface {
        system_prompt: surface.instructions,
        tools: surface
            .tools
            .values()
            .map(|tool| tool.spec())
            .chain(std::iter::once(wait_tool_spec()))
            .collect(),
    })
}

/// A tool that exists only to be listed: calling it is an error.
struct SpecOnly(ToolSpec);

impl FutureTool for SpecOnly {
    fn spec(&self) -> ToolSpec {
        self.0.clone()
    }

    fn call(&self, _call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let name = self.0.name.clone();
        Box::pin(async move {
            ToolOutput {
                images: std::sync::Arc::new(Vec::new()),
                output: Arc::new(format!("{} is not available here", name.as_str())),
                status: ToolOutputStatus::Error,
            }
        })
    }
}

struct ImageTool(crate::image_tool::ImageTools);

impl FutureTool for ImageTool {
    fn spec(&self) -> ToolSpec {
        crate::image_tool::ImageTools::spec()
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let tools = self.0.clone();
        Box::pin(async move { tools.call(call).await })
    }
}

/// One of the collaboration tools, answered by the pool.
struct AgentTool {
    tools: MultiAgentTools,
    spec: ToolSpec,
}

impl FutureTool for AgentTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let tools = self.tools.clone();
        Box::pin(async move { multi_agent_tools::call_agent_tool(tools, call).await })
    }
}
