//! Projection from UI protocol state to the [`rho_webui_messages`]
//! vocabulary: string agent ids, coarse status/attention labels, and bounded
//! tool output so a long-running agent cannot flood the browser.

use rho_ui_proto::remote::{
    UiAgentState, UiAgentStatus, UiBlock, UiMessagePhase, UiTool, UiToolStatus,
};
use rho_ui_proto::{AgentRole, EngineerIntelligence, UiAttention, UiProject, UiTopic};
use rho_webui_messages::{AgentState, AgentSummary, Block, Project, ToBrowser, Topic};

/// Longest tool output/error forwarded to the browser, in bytes.
const TOOL_TEXT_LIMIT: usize = 16 * 1024;

/// Longest one-line tool label, in bytes.
const TOOL_LABEL_LIMIT: usize = 256;

pub fn hello(topics: &[UiTopic], projects: &[UiProject]) -> ToBrowser {
    ToBrowser::Hello {
        topics: topics
            .iter()
            .filter(|topic| !topic.hidden)
            .map(|topic| Topic {
                id: topic.topic_id.encoded(),
                name: topic.name.clone(),
                pinned: topic.status == rho_ui_proto::Status::Pinned,
                agents: topic
                    .agents
                    .iter()
                    .map(|agent| {
                        let id = agent.agent_id.encoded();
                        AgentSummary {
                            name: agent.display_name.clone().unwrap_or_else(|| id.clone()),
                            id,
                            role: role_label(agent.role).to_owned(),
                            pinned: agent.status == rho_ui_proto::Status::Pinned,
                            updated_at: agent.updated_at.0,
                            attention: attention_label(agent.attention).to_owned(),
                            hidden: agent.hidden,
                        }
                    })
                    .collect(),
            })
            .collect(),
        projects: projects
            .iter()
            .map(|project| Project {
                path: project.path.to_string(),
                name: project.name.clone(),
                description: project.description.clone(),
            })
            .collect(),
    }
}

pub fn agent_state(state: &UiAgentState) -> AgentState {
    AgentState {
        status: status_label(&state.status).to_owned(),
        context_used: state.context_used,
        blocks: state.blocks.iter().filter_map(block).collect(),
    }
}

/// `None` for blocks the web UI never renders (reasoning, like the GUI).
fn block(block: &UiBlock) -> Option<Block> {
    Some(match block {
        UiBlock::UserMessage { text } => Block::User { text: text.clone() },
        UiBlock::AssistantMessage { text, phase } => Block::Assistant {
            text: text.clone(),
            final_answer: matches!(phase, Some(UiMessagePhase::FinalAnswer)),
        },
        UiBlock::Reasoning { .. } => return None,
        UiBlock::Tool(tool) => Block::Tool {
            label: truncate(tool_label(&tool.name, &tool.arguments), TOOL_LABEL_LIMIT),
            status: tool_status_label(tool.status).to_owned(),
            duration_ms: tool_duration_ms(tool),
            output: tool
                .output
                .clone()
                .map(|text| truncate(text, TOOL_TEXT_LIMIT)),
            error: tool
                .error
                .clone()
                .map(|text| truncate(text, TOOL_TEXT_LIMIT)),
        },
        UiBlock::Notice { text } => Block::Notice { text: text.clone() },
        UiBlock::QueuedMessage { text, .. } => Block::Queued { text: text.clone() },
        UiBlock::AgentMessage { sender, text } => Block::AgentMessage {
            sender: sender.encoded(),
            text: text.clone(),
        },
    })
}

/// One-line tool label matching the GUI transcript: shell-like tools render
/// as `$ command`, Claude's file tools as `read/write/edit path`, everything
/// else as `name arguments`. Argument extraction tolerates the partial JSON
/// seen while arguments stream.
fn tool_label(name: &str, arguments: &str) -> String {
    match name {
        "shell" | "shell_command" | "exec_command" | "write_stdin" | "Bash" => {
            let command = streaming_json_text_field(arguments, "command")
                .or_else(|| {
                    (!arguments.trim_start().starts_with('{')).then(|| arguments.to_owned())
                })
                .unwrap_or_default();
            if command.is_empty() {
                "$".to_owned()
            } else {
                format!("$ {command}")
            }
        }
        "Read" | "Write" | "Edit" => {
            let verb = name.to_ascii_lowercase();
            match streaming_json_text_field(arguments, "file_path") {
                Some(path) if !path.is_empty() => format!("{verb} {path}"),
                _ => verb,
            }
        }
        _ if arguments.is_empty() => name.to_owned(),
        _ => format!("{name} {arguments}"),
    }
}

fn streaming_json_text_field(arguments: &str, key: &str) -> Option<String> {
    let mut parser = json_stream::JsonStreamParser::new();
    for character in arguments.chars() {
        if parser.add_char(character).is_err() {
            return None;
        }
    }
    parser
        .get_result()
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn tool_duration_ms(tool: &UiTool) -> Option<u64> {
    let started = tool.started_at?.0;
    let finished = tool.finished_at?.0;
    Some(finished.saturating_sub(started))
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

fn role_label(config: AgentRole) -> &'static str {
    match config {
        AgentRole::PM | AgentRole::WorkflowPM { .. } => "pm",
        AgentRole::Advisor {
            intelligence: rho_ui_proto::AdvisorIntelligence::Medium,
        } => "advisor",
        AgentRole::Advisor {
            intelligence: rho_ui_proto::AdvisorIntelligence::High,
        } => "advisor-high",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Mini,
            ..
        } => "eng-mini",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Low,
            ..
        } => "eng-low",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Medium,
            ..
        } => "eng",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::High,
            ..
        } => "eng-high",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Ultra,
            ..
        } => "eng-ultra",
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
}
