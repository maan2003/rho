//! Tools, and the rhythm at which their output reaches the model.
//!
//! A tool is not a future that resolves once. It is a source that produces
//! output over its lifetime, possibly for hours. There is no such thing as a
//! "background" tool: the core only decides *when* to next talk to the model,
//! and pulls from every source at that moment.
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
use std::time::Duration;

use rho_core::{
    ContextBlock, ToolCall, ToolOutput, ToolOutputStatus, ToolResult, ToolSpec, ToolUpdate, UnixMs,
};
use tokio::sync::Notify;

use crate::source::{PreviewData, ToolPreview};

/// How eagerly a source's pending output should reach the model.
///
/// Both bounds are hints from the source, read by the one place that can see
/// every source at once. Nothing acts on its own rhythm alone.
#[derive(Clone, Copy, Debug)]
pub struct Rhythm {
    /// After this long with no new output, the source counts as *settled*:
    /// whatever it holds is probably all it has to say, so the core may stop
    /// waiting for more.
    pub quiet_after: Duration,
    /// Upper bound on sitting on pending output, measured from when the model
    /// last spoke. The most impatient pending source sets the deadline for
    /// everyone, and they all yield together.
    pub max_hold: Duration,
}

impl Rhythm {
    /// Typed input is complete the moment it arrives, so it is always settled;
    /// only `max_hold` matters, and it bounds how long a message can wait
    /// behind tools that are still chattering.
    pub const USER: Self = Self {
        quiet_after: Duration::ZERO,
        max_hold: Duration::from_secs(2),
    };

    /// Peers often send several lines at once; wait a beat so they collapse
    /// into one request instead of waking one per line.
    pub const MAIL: Self = Self {
        quiet_after: Duration::from_secs(1),
        max_hold: Duration::from_secs(10),
    };

    pub const TOOL: Self = Self {
        quiet_after: Duration::from_millis(250),
        max_hold: Duration::from_secs(10),
    };
}

/// What a tool reports about itself, for the core to schedule against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolStatus {
    /// When the tool last produced anything, for [`Rhythm::quiet_after`].
    pub last_output_at: UnixMs,
    /// Output is waiting that the model has not seen.
    pub pending: bool,
    /// The tool has finished; nothing more will arrive. Certain, where quiet
    /// is only a guess, so the core never waits on an exited tool.
    pub exited: bool,
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
    /// until [`ToolStatus::exited`] and its output has been collected.
    fn cancel(&mut self);

    /// What a UI should show while this tool runs. Tools with something richer
    /// to display define their own [`PreviewData`] type.
    fn preview(&self) -> Box<dyn PreviewData> {
        let status = self.status();
        Box::new(ToolPreview {
            exited: status.exited,
            pending: status.pending,
            last_output_at: status.last_output_at,
        })
    }
}

pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> ToolSpec;

    /// How eagerly this tool's output should reach the model. Read by the
    /// core, which is the only thing that can see every source at once.
    fn rhythm(&self) -> Rhythm {
        Rhythm::TOOL
    }

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
            pending: self.output.is_some(),
            exited: true,
        }
    }

    fn take_output(&mut self) -> Option<ToolOutput> {
        self.output.take()
    }

    fn cancel(&mut self) {}
}

/// The core's bookkeeping for one call: which tool, how impatient it is, and
/// whether its single permitted result has been written. The output itself
/// lives in the session.
pub(crate) struct RunningTool {
    pub call: ToolCall,
    pub rhythm: Rhythm,
    pub started_at: UnixMs,
    pub session: Box<dyn ToolSession>,
    /// Whether this call's single permitted [`ToolResult`] has been written.
    pub answered: bool,
}

impl RunningTool {
    pub fn new(call: ToolCall, rhythm: Rhythm, session: Box<dyn ToolSession>, now: UnixMs) -> Self {
        Self {
            call,
            rhythm,
            started_at: now,
            session,
            answered: false,
        }
    }

    pub fn status(&self) -> ToolStatus {
        self.session.status()
    }

    /// Whether this tool has anything the model has not seen. An exited tool
    /// still counts until its call has been answered, even with no output.
    pub fn pending(&self) -> bool {
        let status = self.status();
        status.pending || (status.exited && !self.answered)
    }

    /// Done with, and safe to forget: it exited and the model has seen
    /// everything it produced.
    pub fn reapable(&self) -> bool {
        self.status().exited && !self.pending()
    }

    pub fn cancel(&mut self) {
        self.session.cancel();
    }

