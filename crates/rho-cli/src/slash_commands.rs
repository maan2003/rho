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

pub(crate) struct SlashCommandSpec {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) fn slash_commands() -> &'static [SlashCommandSpec] {
    &[
        SlashCommandSpec {
            name: "/quit",
            description: "Exit chat",
        },
        SlashCommandSpec {
            name: "/cancel",
            description: "Cancel the current in-flight prompt",
        },
        SlashCommandSpec {
            name: "/detach",
            description: "Leave the UI but keep the harness running for later reattach",
        },
        SlashCommandSpec {
            name: "/model",
            description: "Switch selected agent model (e.g. /model openai/gpt-5)",
        },
        SlashCommandSpec {
            name: "/agent",
            description: "Manage visible/suspended agent transcripts",
        },
        SlashCommandSpec {
            name: "/new",
            description: "Alias for /agent new",
        },
        SlashCommandSpec {
            name: "/suspend",
            description: "Alias for /agent suspend on the selected agent",
        },
        SlashCommandSpec {
            name: "/resume",
            description: "Alias for /agent resume on the selected agent",
        },
        SlashCommandSpec {
            name: "/role",
            description: "Switch, create, edit, or delete an agent role",
        },
        SlashCommandSpec {
            name: "/prompt",
            description: "Run a saved prompt template",
        },
        SlashCommandSpec {
            name: "/skill",
            description: "Run a skill command",
        },
        SlashCommandSpec {
            name: "/set",
            description: "Change CLI settings",
        },
        SlashCommandSpec {
            name: "/theme",
            description: "Switch CLI theme",
        },
        SlashCommandSpec {
            name: "/version",
            description: "Show version",
        },
        SlashCommandSpec {
            name: "/provider-auth",
            description: "Manage provider authentication",
        },
        SlashCommandSpec {
            name: "/fast",
            description: "Toggle fast service tier",
        },
        SlashCommandSpec {
            name: "/tree",
            description: "Show agent tree",
        },
        SlashCommandSpec {
            name: "/compact",
            description: "Compact current agent context",
        },
        SlashCommandSpec {
            name: "/help",
            description: "Show commands",
        },
        SlashCommandSpec {
            name: "/clear",
            description: "Clear rendered output",
        },
    ]
}
