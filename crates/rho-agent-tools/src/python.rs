//! Python notebook adapter. Rho owns subprocesses, retained output, and source
//! policy.
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rho_core::{
    ContentPart, ContextBlock, ExecCall, ExecId, InferenceResponseItem, MessageSender, ToolOutput,
    ToolOutputStatus, ToolType, UnixMs,
};
use rho_python::{
    CommandExit, Event, History, HistoryContent, HistoryImage, HistoryItem, HistoryProviderData,
    HostFuture, Input, Sender, Session,
};
use rho_tool_shell::{BoundedOutput, ProcessEvent, ShellTools, decode_output_lossy};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::{JobEnd, SourceWaker, output};

const LOG_LIMIT: usize = 8 * 1024 * 1024;
const JOB_LIMIT: usize = 64;

pub struct PythonNotebook {
    session: Session,
    shared: Arc<Shared>,
    history: Arc<HistoryStore>,
}

#[derive(Default)]
struct HistorySnapshot {
    blocks: Arc<[Arc<ContextBlock>]>,
    locations: Vec<(usize, usize)>,
}

impl HistorySnapshot {
    fn new(blocks: Vec<Arc<ContextBlock>>) -> Self {
        let locations = blocks
            .iter()
            .enumerate()
            .flat_map(|(block, value)| {
                (0..history_block_len(value)).map(move |offset| (block, offset))
            })
            .collect();
        Self {
            blocks: blocks.into(),
            locations,
        }
    }
}

#[derive(Default)]
struct HistoryStore {
    current: Mutex<Arc<HistorySnapshot>>,
    executions: Mutex<HashMap<u64, Arc<HistorySnapshot>>>,
}

impl HistoryStore {
    fn admit(&self, cell: u64) {
        let snapshot = Arc::clone(&self.current.lock().unwrap());
        self.executions.lock().unwrap().insert(cell, snapshot);
    }

    fn forget(&self, cell: u64) {
        self.executions.lock().unwrap().remove(&cell);
    }

    fn snapshot(&self, cell: u64) -> Result<Arc<HistorySnapshot>, String> {
        self.executions
            .lock()
            .unwrap()
            .get(&cell)
            .cloned()
            .ok_or_else(|| "history is unavailable for this execution".into())
    }
}

impl History for HistoryStore {
    fn len(&self, cell: u64) -> Result<usize, String> {
        Ok(self.snapshot(cell)?.locations.len())
    }

    fn get(&self, cell: u64, index: usize) -> Result<HistoryItem, String> {
        let snapshot = self.snapshot(cell)?;
        let &(block, offset) = snapshot
            .locations
            .get(index)
            .ok_or_else(|| "history index out of range".to_owned())?;
        history_item(&snapshot.blocks[block], offset)
    }
}

pub(crate) struct Shared {
    tasks: Mutex<HostTasks>,
    next_cell: AtomicU64,
    /// Host calls and commands, one ID space: they are the cell's sources.
    next_request: AtomicU64,
    shell: ShellTools,
    runtime: tokio::runtime::Handle,
    cells: Mutex<HashMap<u64, Arc<Mutex<ExecState>>>>,
    jobs: Mutex<BTreeMap<u64, Arc<Job>>>,
    /// The newest cell that registered a job: where the foreground begins.
    /// Advanced by registration, never by a cell that only looks or waits.
    foreground_cell: AtomicU64,
}

#[derive(Default)]
struct HostTasks {
    closed: bool,
    running: tokio::task::JoinSet<()>,
    failure: Option<String>,
}

