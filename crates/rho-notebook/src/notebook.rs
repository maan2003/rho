//! The notebook as the agent loop sees it, and the boundary host work
//! crosses: everything a cell starts is registered with it before Python
//! continues, runs to completion whether or not Python awaits it, and is
//! reported with the cell's output.
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::{IntoPyObjectExt, PyClass, PyClassInitializer};
use rho_core::{ContextBlock, ExecCall, ExecId, UnixMs};
use rho_tool_shell::{BoundedOutput, ShellTools};

use crate::SourceWaker;
use crate::cell::{PythonCell, PythonExec};
use crate::history::HistorySnapshot;
use crate::runtime::{Build, Inbox, Input, Message};
use crate::source::Source;

pub struct PythonNotebook {
    shared: Arc<Shared>,
}

/// One notebook's state outside Python, shared with its cells, its thread
/// and the host work they start.
pub(crate) struct Shared {
    pub(crate) inbox: Arc<Inbox>,
    /// The notebook thread has ended; nothing will run.
    pub(crate) stopped: AtomicBool,
    pub(crate) thread: OnceLock<std::thread::ThreadId>,
    pub(crate) runtime: tokio::runtime::Handle,
    tasks: Mutex<HostTasks>,
    pub(crate) next_cell: AtomicU64,
    /// Host calls and commands, one ID space: they are the cell's sources.
    pub(crate) next_request: AtomicU64,
    pub(crate) shell: ShellTools,
    pub(crate) cells: Mutex<HashMap<u64, Arc<Mutex<ExecState>>>>,
    pub(crate) jobs: Mutex<BTreeMap<u64, Arc<Source>>>,
    /// The newest cell that registered a job: where the foreground begins.
    /// Advanced by registration, never by a cell that only looks or waits.
    pub(crate) foreground_cell: AtomicU64,
    /// The transcript newly admitted cells see.
    history: Mutex<Arc<HistorySnapshot>>,
}

#[derive(Default)]
struct HostTasks {
    closed: bool,
    running: tokio::task::JoinSet<()>,
    failure: Option<String>,
}

/// How far a streamed cell's source has got, as byte ends of whole top-level
/// statements.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonStreamProgress {
    /// Parsed and waiting for admission.
    pub ready: Option<usize>,
    /// Allowed to run.
    pub admitted: usize,
    /// Run, successfully or not.
    pub settled: usize,
}

pub(crate) struct ExecState {
    pub(crate) stream: PythonStreamProgress,
    /// No more source is admitted.
    pub(crate) stream_stopped: bool,
    /// The response stopped mid-call; its first reply says so.
    pub(crate) interrupted: bool,
    pub(crate) waker: SourceWaker,
    pub(crate) output: BoundedOutput,
    /// Oldest unsent output.
    pub(crate) since: Option<UnixMs>,
    /// Oldest unsent `notify()`.
    pub(crate) notified: Option<UnixMs>,
    pub(crate) finished: Option<UnixMs>,
    pub(crate) started: bool,
    pub(crate) returned: Option<UnixMs>,
    /// The cell raised, or the runtime stopped underneath it.
    pub(crate) failed: bool,
    /// Something went wrong in the cell, its own code or a job it awaited:
    /// the status of its next answer.
    pub(crate) error: bool,
    pub(crate) delivered: bool,
    pub(crate) checkin: Option<crate::PythonCheckin>,
    /// Operations and commands, in the order they started, until each
    /// has reported its end.
    pub(crate) sources: Vec<Arc<Source>>,
    pub(crate) pending: usize,
    pub(crate) cancelled: tokio::sync::watch::Sender<bool>,
    pub(crate) images: Vec<rho_core::ImageContent>,
}

impl ExecState {
    /// A line of the cell's own output; `notify` when the model asked for it
    /// to be noticed.
    pub(crate) fn say(&mut self, text: &str, notify: bool) {
        self.write(text, notify);
        self.output.push(b"\n");
    }
    pub(crate) fn write(&mut self, text: &str, notify: bool) {
        self.output.push(text.as_bytes());
        self.since.get_or_insert_with(UnixMs::now);
        if notify {
            self.notified.get_or_insert_with(UnixMs::now);
        }
        self.waker.wake();
    }
    /// An error the cell ends in: output, and a failure the scheduler reads
    /// as such rather than as something the model asked to be told.
    pub(crate) fn fail(&mut self, text: &str) {
        self.failed = true;
        self.error = true;
        self.say(text, false);
    }
    pub(crate) fn closed(&self) -> bool {
        self.finished.is_some()
            && self.pending == 0
            && self.sources.iter().all(|source| source.finished())
    }

