//! The notebook thread and the kernel's connection to Rust.
//!
//! `kernel.py` owns notebook semantics: cells, streaming units, ownership
//! and output routing. This side owns transport: the inbox the event loop
//! drains, and the asyncio futures host work resolves when it ends.
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::cell::{Cell, PythonExec};
use crate::notebook::{Export, Shared};

pub(crate) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

pub(crate) enum Input {
    Execute {
        exec: Arc<PythonExec>,
        source: String,
    },
    BeginStream {
        exec: Arc<PythonExec>,
    },
    StreamFeed {
        cell: u64,
        source: String,
        eof: bool,
    },
    StreamPermit {
        cell: u64,
        end: usize,
    },
    StreamStop {
        cell: u64,
    },
    Cancel {
        cell: u64,
    },
    Shutdown,
}

/// A host result, built into Python on the notebook thread.
pub(crate) type Build = Box<dyn FnOnce(Python<'_>) -> PyResult<Py<PyAny>> + Send>;
pub(crate) type Reply = Result<Build, String>;

/// What other threads post to the notebook thread.
pub(crate) enum Message {
    Input(Input),
    /// Host work ended: resolve the asyncio future Python holds for it.
    Done(Py<PyAny>, Reply),
}

/// The notebook thread's queue. Posting never needs the interpreter lock:
/// an eventfd the event loop watches says the queue is non-empty.
pub(crate) struct Inbox {
    queue: Mutex<VecDeque<Message>>,
    wake: OwnedFd,
}

impl Inbox {
    pub(crate) fn new() -> Result<Self, String> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(format!(
                "notebook eventfd: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            queue: Mutex::default(),
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub(crate) fn post(&self, message: Message) {
        self.queue.lock().unwrap().push_back(message);
        let one = 1u64;
        unsafe { libc::write(self.wake.as_raw_fd(), (&raw const one).cast(), 8) };
    }

    /// Everything posted so far, in order.
    fn take(&self) -> VecDeque<Message> {
        let mut count = 0u64;
        unsafe { libc::read(self.wake.as_raw_fd(), (&raw mut count).cast(), 8) };
        std::mem::take(&mut *self.queue.lock().unwrap())
    }
}

impl Shared {
    /// Queue an input behind whatever is already queued. Admission is
    /// unbounded so synchronous Python cannot block delivery of the
    /// completions it needs to make progress. Fails only when the input is
    /// too large or the runtime is gone.
    pub(crate) fn send(&self, input: Input) -> Result<(), String> {
        let size = match &input {
            Input::Execute { source, .. } | Input::StreamFeed { source, .. } => source.len(),
            _ => 0,
        };
        if size > MAX_MESSAGE_BYTES {
            return Err("Python input exceeds 1 MiB".into());
        }
        if self.stopped.load(Ordering::Acquire) {
            return Err("Python runtime disconnected".into());
        }
        self.inbox.post(Message::Input(input));
        Ok(())
    }
}

/// Start the notebook thread. `setup` runs on it after unsharing its cwd
/// state, before Python starts.
pub(crate) fn spawn(
    shared: Arc<Shared>,
    setup: Box<dyn FnOnce() -> Result<(), String> + Send>,
    exports: Vec<Export>,
) -> Result<(), String> {
    crate::interpreter::initialize()?;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rho-python".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&shared, setup, exports, &ready_tx)
            }));
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some("Python notebook panicked; notebook state lost".into()),
            };
            let _ = ready_tx.send(Err(error
                .clone()
                .unwrap_or_else(|| "Python runtime stopped during startup".into())));
            shared.stop_runtime(error);
            // Nothing will drain these; queued cells hold the notebook.
            drop(shared.inbox.take());
        })
        .map_err(|e| e.to_string())?;
    ready_rx
        .recv_timeout(Duration::from_secs(60))
        .map_err(|e| format!("Python startup failed: {e}"))?
}

fn run(
    shared: &Arc<Shared>,
    setup: Box<dyn FnOnce() -> Result<(), String> + Send>,
    exports: Vec<Export>,
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
    let _ = shared.thread.set(std::thread::current().id());
    Python::attach(|py| {
        let mut objects = vec![
            (
                "command",
                pyo3::wrap_pyfunction!(crate::commands::command, py)?.into_any(),
            ),
            (
                "write_stdin",
                pyo3::wrap_pyfunction!(crate::commands::write_stdin, py)?.into_any(),
            ),
            (
                "Command",
                py.get_type::<crate::commands::Command>().into_any(),
            ),
        ];
        for export in exports {
            objects.push((export.name, (export.build)(py)?.into_bound(py)));
        }
        let notebook = crate::interpreter::kernel(py)?
            .getattr("Notebook")?
            .call1((
                Driver {
                    shared: Arc::clone(shared),
                },
                objects,
            ))?;
        let _ = ready.send(Ok(()));
        notebook.call_method0("run").map(drop)
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

/// A new asyncio future on the notebook's loop, for host work to resolve.
pub(crate) fn future(py: Python<'_>, shared: &Shared) -> PyResult<Py<PyAny>> {
    if shared.thread.get() != Some(&std::thread::current().id()) {
        return Err(PyRuntimeError::new_err(
            "Call host functions from the notebook event loop, not a worker thread",
        ));
    }
    let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
    Ok(event_loop.call_method0("create_future")?.unbind())
}

fn resolve(py: Python<'_>, future: Py<PyAny>, reply: Reply) -> PyResult<()> {
    let future = future.bind(py);
    if future.call_method0("done")?.is_truthy()? {
        return Ok(());
    }
    let outcome = reply
        .map_err(PyRuntimeError::new_err)
        .and_then(|build| build(py));
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
        self.shared.inbox.wake.as_raw_fd()
    }

    /// Inputs posted since the last drain, in order, as tuples. Finished
    /// host work resolves its futures here.
    fn drain(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        let mut inputs = Vec::new();
        for message in self.shared.inbox.take() {
            match message {
                Message::Done(future, reply) => resolve(py, future, reply)?,
                Message::Input(input) => inputs.push(match input {
                    Input::Execute { exec, source } => {
                        ("execute", Cell::new(exec), source).into_py_any(py)?
                    }
                    Input::BeginStream { exec } => ("begin", Cell::new(exec)).into_py_any(py)?,
                    Input::StreamFeed { cell, source, eof } => {
                        ("feed", cell, source, eof).into_py_any(py)?
                    }
                    Input::StreamPermit { cell, end } => ("permit", cell, end).into_py_any(py)?,
                    Input::StreamStop { cell } => ("stop", cell).into_py_any(py)?,
                    Input::Cancel { cell } => ("cancel", cell).into_py_any(py)?,
                    Input::Shutdown => ("shutdown",).into_py_any(py)?,
                }),
            }
        }
        Ok(inputs)
    }
}
