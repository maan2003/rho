//! Wire vocabulary for workset-owned Comint-style shell sessions.
//!
//! The shells part of a host, [`crate::Part::Shell`]. A shell is started
//! by [`ShellStart`] and a stream attached to it by [`Open::Attach`]. The
//! workset owns the process and its canonical structured state; clients project
//! that state into a read-only buffer, keep their pending input locally, and
//! submit complete commands.

use senax_encoder::{Decode, Encode, Pack, Unpack};

pub use crate::shell_kernel::{
    MAX_ACTIVE_PAGERS, MAX_COMMAND_BYTES, MAX_PAGER_BYTES, MAX_PAGER_LINES, PagerAction,
    command_fits,
};

/// What a shells stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// Attaches to an agent's running shell ([`ShellStart`]). Answered with
    /// [`crate::Opened`], then [`ShellServerFrame`]s. Closing the stream
    /// only detaches; the shell keeps running.
    Attach { agent: String },
    /// One call, answered with one [`crate::Answer`]; then the stream
    /// closes.
    Request(Request),
}

crate::calls! {
    /// Every call the shells answer, as it goes on the wire.
    pub enum Request {
        ShellStart(ShellStart) -> ();
        ShellList(ShellList) -> Vec<ShellInfo>;
        ShellClose(ShellClose) -> ();
    }
}

impl crate::PartOpen for Open {
    const PART: crate::Part = crate::Part::Shell;

    fn debug_reply(&self, frame: &[u8]) -> Option<String> {
        match self {
            Self::Request(request) => Some(request.debug_answer(frame)),
            Self::Attach { .. } => None,
        }
    }
}

/// Starts the daemon-owned Comint-style shell for an agent. Attaching is
/// [`Open::Attach`].
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ShellStart {
    pub agent: String,
}

/// Running shells, of one agent if it names one.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ShellList {
    pub agent: Option<String>,
}

/// Stops an agent's running shell gracefully.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ShellClose {
    pub agent: String,
}

/// Maximum structured SGR runs retained for one output stream.
pub const MAX_STYLE_SPANS: usize = 4096;

/// A safe, structured color decoded from shell SGR output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum ShellColor {
    Indexed(u8),
    Rgb { red: u8, green: u8, blue: u8 },
}

/// Terminal attributes that apply to a byte range in sanitized shell output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct ShellTextStyle {
    pub foreground: Option<ShellColor>,
    pub background: Option<ShellColor>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikethrough: bool,
}

/// A styled byte range in one execution's output or in terminal output.
///
/// Span lists are capped at [`MAX_STYLE_SPANS`], sorted, non-overlapping, and
/// aligned to UTF-8 boundaries in the corresponding sanitized output string.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ShellStyleSpan {
    pub start: u64,
    pub end: u64,
    pub style: ShellTextStyle,
}

/// One workset-owned shell returned by [`ShellList`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ShellInfo {
    /// Encoded agent id ("eng-ht08").
    pub agent: String,
    /// Clients currently attached to the persistent kernel.
    pub clients: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ShellExecutionState {
    Queued,
    Running,
    Finished { status: i32 },
    Failed,
    Cancelled,
}

/// One daemon-authoritative command block retained by a shell session.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ShellExecution {
    pub execution: u64,
    pub command: String,
    pub prompt: String,
    pub cwd: String,
    pub state: ShellExecutionState,
    /// Sanitized merged stdout/stderr output.
    pub output: String,
    /// Non-default SGR styles over `output` byte ranges.
    pub styles: Vec<ShellStyleSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ShellPager {
    pub execution: u64,
    pub pager: u64,
    pub page: u64,
    pub lines: u32,
    pub bytes: u64,
}

/// Structured state retained by the workset independently of GUI rendering.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ShellState {
    pub prompt: String,
    pub cwd: String,
    pub executions: Vec<ShellExecution>,
    /// Pagers currently waiting for output credit from a client. Kept apart
    /// from transcript retention so an old execution can be trimmed safely.
    pub pagers: Vec<ShellPager>,
    /// Sanitized output not attributable to a submitted execution.
    pub terminal_output: String,
    /// Non-default SGR styles over `terminal_output` byte ranges.
    pub terminal_styles: Vec<ShellStyleSpan>,
}

/// Client to daemon frames after the shell handshake.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ShellClientFrame {
    /// Submit one complete command. Embedded newlines are preserved; the
    /// daemon supplies the final newline consumed by the shell.
    Submit { submission: u64, command: String },
    /// Interrupt descendants attached to the active execution PTY.
    Interrupt,
    /// Send the configured VEOF byte to the active execution PTY.
    Eof,
    /// Control a pager paused within its originating execution.
    PagerAction {
        execution: u64,
        pager: u64,
        page: u64,
        action: PagerAction,
    },
}

/// Daemon to client frames after the shell handshake.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ShellServerFrame {
    /// The daemon accepted a submitted command into the agent shell's bounded
    /// queue. Clients use this to resolve an immediately displayed local
    /// pending submission.
    Accepted {
        submission: u64,
        execution: u64,
    },
    /// The complete retained state, sent when a client attaches or
    /// resynchronizes.
    Snapshot {
        state: ShellState,
    },
    ExecutionQueued {
        execution: ShellExecution,
    },
    ExecutionStarted {
        execution: u64,
        prompt: String,
        cwd: String,
    },
    ExecutionOutput {
        execution: u64,
        start: u64,
        end: u64,
        text: String,
        /// Absolute styled byte ranges in the replacement tail.
        styles: Vec<ShellStyleSpan>,
    },
    PagerPaused {
        execution: u64,
        pager: ShellPager,
    },
    PagerResumed {
        execution: u64,
        pager: u64,
    },
    PagerFinished {
        execution: u64,
        pager: u64,
    },
    ExecutionFinished {
        execution: u64,
        status: i32,
    },
    ExecutionFailed {
        execution: Option<u64>,
    },
    TerminalOutput {
        start: u64,
        end: u64,
        text: String,
        /// Absolute styled byte ranges in the replacement tail.
        styles: Vec<ShellStyleSpan>,
    },
    /// Current prompt for the client-local writable draft. Prompt bytes are
    /// sanitized by the daemon before crossing this protocol.
    Prompt {
        prompt: String,
        cwd: String,
    },
    /// The shell process exited. The stream closes after this frame.
    Exited {
        status: Option<i32>,
    },
}
