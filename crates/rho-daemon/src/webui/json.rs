//! JSON vocabulary between the daemon and the browser page.
//!
//! Deliberately smaller than the UI protocol: string agent ids, coarse
//! status/attention labels, and bounded tool output so a long-running agent
//! cannot flood the browser.

use rho_ui_proto::remote::{UiAgentState, UiAgentStatus, UiBlock, UiMessagePhase, UiToolStatus};
use rho_ui_proto::{AgentMode, UiAttention, UiTopic, UiWorkdir};
use serde::{Deserialize, Serialize};

/// Longest tool output/error forwarded to the browser, in bytes.
const TOOL_TEXT_LIMIT: usize = 16 * 1024;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToBrowser {
    Hello {
        topics: Vec<TopicJson>,
        workdirs: Vec<WorkdirJson>,
    },
    Agent {
        agent_id: String,
        state: AgentStateJson,
    },
    AgentCreated {
        agent_id: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize)]
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
        repo: String,
        text: String,
    },
    Cancel {
        agent_id: String,
    },
}

#[derive(Debug, Serialize)]
pub struct TopicJson {
    pub name: String,
    pub agents: Vec<AgentSummaryJson>,
}

#[derive(Debug, Serialize)]
pub struct AgentSummaryJson {
    pub id: String,
    pub name: String,
    pub mode: &'static str,
    pub attention: &'static str,
    pub hidden: bool,
}

#[derive(Debug, Serialize)]
pub struct WorkdirJson {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct AgentStateJson {
    pub status: &'static str,
    pub context_used: Option<u64>,
    pub blocks: Vec<BlockJson>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockJson {
    User {
        text: String,
    },
    Assistant {
        text: String,
        final_answer: bool,
    },
    Reasoning {
        text: String,
    },
    Tool {
        name: String,
        preview: Option<String>,
        status: &'static str,
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

pub fn hello(topics: &[UiTopic], workdirs: &[UiWorkdir]) -> ToBrowser {
    ToBrowser::Hello {
        topics: topics
            .iter()
            .map(|topic| TopicJson {
                name: topic.name.clone(),
                agents: topic
                    .agents
                    .iter()
                    .map(|agent| {
                        let id = agent.agent_id.encoded();
                        AgentSummaryJson {
                            name: agent.display_name.clone().unwrap_or_else(|| id.clone()),
                            id,
                            mode: mode_label(agent.mode),
                            attention: attention_label(agent.attention),
                            hidden: agent.hidden,
                        }
                    })
                    .collect(),
            })
            .collect(),
        workdirs: workdirs
            .iter()
            .map(|workdir| WorkdirJson {
                path: workdir.path.to_string(),
                name: workdir.name.clone(),
            })
            .collect(),
    }
}

pub fn agent_state(state: &UiAgentState) -> AgentStateJson {
    AgentStateJson {
        status: status_label(&state.status),
        context_used: state.context_used,
        blocks: state.blocks.iter().map(block).collect(),
    }
}

fn block(block: &UiBlock) -> BlockJson {
    match block {
        UiBlock::UserMessage { text } => BlockJson::User { text: text.clone() },
        UiBlock::AssistantMessage { text, phase } => BlockJson::Assistant {
            text: text.clone(),
            final_answer: matches!(phase, Some(UiMessagePhase::FinalAnswer)),
        },
        UiBlock::Reasoning { text } => BlockJson::Reasoning { text: text.clone() },
        UiBlock::Tool(tool) => BlockJson::Tool {
            name: tool.name.clone(),
            preview: tool
                .preview
                .clone()
                .or_else(|| Some(tool.arguments.clone()).filter(|args| !args.is_empty()))
                .map(|text| truncate(text, TOOL_TEXT_LIMIT)),
            status: tool_status_label(tool.status),
            output: tool
                .output
                .clone()
                .map(|text| truncate(text, TOOL_TEXT_LIMIT)),
            error: tool
                .error
                .clone()
                .map(|text| truncate(text, TOOL_TEXT_LIMIT)),
        },
        UiBlock::Notice { text } => BlockJson::Notice { text: text.clone() },
        UiBlock::QueuedMessage { text, .. } => BlockJson::Queued { text: text.clone() },
        UiBlock::AgentMessage { sender, text } => BlockJson::AgentMessage {
            sender: sender.encoded(),
            text: text.clone(),
        },
    }
}

fn truncate(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(" …[truncated]");
    text
}

fn mode_label(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Deep(_) => "deep",
        AgentMode::Fable { .. } => "fable",
        AgentMode::Opus { .. } => "opus",
        AgentMode::Sol(_) => "sol",
        AgentMode::Luna(_) => "luna",
        AgentMode::Terra(_) => "terra",
        AgentMode::Coordinator(_) => "coordinator",
    }
}

fn attention_label(attention: UiAttention) -> &'static str {
    match attention {
        UiAttention::Quiet => "quiet",
        UiAttention::Working => "working",
        UiAttention::Pending => "pending",
        UiAttention::NeedsInput => "needs_input",
    }
}

fn status_label(status: &UiAgentStatus) -> &'static str {
    match status {
        UiAgentStatus::Idle => "idle",
        UiAgentStatus::Streaming => "streaming",
        UiAgentStatus::ToolCalling { .. } => "tool_calling",
        UiAgentStatus::UnfinishedTurn { .. } => "unfinished",
        UiAgentStatus::Error => "error",
    }
}

fn tool_status_label(status: UiToolStatus) -> &'static str {
    match status {
        UiToolStatus::Running => "running",
        UiToolStatus::Success => "success",
        UiToolStatus::Error => "error",
        UiToolStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        let text = "aé".repeat(10);
        let truncated = truncate(text, 3);
        assert!(truncated.starts_with("aé"));
        assert!(truncated.ends_with("…[truncated]"));
    }

    #[test]
    fn from_browser_parses_commands() {
        let message: FromBrowser =
            serde_json::from_str(r#"{"type":"send","agent_id":"abc","text":"hi"}"#).unwrap();
        assert!(matches!(message, FromBrowser::Send { .. }));
    }
}
