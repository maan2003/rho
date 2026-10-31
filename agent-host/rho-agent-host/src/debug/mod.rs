use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rho_agent::record::Runtime;
use rho_agent::store::StoreRead as _;
use rho_agent_hosts::protocol::DebugReport;
use rho_db::RhoDb;
use rho_inference::Accounts;

use crate::default_db_path;

#[derive(Clone, Debug, clap::Args)]
pub struct DebugArgs {
    /// Source database path. Defaults to rho's normal agent host database: a
    /// running host supplies reports and handles deletion/compaction itself.
    /// Other direct writes need the host stopped. A named one is taken to be
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
    /// Print persisted agent records.
    Agents,
    /// Snapshot the database and run pending migrations on the copy.
    Migrate,
    /// Put the real database back as it was before its last migration,
    /// from the savepoint taken then. Stop the agent host first.
    Rollback,
    /// List the recovery savepoints the real database holds, and the
    /// migration each was taken for. Stop the agent host first.
    Savepoints,
    /// Drop the savepoints no migration recorded: leftovers of older
    /// builds that keep freed pages from being reused. Stop the agent host
    /// first.
    DropStaleSavepoints,
    /// Drop the savepoints recorded for migrations once they are verified,
    /// so nothing pins the pages they freed. The running agent host does it;
    /// a named database is opened directly.
    ForgetSavepoints,
    /// Reclaim unused database space. Needs every savepoint gone (see
    /// `savepoints`, `drop-stale-savepoints` and `forget-savepoints`).
    /// The running host compacts its own database. A named --db-path
    /// is compacted offline instead.
    Compact {
        #[arg(long)]
        socket_path: Option<PathBuf>,
    },
    /// Permanently delete worksets: their logs, agents, Claude sessions and
    /// directories. Mail stays. Runs through the host, which stops their
    /// processes first. Use --socket-path to select a host, not --db-path.
    DeleteWorksets {
        /// Workset ids.
        #[arg(required = true)]
        worksets: Vec<String>,
        /// Host socket; defaults to $RHO_SOCKET_PATH, then the user's host.
        #[arg(long)]
        socket_path: Option<PathBuf>,
    },
    /// Print bytes stored per table and pages allocated overall for the
    /// real database. Stop the agent host first.
    Stats,
    /// Print the context usage each agent would restore on load (event log for
    /// Rho agents, session transcript for Claude agents).
    Context,
    /// Print one agent's persisted events, each cut to `--max-chars`.
    Transcript {
        /// Full agent id.
        agent: String,
        #[arg(long = "max-chars", default_value_t = 3000)]
        max_chars: usize,
    },
    /// Create a workset on a running agent host, run `code` in it, and
    /// print the workset, the agent the message went to, and the output.
    CreateWorkset {
        /// A host directory to work in as it is; without it, an isolated
        /// workset with its own `/src`.
        #[arg(long)]
        cwd: Option<camino::Utf8PathBuf>,
        /// The agent host's socket; defaults to `$RHO_SOCKET_PATH`, then the
        /// user's agent host.
        #[arg(long = "socket-path")]
        socket_path: Option<PathBuf>,
        /// Python run once in the new workset's namespace.
        code: String,
        /// The user's first mail, to the first agent `code` creates.
        message: Option<String>,
    },
    /// Mail an agent, as the user would, on a running agent host.
    SendMessage {
        /// Full agent id, as in `$RHO_AGENT_ID`.
        agent: String,
        /// The agent host's socket; defaults to `$RHO_SOCKET_PATH`, then the
        /// user's agent host.
        #[arg(long = "socket-path")]
        socket_path: Option<PathBuf>,
        text: String,
    },
}

