//! Tools, and what they report about themselves.
//!
//! A tool is not a future that resolves once. It is a source that produces
//! output over its lifetime, possibly for hours. No tool declares itself a
//! background job, because at the moment of the call `npm test` and
//! `npm run dev` are the same call and nothing could tell them apart. So the
//! core does not classify them at all: every call is the model's business for
//! a fixed window after it was made, and background once that window is spent.
//! A background job is simply a call that outlived its window, and it costs
//! nothing from then on. The window runs from the call and from nothing else,
//! so no other source can shorten it.
//!
//! Because the core pulls, a tool holds its own output until asked. That is
//! the point — a tool asked for its output after five minutes can hand back
//! the relevant tail plus "1200 lines suppressed", which no buffer owned by
//! the core could have produced. The core never sees a byte it did not ask
//! for, and a chatty tool costs nothing between requests.
//!
//! The core says exactly one thing to a running tool — [`ToolSession::cancel`],
//! meaning *wind down*. Everything else flows the other way: the tool reports
//! its status, and the core weighs it against every other source.

use std::sync::Arc;

use rho_core::{
    ContextBlock, ToolCall, ToolOutput, ToolOutputStatus, ToolResult, ToolSpec, ToolUpdate, UnixMs,
};
use senax_encoder::{Decode, Encode};
use tokio::sync::Notify;

use crate::preview::{PreviewData, ToolPreview};
use crate::source::SourceKind;

/// Whether a tool is still working.
///
/// Ending is the one thing a tool cannot take back, which is why the core is
/// willing to act on it: everything else it reports is a guess about what
/// happens next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ToolActivity {
    Running,
    /// Finished; nothing more will arrive. `at` is when it ended, which is the
    /// moment whatever it was holding became worth sending on its own — not
    /// when that output first started arriving.
    Exited {
        at: UnixMs,
    },
}

/// Whether a tool is holding output the model has not seen, and whether that
/// output can stand on its own yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Unsent {
    Nothing,
    /// Holding output mid-thought. Worth sending eventually — half a build log
    /// beats none — but worth less than the whole, so it is the most patient
    /// thing the core knows about.
    Waiting {
        since: UnixMs,
    },
    /// Holding output that stands on its own: this much matters now, whatever
    /// else the tool goes on to say. The one thing a running tool can say about
    /// its own urgency, and it buys exactly one thing: it stops waiting for the
    /// rest of the call. It still cannot interrupt a request in flight.
    ///
    /// `since` is when it became worth sending, not when it started arriving.
    Settled {
        since: UnixMs,
    },
}

/// What a tool reports about itself, for the core to schedule against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolStatus {
    pub unsent: Unsent,
    pub activity: ToolActivity,
    /// For previews only. The decision measures nothing from it, because a wait
    /// that moved every time a tool spoke would be a wait a chatty tool could
    /// extend forever.
    pub last_output_at: UnixMs,
}

/// Tell the core that something changed.
///
/// Deliberately carries no payload: what changed is discovered by asking, at a
/// moment the core picks. That is what lets several sources collapse into one
/// request instead of each waking one.
#[derive(Clone, Debug)]
pub struct SourceWaker(Arc<Notify>);

impl SourceWaker {
    pub(crate) fn new(notify: Arc<Notify>) -> Self {
        Self(notify)
    }

    /// Signal new output, or an exit. Cheap, and safe to call as often as you
    /// like — the core coalesces.
    pub fn wake(&self) {
        // `notify_one` stores a permit, so a wake that lands while the core is
        // busy is not lost.
        self.0.notify_one();
    }
}

/// One running invocation of a tool.
pub trait ToolSession: Send {
    fn status(&self) -> ToolStatus;

    /// Everything the model has not seen, in whatever shape the tool judges
    /// best — a tail, a summary, a diff, an exit status. Called only at a
    /// request boundary, so a long-lived tool decides how to represent
    /// minutes of activity in one block.
    ///
    /// Returning `None` means "nothing new".
    fn take_output(&mut self) -> Option<ToolOutput>;

    /// Wind down: stop the work and finish soon.
    ///
    /// The tool still gets to produce its last words; the core keeps it around
    /// until it reports [`ToolActivity::Exited`] and its output has been
    /// collected.
    fn cancel(&mut self);

    /// What a UI should show while this tool runs. Tools with something richer
    /// to display define their own [`PreviewData`] type.
    ///
    /// The call is passed in so the preview can name itself; previews travel as
    /// a flat list, and one that cannot say which call it belongs to is of no
    /// use to a UI.
    fn preview(&self, call: &ToolCall) -> Box<dyn PreviewData> {
        let status = self.status();
        Box::new(ToolPreview {
            call_id: call.id.clone(),
            activity: status.activity,
            unsent: status.unsent,
            last_output_at: status.last_output_at,
        })
    }
}

pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> ToolSpec;

    /// Start the work. Call [`SourceWaker::wake`] whenever the status
    /// changes; the core will come and ask.
    fn run(&self, call: ToolCall, waker: SourceWaker) -> Box<dyn ToolSession>;
}

/// A session that is already over. Used for calls that fail before any work
/// starts, so the failure reaches the model through the ordinary path.
pub(crate) struct FinishedSession {
    output: Option<ToolOutput>,
    at: UnixMs,
}

impl FinishedSession {
    pub fn boxed(output: ToolOutput, at: UnixMs) -> Box<dyn ToolSession> {
        Box::new(Self {
            output: Some(output),
            at,
        })
    }
}

