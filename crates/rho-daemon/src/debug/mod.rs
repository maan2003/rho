use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rho_agent::db::{
    AdvisorIntelligence, AgentReadTxnExt as _, AgentRole, AgentRuntime, EngineerIntelligence,
};
use rho_db::RhoDb;
use rho_fs_view::WorkspaceInfo;
use rho_inference::Inference;

use crate::default_db_path;

#[derive(Clone, Debug, clap::Args)]
pub struct DebugArgs {
    /// Source database path. Defaults to rho's normal daemon database,
    /// which the daemon must not be holding; a named one is taken to be
    /// nobody's and is read or written without that check.
    #[arg(long = "db-path")]
    db_path: Option<PathBuf>,

    /// The Claude config directory whose transcripts to read, with accounts
    /// beside it as `<dir>-accounts`. Defaults to the user's. A database
    /// from somewhere else wants the transcripts from that somewhere else:
    /// name it, or the reading is of the user's transcripts under another
    /// store's session ids.
    #[arg(long = "claude-config-dir", value_name = "DIR")]
    claude_config_dir: Option<camino::Utf8PathBuf>,

    #[command(subcommand)]
    command: DebugCommand,
}

#[derive(Clone, Debug, clap::Subcommand)]
enum DebugCommand {
    /// Snapshot the database and print persisted agent records.
    Agents,
    /// Snapshot the database and run pending migrations on the copy.
    Migrate,
    /// Put the real database back as it was before its last migration,
    /// from the savepoint taken then. Stop the daemon first.
    Rollback,
    /// List the recovery savepoints the real database holds, and the
    /// migration each was taken for. Stop the daemon first.
    Savepoints,
    /// Drop the savepoints no migration recorded: leftovers of older
    /// builds that keep freed pages from being reused. Stop the daemon
    /// first.
    DropStaleSavepoints,
    /// Drop the savepoints recorded for migrations once they are verified,
    /// so nothing pins the pages they freed. Stop the daemon first.
    ForgetSavepoints,
    /// Rewrite the real database file without the pages nothing refers to
    /// any more. Needs every savepoint gone (`drop-stale-savepoints` after
    /// the last migration is done). Stop the daemon first.
    Compact,
    /// Print bytes stored per table and pages allocated overall for the
    /// real database. Stop the daemon first.
    Stats,
    /// Snapshot the database and print the context usage each agent would
    /// restore on load (event log for Rho agents, session transcript for
    /// Claude agents).
    Context,
    /// Ask the running daemon to move an agent that predates worksets
    /// into one: clone its repository's origin, check out the old jj
    /// workspace's parent commit (detached) with the working copy's
    /// changes staged on top, and record the new place at the tail of the
    /// agent's log. The old workspace is left as it is.
    MigrateAgent {
        /// The agent, as `eng-xxxx` or a bare id prefix.
        agent: String,
        /// What to clone; the repository's `origin` remote by default.
        #[arg(long)]
        origin: Option<String>,
        /// How the agent sees the filesystem from now on. Exposed by
        /// default: that is what a jj workspace on the host was.
        #[arg(long, value_enum, default_value_t = crate::WorksetModeArg::Exposed)]
        mode: crate::WorksetModeArg,
        /// The daemon's socket; the usual one by default.
        #[arg(long)]
        socket_path: Option<PathBuf>,
    },
    /// Render the system prompt and top-level model-facing tools for a role.
    RenderPrompt {
        /// Role text: eng, eng-mini, eng-low, eng-cheap, eng-high,
        /// eng-ultra, eng-alt, eng-gemini, pm, advisor,
        /// advisor-cheap, or advisor-high.
        role: String,
    },
}

