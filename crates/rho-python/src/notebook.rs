//! The notebook thread and what its Python kernel calls into.
//!
//! `kernel.py` owns notebook semantics: cells, streaming units, ownership
//! and the Python-facing API. This side owns transport: the inbox the event
//! loop drains, events for Rust executions, and host calls, whose futures
//! run on the host's Tokio runtime and resolve asyncio futures when drained.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyTuple};

use crate::host::{Commands, Function, History, HistoryItem, Host, IntoPython, Pending};
use crate::{CellId, Event, Inbox, Input, Message, Registry};

const CALL_LIMIT: usize = 1024;

/// One notebook's state outside Python, shared with its cells.
struct Shared {
    inbox: Arc<Inbox>,
    registry: Arc<Registry>,
    runtime: tokio::runtime::Handle,
    thread: std::thread::ThreadId,
    functions: HashMap<&'static str, Function>,
    commands: Option<Arc<dyn Commands>>,
    history: Arc<dyn History>,
    /// Host calls still running, by call ID: the asyncio future they
    /// resolve and their Tokio task.
    calls: Mutex<HashMap<u64, (Py<PyAny>, tokio::task::AbortHandle)>>,
    next_call: AtomicU64,
}

pub(crate) fn spawn(
    inbox: Arc<Inbox>,
    registry: Arc<Registry>,
    setup: Box<dyn FnOnce() -> Result<(), String> + Send>,
    host: Host,
    runtime: tokio::runtime::Handle,
) -> Result<(), String> {
    crate::interpreter::initialize()?;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rho-python".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let ended = Arc::clone(&registry);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(inbox, registry, setup, host, runtime, &ready_tx)
            }));
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some("Python notebook panicked; notebook state lost".into()),
            };
            let _ = ready_tx.send(Err(error
                .clone()
                .unwrap_or_else(|| "Python runtime stopped during startup".into())));
            ended.stop(error);
        })
        .map_err(|e| e.to_string())?;
    ready_rx
        .recv_timeout(Duration::from_secs(60))
        .map_err(|e| format!("Python startup failed: {e}"))?
}

