//! Managed commands: `command()`, `write_stdin()` and `Command`. Rho owns
//! the subprocesses and their retained output.
use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use rho_agent_types::UnixMs;
use rho_tool_shell::{ProcessEvent, ShellTools};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, watch};

use crate::notebook::{Shared, current, register};
use crate::runtime::{Build, Inbox, Message, Reply, resolved};
use crate::source::{CommandExit, Kind, Log, Process, Source};

const LOG_LIMIT: usize = 4 * 1024 * 1024;
const OUTPUT_TOKEN_LIMIT: usize = 10000;

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

fn new_job(
    shared: &Shared,
    cell: &Source,
    cmd: &str,
    budget: usize,
) -> Result<Arc<Source>, String> {
    let mut jobs = shared.sources.lock().unwrap();
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (writes, queued) = tokio::sync::mpsc::unbounded_channel();
    let job = Arc::new(Source::new(
        id,
        Kind::Command,
        command_name(cmd),
        cell.id,
        budget,
        Some(Process {
            writes,
            queued: Mutex::new(Some(queued)),
            done: watch::channel(false).0,
        }),
        Some(Log {
            file: {
                let dir = std::env::var_os("XDG_STATE_HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                            .join(".local/state")
                    })
                    .join("rho/notebook-output");
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                tempfile::tempfile_in(dir).map_err(|e| e.to_string())?
            },
            len: 0,
            dropped: 0,
            cursor: 0,
            exit: None,
            gone: false,
        }),
    ));
    jobs.insert(id, Arc::clone(&job));
    Ok(job)
}

fn job(shared: &Shared, id: u64) -> PyResult<Arc<Source>> {
    shared
        .sources
        .lock()
        .unwrap()
        .get(&id)
        .filter(|source| source.process.is_some())
        .cloned()
        .ok_or_else(|| PyRuntimeError::new_err("Command handle expired or unknown"))
}

/// The command a handle names, touched by the running cell: the model is
/// watching it again.
fn touch(py: Python<'_>, id: u64) -> PyResult<(Arc<Shared>, Arc<Source>)> {
    let (shared, _) = current(py, "Commands are available")?;
    let job = job(&shared, id)?;
    Ok((shared, job))
}

fn budget(max_tokens: Option<i64>) -> PyResult<usize> {
    match max_tokens {
        None => Ok(2000),
        Some(tokens) if tokens >= 1 => Ok((tokens as usize).min(OUTPUT_TOKEN_LIMIT)),
        Some(_) => Err(PyValueError::new_err(
            "max_tokens must be a positive integer",
        )),
    }
}

/// Start a shell command. The handle is usable at once; awaiting it waits
/// for the command to end.
#[pyfunction]
#[pyo3(signature = (cmd, *, workdir = None, max_tokens = None))]
pub(crate) fn command(
    py: Python<'_>,
    cmd: String,
    workdir: Option<String>,
    max_tokens: Option<i64>,
) -> PyResult<Command> {
    let budget = budget(max_tokens)?;
    let (shared, cell) = current(py, "Commands are available")?;
    let shared = &shared;
    let future = crate::runtime::future(py, shared)?;
    // Published synchronously: write_stdin in the same cell can refer to
    // a command whose process has not started yet.
    let job = new_job(shared, &cell, &cmd, budget).map_err(PyRuntimeError::new_err)?;
    crate::interpreter::kernel(py)?
        .getattr("CELL")?
        .call_method0("get")?
        .call_method1("command", (future.clone_ref(py),))?;
    let id = job.id;
    let shell = shared.shell.clone();
    let reply = future.clone_ref(py);
    let inbox = Arc::clone(&shared.inbox);
    let wake = Arc::clone(&shared.wake);
    let shared_for_work = Arc::clone(shared);
    let registered = register(
        shared,
        &cell,
        Arc::clone(&job),
        async move {
            let process = job.process();
            let mut writes = (process.queued.lock().unwrap().take())
                .expect("a command's task takes its writes once");
            let result = run_command(
                &shell,
                &job,
                &wake,
                &cmd,
                workdir.as_deref(),
                &mut writes,
                &shared_for_work,
            )
            .await;
            // Whatever is still queued never reaches the process.
            writes.close();
            while let Ok(write) = writes.try_recv() {
                write.settle(Err("Command stdin not ready or closed".into()), &job);
            }
            // Failure is a fact of the process, computed here and nowhere
            // else: a non-zero exit, no exit code at all (a signal), a spawn
            // failure, or a cancellation.
            // An explicit cancellation is quiet even when it terminates a
            // subprocess: it must not schedule a failure wake.
            let failed = match &result {
                Ok(CommandExit {
                    exit_code: Some(0), ..
                }) => false,
                Err(error) if error == "Command cancelled" => false,
                _ => true,
            };
            let mut state = job.state.lock().unwrap();
            state.log.as_mut().expect("a command keeps a log").exit = Some(result.clone());
            state.finished = Some(UnixMs::now());
            state.failed = failed;
            drop(state);
            process.done.send_replace(true);
            wake.notify_one();
            result
        },
        move |result| inbox.post(Message::Done(reply, exit_reply(id, result))),
    );
    if let Err(error) = registered {
        shared.sources.lock().unwrap().remove(&id);
        return Err(PyRuntimeError::new_err(error));
    }
    Ok(Command {
        id,
        result: Mutex::new(Some(future)),
    })
}

