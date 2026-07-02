//! The `:` command grammar shared by rho clients.
//!
//! One table drives parsing, help, and completion in every client so the
//! command surface can't diverge between the CLI and the GUI. Clients own
//! the presentation (completion popups, replacement mechanics) and any
//! client-local dispatch (quit, clear); this crate owns what the commands
//! *are*.

use std::path::PathBuf;

use rho_ui_proto::{Status, TopicId};

pub struct CommandSpec {
    /// Full command name after the `:`, e.g. `agent new`.
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "agent new",
        usage: ":agent new [path]",
        description: "Start a new agent, optionally in the given working directory",
    },
    CommandSpec {
        name: "agent cancel",
        usage: ":agent cancel",
        description: "Cancel the current agent's turn",
    },
    CommandSpec {
        name: "agent pin",
        usage: ":agent pin",
        description: "Pin/unpin the current agent",
    },
    CommandSpec {
        name: "agent archive",
        usage: ":agent archive",
        description: "Archive/restore the current agent (hidden, not deleted)",
    },
    CommandSpec {
        name: "topic new",
        usage: ":topic new <name>",
        description: "Create a new topic",
    },
    CommandSpec {
        name: "topic move",
        usage: ":topic move <name>",
        description: "Move the current agent into a topic (created when unknown)",
    },
    CommandSpec {
        name: "topic pin",
        usage: ":topic pin [name]",
        description: "Pin/unpin a topic (default: the current one)",
    },
    CommandSpec {
        name: "topic archive",
        usage: ":topic archive [name]",
        description: "Archive/restore a topic (default: the current one)",
    },
    CommandSpec {
        name: "workdirs add",
        usage: ":workdirs add [path] [name]",
        description: "Register a working directory (defaults to the current one)",
    },
    CommandSpec {
        name: "workdirs rm",
        usage: ":workdirs rm <path|name>",
        description: "Unregister a working directory",
    },
    CommandSpec {
        name: "cancel",
        usage: ":cancel",
        description: "Alias for :agent cancel",
    },
    CommandSpec {
        name: "clear",
        usage: ":clear",
        description: "Clear rendered output",
    },
    CommandSpec {
        name: "help",
        usage: ":help",
        description: "Show commands",
    },
    CommandSpec {
        name: "version",
        usage: ":version",
        description: "Show version",
    },
    CommandSpec {
        name: "quit",
        usage: ":quit",
        description: "Exit",
    },
];

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    AgentNew { working_directory: Option<PathBuf> },
    AgentCancel,
    AgentPin,
    AgentArchive,
    TopicNew { name: String },
    TopicMove { name: String },
    TopicPin { name: Option<String> },
    TopicArchive { name: Option<String> },
    WorkdirAdd { path: Option<PathBuf>, name: Option<String> },
    WorkdirRemove { path: String },
    Quit,
    Clear,
    Help,
    Version,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    Command(Command),
    /// Recognized command with bad arguments; carries the usage message.
    Invalid(String),
    Unknown(String),
}

/// Returns `Some` when the line is a `:` command rather than a message.
pub fn parse(line: &str) -> Option<Parsed> {
    let rest = line.trim().strip_prefix(':')?;
    let mut tokens = rest.split_whitespace();
    let first = tokens.next().unwrap_or("");
    let parsed = match first {
        "agent" => match tokens.next() {
            Some("new") => Parsed::Command(Command::AgentNew {
                working_directory: tokens.next().map(PathBuf::from),
            }),
            Some("cancel") => Parsed::Command(Command::AgentCancel),
            Some("pin") => Parsed::Command(Command::AgentPin),
            Some("archive") => Parsed::Command(Command::AgentArchive),
            _ => Parsed::Invalid(":agent new|cancel|pin|archive".to_owned()),
        },
        "topic" => match tokens.next() {
            Some("new") => match joined_name(rest) {
                Some(name) => Parsed::Command(Command::TopicNew { name }),
                None => Parsed::Invalid(":topic new <name>".to_owned()),
            },
            Some("move") => match joined_name(rest) {
                Some(name) => Parsed::Command(Command::TopicMove { name }),
                None => Parsed::Invalid(":topic move <name>".to_owned()),
            },
            Some("pin") => Parsed::Command(Command::TopicPin {
                name: joined_name(rest),
            }),
            Some("archive") => Parsed::Command(Command::TopicArchive {
                name: joined_name(rest),
            }),
            _ => Parsed::Invalid(":topic new|move|pin|archive".to_owned()),
        },
        "workdirs" => match tokens.next() {
            Some("add") => Parsed::Command(Command::WorkdirAdd {
                path: tokens.next().map(PathBuf::from),
                name: tokens.next().map(str::to_owned),
            }),
            Some("rm") => match tokens.next() {
                Some(path) => Parsed::Command(Command::WorkdirRemove {
                    path: path.to_owned(),
                }),
                None => Parsed::Invalid(":workdirs rm <path|name>".to_owned()),
            },
            _ => Parsed::Invalid(":workdirs add|rm".to_owned()),
        },
        "cancel" => Parsed::Command(Command::AgentCancel),
        "quit" | "exit" => Parsed::Command(Command::Quit),
        "clear" => Parsed::Command(Command::Clear),
        "help" => Parsed::Command(Command::Help),
        "version" => Parsed::Command(Command::Version),
        other => Parsed::Unknown(format!(":{other}")),
    };
    Some(parsed)
}