    /// Whether the next `render` would say anything: unsent output, or a
    /// source with something to report. Sources leave the list once their
    /// end is reported.
    pub(crate) fn has_news(&self) -> bool {
        !self.output.is_empty() || self.sources.iter().any(|source| source.has_news())
    }
}

impl PythonNotebook {
    /// A notebook whose thread works in `shell`'s view, with `exports` among
    /// its globals.
    pub fn new(shell: ShellTools, exports: Vec<Export>) -> Result<Self, String> {
        let runtime = tokio::runtime::Handle::current();
        let shared = Arc::new(Shared {
            inbox: Arc::new(Inbox::new()?),
            stopped: AtomicBool::new(false),
            thread: OnceLock::new(),
            runtime: runtime.clone(),
            tasks: Mutex::new(HostTasks::default()),
            next_cell: AtomicU64::new(1),
            next_request: AtomicU64::new(0),
            shell: shell.clone(),
            cells: Mutex::new(HashMap::new()),
            jobs: Mutex::new(BTreeMap::new()),
            foreground_cell: AtomicU64::new(0),
            history: Mutex::default(),
        });
        crate::runtime::spawn(
            Arc::clone(&shared),
            Box::new(move || {
                unsafe { runtime.block_on(shell.enter_interpreter_thread()) }
                    .map_err(|error| error.to_string())
            }),
            exports,
        )?;
        Ok(Self { shared })
    }

    /// Replace the transcript used for subsequently admitted executions.
    /// Running executions retain their existing cheap `Arc` snapshot.
    pub fn set_history(&self, history: Vec<Arc<ContextBlock>>) {
        *self.shared.history.lock().unwrap() = Arc::new(HistorySnapshot::new(history));
    }

