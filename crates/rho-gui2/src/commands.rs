//! Prompt completions for `:` commands (shared grammar from
//! [`rho_commands`]) and `@` agent mentions.

use std::rc::Rc;

use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{Context, Entity, Task, WeakEntity, Window};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};

use crate::workspace::Workspace;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub value: String,
    pub description: String,
}

/// Completion candidates for the text before the cursor. Values replace the
/// current whitespace-delimited token (the `:` of the leading command token
/// is preserved).
pub fn completions_for(
    text_before_cursor: &str,
    workdirs: &[(String, String)],
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
    let trimmed = text_before_cursor.trim_start();
    if !trimmed.starts_with(':') {
        return Vec::new();
    }
    let colon = if last_token(text_before_cursor).starts_with(':') {
        ":"
    } else {
        ""
    };
    rho_commands::completion_candidates(
        trimmed,
        &rho_commands::CompletionCtx {
            workdirs,
            known_agents,
        },
    )
    .into_iter()
    .map(|candidate| Candidate {
        value: format!("{colon}{}", candidate.value),
        description: candidate.description,
    })
    .collect()
}

fn fuzzy_contains(value: &str, needle: &str) -> bool {
    needle.is_empty() || value.to_lowercase().contains(&needle.to_lowercase())
}

fn last_token(text: &str) -> &str {
    &text[token_start(text)..]
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
        let (workdirs, known_agents, live_agents) = self
            .workspace
            .upgrade()
            .map(|workspace| {
                let workspace = workspace.read(cx);
                (
                    workspace.workdir_table(),
                    workspace.known_agent_names(),
                    workspace.live_agent_names(),
                )
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
        let completions =
            completions_for(&text_before_cursor, &workdirs, &known_agents, &live_agents)
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
            character == ':'
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
        let candidates = completions_for(":", &[], &[], &[]);
        assert!(candidates.iter().any(|c| c.value == ":agent"));
        assert!(candidates.iter().any(|c| c.value == ":workdirs"));
        let candidates = completions_for(":agent lo", &[], &[], &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "load");
    }

    #[test]
    fn load_completes_known_agents() {
        let known = vec!["agent-1".to_owned(), "agent-2".to_owned()];
        let candidates = completions_for(":agent load ", &[], &known, &[]);
        assert_eq!(
            candidates.iter().map(|c| c.value.as_str()).collect::<Vec<_>>(),
            vec!["agent-1", "agent-2"]
        );
        let candidates = completions_for(":agent load 2", &[], &known, &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "agent-2");
    }

    #[test]
    fn agent_new_completes_workdirs() {
        let workdirs = vec![("rho".to_owned(), "/home/u/src/rho".to_owned())];
        let candidates = completions_for(":agent new ", &workdirs, &[], &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "rho");
        assert_eq!(candidates[0].description, "/home/u/src/rho");
    }

    #[test]
    fn mentions_complete_live_agents() {
        let live = vec!["helper".to_owned(), "worker".to_owned()];
        let candidates = completions_for("ask @w", &[], &[], &live);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "worker");
    }
}