pub async fn run(args: DebugArgs) -> anyhow::Result<()> {
    // Resolved once, here: the readers below are handed a path.
    let claude = match args.claude_config_dir.clone() {
        Some(dir) => rho_claude::accounts::ClaudePaths::at(dir),
        None => rho_claude::accounts::ClaudePaths::from_env()?,
    };
    match args.command {
        DebugCommand::Agents => print_agents(args.db_path, &claude).await,
        DebugCommand::Migrate => test_migration(args.db_path).await,
        DebugCommand::Rollback => rollback(args.db_path).await,
        DebugCommand::Savepoints => savepoints(args.db_path).await,
        DebugCommand::DropStaleSavepoints => drop_stale_savepoints(args.db_path).await,
        DebugCommand::Compact => compact(args.db_path),
        DebugCommand::ForgetSavepoints => forget_savepoints(args.db_path).await,
        DebugCommand::Stats => stats(args.db_path),
        DebugCommand::Context => print_context(args.db_path, &claude).await,
        DebugCommand::MigrateAgent {
            agent,
            origin,
            mode,
            socket_path,
        } => migrate_agent(socket_path, agent, origin, mode.into()).await,
        DebugCommand::RenderPrompt { role } => render_prompt(&role).await,
    }
}

/// The daemon's lock, held for as long as the file lives; an error when
/// the daemon has it.
fn hold_daemon_lock(daemon_lock: &Path, what: &str) -> anyhow::Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(daemon_lock)
        .with_context(|| format!("open daemon lock {}", daemon_lock.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            anyhow::bail!(
                "refusing to {what} while the rho daemon is running; stop the daemon first"
            );
        }
        return Err(error).with_context(|| format!("lock {}", daemon_lock.display()));
    }
    Ok(lock)
}

async fn render_prompt(role: &str) -> anyhow::Result<()> {
    let role = parse_role(role)?;
    let cwd = std::env::current_dir().context("read current directory")?;
    // A rendering runs on the invoking directory adopted as a workset. It
    // never runs a command, so no namespace is built and no store server
    // is needed; the state root only has to exist.
    let worksets = rho_fs_view::Worksets::open(
        rho_fs_view::Worksets::default_root()?,
        rho_fs_view::UserEnvironment::new(std::env::vars_os().collect()),
        Default::default(),
        rho_fs_view::StoreService::None,
    )
    .await?;
    let view = worksets.adopt(&cwd)?.enter(
        rho_fs_view::Mode::View {
            home_skeleton: None,
        },
        camino::Utf8Path::new(rho_fs_view::MOUNT_ROOT),
    )?;
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
        "eng-mini" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        },
        "eng-low" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
        },
        "eng-cheap" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        },
        "eng-high" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        },
        "eng-ultra" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
        },
        "eng-alt" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Alt,
        },
        "eng-gemini" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Gemini,
        },
        "advisor" => AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Medium,
        },
        "advisor-cheap" => AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Cheap,
        },
        "advisor-high" => AgentRole::Advisor {
            intelligence: AdvisorIntelligence::High,
        },
        _ => anyhow::bail!(
            "unknown role `{text}`; use eng, eng-mini, eng-low, eng-cheap, eng-high, eng-ultra, eng-alt, eng-gemini, pm, advisor, advisor-cheap, or advisor-high"
        ),
    })
}

#[derive(Debug)]
struct Snapshot {
    source: PathBuf,
    path: PathBuf,
    _temp: tempfile::TempDir,
}

fn copy_snapshot(db_path: Option<PathBuf>) -> anyhow::Result<Snapshot> {
    let source = match db_path {
        Some(source) => return copy_snapshot_unlocked(&source),
        None => default_db_path().context("resolve rho db path")?,
    };
    let paths = rho_ui_proto::RuntimePaths::from_env()?;
    std::fs::create_dir_all(paths.directory()).context("create rho runtime directory")?;
    copy_snapshot_from(&source, &paths.daemon_lock())
}

/// The name every debug copy carries, so a stray one says who made it.
const SNAPSHOT_PREFIX: &str = "rho-debug-snapshot-";

/// Where a copy of the store goes: beside the store itself, never the
/// system temp directory. The copy is as big as the store, and
/// `std::env::temp_dir()` may be a different and smaller disk; beside
/// the store it is on a volume that already holds something that size.
fn snapshot_dir(source: &Path) -> anyhow::Result<PathBuf> {
    let dir = source
        .parent()
        .context("the rho db path has no directory")?
        .join("debug-snapshots");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create debug snapshot directory {}", dir.display()))?;
    report_leftover_snapshots(&dir);
    Ok(dir)
}

