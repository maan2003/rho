use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::PathBuf;

use anyhow::Context as _;
use rho_agent::db::{
    AgentMode, AgentReadTxnExt as _, AgentRuntime, DeepEffort, FableEffort, Status,
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
}

pub async fn run(args: DebugArgs) -> anyhow::Result<()> {
    match args.command {
        DebugCommand::Agents => print_agents(args.db_path).await,
    }
}

async fn print_agents(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let source = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    let temp = tempfile::tempdir().context("create debug db snapshot tempdir")?;
    let snapshot = temp.path().join("rho.redb");
    std::fs::copy(&source, &snapshot)
        .with_context(|| format!("copy rho db snapshot from {}", source.display()))?;

    let db = RhoDb::open(&snapshot);
    let read = db.read();
    let mut agents = read.list_agents();
    agents.sort_by_key(|(id, _)| *id);

    let mut output = String::new();
    writeln!(output, "source: {}", source.display())?;
    writeln!(output, "snapshot: {}", snapshot.display())?;
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

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Normal => "normal",
        Status::Pinned => "pinned",
        Status::Archived => "archived",
    }
}

fn mode_name(mode: AgentMode) -> String {
    match mode {
        AgentMode::Deep { effort } => format!("deep {}", deep_effort_name(effort)),
        AgentMode::Fable { effort } => format!("fable {}", fable_effort_name(effort)),
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

fn workspace_name(workspace: &WorkspaceInfo) -> String {
    match workspace {
        WorkspaceInfo::UserCheckout { repo } => format!("user-checkout {repo}"),
        WorkspaceInfo::Workspace { repo, id } => format!("workspace {} in {repo}", id.encoded()),
    }
}
