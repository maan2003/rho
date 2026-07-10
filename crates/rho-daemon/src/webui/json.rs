//! Projection from UI protocol state to the [`rho_webui_messages`]
//! vocabulary: string agent ids, coarse status/attention labels, and bounded
//! tool output so a long-running agent cannot flood the browser.

use rho_ui_proto::remote::{UiAgentState, UiAgentStatus, UiBlock, UiMessagePhase, UiToolStatus};
use rho_ui_proto::{AgentMode, UiAttention, UiTopic, UiWorkdir};
use rho_webui_messages::{AgentState, AgentSummary, Block, ToBrowser, Topic, Workdir};

/// Longest tool output/error forwarded to the browser, in bytes.
const TOOL_TEXT_LIMIT: usize = 16 * 1024;

pub fn hello(topics: &[UiTopic], workdirs: &[UiWorkdir]) -> ToBrowser {
    ToBrowser::Hello {
        topics: topics
            .iter()
            .map(|topic| Topic {
                name: topic.name.clone(),
                agents: topic
                    .agents
                    .iter()
                    .map(|agent| {
                        let id = agent.agent_id.encoded();
                        AgentSummary {
                            name: agent.display_name.clone().unwrap_or_else(|| id.clone()),
                            id,
                            mode: mode_label(agent.mode).to_owned(),
                            attention: attention_label(agent.attention).to_owned(),
                            hidden: agent.hidden,
                        }
                    })
                    .collect(),
            })
            .collect(),
        workdirs: workdirs
            .iter()
            .map(|workdir| Workdir {
                path: workdir.path.to_string(),
                name: workdir.name.clone(),
            })
            .collect(),
    }
}

pub fn agent_state(state: &UiAgentState) -> AgentState {
    AgentState {
        status: status_label(&state.status).to_owned(),
        context_used: state.context_used,
        blocks: state.blocks.iter().map(block).collect(),
    }
}

fn block(block: &UiBlock) -> Block {
    match block {
        UiBlock::UserMessage { text } => Block::User { text: text.clone() },
        UiBlock::AssistantMessage { text, phase } => Block::Assistant {
            text: text.clone(),
            final_answer: matches!(phase, Some(UiMessagePhase::FinalAnswer)),
        },
        UiBlock::Reasoning { text } => Block::Reasoning { text: text.clone() },
        UiBlock::Tool(tool) => Block::Tool {
            name: tool.name.clone(),
            preview: tool
                .preview
                .clone()
                .or_else(|| Some(tool.arguments.clone()).filter(|args| !args.is_empty()))
                .map(|text| truncate(text, TOOL_TEXT_LIMIT)),
            status: tool_status_label(tool.status).to_owned(),
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
}
