//! A rig: a snapshot made runnable.
//!
//! `rig new` clones a snapshot — a reflink clone where the filesystem has
//! them, so the base snapshot stays pristine and a rig costs almost nothing.
//! `rig up` stands the rig up on that copy: its own daemon, the fakes, and the
//! GUI headless in an isolated Wayland session with the profiler on.
//!
//! A rig is not reset between sessions. That is the point of the accumulated
//! QA desk: agents created, verdicts given, notes filed and threads read stay
//! in the rig's state the way the user's own state accumulates. `rig new` is
//! for starting a new line of QA, not for cleaning up after a run.

use std::collections::HashSet;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use rho_core::{AgentRole, ContentPart};
use rho_ui_proto::client::Client;
use rho_ui_proto::mirror::MirrorEvent;
use rho_ui_proto::{ClientMessage, JoinTarget, ServerMessage, StartMode};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::paths;
use crate::profile::{self, Summary};
use crate::snapshot::human;

/// How long to wait for the daemon's socket and the fake's readiness line.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the last session's daemon gets to let go of the store.
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the GUI's profile sidecars get to land after it has exited.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Subcommand)]
pub enum RigCommand {
    /// Clone a snapshot into a rig that can be run.
    New(NewArgs),
    /// Start the daemon, the fakes and the headless GUI on a rig.
    Up(UpArgs),
    /// Stop everything the rig runs, leaving its state as the run left it.
    Down(NameArgs),
    /// Say what the rig is and what of it is running.
    Status(NameArgs),
    /// Send one native agent turn through a running rig and its fake model.
    Probe(NameArgs),
    /// List the rigs.
    List,
}

#[derive(Args)]
pub struct NewArgs {
    /// The snapshot to clone: a name under the snapshots root, or a path.
    #[arg(long)]
    from: String,

    /// The rig's name. Defaults to the snapshot's.
    #[arg(long)]
    name: Option<String>,
}

#[derive(Args)]
pub struct BuildArgs {
    /// Which build to make.
    #[arg(long, value_enum, default_value_t = Binaries::Profiling)]
    binaries: Binaries,
}

#[derive(Args)]
pub struct NameArgs {
    name: String,
}

#[derive(Args)]
pub struct UpArgs {
    name: String,

    /// Which build to run.
    #[arg(long, value_enum, default_value_t = Binaries::Profiling)]
    binaries: Binaries,

    /// Leave the GUI down; bring up only the daemon and the fakes.
    #[arg(long)]
    no_gui: bool,

    /// Run the GUI without the CPU profiler.
    #[arg(long)]
    no_profile: bool,

    /// Start even when a binary is older than the sources it was built
    /// from. Every session that used this says so in its own report, and
    /// its numbers belong to whatever was actually on disk.
    #[arg(long)]
    allow_stale_binaries: bool,

    /// Take a rig that is already up, stopping whatever is running on it.
    /// Without this, `up` refuses and says whose session holds it.
    #[arg(long)]
    take: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Binaries {
    /// `target/profiling` from the tree under test: release, with frame
    /// pointers and line tables, which is what the profiler needs. The
    /// default, because QA proves the code being written.
    Profiling,
    /// `target/release` from the tree under test.
    Release,
    /// The binaries the user actually runs, out of the nix profile, for
    /// reproducing a report on the build it came from.
    Nix,
    /// `target/debug`: the fast build, for working on the rig itself. Never
    /// for a proof number — an unoptimised GUI says nothing about frame cost.
    Debug,
}

impl Binaries {
    /// The cargo profile these binaries are built at. The nix binaries are
    /// not built here at all; what a missing one wants is the tree's
    /// profiling build, which is where the GUI and the fakes come from.
    fn profile(self) -> &'static str {
        match self {
            Self::Profiling | Self::Nix => "profiling",
            Self::Release => "release",
            Self::Debug => "dev",
        }
    }
}

/// What a rig is, written as `rig.json` at its root.
#[derive(Serialize, Deserialize)]
struct Rig {
    name: String,
    /// The snapshot it was cloned from.
    from: PathBuf,
    created: String,
    /// One line per session the rig has been brought up for. The desk
    /// accumulates; so does its history.
    sessions: Vec<Session>,
}

#[derive(Serialize, Deserialize)]
struct Session {
    at: String,
    /// Who brought it up. A desk is shared and two engineers took it from
    /// each other twice in one afternoon, each time by seconds; a session
    /// that says whose it is turns that into a message instead of a
    /// killed run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    by: Option<String>,
    binaries: String,
    tree_commit: Option<String>,
    /// What was actually launched. The commit alone is a claim about the
    /// tree, not about the binaries built from it, and the two came apart
    /// for five sessions in one afternoon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    binary_hashes: Vec<String>,
    /// Whether this session ran binaries older than their sources. A
    /// session that did must never be readable as one that did not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    stale_binaries: bool,
    /// What the GUI was told to profile to, so `rig down` knows which files
    /// this session left behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<PathBuf>,
    /// What those files said, written here on the way down so a landing
    /// note can quote the run rather than re-derive it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<Summary>,
    /// The last drive this session was named for, and how many steps ran
    /// under it. Two sessions on one commit came back 4.9% and 79% over
    /// budget, and the difference was neither the commit nor the machine:
    /// it was what the reader did. A frame number without these two beside
    /// it cannot be compared with any other frame number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drive: Option<String>,
    #[serde(default)]
    drive_steps: usize,
}

/// Build what a rig runs. Goes through `crate::build` so the linker the
/// optimised binaries need is set here rather than in an engineer's shell.
pub fn build(args: BuildArgs) -> Result<()> {
    crate::build::build(args.binaries.profile(), &repo_root()?)
}

pub fn run(command: RigCommand) -> Result<()> {
    match command {
        RigCommand::New(args) => new(args),
        RigCommand::Up(args) => up(args),
        RigCommand::Down(args) => down(&args.name),
        RigCommand::Status(args) => status(&args.name),
        RigCommand::Probe(args) => probe(&args.name),
        RigCommand::List => list(),
    }
}

