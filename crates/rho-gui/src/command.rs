//! Commands as Rust values.
//!
//! There is no textual command grammar any more — transient menus (and
//! minibuffer prompts they open) construct these values directly and hand
//! them to `Workspace::dispatch_command`.

use camino::Utf8PathBuf;
use rho_ui_proto::{Status, TagId};

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
        /// Also fold the agent away in the rail.
        hide: bool,
    },
    AgentSnooze {
        duration_ms: u64,
    },
    Open {
        path: Utf8PathBuf,
    },
    Term {
        new: bool,
    },
    Quit,
    Version,
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

/// Toggle semantics for pin commands: applying the state an item is
/// already in returns it to normal.
pub fn toggle_status(current: Status, target: Status) -> Status {
    if current == target {
        Status::Normal
    } else {
        target
    }
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
