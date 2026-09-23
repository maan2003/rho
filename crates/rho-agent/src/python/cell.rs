//! One cell: its Rust-owned state, what its kernel reports into it, and
//! how it answers the agent loop.
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use rho_agent_types::{ExecId, ToolOutput, ToolOutputStatus, UnixMs};
use rho_tool_shell::{BoundedOutput, decode_output_lossy};

use crate::python::history::HistorySnapshot;
use crate::python::notebook::{ExecState, PythonStreamProgress, Shared};
use crate::python::output;
use crate::python::runtime::Input;

/// Opens the first reply of a call whose response stopped part-way: the call
/// history keeps is the part that ran.
pub const INTERRUPTED: &str = "Your response was interrupted while writing this call; only the code shown ran. Continue from the existing state without replaying it.";

pub struct PythonExec {
    pub(crate) id: ExecId,
    pub(crate) cell: u64,
    pub(crate) link: Arc<Mutex<ExecState>>,
    pub(crate) shared: Arc<Shared>,
    /// The transcript as it was when the cell was admitted.
    pub(crate) history: Arc<HistorySnapshot>,
}

impl PythonExec {
    pub fn id(&self) -> &ExecId {
        &self.id
    }

    pub fn stream_progress(&self) -> PythonStreamProgress {
        self.link.lock().unwrap().stream
    }