fn new(args: NewArgs) -> Result<()> {
    let snapshot = if args.from.contains('/') {
        PathBuf::from(&args.from)
    } else {
        paths::snapshots_root()?.join(&args.from)
    };
    if !snapshot.join("manifest.json").exists() {
        bail!(
            "{} is not a snapshot (no manifest.json)",
            snapshot.display()
        );
    }
    let name = match args.name {
        Some(name) => name,
        None => snapshot
            .file_name()
            .context("snapshot has no name")?
            .to_string_lossy()
            .into_owned(),
    };
    let root = paths::rigs_root()?.join(&name);
    if root.exists() {
        bail!(
            "{} already exists; a rig is never reset, so bring it up or name another",
            root.display()
        );
    }
    fs::create_dir_all(&root)?;

    // Reflink where the filesystem has it, a full copy where it does not.
    // `--reflink=auto` is the one that falls back rather than failing.
    let status = Command::new("cp")
        .args(["-a", "--reflink=auto"])
        .arg(snapshot.join("state"))
        .arg(root.join("state"))
        .status()
        .context("run cp")?;
    if !status.success() {
        bail!("cloning {} failed", snapshot.display());
    }
    // A snapshot taken with `--gui-state` holds a second device's half: the
    // mirror, the inbox, the journal, the desk device. It is laid over the
    // daemon's state after the clone, because a rig runs one state directory
    // and the GUI reads its files from the same place the daemon does.
    let gui = snapshot.join("gui-state").join("rho");
    if gui.is_dir() {
        for relative in paths::GUI_SNAPSHOT_CONTENTS {
            let from = gui.join(relative);
            if !from.exists() {
                continue;
            }
            let to = root.join("state").join("rho").join(relative);
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
            println!("gui     {relative} from the snapshot's GUI half");
        }
    }
    for dir in ["config", "run", "logs", "screens", "profiles"] {
        fs::create_dir_all(root.join(dir))?;
    }
    // The runtime dir has to be private or the compositor refuses it.
    fs::set_permissions(root.join("run"), permissions(0o700))?;
    // A placeholder until the first `up` learns the fake's real workspace.
    write_credentials(&root, "rig")?;

    let rig = Rig {
        name: name.clone(),
        from: snapshot,
        created: chrono::Local::now().to_rfc3339(),
        sessions: Vec::new(),
    };
    save(&root, &rig)?;
    println!(
        "{} — cloned, {} of state",
        root.display(),
        state_size(&root)
    );
    Ok(())
}

fn up(args: UpArgs) -> Result<()> {
    let root = rig_root(&args.name)?;
    let mut rig = load(&root)?;
    let bin = binaries(args.binaries, !args.no_gui)?;

    // A desk is shared. Anything still running is somebody's session, and
    // taking it silently is how a run gets killed mid-drive — which has
    // happened, twice in one afternoon, in both directions. Say whose it is
    // and let them be asked; `--take` is the deliberate version.
    if !args.take
        && let Some(held) = holding_session(&root, &rig)
    {
        bail!("{held}");
    }
    // Which binaries these actually are, before anything is started and
    // before any number is attributed to a commit. This exists because five
    // sessions in one afternoon ran a GUI three hours older than the tree
    // and were reported as a commit that was never in them.
    let mut identities = vec![
        Identity::of("daemon", bin.daemon()),
        Identity::of("rho", bin.rho()),
        Identity::of("fake_slack", bin.fake_slack()),
    ];
    if !args.no_gui {
        identities.push(Identity::of("gui", bin.gui()));
    }
    let repo = repo_root()?;
    let newest = newest_source(&repo);
    let mut ran_stale = false;
    println!(
        "tree    {} ({} binaries)",
        crate::snapshot::tree_commit().unwrap_or_else(|| "unknown".to_owned()),
        bin.label,
    );
    for identity in &identities {
        println!("{}", identity.line());
    }
    // The same line into the rig's own log. A terminal scrolls away and a
    // report is written from the log; the identity has to be in both or it
    // is not there when it is needed.
    {
        use std::io::Write as _;
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("logs").join("rig.log"))?;
        writeln!(
            log,
            "\n{} rig up: tree {} ({} binaries)\n{}",
            chrono::Local::now().to_rfc3339(),
            crate::snapshot::tree_commit().unwrap_or_else(|| "unknown".to_owned()),
            bin.label,
            identities
                .iter()
                .map(Identity::line)
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
    }
    if let Some((source_at, source)) = &newest {
        let stale = identities
            .iter()
            .filter(|identity| identity.modified.is_some_and(|at| at < *source_at))
            .map(|identity| identity.name)
            .collect::<Vec<_>>();
        println!(
            "  newest source {} ({})",
            source
                .strip_prefix(&repo)
                .unwrap_or(source.as_path())
                .display(),
            source_at
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs().to_string())
                .unwrap_or_else(|_| "unknown".to_owned()),
        );
        if !stale.is_empty() {
            if !args.allow_stale_binaries {
                bail!(
                    "{} older than {}: build before running, or pass \
                     --allow-stale-binaries and own the numbers",
                    stale.join(", "),
                    source
                        .strip_prefix(&repo)
                        .unwrap_or(source.as_path())
                        .display(),
                );
            }
            // Loud, and in the log as well as on the terminal: a session
            // that ran stale binaries must be impossible to read as one
            // that did not.
            ran_stale = true;
            println!(
                "STALE   {} older than the sources, started anyway (--allow-stale-binaries)",
                stale.join(", "),
            );
        }
    }

    // Anything still running from the last session is the last session's, not
    // this one's.
    stop_gui(&root, &bin, &args.name);
    stop_daemon(&root);

    let runtime = root.join("run");
    fs::create_dir_all(runtime.join("rho"))?;
    fs::set_permissions(&runtime, permissions(0o700))?;
    let socket = runtime.join("rho").join("rho.sock");
    let _ = fs::remove_file(&socket);

    fs::create_dir_all(root.join("config").join("claude"))?;
    let model = start_fake_model(&root, &bin)?;
    write_model_credentials(&root)?;
    println!(
        "model   fake on {} (pid {})",
        model.openai_base_url, model.pid
    );

    // The rig daemon is its own node: no `--iroh`, no identity of the user's.
    let log = fs::File::create(root.join("logs").join("daemon.log"))?;
    let daemon = command(bin.daemon(), &root)
        .arg("--socket-path")
        .arg(&socket)
        // Named outright, not left to the environment. A daemon that resolves
        // its Claude directory from `$HOME` reads the user's transcripts the
        // moment it is started any way but this one.
        .arg("--claude-config-dir")
        .arg(root.join("config").join("claude"))
        .args(["--openai-base-url", &model.openai_base_url])
        .args(["--anthropic-base-url", &model.anthropic_base_url])
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("start {}", bin.daemon().display()))?;
    let pid = daemon.id();
    fs::write(root.join("run").join("daemon.pid"), pid.to_string())?;
    wait_for(&socket, "the daemon's socket")?;
    // A socket on disk is not a daemon: the last one's may still be there, and
    // this one may have died opening the store. Ask the process.
    if !alive(pid) {
        bail!(
            "the daemon exited at startup; the last lines of {}:\n{}",
            root.join("logs").join("daemon.log").display(),
            tail(&root.join("logs").join("daemon.log"), 5)
        );
    }
    println!("daemon  up on {} (pid {pid})", socket.display());

    let slack = start_fake_slack(&root, &bin)?;
    println!(
        "slack   fake on {} as workspace `{}`",
        slack.api_base, slack.workspace
    );
    // The client's session is the fake, and a session is per workspace: the
    // credentials have to name the workspace the fake actually came up as,
    // not the one a rig was created with. Written on every `up`, because the
    // mirror can change under a rig and the workspace with it.
    write_credentials(&root, &slack.workspace)?;

    let mut profile_path = None;
    if !args.no_gui {
        let profile = (!args.no_profile).then(|| {
            root.join("profiles").join(format!(
                "gui-{}.bin",
                chrono::Local::now().format("%Y%m%dT%H%M%S")
            ))
        });
        profile_path = profile.clone();
        start_gui(
            &root,
            &bin,
            &args.name,
            &socket,
            &slack.api_base,
            profile.as_deref(),
        )?;
        println!(
            "gui     up in the `{}` wayland session{}",
            args.name,
            match &profile {
                Some(path) => format!(", profiling to {}", path.display()),
                None => String::new(),
            }
        );
    }

    rig.sessions.push(Session {
        at: chrono::Local::now().to_rfc3339(),
        by: holder(),
        binaries: bin.label.clone(),
        tree_commit: crate::snapshot::tree_commit(),
        binary_hashes: identities
            .iter()
            .map(|identity| format!("{}:{}", identity.name, identity.hash))
            .collect(),
        stale_binaries: ran_stale,
        profile: profile_path.clone(),
        summary: None,
        // Filled on the way down, from the log the driver writes as it
        // goes. A session that never drove anything keeps these as they
        // are, and the report says so rather than showing a blank.
        drive: None,
        drive_steps: 0,
    });
    save(&root, &rig)?;
    let tree_commit = crate::snapshot::tree_commit().unwrap_or_else(|| "unknown".to_owned());
    println!(
        "RIG_READY tree_commit={tree_commit} rho_qa_sha256={} fake_sha256={} daemon_sha256={} rig={} session={}",
        sha256_file(&std::env::current_exe()?)?,
        sha256_file(&bin.fake_model())?,
        sha256_file(&bin.daemon())?,
        args.name,
        rig.sessions.len(),
    );
    println!(
        "\nrig {} up on {} binaries — session {} of this desk",
        args.name,
        bin.label,
        rig.sessions.len()
    );
    println!(
        "drive it with: rho wayland --session {0} drive \"<what this run is>\" \
         then rho wayland --session {0} <key|input|screenshot|tree>",
        args.name
    );
    Ok(())
}

