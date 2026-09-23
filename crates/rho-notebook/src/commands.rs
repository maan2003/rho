//! Managed commands: `command()`, `write_stdin()` and `Command`. Rho owns
//! the subprocesses and their retained output.
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use rho_core::UnixMs;
use rho_tool_shell::{BoundedOutput, ProcessEvent, ShellTools};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, watch};

use crate::cell::current;
use crate::notebook::{ExecState, Shared, operation, register};
use crate::runtime::{Build, Message};
use crate::source::{CommandExit, Log, Process, Source, session_id};

const LOG_LIMIT: usize = 8 * 1024 * 1024;
const JOB_LIMIT: usize = 64;
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

fn new_job(shared: &Shared, cmd: &str, budget: usize) -> Result<Arc<Source>, String> {
    let mut jobs = shared.jobs.lock().unwrap();
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
    let id = shared.next_request.fetch_add(1, Ordering::Relaxed);
    let job = Arc::new(Source::new(
        id,
        command_name(cmd),
        budget,
        Some(Process {
            budget,
            stdin: tokio::sync::Mutex::new(None),
            cancel: Notify::new(),
            ready: watch::channel(false).0,
            done: watch::channel(false).0,
        }),
        Some(Log {
            file: tempfile::tempfile().map_err(|e| e.to_string())?,
            len: 0,
            dropped: 0,
            cursor: 0,
            exit: None,
        }),
    ));
    jobs.insert(id, Arc::clone(&job));
    Ok(job)
}

fn job(shared: &Shared, id: u64) -> PyResult<Arc<Source>> {
    shared
        .jobs
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| PyRuntimeError::new_err("Command handle expired or unknown"))
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
    let exec = current(py, "Commands are available")?;
    let shared = &exec.shared;
    let future = crate::runtime::future(py, shared)?;
    // Published synchronously: write_stdin in the same cell can refer to
    // a command whose process has not started yet.
    let job = new_job(shared, &cmd, budget).map_err(PyRuntimeError::new_err)?;
    let id = job.id;
    let shell = shared.shell.clone();
    let reply = future.clone_ref(py);
    let inbox = Arc::clone(&shared.inbox);
    let registered = register(
        shared,
        exec.cell,
        Arc::clone(&job),
        move |link| async move {
            let process = job.process();
            let result = run_command(&shell, &job, &cmd, workdir.as_deref(), &link).await;
            *process.stdin.lock().await = None;
            process.ready.send_replace(true);
            // Failure is a fact of the process, computed here and nowhere
            // else: a non-zero exit, no exit code at all (a signal), a spawn
            // failure, or a cancellation.
            let failed = !matches!(
                &result,
                Ok(CommandExit {
                    exit_code: Some(0),
                    ..
                })
            );
            let mut state = job.state.lock().unwrap();
            state.log.as_mut().expect("a command keeps a log").exit = Some(result.clone());
            state.finished = Some(UnixMs::now());
            state.failed = failed;
            drop(state);
            process.done.send_replace(true);
            result
        },
        move |result: Result<CommandExit, String>| {
            let result = result.map(|exit| {
                Box::new(move |py: Python<'_>| Ok(exit.into_pyobject(py)?.into_any().unbind()))
                    as Build
            });
            inbox.post(Message::Done(reply, result));
        },
    );
    if let Err(error) = registered {
        shared.jobs.lock().unwrap().remove(&id);
        return Err(PyRuntimeError::new_err(error));
    }
    Ok(Command {
        id,
        result: Mutex::new(Some(future)),
    })
}

