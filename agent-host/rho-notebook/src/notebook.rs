//! The notebook as its owner sees it, and the boundary host work crosses:
//! everything a cell starts is a source, registered before Python continues,
//! run to completion whether or not Python awaits it, and reported on its
//! own.
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::{IntoPyObjectExt, PyClass, PyClassInitializer};
use rho_agent_types::UnixMs;
use rho_tool_shell::ShellTools;
use senax_encoder::{Decode, Encode};
use tokio::sync::Notify;

use crate::Image;
use crate::runtime::{Build, Inbox, Input, Message};
use crate::source::{
    Header, Kind, SessionId, Source, SourceFacts, SourceId, SourceUpdate, State, StreamProgress,
};

/// How much of one cell's own output a report carries.
const CELL_TOKENS: usize = 10000;
/// Images one source may show.
const IMAGE_LIMIT: usize = 20;

pub struct Notebook {
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
    /// Cells, commands and calls: one ID space, one table.
    pub(crate) next_id: AtomicU64,
    pub(crate) sources: Mutex<BTreeMap<SourceId, Arc<Source>>>,
    pub(crate) retention: Mutex<()>,
    /// Cell ids in the order they started, to tell old sources from new.
    cells: Mutex<Vec<SourceId>>,
    checkin: Mutex<std::time::Duration>,
    pub(crate) shell: ShellTools,
    /// Woken on any change. `notify_one` stores a permit, so a change that
    /// lands while the owner is busy is not lost.
    pub(crate) wake: Arc<Notify>,
}

#[derive(Default)]
struct HostTasks {
    closed: bool,
    running: tokio::task::JoinSet<()>,
    failure: Option<String>,
}

/// Persistable contributions from notebook sources, in first-seen order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Report {
    entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum Entry {
    Source(Vec<SourceUpdate>),
    Text {
        text: String,
        images: Vec<Image>,
    },
    /// Temporary reader for reports written before text contributions were
    /// named.
    #[senax(rename = "Imported")]
    LegacyText {
        text: String,
        images: Vec<Image>,
    },
}

/// The presentation sent to the model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedReport {
    pub text: String,
    pub images: Vec<Image>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add text and images without inferring notebook source state from them.
    pub fn from_text(text: String, images: Vec<Image>) -> Self {
        if text.is_empty() && images.is_empty() {
            return Self::default();
        }
        Self {
            entries: vec![Entry::Text { text, images }],
        }
    }

    /// Temporary database rewrite; remove with the old text decoder.
    pub fn migrate_text(&mut self) {
        for entry in &mut self.entries {
            if let Entry::LegacyText { text, images } = entry {
                *entry = Entry::Text {
                    text: std::mem::take(text),
                    images: std::mem::take(images),
                };
            }
        }
    }

    /// Combine source updates by their notebook-lifetime-unique identities.
    /// Contributions arrive in log order; equal text is still new output.
    pub fn merge(&mut self, newer: Report) {
        for entry in newer.entries {
            match entry {
                Entry::Source(updates) => {
                    let id = updates[0].identity;
                    if let Some(Entry::Source(existing)) = self.entries.iter_mut().find(
                        |entry| matches!(entry, Entry::Source(events) if events[0].identity == id),
                    ) {
                        existing.extend(updates);
                    } else {
                        self.entries.push(Entry::Source(updates));
                    }
                }
                text => self.entries.push(text),
            }
        }
    }

    pub fn render(&self) -> RenderedReport {
        let mut chunks = Vec::new();
        let mut images = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::Text {
                    text,
                    images: entry_images,
                }
                | Entry::LegacyText {
                    text,
                    images: entry_images,
                } => {
                    if !text.is_empty() {
                        chunks.push(text.clone());
                    }
                    images.extend(entry_images.iter().cloned());
                }
                Entry::Source(updates) => {
                    images.extend(
                        updates
                            .iter()
                            .flat_map(|update| update.images.iter().cloned()),
                    );
                    let with_header: Vec<_> = updates
                        .iter()
                        .filter_map(|update| {
                            update
                                .header
                                .as_ref()
                                .map(|header| (update.session, header))
                        })
                        .collect();
                    let header = with_header
                        .iter()
                        .rev()
                        .find(|(_, header)| matches!(header, Header::Finished { .. }))
                        .or_else(|| {
                            with_header
                                .iter()
                                .rev()
                                .find(|(_, header)| !matches!(header, Header::Session))
                        })
                        .or_else(|| with_header.last());
                    let mut parts = Vec::new();
                    if let Some((session, header)) = header {
                        let rendered = header.render(*session);
                        if !rendered.is_empty() {
                            parts.push(rendered);
                        }
                    }
                    parts.extend(
                        updates
                            .iter()
                            .flat_map(|update| update.parts.iter().map(|part| part.render())),
                    );
                    if !parts.is_empty() {
                        chunks.push(parts.join("\n").trim_end().to_owned());
                    }
                }
            }
        }
        RenderedReport {
            text: chunks.join("\n\n"),
            images,
        }
    }
}

