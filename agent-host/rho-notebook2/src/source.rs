//! A source: anything in the notebook that reports on its own. A cell, a
//! managed command, or a host call. All three live in one table, hold a
//! session ID from the start and report the same way. One that ends before
//! anyone hears of it speaks plainly, as a synchronous call would; one still
//! running when a report goes out is announced under its session ID, and
//! later pieces name that ID again.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

use pyo3::prelude::*;
use pyo3::types::PyDict;
use rho_agent_types::UnixMs;
use rho_tool_shell::{BoundedOutput, decode_output_lossy};
use tokio::sync::{Notify, mpsc, watch};

use crate::Image;
use crate::commands::StdinWrite;

/// The label a source is reported under. Scrambled so the model does not
/// read consecutive IDs as a count; labels repeat every 9,000 sources.
pub(crate) fn session_id(internal_id: u64) -> u32 {
    let x = internal_id % 9_000;
    let (mut left, mut right) = (x / 100, x % 100);
    left = (left + right * right + 17 * right + 43) % 90;
    right = (right + left * left + 29 * left + 71) % 100;
    left = (left + right * right + 53 * right + 19) % 90;
    right = (right + left * left + 11 * left + 37) % 100;
    (1_000 + 100 * left + right) as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cell,
    Task,
    Command,
    Call,
}

/// How a command ended, as awaiting its handle shows it.
#[derive(Clone, Debug)]
pub(crate) struct CommandExit {
    pub(crate) id: u64,
    pub(crate) exit_code: Option<i32>,
}

impl<'py> IntoPyObject<'py> for CommandExit {
    type Target = PyDict;
    type Output = Bound<'py, PyDict>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let exit = PyDict::new(py);
        exit.set_item("id", self.id)?;
        exit.set_item("exit_code", self.exit_code)?;
        Ok(exit)
    }
}

pub(crate) struct Source {
    pub(crate) id: u64,
    pub(crate) kind: Kind,
    /// A command's line, a call's name; a cell's is "Cell".
    pub(crate) name: String,
    /// The cell that started it; a cell's own id for a cell. Cancelling
    /// that cell stops it.
    pub(crate) cell: u64,
    /// Asks it to stop. Stores a permit, so an early request is not lost.
    pub(crate) cancel: Notify,
    pub(crate) process: Option<Process>,
    pub(crate) state: Mutex<State>,
}

pub(crate) struct Process {
    /// Writes for its stdin, queued even before it starts, and failed
    /// together if it ends first.
    pub(crate) writes: mpsc::UnboundedSender<StdinWrite>,
    /// The other end, until the command's task takes it.
    pub(crate) queued: Mutex<Option<mpsc::UnboundedReceiver<StdinWrite>>>,
    /// Flipped when the command ends.
    pub(crate) done: watch::Sender<bool>,
}

pub(crate) struct State {
    pub(crate) output_bytes: usize,
    pub(crate) dropped_output: bool,
    pub(crate) pending_failure: bool,
    pub(crate) budget: usize,
    /// Output not yet reported. A call's arrives with its end; a cell's and
    /// a command's as they come.
    pub(crate) unsent: BoundedOutput,
    /// Oldest unsent output.
    pub(crate) since: Option<UnixMs>,
    /// Oldest unsent `notify()`: cells only.
    pub(crate) notified: Option<UnixMs>,
    /// Told to the model as running.
    pub(crate) announced: bool,
    pub(crate) finished: Option<UnixMs>,
    pub(crate) failed: bool,
    /// A call's error, or a command's failure to run.
    pub(crate) error: Option<String>,
    /// Its end has been reported.
    pub(crate) delivered: bool,
    /// A command's retained output and exit.
    pub(crate) log: Option<Log>,
    /// Pages `more_output` asked for, for the next report to answer.
    pub(crate) pages: Vec<usize>,
    pub(crate) paged_at: Option<UnixMs>,
    pub(crate) images: Vec<Image>,
    /// A cell's own state.
    pub(crate) cell: CellState,
}

#[derive(Default)]
pub(crate) struct CellState {
    pub(crate) returned: Option<UnixMs>,
    pub(crate) cancelled: bool,
    pub(crate) stream: StreamProgress,
    pub(crate) stream_stopped: bool,
}

/// How far a streamed cell's code has got, as byte ends of whole top-level
/// statements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamProgress {
    pub ready: Option<usize>,
    pub admitted: usize,
    pub settled: usize,
}

pub(crate) struct Log {
    pub(crate) file: std::fs::File,
    pub(crate) len: usize,
    pub(crate) dropped: usize,
    pub(crate) cursor: usize,
    pub(crate) exit: Option<Result<CommandExit, String>>,
    pub(crate) gone: bool,
}