/// The session ID a job is reported under, so the pieces of one background
/// command correlate across replies. A display concern only: the scheduler
/// reads facts, never this, and the model refers to a job by its Python handle.
///
/// Each modular shift is reversible by subtraction. Together they permute all
/// 9,000 slots without a lookup table; labels repeat every 9,000 internal
/// requests. Handles and output ordering always use the original internal ID.
fn session_id(internal_id: u64) -> u32 {
    let x = internal_id % 9_000;
    let (mut left, mut right) = (x / 100, x % 100);
    left = (left + right * right + 17 * right + 43) % 90;
    right = (right + left * left + 29 * left + 71) % 100;
    left = (left + right * right + 53 * right + 19) % 90;
    right = (right + left * left + 11 * left + 37) % 100;
    (1_000 + 100 * left + right) as u32
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PythonStreamProgress {
    pub returned: bool,
    pub ready: Option<usize>,
    pub admitted: usize,
    pub settled: usize,
    pub completed: usize,
    pub stopped: bool,
    pub recovery: bool,
    pub interrupted: bool,
}

struct ExecState {
    cell: u64,
    stream: PythonStreamProgress,
    waker: SourceWaker,
    output: BoundedOutput,
    /// Oldest unsent output.
    since: Option<UnixMs>,
    /// Oldest unsent `notify()`.
    notified: Option<UnixMs>,
    finished: Option<UnixMs>,
    started: bool,
    returned: Option<UnixMs>,
    returned_error: Option<String>,
    operations: Vec<Arc<Mutex<Operation>>>,
    /// The cell raised, or the runtime stopped underneath it.
    failed: bool,
    /// Something went wrong in the cell, its own code or a job it awaited:
    /// the status of its next answer.
    error: bool,
    delivered: bool,
    checkin: Option<crate::PythonCheckin>,
    jobs: Vec<Arc<Job>>,
    pending: usize,
    cancelled: tokio::sync::watch::Sender<bool>,
    images: Vec<rho_core::ImageContent>,
}
impl ExecState {
    /// A line of the cell's own output; `notify` when the model asked for it
    /// to be noticed.
    fn say(&mut self, text: &str, notify: bool) {
        self.write(text, notify);
        self.output.push(b"\n");
    }
    fn write(&mut self, text: &str, notify: bool) {
        self.output.push(text.as_bytes());
        self.since.get_or_insert_with(UnixMs::now);
        if notify {
            self.notified.get_or_insert_with(UnixMs::now);
        }
        self.waker.wake();
    }
    /// An error the cell ends in: output, and a failure the scheduler reads
    /// as such rather than as something the model asked to be told.
    fn fail(&mut self, text: &str) {
        self.failed = true;
        self.error = true;
        self.say(text, false);
    }
    fn closed(&self) -> bool {
        self.finished.is_some()
            && self.pending == 0
            && self
                .jobs
                .iter()
                .all(|j| j.state.lock().unwrap().finished.is_some())
    }

    /// Whether the next `render` would say anything: unsent output, a job or
    /// operation not yet announced as running, or one that ended and has not
    /// been reported. Jobs and operations leave the lists once reported.
    fn has_news(&self) -> bool {
        !self.output.is_empty()
            || self.operations.iter().any(|op| {
                let op = op.lock().unwrap();
                op.finished.is_some() || !op.announced
            })
            || self.jobs.iter().any(|job| {
                let state = job.state.lock().unwrap();
                !state.unsent.is_empty() || state.finished.is_some() || !state.announced
            })
    }
}
struct Operation {
    id: u64,
    name: String,
    output: BoundedOutput,
    registered_at: UnixMs,
    /// Told to the model as running, so its end can name the same ID.
    announced: bool,
    finished: Option<UnixMs>,
    failed: bool,
}
struct Job {
    id: u64,
    name: String,
    state: Mutex<JobState>,
    stdin: tokio::sync::Mutex<Option<tokio::net::unix::pipe::Sender>>,
    cancel: Notify,
    budget: usize,
    ready: tokio::sync::watch::Sender<bool>,
    /// Flipped when the job ends, so a handle recovered from a session ID can
    /// wait for it without holding the request that started it.
    done: tokio::sync::watch::Sender<bool>,
}
struct JobState {
    file: std::fs::File,
    len: usize,
    dropped: usize,
    cursor: usize,
    unsent: BoundedOutput,
    registered_at: UnixMs,
    /// Told to the model as running, so its end can name the same ID.
    announced: bool,
    /// Oldest unsent output.
    since: Option<UnixMs>,
    finished: Option<(UnixMs, Result<CommandExit, String>)>,
    /// A non-zero or missing exit code, a spawn failure, or a cancellation.
    failed: bool,
    delivered: bool,
}
fn history_block_len(block: &ContextBlock) -> usize {
    match block {
        ContextBlock::ToolResults { results } => results.len(),
        ContextBlock::InferenceResponse { items, .. } => items.len(),
        _ => 1,
    }
}

fn history_content(content: &[ContentPart]) -> Vec<HistoryContent> {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => HistoryContent {
                kind: "text",
                text: Some(text.clone()),
                media_type: None,
                data: None,
            },
            ContentPart::Image { media_type, data } => HistoryContent {
                kind: "image",
                text: None,
                media_type: Some(media_type.clone()),
                data: Some(data.clone()),
            },
        })
        .collect()
}

fn history_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect()
}

fn history_images(images: &[rho_core::ImageContent]) -> Vec<HistoryImage> {
    images
        .iter()
        .map(|image| HistoryImage {
            media_type: image.media_type.clone(),
            data: image.data.clone(),
            detail: Some(match image.detail {
                rho_core::ImageDetail::High => "high",
                rho_core::ImageDetail::Original => "original",
            }),
        })
        .collect()
}

fn provider_data(
    provider: &dyn rho_core::ProviderSpecificData,
) -> Result<Option<HistoryProviderData>, String> {
    let encoded =
        senax_encoder::encode(&provider.clone_box()).map_err(|error| error.to_string())?;
    Ok(Some(HistoryProviderData {
        tag: provider.tag().to_owned(),
        data: encoded.to_vec(),
    }))
}

fn tool_type(value: ToolType) -> &'static str {
    match value {
        ToolType::Function => "function",
        ToolType::Custom => "custom",
    }
}

fn output_status(value: ToolOutputStatus) -> &'static str {
    match value {
        ToolOutputStatus::Success => "success",
        ToolOutputStatus::Error => "error",
        ToolOutputStatus::Cancelled => "cancelled",
    }
}

