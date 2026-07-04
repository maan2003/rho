//! The voice control-surface tool vocabulary.
//!
//! Mirrors the `rho-commands` verb set (one shared command surface across
//! clients) plus the read tools a spoken interface needs. This module owns
//! what the tools *are* — schemas the model sees and typed parsing of its
//! calls; executing them against live agents is the daemon's job.
//!
//! Every agent-targeting tool takes an optional `agent`: omitted means the
//! focused agent (the one the user's GUI is looking at), a name means
//! fuzzy-resolve against display names. Tool arguments come from the model
//! and are semi-trusted: parsing is strict on shape but never panics.

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::wire::ToolDefinition;

/// A parsed invocation of one voice tool.
#[derive(Clone, Debug, PartialEq)]
pub enum VoiceToolCall {
    /// Topics, agents, and coarse state — the spoken task board.
    ListAgents,
    AgentStatus {
        agent: Option<String>,
    },
    /// Last final answer text, for the model to summarize aloud.
    ReadLastResponse {
        agent: Option<String>,
    },
    SendMessage {
        agent: Option<String>,
        message: String,
    },
    NewAgent {
        workdir: Option<String>,
        topic: Option<String>,
        message: Option<String>,
    },
    CancelTurn {
        agent: Option<String>,
    },
    RenameAgent {
        agent: Option<String>,
        name: String,
    },
    MoveToTopic {
        agent: Option<String>,
        topic: String,
    },
    ArchiveAgent {
        agent: Option<String>,
    },
    FocusAgent {
        agent: Option<String>,
    },
    ShowAgents,
    OpenNewAgentScreen,
}

const AGENT_PARAM_DESCRIPTION: &str = "Agent display name, matched loosely. Omit to target the agent the user is currently looking at.";

fn agent_only_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "agent": { "type": "string", "description": AGENT_PARAM_DESCRIPTION },
        },
    })
}

