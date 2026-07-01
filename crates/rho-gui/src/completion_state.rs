use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{Context, Entity, Task, Window};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionCandidate {
    value: String,
    description: String,
}

impl CompletionCandidate {
    pub(crate) fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: description.into(),
        }
    }
}

#[derive(Default)]
pub(crate) struct TauCompletionState {
    roles: Vec<CompletionCandidate>,
    known_agents: Vec<String>,
    live_agents: HashSet<String>,
    action_schemas: Vec<ActionSchema>,
}

impl TauCompletionState {
    pub(crate) fn set_roles(&mut self, roles: Vec<CompletionCandidate>) {
        self.roles = roles;
    }

    pub(crate) fn set_agents(&mut self, known_agents: Vec<String>, live_agents: HashSet<String>) {
        self.known_agents = known_agents;
        self.live_agents = live_agents;
    }

    pub(crate) fn apply_action_schema(&mut self, published: &tau_proto::ActionSchemaPublished) {
        if published.schema.validate().is_err() {
            return;
        }
        self.action_schemas.retain(|schema| {
            schema.extension_name != published.extension_name
                || schema.instance_id != published.instance_id
        });
        self.action_schemas.push(ActionSchema {
            extension_name: published.extension_name.clone(),
            instance_id: published.instance_id,
            schema: published.schema.clone(),
        });
    }

    pub(crate) fn remove_extension(
        &mut self,
        extension_name: &tau_proto::ExtensionName,
        instance_id: tau_proto::ExtensionInstanceId,
    ) {
        self.action_schemas.retain(|schema| {
            schema.extension_name != *extension_name || schema.instance_id != instance_id
        });
    }

    pub(crate) fn parse_action_line(
        &self,
        line: &str,
    ) -> Option<Result<ActionDispatch, tau_proto::ParseError>> {
        let root = line.split_whitespace().next()?;
        let action_schema = self.action_schemas.iter().find(|schema| {
            schema
                .schema
                .roots
                .iter()
                .any(|command| command.name == root)
        })?;
        Some(
            action_schema
                .schema
                .parse_line(line)
                .map(|parsed| ActionDispatch {
                    extension_name: action_schema.extension_name.clone(),
                    instance_id: action_schema.instance_id,
                    parsed,
                }),
        )
    }

    fn completions_for(&self, text_before_cursor: &str) -> Vec<CompletionCandidate> {
        if let Some(mention_prefix) = agent_mention_prefix(text_before_cursor) {
            return self.agent_mention_completions(mention_prefix);
        }
        if !text_before_cursor.starts_with('/') {
            return Vec::new();
        }

        let args = slash_command_args(text_before_cursor);
        match args.command.as_str() {
            "" => self.root_command_completions(&args.current),
            "/new" | "/compact" => Vec::new(),
            "/load" => self.load_command_completions(&args.arguments),
            "/model" => {
                candidate_matches(&self.roles, args.arguments.first().copied().unwrap_or(""))
            }
            "/role" => self.role_command_completions(&args.arguments),
            _ => self.action_command_completions(&args.command, &args.arguments),
        }
    }

    fn root_command_completions(&self, needle: &str) -> Vec<CompletionCandidate> {
        root_command_completions(needle)
            .into_iter()
            .chain(self.action_schemas.iter().flat_map(|schema| {
                schema.schema.roots.iter().filter_map(move |root| {
                    completion_matches(&root.name, needle).then(|| {
                        CompletionCandidate::new(
                            root.name.clone(),
                            format!("{} ({})", root.description, schema.extension_name),
                        )
                    })
                })
            }))
            .collect()
    }

    fn action_command_completions(
        &self,
        root_name: &str,
        args: &[&str],
    ) -> Vec<CompletionCandidate> {
        let Some(root) = self
            .action_schemas
            .iter()
            .flat_map(|schema| schema.schema.roots.iter())
            .find(|root| root.name == root_name)
        else {
            return Vec::new();
        };
        complete_action_args(root, args)
    }

