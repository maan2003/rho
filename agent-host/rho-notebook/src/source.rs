//! A source: anything in the notebook that reports on its own. A cell, a
//! managed command, or a host call. All three live in one table, hold a
//! session ID from the start and report the same way. One that ends before
//! anyone hears of it speaks plainly, as a synchronous call would; one still
//! running when a report goes out is announced under its session ID, and
//! later pieces name that ID again.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

use rho_agent_types::UnixMs;
use rho_tool_shell::{BoundedOutput, decode_output_lossy};
use senax_encoder::{Decode, Encode};
use tokio::sync::{Notify, mpsc, watch};
use uuid::Uuid;

use crate::Image;
use crate::commands::StdinWrite;

/// A source's place in the notebook's one table: cells, tasks, commands
/// and calls share the space, in the order they started. Never shown to the
/// model; it sees the [`SessionId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SourceId(pub(crate) u64);

impl SourceId {
    /// The label this source is reported under. Scrambled so the model does
    /// not read consecutive IDs as a count; labels repeat every 9,000
    /// sources.
    pub(crate) fn session(self) -> SessionId {
        let x = self.0 % 9_000;
        let (mut left, mut right) = (x / 100, x % 100);
        left = (left + right * right + 17 * right + 43) % 90;
        right = (right + left * left + 29 * left + 71) % 100;
        left = (left + right * right + 53 * right + 19) % 90;
        right = (right + left * left + 11 * left + 37) % 100;
        SessionId((1_000 + 100 * left + right) as u32)
    }
}

/// The label a source is reported under, and what the model names it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(u32);

impl SessionId {
    pub fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
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
    pub(crate) id: SessionId,
    pub(crate) exit_code: Option<i32>,
}