fn down(name: &str) -> Result<()> {
    let root = rig_root(name)?;
    let which = match load(&root)?
        .sessions
        .last()
        .map(|session| session.binaries.as_str())
    {
        Some("release") => Binaries::Release,
        Some("nix") => Binaries::Nix,
        Some("debug") => Binaries::Debug,
        _ => Binaries::Profiling,
    };
    let bin = binaries(which, false)?;
    stop_gui(&root, &bin, name);
    file_application_log(&root, name);
    record_drive(&root, name);
    file_drive_log(&root, name);
    stop_daemon(&root);
    println!("rig {name} down; its state is as the run left it");
    summarize_session(&root)
}

/// One line of a drive log: the driver writes a `drive` line when a recipe
/// is named and a `step` line for every key, chord, click or move it sends.
#[derive(Deserialize)]
struct DriveLine {
    #[serde(default)]
    at_ms: Option<u64>,
    #[serde(default)]
    drive: Option<String>,
    #[serde(default)]
    step: Option<String>,
}

/// What the driver did this session, read from the log it wrote beside the
/// wayland session directory.
///
/// A session may name several drives in turn; what is reported is the last
/// one and the steps under it, because that is the run whose frames are in
/// the profile. Steps sent before any drive was named are counted under no
/// name, which is the case the report has to be loud about rather than hide.
fn drive_taken(root: &Path, name: &str) -> (Option<String>, usize) {
    let log = root
        .join("run")
        .join("rho-wayland")
        .join(format!("{name}-drive.log"));
    let Ok(text) = fs::read_to_string(&log) else {
        return (None, 0);
    };
    let mut drive = None;
    let mut steps = 0;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<DriveLine>(line) else {
            continue;
        };
        if let Some(named) = entry.drive {
            drive = Some(named);
            steps = 0;
        } else if entry.step.is_some() {
            steps += 1;
        }
    }
    (drive, steps)
}

/// Put the drive on the session and say so, including when there is none.
///
/// The silence is the point: a run with no drive named and no steps counted
/// is a run nobody can compare, and the report says that in words rather
/// than leaving a reader to assume the recipe was the usual one.
fn record_drive(root: &Path, name: &str) {
    let (drive, steps) = drive_taken(root, name);
    match (&drive, steps) {
        (Some(drive), steps) => println!("drive   {drive}, {steps} steps"),
        (None, 0) => println!(
            "drive   none named and no steps recorded; \
             this session's numbers cannot be compared with another run's"
        ),
        (None, steps) => println!(
            "drive   unnamed, {steps} steps; \
             name the next one with `rho wayland --session {name} drive <name>`"
        ),
    }
    let Ok(mut rig) = load(root) else { return };
    let Some(session) = rig.sessions.last_mut() else {
        return;
    };
    session.drive = drive;
    session.drive_steps = steps;
    let _ = save(root, &rig);
}

