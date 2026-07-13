use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::PathBuf;

use anyhow::Context as _;
use rho_agent::db::{
    AdvisorIntelligence, AgentReadTxnExt as _, AgentRole, AgentRuntime, AgentWriteTxnExt as _,
    EngineerIntelligence, Status,
};
use rho_db::RhoDb;
use rho_workspaces::WorkspaceInfo;

use crate::default_db_path;

#[derive(Clone, Debug, clap::Args)]
pub struct DebugArgs {
    /// Source database path. Defaults to rho's normal daemon database.
    #[arg(long = "db-path")]
    db_path: Option<PathBuf>,

    #[command(subcommand)]
    command: DebugCommand,
}

#[derive(Clone, Debug, clap::Subcommand)]
enum DebugCommand {
    /// Snapshot the database and print persisted agent records.
    Agents,
    /// Snapshot the database and run pending migrations on the copy.
    Migrate,
    /// Snapshot the database and print the context usage each agent would
    /// restore on load (event log for Rho agents, session transcript for
    /// Claude agents).
    Context,
    /// Render the system prompt and top-level model-facing tools for a role.
    RenderPrompt {
        /// Role text: eng, eng-low, eng-high, eng-ultra, pm, advisor, or
        /// advisor-high.
        role: String,
    },
}

pub async fn run(args: DebugArgs) -> anyhow::Result<()> {
    match args.command {
        DebugCommand::Agents => print_agents(args.db_path).await,
        DebugCommand::Migrate => test_migration(args.db_path).await,
        DebugCommand::Context => print_context(args.db_path).await,
        DebugCommand::RenderPrompt { role } => render_prompt(&role).await,
    }
}

async fn render_prompt(role: &str) -> anyhow::Result<()> {
    let role = parse_role(role)?;
    let cwd = std::env::current_dir().context("read current directory")?;
    let (root, is_jj) = rho_workspaces::resolve_workdir_root(&cwd)?;
    let repo = if is_jj {
        rho_workspaces::Repo::open(root.as_std_path())?
    } else {
        rho_workspaces::Repo::open_plain_with_path_overrides(
            root.as_std_path(),
            rho_workspaces::PathOverrides::default(),
        )?
    };
    let workspace = std::sync::Arc::new(repo).user_checkout().await?;
    let view = rho_workspaces::View::new(vec![workspace])?;
    let surface = rho_agent::render_agent_surface(view, role)?;

    println!("# System prompt\n");
    if surface.system_prompt.is_empty() {
        println!("(empty; Claude Code supplies its own system prompt)");
    } else {
        print!("{}", surface.system_prompt);
        if !surface.system_prompt.ends_with('\n') {
            println!();
        }
    }
    println!("\n# Tools");
    if surface.tools.is_empty() {
        println!("\n(none supplied by Rho at the provider API level)");
    }
    for tool in surface.tools.iter() {
        println!("\n## {} ({:?})\n", tool.name.as_str(), tool.tool_type);
        println!("{}", tool.description);
        if !tool.input_schema.is_null() {
            println!(
                "\nInput schema:\n```json\n{}\n```",
                serde_json::to_string_pretty(&tool.input_schema)?
            );
        }
        if let Some(format) = &tool.format {
            println!("\nFormat:\n```text\n{format:?}\n```");
        }
    }
    Ok(())
}

fn parse_role(text: &str) -> anyhow::Result<AgentRole> {
    Ok(match text {
        "eng" => AgentRole::default(),
        "eng-low" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
        },
        "eng-high" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        },
        "eng-ultra" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
        },
        "pm" => AgentRole::pm(),
        "advisor" => AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Medium,
        },
        "advisor-high" => AgentRole::Advisor {
            intelligence: AdvisorIntelligence::High,
        },
        _ => anyhow::bail!(
            "unknown role `{text}`; use eng, eng-low, eng-high, eng-ultra, pm, advisor, or advisor-high"
        ),
    })
}

struct Snapshot {
    source: PathBuf,
    path: PathBuf,
    _temp: tempfile::TempDir,
}