impl Notebook {
    /// A notebook whose thread works in `shell`'s view, with `exports` among
    /// its globals. `wake` is notified whenever something changes.
    pub fn new(shell: ShellTools, exports: Vec<Export>, wake: Arc<Notify>) -> Result<Self, String> {
        let runtime = tokio::runtime::Handle::current();
        let shared = Arc::new(Shared {
            inbox: Arc::new(Inbox::new()?),
            stopped: AtomicBool::new(false),
            thread: OnceLock::new(),
            runtime: runtime.clone(),
            tasks: Mutex::new(HostTasks::default()),
            next_id: AtomicU64::new(1),
            sources: Mutex::default(),
            retention: Mutex::default(),
            cells: Mutex::default(),
            checkin: Mutex::new(std::time::Duration::from_secs(120)),
            shell: shell.clone(),
            wake,
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

    /// Run `code` as a new cell.
    pub fn run(&self, code: String) -> CellHandle {
        self.start(Some(code))
    }

    /// A new cell whose code arrives in pieces through [`CellHandle::feed`].
    /// Each whole top-level statement runs as soon as it has arrived.
    pub fn stream(&self) -> CellHandle {
        self.start(None)
    }

    fn start(&self, code: Option<String>) -> CellHandle {
        let shared = &self.shared;
        let id = shared.next_source();
        let cell = Arc::new(Source::new(
            id,
            Kind::Cell,
            "Cell".to_owned(),
            id,
            CELL_TOKENS,
            None,
            None,
        ));
        shared.sources.lock().unwrap().insert(id, Arc::clone(&cell));
        shared.cells.lock().unwrap().push(id);
        let result = if shared.tasks.lock().unwrap().closed {
            Err("Python notebook closed".into())
        } else {
            shared.send(match code {
                Some(code) => Input::Execute {
                    cell: Arc::clone(&cell),
                    code,
                },
                None => Input::Begin {
                    cell: Arc::clone(&cell),
                },
            })
        };
        if let Err(error) = result {
            let mut state = cell.state.lock().unwrap();
            fail(&mut state, &error);
            state.cell.returned = Some(UnixMs::now());
            state.finished = state.cell.returned;
        }
        shared.wake.notify_one();
        CellHandle {
            shared: Arc::clone(shared),
            cell,
        }
    }

    pub fn checkin(&self) -> std::time::Duration {
        *self.shared.checkin.lock().unwrap()
    }

    pub fn reset_checkin(&self) {
        *self.shared.checkin.lock().unwrap() = std::time::Duration::from_secs(120);
    }

    /// Every source that still has something to say, or may yet: what the
    /// owner decides when to report from.
    pub fn facts(&self) -> Vec<SourceFacts> {
        (self.shared.sources.lock().unwrap().values())
            .filter(|source| source.owes_report())
            .map(|source| source.facts())
            .collect()
    }

    /// Everything unsent, in the order the sources started, or `None` when
    /// nothing has anything to say. A source is forgotten once its end is
    /// reported, except a command, which stays to be paged.
    pub fn report(&self) -> Option<Report> {
        let shared = &self.shared;
        let cells = shared.cells.lock().unwrap().clone();
        let sources = shared.sources.lock().unwrap().clone();
        let mut report = Report::default();
        for source in sources
            .values()
            .filter(|s| s.id == cells.last().copied().unwrap_or(SourceId(0)))
            .chain(
                sources
                    .values()
                    .filter(|s| s.id != cells.last().copied().unwrap_or(SourceId(0))),
            )
        {
            let old = cells.iter().filter(|cell| **cell > source.cell).count() >= 2;
            if let Some(update) = source.report(old) {
                report.entries.push(Entry::Source(vec![update]));
            }
        }
        (!report.is_empty()).then_some(report)
    }

    /// Stop every cell and everything they started. Each reports how it
    /// ended.
    pub fn cancel(&self) {
        let sources = self.shared.sources.lock().unwrap().clone();
        for source in sources.values() {
            cancel(&self.shared, source);
        }
    }

    /// Stop admission, cancel managed work, and await its cleanup.
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

    fn stop(&self) {
        self.shared.tasks.lock().unwrap().closed = true;
        self.cancel();
    }
}

impl Drop for Notebook {
    fn drop(&mut self) {
        self.stop();
        // Never join in-process code on a Tokio thread: the loop stops when
        // it next runs, and synchronous Python may still block it.
        self.shared.inbox.post(Message::Input(Input::Shutdown));
    }
}

fn cancel(shared: &Shared, source: &Source) {
    if matches!(source.kind, Kind::Cell | Kind::Task) {
        let mut state = source.state.lock().unwrap();
        if state.finished.is_none() {
            state.cell.cancelled = true;
            state.cell.stream_stopped = true;
            drop(state);
            let _ = shared.send(Input::Cancel { cell: source.id });
        }
    }
    source.cancel.notify_one();
}

/// A cell's own words for an error it ends in.
fn fail(state: &mut State, error: &str) {
    state.failed = true;
    state.error = Some(error.to_owned());
}

/// The owner's hold on one cell.
pub struct CellHandle {
    shared: Arc<Shared>,
    cell: Arc<Source>,
}

impl CellHandle {
    pub fn session_id(&self) -> SessionId {
        self.cell.id.session()
    }

    pub fn facts(&self) -> SourceFacts {
        self.cell.facts()
    }

    pub fn progress(&self) -> StreamProgress {
        self.cell.state.lock().unwrap().cell.stream
    }

    /// More of the cell's code. Once `eof` says it has all arrived, the rest
    /// runs.
    pub fn feed(&self, code: String, eof: bool) -> Result<(), String> {
        let state = self.cell.state.lock().unwrap();
        if state.cell.stream_stopped || state.cell.returned.is_some() {
            return Ok(());
        }
        drop(state);
        self.shared.send(Input::Feed {
            cell: self.cell.id,
            code,
            eof,
        })
    }

    /// Admit no more code. What is running runs on.
    pub fn stop(&self) {
        self.cell.state.lock().unwrap().cell.stream_stopped = true;
        let _ = self.shared.send(Input::Stop { cell: self.cell.id });
    }

    /// The code stopped arriving part-way: let what ran stand as the whole
    /// cell. Returns how many bytes that is, or `None` if nothing ran.
    pub fn interrupt(&self) -> Option<usize> {
        self.stop();
        Some(self.progress().admitted).filter(|admitted| *admitted > 0)
    }

    /// Stop this cell and what it started.
    pub fn cancel(&self) {
        let sources = self.shared.sources.lock().unwrap().clone();
        for source in sources
            .values()
            .filter(|s| s.id == self.cell.id || (s.kind == Kind::Command && s.cell == self.cell.id))
        {
            cancel(&self.shared, source);
        }
    }
}

impl Shared {
    pub(crate) fn next_source(&self) -> SourceId {
        SourceId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Keep at most 50 MB across live retained command logs, dropping oldest
    /// first.
    pub(crate) fn retain_space(&self, current: SourceId, incoming: usize) {
        let _guard = self.retention.lock().unwrap();
        let sources = self.sources.lock().unwrap();
        let mut total: usize = sources
            .values()
            .filter_map(|source| source.state.lock().unwrap().log.as_ref().map(|log| log.len))
            .sum();
        for source in sources.values().filter(|source| source.id != current) {
            if total + incoming <= 50 * 1024 * 1024 {
                break;
            }
            let mut state = source.state.lock().unwrap();
            if let Some(log) = state.log.as_mut()
                && !log.gone
            {
                total -= log.len;
                log.gone = true;
                log.len = 0;
                log.cursor = 0;
                let _ = log.file.set_len(0);
            }
        }
    }

    /// The runtime ended: every cell still running learns why.
    pub(crate) fn stop_runtime(&self, error: Option<String>) {
        self.stopped.store(true, Ordering::Release);
        for source in self.sources.lock().unwrap().values() {
            if !matches!(source.kind, Kind::Cell | Kind::Task) {
                source.cancel.notify_one();
                continue;
            }
            let mut state = source.state.lock().unwrap();
            if state.finished.is_some() {
                continue;
            }
            fail(
                &mut state,
                error.as_deref().unwrap_or("Python runtime stopped"),
            );
            state.finished = Some(UnixMs::now());
            state.cell.returned.get_or_insert(UnixMs::now());
        }
        self.wake.notify_one();
    }
}

/// The running code's cell: every host call names the cell it belongs to.
pub(crate) fn current(py: Python<'_>, purpose: &str) -> PyResult<(Arc<Shared>, Arc<Source>)> {
    let owner = crate::interpreter::kernel(py)?
        .getattr("CELL")?
        .call_method0("get")?;
    if owner.is_none() {
        return Err(PyRuntimeError::new_err(format!(
            "{purpose} only while a cell runs"
        )));
    }
    let cell = owner.getattr("cell")?.cast_into::<Cell>()?;
    let cell = cell.get();
    Ok((Arc::clone(&cell.shared), Arc::clone(&cell.source)))
}

/// A cell as its kernel sees it: where its events go.
#[pyclass(frozen)]
pub(crate) struct Cell {
    shared: Arc<Shared>,
    source: Arc<Source>,
}

impl Cell {
    pub(crate) fn new(shared: Arc<Shared>, source: Arc<Source>) -> Self {
        Self { shared, source }
    }

    /// The cell's state, unless it has already finished: whatever arrives
    /// after that has nowhere to go. Wakes the owner when released.
    fn state(&self) -> Option<Woken<'_>> {
        let state = self.source.state.lock().unwrap();
        state.finished.is_none().then(|| Woken {
            state,
            wake: &self.shared.wake,
        })
    }

    /// Run the next ready unit, if nothing else is running: a streamed cell
    /// runs each whole statement as soon as it arrives.
    fn admit(&self, state: &mut State) {
        let stream = &mut state.cell.stream;
        let Some(end) = stream.ready else { return };
        if state.cell.returned.is_some()
            || state.cell.stream_stopped
            || stream.settled != stream.admitted
            || end <= stream.admitted
        {
            return;
        }
        stream.admitted = end;
        if self
            .shared
            .send(Input::Permit {
                cell: self.source.id,
                end,
            })
            .is_err()
        {
            state.cell.stream_stopped = true;
        }
    }
}

/// A state guard that wakes the owner when dropped.
struct Woken<'a> {
    state: MutexGuard<'a, State>,
    wake: &'a Notify,
}

impl Drop for Woken<'_> {
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}

impl std::ops::Deref for Woken<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.state
    }
}