    fn agent_mention_completions(&self, needle: &str) -> Vec<CompletionCandidate> {
        self.known_agents
            .iter()
            .filter(|agent| self.live_agents.contains(*agent))
            .filter(|agent| completion_matches(agent, needle))
            .map(|agent| CompletionCandidate::new(agent, "agent"))
            .collect()
    }

    fn load_command_completions(&self, args: &[&str]) -> Vec<CompletionCandidate> {
        match args.len() {
            0 | 1 => {
                let needle = args.first().copied().unwrap_or("");
                self.known_agents
                    .iter()
                    .filter(|agent| completion_matches(agent, needle))
                    .map(|agent| CompletionCandidate::new(agent, "agent"))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn role_command_completions(&self, args: &[&str]) -> Vec<CompletionCandidate> {
        match args.len() {
            0 | 1 => candidate_matches(&self.roles, args.first().copied().unwrap_or("")),
            2 => role_setting_completions(args[1]),
            3 => role_setting_value_completions(args[1], args[2]),
            _ => Vec::new(),
        }
    }
}

struct ActionSchema {
    extension_name: tau_proto::ExtensionName,
    instance_id: tau_proto::ExtensionInstanceId,
    schema: tau_proto::ActionSchema,
}

pub(crate) struct ActionDispatch {
    pub(crate) extension_name: tau_proto::ExtensionName,
    pub(crate) instance_id: tau_proto::ExtensionInstanceId,
    pub(crate) parsed: tau_proto::ParsedAction,
}

pub(crate) struct TauCompletionProvider {
    state: Arc<Mutex<TauCompletionState>>,
}

impl TauCompletionProvider {
    pub(crate) fn new(state: Arc<Mutex<TauCompletionState>>) -> Self {
        Self { state }
    }
}

impl CompletionProvider for TauCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let cursor_offset = buffer_position.to_offset(&buffer);
        let start_anchor = buffer.anchor_before(0);
        let text_before_cursor: String = buffer
            .text_for_range(start_anchor..buffer_position)
            .collect();
        let replace_start = completion_replace_start(&text_before_cursor);
        let replace_range =
            buffer.anchor_before(replace_start)..buffer.anchor_before(cursor_offset);
        let completions = self
            .state
            .lock()
            .ok()
            .map(|state| state.completions_for(&text_before_cursor))
            .unwrap_or_default()
            .into_iter()
            .map(|candidate| Completion {
                replace_range: replace_range.clone(),
                new_text: candidate.value.clone(),
                label: CodeLabel::plain(candidate.value, None),
                documentation: None,
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

struct SlashCommandArgs<'a> {
    command: String,
    arguments: Vec<&'a str>,
    current: String,
}

fn slash_command_args(text: &str) -> SlashCommandArgs<'_> {
    let has_trailing_space = text.ends_with(char::is_whitespace);
    let mut parts = text.split_whitespace().collect::<Vec<_>>();
    if parts.len() <= 1 && !has_trailing_space {
        return SlashCommandArgs {
            command: String::new(),
            arguments: Vec::new(),
            current: text.to_owned(),
        };
    }
    if has_trailing_space {
        parts.push("");
    }
    let command = parts.first().copied().unwrap_or("").to_owned();
    let arguments = parts.into_iter().skip(1).collect::<Vec<_>>();
    let current = arguments.last().copied().unwrap_or("").to_owned();
    SlashCommandArgs {
        command,
        arguments,
        current,
    }
}

fn completion_replace_start(text_before_cursor: &str) -> usize {
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

fn root_command_completions(needle: &str) -> Vec<CompletionCandidate> {
    [
        CompletionCandidate::new("/new", "Create a new agent"),
        CompletionCandidate::new("/load", "Load an agent by id"),
        CompletionCandidate::new("/model", "Select an agent role"),
        CompletionCandidate::new("/role", "Switch, create, edit, or delete an agent role"),
        CompletionCandidate::new("/compact", "Force a compaction pass"),
    ]
    .into_iter()
    .filter(|candidate| completion_matches(&candidate.value, needle))
    .collect()
}

fn complete_action_args(
    root: &tau_proto::ActionCommand,
    args: &[&str],
) -> Vec<CompletionCandidate> {
    let Some(partial) = args.last().copied() else {
        return complete_action_children(root, "");
    };
    let mut command = root;
    let mut index = 0;
    while index + 1 < args.len() && !command.children.is_empty() {
        let token = args[index];
        let Some(child) = command.children.iter().find(|child| child.name == token) else {
            return Vec::new();
        };
        command = child;
        index += 1;
    }
    if !command.children.is_empty() {
        return complete_action_children(command, partial);
    }
    let arg_index = args.len().saturating_sub(index + 1);
    let Some(arg) = command.args.get(arg_index) else {
        return Vec::new();
    };
    match &arg.kind {
        tau_proto::ActionArgKind::Enum { values } => complete_action_choices(values, partial),
        tau_proto::ActionArgKind::String
        | tau_proto::ActionArgKind::Integer
        | tau_proto::ActionArgKind::RestString => {
            complete_action_choices(&arg.suggestions, partial)
        }
    }
}

fn complete_action_children(
    command: &tau_proto::ActionCommand,
    partial: &str,
) -> Vec<CompletionCandidate> {
    ranked_action_items(
        command
            .children
            .iter()
            .map(|child| (child.name.as_str(), child.description.as_str())),
        partial,
    )
}

fn complete_action_choices(
    choices: &[tau_proto::ActionChoice],
    partial: &str,
) -> Vec<CompletionCandidate> {
    ranked_action_items(
        choices
            .iter()
            .map(|choice| (choice.value.as_str(), choice.description.as_str())),
        partial,
    )
}

fn ranked_action_items<'a>(
    values: impl IntoIterator<Item = (&'a str, &'a str)>,
    partial: &str,
) -> Vec<CompletionCandidate> {
    values
        .into_iter()
        .filter(|(value, _)| completion_matches(value, partial))
        .map(|(value, description)| CompletionCandidate::new(value, description))
        .collect()
}

fn role_setting_completions(needle: &str) -> Vec<CompletionCandidate> {
    [
        ("delete", "delete role"),
        ("model", "model id"),
        ("effort", "reasoning effort"),
        ("verbosity", "response verbosity"),
        ("thinking-summary", "thinking summary"),
        ("service-tier", "service tier"),
        ("compaction-threshold", "compaction threshold"),
        ("tools", "enabled tools"),
        ("enable-tools", "enable tools"),
        ("disable-tools", "disable tools"),
    ]
    .into_iter()
    .filter(|(value, _)| completion_matches(value, needle))
    .map(|(value, description)| CompletionCandidate::new(value, description))
    .collect()
}

fn role_setting_value_completions(setting: &str, needle: &str) -> Vec<CompletionCandidate> {
    let values: &[(&str, &str)] = match setting {
        "model" | "tools" | "enable-tools" | "disable-tools" => &[("reset", "clear override")],
        "effort" => &[
            ("reset", "clear override"),
            ("off", "disable reasoning"),
            ("minimal", "minimal"),
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
            ("xhigh", "extra high"),
        ],
        "verbosity" => &[
            ("reset", "clear override"),
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
        ],
        "thinking-summary" => &[
            ("reset", "clear override"),
            ("off", "off"),
            ("auto", "auto"),
            ("concise", "concise"),
            ("detailed", "detailed"),
        ],
        "service-tier" => &[
            ("reset", "clear override"),
            ("fast", "fast"),
            ("flex", "flex"),
        ],
        _ => &[],
    };
    values
        .iter()
        .copied()
        .filter(|(value, _)| completion_matches(value, needle))
        .map(|(value, description)| CompletionCandidate::new(value, description))
        .collect()
}

fn candidate_matches(candidates: &[CompletionCandidate], needle: &str) -> Vec<CompletionCandidate> {
    candidates
        .iter()
        .filter(|candidate| completion_matches(&candidate.value, needle))
        .cloned()
        .collect()
}

fn completion_matches(value: &str, needle: &str) -> bool {
    let needle = needle.to_lowercase();
    needle.is_empty() || value.to_lowercase().contains(&needle)
}

fn agent_mention_prefix(text: &str) -> Option<&str> {
    let token_start = completion_replace_start(text);
    let token = text.get(token_start..)?;
    token.strip_prefix('@')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_completions_offer_known_agents() {
        let mut state = TauCompletionState::default();
        state.set_agents(
            vec!["agent-1".to_owned(), "agent-2".to_owned()],
            HashSet::from(["helper".to_owned(), "worker".to_owned()]),
        );

        let load_agents = state
            .completions_for("/load ")
            .into_iter()
            .map(|candidate| candidate.value)
            .collect::<Vec<_>>();
        assert_eq!(load_agents, vec!["agent-1", "agent-2"]);
    }

    #[test]
    fn role_completions_offer_role_settings_and_values() {
        let mut state = TauCompletionState::default();
        state.set_roles(vec![CompletionCandidate::new("senior-engineer", "role")]);

        assert_eq!(
            state.completions_for("/model senior")[0].value,
            "senior-engineer"
        );
        assert_eq!(
            state.completions_for("/role senior")[0].value,
            "senior-engineer"
        );
        assert!(
            state
                .completions_for("/role senior-engineer effort ")
                .iter()
                .any(|candidate| candidate.value == "high")
        );
    }

    #[test]
    fn agent_mentions_complete_active_agents() {
        let mut state = TauCompletionState::default();
        state.set_agents(
            vec!["helper".to_owned(), "worker".to_owned()],
            HashSet::from(["helper".to_owned(), "worker".to_owned()]),
        );

        let mentions = state
            .completions_for("ask @w")
            .into_iter()
            .map(|candidate| candidate.value)
            .collect::<Vec<_>>();
        assert_eq!(mentions, vec!["worker"]);
    }

    #[test]
    fn action_schema_adds_dynamic_slash_completions_and_dispatch() {
        let mut state = TauCompletionState::default();
        state.apply_action_schema(&tau_proto::ActionSchemaPublished {
            extension_name: "factory".into(),
            instance_id: tau_proto::ExtensionInstanceId::new(1),
            schema: tau_proto::ActionSchema {
                version: tau_proto::ACTION_SCHEMA_VERSION,
                roots: vec![tau_proto::ActionCommand {
                    name: "/factory".to_owned(),
                    description: "Manage tasks".to_owned(),
                    action_id: None,
                    args: Vec::new(),
                    children: vec![tau_proto::ActionCommand {
                        name: "start".to_owned(),
                        description: "Start a task".to_owned(),
                        action_id: Some("start".to_owned()),
                        args: vec![tau_proto::ActionArg {
                            name: "id".to_owned(),
                            description: "Task id".to_owned(),
                            required: true,
                            suggestions: vec![tau_proto::ActionChoice {
                                value: "1".to_owned(),
                                description: "First task".to_owned(),
                            }],
                            kind: tau_proto::ActionArgKind::Integer,
                        }],
                        children: Vec::new(),
                    }],
                }],
            },
        });

        assert!(
            state
                .completions_for("/fac")
                .iter()
                .any(|candidate| candidate.value == "/factory")
        );
        assert_eq!(state.completions_for("/factory ")[0].value, "start");
        assert_eq!(state.completions_for("/factory start ")[0].value, "1");

        let dispatch = state
            .parse_action_line("/factory start 1")
            .expect("known action")
            .expect("valid action");
        assert_eq!(dispatch.parsed.action_id, "start");
        assert_eq!(dispatch.parsed.argv, vec!["1"]);
    }
}