pub async fn run(args: DebugArgs) -> anyhow::Result<()> {
    // Resolved once, here: the readers below are handed a path.
    let claude = match args.claude_config_dir.clone() {
        Some(dir) => rho_claude::accounts::ClaudePaths::at(dir),
        None => rho_claude::accounts::ClaudePaths::from_env()?,
    };
    match args.command {
        DebugCommand::Agents => print_report(args.db_path, &claude, DebugReport::Agents).await,
        DebugCommand::Migrate => test_migration(args.db_path).await,
        DebugCommand::Rollback => rollback(args.db_path).await,
        DebugCommand::Savepoints => savepoints(args.db_path).await,
        DebugCommand::DropStaleSavepoints => drop_stale_savepoints(args.db_path).await,
        DebugCommand::Compact { socket_path } => compact(args.db_path, socket_path).await,
        DebugCommand::ForgetSavepoints => forget_savepoints(args.db_path).await,
        DebugCommand::Stats => stats(args.db_path),
        DebugCommand::DeleteWorksets {
            worksets,
            socket_path,
        } => {
            anyhow::ensure!(
                args.db_path.is_none(),
                "deletion runs through the host; select it with --socket-path, not --db-path"
            );
            delete_worksets(socket_path, &worksets).await
        }
        DebugCommand::Context => print_report(args.db_path, &claude, DebugReport::Context).await,
        DebugCommand::Transcript { agent, max_chars } => {
            let report = DebugReport::Transcript {
                agent,
                max_chars: max_chars as u64,
            };
            print_report(args.db_path, &claude, report).await
        }
        DebugCommand::CreateWorkset {
            cwd,
            socket_path,
            code,
            message,
        } => {
            let call = rho_agents_client::protocol::rho2::CreateWorkset {
                kind: match cwd {
                    Some(cwd) => rho_agent_types::WorksetKind::Host { cwd },
                    None => rho_agent_types::WorksetKind::Isolated,
                },
                code,
                message: message.map(|text| vec![rho_agent_types::ContentPart::Text { text }]),
            };
            let created = rho_rpc::protocol::client::call(host_socket(socket_path)?, call).await?;
            println!("workset {}", created.workset);
            if let Some(agent) = created.result.agent {
                println!("agent {}", agent.encoded());
            }
            print!("{}", created.result.output);
            if let Some(error) = created.result.error {
                eprintln!("{error}");
            }
            Ok(())
        }
        DebugCommand::SendMessage {
            agent,
            socket_path,
            text,
        } => {
            let to = rho_agent_types::parse_full_handle(&agent)
                .with_context(|| format!("agent id {agent}"))?;
            let call = rho_agents_client::protocol::rho2::PostMail {
                client_id: uuid::Uuid::new_v4().as_u64_pair().0,
                to,
                re: None,
                content: vec![rho_agent_types::ContentPart::Text { text }],
            };
            let id = rho_rpc::protocol::client::call(host_socket(socket_path)?, call).await?;
            println!("mail {}", id.0);
            Ok(())
        }
    }
}

fn host_socket(socket_path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    Ok(rho_rpc::protocol::RuntimePaths::resolve(socket_path)?
        .socket()
        .to_owned())
}

/// The agent host's lock, held for as long as the file lives, or `None` while
/// the agent host has it.
fn try_host_lock(host_lock: &Path) -> anyhow::Result<Option<std::fs::File>> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(host_lock)
        .with_context(|| format!("open agent host lock {}", host_lock.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("lock {}", host_lock.display()));
    }
    Ok(Some(lock))
}

#[derive(Debug)]
struct Snapshot {
    source: PathBuf,
    path: PathBuf,
    _dir: SnapshotDir,
}

/// A snapshot's own directory, removed when the run is done with it.
#[derive(Debug)]
struct SnapshotDir(PathBuf);

impl Drop for SnapshotDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn copy_snapshot(db_path: Option<PathBuf>) -> anyhow::Result<Snapshot> {
    let source = match db_path {
        Some(source) => return copy_snapshot_unlocked(&source),
        None => default_db_path().context("resolve rho db path")?,
    };
    let paths = rho_rpc::protocol::RuntimePaths::from_env()?;
    std::fs::create_dir_all(paths.directory()).context("create rho runtime directory")?;
    match copy_snapshot_from(&source, &paths.host_lock())? {
        Some(snapshot) => Ok(snapshot),
        None => request_snapshot(paths.socket(), &source).await,
    }
}

/// Ask the running agent host for a snapshot: it alone can copy the file
/// between commits, in a state that opens without repair.
async fn request_snapshot(socket: &Path, source: &Path) -> anyhow::Result<Snapshot> {
    let path = rho_rpc::protocol::client::call(socket, rho_agent_hosts::protocol::Snapshot)
        .await
        .context("the agent host holds the database, and its socket does not answer")?
        .into_std_path_buf();
    let dir = path.parent().context("snapshot has no directory")?;
    Ok(Snapshot {
        source: source.to_owned(),
        _dir: SnapshotDir(dir.to_owned()),
        path,
    })
}

/// The agent host's half of
/// [`host::Snapshot`](rho_agent_hosts::protocol::Snapshot):
/// a snapshot of `db` in a directory of its own beside it.
pub(crate) async fn host_snapshot(db: &RhoDb) -> anyhow::Result<camino::Utf8PathBuf> {
    let dir = new_snapshot_dir(db.path())?;
    let path = dir.0.join("rho.redb");
    db.snapshot(&path).await?;
    let path = camino::Utf8PathBuf::from_path_buf(path)
        .map_err(|path| anyhow::anyhow!("snapshot path is not UTF-8: {}", path.display()))?;
    // Handed over: the directory is the caller's to delete now.
    std::mem::forget(dir);
    Ok(path)
}

