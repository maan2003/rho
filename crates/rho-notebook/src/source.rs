//! What a cell starts that reports on its own: a host operation or a
//! managed command. Both are registered with their cell, announced under a
//! session ID while they run, and reported once when they end.
use std::sync::Mutex;

use pyo3::prelude::*;
use pyo3::types::PyDict;
use rho_core::UnixMs;
use rho_tool_shell::{BoundedOutput, decode_output_lossy};
use tokio::sync::{Notify, watch};

use crate::{JobEnd, JobFacts};

/// The session ID a source is reported under, so the pieces of one
/// background job correlate across replies. Scrambled so the model does not
/// read consecutive IDs as a count or confuse one with its neighbour. The
/// scheduler reads facts, never this, and the model refers to a job by its
/// Python handle.
///
/// Each modular shift is reversible by subtraction. Together they permute all
/// 9,000 slots without a lookup table; labels repeat every 9,000 internal
/// requests. Handles and output ordering always use the original internal ID.
pub(crate) fn session_id(internal_id: u64) -> u32 {
    let x = internal_id % 9_000;
    let (mut left, mut right) = (x / 100, x % 100);
    left = (left + right * right + 17 * right + 43) % 90;
    right = (right + left * left + 29 * left + 71) % 100;
    left = (left + right * right + 53 * right + 19) % 90;
    right = (right + left * left + 11 * left + 37) % 100;
    (1_000 + 100 * left + right) as u32
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
    /// The operation's name, or the command line.
    pub(crate) name: String,
    pub(crate) state: Mutex<SourceState>,
    /// A command's process; an operation has none.
    pub(crate) process: Option<Process>,
}

pub(crate) struct Process {
    pub(crate) budget: usize,
    pub(crate) stdin: tokio::sync::Mutex<Option<tokio::net::unix::pipe::Sender>>,
    pub(crate) cancel: Notify,
    pub(crate) ready: watch::Sender<bool>,
    /// Flipped when the command ends, so a handle recovered from a session
    /// ID can wait for it without holding the request that started it.
    pub(crate) done: watch::Sender<bool>,
}

pub(crate) struct SourceState {
    /// Output not yet reported. An operation's report arrives with its end;
    /// a command's output as it comes.
    pub(crate) unsent: BoundedOutput,
    pub(crate) registered_at: UnixMs,
    /// Told to the model as running, so its end can name the same ID.
    pub(crate) announced: bool,
    /// Oldest unsent command output.
    pub(crate) since: Option<UnixMs>,
    pub(crate) finished: Option<UnixMs>,
    /// An operation's error; a command's non-zero or missing exit code,
    /// spawn failure, or cancellation.
    pub(crate) failed: bool,
    /// Its end has been reported.
    pub(crate) delivered: bool,
    /// A command's retained output and exit.
    pub(crate) log: Option<Log>,
}

pub(crate) struct Log {
    pub(crate) file: std::fs::File,
    pub(crate) len: usize,
    pub(crate) dropped: usize,
    pub(crate) cursor: usize,
    pub(crate) exit: Option<Result<CommandExit, String>>,
}

impl Source {
    pub(crate) fn new(
        id: u64,
        name: String,
        budget: usize,
        process: Option<Process>,
        log: Option<Log>,
    ) -> Self {
        Self {
            id,
            name,
            state: Mutex::new(SourceState {
                unsent: BoundedOutput::for_tokens(Some(budget)),
                registered_at: UnixMs::now(),
                announced: false,
                since: None,
                finished: None,
                failed: false,
                delivered: false,
                log,
            }),
            process,
        }
    }

    /// A command's process.
    pub(crate) fn process(&self) -> &Process {
        self.process.as_ref().expect("a command has a process")
    }

    pub(crate) fn finished(&self) -> bool {
        self.state.lock().unwrap().finished.is_some()
    }

    fn budget(&self) -> usize {
        self.process
            .as_ref()
            .map_or(10000, |process| process.budget)
    }

    /// Whether the next report would say anything.
    pub(crate) fn has_news(&self) -> bool {
        let state = self.state.lock().unwrap();
        self.shows_output(&state) || state.finished.is_some() || !state.announced
    }

    fn shows_output(&self, state: &SourceState) -> bool {
        !state.unsent.is_empty() && (state.finished.is_some() || self.process.is_some())
    }

    /// Everything unsent, if anything: announced as running the first time
    /// a reply goes out while it is, under a session ID its later pieces
    /// name again; a source that ends before any reply is only ever reported
    /// finished. `old` names a command from a cell two or more turns back,
    /// so the model can place it without its own turn for context.
    pub(crate) fn report(&self, old: bool) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        let output = self.shows_output(&state);
        if !output && state.finished.is_none() && state.announced {
            return None;
        }
        let mut parts = Vec::new();
        if state.finished.is_some() {
            if state.announced {
                parts.push(format!("Session ID: {}", session_id(self.id)));
            }
            parts.push(match state.log.as_ref().map(|log| &log.exit) {
                None => format!("Operation {} completed", self.name),
                Some(Some(Ok(CommandExit {
                    exit_code: Some(exit_code),
                    ..
                }))) => format!("Process exited with code {exit_code}"),
                Some(Some(Ok(CommandExit {
                    exit_code: None, ..
                }))) => "Process ended without an exit code".to_owned(),
                Some(Some(Err(error))) => format!("Command failed: {error}"),
                Some(None) => unreachable!("a finished command has exited"),
            });
        } else {
            state.announced = true;
            parts.push(match self.process {
                None => format!(
                    "Operation {} running in background with session ID {}",
                    self.name,
                    session_id(self.id)
                ),
                Some(_) => format!(
                    "Command running in background with session ID {}",
                    session_id(self.id)
                ),
            });
        }
        if old && self.process.is_some() {
            parts.push(format!("Command: {}", self.name));
        }
        if output {
            // A reply and an explicit `more_output` share one cursor, so a
            // read after an automatic report carries on from where the
            // report stopped instead of repeating it. A report that dropped
            // its own middle showed only a sample, so it leaves the cursor
            // alone and `more_output` can still page the whole span.
            let complete = !state.unsent.is_truncated();
            let unsent = std::mem::replace(
                &mut state.unsent,
                BoundedOutput::for_tokens(Some(self.budget())),
            );
            if complete && let Some(log) = &mut state.log {
                log.cursor = log.len;
            }
            parts.push(format!(
                "Output:\n{}",
                decode_output_lossy(unsent.into_bytes())
            ));
        }
        state.since = None;
        if state.finished.is_some() {
            state.delivered = true;
        }
        Some(parts.join("\n"))
    }

    pub(crate) fn facts(&self, cell: u64) -> JobFacts {
        let state = self.state.lock().unwrap();
        JobFacts {
            cell,
            registered_at: state.registered_at,
            output_since: state.since,
            finished: state.finished.map(|at| JobEnd {
                at,
                failed: state.failed,
            }),
        }
    }
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
        assert_eq!(session_id(9_000), session_id(0));
    }
}