fn run(
    inbox: Arc<Inbox>,
    registry: Arc<Registry>,
    setup: Box<dyn FnOnce() -> Result<(), String> + Send>,
    host: Host,
    runtime: tokio::runtime::Handle,
    ready: &mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    // Filesystem state (not the process or descriptor table) is private to
    // this thread and the threads it starts: Python chdir must not move the
    // daemon or another notebook. The caller then installs its workspace view.
    if unsafe { libc::unshare(libc::CLONE_FS) } != 0 {
        return Err(format!(
            "unshare notebook cwd: {}",
            std::io::Error::last_os_error()
        ));
    }
    setup()?;
    let exports: Vec<(&'static str, bool)> = host
        .functions
        .iter()
        .map(|function| (function.path, function.detached))
        .collect();
    let shared = Arc::new(Shared {
        inbox,
        registry,
        runtime,
        thread: std::thread::current().id(),
        functions: host
            .functions
            .into_iter()
            .map(|function| (function.path, function))
            .collect(),
        commands: host.commands,
        history: host.history,
        calls: Mutex::default(),
        next_call: AtomicU64::new(0),
    });
    Python::attach(|py| {
        let notebook = crate::interpreter::kernel(py)?
            .getattr("Notebook")?
            .call1((Driver { shared: Arc::clone(&shared) }, exports))?;
        let _ = ready.send(Ok(()));
        let result = notebook.call_method0("run").map(drop);
        // Nothing will resolve these futures now; stop their host work.
        for (_, task) in std::mem::take(&mut *shared.calls.lock().unwrap()).into_values() {
            task.abort();
        }
        result
    })
    .map_err(|error: PyErr| Python::attach(|py| format_error(py, &error)))
}

fn format_error(py: Python<'_>, error: &PyErr) -> String {
    let formatted = py.import("traceback").and_then(|traceback| {
        traceback
            .call_method1("format_exception", (error.value(py),))?
            .extract::<Vec<String>>()
    });
    formatted.map_or_else(|_| error.to_string(), |lines| lines.concat())
}

impl Shared {
    fn emit(&self, event: Event) {
        self.registry.emit(event);
    }

    fn start_call(self: &Arc<Self>, py: Python<'_>, task: Pending) -> PyResult<Py<PyAny>> {
        if std::thread::current().id() != self.thread {
            return Err(PyRuntimeError::new_err(
                "Call host functions from the notebook event loop, not a worker thread",
            ));
        }
        if self.calls.lock().unwrap().len() >= CALL_LIMIT {
            return Err(PyRuntimeError::new_err("Too many pending host requests"));
        }
        let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
        let future = event_loop.call_method0("create_future")?.unbind();
        let id = self.next_call.fetch_add(1, Ordering::Relaxed);
        let inbox = Arc::clone(&self.inbox);
        let task = self.runtime.spawn(async move {
            let result = task.await;
            inbox.post(Message::Done(id, result));
        });
        let mut calls = self.calls.lock().unwrap();
        calls.insert(id, (future.clone_ref(py), task.abort_handle()));
        drop(calls);
        // Cancelling the awaitable cancels the host work.
        future.call_method1(py, "add_done_callback", (Abort { shared: Arc::clone(self), id },))?;
        Ok(future)
    }

    fn resolve(
        &self,
        py: Python<'_>,
        id: u64,
        result: Result<Box<dyn IntoPython>, String>,
    ) -> PyResult<()> {
        let Some((future, _)) = self.calls.lock().unwrap().remove(&id) else {
            return Ok(());
        };
        let future = future.bind(py);
        if future.call_method0("done")?.is_truthy()? {
            return Ok(());
        }
        let outcome = result
            .map_err(PyRuntimeError::new_err)
            .and_then(|value| value.into_python(py));
        match outcome {
            Ok(value) => {
                future.call_method1("set_result", (value,))?;
            }
            Err(error) => {
                future.call_method1("set_exception", (error.value(py),))?;
                // The host already reports its failures; don't also log an
                // unretrieved exception. Awaiting still raises it.
                future.call_method0("exception")?;
            }
        }
        Ok(())
    }
}

/// A host call's done-callback: a cancelled call stops its host work.
#[pyclass(frozen)]
struct Abort {
    shared: Arc<Shared>,
    id: u64,
}

#[pymethods]
impl Abort {
    fn __call__(&self, future: &Bound<'_, PyAny>) -> PyResult<()> {
        if future.call_method0("cancelled")?.is_truthy()?
            && let Some((_, task)) = self.shared.calls.lock().unwrap().remove(&self.id)
        {
            task.abort();
        }
        Ok(())
    }
}

/// The notebook's connection to Rust, given to its kernel.
#[pyclass(frozen)]
struct Driver {
    shared: Arc<Shared>,
}

#[pymethods]
impl Driver {
    /// The descriptor that becomes readable when inputs arrive.
    #[getter]
    fn fd(&self) -> i32 {
        self.shared.inbox.fd()
    }

    /// Inputs posted since the last drain, in order, as tuples. Finished
    /// host calls resolve their futures here.
    fn drain(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        let mut inputs = Vec::new();
        for message in self.shared.inbox.take() {
            match message {
                Message::Done(id, result) => self.shared.resolve(py, id, result)?,
                Message::Input(input) => inputs.push(self.input(py, input)?),
            }
        }
        Ok(inputs)
    }
}

impl Driver {
    fn cell(&self, cell: CellId) -> Cell {
        Cell {
            id: cell,
            shared: Arc::clone(&self.shared),
        }
    }

    fn input(&self, py: Python<'_>, input: Input) -> PyResult<Py<PyAny>> {
        match input {
            Input::Execute { cell, source } => ("execute", self.cell(cell), source).into_py_any(py),
            Input::BeginStream { cell } => ("begin", self.cell(cell)).into_py_any(py),
            Input::StreamFeed { cell, source, eof } => ("feed", cell, source, eof).into_py_any(py),
            Input::StreamPermit { cell, end } => ("permit", cell, end).into_py_any(py),
            Input::StreamStop { cell } => ("stop", cell).into_py_any(py),
            Input::Cancel { cell } => ("cancel", cell).into_py_any(py),
            Input::Shutdown => ("shutdown",).into_py_any(py),
        }
    }
}

/// A cell as its kernel sees it: where its events go and what it may call.
#[pyclass(frozen)]
struct Cell {
    id: CellId,
    shared: Arc<Shared>,
}

#[pymethods]
impl Cell {
    #[getter]
    fn id(&self) -> CellId {
        self.id
    }

    fn started(&self) {
        self.shared.emit(Event::Started { cell: self.id });
    }

    fn unit_ready(&self, end: usize) {
        self.shared.emit(Event::UnitReady { cell: self.id, end });
    }

    fn unit_settled(&self, end: usize, error: Option<String>) {
        self.shared.emit(Event::UnitSettled {
            cell: self.id,
            end,
            error,
        });
    }

    fn returned(&self, error: Option<String>) {
        self.shared.emit(Event::Returned {
            cell: self.id,
            error,
        });
    }

    fn finished(&self, error: Option<String>) {
        self.shared.emit(Event::Finished {
            cell: self.id,
            error,
        });
    }

    fn text(&self, text: String, max_tokens: usize, important: bool) {
        self.shared.emit(Event::Text {
            cell: self.id,
            text,
            max_tokens,
            important,
        });
    }

    fn max_wait(&self, seconds: u64) {
        self.shared.emit(Event::MaxWait {
            cell: self.id,
            seconds,
        });
    }

    fn suppress_tool_wakeups(&self) {
        self.shared
            .emit(Event::SuppressToolWakeups { cell: self.id });
    }

    /// Start the host function at `path`; the returned future resolves with
    /// its result.
    fn call(
        &self,
        py: Python<'_>,
        path: &str,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        let function = self.shared.functions.get(path).ok_or_else(|| {
            PyRuntimeError::new_err(format!("{path}() is not available in this notebook"))
        })?;
        if args.len() > function.positional.len() {
            return Err(PyTypeError::new_err(format!(
                "{path}() takes {} positional arguments but {} were given",
                function.positional.len(),
                args.len()
            )));
        }
        let arguments = PyDict::new(py);
        for (name, value) in function.positional.iter().zip(args.iter()) {
            arguments.set_item(*name, value)?;
        }
        for (name, value) in kwargs.into_iter().flatten() {
            if arguments.contains(&name)? {
                return Err(PyTypeError::new_err(format!(
                    "{path}() got multiple values for argument '{name}'"
                )));
            }
            arguments.set_item(name, value)?;
        }
        let task = (function.start)(self.id, arguments.as_any()).map_err(PyRuntimeError::new_err)?;
        self.shared.start_call(py, task)
    }

    fn command_start(
        &self,
        py: Python<'_>,
        cmd: String,
        workdir: Option<String>,
        max_tokens: usize,
    ) -> PyResult<(u64, Py<PyAny>)> {
        let (id, exit) = self
            .commands()?
            .start(self.id, cmd, workdir, max_tokens)
            .map_err(PyRuntimeError::new_err)?;
        Ok((id, self.shared.start_call(py, crate::host::pending(exit))?))
    }

    fn command_find(&self, session_id: u64) -> PyResult<u64> {
        self.commands()?
            .find(session_id)
            .map_err(PyRuntimeError::new_err)
    }

    fn command_wait(&self, py: Python<'_>, id: u64) -> PyResult<Py<PyAny>> {
        let task = self.commands()?.wait(self.id, id);
        self.pending(py, task)
    }

    fn command_write_stdin(&self, py: Python<'_>, id: u64, chars: String) -> PyResult<Py<PyAny>> {
        let task = self.commands()?.write_stdin(self.id, id, chars);
        self.pending(py, task)
    }

    fn command_more_output(&self, py: Python<'_>, id: u64, max_tokens: usize) -> PyResult<Py<PyAny>> {
        let task = self.commands()?.more_output(self.id, id, max_tokens);
        self.pending(py, task)
    }

    fn command_cancel(&self, py: Python<'_>, id: u64) -> PyResult<Py<PyAny>> {
        let task = self.commands()?.cancel(self.id, id);
        self.pending(py, task)
    }

    fn history_len(&self) -> PyResult<usize> {
        self.shared
            .history
            .len(self.id)
            .map_err(PyRuntimeError::new_err)
    }

    /// One transcript item as a plain tuple in `HistoryItem` field order;
    /// the kernel builds the named tuples.
    fn history_get<'py>(&self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyTuple>> {
        let item = self
            .shared
            .history
            .get(self.id, index)
            .map_err(PyRuntimeError::new_err)?;
        history_tuple(py, item)
    }
}