impl State {
    pub(crate) fn output(&mut self, bytes: &[u8]) {
        const LIMIT: usize = 4 * 1024 * 1024;
        let keep = bytes.len().min(LIMIT.saturating_sub(self.output_bytes));
        self.unsent.push(&bytes[..keep]);
        self.output_bytes += keep;
        if keep < bytes.len() && !self.dropped_output {
            self.dropped_output = true;
            self.unsent.push(b"\n[output past 4 MB was dropped]\n");
        }
    }
}

impl Source {
    pub(crate) fn new(
        id: u64,
        kind: Kind,
        name: String,
        cell: u64,
        budget: usize,
        process: Option<Process>,
        log: Option<Log>,
    ) -> Self {
        Self {
            id,
            kind,
            name,
            cell,
            cancel: Notify::new(),
            process,
            state: Mutex::new(State {
                output_bytes: 0,
                dropped_output: false,
                pending_failure: false,
                budget,
                unsent: BoundedOutput::for_tokens(Some(budget)),
                since: None,
                notified: None,
                announced: false,
                finished: None,
                failed: false,
                error: None,
                delivered: false,
                log,
                pages: Vec::new(),
                paged_at: None,
                images: Vec::new(),
                cell: CellState::default(),
            }),
        }
    }

    pub(crate) fn process(&self) -> &Process {
        self.process.as_ref().expect("a command has a process")
    }

    /// Whether it still owes a report: its end, or pages asked for since.
    pub(crate) fn owes_report(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.delivered || !state.pages.is_empty() || !state.unsent.is_empty()
    }

    pub(crate) fn take_images(&self) -> Vec<Image> {
        std::mem::take(&mut self.state.lock().unwrap().images)
    }

    fn label(&self) -> &str {
        match self.kind {
            Kind::Cell | Kind::Task => "Task",
            Kind::Command => "Command",
            Kind::Call => &self.name,
        }
    }

    /// Everything unsent, if anything. `old` names a command started two or
    /// more cells back, so the model can place it.
    pub(crate) fn report(&self, old: bool) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        if state.pending_failure {
            return None;
        }
        if !state.pages.is_empty() {
            return Some(self.answer_pages(state, old));
        }
        if state.delivered {
            if state.unsent.is_empty() {
                return None;
            }
            state.notified = None;
            state.since = None;
            return Some(format!(
                "Session ID: {}\nOutput:\n{}",
                session_id(self.id),
                take(state)
            ));
        }
        state.notified = None;
        // A call's text is its result, so it arrives with its end.
        let output =
            !state.unsent.is_empty() && (self.kind != Kind::Call || state.finished.is_some());
        if state.finished.is_some() && !state.announced && !state.failed {
            // Ended before anyone heard of it: its words are plain.
            state.delivered = true;
            let mut parts = Vec::new();
            if output {
                parts.push(take(state));
            }
            if let (Kind::Call, Some(error)) = (self.kind, &state.error) {
                parts.push(format!("{} failed: {error}", self.name));
            }
            // A silent cell still ended, and the model is owed that much.
            if matches!(self.kind, Kind::Cell | Kind::Task) && parts.is_empty() {
                parts.push(self.end(state));
            }
            return (!parts.is_empty()).then(|| parts.join("\n"));
        }
        if !output && state.finished.is_none() && state.announced {
            return None;
        }
        let mut parts = Vec::new();
        if state.finished.is_some() {
            if state.announced || self.kind != Kind::Cell {
                parts.push(format!("Session ID: {}", session_id(self.id)));
            }
            if old {
                match self.kind {
                    Kind::Command => parts.push(format!("Command: {}", self.name)),
                    Kind::Task => parts.push(format!("Task: {}()", self.name)),
                    _ => {}
                }
            }
            parts.push(self.end(state));
            if matches!(self.kind, Kind::Cell | Kind::Task)
                && let Some(error) = &state.error
            {
                parts.push(error.clone());
            }
        } else {
            state.announced = true;
            parts.push(format!(
                "{} running in background with session ID {}",
                self.label(),
                session_id(self.id)
            ));
            if old {
                match self.kind {
                    Kind::Command => parts.push(format!("Command: {}", self.name)),
                    Kind::Task => parts.push(format!("Task: {}()", self.name)),
                    _ => {}
                }
            }
        }
        if output {
            // A report and `more_output` share one cursor, so a page after a
            // report carries on where the report stopped. A report that
            // dropped its middle leaves the cursor, so the span can be paged.
            let complete = !state.unsent.is_truncated();
            if complete && let Some(log) = &mut state.log {
                log.cursor = log.len;
            }
            parts.push(format!("Output:\n{}", take(state)));
        }
        state.since = None;
        if state.finished.is_some() {
            state.delivered = true;
        }
        Some(parts.join("\n"))
    }

    /// How it ended, in one line.
    fn end(&self, state: &State) -> String {
        match self.kind {
            Kind::Cell | Kind::Task if state.cell.cancelled => "Task cancelled".to_owned(),
            Kind::Cell | Kind::Task if state.failed => "Task failed".to_owned(),
            Kind::Cell | Kind::Task => "Task finished".to_owned(),
            Kind::Call => match &state.error {
                Some(error) => format!("{} failed: {error}", self.name),
                None => format!("{} finished", self.name),
            },
            Kind::Command => match state.log.as_ref().and_then(|log| log.exit.as_ref()) {
                Some(Ok(CommandExit {
                    exit_code: Some(code),
                    ..
                })) => format!("Process exited with code {code}"),
                Some(Ok(CommandExit {
                    exit_code: None, ..
                })) => "Process ended without an exit code".to_owned(),
                Some(Err(error)) => format!("Command failed: {error}"),
                None => "Command ended".to_owned(),
            },
        }
    }

    /// The pages `more_output` asked for, under the session ID the model
    /// asked by, with the end too if that has not been reported yet.
    fn answer_pages(&self, state: &mut State, old: bool) -> String {
        let mut parts = vec![format!("Session ID: {}", session_id(self.id))];
        if old {
            parts.push(format!("Command: {}", self.name));
        }
        state.paged_at = None;
        if state.finished.is_none() {
            state.announced = true;
        } else if !state.delivered {
            parts.push(self.end(state));
            state.delivered = true;
        }
        for tokens in std::mem::take(&mut state.pages) {
            parts.push(page(state, tokens));
        }
        parts.join("\n")
    }

    pub(crate) fn facts(&self) -> SourceFacts {
        let state = self.state.lock().unwrap();
        SourceFacts {
            session_id: session_id(self.id),
            kind: self.kind,
            cell: self.cell,
            output_since: state.since,
            notified_at: state.notified,
            paged_at: state.paged_at,
            returned: state.cell.returned,
            finished: state.finished.map(|at| End {
                at,
                failed: state.failed,
            }),
            delivered: state.delivered,
        }
    }
}