/// The function tools offered on `session.update`.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let function =
        |name: &str, description: &str, parameters: serde_json::Value| ToolDefinition::Function {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters,
        };
    vec![
        function(
            "list_agents",
            "List the user's coding agents grouped by topic, with what each one \
             is doing right now. Use this to answer any 'what is going on' \
             question and before resolving a spoken agent name.",
            json!({ "type": "object", "properties": {} }),
        ),
        function(
            "agent_status",
            "Detailed status of one agent: current activity, how long it has \
             been running, and any error.",
            agent_only_schema(),
        ),
        function(
            "read_last_response",
            "The agent's most recent answer, as text. Summarize it aloud; never \
             read code or long output verbatim.",
            agent_only_schema(),
        ),
        function(
            "send_message",
            "Send an instruction or reply to an agent. The agent works \
             asynchronously; you will be told when it finishes.",
            json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": AGENT_PARAM_DESCRIPTION },
                    "message": { "type": "string", "description": "What to tell the agent." },
                },
                "required": ["message"],
            }),
        ),
        function(
            "new_agent",
            "Start a new coding agent, optionally with an initial instruction.",
            json!({
                "type": "object",
                "properties": {
                    "workdir": {
                        "type": "string",
                        "description": "Registered working directory name. Omit for the default.",
                    },
                    "topic": {
                        "type": "string",
                        "description": "Topic to file the agent under. Omit for the default topic.",
                    },
                    "message": {
                        "type": "string",
                        "description": "Initial instruction for the agent.",
                    },
                },
            }),
        ),
        function(
            "cancel_turn",
            "Stop what an agent is currently doing. The agent keeps its work so \
             far and waits for instructions.",
            agent_only_schema(),
        ),
        function(
            "rename_agent",
            "Give an agent a new display name.",
            json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": AGENT_PARAM_DESCRIPTION },
                    "name": { "type": "string", "description": "The new name." },
                },
                "required": ["name"],
            }),
        ),
        function(
            "move_to_topic",
            "Move an agent into a topic, creating the topic if it does not \
             exist yet.",
            json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": AGENT_PARAM_DESCRIPTION },
                    "topic": { "type": "string", "description": "Topic name." },
                },
                "required": ["topic"],
            }),
        ),
        function(
            "archive_agent",
            "Archive an agent: hide it from the board without deleting it.",
            agent_only_schema(),
        ),
        function(
            "focus_agent",
            "Select an agent in the user's GUI so it becomes the current agent. \
             Use this when the user asks to switch to, open, or focus an agent.",
            agent_only_schema(),
        ),
        function(
            "show_agents",
            "Show the user's active agent list in the GUI.",
            json!({ "type": "object", "properties": {} }),
        ),
        function(
            "open_new_agent_screen",
            "Open the GUI's new-agent compose screen.",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

/// Parses a function call from the model into a typed [`VoiceToolCall`].
/// Unknown names and malformed arguments are errors the caller should relay
/// back to the model as the tool output, so it can correct itself.
pub fn parse_tool_call(name: &str, arguments: &str) -> Result<VoiceToolCall> {
    let arguments = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    fn args<'a, T: Deserialize<'a>>(arguments: &'a str) -> Result<T> {
        serde_json::from_str(arguments).context("parse tool arguments")
    }

    #[derive(Deserialize)]
    struct AgentArg {
        agent: Option<String>,
    }
    #[derive(Deserialize)]
    struct SendMessageArgs {
        agent: Option<String>,
        message: String,
    }
    #[derive(Deserialize)]
    struct NewAgentArgs {
        workdir: Option<String>,
        topic: Option<String>,
        message: Option<String>,
    }
    #[derive(Deserialize)]
    struct RenameArgs {
        agent: Option<String>,
        name: String,
    }
    #[derive(Deserialize)]
    struct MoveArgs {
        agent: Option<String>,
        topic: String,
    }

    Ok(match name {
        "list_agents" => VoiceToolCall::ListAgents,
        "agent_status" => {
            let AgentArg { agent } = args(arguments)?;
            VoiceToolCall::AgentStatus { agent }
        }
        "read_last_response" => {
            let AgentArg { agent } = args(arguments)?;
            VoiceToolCall::ReadLastResponse { agent }
        }
        "send_message" => {
            let SendMessageArgs { agent, message } = args(arguments)?;
            VoiceToolCall::SendMessage { agent, message }
        }
        "new_agent" => {
            let NewAgentArgs {
                workdir,
                topic,
                message,
            } = args(arguments)?;
            VoiceToolCall::NewAgent {
                workdir,
                topic,
                message,
            }
        }
        "cancel_turn" => {
            let AgentArg { agent } = args(arguments)?;
            VoiceToolCall::CancelTurn { agent }
        }
        "rename_agent" => {
            let RenameArgs { agent, name } = args(arguments)?;
            VoiceToolCall::RenameAgent { agent, name }
        }
        "move_to_topic" => {
            let MoveArgs { agent, topic } = args(arguments)?;
            VoiceToolCall::MoveToTopic { agent, topic }
        }
        "archive_agent" => {
            let AgentArg { agent } = args(arguments)?;
            VoiceToolCall::ArchiveAgent { agent }
        }
        "focus_agent" => {
            let AgentArg { agent } = args(arguments)?;
            VoiceToolCall::FocusAgent { agent }
        }
        "show_agents" => VoiceToolCall::ShowAgents,
        "open_new_agent_screen" => VoiceToolCall::OpenNewAgentScreen,
        other => bail!("unknown tool: {other}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_definition_parses_back() {
        for definition in tool_definitions() {
            let ToolDefinition::Function { name, .. } = &definition;
            // Tools whose only fields are optional must accept empty args.
            let minimal = match name.as_str() {
                "send_message" => r#"{"message":"hi"}"#,
                "rename_agent" => r#"{"name":"builder"}"#,
                "move_to_topic" => r#"{"topic":"gui work"}"#,
                _ => "{}",
            };
            parse_tool_call(name, minimal)
                .unwrap_or_else(|error| panic!("tool {name} rejected {minimal}: {error:#}"));
        }
    }

    #[test]
    fn empty_arguments_string_means_no_arguments() {
        assert_eq!(
            parse_tool_call("list_agents", "").unwrap(),
            VoiceToolCall::ListAgents
        );
        assert_eq!(
            parse_tool_call("cancel_turn", "").unwrap(),
            VoiceToolCall::CancelTurn { agent: None }
        );
    }

    #[test]
    fn send_message_requires_message() {
        assert!(parse_tool_call("send_message", r#"{"agent":"builder"}"#).is_err());
        assert_eq!(
            parse_tool_call(
                "send_message",
                r#"{"agent":"builder","message":"run the tests"}"#
            )
            .unwrap(),
            VoiceToolCall::SendMessage {
                agent: Some("builder".to_owned()),
                message: "run the tests".to_owned(),
            }
        );
    }

    #[test]
    fn unknown_tool_is_an_error_not_a_panic() {
        let error = parse_tool_call("reboot_machine", "{}").unwrap_err();
        assert!(error.to_string().contains("unknown tool"));
    }

    #[test]
    fn malformed_arguments_are_an_error() {
        assert!(parse_tool_call("agent_status", "{not json").is_err());
    }
}
