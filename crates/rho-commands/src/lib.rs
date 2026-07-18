//! The `:` command grammar shared by rho clients.
//!
//! One table drives parsing, help, and completion in every client so the
//! command surface can't diverge between the CLI and the GUI. Clients own
//! the presentation (completion popups, replacement mechanics) and any
//! client-local dispatch (quit, clear); this crate owns what the commands
//! *are*.

use camino::Utf8PathBuf;
use rho_ui_proto::{Status, TagId};

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
        name: "agent rename",
        usage: ":agent rename <name>",
        description: "Rename the current agent",
    },
    CommandSpec {
        name: "agent change-prompt-cache-key",
        usage: ":agent change-prompt-cache-key",
        description: "Give the current agent a fresh prompt cache key",
    },
    CommandSpec {
        name: "agent pin",
        usage: ":agent pin",
        description: "Pin/unpin the current agent",
    },
    CommandSpec {
        name: "tag move",
        usage: ":tag move <workstream>",
        description: "Move the current agent into a workstream (created when unknown)",
    },
    CommandSpec {
        name: "tag group",
        usage: ":tag group <group>",
        description: "Put the current workstream under a group (created when unknown)",
    },
    CommandSpec {
        name: "tag label",
        usage: ":tag label <name>",
        description: "Add a label to the current agent (created when unknown)",
    },
    CommandSpec {
        name: "tag unlabel",
        usage: ":tag unlabel <name>",
        description: "Remove a label from the current agent",
    },
    CommandSpec {
        name: "tag rename",
        usage: ":tag rename <name>",
        description: "Rename the current workstream",
    },
    CommandSpec {
        name: "tag pin",
        usage: ":tag pin [workstream]",
        description: "Pin/unpin a workstream (default: the current one)",
    },
    CommandSpec {
        name: "projects add",
        usage: ":projects add [path] [name]",
        description: "Register a working directory (defaults to the current one)",
    },
    CommandSpec {
        name: "projects rm",
        usage: ":projects rm <path|name>",
        description: "Unregister a working directory",
    },
    CommandSpec {
        name: "cancel",
        usage: ":cancel",
        description: "Alias for :agent cancel",
    },
    CommandSpec {
        name: "rewind",
        usage: ":rewind [turns]",
        description: "Fork the current agent's history before previous turns",
    },
    CommandSpec {
        name: "continue",
        usage: ":continue",
        description: "Continue an unfinished turn after daemon restart",
    },
    CommandSpec {
        name: "compact",
        usage: ":compact",
        description: "Compact the current agent's context",
    },
    CommandSpec {
        name: "done",
        usage: ":done [hide]",
        description: "Mark the current agent's finished turn as handled; :done hide also folds it away",
    },
    CommandSpec {
        name: "snooze",
        usage: ":snooze <duration>",
        description: "Silence the current agent until later, e.g. :snooze 2h (m/h/d)",
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
    CommandSpec {
        name: "open",
        usage: ":open <path>",
        description: "Open a file from the agent's workspace",
    },
];

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    AgentNew {
        working_directory: Option<Utf8PathBuf>,
    },
    AgentRename {
        name: String,
    },
    AgentCancel,
    AgentChangePromptCacheKey,
    AgentPin,
    TagMove {
        name: String,
    },
    TagGroup {
        name: String,
    },
    TagLabel {
        name: String,
    },
    TagUnlabel {
        name: String,
    },
    TagRename {
        name: String,
    },
    TagPin {
        name: Option<String>,
    },
    ProjectAdd {
        path: Option<Utf8PathBuf>,
        name: Option<String>,
        description: String,
    },
    ProjectRemove {
        path: String,
    },
    Rewind {
        turns: u32,
    },
    Continue,
    Compact,
    AgentDone {
        /// Also fold the agent away in the rail (`:done hide`).
        hide: bool,
    },
    AgentSnooze {
        duration_ms: u64,
    },

    Open {
        path: Utf8PathBuf,
    },
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
                working_directory: tokens.next().map(Utf8PathBuf::from),
            }),
            Some("rename") => match joined_name(rest) {
                Some(name) => Parsed::Command(Command::AgentRename { name }),
                None => Parsed::Invalid(":agent rename <name>".to_owned()),
            },
            Some("cancel") => Parsed::Command(Command::AgentCancel),
            Some("change-prompt-cache-key") => Parsed::Command(Command::AgentChangePromptCacheKey),
            Some("pin") => Parsed::Command(Command::AgentPin),
            _ => Parsed::Invalid(":agent new|rename|cancel|change-prompt-cache-key|pin".to_owned()),
        },
        "tag" => match tokens.next() {
            Some("move") => match joined_name(rest) {
                Some(name) => Parsed::Command(Command::TagMove { name }),
                None => Parsed::Invalid(":tag move <workstream>".to_owned()),
            },
            Some("group") => match joined_name(rest) {
                Some(name) => Parsed::Command(Command::TagGroup { name }),
                None => Parsed::Invalid(":tag group <group>".to_owned()),
            },
            Some("label") => match joined_name(rest) {
                Some(name) => Parsed::Command(Command::TagLabel { name }),
                None => Parsed::Invalid(":tag label <name>".to_owned()),
            },
            Some("unlabel") => match joined_name(rest) {
                Some(name) => Parsed::Command(Command::TagUnlabel { name }),
                None => Parsed::Invalid(":tag unlabel <name>".to_owned()),
            },
            Some("rename") => match joined_name(rest) {
                Some(name) => Parsed::Command(Command::TagRename { name }),
                None => Parsed::Invalid(":tag rename <name>".to_owned()),
            },
            Some("pin") => Parsed::Command(Command::TagPin {
                name: joined_name(rest),
            }),
            _ => Parsed::Invalid(":tag move|group|label|unlabel|rename|pin".to_owned()),
        },
        "projects" => match tokens.next() {
            Some("add") => {
                let path = tokens.next().map(Utf8PathBuf::from);
                let name = tokens.next().map(str::to_owned);
                Parsed::Command(Command::ProjectAdd {
                    path,
                    name,
                    description: tokens.collect::<Vec<_>>().join(" "),
                })
            }
            Some("rm") => match tokens.next() {
                Some(path) => Parsed::Command(Command::ProjectRemove {
                    path: path.to_owned(),
                }),
                None => Parsed::Invalid(":projects rm <path|name>".to_owned()),
            },
            _ => Parsed::Invalid(":projects add|rm".to_owned()),
        },
        "cancel" => Parsed::Command(Command::AgentCancel),
        "rewind" => parse_rewind(&mut tokens),
        "continue" => Parsed::Command(Command::Continue),
        "compact" => Parsed::Command(Command::Compact),
        "done" => parse_done(&mut tokens),
        "snooze" => parse_snooze(&mut tokens),
        "open" => match tokens.next() {
            Some(path) => Parsed::Command(Command::Open {
                path: Utf8PathBuf::from(path),
            }),
            None => Parsed::Invalid(":open <path>".to_owned()),
        },
        "quit" | "exit" => Parsed::Command(Command::Quit),
        "clear" => Parsed::Command(Command::Clear),
        "help" => Parsed::Command(Command::Help),
        "version" => Parsed::Command(Command::Version),
        other => Parsed::Unknown(format!(":{other}")),
    };
    Some(parsed)
}

