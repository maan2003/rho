//! Wire vocabulary for daemon-owned Comint-style shell sessions.
//!
//! A shell stream is dedicated by [`crate::ClientMessage::ShellStart`] or
//! [`crate::ClientMessage::ShellAttach`]. The daemon owns the process and its
//! canonical structured state; clients project that state into a read-only
//! buffer, keep their pending input locally, and submit complete commands.

pub use rho_shell_proto::{MAX_COMMAND_BYTES, command_fits};
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// One daemon-owned shell returned by [`crate::ServerMessage::ShellList`].
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
}

/// Structured state retained by the daemon independently of GUI rendering.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ShellState {
    pub prompt: String,
    pub cwd: String,
    pub executions: Vec<ShellExecution>,
    /// Sanitized output not attributable to a submitted execution.
    pub terminal_output: String,
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