/// Copies an earlier run left behind. `TempDir` removes its own on the
/// way out, panic included, but nothing runs when the process is killed.
/// One that is still here is a dead item, and a dead item is a question
/// rather than a deletion: this names it and goes on, and the user is the
/// one who decides it is rubbish. A copy another run is reading right now
/// is named too, which is the same answer said early.
fn leftover_snapshots(dir: &Path) -> Vec<(PathBuf, std::time::Duration)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut leftovers = Vec::new();
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(SNAPSHOT_PREFIX)
        {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|data| data.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .unwrap_or_default();
        leftovers.push((entry.path(), age));
    }
    leftovers.sort();
    leftovers
}

fn report_leftover_snapshots(dir: &Path) {
    for (path, age) in leftover_snapshots(dir) {
        eprintln!(
            "a debug db copy from an earlier run is still here: {} ({}); it is yours to delete",
            path.display(),
            describe_age(age)
        );
    }
}

fn describe_age(age: std::time::Duration) -> String {
    let hours = age.as_secs_f64() / 3600.0;
    if hours < 1.0 {
        format!("{:.0} minutes old", age.as_secs_f64() / 60.0)
    } else if hours < 48.0 {
        format!("{hours:.1} hours old")
    } else {
        format!("{:.1} days old", hours / 24.0)
    }
}

fn copy_snapshot_from(source: &Path, daemon_lock: &Path) -> anyhow::Result<Snapshot> {
    let _lock = hold_daemon_lock(daemon_lock, &format!("copy {}", source.display()))?;
    copy_snapshot_unlocked(source)
}

fn copy_snapshot_unlocked(source: &Path) -> anyhow::Result<Snapshot> {
    let temp = tempfile::Builder::new()
        .prefix(SNAPSHOT_PREFIX)
        .tempdir_in(snapshot_dir(source)?)
        .context("create debug db snapshot directory")?;
    let snapshot = temp.path().join("rho.redb");
    std::fs::copy(source, &snapshot)
        .with_context(|| format!("copy rho db snapshot from {}", source.display()))?;
    Ok(Snapshot {
        source: source.to_owned(),
        path: snapshot,
        _temp: temp,
    })
}

