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

pub fn run(args: DebugArgs) -> anyhow::Result<()> {
    match args.command {
        DebugCommand::Agents => print_agents(args.db_path),
    }
}

fn print_agents(db_path: Option<PathBuf>) -> anyhow::Result<()> {
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

    let mut stdout = io::stdout().lock();
    writeln!(stdout, "source: {}", source.display())?;
    writeln!(stdout, "snapshot: {}", snapshot.display())?;
    writeln!(stdout, "agents: {}", agents.len())?;
    for (agent_id, agent) in agents {
        writeln!(stdout)?;
        writeln!(stdout, "{agent_id:?}")?;
        if let Some(name) = &agent.display_name {
            writeln!(stdout, "  name: {name}")?;
        }
        writeln!(stdout, "  status: {}", status_name(agent.status))?;
        writeln!(stdout, "  mode: {}", mode_name(agent.mode))?;
        writeln!(stdout, "  workspace: {}", workspace_name(&agent.workspace))?;
        match agent.runtime {
            AgentRuntime::Rho { prompt_cache_key } => {
                writeln!(stdout, "  runtime: rho")?;
                writeln!(stdout, "  prompt_cache_key: {prompt_cache_key:?}")?;
            }
            AgentRuntime::Claude {
                session_id,
                transcript_path,
            } => {
                writeln!(stdout, "  runtime: claude")?;
                writeln!(stdout, "  session_id: {session_id}")?;
                match transcript_path {
                    Some(path) => writeln!(stdout, "  transcript_path: {path}")?,
                    None => writeln!(stdout, "  transcript_path: <none>")?,
                }
            }
        }
    }
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
        WorkspaceInfo::Workspace { repo, name } => format!("workspace {name} in {repo}"),
    }
}
