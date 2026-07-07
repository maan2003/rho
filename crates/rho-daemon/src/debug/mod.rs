use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::PathBuf;

use anyhow::Context as _;
use rho_agent::db::{
    AgentMode, AgentReadTxnExt as _, AgentRuntime, AgentWriteTxnExt as _, DeepEffort, FableEffort,
    OpusEffort, Status,
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
}

pub async fn run(args: DebugArgs) -> anyhow::Result<()> {
    match args.command {
        DebugCommand::Agents => print_agents(args.db_path).await,
        DebugCommand::Migrate => test_migration(args.db_path).await,
        DebugCommand::Context => print_context(args.db_path).await,
    }
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
        writeln!(output, "  mode: {}", mode_name(agent.mode))?;
        writeln!(output, "  workspace: {}", workspace_name(&agent.workspace))?;
        match agent.runtime {
            AgentRuntime::Rho { prompt_cache_key } => {
                writeln!(output, "  runtime: rho")?;
                writeln!(output, "  prompt_cache_key: {prompt_cache_key:?}")?;
            }
            AgentRuntime::Claude { session_id } => {
                writeln!(output, "  runtime: claude")?;
                writeln!(output, "  session_id: {session_id}")?;
                match rho_claude::find_session_transcript(session_id, agent.workspace.repo())
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
                    rho_claude::find_session_transcript(session_id, agent.workspace.repo()).await?;
                let Some(transcript) = transcript else {
                    writeln!(output, "  transcript: <missing>")?;
                    continue;
                };
                writeln!(output, "  transcript: {transcript}")?;
                let messages = rho_claude::read_session_messages_by_id(
                    session_id,
                    agent.workspace.repo(),
                    rho_claude::SessionMessagesOptions::default(),
                )
                .await?;
                writeln!(output, "  messages: {}", messages.len())?;
                match rho_agent::claude::transcript_messages_to_context(&messages) {
                    Ok(blocks) => writeln!(output, "  restored blocks: {}", blocks.len())?,
                    Err(error) => writeln!(output, "  restored blocks: ERROR: {error:#}")?,
                }
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
    let mut write = db.write().await;
    write.init_agent_tables();
    write.commit();

    let read = db.read();
    let agents = read.list_agents();

    let mut output = String::new();
    writeln!(output, "source: {}", snapshot.source.display())?;
    writeln!(output, "snapshot: {}", snapshot.path.display())?;
    writeln!(output, "migration on copied database: ok")?;
    writeln!(output, "agents decoded: {}", agents.len())?;
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Normal => "normal",
        Status::Pinned => "pinned",
        Status::Archived => "archived",
    }
}

fn mode_name(mode: AgentMode) -> String {
    match mode {
        AgentMode::Deep(config) => {
            let effort = config.effort;
            let fast_mode = config.fast_mode;
            let fast = if fast_mode { " ⚡" } else { "" };
            format!("deep{fast} {}", deep_effort_name(effort))
        }
        AgentMode::Fable { effort } => format!("fable {}", fable_effort_name(effort)),
        AgentMode::Opus { effort } => format!("opus {}", opus_effort_name(effort)),
    }
}

fn deep_effort_name(effort: DeepEffort) -> &'static str {
    match effort {
        DeepEffort::Low => "low",
        DeepEffort::Medium => "medium",
        DeepEffort::Xhigh => "xhigh",
    }
}

fn fable_effort_name(effort: FableEffort) -> &'static str {
    match effort {
        FableEffort::Medium => "medium",
        FableEffort::Xhigh => "xhigh",
    }
}

fn opus_effort_name(effort: OpusEffort) -> &'static str {
    match effort {
        OpusEffort::Medium => "medium",
        OpusEffort::Xhigh => "xhigh",
    }
}

fn workspace_name(workspace: &WorkspaceInfo) -> String {
    match workspace {
        WorkspaceInfo::UserCheckout { repo } => format!("user-checkout {repo}"),
        WorkspaceInfo::Workspace { repo, id } => format!("workspace {} in {repo}", id.encoded()),
    }
}