impl std::ops::DerefMut for Woken<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}

#[pymethods]
impl Cell {
    #[getter]
    fn id(&self) -> u64 {
        self.source.id.0
    }

    fn pending_failure(&self) {
        self.source.state.lock().unwrap().pending_failure = true;
    }

    fn claimed(&self) {
        let mut state = self.source.state.lock().unwrap();
        state.pending_failure = false;
        state.delivered = true;
        state.finished = Some(UnixMs::now());
        self.shared.wake.notify_one();
    }

    fn cancel_commands(&self) {
        for source in self.shared.sources.lock().unwrap().values() {
            if source.cell == self.source.id && source.kind == Kind::Command {
                source.cancel.notify_one();
            }
        }
    }

    fn started(&self) {}

    fn unit_ready(&self, end: usize) {
        if let Some(mut state) = self.state() {
            state.cell.stream.ready = Some(end);
            self.admit(&mut state);
        }
    }

    fn unit_settled(&self, end: usize, error: Option<String>) {
        if let Some(mut state) = self.state()
            && end > state.cell.stream.settled
        {
            state.cell.stream.settled = end;
            if error.is_some() {
                state.cell.stream_stopped = true;
            } else {
                self.admit(&mut state);
            }
        }
    }

    fn returned(&self, error: Option<String>) {
        if let Some(mut state) = self.state() {
            state.cell.returned = Some(UnixMs::now());
            if let Some(error) = &error {
                fail(&mut state, error);
                state.finished = state.cell.returned;
            }
        }
    }