fn copy_snapshot(db_path: Option<PathBuf>) -> anyhow::Result<Snapshot> {
    let source = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    let temp = tempfile::tempdir().context("create debug db snapshot tempdir")?;
    let snapshot = temp.path().join("rho.redb");
    std::fs::copy(&source, &snapshot)
        .with_context(|| format!("copy rho db snapshot from {}", source.display()))?;
    Ok(Snapshot {
        source,
        path: snapshot,
        _temp: temp,
    })
}

async fn print_agents(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let snapshot = copy_snapshot(db_path)?;

    let db = RhoDb::open(&snapshot.path);
    migrate_snapshot(&db).await;
    let read = db.read();
    let mut agents = read.list_agents();
    agents.sort_by_key(|(id, _)| *id);

    let mut output = String::new();
    writeln!(output, "source: {}", snapshot.source.display())?;
    writeln!(output, "snapshot: {}", snapshot.path.display())?;
    writeln!(output, "agents: {}", agents.len())?;
    for (agent_id, agent) in agents {
        writeln!(output)?;
        writeln!(output, "{agent_id:?}")?;
        if let Some(name) = &agent.display_name {
            writeln!(output, "  name: {name}")?;
        }
        writeln!(output, "  status: {}", status_name(agent.status))?;
        writeln!(
            output,
            "  disposition: {}",
            disposition_name(agent.disposition)
        )?;
        writeln!(output, "  last_user_message: {}", agent.last_user_message.0)?;
        writeln!(output, "  mode: {}", config_name(agent.config()))?;
        writeln!(
            output,
            "  workdirs: {}",
            agent
                .workdirs
                .iter()
                .map(workspace_name)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        match agent.runtime {
            AgentRuntime::Rho { prompt_cache_key } => {
                writeln!(output, "  runtime: rho")?;
                writeln!(output, "  prompt_cache_key: {prompt_cache_key:?}")?;
            }
            AgentRuntime::Claude { session_id } => {
                writeln!(output, "  runtime: claude")?;
                writeln!(output, "  session_id: {session_id}")?;
                match rho_claude::find_session_transcript(
                    session_id,
                    agent.primary_workdir().repo(),
                )
                .await?
                {
                    Some(path) => writeln!(output, "  transcript: {path}")?,
                    None => writeln!(output, "  transcript: <missing>")?,
                }
            }
        }
    }
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

async fn print_context(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let snapshot = copy_snapshot(db_path)?;
    let db = RhoDb::open(&snapshot.path);
    migrate_snapshot(&db).await;
    let read = db.read();
    let mut agents = read.list_agents();
    agents.sort_by_key(|(id, _)| *id);

    let mut output = String::new();
    writeln!(output, "source: {}", snapshot.source.display())?;
    writeln!(output, "agents: {}", agents.len())?;
    for (agent_id, agent) in agents {
        writeln!(output)?;
        writeln!(
            output,
            "{agent_id:?} ({})",
            agent.display_name.as_deref().unwrap_or("unnamed")
        )?;
        match agent.runtime {
            AgentRuntime::Rho { .. } => {
                let (_, events) = read.agent_events(agent_id);
                let mut context_used = None;
                let mut responses = 0usize;
                for event in &events {
                    if let rho_agent::AgentEvent::InferenceResponse {
                        context_used: response_context_used,
                        ..
                    } = event
                    {
                        responses += 1;
                        if response_context_used.is_some() {
                            context_used = *response_context_used;
                        }
                    }
                }
                writeln!(output, "  runtime: rho")?;
                writeln!(
                    output,
                    "  events: {} ({responses} inference responses)",
                    events.len()
                )?;
                writeln!(output, "  restored context_used: {context_used:?}")?;
            }
            AgentRuntime::Claude { session_id } => {
                writeln!(output, "  runtime: claude")?;
                writeln!(output, "  session_id: {session_id}")?;
                let transcript =
                    rho_claude::find_session_transcript(session_id, agent.primary_workdir().repo())
                        .await?;
                let Some(transcript) = transcript else {
                    writeln!(output, "  transcript: <missing>")?;
                    continue;
                };
                writeln!(output, "  transcript: {transcript}")?;
                let messages = rho_claude::read_session_messages_by_id(
                    session_id,
                    agent.primary_workdir().repo(),
                    rho_claude::SessionMessagesOptions::default(),
                )
                .await?;
                writeln!(output, "  messages: {}", messages.len())?;
                match rho_claude::last_assistant_usage(&messages) {
                    Some(usage) => {
                        writeln!(output, "  last assistant usage:")?;
                        writeln!(output, "    input_tokens: {:?}", usage.input_tokens)?;
                        writeln!(
                            output,
                            "    cache_creation_input_tokens: {:?}",
                            usage.cache_creation_input_tokens
                        )?;
                        writeln!(
                            output,
                            "    cache_read_input_tokens: {:?}",
                            usage.cache_read_input_tokens
                        )?;
                        writeln!(output, "    output_tokens: {:?}", usage.output_tokens)?;
                        writeln!(
                            output,
                            "  restored context_used: Some({})",
                            usage.context_total()
                        )?;
                    }
                    None => {
                        writeln!(output, "  last assistant usage: <none>")?;
                        writeln!(output, "  restored context_used: None")?;
                    }
                }
            }
        }
    }
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

async fn test_migration(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let snapshot = copy_snapshot(db_path)?;
    let db = RhoDb::open(&snapshot.path);
    migrate_snapshot(&db).await;

    let read = db.read();
    let agents = read.list_agents();
    let mut events = 0usize;
    for (agent_id, _) in &agents {
        events += read.agent_events(*agent_id).1.len();
    }

    let mut output = String::new();
    writeln!(output, "source: {}", snapshot.source.display())?;
    writeln!(output, "snapshot: {}", snapshot.path.display())?;
    writeln!(output, "migration on copied database: ok")?;
    if let Some(point) = rho_agent::db::migration_recovery_point(&db) {
        writeln!(
            output,
            "recovery savepoint: {} ({} -> {}, created {})",
            point.savepoint_id, point.from_format, point.to_format, point.created_at.0
        )?;
    }
    writeln!(output, "agents decoded: {}", agents.len())?;
    writeln!(output, "events decoded: {events}")?;
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

async fn migrate_snapshot(db: &RhoDb) {
    rho_agent::db::prepare_agent_db_migration(db).await;
    let mut write = db.write().await;
    write.init_agent_tables();
    write.commit();
}

fn disposition_name(disposition: rho_agent::db::AgentDisposition) -> String {
    use rho_agent::db::AgentDisposition;
    match disposition {
        AgentDisposition::Pending => "pending".to_owned(),
        AgentDisposition::Done => "done".to_owned(),
        AgentDisposition::Snoozed { until } => format!("snoozed until {}", until.0),
        AgentDisposition::Hidden => "hidden".to_owned(),
    }
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Normal => "normal",
        Status::Pinned => "pinned",
    }
}

fn config_name(config: rho_agent::db::AgentRole) -> String {
    use rho_agent::db::{AgentRole, EngineerIntelligence};
    match config {
        AgentRole::PM | AgentRole::WorkflowPM { .. } => "pm".to_owned(),
        AgentRole::Advisor { intelligence } => format!(
            "advisor {}",
            match intelligence {
                rho_agent::db::AdvisorIntelligence::Medium => "medium",
                rho_agent::db::AdvisorIntelligence::High => "high",
            }
        ),
        AgentRole::Engineer { intelligence } | AgentRole::WorkflowEngineer { intelligence, .. } => {
            let intelligence = match intelligence {
                EngineerIntelligence::Low => "low",
                EngineerIntelligence::Medium => "medium",
                EngineerIntelligence::High => "high",
                EngineerIntelligence::Ultra => "ultra",
            };
            format!("engineer {intelligence}")
        }
    }
}

fn workspace_name(workspace: &WorkspaceInfo) -> String {
    match workspace {
        WorkspaceInfo::UserCheckout { repo } => format!("user-checkout {repo}"),
        WorkspaceInfo::Workspace { repo, id } => format!("workspace {} in {repo}", id.encoded()),
    }
}

#[cfg(test)]
mod render_prompt_tests {
    use super::*;

    #[test]
    fn parses_render_prompt_roles() {
        assert_eq!(parse_role("eng").unwrap(), AgentRole::default());
        assert_eq!(
            parse_role("advisor-high").unwrap(),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::High
            }
        );
        assert!(parse_role("ultra").is_err());
    }
}