fn new_snapshot_dir(source: &Path) -> anyhow::Result<SnapshotDir> {
    let dir = tempfile::Builder::new()
        .prefix(SNAPSHOT_PREFIX)
        .tempdir_in(snapshot_dir(source)?)
        .context("create debug db snapshot directory")?;
    Ok(SnapshotDir(dir.keep()))
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

/// A copy of the closed database, or `None` while the agent host has it open.
fn copy_snapshot_from(source: &Path, host_lock: &Path) -> anyhow::Result<Option<Snapshot>> {
    let Some(_lock) = try_host_lock(host_lock)? else {
        return Ok(None);
    };
    copy_snapshot_unlocked(source).map(Some)
}

fn copy_snapshot_unlocked(source: &Path) -> anyhow::Result<Snapshot> {
    let dir = new_snapshot_dir(source)?;
    let path = dir.0.join("rho.redb");
    rho_db::clone_file(source, &path)
        .with_context(|| format!("copy rho db snapshot from {}", source.display()))?;
    Ok(Snapshot {
        source: source.to_owned(),
        path,
        _dir: dir,
    })
}

/// Prints a read-only report. While the agent host runs, it renders the
/// report from its live database; otherwise it is read from a migrated copy.
async fn print_report(
    db_path: Option<PathBuf>,
    claude: &rho_claude::accounts::ClaudePaths,
    report: DebugReport,
) -> anyhow::Result<()> {
    let snapshot = match db_path {
        Some(source) => copy_snapshot_unlocked(&source)?,
        None => {
            let source = default_db_path().context("resolve rho db path")?;
            let paths = rho_rpc::protocol::RuntimePaths::from_env()?;
            std::fs::create_dir_all(paths.directory()).context("create rho runtime directory")?;
            match copy_snapshot_from(&source, &paths.host_lock())? {
                Some(snapshot) => snapshot,
                None => {
                    let path = rho_rpc::protocol::client::call(paths.socket(), report)
                        .await
                        .context("ask the agent host, which holds the database, for the report")?
                        .into_std_path_buf();
                    let _dir =
                        SnapshotDir(path.parent().context("report has no directory")?.to_owned());
                    let mut file = std::fs::File::open(&path)
                        .with_context(|| format!("open report {}", path.display()))?;
                    io::copy(&mut file, &mut io::stdout().lock())?;
                    return Ok(());
                }
            }
        }
    };
    let db = RhoDb::open(&snapshot.path);
    migrate_snapshot(&db).await?;
    let output = render_report(&db, &snapshot.source, claude, report).await?;
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

/// The agent host's half of
/// [`host::DebugReport`](rho_agent_hosts::protocol::DebugReport): the
/// report, read from the live `db`, in a file of a directory of its own
/// beside it.
pub(crate) async fn host_report(
    db: &RhoDb,
    claude: &rho_claude::accounts::ClaudePaths,
    report: DebugReport,
) -> anyhow::Result<camino::Utf8PathBuf> {
    // Decoding every agent's log takes seconds of CPU: keep it off the
    // runtime's workers.
    let (live, claude) = (db.clone(), claude.clone());
    let runtime = tokio::runtime::Handle::current();
    let output = tokio::task::spawn_blocking(move || {
        runtime.block_on(render_report(&live, live.path(), &claude, report))
    })
    .await??;
    let dir = new_snapshot_dir(db.path())?;
    let path = dir.0.join("report.txt");
    std::fs::write(&path, output).with_context(|| format!("write report {}", path.display()))?;
    let path = camino::Utf8PathBuf::from_path_buf(path)
        .map_err(|path| anyhow::anyhow!("report path is not UTF-8: {}", path.display()))?;
    // Handed over: the directory is the caller's to delete now.
    std::mem::forget(dir);
    Ok(path)
}

/// The text of `report`, read from `db`, a copy of `source` or `source`
/// itself.
pub(crate) async fn render_report(
    db: &RhoDb,
    source: &Path,
    claude: &rho_claude::accounts::ClaudePaths,
    report: DebugReport,
) -> anyhow::Result<String> {
    match report {
        DebugReport::Agents => render_agents(db, source, claude).await,
        DebugReport::Context => render_context(db, source, claude).await,
        DebugReport::Transcript { agent, max_chars } => {
            render_transcript(db, &agent, max_chars as usize)
        }
    }
}

async fn render_agents(
    db: &RhoDb,
    source: &Path,
    claude: &rho_claude::accounts::ClaudePaths,
) -> anyhow::Result<String> {
    let read = db.read();
    let worksets = read.worksets();
    let mut agents = read.agents();
    agents.sort_by_key(|(_, head)| head.created_at);

    let mut output = String::new();
    writeln!(output, "source: {}", source.display())?;
    writeln!(output, "worksets: {}", worksets.len())?;
    for (workset, head) in &worksets {
        writeln!(
            output,
            "  {workset}: {:?}, {} rows, {} agents",
            head.kind,
            head.next.0,
            head.agents.len()
        )?;
    }
    writeln!(output, "agents: {}", agents.len())?;
    for (agent_id, head) in agents {
        writeln!(output)?;
        writeln!(output, "{}", agent_id.encoded())?;
        writeln!(output, "  workset: {}", head.workset)?;
        writeln!(output, "  props: {:?}", head.spec.props)?;
        writeln!(
            output,
            "  log: rows {}..={}, {} visible",
            head.created.0,
            head.last.0,
            read.agent_rows(agent_id, head.created).len()
        )?;
        match head.spec.runtime {
            Runtime::Native {
                model,
                profile,
                cache_key,
            } => {
                writeln!(output, "  runtime: native {model:?} {profile:?}")?;
                writeln!(output, "  prompt_cache_key: {cache_key:?}")?;
            }
            Runtime::Claude {
                model,
                effort,
                session_id,
            } => {
                writeln!(output, "  runtime: claude {model:?} {effort:?}")?;
                writeln!(output, "  session_id: {session_id}")?;
                let cwd = workset_cwd(&read, head.workset);
                match rho_claude::find_session_transcript(&claude.projects(), session_id, &cwd)
                    .await?
                {
                    Some(path) => writeln!(output, "  transcript: {path}")?,
                    None => writeln!(output, "  transcript: <missing>")?,
                }
            }
        }
    }
    Ok(output)
}

/// Where a workset's agents work.
fn workset_cwd(read: &rho_db::ReadTxn, workset: rho_agent_types::WorksetId) -> camino::Utf8PathBuf {
    read.workset(workset).map_or_else(
        || camino::Utf8PathBuf::from(rho_fs_view::MOUNT_ROOT),
        |head| rho_agent::host::pool::workset_root(&head.kind),
    )
}

fn render_transcript(db: &RhoDb, agent: &str, max_chars: usize) -> anyhow::Result<String> {
    let agent_id = rho_agent_types::AgentId::from_encoded(agent)
        .with_context(|| format!("agent id {agent}"))?;
    let read = db.read();
    let head = read.agent(agent_id).context("no such agent")?;
    let mut output = String::new();
    for (pos, record) in read.agent_rows(agent_id, head.created) {
        let text = format!("{record:?}");
        let cut = text
            .char_indices()
            .nth(max_chars)
            .map_or(text.len(), |(at, _)| at);
        writeln!(output, "#{} {}", pos.0, &text[..cut])?;
        if cut < text.len() {
            writeln!(output, "   … {} more bytes", text.len() - cut)?;
        }
    }
    Ok(output)
}

async fn render_context(
    db: &RhoDb,
    source: &Path,
    claude: &rho_claude::accounts::ClaudePaths,
) -> anyhow::Result<String> {
    let read = db.read();
    let mut agents = read.agents();
    agents.sort_by_key(|(_, head)| head.created_at);

    let mut output = String::new();
    writeln!(output, "source: {}", source.display())?;
    writeln!(output, "agents: {}", agents.len())?;
    for (agent_id, head) in agents {
        writeln!(output)?;
        writeln!(output, "{}", agent_id.encoded())?;
        match head.spec.runtime {
            Runtime::Native { .. } => {
                writeln!(output, "  runtime: native")?;
                writeln!(output, "  context from row: {}", head.native.from.0)?;
                writeln!(
                    output,
                    "  restored context_used: {:?}",
                    head.native.compaction.context_used
                )?;
            }
            Runtime::Claude { session_id, .. } => {
                writeln!(output, "  runtime: claude")?;
                writeln!(output, "  session_id: {session_id}")?;
                let cwd = workset_cwd(&read, head.workset);
                let transcript =
                    rho_claude::find_session_transcript(&claude.projects(), session_id, &cwd)
                        .await?;
                let Some(transcript) = transcript else {
                    writeln!(output, "  transcript: <missing>")?;
                    continue;
                };
                writeln!(output, "  transcript: {transcript}")?;
                let messages = rho_claude::read_session_messages_by_id(
                    &claude.projects(),
                    session_id,
                    &cwd,
                    rho_claude::SessionMessagesOptions::default(),
                )
                .await?;
                writeln!(output, "  messages: {}", messages.len())?;
                match rho_claude::last_assistant_usage(&messages) {
                    Some(usage) => writeln!(
                        output,
                        "  restored context_used: Some({})",
                        usage.context_total()
                    )?,
                    None => writeln!(output, "  restored context_used: None")?,
                }
            }
        }
    }
    Ok(output)
}

async fn test_migration(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let snapshot = copy_snapshot(db_path).await?;
    let db = RhoDb::open(&snapshot.path);
    migrate_snapshot(&db).await?;

    let read = db.read();
    let worksets = read.worksets();
    let mut rows = 0usize;
    // Every physical row decodes: a later rewind must not hide an obsolete
    // decoder.
    for (workset, head) in &worksets {
        let mut from = rho_agent_types::Wpos::ZERO;
        while from < head.next {
            let page = read.workset_rows(*workset, from, 4096);
            let Some((last, _)) = page.last() else {
                anyhow::bail!("workset {workset} ends at {} of {}", from.0, head.next.0);
            };
            from = last.next();
            rows += page.len();
        }
    }
    let mails = read.mail_head().0;

    let mut output = String::new();
    writeln!(output, "source: {}", snapshot.source.display())?;
    writeln!(output, "snapshot: {}", snapshot.path.display())?;
    writeln!(output, "migration on copied database: ok")?;
    writeln!(output, "worksets decoded: {}", worksets.len())?;
    writeln!(output, "rows decoded: {rows}")?;
    writeln!(output, "mails: {mails}")?;
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

async fn migrate_snapshot(db: &RhoDb) -> anyhow::Result<()> {
    Accounts::init(db).await?;
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
    let (path, dropped) = match db_path {
        Some(path) => {
            let dropped = rho_agent::db::forget_savepoints(&RhoDb::open(&path)).await;
            (path, dropped)
        }
        None => {
            let paths = rho_rpc::protocol::RuntimePaths::from_env()?;
            let dropped = rho_rpc::protocol::client::call(
                paths.socket(),
                rho_agent_hosts::protocol::ForgetSavepoints,
            )
            .await
            .context("ask the agent host, which holds the database")?;
            (default_db_path().context("resolve rho db path")?, dropped)
        }
    };
    println!(
        "{}: forgot {} migration savepoint(s) {dropped:?}",
        path.display(),
        dropped.len()
    );
    Ok(())
}

async fn delete_worksets(socket_path: Option<PathBuf>, worksets: &[String]) -> anyhow::Result<()> {
    let worksets = worksets
        .iter()
        .map(|id| id.parse().with_context(|| format!("workset id {id}")))
        .collect::<anyhow::Result<Vec<rho_agent_types::WorksetId>>>()?;
    rho_rpc::protocol::client::call(
        host_socket(socket_path)?,
        rho_agents_client::protocol::rho2::DeleteWorksets {
            worksets: worksets.clone(),
        },
    )
    .await
    .context("ask the running host to delete worksets")?;
    for workset in worksets {
        println!("deleted {workset}");
    }
    Ok(())
}

fn stats(db_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = db_path
        .map(Ok)
        .unwrap_or_else(default_db_path)
        .context("resolve rho db path")?;
    RhoDb::print_stats(&path)
}

async fn compact(db_path: Option<PathBuf>, socket_path: Option<PathBuf>) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let (before, after) = match db_path {
        Some(path) => {
            anyhow::ensure!(
                socket_path.is_none(),
                "--db-path and --socket-path are mutually exclusive"
            );
            RhoDb::compact(path)?
        }
        None => rho_rpc::protocol::client::call(
            host_socket(socket_path)?,
            rho_agent_hosts::protocol::CompactDatabase,
        )
        .await
        .context("ask the running host to compact its database")?,
    };
    println!(
        "database: {before} -> {after} bytes in {:.1?}",
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn a_live_host_lock_leaves_the_copy_to_the_host() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("rho.redb");
        drop(RhoDb::open(&source));
        let lock_path = directory.path().join("agent-host.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);

        assert!(copy_snapshot_from(&source, &lock_path).unwrap().is_none());

        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(lock);
        let snapshot = copy_snapshot_from(&source, &lock_path).unwrap().unwrap();
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
        let lock_path = directory.path().join("agent-host.lock");

        let snapshot = copy_snapshot_from(&source, &lock_path).unwrap().unwrap();
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