    /// Everything the cell started has ended; `error` is what failed after
    /// its code returned.
    fn finished(&self, error: Option<String>, cancelled: bool) {
        if let Some(mut state) = self.state() {
            state.cell.cancelled = cancelled;
            state.pending_failure = false;
            if let Some(error) = error {
                fail(&mut state, &error);
            }
            state.finished = Some(if state.failed && self.source.kind == Kind::Task {
                UnixMs(UnixMs::now().0.saturating_sub(20_000))
            } else {
                UnixMs::now()
            });
            state.cell.returned.get_or_insert(UnixMs::now());
        }
    }

    fn text(&self, text: &str, important: bool) {
        let mut state = self.source.state.lock().unwrap();
        state.output(text.as_bytes());
        state.since.get_or_insert_with(UnixMs::now);
        if important {
            state.notified.get_or_insert_with(UnixMs::now);
        }
        self.shared.wake.notify_one();
    }

    fn max_wait(&self, seconds: u64) {
        *self.shared.checkin.lock().unwrap() = std::time::Duration::from_secs(seconds);
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

/// Start `work` as a source of the running cell, reported under `name`. It
/// is registered before this returns, runs to completion whether or not
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
    let (shared, cell) = current(py, "Host functions are available")?;
    let future = crate::runtime::future(py, &shared)?;
    let reply = future.clone_ref(py);
    let inbox = Arc::clone(&shared.inbox);
    let id = shared.next_source();
    let source = Arc::new(Source::new(
        id,
        Kind::Call,
        name.to_owned(),
        cell.id,
        CELL_TOKENS,
        None,
        None,
    ));
    register(
        &shared,
        &cell,
        Arc::clone(&source),
        work(ToolCx { source }),
        move |result: Result<R, String>| {
            let result =
                result.map(|value| Box::new(move |py: Python<'_>| value.into_py_any(py)) as Build);
            inbox.post(Message::Done(reply, result));
        },
    )
    .map_err(PyRuntimeError::new_err)?;
    Ok(future)
}

/// What one host call shows the model: its text and images arrive with its
/// report.
pub struct ToolCx {
    source: Arc<Source>,
}

impl ToolCx {
    /// Text the model sees in the call's report.
    pub fn report(&self, text: &str) {
        if !text.is_empty() {
            let mut state = self.source.state.lock().unwrap();
            state.output(text.as_bytes());
            state.since.get_or_insert_with(UnixMs::now);
        }
    }

    /// Show an image with the call's report.
    pub fn show_image(&self, image: Image) {
        let mut state = self.source.state.lock().unwrap();
        if state.images.len() < IMAGE_LIMIT {
            state.images.push(image);
            return;
        }
        drop(state);
        self.report("[an image was not shown: this call is at its limit of 20]");
    }
}

/// Publish a source and run its work on the host runtime. It is in the
/// table before Python continues; `deliver` receives the outcome, and
/// awaiting it from Python is optional and never controls its lifetime. A
/// call is interrupted by cancellation; a command handles its own.
pub(crate) fn register<R, Fut>(
    shared: &Arc<Shared>,
    cell: &Source,
    source: Arc<Source>,
    work: Fut,
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
    if cell.state.lock().unwrap().cell.cancelled {
        return Err("Cell cancelled".into());
    }
    while let Some(result) = tasks.running.try_join_next() {
        if let Err(error) = result {
            tasks.failure.get_or_insert_with(|| error.to_string());
        }
    }
    shared
        .sources
        .lock()
        .unwrap()
        .insert(source.id, Arc::clone(&source));
    shared.wake.notify_one();
    let wake = Arc::clone(&shared.wake);
    tasks.running.spawn_on(
        async move {
            let result = if source.kind == Kind::Command {
                work.await
            } else {
                let result = tokio::select! {
                    biased;
                    () = source.cancel.notified() => Err("cancelled".to_owned()),
                    result = work => result,
                };
                let mut state = source.state.lock().unwrap();
                if let Err(error) = &result {
                    state.error = Some(error.clone());
                    state.failed = true;
                }
                state.finished = Some(UnixMs::now());
                result
            };
            wake.notify_one();
            deliver(result);
        },
        &shared.runtime,
    );
    Ok(())
}

#[cfg(test)]
mod report_tests {
    use uuid::Uuid;

    use super::{Entry, Report};
    use crate::Image;
    use crate::source::{Header, Outcome, Part, SourceUpdate};

    fn image(byte: u8) -> Image {
        Image {
            media_type: "image/png".into(),
            data: vec![byte],
        }
    }

    fn update(id: Uuid, header: Header, parts: Vec<Part>, images: Vec<Image>) -> Report {
        Report {
            entries: vec![Entry::Source(vec![SourceUpdate {
                identity: id,
                session: 1234,
                header: Some(header),
                parts,
                images,
            }])],
        }
    }

    #[test]
    fn merge_replaces_running_status_but_never_deduplicates_equal_output() {
        let id = Uuid::new_v4();
        let mut report = update(
            id,
            Header::Running {
                label: "Command".into(),
                old_name: None,
            },
            vec![Part::Output("same".into())],
            vec![image(1)],
        );
        let end = update(
            id,
            Header::Finished {
                session: true,
                old_name: Some("Command: echo hi".into()),
                end: Outcome::ProcessExited(7),
            },
            vec![
                Part::Output("same".into()),
                Part::Output("different".into()),
            ],
            vec![image(2)],
        );
        report.merge(end); // Equal text in separate drains is separate output.
        let rendered = report.render();
        assert_eq!(
            rendered.text,
            "Session ID: 1234\nCommand: echo hi\nProcess exited with code 7\nOutput:\nsame\nOutput:\nsame\nOutput:\ndifferent"
        );
        assert_eq!(rendered.images, vec![image(1), image(2)]);
        assert_eq!(rendered, report.render());
        let mut bytes = senax_encoder::encode(&report).unwrap();
        let persisted: Report = senax_encoder::decode(&mut bytes).unwrap();
        assert_eq!(persisted, report);
        assert_eq!(persisted.render(), rendered);
    }

    #[test]
    fn same_displayed_session_across_notebook_lifetimes_does_not_merge() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut report = update(
            first,
            Header::Plain,
            vec![Part::Text("first".into())],
            vec![],
        );
        report.merge(update(
            second,
            Header::Plain,
            vec![Part::Text("second".into())],
            vec![],
        ));
        assert_eq!(report.render().text, "first\n\nsecond");
        assert_eq!(report.entries.len(), 2);
    }

    #[test]
    fn page_and_truncation_survive_merging_without_reinterpreting_text() {
        let id = Uuid::new_v4();
        let mut report = update(
            id,
            Header::Running {
                label: "Command".into(),
                old_name: None,
            },
            vec![Part::Output("head\n[output truncated]\ntail".into())],
            vec![],
        );
        report.merge(update(
            id,
            Header::Page {
                old_name: Some("Command: job".into()),
                end: None,
            },
            vec![Part::Page {
                output: "next".into(),
                finished: false,
                remaining: 17,
                dropped: 0,
            }],
            vec![],
        ));
        let text = report.render().text;
        assert!(text.contains("head\n[output truncated]\ntail"));
        assert!(text.contains(
            "Output:\nnext\n[17 more bytes; call more_output() again for the next page]"
        ));
        assert_eq!(report.render().text, text);
    }

    #[test]
    fn migration_rewrites_old_text_tag_without_changing_content() {
        #[derive(senax_encoder::Encode)]
        enum OldEntry {
            Imported { text: String, images: Vec<Image> },
        }
        #[derive(senax_encoder::Encode)]
        struct OldReport {
            entries: Vec<OldEntry>,
        }
        let bytes = senax_encoder::encode(&OldReport {
            entries: vec![OldEntry::Imported {
                text: "distinct old output".into(),
                images: vec![image(7), image(2)],
            }],
        })
        .unwrap();
        let mut report: Report = senax_encoder::decode(&mut bytes.as_ref()).unwrap();
        report.migrate_text();
        let expected = Report::from_text("distinct old output".into(), vec![image(7), image(2)]);
        assert_eq!(report, expected);
        assert_eq!(
            senax_encoder::encode(&report).unwrap(),
            senax_encoder::encode(&expected).unwrap()
        );
    }

    #[test]
    fn text_contributions_are_opaque_and_ordered() {
        let mut report = Report::from_text("Session ID: 1234\nOutput:\nold".into(), vec![image(3)]);
        report.merge(update(
            Uuid::new_v4(),
            Header::Plain,
            vec![Part::Text("new".into())],
            vec![],
        ));
        assert_eq!(
            report.render().text,
            "Session ID: 1234\nOutput:\nold\n\nnew"
        );
        assert_eq!(report.render().images, vec![image(3)]);
        assert!(Report::from_text(String::new(), vec![]).is_empty());
    }
}