/// Keep the drive log next to the profile it belongs to, the way the
/// application log is kept: the numbers, the errors and the steps that
/// produced both share a stem.
fn file_drive_log(root: &Path, name: &str) {
    let from = root
        .join("run")
        .join("rho-wayland")
        .join(format!("{name}-drive.log"));
    if !from.exists() {
        return;
    }
    let stem = load(root)
        .ok()
        .and_then(|rig| {
            let session = rig.sessions.last()?;
            let profile = session.profile.as_ref()?;
            Some(profile.file_stem()?.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| name.to_owned());
    let to = root.join("logs").join(format!("{stem}-drive.log"));
    if fs::rename(&from, &to).is_err() {
        let _ = fs::copy(&from, &to).and_then(|_| fs::remove_file(&from));
    }
}

/// File the stopped session's application log next to the profile it belongs
/// to, under the same name.
///
/// `rho wayland stop` moves the log beside the session directory before
/// removing it; this puts it where a later reader will look, which is the
/// rig's `logs/`, named after the profile so a run's numbers and a run's
/// errors share a stem. Without it a panic is only ever visible to whoever
/// was watching when it happened: the summary line says how a run drew, and
/// nothing says what it said.
fn file_application_log(root: &Path, name: &str) {
    let kept = root
        .join("run")
        .join("rho-wayland")
        .join(format!("{name}-application.log"));
    if !kept.exists() {
        return;
    }
    let stem = load(root)
        .ok()
        .and_then(|rig| {
            let session = rig.sessions.last()?;
            let profile = session.profile.as_ref()?;
            Some(profile.file_stem()?.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| name.to_owned());
    let to = root.join("logs").join(format!("{stem}.log"));
    if fs::rename(&kept, &to).is_err() {
        let _ = fs::copy(&kept, &to).and_then(|_| fs::remove_file(&kept));
    }
}

/// The line the run earned. The GUI writes its frame log, its editor log and
/// its CPU profile as it exits, so this runs after the GUI is stopped and
/// waits for the files rather than racing them. A session with no profile —
/// `--no-gui`, `--no-profile`, or a GUI that died before it could write —
/// says nothing, because a summary of nothing is worse than silence.
fn summarize_session(root: &Path) -> Result<()> {
    let mut rig = load(root)?;
    let Some(session) = rig.sessions.last_mut() else {
        return Ok(());
    };
    let Some(path) = session.profile.clone() else {
        return Ok(());
    };
    if !wait_for_profile(&path) {
        println!("profile {} never landed; no summary", path.display());
        return Ok(());
    }
    match profile::summarize(&path) {
        Ok(summary) => {
            println!("{}", summary.line);
            session.summary = Some(summary);
            save(root, &rig)
        }
        Err(error) => {
            println!("the profile is there but would not read: {error:#}");
            Ok(())
        }
    }
}

/// The frame log is the one the summary cannot do without; the CPU profile is
/// compressed on the way out and may be a moment behind it.
fn wait_for_profile(path: &Path) -> bool {
    let frames = PathBuf::from({
        let mut held = path.as_os_str().to_owned();
        held.push(".frames.json");
        held
    });
    let deadline = Instant::now() + FLUSH_TIMEOUT;
    while Instant::now() < deadline {
        if frames.exists() {
            // Give the compressor the same grace, but do not hold the
            // command open for it: the summary reads what is there.
            std::thread::sleep(Duration::from_millis(500));
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn status(name: &str) -> Result<()> {
    let root = rig_root(name)?;
    let rig = load(&root)?;
    println!("{}", root.display());
    println!("  from     {}", rig.from.display());
    println!("  created  {}", rig.created);
    println!("  sessions {}", rig.sessions.len());
    if let Some(last) = rig.sessions.last() {
        println!("  last     {} on {} binaries", last.at, last.binaries);
        if let Some(commit) = &last.tree_commit {
            println!("  tree     {commit}");
        }
        if !last.binary_hashes.is_empty() {
            println!("  ran      {}", last.binary_hashes.join(" "));
        }
        if last.stale_binaries {
            println!(
                "  STALE    this session ran binaries older than their sources; \
                 its numbers belong to what was on disk, not to the tree above"
            );
        }
        match (&last.drive, last.drive_steps) {
            (Some(drive), steps) => println!("  drive    {drive}, {steps} steps"),
            (None, 0) => println!("  drive    none named"),
            (None, steps) => println!("  drive    unnamed, {steps} steps"),
        }
        if let Some(summary) = &last.summary {
            println!("  profile  {}", summary.line);
        }
    }
    println!("  touched  {}", touch_line(&root, name));
    println!("  state    {}", state_size(&root));
    let socket = root.join("run").join("rho").join("rho.sock");
    println!(
        "  daemon   {}",
        match daemon_pid(&root) {
            Some(pid) if alive(pid) => format!("running, pid {pid}, {}", socket.display()),
            _ => "down".to_owned(),
        }
    );
    Ok(())
}

fn list() -> Result<()> {
    let root = paths::rigs_root()?;
    let Ok(entries) = fs::read_dir(&root) else {
        println!("no rigs yet ({} does not exist)", root.display());
        return Ok(());
    };
    let mut names: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    names.sort();
    for name in names {
        let path = root.join(&name);
        let Ok(rig) = load(&path) else { continue };
        println!(
            "{}  {} sessions  {}  from {}",
            name.display(),
            rig.sessions.len(),
            state_size(&path),
            rig.from.display()
        );
    }
    Ok(())
}

/// The fake Slack, started on the rig's own ports, with the API base it prints
/// read back out of its log.
///
/// Fed from the rig's own Slack mirror when it has one, which is the point of
/// running on a snapshot: the client meets the conversations the user has
/// rather than a fixture's five. The fixture is the fallback, and says so in
/// the log.
/// The fake, once it is serving: where it listens and which workspace it came
/// up as. Both are read back from its log, because the fake chooses the
/// workspace itself when the mirror holds more than one.
struct FakeSlack {
    api_base: String,
    workspace: String,
}

#[derive(Deserialize)]
struct FakeModel {
    ready: bool,
    openai_base_url: String,
    anthropic_base_url: String,
    pid: u32,
}

#[derive(Deserialize)]
struct FakeModelMetrics {
    completed_turns: u64,
}

fn start_fake_model(root: &Path, bin: &Build) -> Result<FakeModel> {
    let path = root.join("logs").join("fake-model.log");
    let log = fs::File::create(&path)?;
    let child = command(bin.fake_model(), root)
        .args(["--seed", "0", "--no-faults"])
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("start {}", bin.fake_model().display()))?;
    let pid = child.id();
    fs::write(root.join("run").join("fake-model.pid"), pid.to_string())?;

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let text = fs::read_to_string(&path).unwrap_or_default();
        if let Some(line) = text.lines().next()
            && let Ok(model) = serde_json::from_str::<FakeModel>(line)
            && model.ready
        {
            if model.pid != pid {
                bail!("fake model reported pid {}, expected {pid}", model.pid);
            }
            return Ok(model);
        }
        if !alive(pid) {
            bail!(
                "the fake model exited at startup; the last lines of {}:\n{}",
                path.display(),
                tail(&path, 8)
            );
        }
        if Instant::now() > deadline {
            bail!(
                "the fake model never became ready; the last lines of {}:\n{}",
                path.display(),
                tail(&path, 8)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn start_fake_slack(root: &Path, bin: &Build) -> Result<FakeSlack> {
    let path = root.join("logs").join("fake-slack.log");
    let log = fs::File::create(&path)?;
    let mirror = root.join("state").join("rho").join("rho-client.redb");
    let fixture = !mirror.exists();
    let mut process = if !fixture {
        let mut loader = command(std::env::current_exe()?, root);
        loader
            .arg("fake-slack")
            .arg("--mirror")
            .arg(&mirror)
            .arg("--scratch")
            .arg(root.join("run").join("slack-seed.redb"));
        loader
    } else {
        command(bin.fake_slack(), root)
    };
    let child = process
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("start {}", bin.fake_slack().display()))?;
    fs::write(
        root.join("run").join("fake-slack.pid"),
        child.id().to_string(),
    )?;

    // Both lines, not just the first: a fake that is listening but came up as
    // no workspace is not a session, and a GUI started against it shows every
    // Slack row as Open. Waiting for `workspace=` is the refusal.
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let text = fs::read_to_string(&path).unwrap_or_default();
        if let Some(fake) = fake_slack_from_log(&text, fixture.then_some("acme")) {
            return Ok(fake);
        }
        if Instant::now() > deadline {
            bail!(
                "the fake Slack never came up as a workspace, so the GUI was not \
                 started: a client with no Slack session shows every row the desk \
                 ever held as Open and no rule can close them, which reads as a \
                 dealing bug. The last lines of {}:\n{}",
                path.display(),
                tail(&path, 8)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What the fake said about itself, or `None` while it is still saying it.
/// Both lines are required: an API base without a workspace is a server, not
/// a session.
fn fake_slack_from_log(text: &str, fixture_workspace: Option<&str>) -> Option<FakeSlack> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::to_owned)
    };
    Some(FakeSlack {
        api_base: field("RHO_SLACK_API_BASE=")?,
        workspace: field("workspace=").or_else(|| {
            if text.lines().any(|line| line == "ready") {
                fixture_workspace.map(str::to_owned)
            } else {
                None
            }
        })?,
    })
}

/// When this rig was last touched by a person, and by what.
///
/// R7's open gap. A rig with nobody on it and a rig somebody is mid-case
/// on look identical from the outside — same processes, same directory,
/// the same `rig status` — and the only thing that distinguished the one
/// found up for 2h19m was that its newest screenshot was from the day
/// before. That is too thin a thread to take a rig down on, and it was
/// thin because nothing read it. This reads it: the newest of the drive
/// log's last step and the newest screenshot, which are the two things a
/// person leaves behind by driving.
///
/// `None` means nobody has driven this session at all, which is a
/// different sentence and gets one.
fn last_touch(root: &Path, name: &str) -> Option<(String, Duration)> {
    let now = SystemTime::now();
    let ago = |at: SystemTime| now.duration_since(at).unwrap_or_default();

    let mut newest: Option<(String, SystemTime)> = None;
    let mut keep = |what: String, at: SystemTime| {
        if newest.as_ref().is_none_or(|(_, held)| at > *held) {
            newest = Some((what, at));
        }
    };

    let log = root
        .join("run")
        .join("rho-wayland")
        .join(format!("{name}-drive.log"));
    if let Ok(text) = fs::read_to_string(&log) {
        for line in text.lines().rev() {
            let Ok(entry) = serde_json::from_str::<DriveLine>(line) else {
                continue;
            };
            let Some(at_ms) = entry.at_ms else { continue };
            let what = entry
                .step
                .or(entry.drive)
                .unwrap_or_else(|| "a step".to_owned());
            keep(what, SystemTime::UNIX_EPOCH + Duration::from_millis(at_ms));
            break;
        }
    }
    if let Ok(entries) = fs::read_dir(root.join("screens")) {
        for entry in entries.flatten() {
            let Ok(at) = entry.metadata().and_then(|data| data.modified()) else {
                continue;
            };
            keep(
                format!("screenshot {}", entry.file_name().to_string_lossy()),
                at,
            );
        }
    }
    newest.map(|(what, at)| (what, ago(at)))
}

/// A duration as a person says it, coarsest unit first. Nothing here is
/// worth a second decimal: the question it answers is "is anybody on this",
/// and the answer is minutes or hours.
fn since_label(since: Duration) -> String {
    let seconds = since.as_secs();
    match (seconds / 3600, (seconds % 3600) / 60) {
        (0, 0) => format!("{seconds}s"),
        (0, minutes) => format!("{minutes}m"),
        (hours, minutes) => format!("{hours}h{minutes:02}m"),
    }
}

/// The line both `status` and the refusal print: whether anyone has driven
/// this session, and how long ago.
fn touch_line(root: &Path, name: &str) -> String {
    match last_touch(root, name) {
        Some((what, since)) => format!("last driven {} ago ({what})", since_label(since)),
        None => "never driven; nothing has been sent to this session".to_owned(),
    }
}

/// Who is running this rig, for the session line. The agent handle if this is
/// an agent's shell, the user otherwise, and nothing rather than a guess.
fn holder() -> Option<String> {
    ["RHO_AGENT_ID", "USER"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// Whether something is already running on this rig, and what to say about
/// it. The daemon's pid is the lock: it is written by `up` and outlives the
/// shell that started it, and `rig.json`'s last session says whose it is.
fn holding_session(root: &Path, rig: &Rig) -> Option<String> {
    let pid = read_pid(&root.join("run").join("daemon.pid")).filter(|pid| alive(*pid))?;
    let last = rig.sessions.last();
    let at = last.map_or("at an unknown time", |session| session.at.as_str());
    let binaries = last.map_or("unknown", |session| session.binaries.as_str());
    let held = last.and_then(|session| session.by.as_deref());
    let mine = held.is_some() && held == holder().as_deref();
    let whose = match held {
        Some(who) if mine => format!("you ({who}), in another shell"),
        Some(who) => who.to_owned(),
        None => "someone who did not say so".to_owned(),
    };
    let what_to_do = if mine {
        format!(
            "If that shell is finished with it, `rho-qa rig down {}`; `--take` \
             stops whatever is running and takes it.",
            rig.name
        )
    } else {
        format!(
            "Ask them before taking it — a `rig up` on a rig someone is driving \
             kills their run. `rho-qa rig down {}` once they say they are down, \
             or `--take` to take it anyway.",
            rig.name
        )
    };
    // Whether the holder is actually on it. A refusal that says only who
    // started a session leaves the next person with a name and no way to
    // tell a live run from one left standing overnight.
    let touched = touch_line(root, &rig.name);
    Some(format!(
        "rig {} is already up: session {}, held by {whose}, started {at} on \
         {binaries} binaries (daemon pid {pid}); {touched}.\n{what_to_do}",
        rig.name,
        rig.sessions.len(),
    ))
}

fn start_gui(
    root: &Path,
    bin: &Build,
    session: &str,
    socket: &Path,
    slack_api: &str,
    profile: Option<&Path>,
) -> Result<()> {
    let mut wayland = command(bin.rho(), root);
    wayland
        .args(["wayland", "--session", session, "start", "--"])
        .arg(bin.gui());
    if let Some(profile) = profile {
        wayland.arg("--cpu-profile").arg(profile);
    }
    wayland
        .arg("--attach")
        .arg(format!("rig=unix:{}", socket.display()))
        // Slack and the browser are the fakes, and only the fakes: the API
        // base points at the fake's port, and the client's "Brave" is the
        // fake browser.
        .env("RHO_SLACK_API_BASE", slack_api)
        .env("RHO_SLACK_CREDENTIALS", root.join("credentials.json"))
        .env("RHO_CUSTOM_BRAVE_BIN", bin.fake_browser())
        .env(
            "RHO_FAKE_BROWSER_CONTROL",
            root.join("run").join("fake-browser.control"),
        )
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "rho_gui::model=debug,warn".to_owned()),
        );
    let status = wayland.status().context("start the wayland session")?;
    if !status.success() {
        bail!("the GUI did not start; see the session's application.log");
    }
    Ok(())
}

fn stop_gui(root: &Path, bin: &Build, session: &str) {
    let _ = command(bin.rho(), root)
        .args(["wayland", "--session", session, "stop"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Stop the daemon and the fake, and wait for them to be gone.
///
/// Waiting is the point. redb allows one writer process, so a daemon started
/// while the last one is still shutting down dies with `DatabaseAlreadyOpen` —
/// and it dies after its socket is already on disk, so everything downstream
/// looks up and the GUI simply says "reconnecting" for ever.
fn stop_daemon(root: &Path) {
    for name in ["daemon.pid", "fake-model.pid", "fake-slack.pid"] {
        let path = root.join("run").join(name);
        if let Some(pid) = read_pid(&path) {
            terminate(pid);
            let deadline = Instant::now() + EXIT_TIMEOUT;
            while alive(pid) {
                if Instant::now() > deadline {
                    println!("killing {name} pid {pid}: it did not stop on TERM");
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let _ = fs::remove_file(path);
    }
}

fn probe(name: &str) -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(probe_async(name))
}

async fn probe_async(name: &str) -> Result<()> {
    let root = rig_root(name)?;
    let socket = root.join("run/rho/rho.sock");
    let workspace = root.join("workspace");
    if !workspace.join(".git").exists() {
        fs::create_dir_all(&workspace)?;
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .arg(&workspace)
            .status()
            .context("initialize the rig probe workspace")?;
        if !status.success() {
            bail!("could not initialize the rig probe workspace");
        }
    }

    let mut client = Client::connect(&socket)
        .await
        .with_context(|| format!("connect to the running rig at {}", socket.display()))?;
    client.send(&ClientMessage::Subscribe).await?;
    let head = loop {
        match client.recv().await? {
            ServerMessage::Ready { journal_head, .. } => break journal_head,
            ServerMessage::Error { message } => bail!("daemon readiness error: {message}"),
            _ => {}
        }
    };
    client.send(&ClientMessage::Follow { since: head }).await?;
    client
        .send(&ClientMessage::NewAgent {
            role: AgentRole::default(),
            start: StartMode::Join(JoinTarget::User {
                repo: workspace.try_into().context("rig workspace is not UTF-8")?,
            }),
            content: Some(vec![ContentPart::Text {
                text: "Complete one deterministic rig probe turn.".to_owned(),
            }]),
        })
        .await?;
    // Follow has no acknowledgement. Let the deliberately fast fake finish,
    // then replay from the pre-creation head so setup cannot race the turn.
    tokio::time::sleep(Duration::from_secs(1)).await;
    client.send(&ClientMessage::Follow { since: head }).await?;

    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    let mut replies = 0_u64;
    let mut seen = HashSet::new();
    loop {
        let message = tokio::time::timeout_at(deadline, client.recv())
            .await
            .context("rig probe timed out")??;
        match message {
            ServerMessage::Log { entries } => {
                for entry in entries {
                    if !seen.insert(entry.seq) {
                        continue;
                    }
                    let completed_reply = matches!(
                        &entry.event,
                        MirrorEvent::Replied { calls, .. } if calls.is_empty()
                    );
                    if matches!(&entry.event, MirrorEvent::Replied { .. }) {
                        replies += 1;
                    }
                    if completed_reply {
                        let model: FakeModel = serde_json::from_str(
                            fs::read_to_string(root.join("logs/fake-model.log"))?
                                .lines()
                                .next()
                                .context("fake model readiness line is missing")?,
                        )?;
                        let metrics: FakeModelMetrics =
                            reqwest::get(format!("{}/metrics", model.anthropic_base_url))
                                .await?
                                .json()
                                .await?;
                        if replies == 0 || metrics.completed_turns == 0 {
                            bail!("rig turn ended without a fake-model reply");
                        }
                        println!(
                            "RIG_PROOF agent={:?} replies={replies} fake_completed_turns={} journal_head={}",
                            entry.agent_id, metrics.completed_turns, entry.seq.0
                        );
                        return Ok(());
                    }
                }
            }
            ServerMessage::Error { message } => bail!("rig probe failed: {message}"),
            _ => {}
        }
    }
}

/// The rig's environment: its own XDG dirs and nothing of the user's. The
/// state dir is the copied state, which is what makes the daemon run on the
/// snapshot rather than on the user's store.
///
/// `CLAUDE_CONFIG_DIR` is here for the binaries that still read it (the `rho`
/// CLI); the daemon is told its Claude directory by argument instead, so a
/// rig daemon is sealed whether or not it inherits this environment.
fn command(program: PathBuf, root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("CLAUDE_CONFIG_DIR", root.join("config").join("claude"))
        .env("HOME", root);
    command
}

/// Where the binaries come from, resolved once so a missing one is reported
/// before anything is started.
struct Build {
    dir: PathBuf,
    label: String,
    /// The nix profile has no `rho-gui`, and examples are only ever in a
    /// cargo target dir.
    fallback: Option<PathBuf>,
}

impl Build {
    fn rho(&self) -> PathBuf {
        self.dir.join("rho")
    }

    fn daemon(&self) -> PathBuf {
        self.dir.join("rho-daemon")
    }

    fn gui(&self) -> PathBuf {
        self.find("rho-gui")
    }

    fn fake_slack(&self) -> PathBuf {
        self.find("examples/fake_slack")
    }

    fn fake_model(&self) -> PathBuf {
        self.find("rho-fake-model")
    }

    fn fake_browser(&self) -> PathBuf {
        self.find("examples/fake_browser")
    }

    fn find(&self, relative: &str) -> PathBuf {
        let path = self.dir.join(relative);
        if path.exists() {
            return path;
        }
        match &self.fallback {
            Some(dir) => dir.join(relative),
            None => path,
        }
    }
}

fn binaries(which: Binaries, gui: bool) -> Result<Build> {
    let target = repo_root()?.join("target");
    let (dir, label, fallback) = match which {
        Binaries::Profiling => (target.join("profiling"), "profiling", None),
        Binaries::Release => (target.join("release"), "release", None),
        Binaries::Debug => (target.join("debug"), "debug", None),
        Binaries::Nix => (
            dirs::home_dir()
                .context("home directory not available")?
                .join(".nix-profile")
                .join("bin"),
            "nix",
            // The nix profile ships `rho` and `rho-daemon` only; the GUI and
            // the fakes come from the tree either way.
            Some(target.join("profiling")),
        ),
    };
    let bin = Build {
        dir,
        label: label.to_owned(),
        fallback,
    };
    // The same list the build builds from, so a binary that is wanted here
    // is one `rho-qa build` makes. Only a run that starts the GUI needs the
    // GUI, the driver and the browser the client launches.
    let mut missing = Vec::new();
    for binary in crate::build::RIG_BINARIES {
        if binary.gui_only && !gui {
            continue;
        }
        let path = bin.find(binary.file);
        if !path.exists() {
            missing.push(path.display().to_string());
        }
    }
    if !missing.is_empty() {
        bail!(
            "missing binaries:\n  {}\n{}",
            missing.join("\n  "),
            crate::build::instructions(which.profile())
        );
    }
    Ok(bin)
}

/// What a binary is, so a session can be believed.
///
/// Every number the rig has ever produced was attributed to a commit on the
/// assumption that the binaries were built from it. On 2026-09-07 five
/// sessions ran a GUI three hours older than the tree and were reported as a
/// commit that was never in them, which withdrew a crash result and a whole
/// table of frame numbers. Nothing had ever checked.
struct Identity {
    name: &'static str,
    path: PathBuf,
    /// A short content hash. The mtime says when it was written; this says
    /// whether it is the same binary as last time, which is the question
    /// when a rebuild silently does nothing.
    hash: String,
    modified: Option<std::time::SystemTime>,
}

impl Identity {
    fn of(name: &'static str, path: PathBuf) -> Self {
        let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        // The whole file, hashed. These are hundreds of megabytes and this
        // runs once per `rig up`, against a session that takes half a
        // minute to become usable — the cost is not worth avoiding, and a
        // hash of the first block would miss exactly the case that matters.
        let hash = fs::read(&path)
            .map(|bytes| {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hasher::write(&mut hasher, &bytes);
                format!("{:016x}", std::hash::Hasher::finish(&hasher))
            })
            .unwrap_or_else(|_| "missing".to_owned());
        Self {
            name,
            path,
            hash,
            modified,
        }
    }

    fn line(&self) -> String {
        format!(
            "  {:<12} {} {}  {}",
            self.name,
            self.hash,
            self.modified
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| format!("mtime {}", since.as_secs()))
                .unwrap_or_else(|| "mtime unknown".to_owned()),
            self.path.display(),
        )
    }
}

/// The newest thing that could change a binary.
///
/// Scoped to `crates/`, `vendor/` and `Cargo.lock` rather than the whole
/// tree: a doc-only edit is most of some engineers' commits and must not
/// make every binary look stale, or the override becomes habit and the
/// check becomes noise.
fn newest_source(root: &Path) -> Option<(std::time::SystemTime, PathBuf)> {
    fn walk(dir: &Path, newest: &mut Option<(std::time::SystemTime, PathBuf)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                // `target` is output, not source, and walking it would take
                // longer than the rest of the tree put together.
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                walk(&path, newest);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(at) = entry.metadata().and_then(|meta| meta.modified())
                && newest.as_ref().is_none_or(|(held, _)| at > *held)
            {
                *newest = Some((at, path));
            }
        }
    }

    let mut newest = None;
    walk(&root.join("crates"), &mut newest);
    walk(&root.join("vendor"), &mut newest);
    if let Ok(at) = fs::metadata(root.join("Cargo.lock")).and_then(|meta| meta.modified())
        && newest.as_ref().is_none_or(|(held, _)| at > *held)
    {
        newest = Some((at, root.join("Cargo.lock")));
    }
    newest
}

fn repo_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("RHO_QA_REPO") {
        return Ok(PathBuf::from(dir));
    }
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("Cargo.lock").exists() && dir.join("crates").is_dir() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("no rho checkout above the working directory; set RHO_QA_REPO");
        }
    }
}

/// The fake takes any token; what matters is that the client finds a file
/// where it looks, so nothing reaches for the user's real Slack session.
/// The client's Slack session, written where `RHO_SLACK_CREDENTIALS` points.
///
/// Keyed by workspace name, and the name has to be the one the fake came up
/// as: a session is per workspace, so credentials for `rig` against a fake
/// serving `acme` leave the client with no session for anything it can see,
/// and every Slack row the desk ever held reads as Open with no rule able to
/// close it. That looks exactly like a dealing bug and is not one.
fn write_credentials(root: &Path, workspace: &str) -> Result<()> {
    let path = root.join("credentials.json");
    let stored = serde_json::json!({
        "workspaces": { workspace: { "token": "xoxc-fake", "cookie": "fake" } }
    });
    fs::write(&path, serde_json::to_string(&stored)?)
        .with_context(|| format!("write {}", path.display()))?;
    fs::set_permissions(&path, permissions(0o600))?;
    Ok(())
}

fn write_model_credentials(root: &Path) -> Result<()> {
    let dir = root.join("state").join("rho").join("auth.d");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, permissions(0o700))?;
    let path = dir.join("default.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "access_token": "rho-qa-synthetic-default-token",
            "expires_at_ms": u64::MAX,
            "account_id": "rho-qa-synthetic-default-account",
            "client_secret": vec![0u8; 32],
        }))?,
    )?;
    fs::set_permissions(&path, permissions(0o600))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn rig_root(name: &str) -> Result<PathBuf> {
    let root = paths::rigs_root()?.join(name);
    if !root.join("rig.json").exists() {
        bail!("no rig at {}", root.display());
    }
    Ok(root)
}

fn load(root: &Path) -> Result<Rig> {
    let path = root.join("rig.json");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn save(root: &Path, rig: &Rig) -> Result<()> {
    fs::write(root.join("rig.json"), serde_json::to_string_pretty(rig)?)?;
    Ok(())
}

fn state_size(root: &Path) -> String {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(&entry.path()),
                Ok(kind) if kind.is_file() => entry.metadata().map(|meta| meta.len()).unwrap_or(0),
                _ => 0,
            })
            .sum()
    }
    human(walk(&root.join("state")))
}

fn wait_for(path: &Path, what: &str) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while !path.exists() {
        if Instant::now() > deadline {
            bail!("{what} never appeared at {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn daemon_pid(root: &Path) -> Option<u32> {
    read_pid(&root.join("run").join("daemon.pid"))
}

/// The last `lines` lines of a log, for an error that should not need the
/// reader to go and look.
fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let kept: Vec<&str> = text.lines().rev().take(lines).collect();
    kept.into_iter().rev().collect::<Vec<_>>().join("\n")
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// A plain `SIGTERM`, without pulling in a libc dependency for one call.
fn terminate(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn permissions(mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt as _;
    fs::Permissions::from_mode(mode)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_fake_that_is_listening_but_holds_no_workspace_is_not_a_session() {
        let listening = "RHO_SLACK_API_BASE=http://127.0.0.1:1/api\nws=ws://127.0.0.1:2\n";
        assert!(
            fake_slack_from_log(listening, None).is_none(),
            "a fake with no workspace must not be taken for a Slack session"
        );

        let serving =
            format!("{listening}control=http://127.0.0.1:1/control\nworkspace=acme\nready\n");
        let fake = fake_slack_from_log(&serving, None).expect("a workspace and an api base");
        assert_eq!(fake.workspace, "acme");
        assert_eq!(fake.api_base, "http://127.0.0.1:1/api");

        let fixture = format!("{listening}ready\n");
        assert_eq!(
            fake_slack_from_log(&fixture, Some("acme"))
                .expect("the built-in fixture has a known workspace")
                .workspace,
            "acme"
        );
    }

    /// The session a rig writes for the client has to name the workspace the
    /// fake came up as, or the client has no session for what it can see.
    #[test]
    fn the_credentials_name_the_fake_s_workspace() {
        let dir = std::env::temp_dir().join(format!("rho-qa-credentials-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        write_credentials(&dir, "acme").expect("write credentials");
        let written = fs::read_to_string(dir.join("credentials.json")).expect("read back");
        assert!(written.contains("\"acme\""), "{written}");
        assert!(!written.contains("\"rig\""), "{written}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_credentials_live_only_under_the_rig_state() {
        let dir =
            std::env::temp_dir().join(format!("rho-qa-model-credentials-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        write_model_credentials(&dir).expect("write credentials");
        let path = dir.join("state/rho/auth.d/default.json");
        let written = fs::read_to_string(&path).expect("read back");
        assert!(written.contains("rho-qa-synthetic-default-token"));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// The staleness check, shown to fail before it is trusted to pass.
    ///
    /// R5: an instrument that has only ever said "fine" has not been shown
    /// capable of saying anything else. This builds a tree where a source
    /// file is newer than a binary and asserts the comparison catches it,
    /// then makes the binary newer and asserts it does not — the same
    /// check, both answers, so a green `rig up` means something.
    #[test]
    fn a_binary_older_than_its_sources_is_seen_as_older() {
        let dir = std::env::temp_dir().join(format!("rho-qa-staleness-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(dir.join("crates").join("thing").join("src")).expect("make a tree");

        let binary = dir.join("binary");
        fs::write(&binary, b"a binary").expect("write the binary");
        // Sleep-free: set the times explicitly, so the test is about the
        // comparison and not about how fast the filesystem's clock ticks.
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let new = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000);
        let source = dir.join("crates").join("thing").join("src").join("lib.rs");
        fs::write(&source, b"fn main() {}").expect("write the source");

        set_modified(&binary, old);
        set_modified(&source, new);
        let (newest_at, newest_path) = super::newest_source(&dir).expect("a source was found");
        assert_eq!(newest_path, source, "it must name the file it found");
        let identity = super::Identity::of("binary", binary.clone());
        assert!(
            identity.modified.expect("the binary has an mtime") < newest_at,
            "a binary written before its sources must read as older"
        );

        // The other answer. Without this the assertion above could hold for
        // a check that always says "older".
        set_modified(&binary, new + std::time::Duration::from_secs(1));
        let identity = super::Identity::of("binary", binary);
        assert!(
            identity.modified.expect("the binary has an mtime") > newest_at,
            "a binary written after its sources must read as newer"
        );

        // And a doc-only change must not make anything look stale, or the
        // override becomes habit and the check becomes noise.
        let doc = dir.join("crates").join("thing").join("NOTES.md");
        fs::write(&doc, b"words").expect("write the doc");
        set_modified(&doc, new + std::time::Duration::from_secs(600));
        let (after_doc, _) = super::newest_source(&dir).expect("a source is still found");
        assert_eq!(
            after_doc, newest_at,
            "a markdown file is not something a binary is built from"
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// The steps counted are the ones under the last drive named, and a
    /// run with no name is reported as having none rather than as zero
    /// steps of something.
    ///
    /// Both answers are shown before either is trusted: the same log read
    /// with and without a second `drive` line gives different counts, so
    /// the reset is doing work rather than the count happening to be right.
    #[test]
    fn steps_are_counted_under_the_drive_they_ran_for() {
        let dir = std::env::temp_dir().join(format!("rho-qa-drive-{}", std::process::id()));
        let session = dir.join("run").join("rho-wayland");
        fs::create_dir_all(&session).expect("make the session directory");
        let log = session.join("desk-drive.log");

        // Two steps before anything is named: no name, and the count is
        // still reported, because steps nobody named still happened.
        fs::write(
            &log,
            b"{\"at_ms\":1,\"step\":\"key alt+d\"}\n{\"at_ms\":2,\"step\":\"key j\"}\n",
        )
        .expect("write the log");
        let (unnamed, before) = super::drive_taken(&dir, "desk");
        assert_eq!(unnamed, None, "nothing named the drive");
        assert_eq!(before, 2, "steps with no name are still steps");

        // A name, then three steps: the count starts again under the name.
        fs::write(
            &log,
            b"{\"at_ms\":1,\"step\":\"key alt+d\"}\n\
              {\"at_ms\":2,\"drive\":\"09:12 recipe\"}\n\
              {\"at_ms\":3,\"step\":\"key j\"}\n\
              {\"at_ms\":4,\"step\":\"key enter\"}\n\
              {\"at_ms\":5,\"click 10 20\":\"ignored\"}\n\
              {\"at_ms\":6,\"step\":\"click 10 20\"}\n",
        )
        .expect("write the named log");
        let (named, after) = super::drive_taken(&dir, "desk");
        assert_eq!(named.as_deref(), Some("09:12 recipe"));
        assert_eq!(
            after, 3,
            "only the steps after the name belong to it, and a line that is \
             neither a step nor a name is not counted as either"
        );

        // A log that is not there at all is the third answer, and it is
        // the one a report has to say out loud.
        let (missing, none) = super::drive_taken(&dir, "no-such-session");
        assert_eq!((missing, none), (None, 0));

        fs::remove_dir_all(&dir).ok();
    }

    /// A rig that nobody has driven says so, and one that somebody has
    /// says when and what.
    ///
    /// The never-driven answer is checked first and on purpose: it is the
    /// one the found-idle session would have given, and a reading that
    /// cannot tell it from a live run is the whole gap this closes.
    #[test]
    fn a_rig_says_whether_anyone_has_driven_it() {
        let dir = std::env::temp_dir().join(format!("rho-qa-touch-{}", std::process::id()));
        let session = dir.join("run").join("rho-wayland");
        let screens = dir.join("screens");
        fs::create_dir_all(&session).expect("make the session directory");
        fs::create_dir_all(&screens).expect("make the screens directory");

        assert_eq!(super::last_touch(&dir, "desk"), None);
        assert!(
            super::touch_line(&dir, "desk").starts_with("never driven"),
            "a session nobody drove has to say so in words: {}",
            super::touch_line(&dir, "desk")
        );

        // A screenshot and nothing else: the thread the found-idle session
        // was read from, now read by the rig itself.
        let shot = screens.join("case.png");
        fs::write(&shot, b"png").expect("write a screenshot");
        set_modified(
            &shot,
            std::time::SystemTime::now() - Duration::from_secs(3 * 3600),
        );
        let (what, since) = super::last_touch(&dir, "desk").expect("the screenshot is a touch");
        assert_eq!(what, "screenshot case.png");
        assert_eq!(super::since_label(since), "3h00m");

        // A step after it wins, because it is newer, and the drive log is
        // the better witness when both are there.
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_millis() as u64
            - 90_000;
        fs::write(
            session.join("desk-drive.log"),
            format!("{{\"at_ms\":{at_ms},\"step\":\"key j\"}}\n").as_bytes(),
        )
        .expect("write the log");
        let (what, since) = super::last_touch(&dir, "desk").expect("the step is a touch");
        assert_eq!(what, "key j");
        assert_eq!(super::since_label(since), "1m");

        fs::remove_dir_all(&dir).ok();
    }

    /// The units a person answers "is anyone on this" in.
    #[test]
    fn how_long_ago_is_said_in_the_coarsest_unit_that_fits() {
        assert_eq!(super::since_label(Duration::from_secs(9)), "9s");
        assert_eq!(super::since_label(Duration::from_secs(60)), "1m");
        assert_eq!(super::since_label(Duration::from_secs(3599)), "59m");
        assert_eq!(super::since_label(Duration::from_secs(8340)), "2h19m");
    }

    fn set_modified(path: &std::path::Path, at: std::time::SystemTime) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open to set the time");
        file.set_modified(at).expect("set the time");
    }
}