    /// More of the cell's source. Once `eof` says it has all arrived, the
    /// statements not yet admitted run without waiting to be.
    pub fn feed(&self, source: String, eof: bool) -> Result<(), String> {
        let state = self.link.lock().unwrap();
        if state.stream_stopped || state.returned.is_some() {
            return Ok(());
        }
        self.shared.send(Input::StreamFeed {
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
            || state.stream_stopped
            || progress.settled != progress.admitted
            || end <= progress.admitted
        {
            return Ok(());
        }
        state.stream.admitted = end;
        let result = self.shared.send(Input::StreamPermit {
            cell: self.cell,
            end,
        });
        drop(state);
        if result.is_err() {
            self.stop_stream();
        }
        result
    }

    /// Stop source admission, not the active unit or its managed commands.
    pub fn stop_stream(&self) {
        self.link.lock().unwrap().stream_stopped = true;
        let _ = self.shared.send(Input::StreamStop { cell: self.cell });
    }

    /// The response stopped mid-call: admit nothing more, and let what ran
    /// stand as the whole call. Returns how much source that is, or `None`
    /// when nothing ran. Execution is not stopped; the notebook explains the
    /// interruption in the call's first reply.
    pub fn interrupt_stream(&self) -> Option<usize> {
        self.stop_stream();
        let mut state = self.link.lock().unwrap();
        state.interrupted = true;
        Some(state.stream.admitted).filter(|admitted| *admitted > 0)
    }

    pub fn sequence(&self) -> u64 {
        self.cell
    }

    /// All Python activity and host operations have stopped; output may still
    /// need draining. Distinct from the submitted code's return.
    pub fn quiescent(&self) -> bool {
        self.link.lock().unwrap().closed()
    }

    pub fn facts(&self) -> crate::python::CellFacts {
        let state = self.link.lock().unwrap();
        crate::python::CellFacts {
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
/// The running code's cell: every host call names the cell it belongs to.
pub(crate) fn current(py: Python<'_>, purpose: &str) -> PyResult<Arc<PythonExec>> {
    let owner = crate::python::interpreter::kernel(py)?
        .getattr("CELL")?
        .call_method0("get")?;
    if owner.is_none() {
        return Err(PyRuntimeError::new_err(format!(
            "{purpose} only while a cell runs"
        )));
    }
    Ok(Arc::clone(
        &owner.getattr("cell")?.cast_into::<Cell>()?.get().exec,
    ))
}

/// A cell as its kernel sees it: where its events go.
#[pyclass(frozen)]
pub(crate) struct Cell {
    exec: Arc<PythonExec>,
}

impl Cell {
    pub(crate) fn new(exec: Arc<PythonExec>) -> Self {
        Self { exec }
    }

    /// The cell's state, unless it has already finished: whatever arrives
    /// after that has nowhere to go.
    fn state(&self) -> Option<MutexGuard<'_, ExecState>> {
        let state = self.exec.link.lock().unwrap();
        state.finished.is_none().then_some(state)
    }
}

#[pymethods]
impl Cell {
    #[getter]
    fn id(&self) -> u64 {
        self.exec.cell
    }

    fn started(&self) {
        if let Some(mut state) = self.state() {
            state.started = true;
            state.wake.notify_one();
        }
    }

    fn unit_ready(&self, end: usize) {
        if let Some(mut state) = self.state() {
            state.stream.ready = Some(end);
            state.wake.notify_one();
        }
    }

    fn unit_settled(&self, end: usize, error: Option<String>) {
        let Some(mut state) = self.state() else {
            return;
        };
        if end > state.stream.settled {
            state.stream.settled = end;
            if error.is_some() {
                state.stream_stopped = true;
            }
        }
        state.wake.notify_one();
        drop(state);
        if error.is_some() {
            self.exec.stop_stream();
        }
    }

    fn returned(&self, error: Option<String>) {
        let Some(mut state) = self.state() else {
            return;
        };
        state.returned = Some(UnixMs::now());
        if let Some(error) = &error {
            state.fail(error);
        }
        state.wake.notify_one();
    }

    /// Everything the cell started has ended; `error` is what failed after
    /// its code returned.
    fn finished(&self, error: Option<String>) {
        let Some(mut state) = self.state() else {
            return;
        };
        if let Some(error) = error {
            state.fail(&error);
        }
        state.finished = Some(UnixMs::now());
        if state.returned.is_none() {
            state.returned = state.finished;
        }
        state.wake.notify_one();
    }

    fn text(&self, text: &str, important: bool) {
        if let Some(mut state) = self.state() {
            state.write(text, important);
        }
    }

    fn max_wait(&self, seconds: u64) {
        if let Some(mut state) = self.state() {
            state.checkin.get_or_insert_default().after = std::time::Duration::from_secs(seconds);
            state.wake.notify_one();
        }
    }

    fn suppress_tool_wakeups(&self) {
        if let Some(mut state) = self.state() {
            state.checkin.get_or_insert_default().wake_on_tools = false;
            state.wake.notify_one();
        }
    }

    fn history_len(&self) -> usize {
        self.exec.history.len()
    }

    fn history_get<'py>(&self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyAny>> {
        self.exec.history.get(py, index)
    }
}

impl PythonExec {
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
    /// each source's report in the order they started. A source is forgotten
    /// once its end is reported.
    fn render(&self, first: bool) -> Option<ToolOutput> {
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
        cell.sources.retain(|source| {
            if let Some(report) = source.report(old) {
                sources.push((source.id, report));
            }
            !source.state.lock().unwrap().delivered
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
        if first && cell.interrupted {
            result.output = Arc::new(format!("{INTERRUPTED}\n\n{}", result.output));
        }
        result.images = Arc::new(std::mem::take(&mut cell.images));
        Some(result)
    }
}
impl PythonExec {
    /// The facts of each job the cell started and has not finished
    /// reporting, in the order they started.
    pub fn jobs(&self) -> Vec<crate::python::JobFacts> {
        let cell = self.link.lock().unwrap();
        cell.sources
            .iter()
            .map(|source| source.facts(self.cell))
            .collect()
    }
    /// Everything has been said and acknowledged.
    pub fn done(&self) -> bool {
        let state = self.link.lock().unwrap();
        state.lease.is_none() && state.delivered
    }
    /// Lease the first contribution. Repeated reads return this same snapshot
    /// until its owner has committed or handed it off and acknowledges it.
    pub fn first_output(&self) -> ToolOutput {
        self.more_output_or(true)
            .expect("a first contribution always exists")
    }
    pub fn more_output(&self) -> Option<ToolOutput> {
        self.more_output_or(false)
    }
    fn more_output_or(&self, first: bool) -> Option<ToolOutput> {
        let leased = self.link.lock().unwrap().lease.clone();
        if leased.is_some() {
            return leased;
        }
        let output = self.render(first);
        self.link.lock().unwrap().lease = output.clone();
        output
    }
    /// Release a leased contribution only after its recipient owns it.
    pub fn acknowledge_output(&self) -> bool {
        self.link.lock().unwrap().lease.take().is_some()
    }
    pub fn cancel(&self) {
        let _ = self.shared.send(Input::Cancel { cell: self.cell });
        let state = self.link.lock().unwrap();
        state.cancelled.send_replace(true);
        for source in &state.sources {
            if let Some(process) = &source.process {
                process.cancel.notify_one();
            }
        }
    }
    /// Its holder is done with the cell: stop whatever it still owes, and
    /// forget it.
    pub fn release(&self) {
        if !self.done() {
            self.cancel();
        }
        self.shared.cells.lock().unwrap().remove(&self.cell);
    }
}
