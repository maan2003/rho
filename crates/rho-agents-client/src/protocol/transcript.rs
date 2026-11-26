//! The vocabulary of a transcript: what a workset row
//! ([`super::worksets::LogEvent`]) carries of a response and its calls, and the
//! live tail a runtime has past the log.

pub use rho_agent_types::RuntimeKind;
use rho_agent_types::UnixMs;
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Who said a mirrored Claude message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Speaker {
    User,
    Agent,
    Assistant,
}

/// What one model response cost, as the provider reported it. The model
/// is named so a client can price it; the tables the agent host keeps hold
/// the same numbers keyed by time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Usage {
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
}

/// What a runtime has that the log does not yet, told as it changes:
/// the response in flight and the phase. Everything else a reader wants
/// is a row: the queue is `Message` rows no `Sent` has carried, a call
/// runs until a `Sent` answers it, a turn ends with a `Turn` row.
///
/// Each snapshot replaces the runtime state, streaming response and tentative
/// draft. Durable transcript rows remain the source of committed history.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Live {
    Snapshot {
        state: RuntimeState,
        response: Option<StreamingResponse>,
        /// Host-computed draft retained while the originating cell runs.
        #[senax(default)]
        draft: Option<Draft>,
    },
}

/// A message an agent is still writing, as far as its code shows it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Draft {
    /// The thread its post names, when the code names it as a number.
    pub thread: Option<rho_agent_types::MessageId>,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct RuntimeState {
    pub inference: InferenceState,
    pub running_tasks: u32,
    pub awaiting_human: bool,
    pub checkin_at: Option<UnixMs>,
    pub archived: bool,
    /// The agent runs in a workset process older than its agent host's
    /// build, so fixes since reach it only when the workset restarts.
    #[senax(default)]
    pub stale: bool,
    /// The stopped agent has not submitted a request, so changing runtime
    /// kind will not split its provider-owned conversation.
    #[senax(default)]
    pub may_change_runtime_kind: bool,
}

impl RuntimeState {
    pub fn is_working(&self) -> bool {
        matches!(
            self.inference,
            InferenceState::Responding | InferenceState::Retrying { .. }
        ) || (!self.archived && self.running_tasks > 0)
    }

    /// What it is doing, in the word every screen says: the host stamps
    /// when it started on this, and a client says it.
    pub fn activity(&self) -> rho_agent_types::Activity {
        use rho_agent_types::Activity;
        match &self.inference {
            InferenceState::Lost { .. } => Activity::Lost,
            InferenceState::Failed { .. } => Activity::Failed,
            InferenceState::Retrying { .. } => Activity::Retrying,
            _ if self.awaiting_human => Activity::Waiting,
            _ if self.is_working() => Activity::Working,
            _ if self.archived => Activity::Archived,
            // An agent between turns waits for its next message, whether
            // or not it asked for one.
            _ => Activity::Waiting,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum InferenceState {
    #[default]
    Idle,
    Responding,
    Retrying {
        at: UnixMs,
        error: String,
        /// When the first of these failures came: the runtime keeps one
        /// streak across its attempts, including when an attempt is in flight.
        since: UnixMs,
    },
    Failed {
        error: String,
        /// When it failed. A failure the user retried into another can reach
        /// the host as one status, so the time is the runtime's to say; none
        /// from a runtime built before it said one.
        since: Option<UnixMs>,
    },
    /// The process the agent ran in ended without a word: its host lost
    /// it. Only the host says this, never the runtime itself.
    Lost {
        error: String,
        /// When its host lost it.
        since: Option<UnixMs>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct StreamingResponse {
    pub id: String,
    pub items: Vec<Item>,
}

/// One response item, shared by the live tail and committed mirror.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Item {
    Text {
        text: String,
        phase: Option<TextPhase>,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        /// How to read `arguments`.
        format: ArgumentsFormat,
    },
}

/// What a call's `arguments` string holds. A function tool is given a JSON
/// object, which is only whole once the call is; a custom tool is given the
/// text the model wrote, which is never JSON and must not be parsed as it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ArgumentsFormat {
    Json,
    Text,
}

/// Whether a text item is the model thinking aloud or its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TextPhase {
    Commentary,
    FinalAnswer,
}

#[cfg(test)]
mod tests {
    use rho_agent_types::UnixMs;

    use super::*;

    /// Every state a runtime reports reads as one of the product's words;
    /// an agent between turns waits, and nothing says `idle`.
    #[test]
    fn every_runtime_state_reads_as_a_product_state_and_never_idle() {
        let between_turns = RuntimeState::default();
        let cases = [
            (between_turns.clone(), "waiting"),
            (
                RuntimeState {
                    awaiting_human: true,
                    ..between_turns.clone()
                },
                "waiting",
            ),
            (
                RuntimeState {
                    inference: InferenceState::Responding,
                    ..between_turns.clone()
                },
                "working",
            ),
            (
                RuntimeState {
                    running_tasks: 1,
                    ..between_turns.clone()
                },
                "working",
            ),
            (
                RuntimeState {
                    inference: InferenceState::Retrying {
                        at: UnixMs(2),
                        error: "overloaded".into(),
                        since: UnixMs(1),
                    },
                    ..between_turns.clone()
                },
                "retrying",
            ),
            (
                RuntimeState {
                    inference: InferenceState::Failed {
                        error: "bad request".into(),
                        since: None,
                    },
                    ..between_turns.clone()
                },
                "failed",
            ),
            (
                RuntimeState {
                    archived: true,
                    // A stopped agent's tasks no longer count as work.
                    running_tasks: 1,
                    ..between_turns.clone()
                },
                "archived",
            ),
        ];
        for (state, word) in cases {
            assert_eq!(state.activity().word(), word, "{state:?}");
        }
    }
}