fn take(state: &mut State) -> String {
    let unsent = std::mem::replace(
        &mut state.unsent,
        BoundedOutput::for_tokens(Some(state.budget)),
    );
    decode_output_lossy(unsent.into_bytes())
}

/// The next page of a command's log, from where the last report or page
/// stopped. Paging takes over from the automatic report: what was waiting
/// to be reported is dropped, so the reply does not say it twice.
fn page(state: &mut State, max_tokens: usize) -> String {
    let finished = state.finished.is_some();
    let Some(log) = state.log.as_mut() else {
        return "[only a command can be paged]".to_owned();
    };
    if log.gone {
        return "[retained output is gone: notebook reached its 50 MB limit]".to_owned();
    }
    let start = log.cursor;
    let size = (log.len - start).min(max_tokens * 4);
    let mut bytes = vec![0; size];
    let read = log
        .file
        .seek(SeekFrom::Start(start as u64))
        .and_then(|_| log.file.read_exact(&mut bytes));
    if let Err(error) = read {
        return format!("[the retained output could not be read: {error}]");
    }
    // Do not split a UTF-8 character merely because a page hit its budget.
    if size < log.len - start
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
    log.cursor += bytes.len();
    let remaining = log.len - log.cursor;
    let dropped = log.dropped;
    state.unsent = BoundedOutput::for_tokens(Some(state.budget));
    state.since = None;
    let page = String::from_utf8_lossy(&bytes).into_owned();
    let mut parts = vec![if page.is_empty() {
        if finished {
            "No more output.".to_owned()
        } else {
            "No more output yet. Output and completion arrive automatically.".to_owned()
        }
    } else {
        format!("Output:\n{page}")
    }];
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
    parts.join("\n")
}

/// One source, as its reader sees it. Observations, not verdicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFacts {
    pub session_id: u32,
    pub kind: Kind,
    /// The cell that started it.
    pub cell: u64,
    pub output_since: Option<UnixMs>,
    /// A cell's oldest unsent `notify()`.
    pub notified_at: Option<UnixMs>,
    /// The model asked for more of a command's output, and it is ready.
    pub paged_at: Option<UnixMs>,
    /// A cell's code returned. Work it started may still run.
    pub returned: Option<UnixMs>,
    pub finished: Option<End>,
    /// Its end has been reported.
    pub delivered: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct End {
    pub at: UnixMs,
    /// A raise, a non-zero or missing exit code, a cancellation, or a host
    /// call that returned an error.
    pub failed: bool,
}

#[cfg(test)]
mod tests {
    use super::session_id;

    #[test]
    fn labels_are_distinct_within_a_cycle_and_in_range() {
        let labels = (0..9_000)
            .map(session_id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(labels.len(), 9_000);
        assert!(labels.iter().all(|label| (1_000..10_000).contains(label)));
    }
}