fn history_item(block: &ContextBlock, offset: usize) -> Result<HistoryItem, String> {
    let item = match block {
        ContextBlock::UserMessage { sender, content } => {
            let (role, sender) = match sender {
                MessageSender::User => ("user", None),
                MessageSender::Agent { id } => ("agent", Some(id.encoded().to_owned())),
            };
            HistoryItem {
                kind: "message",
                role: Some(role),
                sender,
                text: Some(history_text(content)),
                content: history_content(content),
                ..HistoryItem::default()
            }
        }
        ContextBlock::DeveloperMessage { text } => HistoryItem {
            kind: "message",
            role: Some("developer"),
            text: Some(text.clone()),
            ..HistoryItem::default()
        },
        ContextBlock::ToolResults { results } => {
            let result = &results[offset];
            HistoryItem {
                kind: "tool_result",
                call_id: Some(result.call_id.as_str().to_owned()),
                tool_type: Some(tool_type(result.tool_type)),
                text: Some(result.body.output.as_str().to_owned()),
                images: history_images(&result.body.images),
                status: Some(output_status(result.body.status)),
                started_at: Some(result.started_at.0 as i64),
                finished_at: Some(result.finished_at.0 as i64),
                metadata: result
                    .metadata
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|error| error.to_string())?,
                ..HistoryItem::default()
            }
        }
        ContextBlock::ToolUpdate(update) => HistoryItem {
            kind: "tool_update",
            call_id: Some(update.call_id.as_str().to_owned()),
            tool_type: Some(tool_type(update.tool_type)),
            text: Some(update.output.as_str().to_owned()),
            images: history_images(&update.images),
            status: update.status.map(output_status),
            at: Some(update.at.0 as i64),
            ..HistoryItem::default()
        },
        ContextBlock::InferenceResponse {
            items,
            provider_response_id,
        } => {
            let response_id = provider_response_id.as_ref().map(|id| id.as_str().to_owned());
            match &items[offset] {
                InferenceResponseItem::AssistantMessage {
                    provider_specific,
                    content,
                    phase,
                } => HistoryItem {
                    kind: "message",
                    role: Some("assistant"),
                    text: Some(history_text(content)),
                    content: history_content(content),
                    phase: phase.map(|phase| match phase {
                        rho_core::MessagePhase::Commentary => "commentary",
                        rho_core::MessagePhase::FinalAnswer => "final_answer",
                    }),
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::ToolCall {
                    provider_specific,
                    id,
                    name,
                    tool_type: kind,
                    arguments,
                } => HistoryItem {
                    kind: "tool_call",
                    name: Some(name.as_str().to_owned()),
                    text: Some(arguments.clone()),
                    call_id: Some(id.as_str().to_owned()),
                    tool_type: Some(tool_type(*kind)),
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::EncryptedReasoning {
                    provider_specific,
                    summary,
                } => HistoryItem {
                    kind: "encrypted_reasoning",
                    summary: summary.clone(),
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::RawReasoning {
                    provider_specific,
                    content,
                    summary,
                } => HistoryItem {
                    kind: "reasoning",
                    text: Some(content.clone()),
                    summary: summary.clone(),
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::Compaction { provider_specific } => HistoryItem {
                    kind: "compaction",
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::Unknown { provider_specific } => HistoryItem {
                    kind: "unknown",
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
            }
        }
        ContextBlock::CompactionTrigger => HistoryItem {
            kind: "compaction_trigger",
            ..HistoryItem::default()
        },
        ContextBlock::ContextRotation { retain_from } => HistoryItem {
            kind: "context_rotation",
            retain_from: Some(*retain_from),
            ..HistoryItem::default()
        },
        ContextBlock::ToolHistoryEvicted { call_ids } => HistoryItem {
            kind: "tool_history_evicted",
            call_ids: call_ids.iter().map(|id| id.as_str().to_owned()).collect(),
            ..HistoryItem::default()
        },
    };
    Ok(item)
}

impl PythonNotebook {
    pub fn new(shell: ShellTools, functions: Vec<HostFunction>) -> Result<Self, String> {
        let runtime = tokio::runtime::Handle::current();
        let shared = Arc::new(Shared {
            tasks: Mutex::new(HostTasks::default()),
            next_cell: AtomicU64::new(1),
            next_request: AtomicU64::new(0),
            shell,
            runtime: runtime.clone(),
            cells: Mutex::new(HashMap::new()),
            jobs: Mutex::new(BTreeMap::new()),
            foreground_cell: AtomicU64::new(0),
        });
        let history = Arc::new(HistoryStore::default());
        let shell = shared.shell.clone();
        let setup_runtime = runtime.clone();
        let session = Session::new(
            move || {
                unsafe { setup_runtime.block_on(shell.enter_interpreter_thread()) }
                    .map_err(|error| error.to_string())
            },
            rho_python::Host {
                functions: functions
                    .iter()
                    .map(|function| (function.bind)(&shared))
                    .collect(),
                commands: Some(Arc::new(NotebookCommands(Arc::clone(&shared)))),
                history: history.clone(),
            },
            runtime,
        )?;
        Ok(Self {
            session,
            shared,
            history,
        })
    }
}
impl Drop for PythonNotebook {
    fn drop(&mut self) {
        self.stop();
    }
}
impl PythonNotebook {
    /// Replace the transcript used for subsequently admitted executions.
    /// Running executions retain their existing cheap `Arc` snapshot.
    pub fn set_history(&self, history: Vec<Arc<ContextBlock>>) {
        *self.history.current.lock().unwrap() = Arc::new(HistorySnapshot::new(history));
    }

    fn stop(&self) {
        self.shared.tasks.lock().unwrap().closed = true;
        for cell in self.shared.cells.lock().unwrap().values() {
            let mut cell = cell.lock().unwrap();
            self.session.sender().cancel(cell.cell);
            cell.cancelled.send_replace(true);
            cell.fail("Python notebook closed");
        }
        for job in self.shared.jobs.lock().unwrap().values() {
            job.cancel.notify_one();
        }
    }

    /// Stop admission, cancel managed work, and await its child cleanup.
    /// This uses no daemon service or persistence acknowledgement.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.stop();
        let (mut tasks, mut failure) = {
            let mut tasks = self.shared.tasks.lock().unwrap();
            (std::mem::take(&mut tasks.running), tasks.failure.take())
        };
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                failure.get_or_insert_with(|| error.to_string());
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
impl PythonNotebook {
    pub fn exec(&self, call: ExecCall, waker: SourceWaker) -> Box<PythonCell> {
        self.start(call.id, Some(call.source), waker)
    }
    pub fn start_stream(&self, id: ExecId, waker: SourceWaker) -> Box<PythonCell> {
        self.start(id, None, waker)
    }
}
impl PythonNotebook {
    fn start(&self, id: ExecId, source: Option<String>, waker: SourceWaker) -> Box<PythonCell> {
        let tasks = self.shared.tasks.lock().unwrap();
        let cell = self.shared.next_cell.fetch_add(1, Ordering::Relaxed);
        let link = Arc::new(Mutex::new(ExecState {
            cell,
            stream: PythonStreamProgress::default(),
            waker,
            output: BoundedOutput::for_tokens(Some(10000)),
            since: None,
            notified: None,
            finished: None,
            started: false,
            returned: None,
            returned_error: None,
            operations: Vec::new(),
            failed: false,
            error: false,
            delivered: false,
            checkin: None,
            jobs: Vec::new(),
            pending: 0,
            cancelled: tokio::sync::watch::channel(false).0,
            images: Vec::new(),
        }));
        self.shared
            .cells
            .lock()
            .unwrap()
            .insert(cell, Arc::clone(&link));
        let exec = Arc::new(PythonExec {
            id,
            cell,
            link,
            shared: Arc::clone(&self.shared),
            history: Arc::clone(&self.history),
            sender: self.session.sender(),
        });
        let sender = self.session.sender();
        self.history.admit(cell);
        let result = if tasks.closed {
            Err("Python notebook closed".into())
        } else {
            match source {
                Some(source) => sender.execute(cell, source, exec.clone()),
                None => sender.stream(cell, exec.clone()),
            }
        };
        if let Err(error) = result {
            self.history.forget(cell);
            let mut state = exec.link.lock().unwrap();
            state.fail(&error);
            state.returned = Some(UnixMs::now());
            state.finished = state.returned;
        }
        Box::new(PythonCell(exec, None))
    }
}
/// What one host call made from a cell shows the model: its report text and
/// images arrive with the cell's output.
pub struct ToolCx {
    link: Arc<Mutex<ExecState>>,
    operation: Arc<Mutex<Operation>>,
}

impl ToolCx {
    /// Text the model sees in the cell's next report.
    pub fn report(&self, text: &str) {
        if !text.is_empty() {
            self.operation.lock().unwrap().output.push(text.as_bytes());
        }
    }

    /// Show an image with the cell's next report.
    pub fn show_image(&self, image: rho_core::ImageContent) {
        let mut cell = self.link.lock().unwrap();
        if cell.images.len() < 20 {
            cell.images.push(image);
            return;
        }
        drop(cell);
        self.report("[an image was not shown: this cell is at its limit of 20]");
    }
}

type Bind = dyn Fn(&Arc<Shared>) -> rho_python::Function + Send + Sync;

/// A Rust function callable from notebook Python. Arguments arrive typed,
/// deserialized straight from the call; the result goes straight back.
#[derive(Clone)]
pub struct HostFunction {
    path: &'static str,
    bind: Arc<Bind>,
}

impl HostFunction {
    /// `path` places the function in the notebook (`"agents.message"` is
    /// `message` in the `agents` module); `positional` names the leading
    /// parameters that may be passed positionally, the rest are keyword-only.
    /// The call is registered with its cell before Python continues, runs to
    /// completion whether or not Python awaits it, and ends with the cell's
    /// cancellation.
    pub fn new<A, R, F, Fut>(path: &'static str, positional: &'static [&'static str], call: F) -> Self
    where
        A: DeserializeOwned + Send + 'static,
        R: Serialize + Send + 'static,
        F: Fn(ToolCx, A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R, String>> + Send + 'static,
    {
        let call = Arc::new(call);
        Self {
            path,
            bind: Arc::new(move |shared| {
                let shared = Arc::clone(shared);
                let call = Arc::clone(&call);
                rho_python::Function::new(path, positional, move |cell, args: A| {
                    let call = Arc::clone(&call);
                    register(&shared, cell, Some(path), None, move |link, operation| {
                        let cx = ToolCx {
                            link,
                            operation: operation.expect("host calls are operations"),
                        };
                        call(cx, args)
                    })
                })
            }),
        }
    }

    /// Calling it returns `None` rather than an awaitable; its outcome shows
    /// only in the cell's report.
    pub fn detached(self) -> Self {
        let bind = self.bind;
        Self {
            path: self.path,
            bind: Arc::new(move |shared| bind(shared).detached()),
        }
    }

    pub fn path(&self) -> &'static str {
        self.path
    }
}

/// Register work with its cell, then run it on the host runtime. Ownership
/// is recorded before Python continues; awaiting the returned future is
/// optional and never controls the work's lifetime.
fn register<R, Fut>(
    shared: &Arc<Shared>,
    cell: u64,
    operation: Option<&str>,
    job: Option<Arc<Job>>,
    work: impl FnOnce(Arc<Mutex<ExecState>>, Option<Arc<Mutex<Operation>>>) -> Fut,
) -> Result<HostFuture<R>, String>
where
    R: Send + 'static,
    Fut: Future<Output = Result<R, String>> + Send + 'static,
{
    let mut tasks = shared.tasks.lock().unwrap();
    if tasks.closed {
        return Err("Python notebook closed".into());
    }
    while let Some(result) = tasks.running.try_join_next() {
        if let Err(error) = result {
            tasks.failure.get_or_insert_with(|| error.to_string());
        }
    }
    let link = shared
        .cells
        .lock()
        .unwrap()
        .get(&cell)
        .cloned()
        .ok_or("Execution is no longer running")?;
    if *link.lock().unwrap().cancelled.borrow() {
        return Err("Execution cancelled".into());
    }
    let operation = operation.map(|name| {
        Arc::new(Mutex::new(Operation {
            id: shared.next_request.fetch_add(1, Ordering::Relaxed),
            name: name.to_owned(),
            output: BoundedOutput::for_tokens(Some(10000)),
            registered_at: UnixMs::now(),
            announced: false,
            finished: None,
            failed: false,
        }))
    });
    {
        let mut state = link.lock().unwrap();
        // Registering work is what moves the foreground: from here on, older
        // cells' jobs are background to this one's.
        shared.foreground_cell.fetch_max(cell, Ordering::Relaxed);
        if let Some(operation) = &operation {
            state.operations.push(Arc::clone(operation));
        }
        if let Some(job) = &job {
            state.jobs.push(Arc::clone(job));
        }
        state.pending += 1;
        state.waker.wake();
    }
    let work = work(Arc::clone(&link), operation.clone());
    let (done, result) = tokio::sync::oneshot::channel();
    tasks.running.spawn_on(
        async move {
            let result = if job.is_some() {
                // A command stops its process and records how it ended
                // itself; interrupting it here would skip that.
                work.await
            } else {
                let mut cancelled = link.lock().unwrap().cancelled.subscribe();
                tokio::select! {
                    biased;
                    _ = cancelled.wait_for(|cancelled| *cancelled) => {
                        Err("Tool call cancelled".to_owned())
                    }
                    result = work => result,
                }
            };
            if let Some(operation) = &operation {
                let mut operation = operation.lock().unwrap();
                if let Err(error) = &result {
                    operation.output.push(error.as_bytes());
                    operation.failed = true;
                    link.lock().unwrap().error = true;
                }
                operation.finished = Some(UnixMs::now());
            }
            {
                let mut state = link.lock().unwrap();
                state.pending -= 1;
                state.waker.wake();
            }
            let _ = done.send(result);
        },
        &shared.runtime,
    );
    Ok(Box::pin(async move {
        result
            .await
            .map_err(|_| "Python notebook closed".to_owned())?
    }))
}

fn command_name(cmd: &str) -> String {
    let mut name = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.len() > 60 {
        let mut end = 57;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        name.push_str("...");
    }
    name
}

/// The notebook's `command()` and `Command`, over the jobs in [`Shared`].
struct NotebookCommands(Arc<Shared>);

impl NotebookCommands {
    fn job(&self, id: u64) -> Result<Arc<Job>, String> {
        self.0
            .jobs
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| "Command handle expired or unknown".to_owned())
    }

    fn new_job(&self, cmd: &str, budget: usize) -> Result<Arc<Job>, String> {
        let mut jobs = self.0.jobs.lock().unwrap();
        if jobs.len() >= JOB_LIMIT {
            let old = jobs
                .iter()
                .find(|(_, j)| j.state.lock().unwrap().delivered)
                .map(|(id, _)| *id);
            match old {
                Some(id) => jobs.remove(&id),
                None => {
                    return Err("64 command handles are still active or awaiting delivery".into());
                }
            };
        }
        let id = self.0.next_request.fetch_add(1, Ordering::Relaxed);
        let job = Arc::new(Job {
            id,
            name: command_name(cmd),
            state: Mutex::new(JobState {
                file: tempfile::tempfile().map_err(|e| e.to_string())?,
                len: 0,
                dropped: 0,
                cursor: 0,
                unsent: BoundedOutput::for_tokens(Some(budget)),
                registered_at: UnixMs::now(),
                announced: false,
                since: None,
                finished: None,
                failed: false,
                delivered: false,
            }),
            stdin: tokio::sync::Mutex::new(None),
            cancel: Notify::new(),
            budget,
            ready: tokio::sync::watch::channel(false).0,
            done: tokio::sync::watch::channel(false).0,
        });
        jobs.insert(id, Arc::clone(&job));
        Ok(job)
    }
}

impl rho_python::Commands for NotebookCommands {
    fn start(
        &self,
        cell: u64,
        cmd: String,
        workdir: Option<String>,
        max_tokens: usize,
    ) -> Result<(u64, HostFuture<CommandExit>), String> {
        // Published synchronously: write_stdin in the same cell can refer to
        // a command whose process has not started yet.
        let job = self.new_job(&cmd, max_tokens.clamp(1, 10000))?;
        let id = job.id;
        let shell = self.0.shell.clone();
        let registered = Arc::clone(&job);
        let result = register(&self.0, cell, None, Some(Arc::clone(&job)), move |link, _| async move {
            let result = run_command(&shell, &job, &cmd, workdir.as_deref(), &link).await;
            *job.stdin.lock().await = None;
            job.ready.send_replace(true);
            // Failure is a fact of the process, computed here and nowhere
            // else: a non-zero exit, no exit code at all (a signal), a spawn
            // failure, or a cancellation.
            let failed = !matches!(&result, Ok(CommandExit { exit_code: Some(0), .. }));
            let mut state = job.state.lock().unwrap();
            state.finished = Some((UnixMs::now(), result.clone()));
            state.failed = failed;
            drop(state);
            job.done.send_replace(true);
            result
        });
        if result.is_err() {
            self.0.jobs.lock().unwrap().remove(&registered.id);
        }
        Ok((id, result?))
    }

    // A session ID is a label the reports show, not a handle, and labels come
    // round again every 9,000 requests. Only live jobs are searched, and two
    // live jobs wearing one label is an error rather than a guess.
    fn find(&self, label: u64) -> Result<u64, String> {
        let found: Vec<u64> = self
            .0
            .jobs
            .lock()
            .unwrap()
            .keys()
            .copied()
            .filter(|id| u64::from(session_id(*id)) == label)
            .collect();
        match found.as_slice() {
            [id] => Ok(*id),
            [] => Err(format!("No live command has session ID {label}")),
            _ => Err(format!(
                "Session ID {label} names more than one live command; keep the handle command() returned"
            )),
        }
    }

    fn wait(&self, cell: u64, id: u64) -> Result<HostFuture<CommandExit>, String> {
        let job = self.job(id)?;
        register(&self.0, cell, Some("wait_command"), None, move |_, _| async move {
            let mut done = job.done.subscribe();
            loop {
                let finished = job.state.lock().unwrap().finished.clone();
                if let Some((_, result)) = finished {
                    return result;
                }
                done.changed().await.map_err(|e| e.to_string())?;
            }
        })
    }

    // `write_stdin` only writes. Reading output is `more_output`'s job, so
    // that one function owns the cursor and nobody reads by accident.
    fn write_stdin(&self, cell: u64, id: u64, chars: String) -> Result<HostFuture<()>, String> {
        let job = self.job(id)?;
        register(&self.0, cell, Some("write_stdin"), None, move |_, _| async move {
            if chars.is_empty() {
                return Ok(());
            }
            job.ready
                .subscribe()
                .wait_for(|ready| *ready)
                .await
                .map_err(|e| e.to_string())?;
            let mut stdin = job.stdin.lock().await;
            let stdin = stdin.as_mut().ok_or("Command stdin not ready or closed")?;
            stdin
                .write_all(chars.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stdin.flush().await.map_err(|e| e.to_string())
        })
    }

    fn more_output(&self, cell: u64, id: u64, max_tokens: usize) -> Result<HostFuture<()>, String> {
        let job = self.job(id)?;
        register(&self.0, cell, Some("more_output"), None, move |_, operation| async move {
            let page = read_page(&job, max_tokens)?;
            operation
                .expect("more_output is an operation")
                .lock()
                .unwrap()
                .output
                .push(page.as_bytes());
            Ok(())
        })
    }

    fn cancel(&self, cell: u64, id: u64) -> Result<HostFuture<()>, String> {
        let job = self.job(id)?;
        register(&self.0, cell, Some("cancel_command"), None, move |_, _| async move {
            job.cancel.notify_one();
            Ok(())
        })
    }
}

/// The next page of a command's log, in the shape its own output arrives in.
fn read_page(job: &Job, max_tokens: usize) -> Result<String, String> {
    let mut state = job.state.lock().unwrap();
    let start = state.cursor;
    let size = (state.len - start).min(max_tokens.clamp(1, 10000) * 4);
    let mut bytes = vec![0; size];
    state
        .file
        .seek(SeekFrom::Start(start as u64))
        .map_err(|e| e.to_string())?;
    state
        .file
        .read_exact(&mut bytes)
        .map_err(|e| e.to_string())?;
    // Do not split a UTF-8 character merely because a page hit its budget.
    // Non-UTF-8 process output still follows the shell's lossy-text contract.
    if size < state.len - start
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
    state.cursor += bytes.len();
    // Reading by hand takes over from the automatic report: whatever was
    // waiting to be reported is dropped, so the next reply does not say
    // again what this page just showed. The rest is paged the same way.
    state.unsent = BoundedOutput::for_tokens(Some(job.budget));
    state.since = None;
    let page = String::from_utf8_lossy(&bytes).into_owned();
    let remaining = state.len - state.cursor;
    let finished = state.finished.is_some();
    let dropped = state.dropped;
    drop(state);
    let mut parts = vec![format!("Session ID: {}", session_id(job.id))];
    if page.is_empty() {
        parts.push(
            if finished {
                "No more output."
            } else {
                "No more output yet. Output and completion arrive automatically."
            }
            .to_owned(),
        );
    } else {
        parts.push(format!("Output:\n{page}"));
    }
    if remaining > 0 {
        parts.push(format!(
            "[{remaining} more bytes; call more_output() again for the next page]"
        ));
    }
    if dropped > 0 {
        parts.push(format!(
            "[{dropped} bytes never reached the log: the command outran its limit]"
        ));
    }
    Ok(parts.join("\n"))
}

async fn run_command(
    shell: &ShellTools,
    job: &Job,
    cmd: &str,
    workdir: Option<&str>,
    link: &Arc<Mutex<ExecState>>,
) -> Result<CommandExit, String> {
    let mut cancelled = link.lock().unwrap().cancelled.subscribe();
    let mut process = tokio::select! {
        biased;
        _ = cancelled.wait_for(|cancelled| *cancelled) => return Err("Command cancelled".into()),
        _ = job.cancel.notified() => return Err("Command cancelled".into()),
        process = shell.spawn(cmd, workdir) => process.map_err(|e| e.to_string())?,
    };
    let work = async {
        *job.stdin.lock().await = process.take_stdin();
        job.ready.send_replace(true);
        let mut exit_code = None;
        loop {
            let event = process.next().await;
            match event {
                ProcessEvent::Output(chunk) => {
                    let mut state = job.state.lock().unwrap();
                    let keep = chunk.len().min(LOG_LIMIT - state.len);
                    let end = state.len as u64;
                    state
                        .file
                        .seek(SeekFrom::Start(end))
                        .map_err(|e| e.to_string())?;
                    state
                        .file
                        .write_all(&chunk[..keep])
                        .map_err(|e| e.to_string())?;
                    state.len += keep;
                    state.dropped = state.dropped.saturating_add(chunk.len() - keep);
                    state.unsent.push(&chunk);
                    state.since.get_or_insert_with(UnixMs::now);
                }
                ProcessEvent::Exited(status) => exit_code = status.code(),
                ProcessEvent::Failed(error) => return Err(error),
                ProcessEvent::Closed => break,
            }
            link.lock().unwrap().waker.wake();
        }
        Ok(CommandExit {
            id: job.id,
            exit_code,
        })
    };
    let result = tokio::select! {
        biased;
        _ = cancelled.wait_for(|cancelled| *cancelled) => Err("Command cancelled".into()),
        _ = job.cancel.notified() => Err("Command cancelled".into()),
        result = work => result,
    };
    process
        .terminate()
        .await
        .map_err(|error| error.to_string())?;
    result
}

pub struct PythonExec {
    id: ExecId,
    cell: u64,
    link: Arc<Mutex<ExecState>>,
    shared: Arc<Shared>,
    history: Arc<HistoryStore>,
    sender: Sender,
}
impl PythonExec {
    pub fn id(&self) -> &ExecId {
        &self.id
    }

    pub fn stream_progress(&self) -> PythonStreamProgress {
        let state = self.link.lock().unwrap();
        PythonStreamProgress {
            returned: state.returned.is_some(),
            ..state.stream
        }
    }

    /// Snapshot recovery facts and acknowledge their notification together,
    /// without acknowledging output or changing execution admission.
    pub fn take_stream_report(&self) -> PythonStreamProgress {
        let mut state = self.link.lock().unwrap();
        let progress = PythonStreamProgress {
            returned: state.returned.is_some(),
            ..state.stream
        };
        state.stream.recovery = false;
        progress
    }

    pub fn feed(&self, source: String, eof: bool) -> Result<(), String> {
        let state = self.link.lock().unwrap();
        if state.stream.stopped || state.returned.is_some() {
            return Ok(());
        }
        self.sender.send(Input::StreamFeed {
            cell: self.cell,
            source,
            eof,
        })
    }

    /// The caller has chosen to allow execution. Admit at most one ready unit;
    /// this method owns progress bookkeeping, not scheduling policy.
    pub fn admit_stream_unit(&self) -> Result<(), String> {
        let mut state = self.link.lock().unwrap();
        let progress = &state.stream;
        let Some(end) = progress.ready else {
            return Ok(());
        };
        if state.returned.is_some()
            || progress.stopped
            || progress.settled != progress.admitted
            || end <= progress.admitted
        {
            return Ok(());
        }
        state.stream.admitted = end;
        let result = self.sender.send(Input::StreamPermit {
            cell: self.cell,
            end,
        });
        if result.is_err() {
            state.stream.stopped = true;
            state.stream.recovery = true;
        }
        drop(state);
        if result.is_err() {
            self.stop_stream();
        }
        result
    }

    /// Stop source admission, not the active unit or its managed commands.
    pub fn stop_stream(&self) {
        self.link.lock().unwrap().stream.stopped = true;
        let _ = self.sender.send(Input::StreamStop { cell: self.cell });
    }

    /// Provider interruption stops source admission, not execution. The
    /// notebook owns the explanation in its first leased contribution.
    pub fn interrupt_stream(&self) {
        self.link.lock().unwrap().stream.interrupted = true;
        self.stop_stream();
    }

    pub fn sequence(&self) -> u64 {
        self.cell
    }

    /// All Python activity and host operations have stopped; output may still
    /// need draining. Distinct from the submitted code's return.
    pub fn quiescent(&self) -> bool {
        self.link.lock().unwrap().closed()
    }

    pub fn facts(&self) -> crate::CellFacts {
        let state = self.link.lock().unwrap();
        crate::CellFacts {
            cell: self.cell,
            started: state.started,
            returned: state.returned,
            failed: state.failed,
            output_since: state.since,
            notified_at: state.notified,
            checkin: state.checkin,
            foreground_cell: self.shared.foreground_cell.load(Ordering::Relaxed),
        }
    }
}
pub struct PythonCell(Arc<PythonExec>, Option<ToolOutput>);
impl std::ops::Deref for PythonCell {
    type Target = PythonExec;
    fn deref(&self) -> &PythonExec {
        &self.0
    }
}
impl rho_python::Execution for PythonExec {
    fn event(&self, event: Event) {
        match event {
            Event::UnitReady { end, .. } => {
                let mut state = self.link.lock().unwrap();
                state.stream.ready = Some(end);
                state.waker.wake();
            }
            Event::UnitSettled { end, error, .. } => {
                let mut state = self.link.lock().unwrap();
                let progress = &mut state.stream;
                if end > progress.settled {
                    progress.settled = end;
                    progress.recovery |= progress.stopped;
                    if error.is_none() {
                        progress.completed = end;
                    } else {
                        progress.stopped = true;
                    }
                }
                state.waker.wake();
                drop(state);
                if error.is_some() {
                    self.stop_stream();
                }
            }
            Event::Started { .. } => {
                let mut state = self.link.lock().unwrap();
                state.started = true;
                state.waker.wake();
            }
            Event::Returned { error, .. } => {
                let mut state = self.link.lock().unwrap();
                state.returned = Some(UnixMs::now());
                if let Some(error) = &error {
                    state.fail(error);
                }
                state.returned_error = error;
                state.waker.wake();
            }
            Event::Text {
                text, important, ..
            } => self.link.lock().unwrap().write(&text, important),
            Event::MaxWait { seconds, .. } => {
                let mut state = self.link.lock().unwrap();
                state.checkin.get_or_insert_default().after =
                    std::time::Duration::from_secs(seconds);
                state.waker.wake();
            }
            Event::SuppressToolWakeups { .. } => {
                let mut state = self.link.lock().unwrap();
                state.checkin.get_or_insert_default().wake_on_tools = false;
                state.waker.wake();
            }
            Event::Finished { error, .. } => {
                let mut state = self.link.lock().unwrap();
                if let Some(error) = error {
                    let error = state
                        .returned_error
                        .as_ref()
                        .and_then(|root| error.strip_prefix(root))
                        .unwrap_or(&error)
                        .trim_start_matches('\n')
                        .to_owned();
                    if !error.is_empty() {
                        state.fail(&error);
                    }
                }
                state.finished = Some(UnixMs::now());
                if state.returned.is_none() {
                    state.returned = state.finished;
                }
                state.waker.wake();
                drop(state);
                self.history.forget(self.cell);
            }
            Event::Stopped { error } => {
                let mut state = self.link.lock().unwrap();
                state.fail(error.as_deref().unwrap_or("Python runtime stopped"));
                state.finished = Some(UnixMs::now());
                state.returned = state.finished;
                state.cancelled.send_replace(true);
                state.waker.wake();
                drop(state);
                self.history.forget(self.cell);
            }
        }
    }
}
impl PythonCell {
    /// Whether any other live cell has something unsent, which the same
    /// reply will carry after this cell's own answer.
    fn others_have_news(&self) -> bool {
        self.shared
            .cells
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| **id != self.cell)
            .any(|(_, state)| state.lock().unwrap().has_news())
    }

    /// Everything unsent, in one block: the cell's own output first, then
    /// each job's. A job is announced as running the first time a reply goes
    /// out while it is, under a session ID its later pieces name again; a job
    /// that ends before any reply is only ever reported finished. Delivered
    /// once and then forgotten.
    fn render(&mut self, first: bool) -> Option<ToolOutput> {
        let mut cell = self.link.lock().unwrap();
        let mut chunks = Vec::new();
        if !cell.output.is_empty() {
            chunks.push(decode_output_lossy(
                std::mem::replace(&mut cell.output, BoundedOutput::for_tokens(Some(10000)))
                    .into_bytes(),
            ));
        }
        cell.since = None;
        cell.notified = None;
        let mut sources = Vec::new();
        // Output from a cell two or more turns back names its command, so
        // the model can place it without its own turn for context.
        let old = self.shared.next_cell.load(Ordering::Relaxed) > self.cell + 2;
        cell.operations.retain(|op| {
            let mut op = op.lock().unwrap();
            if op.finished.is_some() {
                let output = decode_output_lossy(
                    std::mem::replace(&mut op.output, BoundedOutput::for_tokens(Some(10000)))
                        .into_bytes(),
                );
                let mut parts = Vec::new();
                if op.announced {
                    parts.push(format!("Session ID: {}", session_id(op.id)));
                }
                parts.push(format!("Operation {} completed", op.name));
                if !output.is_empty() {
                    parts.push(format!("Output:\n{output}"));
                }
                sources.push((op.id, parts.join("\n")));
                false
            } else {
                if !op.announced {
                    op.announced = true;
                    sources.push((
                        op.id,
                        format!(
                            "Operation {} running in background with session ID {}",
                            op.name,
                            session_id(op.id)
                        ),
                    ));
                }
                true
            }
        });
        cell.jobs.retain(|job| {
            let mut state = job.state.lock().unwrap();
            let has_output = !state.unsent.is_empty();
            let finished = state.finished.clone();
            let announce = finished.is_none() && !state.announced;
            if !has_output && finished.is_none() && !announce {
                return true;
            }
            let mut parts = Vec::new();
            if let Some((_, result)) = &finished {
                if state.announced {
                    parts.push(format!("Session ID: {}", session_id(job.id)));
                }
                parts.push(match result {
                    Ok(CommandExit {
                        exit_code: Some(exit_code),
                        ..
                    }) => format!("Process exited with code {exit_code}"),
                    Ok(CommandExit { exit_code: None, .. }) => {
                        "Process ended without an exit code".to_owned()
                    }
                    Err(error) => format!("Command failed: {error}"),
                });
            } else {
                state.announced = true;
                parts.push(format!(
                    "Command running in background with session ID {}",
                    session_id(job.id)
                ));
            }
            if old {
                parts.push(format!("Command: {}", job.name));
            }
            if has_output {
                // A reply and an explicit `more_output` share one cursor, so a read
                // after an automatic report carries on from where the report
                // stopped instead of repeating it. A report that dropped its
                // own middle showed only a sample, so it leaves the cursor
                // alone and `display` can still page the whole span.
                let complete = !state.unsent.is_truncated();
                let output = decode_output_lossy(
                    std::mem::replace(
                        &mut state.unsent,
                        BoundedOutput::for_tokens(Some(job.budget)),
                    )
                    .into_bytes(),
                );
                if complete {
                    state.cursor = state.len;
                }
                parts.push(format!("Output:\n{output}"));
            }
            state.since = None;
            sources.push((job.id, parts.join("\n")));
            if finished.is_some() {
                state.delivered = true;
                false
            } else {
                true
            }
        });
        sources.sort_by_key(|(id, _)| *id);
        chunks.extend(sources.into_iter().map(|(_, text)| text));
        let closed = cell.closed();
        if closed {
            cell.delivered = true;
        }
        if chunks.is_empty() {
            if !first {
                return None;
            }
            // The call's one required answer, in the notebook's own words
            // (`DECISION-the-core-never-speaks-for-a-tool`): a silent cell
            // whose work is over, or one whose work is still going. Unless an
            // older cell speaks in the same reply: then the silence is not
            // the news, and this cell adds nothing to it.
            if !self.others_have_news() {
                chunks.push(
                    if closed {
                        "No output."
                    } else {
                        "No output yet. Output and completion arrive automatically."
                    }
                    .into(),
                );
            }
        }
        let mut result = output(
            chunks.join("\n"),
            if *cell.cancelled.borrow() {
                ToolOutputStatus::Cancelled
            } else if cell.error {
                ToolOutputStatus::Error
            } else {
                ToolOutputStatus::Success
            },
        );
        if first && cell.stream.interrupted {
            let execution = if result.status == ToolOutputStatus::Cancelled {
                "Execution was cancelled."
            } else {
                "Execution was not cancelled."
            };
            result.output = Arc::new(format!(
                "Your response was interrupted while generating this tool call. {execution} Continue from the existing state without replaying this call.\n\n{}",
                result.output,
            ));
        }
        result.images = Arc::new(std::mem::take(&mut cell.images));
        Some(result)
    }
}
impl PythonCell {
    pub fn sources(&self) -> Vec<(u64, crate::SourceFacts)> {
        use crate::{JobFacts, SourceFacts};
        // Keep the cell marker distinct from zero-based host request IDs.
        let mut sources = vec![(u64::MAX, SourceFacts::Cell(self.facts()))];
        let cell = self.link.lock().unwrap();
        sources.extend(cell.jobs.iter().map(|job| {
            let state = job.state.lock().unwrap();
            (
                job.id,
                SourceFacts::Job(JobFacts {
                    cell: self.cell,
                    registered_at: state.registered_at,
                    output_since: state.since,
                    finished: state.finished.as_ref().map(|(at, _)| JobEnd {
                        at: *at,
                        failed: state.failed,
                    }),
                }),
            )
        }));
        sources.extend(cell.operations.iter().map(|operation| {
            let operation = operation.lock().unwrap();
            (
                operation.id,
                SourceFacts::Job(JobFacts {
                    cell: self.cell,
                    registered_at: operation.registered_at,
                    output_since: None,
                    finished: operation.finished.map(|at| JobEnd {
                        at,
                        failed: operation.failed,
                    }),
                }),
            )
        }));
        sources
    }
    pub fn execution(&self) -> Arc<PythonExec> {
        self.0.clone()
    }
    pub fn done(&self) -> bool {
        self.1.is_none() && self.link.lock().unwrap().delivered
    }
    /// Lease the first contribution. Repeated reads return this same snapshot
    /// until its owner has committed or handed it off and acknowledges it.
    pub fn first_output(&mut self) -> ToolOutput {
        if self.1.is_none() {
            self.1 = self.render(true);
        }
        self.1.clone().expect("a first contribution always exists")
    }
    pub fn more_output(&mut self) -> Option<ToolOutput> {
        if self.1.is_none() {
            self.1 = self.render(false);
        }
        self.1.clone()
    }
    /// Release a leased contribution only after its recipient owns it.
    pub fn acknowledge_output(&mut self) -> bool {
        self.1.take().is_some()
    }
    pub fn cancel(&mut self) {
        self.sender.cancel(self.cell);
        self.link.lock().unwrap().cancelled.send_replace(true);
        for job in &self.link.lock().unwrap().jobs {
            job.cancel.notify_one();
        }
    }
}
impl Drop for PythonCell {
    fn drop(&mut self) {
        if !self.done() {
            self.cancel();
        }
        self.shared.cells.lock().unwrap().remove(&self.cell);
    }
}

#[cfg(test)]
mod session_id_tests {
    use super::session_id;

    #[test]
    fn labels_are_distinct_within_a_cycle_and_in_range() {
        let labels = (0..9_000)
            .map(session_id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(labels.len(), 9_000);
        assert!(labels.iter().all(|label| (1_000..10_000).contains(label)));
        assert_eq!(session_id(9_000), session_id(0));
    }
}
