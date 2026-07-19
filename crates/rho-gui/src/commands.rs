//! Prompt completions: `@` agent mentions and the draft's field buffers.

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

/// Workstream, group, and label names, feeding prompt completion.
#[derive(Default)]
pub struct PromptNames {
    pub workstreams: Vec<String>,
    pub groups: Vec<String>,
    pub labels: Vec<String>,
}

/// Completion candidates for the text before the cursor: `@` mentions of
/// live agents. Values replace the current whitespace-delimited token.
pub fn completions_for(text_before_cursor: &str, live_agents: &[Candidate]) -> Vec<Candidate> {
    let Some(mention) = mention_prefix(text_before_cursor) else {
        return Vec::new();
    };
    live_agents
        .iter()
        .filter(|agent| {
            fuzzy_contains(&agent.value, mention) || fuzzy_contains(&agent.description, mention)
        })
        .cloned()
        .collect()
}

fn fuzzy_contains(value: &str, needle: &str) -> bool {
    needle.is_empty() || value.to_lowercase().contains(&needle.to_lowercase())
}

fn last_token(text: &str) -> &str {
    &text[token_start(text)..]
}

pub fn token_start(text_before_cursor: &str) -> usize {
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

/// Completion inside the draft's start field buffer: `user` plus the live
/// agent labels, filtered by the token being typed.
pub fn start_field_candidates(
    text_before_cursor: &str,
    live_agents: &[Candidate],
) -> Vec<Candidate> {
    let needle = last_token(text_before_cursor);
    std::iter::once(Candidate {
        value: "user".to_owned(),
        description: "your checkout (Join mode)".to_owned(),
    })
    .chain(live_agents.iter().cloned())
    .filter(|candidate| {
        fuzzy_contains(&candidate.value, needle) || fuzzy_contains(&candidate.description, needle)
    })
    .collect()
}

/// Completion inside the draft's workdir field buffer: registered projects,
/// filtered by the token being typed.
pub fn workdir_field_candidates(
    text_before_cursor: &str,
    workdirs: &[(String, String)],
) -> Vec<Candidate> {
    let needle = last_token(text_before_cursor);
    workdirs
        .iter()
        .filter(|(name, path)| fuzzy_contains(name, needle) || fuzzy_contains(path, needle))
        .map(|(name, path)| Candidate {
            value: name.clone(),
            description: path.clone(),
        })
        .collect()
}

/// Completion inside the draft's role field buffer.
pub fn role_field_candidates(text_before_cursor: &str) -> Vec<Candidate> {
    let trimmed = text_before_cursor.trim_start();
    let token = last_token(text_before_cursor);
    let typing_new_token = text_before_cursor
        .chars()
        .last()
        .is_none_or(char::is_whitespace);
    let words = trimmed.split_whitespace().collect::<Vec<_>>();
    if words.is_empty() || (words.len() == 1 && !typing_new_token) {
        return ["eng", "eng-mini", "eng-low", "eng-high", "eng-ultra", "pm"]
            .into_iter()
            .filter(|mode| fuzzy_contains(mode, token))
            .map(|mode| Candidate {
                value: mode.to_owned(),
                description: match mode {
                    "pm" => "project manager".to_owned(),
                    _ => "engineer intelligence".to_owned(),
                },
            })
            .collect();
    }

    Vec::new()
}

pub struct WorkspaceCompletionProvider {
    workspace: WeakEntity<Workspace>,
    /// The draft view's workdir field buffer: completions in it come from
    /// the registered projects, not the prompt grammar.
    workdir_buffer: Option<gpui::EntityId>,
    /// The draft view's role field buffer: completions are role/intelligence
    /// names.
    role_buffer: Option<gpui::EntityId>,
    /// The draft view's start field buffer: completions are `user` and the
    /// live agent labels.
    start_buffer: Option<gpui::EntityId>,
}

impl WorkspaceCompletionProvider {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        workdir_buffer: Option<gpui::EntityId>,
        role_buffer: Option<gpui::EntityId>,
        start_buffer: Option<gpui::EntityId>,
    ) -> Rc<Self> {
        Rc::new(Self {
            workspace,
            workdir_buffer,
            role_buffer,
            start_buffer,
        })
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
        let (workdirs, live_agents) = self
            .workspace
            .upgrade()
            .map(|workspace| {
                let workspace = workspace.read(cx);
                (workspace.workdir_table(), workspace.live_agent_targets())
            })
            .unwrap_or_default();

        let in_workdir_field = self.workdir_buffer == Some(buffer.entity_id());
        let in_role_field = self.role_buffer == Some(buffer.entity_id());
        let in_start_field = self.start_buffer == Some(buffer.entity_id());
        let buffer = buffer.read(cx);
        let cursor_offset = buffer_position.to_offset(buffer);
        let text_before_cursor = buffer.text_for_range(0..cursor_offset).collect::<String>();
        let replace_start = token_start(&text_before_cursor);
        let replace_range =
            buffer.anchor_before(replace_start)..buffer.anchor_before(cursor_offset);
        let candidates = if in_workdir_field {
            workdir_field_candidates(&text_before_cursor, &workdirs)
        } else if in_role_field {
            role_field_candidates(&text_before_cursor)
        } else if in_start_field {
            start_field_candidates(&text_before_cursor, &live_agents)
        } else {
            completions_for(&text_before_cursor, &live_agents)
        };
        let completions = candidates
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
            matches!(character, ':' | '@' | ' ' | '-' | '/' | '~' | '_')
                || character.is_ascii_alphanumeric()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_field_offers_user_and_agents() {
        let agents = vec![Candidate {
            value: "a3f".to_owned(),
            description: "fix tests".to_owned(),
        }];
        let candidates = start_field_candidates("", &agents);
        assert_eq!(candidates[0].value, "user");
        assert!(candidates.iter().any(|c| c.value == "a3f"));
        let candidates = start_field_candidates("tes", &agents);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "a3f");
        assert_eq!(candidates[0].description, "fix tests");
    }

    #[test]
    fn mentions_complete_live_agents() {
        let live = vec![
            Candidate {
                value: "helper".to_owned(),
                description: "agent".to_owned(),
            },
            Candidate {
                value: "worker".to_owned(),
                description: "agent".to_owned(),
            },
        ];
        let candidates = completions_for("ask @w", &live);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "worker");
    }

    #[test]
    fn workdir_field_completes_workdirs() {
        let workdirs = vec![
            ("rho".to_owned(), "/home/u/src/rho".to_owned()),
            ("zed".to_owned(), "/home/u/src/zed".to_owned()),
        ];
        let candidates = workdir_field_candidates("rh", &workdirs);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "rho");
        assert_eq!(candidates[0].description, "/home/u/src/rho");
        // An empty field offers everything.
        assert_eq!(workdir_field_candidates("", &workdirs).len(), 2);
    }

    #[test]
    fn role_field_completes_roles_and_intelligence() {
        let candidates = role_field_candidates("eng-h");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].value, "eng-high");
    }
}
