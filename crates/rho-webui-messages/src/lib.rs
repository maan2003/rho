//! JSON vocabulary between the rho daemon and the web UI client.
//!
//! Deliberately smaller than the full UI protocol: string agent ids, coarse
//! status/attention labels, and bounded tool output. The daemon owns the
//! projection from UI protocol state; the web client only renders and sends
//! commands. Messages travel as newline-delimited JSON over an iroh
//! bi-stream ([`ALPN`]); `serde_json` never emits raw newlines, so one line
//! is one message.
//!
//! This crate must stay free of native-only dependencies: the web client
//! compiles it to wasm.

use serde::{Deserialize, Serialize};

/// ALPN of the web UI JSON session on the daemon's iroh endpoint.
pub const ALPN: &[u8] = b"rho/webui-json/1";

/// Longest accepted line on either side, in bytes.
pub const MAX_LINE_LEN: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToBrowser {
    Hello {
        topics: Vec<Topic>,
        workdirs: Vec<Workdir>,
    },
    Agent {
        agent_id: String,
        state: AgentState,
    },
    AgentCreated {
        agent_id: String,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromBrowser {
    /// Focus an agent: the daemon loads it and starts forwarding its state.
    Select {
        agent_id: String,
    },
    Send {
        agent_id: String,
        text: String,
    },
    NewAgent {
        topic_id: String,
        repo: String,
        role: String,
        /// Work directly in the registered checkout instead of creating an
        /// isolated workspace.
        join: bool,
        revset: String,
        text: String,
    },
    Cancel {
        agent_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Topic {
    pub id: String,
    pub name: String,
    pub pinned: bool,
    pub agents: Vec<AgentSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub role: String,
    pub pinned: bool,
    pub updated_at: u64,
    pub attention: String,
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workdir {
    pub path: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub status: String,
    pub context_used: Option<u64>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    User {
        text: String,
    },
    Assistant {
        text: String,
        final_answer: bool,
    },
    Tool {
        /// One-line label already rendered by the daemon (`$ command`,
        /// `read path`, …), mirroring the GUI transcript.
        label: String,
        status: String,
        /// Wall time of a finished tool; the client formats and sums these.
        duration_ms: Option<u64>,
        output: Option<String>,
        error: Option<String>,
    },
    Notice {
        text: String,
    },
    Queued {
        text: String,
    },
    AgentMessage {
        sender: String,
        text: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip() {
        let text = r#"{"type":"send","agent_id":"abc","text":"hi"}"#;
        let message: FromBrowser = serde_json::from_str(text).unwrap();
        assert!(matches!(message, FromBrowser::Send { .. }));
        let encoded = serde_json::to_string(&message).unwrap();
        assert!(!encoded.contains('\n'));
    }
}
