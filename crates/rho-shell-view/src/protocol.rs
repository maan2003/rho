//! Wire vocabulary for workset-owned Comint-style shell sessions.
//!
//! The shell protocol of a host, [`rho_rpc::protocol::Protocol::Shell`]. A
//! shell is started by [`ShellStart`] and a stream attached to it by
//! [`Open::Attach`]. The workset owns the process and its canonical
//! structured state; clients project that state into a read-only buffer,
//! keep their pending input locally, and submit complete commands.

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Most pagers one shell holds paused at once.
pub const MAX_ACTIVE_PAGERS: usize = 64;
/// Most lines one pager page shows.
pub const MAX_PAGER_LINES: u32 = 1_000;
/// Most bytes one pager page shows.
pub const MAX_PAGER_BYTES: u64 = 64 * 1024;
/// Longest command a client may submit.
pub const MAX_COMMAND_BYTES: usize = 1024 * 1024;

pub fn command_fits(command: &str) -> bool {
    command.len() <= MAX_COMMAND_BYTES
}

/// What a client asks of a paused pager.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum PagerAction {
    /// Show the next page.
    Continue,
    /// Show the rest without pausing.
    Drain,
    /// Stop paging.
    Quit,
}

/// What a shells stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// Attaches to an agent's running shell ([`ShellStart`]). Answered with
    /// [`rho_rpc::protocol::Opened`], then [`ShellServerFrame`]s. Closing
    /// the stream only detaches; the shell keeps running.
    Attach { agent: String },
    /// One call, answered with one [`rho_rpc::protocol::Answer`]; then the
    /// stream closes.
    Request(Request),
}

rho_rpc::calls! {
    /// Every call the shells answer, as it goes on the wire.
    pub enum Request {
        ShellStart(ShellStart) -> ();
        ShellList(ShellList) -> Vec<ShellInfo>;
        ShellClose(ShellClose) -> ();
    }
}

impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Shell;

    fn debug_reply(&self, frame: &[u8]) -> Option<String> {
        match self {
            Self::Request(request) => Some(request.debug_answer(frame)),
            Self::Attach { .. } => None,
        }
    }
}

/// Starts the host-owned Comint-style shell for an agent. Attaching is
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

/// One host-authoritative command block retained by a shell session.
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

/// Client to agent host frames after the shell handshake.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ShellClientFrame {
    /// Submit one complete command. Embedded newlines are preserved; the
    /// agent host supplies the final newline consumed by the shell.
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

/// Agent host to client frames after the shell handshake.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ShellServerFrame {
    /// The agent host accepted a submitted command into the agent shell's
    /// bounded queue. Clients use this to resolve an immediately displayed
    /// local pending submission.
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
    /// sanitized by the agent host before crossing this protocol.
    Prompt {
        prompt: String,
        cwd: String,
    },
    /// The shell process exited. The stream closes after this frame.
    Exited {
        status: Option<i32>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trips<T>(value: T)
    where
        T: senax_encoder::Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
    {
        let bytes = senax_encoder::pack(&value).unwrap();
        assert_eq!(
            senax_encoder::unpack::<T>(&mut bytes.clone()).unwrap(),
            value
        );
    }

    #[test]
    fn frames_round_trip() {
        round_trips(ShellServerFrame::ExecutionOutput {
            execution: 3,
            start: 0,
            end: 0,
            text: "λ".to_owned(),
            styles: vec![ShellStyleSpan {
                start: 0,
                end: 2,
                style: ShellTextStyle {
                    foreground: Some(ShellColor::Indexed(1)),
                    bold: true,
                    ..Default::default()
                },
            }],
        });
        round_trips(ShellClientFrame::PagerAction {
            execution: 1,
            pager: 2,
            page: 3,
            action: PagerAction::Drain,
        });
    }

    #[test]
    fn opening_survives_the_envelope() {
        let open = Open::Request(
            ShellStart {
                agent: "eng-test".to_owned(),
            }
            .into(),
        );
        let envelope = rho_rpc::protocol::Open::of(&open).unwrap();
        assert_eq!(envelope.unpack::<Open>().unwrap(), open);
    }

    #[test]
    fn commands_fit_up_to_the_limit() {
        assert!(command_fits(&"x".repeat(MAX_COMMAND_BYTES)));
        assert!(!command_fits(&"x".repeat(MAX_COMMAND_BYTES + 1)));
    }
}
