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

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

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
    /// What the GUI was told to profile to, so `rig down` knows which files
    /// this session left behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<PathBuf>,
    /// What those files said, written here on the way down so a landing
    /// note can quote the run rather than re-derive it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<Summary>,
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
    // Anything still running from the last session is the last session's, not
    // this one's.
    stop_gui(&root, &bin, &args.name);
    stop_daemon(&root);

    let runtime = root.join("run");
    fs::create_dir_all(runtime.join("rho"))?;
    fs::set_permissions(&runtime, permissions(0o700))?;
    let socket = runtime.join("rho").join("rho.sock");
    let _ = fs::remove_file(&socket);

    // The rig daemon is its own node: no `--iroh`, no identity of the user's.
    let log = fs::File::create(root.join("logs").join("daemon.log"))?;
    let daemon = command(bin.daemon(), &root)
        .arg("--socket-path")
        .arg(&socket)
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
        profile: profile_path.clone(),
        summary: None,
    });
    save(&root, &rig)?;
    println!(
        "\nrig {} up on {} binaries — session {} of this desk",
        args.name,
        bin.label,
        rig.sessions.len()
    );
    println!(
        "drive it with: rho wayland --session {} <key|input|screenshot|tree>",
        args.name
    );
    Ok(())
}

fn down(name: &str) -> Result<()> {
    let root = rig_root(name)?;
    let bin = binaries(Binaries::Profiling, false).or_else(|_| binaries(Binaries::Nix, false))?;
    stop_gui(&root, &bin, name);
    file_application_log(&root, name);
    stop_daemon(&root);
    println!("rig {name} down; its state is as the run left it");
    summarize_session(&root)
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
        if let Some(summary) = &last.summary {
            println!("  profile  {}", summary.line);
        }
    }
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

fn start_fake_slack(root: &Path, bin: &Build) -> Result<FakeSlack> {
    let path = root.join("logs").join("fake-slack.log");
    let log = fs::File::create(&path)?;
    let mirror = root.join("state").join("rho").join("slack.redb");
    let mut process = if mirror.exists() {
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
        if let Some(fake) = fake_slack_from_log(&text) {
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
fn fake_slack_from_log(text: &str) -> Option<FakeSlack> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::to_owned)
    };
    Some(FakeSlack {
        api_base: field("RHO_SLACK_API_BASE=")?,
        workspace: field("workspace=")?,
    })
}

/// Who is running this rig, for the session line. The agent handle if this is
/// an agent's shell, the user otherwise, and nothing rather than a guess.
fn holder() -> Option<String> {
    ["RHO_MCP_AGENT_ID", "RHO_AGENT_ID", "USER"]
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
    Some(format!(
        "rig {} is already up: session {}, held by {whose}, started {at} on \
         {binaries} binaries (daemon pid {pid}).\n{what_to_do}",
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
    for name in ["daemon.pid", "fake-slack.pid"] {
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

/// The rig's environment: its own XDG dirs and nothing of the user's. The
/// state dir is the copied state, which is what makes the daemon run on the
/// snapshot rather than on the user's store.
fn command(program: PathBuf, root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
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
    let mut wanted = vec![bin.daemon(), bin.fake_slack()];
    if gui {
        // Only a run that starts the GUI needs the GUI, the driver and the
        // browser the client launches.
        wanted.extend([bin.rho(), bin.gui(), bin.fake_browser()]);
    }
    let mut missing = Vec::new();
    for path in wanted {
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
    use super::*;

    #[test]
    fn a_fake_that_is_listening_but_holds_no_workspace_is_not_a_session() {
        let listening = "RHO_SLACK_API_BASE=http://127.0.0.1:1/api\nws=ws://127.0.0.1:2\n";
        assert!(
            fake_slack_from_log(listening).is_none(),
            "a fake with no workspace must not be taken for a Slack session"
        );

        let serving =
            format!("{listening}control=http://127.0.0.1:1/control\nworkspace=acme\nready\n");
        let fake = fake_slack_from_log(&serving).expect("a workspace and an api base");
        assert_eq!(fake.workspace, "acme");
        assert_eq!(fake.api_base, "http://127.0.0.1:1/api");
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
}