/// The words after `:topic <sub>` as one name, `None` when absent.
fn joined_name(rest: &str) -> Option<String> {
    let name = rest
        .split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(" ");
    (!name.is_empty()).then_some(name)
}

/// Toggle semantics for pin/archive commands: applying the state an item is
/// already in returns it to normal.
pub fn toggle_status(current: Status, target: Status) -> Status {
    if current == target {
        Status::Normal
    } else {
        target
    }
}

/// Client-held data commands complete against.
#[derive(Default)]
pub struct CompletionCtx<'a> {
    /// Registered workdirs as `(name, path)`.
    pub workdirs: &'a [(String, String)],
    pub topics: &'a [String],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Replacement for the token being completed.
    pub value: String,
    pub description: String,
}

/// Token-level completion for text before the cursor, which must start with
/// `:` (leading whitespace stripped by the caller). Clients splice the value
/// over the current token.
pub fn completion_candidates(text_before_cursor: &str, ctx: &CompletionCtx) -> Vec<Candidate> {
    let Some(rest) = text_before_cursor.strip_prefix(':') else {
        return Vec::new();
    };
    let mut tokens = rest.split_whitespace().collect::<Vec<_>>();
    let partial = if rest.ends_with(char::is_whitespace) || rest.is_empty() {
        ""
    } else {
        tokens.pop().unwrap_or("")
    };

    match tokens.as_slice() {
        // Completing (part of) the command name itself, word by word.
        [] => command_word_candidates(&[], partial),
        [first] if !command_exists(&[first]) => command_word_candidates(&[first], partial),
        resolved => argument_candidates(resolved, partial, ctx),
    }
}

fn command_exists(words: &[&str]) -> bool {
    COMMANDS.iter().any(|spec| {
        let name = spec.name.split_whitespace().collect::<Vec<_>>();
        name == words
    })
}

/// Completes the next word of a command name after `prefix_words`.
fn command_word_candidates(prefix_words: &[&str], partial: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for spec in COMMANDS {
        let words = spec.name.split_whitespace().collect::<Vec<_>>();
        if words.len() <= prefix_words.len() || !words.starts_with(prefix_words) {
            continue;
        }
        let word = words[prefix_words.len()];
        // Command words complete by prefix: `:c` should offer `cancel`, not
        // every command containing a `c`.
        if !word.to_lowercase().starts_with(&partial.to_lowercase()) {
            continue;
        }
        // Group words (like a bare `agent`) describe the family; full names
        // describe the command.
        let description = if words.len() == prefix_words.len() + 1 {
            spec.description.to_owned()
        } else {
            format!(":{} …", words[..=prefix_words.len()].join(" "))
        };
        if !candidates
            .iter()
            .any(|candidate: &Candidate| candidate.value == word)
        {
            candidates.push(Candidate {
                value: word.to_owned(),
                description,
            });
        }
    }
    candidates
}

fn argument_candidates(command: &[&str], partial: &str, ctx: &CompletionCtx) -> Vec<Candidate> {
    match command {
        ["topic", "move"] | ["topic", "pin"] | ["topic", "archive"] => ctx
            .topics
            .iter()
            .filter(|topic| fuzzy_contains(topic, partial))
            .map(|topic| Candidate {
                value: topic.clone(),
                description: "topic".to_owned(),
            })
            .collect(),
        ["agent", "new"] | ["workdirs", "rm"] => ctx
            .workdirs
            .iter()
            .filter(|(name, path)| fuzzy_contains(name, partial) || fuzzy_contains(path, partial))
            .map(|(name, path)| Candidate {
                value: name.clone(),
                description: path.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Resolves a topic argument against `(label, id)` pairs, where the label is
/// the display name or the id string for unnamed topics. `None` means no
/// such topic exists yet.
pub fn resolve_topic(argument: &str, topics: &[(String, TopicId)]) -> Option<TopicId> {
    if let Some((_, topic_id)) = topics.iter().find(|(label, _)| label == argument) {
        return Some(*topic_id);
    }
    argument
        .parse::<TopicId>()
        .ok()
        .filter(|parsed| topics.iter().any(|(_, topic_id)| topic_id == parsed))
}

/// Resolves a workdir argument (registered name or path) to its path.
pub fn resolve_workdir<'a>(argument: &str, workdirs: &'a [(String, String)]) -> Option<&'a str> {
    workdirs
        .iter()
        .find(|(name, path)| name == argument || path == argument)
        .map(|(_, path)| path.as_str())
}

fn fuzzy_contains(value: &str, needle: &str) -> bool {
    needle.is_empty() || value.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests;
