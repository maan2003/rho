use rho_cli_term_raw::Candidate;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SlashCommand {
    Quit,
    Cancel,
    Clear,
    Help,
    Version,
    Unsupported(String),
    Unknown(String),
}

impl SlashCommand {
    pub(crate) fn parse(line: &str) -> Option<Self> {
        if !line.starts_with('/') {
            return None;
        }
        let command = line.split_whitespace().next().unwrap_or(line);
        Some(match command {
            "/quit" | "/exit" => Self::Quit,
            "/cancel" => Self::Cancel,
            "/clear" => Self::Clear,
            "/help" => Self::Help,
            "/version" => Self::Version,
            "/detach" | "/model" | "/agent" | "/new" | "/suspend" | "/resume" | "/role"
            | "/prompt" | "/skill" | "/set" | "/theme" | "/provider-auth" | "/fast" | "/tree"
            | "/compact" => Self::Unsupported(command.to_owned()),
            other => Self::Unknown(other.to_owned()),
        })
    }
}

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/quit", "Exit chat"),
    ("/cancel", "Cancel the current in-flight prompt"),
    (
        "/detach",
        "Leave the UI but keep the harness running for later reattach",
    ),
    (
        "/model",
        "Switch selected agent model (e.g. /model openai/gpt-5)",
    ),
    ("/agent", "Manage visible/suspended agent transcripts"),
    ("/new", "Alias for /agent new"),
    ("/suspend", "Alias for /agent suspend on the selected agent"),
    ("/resume", "Alias for /agent resume on the selected agent"),
    ("/role", "Switch, create, edit, or delete an agent role"),
    ("/prompt", "Run a saved prompt template"),
    ("/skill", "Run a skill command"),
    ("/set", "Change CLI settings"),
    ("/theme", "Switch CLI theme"),
    ("/version", "Show version"),
    ("/provider-auth", "Manage provider authentication"),
    ("/fast", "Toggle fast service tier"),
    ("/tree", "Show agent tree"),
    ("/compact", "Compact current agent context"),
    ("/help", "Show commands"),
    ("/clear", "Clear rendered output"),
];

pub(crate) fn slash_completion(buffer: &str, cursor: usize) -> Vec<Candidate> {
    if cursor != buffer.len() || !buffer.starts_with('/') || buffer.contains(char::is_whitespace) {
        return Vec::new();
    }
    let needle = buffer.to_lowercase();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    for (command, description) in SLASH_COMMANDS {
        let haystack = command.to_lowercase();
        let candidate = Candidate {
            label: (*command).to_owned(),
            description: (*description).to_owned(),
            replacement: (*command).to_owned(),
        };
        if haystack.starts_with(&needle) {
            prefix.push(candidate);
        } else if haystack.contains(&needle) {
            contains.push(candidate);
        }
    }
    prefix.extend(contains);
    prefix
}