impl ToolSession for FinishedSession {
    fn status(&self) -> ToolStatus {
        ToolStatus {
            last_output_at: self.at,
            unsent: match self.output {
                Some(_) => Unsent::Waiting { since: self.at },
                None => Unsent::Nothing,
            },
            activity: ToolActivity::Exited { at: self.at },
        }
    }

    fn take_output(&mut self) -> Option<ToolOutput> {
        self.output.take()
    }

    fn cancel(&mut self) {}
}

/// What the model has been told about one call. The milestones are ordered and
/// each happens once, which is why they are a state rather than two flags: a
/// call cannot end before it is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Told {
    /// Nothing yet. The next thing this call produces is its one
    /// [`ToolResult`].
    Nothing,
    /// It has its result, so everything after arrives as a [`ToolUpdate`].
    Result,
    /// ...and that it ended. Nothing more will ever be said about it.
    Exit,
}

/// The core's bookkeeping for one call: which tool, how much of its story the
/// model has, and since when it has been holding something. The output itself
/// lives in the session, which is asked for it at every boundary.
pub(crate) struct RunningTool {
    pub call: ToolCall,
    pub started_at: UnixMs,
    pub session: Box<dyn ToolSession>,
    pub told: Told,
}

impl RunningTool {
    pub fn new(call: ToolCall, session: Box<dyn ToolSession>, now: UnixMs) -> Self {
        Self {
            call,
            started_at: now,
            session,
            told: Told::Nothing,
        }
    }

    pub fn status(&self) -> ToolStatus {
        self.session.status()
    }

    /// Done with, and safe to forget.
    pub fn reapable(&self) -> bool {
        self.told == Told::Exit
    }

    /// What it is, with nothing decided about it. Every one of these is
    /// something the tool observed; what any of them is worth is `boundary`'s
    /// business, and so is every duration.
    pub fn source(&self) -> SourceKind {
        let status = self.status();
        SourceKind::Tool {
            id: self.call.id.clone(),
            told: self.told,
            activity: status.activity,
            unsent: status.unsent,
        }
    }

    pub fn preview(&self) -> Box<dyn PreviewData> {
        self.session.preview(&self.call)
    }

    pub fn cancel(&mut self) {
        self.session.cancel();
    }

    /// Hand over everything unsent. The call's first contribution becomes its
    /// [`ToolResult`]; every later one becomes a [`ToolUpdate`], because a
    /// provider accepts exactly one result per call id.
    pub fn take(&mut self, now: UnixMs) -> Option<ToolTake> {
        let exited = matches!(self.status().activity, ToolActivity::Exited { .. });
        let output = self.session.take_output();
        let take = match self.told {
            Told::Exit => return None,
            Told::Nothing => {
                if output.is_none() && !exited {
                    return None;
                }
                // A tool that exits silently still owes the provider a result.
                let body = output.unwrap_or(ToolOutput {
                    output: Arc::new(String::new()),
                    status: ToolOutputStatus::Success,
                });
                // A result carries `finished_at`, so answering a call that has
                // already ended says both things at once.
                self.told = if exited { Told::Exit } else { Told::Result };
                ToolTake::Result(ToolResult {
                    call_id: self.call.id.clone(),
                    tool_type: self.call.tool_type,
                    body,
                    started_at: self.started_at,
                    finished_at: now,
                    metadata: None,
                })
            }
            Told::Result if !exited => ToolTake::Update(ToolUpdate {
                call_id: self.call.id.clone(),
                tool_type: self.call.tool_type,
                output: output?.output,
                at: now,
            }),
            // Answered long ago and now over. Nothing else will report the
            // ending, so this is the only chance to say it — a background job
            // that dies quietly would otherwise just stop existing.
            Told::Result => {
                self.told = Told::Exit;
                let ended = "[the tool has exited]";
                ToolTake::Update(ToolUpdate {
                    call_id: self.call.id.clone(),
                    tool_type: self.call.tool_type,
                    output: Arc::new(match output {
                        Some(output) => format!("{}\n{ended}", output.output),
                        None => ended.to_owned(),
                    }),
                    at: now,
                })
            }
        };
        Some(take)
    }
}

/// The two shapes a drained tool can take in history.
pub(crate) enum ToolTake {
    Result(ToolResult),
    Update(ToolUpdate),
}

/// Note appended for a tool that was still running when the process stopped.
pub(crate) fn lost_to_restart(call: &ToolCall, result_sent: bool, now: UnixMs) -> ContextBlock {
    let text = "This tool was still running when the agent restarted. Its output is lost; \
                re-run it if you still need the result."
        .to_owned();
    if result_sent {
        ContextBlock::ToolUpdate(ToolUpdate {
            call_id: call.id.clone(),
            tool_type: call.tool_type,
            output: Arc::new(text),
            at: now,
        })
    } else {
        ContextBlock::ToolResults {
            results: vec![ToolResult {
                call_id: call.id.clone(),
                tool_type: call.tool_type,
                body: ToolOutput {
                    output: Arc::new(text),
                    status: ToolOutputStatus::Cancelled,
                },
                started_at: now,
                finished_at: now,
                metadata: None,
            }],
        }
    }
}

/// Keep the head and tail of oversized output, noting what was dropped.
///
/// Offered to tools rather than applied by the core: a tool that knows its own
/// output can nearly always choose better than "keep both ends". This is the
/// fallback for ones that cannot.
pub fn elide_middle(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let half = budget / 2;
    let head = floor_boundary(text, half);
    let tail = ceil_boundary(text, text.len() - half);
    format!(
        "{}\n... {} bytes elided ...\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}