impl Cell {
    fn commands(&self) -> PyResult<&Arc<dyn Commands>> {
        self.shared
            .commands
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("This notebook has no command host"))
    }

    fn pending<T: serde::Serialize + Send + 'static>(
        &self,
        py: Python<'_>,
        task: Result<crate::HostFuture<T>, String>,
    ) -> PyResult<Py<PyAny>> {
        let task = task.map_err(PyRuntimeError::new_err)?;
        self.shared.start_call(py, crate::host::pending(task))
    }
}

fn history_tuple(py: Python<'_>, item: HistoryItem) -> PyResult<Bound<'_, PyTuple>> {
    let bytes = |data: &[u8]| PyBytes::new(py, data).into_any();
    let content = item
        .content
        .into_iter()
        .map(|part| {
            let data = part.data.as_deref().map(bytes);
            (part.kind, part.text, part.media_type, data).into_bound_py_any(py)
        })
        .collect::<PyResult<Vec<_>>>()?;
    let images = item
        .images
        .into_iter()
        .map(|image| (image.media_type, bytes(&image.data), image.detail).into_bound_py_any(py))
        .collect::<PyResult<Vec<_>>>()?;
    let provider = item
        .provider
        .map(|provider| (provider.tag, bytes(&provider.data)).into_bound_py_any(py))
        .transpose()?;
    let metadata = item.metadata.as_ref().map(serde_json::Value::to_string);
    PyTuple::new(
        py,
        [
            item.kind.into_bound_py_any(py)?,
            item.role.into_bound_py_any(py)?,
            item.sender.into_bound_py_any(py)?,
            item.text.into_bound_py_any(py)?,
            PyTuple::new(py, content)?.into_any(),
            item.name.into_bound_py_any(py)?,
            item.call_id.into_bound_py_any(py)?,
            PyTuple::new(py, item.summary)?.into_any(),
            PyTuple::new(py, images)?.into_any(),
            provider.into_bound_py_any(py)?,
            item.status.into_bound_py_any(py)?,
            item.phase.into_bound_py_any(py)?,
            item.tool_type.into_bound_py_any(py)?,
            item.started_at.into_bound_py_any(py)?,
            item.finished_at.into_bound_py_any(py)?,
            item.at.into_bound_py_any(py)?,
            item.retain_from.into_bound_py_any(py)?,
            PyTuple::new(py, item.call_ids)?.into_any(),
            item.response_id.into_bound_py_any(py)?,
            metadata.into_bound_py_any(py)?,
        ],
    )
}