fn parse_done<'a>(tokens: &mut impl Iterator<Item = &'a str>) -> Parsed {
    match (tokens.next(), tokens.next()) {
        (None, None) => Parsed::Command(Command::AgentDone { hide: false }),
        (Some("hide"), None) => Parsed::Command(Command::AgentDone { hide: true }),
        _ => Parsed::Invalid(":done [hide]".to_owned()),
    }
}

fn parse_snooze<'a>(tokens: &mut impl Iterator<Item = &'a str>) -> Parsed {
    match (tokens.next().and_then(parse_duration_ms), tokens.next()) {
        (Some(duration_ms), None) => Parsed::Command(Command::AgentSnooze { duration_ms }),
        _ => Parsed::Invalid(":snooze <duration> (e.g. 30m, 2h, 1d)".to_owned()),
    }
}

/// `30m`, `2h`, `1d`; a bare number means minutes.
pub fn parse_duration_ms(text: &str) -> Option<u64> {
    let (digits, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "m"),
    };
    let count: u64 = digits.parse().ok()?;
    let minutes = match unit {
        "m" | "min" => count,
        "h" | "hr" => count.checked_mul(60)?,
        "d" => count.checked_mul(60 * 24)?,
        _ => return None,
    };
    minutes.checked_mul(60 * 1000)
}

/// The words after `:tag <sub>` as one name, `None` when absent.
fn joined_name(rest: &str) -> Option<String> {
    let name = rest
        .split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(" ");
    (!name.is_empty()).then_some(name)
}

/// Toggle semantics for pin commands: applying the state an item is
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
    /// Tag names by kind for `:tag` argument completion.
    pub workstreams: &'a [String],
    pub groups: &'a [String],
    pub labels: &'a [String],
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

fn parse_rewind<'a>(tokens: &mut impl Iterator<Item = &'a str>) -> Parsed {
    match (tokens.next(), tokens.next()) {
        (None, None) => Parsed::Command(Command::Rewind { turns: 1 }),
        (Some(value), None) => match value.parse::<u32>() {
            Ok(turns) if turns > 0 => Parsed::Command(Command::Rewind { turns }),
            _ => Parsed::Invalid(":rewind [turns]".to_owned()),
        },
        _ => Parsed::Invalid(":rewind [turns]".to_owned()),
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
        ["tag", "move"] | ["tag", "pin"] => {
            tag_candidates(ctx.workstreams, "workstream", partial)
        }
        ["tag", "group"] => tag_candidates(ctx.groups, "group", partial),
        ["tag", "label"] | ["tag", "unlabel"] => tag_candidates(ctx.labels, "label", partial),
        ["agent", "new"] | ["projects", "rm"] => ctx
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

fn tag_candidates(names: &[String], description: &str, partial: &str) -> Vec<Candidate> {
    names
        .iter()
        .filter(|name| fuzzy_contains(name, partial))
        .map(|name| Candidate {
            value: name.clone(),
            description: description.to_owned(),
        })
        .collect()
}

/// Resolves a tag argument against `(name, id)` pairs; tag names are unique,
/// so the name is the identity. `None` means no such tag exists yet.
pub fn resolve_tag(argument: &str, tags: &[(String, TagId)]) -> Option<TagId> {
    tags.iter()
        .find(|(name, _)| name == argument)
        .map(|(_, tag_id)| *tag_id)
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