    fn stop(&self) {
        self.shared.tasks.lock().unwrap().closed = true;
        for (id, cell) in self.shared.cells.lock().unwrap().iter() {
            let mut cell = cell.lock().unwrap();
            let _ = self.shared.send(Input::Cancel { cell: *id });
            cell.cancelled.send_replace(true);
            cell.fail("Python notebook closed");
        }
        for job in self.shared.jobs.lock().unwrap().values() {
            job.process().cancel.notify_one();
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

    pub fn exec(&self, call: ExecCall, waker: SourceWaker) -> Box<PythonCell> {
        self.start(call.id, Some(call.source), waker)
    }

    pub fn start_stream(&self, id: ExecId, waker: SourceWaker) -> Box<PythonCell> {
        self.start(id, None, waker)
    }

    fn start(&self, id: ExecId, source: Option<String>, waker: SourceWaker) -> Box<PythonCell> {
        let tasks = self.shared.tasks.lock().unwrap();
        let cell = self.shared.next_cell.fetch_add(1, Ordering::Relaxed);
        let link = Arc::new(Mutex::new(ExecState {
            stream: PythonStreamProgress::default(),
            stream_stopped: false,
            interrupted: false,
            waker,
            output: BoundedOutput::for_tokens(Some(10000)),
            since: None,
            notified: None,
            finished: None,
            started: false,
            returned: None,
            failed: false,
            error: false,
            delivered: false,
            checkin: None,
            sources: Vec::new(),
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
            history: Arc::clone(&self.shared.history.lock().unwrap()),
        });
        let result = if tasks.closed {
            Err("Python notebook closed".into())
        } else {
            self.shared.send(match source {
                Some(source) => Input::Execute {
                    exec: Arc::clone(&exec),
                    source,
                },
                None => Input::BeginStream {
                    exec: Arc::clone(&exec),
                },
            })
        };
        if let Err(error) = result {
            let mut state = exec.link.lock().unwrap();
            state.fail(&error);
            state.returned = Some(UnixMs::now());
            state.finished = state.returned;
        }
        Box::new(PythonCell::new(exec))
    }
}

impl Drop for PythonNotebook {
    fn drop(&mut self) {
        self.stop();
        // Never join in-process code on an agent/Tokio thread: the loop stops
        // when it next runs, and synchronous Python may still block it.
        self.shared.inbox.post(Message::Input(Input::Shutdown));
    }
}

impl Shared {
    /// The runtime ended: every cell still running learns why.
    pub(crate) fn stop_runtime(&self, error: Option<String>) {
        self.stopped.store(true, Ordering::Release);
        for link in self.cells.lock().unwrap().values() {
            let mut state = link.lock().unwrap();
            if state.finished.is_some() {
                continue;
            }
            state.fail(error.as_deref().unwrap_or("Python runtime stopped"));
            state.finished = Some(UnixMs::now());
            state.returned = state.finished;
            state.cancelled.send_replace(true);
            state.waker.wake();
        }
    }
}

/// A notebook global provided by the host, typically a `#[pyclass]` whose
/// methods start [`operation`]s. Notebook code can also import it by name.
pub struct Export {
    pub(crate) name: &'static str,
    pub(crate) build: Build,
}

impl Export {
    /// `value` is moved into Python on the notebook's thread.
    pub fn new<T: PyClass + Into<PyClassInitializer<T>> + Send + 'static>(
        name: &'static str,
        value: T,
    ) -> Self {
        Self::build(name, move |py| Ok(Py::new(py, value)?.into_any()))
    }

    /// The object `build` makes on the notebook's thread.
    pub fn build(
        name: &'static str,
        build: impl FnOnce(Python<'_>) -> PyResult<Py<PyAny>> + Send + 'static,
    ) -> Self {
        Self {
            name,
            build: Box::new(build),
        }
    }
}

/// Start `work` as an operation of the running cell, reported under `name`.
/// It is registered before this returns, runs to completion whether or not
/// Python awaits the returned future, and ends with the cell's cancellation.
/// An error raises `RuntimeError` in whoever awaits.
pub fn operation<R, Fut>(
    py: Python<'_>,
    name: &str,
    work: impl FnOnce(ToolCx) -> Fut,
) -> PyResult<Py<PyAny>>
where
    R: for<'py> IntoPyObject<'py> + Send + 'static,
    Fut: Future<Output = Result<R, String>> + Send + 'static,
{
    let exec = crate::cell::current(py, "Host functions are available")?;
    let future = crate::runtime::future(py, &exec.shared)?;
    let reply = future.clone_ref(py);
    let inbox = Arc::clone(&exec.shared.inbox);
    start(&exec, name, work, move |result: Result<R, String>| {
        let result =
            result.map(|value| Box::new(move |py: Python<'_>| value.into_py_any(py)) as Build);
        inbox.post(Message::Done(reply, result));
    })?;
    Ok(future)
}

/// [`operation`] for work nobody awaits: its outcome shows only in the
/// cell's report.
pub fn detached<Fut>(py: Python<'_>, name: &str, work: impl FnOnce(ToolCx) -> Fut) -> PyResult<()>
where
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    let exec = crate::cell::current(py, "Host functions are available")?;
    start(&exec, name, work, drop)
}

fn start<R, Fut>(
    exec: &PythonExec,
    name: &str,
    work: impl FnOnce(ToolCx) -> Fut,
    deliver: impl FnOnce(Result<R, String>) + Send + 'static,
) -> PyResult<()>
where
    R: Send + 'static,
    Fut: Future<Output = Result<R, String>> + Send + 'static,
{
    let id = exec.shared.next_request.fetch_add(1, Ordering::Relaxed);
    let source = Arc::new(Source::new(id, name.to_owned(), 10000, None, None));
    register(
        &exec.shared,
        exec.cell,
        Arc::clone(&source),
        |link| work(ToolCx { link, source }),
        deliver,
    )
    .map_err(PyRuntimeError::new_err)
}

/// What one host call made from a cell shows the model: its report text and
/// images arrive with the cell's output.
pub struct ToolCx {
    link: Arc<Mutex<ExecState>>,
    source: Arc<Source>,
}

impl ToolCx {
    /// Text the model sees in the cell's next report.
    pub fn report(&self, text: &str) {
        if !text.is_empty() {
            self.source
                .state
                .lock()
                .unwrap()
                .unsent
                .push(text.as_bytes());
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

/// Register work with its cell, then run it on the host runtime. Ownership
/// is recorded before Python continues; `deliver` receives the outcome, and
/// awaiting it from Python is optional and never controls the work's
/// lifetime.
pub(crate) fn register<R, Fut>(
    shared: &Arc<Shared>,
    cell: u64,
    source: Arc<Source>,
    work: impl FnOnce(Arc<Mutex<ExecState>>) -> Fut,
    deliver: impl FnOnce(Result<R, String>) + Send + 'static,
) -> Result<(), String>
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
    {
        let mut state = link.lock().unwrap();
        // Registering work is what moves the foreground: from here on, older
        // cells' jobs are background to this one's.
        shared.foreground_cell.fetch_max(cell, Ordering::Relaxed);
        state.sources.push(Arc::clone(&source));
        state.pending += 1;
        state.waker.wake();
    }
    let work = work(Arc::clone(&link));
    tasks.running.spawn_on(
        async move {
            let result = if source.process.is_some() {
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
            if source.process.is_none() {
                let mut state = source.state.lock().unwrap();
                if let Err(error) = &result {
                    state.unsent.push(error.as_bytes());
                    state.failed = true;
                }
                state.finished = Some(UnixMs::now());
            }
            {
                let mut state = link.lock().unwrap();
                if source.process.is_none() && result.is_err() {
                    state.error = true;
                }
                state.pending -= 1;
                state.waker.wake();
            }
            deliver(result);
        },
        &shared.runtime,
    );
    Ok(())
}