pub(crate) struct Source {
    pub(crate) id: SourceId,
    pub(crate) identity: Uuid,
    pub(crate) kind: Kind,
    /// A command's line, a call's name; a cell's is "Cell".
    pub(crate) name: String,
    /// The cell that started it; a cell's own id for a cell. Cancelling
    /// that cell stops it.
    pub(crate) cell: SourceId,
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
    /// Whether it was started with a stdin pipe; otherwise it reads
    /// `/dev/null` and refuses writes.
    pub(crate) stdin: bool,
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
    /// A command whose exit code someone read: its failure wakes nobody.
    pub(crate) checked: bool,
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
        id: SourceId,
        kind: Kind,
        name: String,
        cell: SourceId,
        budget: usize,
        process: Option<Process>,
        log: Option<Log>,
    ) -> Self {
        Self {
            id,
            identity: Uuid::new_v4(),
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
                checked: false,
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

    fn label(&self) -> &str {
        match self.kind {
            Kind::Cell | Kind::Task => "Task",
            Kind::Command => "Command",
            Kind::Call => &self.name,
        }
    }

    /// Drain one structured update, including image-only updates.
    pub(crate) fn report(&self, old: bool) -> Option<SourceUpdate> {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        let mut header = None;
        let mut parts = Vec::new();
        if !state.pending_failure {
            if !state.pages.is_empty() {
                header = Some(self.answer_pages(state, old, &mut parts));
            } else if state.delivered {
                if !state.unsent.is_empty() {
                    state.notified = None;
                    state.since = None;
                    header = Some(Header::Session);
                    parts.push(Part::Output(take(state)));
                }
            } else {
                state.notified = None;
                let output = !state.unsent.is_empty()
                    && (self.kind != Kind::Call || state.finished.is_some());
                if state.finished.is_some() && !state.announced && !state.failed {
                    state.delivered = true;
                    header = Some(Header::Plain);
                    if output {
                        parts.push(Part::Text(take(state)));
                    }
                    if let (Kind::Call, Some(error)) = (self.kind, &state.error) {
                        parts.push(Part::Outcome(Outcome::CallFailed {
                            name: self.name.clone(),
                            error: error.clone(),
                        }));
                    }
                    if matches!(self.kind, Kind::Cell | Kind::Task) && parts.is_empty() {
                        parts.push(Part::Outcome(self.end(state)));
                    }
                    if parts.is_empty() {
                        header = None;
                    }
                } else if output || state.finished.is_some() || !state.announced {
                    if state.finished.is_some() {
                        header = Some(Header::Finished {
                            session: state.announced || self.kind != Kind::Cell,
                            old_name: old.then(|| self.old_name()).flatten(),
                            end: self.end(state),
                        });
                        if matches!(self.kind, Kind::Cell | Kind::Task)
                            && let Some(error) = &state.error
                        {
                            parts.push(Part::Text(error.clone()));
                        }
                    } else {
                        state.announced = true;
                        header = Some(Header::Running {
                            label: self.label().to_owned(),
                            old_name: old.then(|| self.old_name()).flatten(),
                        });
                    }
                    if output {
                        let complete = !state.unsent.is_truncated();
                        if complete && let Some(log) = &mut state.log {
                            log.cursor = log.len;
                        }
                        parts.push(Part::Output(take(state)));
                    }
                    state.since = None;
                    if state.finished.is_some() {
                        state.delivered = true;
                    }
                }
            }
        }
        let images = std::mem::take(&mut state.images);
        if header.is_none() && images.is_empty() {
            return None;
        }
        Some(SourceUpdate {
            identity: self.identity,
            session: self.id.session().get(),
            header,
            parts,
            images,
        })
    }

    fn old_name(&self) -> Option<String> {
        match self.kind {
            Kind::Command => Some(format!("Command: {}", self.name)),
            Kind::Task => Some(format!("Task: {}()", self.name)),
            _ => None,
        }
    }

    /// Snapshot how it ended; its wording belongs to report rendering.
    fn end(&self, state: &State) -> Outcome {
        match self.kind {
            Kind::Cell | Kind::Task if state.cell.cancelled => Outcome::TaskCancelled,
            Kind::Cell | Kind::Task if state.failed => Outcome::TaskFailed,
            Kind::Cell | Kind::Task => Outcome::TaskFinished,
            Kind::Call => match &state.error {
                Some(error) => Outcome::CallFailed {
                    name: self.name.clone(),
                    error: error.clone(),
                },
                None => Outcome::CallFinished(self.name.clone()),
            },
            Kind::Command => match state.log.as_ref().and_then(|log| log.exit.as_ref()) {
                Some(Ok(CommandExit {
                    exit_code: Some(code),
                    ..
                })) => Outcome::ProcessExited(*code),
                Some(Ok(CommandExit {
                    exit_code: None, ..
                })) => Outcome::ProcessMissingExit,
                Some(Err(error)) if error == "Command cancelled" => Outcome::ProcessMissingExit,
                Some(Err(error)) => Outcome::CommandFailed(error.clone()),
                None => Outcome::CommandEnded,
            },
        }
    }

    /// The pages `more_output` asked for, under the session ID the model
    /// asked by, with the end too if that has not been reported yet.
    fn answer_pages(&self, state: &mut State, old: bool, parts: &mut Vec<Part>) -> Header {
        let old_name = old.then(|| format!("Command: {}", self.name));
        state.paged_at = None;
        if state.finished.is_none() {
            state.announced = true;
        }
        let end = if state.finished.is_some() && !state.delivered {
            state.delivered = true;
            Some(self.end(state))
        } else {
            None
        };
        for tokens in std::mem::take(&mut state.pages) {
            parts.push(page(state, tokens));
        }
        Header::Page { old_name, end }
    }

    pub(crate) fn facts(&self) -> SourceFacts {
        let state = self.state.lock().unwrap();
        SourceFacts {
            session_id: self.id.session(),
            kind: self.kind,
            owner: self.cell.session(),
            output_since: state.since,
            notified_at: state.notified,
            paged_at: state.paged_at,
            returned: state.cell.returned,
            finished: state.finished.map(|at| End {
                at,
                failed: state.failed && !state.checked,
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
fn page(state: &mut State, max_tokens: usize) -> Part {
    let finished = state.finished.is_some();
    let Some(log) = state.log.as_mut() else {
        return Part::Text("[only a command can be paged]".to_owned());
    };
    if log.gone {
        return Part::Text(
            "[retained output is gone: notebook reached its 50 MB limit]".to_owned(),
        );
    }
    let start = log.cursor;
    let size = (log.len - start).min(max_tokens * 4);
    let mut bytes = vec![0; size];
    let read = log
        .file
        .seek(SeekFrom::Start(start as u64))
        .and_then(|_| log.file.read_exact(&mut bytes));
    if let Err(error) = read {
        return Part::Text(format!("[the retained output could not be read: {error}]"));
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
    Part::Page {
        output: String::from_utf8_lossy(&bytes).into_owned(),
        finished,
        remaining,
        dropped,
    }
}

/// One source's contribution to a notebook report. Its identity is independent
/// of the session label, which repeats across notebook lifetimes.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct SourceUpdate {
    pub(crate) identity: Uuid,
    pub(crate) session: u32,
    pub(crate) header: Option<Header>,
    pub(crate) parts: Vec<Part>,
    pub(crate) images: Vec<Image>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) enum Header {
    Plain,
    Running {
        label: String,
        old_name: Option<String>,
    },
    Finished {
        session: bool,
        old_name: Option<String>,
        end: Outcome,
    },
    Session,
    Page {
        old_name: Option<String>,
        end: Option<Outcome>,
    },
}

impl Header {
    pub(crate) fn render(&self, session_id: u32) -> String {
        let mut lines = Vec::new();
        match self {
            Self::Plain => {}
            Self::Running { label, old_name } => {
                lines.push(format!(
                    "{label} running in background with session ID {session_id}"
                ));
                lines.extend(old_name.iter().cloned());
            }
            Self::Finished {
                session,
                old_name,
                end,
            } => {
                if *session {
                    lines.push(format!("Session ID: {session_id}"));
                }
                lines.extend(old_name.iter().cloned());
                lines.push(end.render());
            }
            Self::Session => lines.push(format!("Session ID: {session_id}")),
            Self::Page { old_name, end } => {
                lines.push(format!("Session ID: {session_id}"));
                lines.extend(old_name.iter().cloned());
                lines.extend(end.iter().map(Outcome::render));
            }
        }
        lines.join("\n")
    }
}

/// One source's completion, retained as facts until presentation.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) enum Outcome {
    TaskFinished,
    TaskFailed,
    TaskCancelled,
    ProcessExited(i32),
    ProcessMissingExit,
    CallFinished(String),
    CallFailed { name: String, error: String },
    CommandFailed(String),
    CommandEnded,
}

impl Outcome {
    fn render(&self) -> String {
        match self {
            Self::TaskFinished => "Task finished".into(),
            Self::TaskFailed => "Task failed".into(),
            Self::TaskCancelled => "Task cancelled".into(),
            Self::ProcessExited(code) => format!("Process exited with code {code}"),
            Self::ProcessMissingExit => "Process ended without an exit code".into(),
            Self::CallFinished(name) => format!("{name} finished"),
            Self::CallFailed { name, error } => format!("{name} failed: {error}"),
            Self::CommandFailed(error) => format!("Command failed: {error}"),
            Self::CommandEnded => "Command ended".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) enum Part {
    Text(String),
    Outcome(Outcome),
    Output(String),
    Page {
        output: String,
        finished: bool,
        remaining: usize,
        dropped: usize,
    },
}

impl Part {
    pub(crate) fn render(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Outcome(outcome) => outcome.render(),
            Self::Output(text) => format!("Output:\n{text}"),
            Self::Page {
                output,
                finished,
                remaining,
                dropped,
            } => {
                let mut parts = vec![if output.is_empty() {
                    if *finished {
                        "No more output.".to_owned()
                    } else {
                        "No more output yet. Output and completion arrive automatically.".to_owned()
                    }
                } else {
                    format!("Output:\n{output}")
                }];
                if *remaining > 0 {
                    parts.push(format!(
                        "[{remaining} more bytes; call more_output() again for the next page]"
                    ));
                }
                if *dropped > 0 {
                    parts.push(format!(
                        "[{dropped} bytes never reached the log: the command outran its limit]"
                    ));
                }
                parts.join("\n")
            }
        }
    }
}

/// One source, as its reader sees it. Observations, not verdicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFacts {
    pub session_id: SessionId,
    pub kind: Kind,
    /// The task that started it; its own session for a task.
    pub owner: SessionId,
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
    /// call that returned an error. A command's is not, once its exit code
    /// is read.
    pub failed: bool,
}

#[cfg(test)]
mod tests {
    use rho_agent_types::UnixMs;

    use super::{Header, Kind, Outcome, Part, Source, SourceId};

    #[test]
    fn source_snapshots_completion_facts_instead_of_rendered_status() {
        let cell = Source::new(
            SourceId(1),
            Kind::Cell,
            "Cell".into(),
            SourceId(1),
            100,
            None,
            None,
        );
        cell.state.lock().unwrap().finished = Some(UnixMs(10));
        let plain = cell.report(false).unwrap();
        assert_eq!(plain.header, Some(Header::Plain));
        assert_eq!(plain.parts, vec![Part::Outcome(Outcome::TaskFinished)]);

        let call = Source::new(
            SourceId(2),
            Kind::Call,
            "fetch".into(),
            SourceId(1),
            100,
            None,
            None,
        );
        {
            let mut state = call.state.lock().unwrap();
            state.finished = Some(UnixMs(20));
            state.failed = true;
            state.error = Some("refused".into());
        }
        assert_eq!(
            call.report(false).unwrap().header,
            Some(Header::Finished {
                session: true,
                old_name: None,
                end: Outcome::CallFailed {
                    name: "fetch".into(),
                    error: "refused".into()
                },
            })
        );
    }

    #[test]
    fn labels_are_distinct_within_a_cycle_and_in_range() {
        let labels = (0..9_000)
            .map(|id| SourceId(id).session().get())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(labels.len(), 9_000);
        assert!(labels.iter().all(|label| (1_000..10_000).contains(label)));
    }
}