async fn print_agents(
    db_path: Option<PathBuf>,
    claude: &rho_claude::accounts::ClaudePaths,
) -> anyhow::Result<()> {
    let snapshot = copy_snapshot(db_path)?;

    let db = RhoDb::open(&snapshot.path);
    migrate_snapshot(&db).await?;
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
        if let Some(name) = agent.title() {
            writeln!(output, "  name: {name}")?;
        }
        writeln!(output, "  mode: {}", config_name(agent.config()))?;
        writeln!(
            output,
            "  log: next {:?}, {} rows visible",
            agent.next,
            read.agent_events(agent_id).1.len()
        )?;
        writeln!(
            output,
            "  workdirs: {}",
            agent
                .config
                .workdirs
                .iter()
                .map(workspace_name)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        match agent.config.runtime {
            AgentRuntime::Rho { prompt_cache_key } => {
                writeln!(output, "  runtime: rho")?;
                writeln!(output, "  prompt_cache_key: {prompt_cache_key:?}")?;
            }
            AgentRuntime::Claude { session_id } => {
                writeln!(output, "  runtime: claude")?;
                writeln!(output, "  session_id: {session_id}")?;
                match rho_claude::find_session_transcript(
                    &claude.projects(),
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

async fn print_context(
    db_path: Option<PathBuf>,
    claude: &rho_claude::accounts::ClaudePaths,
) -> anyhow::Result<()> {
    let snapshot = copy_snapshot(db_path)?;
    let db = RhoDb::open(&snapshot.path);
    migrate_snapshot(&db).await?;
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
            agent.title().unwrap_or("unnamed")
        )?;
        match agent.config.runtime {
            AgentRuntime::Rho { .. } => {
                let (_, events) = read.agent_events(agent_id);
                let mut context_used = None;
                let mut responses = 0usize;
                for event in &events {
                    if let rho_agent::AgentEvent::Replied {
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
                let transcript = rho_claude::find_session_transcript(
                    &claude.projects(),
                    session_id,
                    agent.primary_workdir().repo(),
                )
                .await?;
                let Some(transcript) = transcript else {
                    writeln!(output, "  transcript: <missing>")?;
                    continue;
                };
                writeln!(output, "  transcript: {transcript}")?;
                let messages = rho_claude::read_session_messages_by_id(
                    &claude.projects(),
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
    migrate_snapshot(&db).await?;

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
    writeln!(output, "agents decoded: {}", agents.len())?;
    writeln!(output, "events decoded: {events}")?;
    // The fused migration drops the old layout; a migration check is the
    // place that says whether any of it is still there.
    for table in [
        "projects",
        "view_config",
        "agent_heads",
        "agent_events",
        "lineage_parents",
        "agent_story",
        "agent_story_source",
        "agent_attention_until_slice_b",
    ] {
        writeln!(
            output,
            "table {table}: {}",
            if read.has_table(table) {
                "present"
            } else {
                "dropped"
            }
        )?;
    }
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

async fn migrate_snapshot(db: &RhoDb) -> anyhow::Result<()> {
    Inference::migrate(db).await?;
    rho_agent::db::prepare(db).await;
    Ok(())
}

async fn savepoints(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    let db = RhoDb::open(&path);
    for (id, hop) in rho_agent::db::savepoints(&db).await {
        println!(
            "savepoint {id}: {}",
            hop.as_deref().unwrap_or("not recorded for a migration")
        );
    }
    Ok(())
}

async fn drop_stale_savepoints(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    let db = RhoDb::open(&path);
    let dropped = rho_agent::db::drop_stale_savepoints(&db).await;
    println!(
        "{}: dropped {} stale savepoint(s) {dropped:?}",
        path.display(),
        dropped.len()
    );
    Ok(())
}

async fn forget_savepoints(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    let db = RhoDb::open(&path);
    let dropped = rho_agent::db::forget_savepoints(&db).await;
    println!(
        "{}: forgot {} migration savepoint(s) {dropped:?}",
        path.display(),
        dropped.len()
    );
    Ok(())
}

fn stats(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    RhoDb::print_stats(&path)
}

fn compact(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    let started = std::time::Instant::now();
    let (before, after) = RhoDb::compact(&path)?;
    println!(
        "{}: {} -> {} bytes in {:.1?}",
        path.display(),
        before,
        after,
        started.elapsed()
    );
    Ok(())
}

async fn rollback(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    let db = RhoDb::open(&path);
    let hop = rho_agent::db::rollback(&db).await?;
    println!("{}: migration {hop} undone", path.display());
    Ok(())
}

/// Sends the migration to the daemon and prints its account of it.
async fn migrate_agent(
    socket_path: Option<PathBuf>,
    agent: String,
    origin: Option<String>,
    mode: rho_fs_view::WorksetMode,
) -> anyhow::Result<()> {
    let paths = rho_ui_proto::RuntimePaths::resolve(socket_path)?;
    let mut client = rho_ui_proto::client::Client::connect(paths.socket())
        .await
        .with_context(|| format!("connect to rho daemon at {}", paths.socket().display()))?;
    client
        .send(&rho_ui_proto::ClientMessage::MigrateAgent {
            agent,
            origin,
            mode,
        })
        .await?;
    match client.recv().await? {
        rho_ui_proto::ServerMessage::AgentMigrated { report } => {
            print!("{report}");
            Ok(())
        }
        rho_ui_proto::ServerMessage::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected reply: {other:?}"),
    }
}

fn config_name(config: rho_agent::db::AgentRole) -> String {
    use rho_agent::db::{AgentRole, EngineerIntelligence};
    match config {
        AgentRole::Advisor { intelligence } => format!(
            "advisor {}",
            match intelligence {
                rho_agent::db::AdvisorIntelligence::Medium => "medium",
                rho_agent::db::AdvisorIntelligence::High => "high",
                rho_agent::db::AdvisorIntelligence::Cheap => "cheap",
            }
        ),
        AgentRole::Engineer { intelligence } => {
            let intelligence = match intelligence {
                EngineerIntelligence::Mini => "mini",
                EngineerIntelligence::Low => "low",
                EngineerIntelligence::Cheap => "cheap",
                EngineerIntelligence::Medium => "medium",
                EngineerIntelligence::High => "high",
                EngineerIntelligence::Ultra => "ultra",
                EngineerIntelligence::Alt => "alt",
                EngineerIntelligence::Gemini => "gemini",
            };
            format!("engineer {intelligence}")
        }
    }
}

fn workspace_name(workspace: &WorkspaceInfo) -> String {
    match workspace {
        WorkspaceInfo::UserCheckout { repo } => format!("user-checkout {repo}"),
        WorkspaceInfo::Workspace { repo, id } => {
            format!("workspace ws-{} in {repo}", id.encoded())
        }
        WorkspaceInfo::Sandbox { repo, id } => {
            format!("sandbox ws-{} from {repo}", id.encoded())
        }
        WorkspaceInfo::Workset { workset, cwd, .. } => format!("{cwd} in workset {workset}"),
    }
}

#[cfg(test)]
mod render_prompt_tests {
    use super::*;

    #[test]
    fn parses_render_prompt_roles() {
        assert_eq!(parse_role("eng").unwrap(), AgentRole::default());
        assert_eq!(
            parse_role("eng-gemini").unwrap(),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Gemini,
            }
        );
        assert_eq!(
            parse_role("advisor-high").unwrap(),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::High
            }
        );
        assert!(parse_role("ultra").is_err());
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn a_live_daemon_lock_refuses_the_redb_copy() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("rho.redb");
        drop(RhoDb::open(&source));
        let lock_path = directory.path().join("daemon.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);

        let error = copy_snapshot_from(&source, &lock_path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("while the rho daemon is running")
        );

        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(lock);
        let snapshot = copy_snapshot_from(&source, &lock_path).unwrap();
        drop(RhoDb::open(&snapshot.path));
    }

    /// The copy lands beside the store, which is what keeps it off the
    /// system temp directory and its own disk. (This test's own store is
    /// under a temp directory, so "beside the store" is the whole of what
    /// can be asserted here.)
    #[test]
    fn the_copy_is_taken_beside_the_store_and_is_gone_when_the_run_ends() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("rho.redb");
        drop(RhoDb::open(&source));
        let lock_path = directory.path().join("daemon.lock");

        let snapshot = copy_snapshot_from(&source, &lock_path).unwrap();
        let held = snapshot.path.clone();
        assert!(
            held.starts_with(directory.path().join("debug-snapshots")),
            "the copy is beside the store: {}",
            held.display()
        );

        drop(snapshot);
        assert!(
            !held.exists(),
            "the copy goes when the run that took it does"
        );
    }

    /// What a killed run left behind is named, not removed. A copy the
    /// tool did not take away is a dead item, and a dead item is the
    /// user's to decide about.
    #[test]
    fn a_copy_from_an_earlier_run_is_named_and_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join("debug-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let killed = dir.join(format!("{SNAPSHOT_PREFIX}killed"));
        std::fs::create_dir(&killed).unwrap();
        std::fs::write(killed.join("rho.redb"), b"x").unwrap();
        let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 60 * 60);
        std::fs::File::open(&killed)
            .unwrap()
            .set_modified(two_days)
            .unwrap();

        let leftovers = leftover_snapshots(&dir);

        assert_eq!(
            leftovers.iter().map(|(path, _)| path).collect::<Vec<_>>(),
            vec![&killed],
            "the copy is named"
        );
        assert!(leftovers[0].1.as_secs() >= 47 * 60 * 60, "with its age");
        assert!(killed.exists(), "and it is still there afterwards");
    }
}