/// How a command ended, as awaiting its handle returns it.
fn exit_reply(id: u64, result: Result<CommandExit, String>) -> Reply {
    Ok(result.unwrap_or(CommandExit {
        id,
        exit_code: None,
    }))
    .map(|exit| {
        Box::new(move |py: Python<'_>| Ok(exit.into_pyobject(py)?.into_any().unbind())) as Build
    })
}

/// Send input to a running command. The write is queued on the command at
/// once, so it happens whether or not anyone awaits; awaiting waits for the
/// write, not for output. Reading output is `more_output`'s job, so that one
/// function owns the cursor and nobody reads by accident.
#[pyfunction]
pub(crate) fn write_stdin(
    py: Python<'_>,
    handle: PyRef<'_, Command>,
    chars: String,
) -> PyResult<Py<PyAny>> {
    let (shared, job) = touch(py, handle.id)?;
    let shared = &shared;
    if chars.is_empty() {
        return resolved(py, shared);
    }
    let future = crate::runtime::future(py, shared)?;
    let write = StdinWrite {
        chars,
        reply: (future.clone_ref(py), Arc::clone(&shared.inbox)),
    };
    job.process()
        .writes
        .send(write)
        .map_err(|_| PyRuntimeError::new_err("Command stdin not ready or closed"))?;
    Ok(future)
}

/// One `write_stdin`, queued on its command.
pub(crate) struct StdinWrite {
    chars: String,
    reply: (Py<PyAny>, Arc<Inbox>),
}

impl StdinWrite {
    /// The write's outcome, to whoever awaits it. A failure also goes into
    /// the command's own output, since nobody may be awaiting.
    fn settle(self, result: Result<(), String>, job: &Source) {
        if let Err(error) = &result {
            let mut state = job.state.lock().unwrap();
            state.output(format!("write_stdin failed: {error}\n").as_bytes());
            state.since.get_or_insert_with(UnixMs::now);
        }
        let (future, inbox) = self.reply;
        let result = result.map(|()| Box::new(|py: Python<'_>| Ok(py.None())) as Build);
        inbox.post(Message::Done(future, result));
    }
}

/// A managed command. Awaiting it waits for the command to end.
#[pyclass(frozen, module = "__main__")]
pub(crate) struct Command {
    id: u64,
    /// Resolves when the command ends.
    result: Mutex<Option<Py<PyAny>>>,
}

#[pymethods]
impl Command {
    #[getter]
    fn id(&self) -> u64 {
        self.id
    }

    /// The live command a report's session ID refers to.
    ///
    /// A session ID is a label the reports show, not a handle, and labels
    /// come round again every 9,000 requests. Only live jobs are searched,
    /// and two live jobs wearing one label is an error rather than a guess.
    #[staticmethod]
    fn from_session_id(py: Python<'_>, session_id: u64) -> PyResult<Self> {
        let (shared, _) = current(py, "Commands are available")?;
        let found: Vec<u64> = (shared.sources.lock().unwrap().values())
            .filter(|source| source.process.is_some())
            .map(|source| source.id)
            .filter(|id| u64::from(crate::source::session_id(*id)) == session_id)
            .collect();
        match found.as_slice() {
            [id] => Ok(Self {
                id: *id,
                result: Mutex::new(None),
            }),
            [] => Err(PyRuntimeError::new_err(format!(
                "No live command has session ID {session_id}"
            ))),
            _ => Err(PyRuntimeError::new_err(format!(
                "Session ID {session_id} names more than one live command; keep the handle command() returned"
            ))),
        }
    }

    fn __await__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let (shared, job) = touch(py, self.id)?;
        let mut result = self.result.lock().unwrap();
        let future = match &*result {
            Some(future) => future.clone_ref(py),
            // Waiting starts nothing: no job of the cell's and no report,
            // since the command reports its own end.
            None => {
                let future = crate::runtime::future(py, &shared)?;
                let reply = future.clone_ref(py);
                let inbox = Arc::clone(&shared.inbox);
                shared.runtime.spawn(async move {
                    let mut done = job.process().done.subscribe();
                    let result = loop {
                        let exit = (job.state.lock().unwrap().log.as_ref())
                            .and_then(|log| log.exit.clone());
                        if let Some(result) = exit {
                            break result;
                        }
                        if let Err(error) = done.changed().await {
                            break Err(error.to_string());
                        }
                    };
                    inbox.post(Message::Done(reply, exit_reply(job.id, result)));
                });
                result.insert(future).clone_ref(py)
            }
        };
        drop(result);
        py.import("asyncio")?
            .call_method1("shield", (future,))?
            .call_method0("__await__")
    }

    /// Ask the command for the next page of its retained output. The
    /// command answers in its next report, like everything else it says.
    /// The awaitable is already done.
    #[pyo3(signature = (*, max_tokens = None))]
    fn more_output(&self, py: Python<'_>, max_tokens: Option<i64>) -> PyResult<Py<PyAny>> {
        let max_tokens = budget(max_tokens)?;
        let (shared, job) = touch(py, self.id)?;
        let mut state = job.state.lock().unwrap();
        state.pages.push(max_tokens);
        state.paged_at.get_or_insert_with(UnixMs::now);
        drop(state);
        shared.wake.notify_one();
        resolved(py, &shared)
    }

    /// Stop the command. The request is made on the spot and its outcome is
    /// the command's own report; the awaitable is already done.
    fn cancel(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let (shared, job) = touch(py, self.id)?;
        job.cancel.notify_one();
        resolved(py, &shared)
    }

    fn __repr__(&self) -> String {
        format!("<command {}>", self.id)
    }
}

pub(crate) async fn run_command(
    shell: &ShellTools,
    job: &Source,
    wake: &Notify,
    cmd: &str,
    workdir: Option<&str>,
    writes: &mut tokio::sync::mpsc::UnboundedReceiver<StdinWrite>,
    shared: &Shared,
) -> Result<CommandExit, String> {
    let mut process = tokio::select! {
        biased;
        () = job.cancel.notified() => return Err("Command cancelled".into()),
        process = shell.spawn(cmd, workdir) => process.map_err(|e| e.to_string())?,
    };
    let mut stdin = process.take_stdin();
    // The write in flight, so one the command's end cuts short is still
    // answered.
    let mut writing: Option<StdinWrite> = None;
    let feed = async {
        while let Some(mut write) = writes.recv().await {
            let chars = std::mem::take(&mut write.chars);
            writing = Some(write);
            let result = match stdin.as_mut() {
                Some(stdin) => async {
                    stdin.write_all(chars.as_bytes()).await?;
                    stdin.flush().await
                }
                .await
                .map_err(|e| e.to_string()),
                None => Err("Command stdin not ready or closed".to_owned()),
            };
            let failed = result.is_err();
            (writing.take().expect("set above")).settle(result, job);
            if failed {
                wake.notify_one();
            }
        }
    };
    let read = async {
        let mut exit_code = None;
        loop {
            let event = process.next().await;
            match event {
                ProcessEvent::Output(chunk) => {
                    shared.retain_space(job.id, chunk.len());
                    let mut state = job.state.lock().unwrap();
                    let log = state.log.as_mut().expect("a command keeps a log");
                    let keep = if log.gone {
                        0
                    } else {
                        chunk.len().min(LOG_LIMIT - log.len)
                    };
                    log.file
                        .seek(SeekFrom::Start(log.len as u64))
                        .map_err(|e| e.to_string())?;
                    log.file
                        .write_all(&chunk[..keep])
                        .map_err(|e| e.to_string())?;
                    log.len += keep;
                    log.dropped = log.dropped.saturating_add(chunk.len() - keep);
                    state.output(&chunk);
                    state.since.get_or_insert_with(UnixMs::now);
                }
                ProcessEvent::Exited(status) => exit_code = status.code(),
                ProcessEvent::Failed(error) => return Err(error),
                ProcessEvent::Closed => break,
            }
            wake.notify_one();
        }
        Ok(CommandExit {
            id: job.id,
            exit_code,
        })
    };
    let result = tokio::select! {
        biased;
        () = job.cancel.notified() => Err("Command cancelled".into()),
        result = read => result,
        // The command holds its own sender, so the queue never closes first.
        () = feed => unreachable!("a command's write queue outlives it"),
    };
    if let Some(write) = writing.take() {
        write.settle(Err("Command ended before the write finished".into()), job);
    }
    process
        .terminate()
        .await
        .map_err(|error| error.to_string())?;
    result
}