/// Send input to a running command. Awaiting waits for the write, not for
/// output; reading output is `more_output`'s job, so that one function owns
/// the cursor and nobody reads by accident.
#[pyfunction]
pub(crate) fn write_stdin(
    py: Python<'_>,
    handle: PyRef<'_, Command>,
    chars: String,
) -> PyResult<Py<PyAny>> {
    let job = job(&current(py, "Commands are available")?.shared, handle.id)?;
    operation(py, "write_stdin", move |_| async move {
        if chars.is_empty() {
            return Ok(());
        }
        let process = job.process();
        process
            .ready
            .subscribe()
            .wait_for(|ready| *ready)
            .await
            .map_err(|e| e.to_string())?;
        let mut stdin = process.stdin.lock().await;
        let stdin = stdin.as_mut().ok_or("Command stdin not ready or closed")?;
        stdin
            .write_all(chars.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())
    })
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
        let exec = current(py, "Commands are available")?;
        let found: Vec<u64> = exec
            .shared
            .jobs
            .lock()
            .unwrap()
            .keys()
            .copied()
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
        let mut result = self.result.lock().unwrap();
        let future = match &*result {
            Some(future) => future.clone_ref(py),
            None => {
                let job = job(&current(py, "Commands are available")?.shared, self.id)?;
                let future = operation(py, "wait_command", move |_| async move {
                    let mut done = job.process().done.subscribe();
                    loop {
                        let exit = (job.state.lock().unwrap().log.as_ref())
                            .and_then(|log| log.exit.clone());
                        if let Some(result) = exit {
                            return result;
                        }
                        done.changed().await.map_err(|e| e.to_string())?;
                    }
                })?;
                result.insert(future).clone_ref(py)
            }
        };
        drop(result);
        py.import("asyncio")?
            .call_method1("shield", (future,))?
            .call_method0("__await__")
    }

    /// The next page of the command's retained output.
    #[pyo3(signature = (*, max_tokens = None))]
    fn more_output(&self, py: Python<'_>, max_tokens: Option<i64>) -> PyResult<Py<PyAny>> {
        let max_tokens = budget(max_tokens)?;
        let job = job(&current(py, "Commands are available")?.shared, self.id)?;
        operation(py, "more_output", move |cx| async move {
            cx.report(&read_page(&job, max_tokens)?);
            Ok(())
        })
    }

    /// Stop the command.
    fn cancel(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let job = job(&current(py, "Commands are available")?.shared, self.id)?;
        operation(py, "cancel_command", move |_| async move {
            job.process().cancel.notify_one();
            Ok(())
        })
    }

    fn __repr__(&self) -> String {
        format!("<command {}>", self.id)
    }
}

/// The next page of a command's log, in the shape its own output arrives in.
pub(crate) fn read_page(job: &Source, max_tokens: usize) -> Result<String, String> {
    let mut state = job.state.lock().unwrap();
    let finished = state.finished.is_some();
    let log = state.log.as_mut().expect("a command keeps a log");
    let start = log.cursor;
    let size = (log.len - start).min(max_tokens * 4);
    let mut bytes = vec![0; size];
    log.file
        .seek(SeekFrom::Start(start as u64))
        .map_err(|e| e.to_string())?;
    log.file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    // Do not split a UTF-8 character merely because a page hit its budget.
    // Non-UTF-8 process output still follows the shell's lossy-text contract.
    if size < log.len - start
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
    log.cursor += bytes.len();
    let remaining = log.len - log.cursor;
    let dropped = log.dropped;
    // Reading by hand takes over from the automatic report: whatever was
    // waiting to be reported is dropped, so the next reply does not say
    // again what this page just showed. The rest is paged the same way.
    state.unsent = BoundedOutput::for_tokens(Some(job.process().budget));
    state.since = None;
    drop(state);
    let page = String::from_utf8_lossy(&bytes).into_owned();
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

pub(crate) async fn run_command(
    shell: &ShellTools,
    job: &Source,
    cmd: &str,
    workdir: Option<&str>,
    link: &Arc<Mutex<ExecState>>,
) -> Result<CommandExit, String> {
    let control = job.process();
    let mut cancelled = link.lock().unwrap().cancelled.subscribe();
    let mut process = tokio::select! {
        biased;
        _ = cancelled.wait_for(|cancelled| *cancelled) => return Err("Command cancelled".into()),
        _ = control.cancel.notified() => return Err("Command cancelled".into()),
        process = shell.spawn(cmd, workdir) => process.map_err(|e| e.to_string())?,
    };
    let work = async {
        *control.stdin.lock().await = process.take_stdin();
        control.ready.send_replace(true);
        let mut exit_code = None;
        loop {
            let event = process.next().await;
            match event {
                ProcessEvent::Output(chunk) => {
                    let mut state = job.state.lock().unwrap();
                    let log = state.log.as_mut().expect("a command keeps a log");
                    let keep = chunk.len().min(LOG_LIMIT - log.len);
                    log.file
                        .seek(SeekFrom::Start(log.len as u64))
                        .map_err(|e| e.to_string())?;
                    log.file
                        .write_all(&chunk[..keep])
                        .map_err(|e| e.to_string())?;
                    log.len += keep;
                    log.dropped = log.dropped.saturating_add(chunk.len() - keep);
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
        _ = control.cancel.notified() => Err("Command cancelled".into()),
        result = work => result,
    };
    process
        .terminate()
        .await
        .map_err(|error| error.to_string())?;
    result
}
