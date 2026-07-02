//! Slash commands and prompt completions.
//!
//! One table drives both dispatch and completion so the two can't diverge:
//! every completable command is handled and every handled command is
//! completable.

use std::rc::Rc;
use std::str::FromStr as _;

use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{Context, Entity, Task, WeakEntity, Window};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
use rho_ui_proto::AgentId;

use crate::workspace::Workspace;

pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/new",
        description: "Start a new agent draft",
    },
    CommandSpec {
        name: "/load",
        description: "Load an agent by id",
    },
    CommandSpec {
        name: "/cancel",
        description: "Cancel the current agent's turn",
    },
];

pub enum ParsedCommand {
    New,
    Load(AgentId),
    Cancel,
    /// A recognized command with a bad argument; carries the usage message.
    Invalid(String),
    Unsupported,
}

/// Returns `Some` when the submitted text is a command rather than a message.
pub fn parse(text: &str) -> Option<ParsedCommand> {
    if text == "/new" {
        return Some(ParsedCommand::New);
    }
    if text == "/cancel" {
        return Some(ParsedCommand::Cancel);
    }
    if let Some(argument) = text.strip_prefix("/load") {
        let argument = argument.trim();
        if argument.is_empty() {
            return Some(ParsedCommand::Invalid("/load <agent-id>".to_owned()));
        }
        return Some(match AgentId::from_str(argument) {
            Ok(agent_id) => ParsedCommand::Load(agent_id),
            Err(_) => ParsedCommand::Invalid(format!("/load: invalid agent id `{argument}`")),
        });
    }
    if text.starts_with('/') || text.starts_with('!') {
        return Some(ParsedCommand::Unsupported);
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub value: String,
    pub description: String,
}

/// Completion candidates for the text before the cursor.
pub fn completions_for(
    text_before_cursor: &str,
    known_agents: &[String],
    live_agents: &[String],
) -> Vec<Candidate> {
    if let Some(mention) = mention_prefix(text_before_cursor) {
        return live_agents
            .iter()
            .filter(|agent| fuzzy_contains(agent, mention))
            .map(|agent| Candidate {
                value: agent.clone(),
                description: "agent".to_owned(),
            })
            .collect();
    }
    if !text_before_cursor.starts_with('/') {
        return Vec::new();
    }
    let mut words = text_before_cursor.split_whitespace();
    let command = words.next().unwrap_or("");
    let complete_command = !text_before_cursor.contains(char::is_whitespace);
    if complete_command {
        return COMMANDS
            .iter()
            .filter(|spec| fuzzy_contains(spec.name, command))
            .map(|spec| Candidate {
                value: spec.name.to_owned(),
                description: spec.description.to_owned(),
            })
            .collect();
    }
    match command {
        "/load" => {
            let argument = last_token(text_before_cursor);
            if words.count() > 1 {
                return Vec::new();
            }
            known_agents
                .iter()
                .filter(|agent| fuzzy_contains(agent, argument))
                .map(|agent| Candidate {
                    value: agent.clone(),
                    description: "agent".to_owned(),
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn fuzzy_contains(value: &str, needle: &str) -> bool {
    needle.is_empty() || value.to_lowercase().contains(&needle.to_lowercase())
}

fn last_token(text: &str) -> &str {
    if text.ends_with(char::is_whitespace) {
        return "";
    }
    text.split_whitespace().last().unwrap_or("")
}

fn token_start(text_before_cursor: &str) -> usize {
    text_before_cursor
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            character
                .is_whitespace()
                .then_some(index + character.len_utf8())
        })
        .unwrap_or(0)
}

fn mention_prefix(text: &str) -> Option<&str> {
    text.get(token_start(text)..)?.strip_prefix('@')
}

pub struct WorkspaceCompletionProvider {
    workspace: WeakEntity<Workspace>,
}

impl WorkspaceCompletionProvider {
    pub fn new(workspace: WeakEntity<Workspace>) -> Rc<Self> {
        Rc::new(Self { workspace })
    }
}

impl CompletionProvider for WorkspaceCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let (known_agents, live_agents) = self
            .workspace
            .upgrade()
            .map(|workspace| {
                let workspace = workspace.read(cx);
                (workspace.known_agent_names(), workspace.live_agent_names())
            })
            .unwrap_or_default();

        let buffer = buffer.read(cx);
        let cursor_offset = buffer_position.to_offset(buffer);
        let text_before_cursor = buffer
            .text_for_range(0..cursor_offset)
            .collect::<String>();
        let replace_start = token_start(&text_before_cursor);
        let replace_range =
            buffer.anchor_before(replace_start)..buffer.anchor_before(cursor_offset);
        let completions = completions_for(&text_before_cursor, &known_agents, &live_agents)
            .into_iter()
            .map(|candidate| Completion {
                replace_range: replace_range.clone(),
                new_text: candidate.value.clone(),
                label: CodeLabel::plain(candidate.value, None),
                documentation: if candidate.description.is_empty() {
                    None
                } else {
                    Some(project::lsp_store::CompletionDocumentation::SingleLine(
                        candidate.description.into(),
                    ))
                },
                source: CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
                group: None,
            })
            .collect();

        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<Buffer>,
        _position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        text.chars().last().is_some_and(|character| {
            character == '/'
                || character == '@'
                || character == ' '
                || character == '-'
                || character.is_ascii_alphanumeric()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_commands_complete_by_prefix() {
        let candidates = completions_for("/", &[], &[]);
        assert_eq!(candidates.len(), COMMANDS.len());
        let candidates = completions_for("/lo", &[], &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "/load");
    }

    #[test]
    fn load_completes_known_agents() {
        let known = vec!["agent-1".to_owned(), "agent-2".to_owned()];
        let candidates = completions_for("/load ", &known, &[]);
        assert_eq!(
            candidates.iter().map(|c| c.value.as_str()).collect::<Vec<_>>(),
            vec!["agent-1", "agent-2"]
        );
        let candidates = completions_for("/load 2", &known, &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "agent-2");
    }

    #[test]
    fn mentions_complete_live_agents() {
        let live = vec!["helper".to_owned(), "worker".to_owned()];
        let candidates = completions_for("ask @w", &[], &live);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "worker");
    }

    #[test]
    fn every_completable_command_parses() {
        for spec in COMMANDS {
            assert!(
                !matches!(parse(spec.name), None | Some(ParsedCommand::Unsupported)),
                "{} completes but does not dispatch",
                spec.name
            );
        }
    }
}