    /// Hand over everything unsent. The call's first contribution becomes its
    /// [`ToolResult`]; every later one becomes a [`ToolUpdate`], because a
    /// provider accepts exactly one result per call id.
    pub fn take(&mut self, now: UnixMs) -> Option<ToolTake> {
        let exited = self.status().exited;
        let output = self.session.take_output();
        if self.answered {
            return output.map(|output| {
                ToolTake::Update(ToolUpdate {
                    call_id: self.call.id.clone(),
                    tool_type: self.call.tool_type,
                    output: output.output,
                    at: now,
                })
            });
        }
        if output.is_none() && !exited {
            return None;
        }
        self.answered = true;
        // A tool that exits silently still owes the provider a result.
        let body = output.unwrap_or(ToolOutput {
            output: Arc::new(String::new()),
            status: ToolOutputStatus::Success,
        });
        Some(ToolTake::Result(ToolResult {
            call_id: self.call.id.clone(),
            tool_type: self.call.tool_type,
            body,
            started_at: self.started_at,
            finished_at: now,
            metadata: None,
        }))
    }
}

/// The two shapes a drained tool can take in history.
pub(crate) enum ToolTake {
    Result(ToolResult),
    Update(ToolUpdate),
}

/// Note appended for a tool that was still running when the process stopped.
pub(crate) fn lost_to_restart(call: &ToolCall, answered: bool, now: UnixMs) -> ContextBlock {
    let text = "This tool was still running when the agent restarted. Its output is lost; \
                re-run it if you still need the result."
        .to_owned();
    if answered {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: rho_core::ToolCallId::try_from("call-1").unwrap(),
            name: rho_core::ToolName::try_from("shell").unwrap(),
            tool_type: rho_core::ToolType::Function,
            arguments: "{}".to_owned(),
        }
    }

    /// Hands back whatever it has been given, one pull at a time.
    struct Chatty {
        chunks: std::collections::VecDeque<String>,
        exited: bool,
    }

    impl ToolSession for Chatty {
        fn status(&self) -> ToolStatus {
            ToolStatus {
                last_output_at: UnixMs(1_000),
                pending: !self.chunks.is_empty(),
                exited: self.exited,
            }
        }

        fn take_output(&mut self) -> Option<ToolOutput> {
            // The tool decides how to represent everything unsent — here, by
            // concatenating; a log tailer would return only the tail.
            let text: String = std::mem::take(&mut self.chunks).into_iter().collect();
            (!text.is_empty()).then(|| ToolOutput {
                output: Arc::new(text),
                status: ToolOutputStatus::Success,
            })
        }

        fn cancel(&mut self) {
            self.exited = true;
        }
    }

    fn chatty(chunks: &[&str]) -> Box<dyn ToolSession> {
        Box::new(Chatty {
            chunks: chunks.iter().map(|text| text.to_string()).collect(),
            exited: false,
        })
    }

    #[test]
    fn first_take_answers_the_call_and_later_takes_annotate_it() {
        let now = UnixMs(1_000);
        let mut tool = RunningTool::new(call(), Rhythm::TOOL, chatty(&["one"]), now);

        let Some(ToolTake::Result(result)) = tool.take(now) else {
            panic!("first take must answer the call")
        };
        assert_eq!(*result.body.output, "one");

        // The same tool produces more later; a provider accepts only one
        // result per call, so this has to arrive as an update.
        tool.session = chatty(&["two"]);
        let Some(ToolTake::Update(update)) = tool.take(now) else {
            panic!("later takes must annotate")
        };
        assert_eq!(*update.output, "two");
    }

    #[test]
    fn several_chunks_collapse_into_one_block() {
        // The core pulls once and gets everything, however much accumulated.
        let now = UnixMs(1_000);
        let mut tool = RunningTool::new(call(), Rhythm::TOOL, chatty(&["a", "b", "c"]), now);

        let Some(ToolTake::Result(result)) = tool.take(now) else {
            panic!("expected a result")
        };
        assert_eq!(*result.body.output, "abc");
        assert!(!tool.pending(), "nothing left over");
    }

    #[test]
    fn a_tool_that_exits_silently_still_owes_a_result() {
        let now = UnixMs(1_000);
        let mut tool = RunningTool::new(
            call(),
            Rhythm::TOOL,
            Box::new(Chatty {
                chunks: Default::default(),
                exited: true,
            }),
            now,
        );

        assert!(tool.pending(), "the call is unanswered");
        assert!(!tool.reapable());

        assert!(matches!(tool.take(now), Some(ToolTake::Result(_))));
        assert!(tool.reapable(), "answered and exited, safe to forget");
    }

    #[test]
    fn a_running_tool_with_nothing_to_say_contributes_nothing() {
        let now = UnixMs(1_000);
        let mut tool = RunningTool::new(call(), Rhythm::TOOL, chatty(&[]), now);
        assert!(!tool.pending());
        assert!(tool.take(now).is_none());
        assert!(!tool.answered, "and its call stays open");
    }

    #[test]
    fn elide_middle_keeps_both_ends() {
        let text = "x".repeat(1_000);
        let elided = elide_middle(&text, 100);
        assert!(elided.len() < 200);
        assert!(elided.contains("bytes elided"));
        assert_eq!(elide_middle("short", 100), "short");
    }
}
