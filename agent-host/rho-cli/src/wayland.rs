//! Headless Wayland desktop control: forwards compositor driver commands to
//! `rho-agent-desktop` and handles `record` directly over the local desktop
//! protocol.
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _, RawFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use clap::Parser as _;
use rho_desktop_media::webm::{WebmCodec, WebmRecorder};
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;

const DEFAULT_SESSION: &str = "default";
const DEFAULT_FPS: u32 = 15;
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, clap::Args)]
#[command(disable_help_flag = true)]
pub struct WaylandArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

pub fn run(args: WaylandArgs) -> Result<()> {
    anyhow::ensure!(cfg!(target_os = "linux"), "agent desktops require Linux");
    match classify_invocation(&args.args)? {
        Invocation::Record(record_args) => {
            let cli = RecordCli::try_parse_from(record_args).unwrap_or_else(|error| error.exit());
            run_record(cli)
        }
        Invocation::Forward { print_record_help } => {
            let program = desktop_program();
            let status = Command::new(&program)
                .arg("wayland")
                .args(&args.args)
                .status()
                .with_context(|| format!("start bundled {}", program.display()))?;
            anyhow::ensure!(status.success(), "rho-agent-desktop exited with {status}");
            if print_record_help {
                println!(
                    "\nRecording:\n  \
                     record      Record the virtual output to a playable WebM video \
                     (`rho wayland record --help`)"
                );
            }
            Ok(())
        }
    }
}

fn desktop_program() -> PathBuf {
    std::env::var_os("RHO_AGENT_DESKTOP_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| rho_fs_view::agent_base().join("bin/rho-agent-desktop"))
}

enum Invocation {
    Forward { print_record_help: bool },
    Record(Vec<OsString>),
}

/// Inspect leading `--session` / `--state-dir` options before the Wayland
/// subcommand so `rho wayland --session S record ...` and
/// `rho wayland record ...` both route to the recorder, while
/// `rho wayland --session record start` still forwards to `rho-agent-desktop`.
fn classify_invocation(args: &[OsString]) -> Result<Invocation> {
    let mut prefix = Vec::new();
    let mut index = 0usize;
    let mut help_requested = false;

    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        let s = arg.to_string_lossy();
        if s == "--session" || s == "--state-dir" {
            prefix.push(arg.clone());
            index += 1;
            if index >= args.len() {
                return Ok(Invocation::Forward {
                    print_record_help: false,
                });
            }
            prefix.push(args[index].clone());
            index += 1;
            continue;
        }
        if s.starts_with("--session=") || s.starts_with("--state-dir=") {
            prefix.push(arg.clone());
            index += 1;
            continue;
        }
        if s == "-h" || s == "--help" {
            help_requested = true;
            index += 1;
            continue;
        }
        if s.starts_with('-') {
            return Ok(Invocation::Forward {
                print_record_help: false,
            });
        }
        if s == "record" {
            let mut argv = Vec::with_capacity(1 + prefix.len() + (args.len() - index - 1));
            argv.push(OsString::from("rho wayland record"));
            argv.extend(prefix);
            argv.extend_from_slice(&args[index + 1..]);
            return Ok(Invocation::Record(argv));
        }
        return Ok(Invocation::Forward {
            print_record_help: false,
        });
    }

    Ok(Invocation::Forward {
        print_record_help: help_requested,
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
enum CodecArg {
    #[default]
    Vp9,
    Vp8,
}

impl From<CodecArg> for WebmCodec {
    fn from(value: CodecArg) -> Self {
        match value {
            CodecArg::Vp9 => WebmCodec::Vp9,
            CodecArg::Vp8 => WebmCodec::Vp8,
        }
    }
}

#[derive(Clone, Debug, clap::Parser)]
#[command(
    name = "rho wayland record",
    about = "Record a headless Wayland session output to a browser-playable WebM video"
)]
struct RecordCli {
    /// Desktop name, scoped to the invoking agent.
    #[arg(long = "session", global = true, default_value = DEFAULT_SESSION)]
    session: String,
    /// Directory containing driver sessions.
    #[arg(long = "state-dir", global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<RecordSubcommand>,
    #[command(flatten)]
    capture: RecordCaptureArgs,
}

#[derive(Clone, Debug, clap::Subcommand)]
enum RecordSubcommand {
    /// Start recording in the background until `record stop`.
    Start(RecordStartArgs),
    /// Stop an active background recording and finalize the WebM file.
    Stop,
    /// Internal background worker entrypoint.
    #[command(name = "__worker", hide = true)]
    Worker(RecordWorkerArgs),
}

#[derive(Clone, Debug, clap::Args)]
struct RecordCaptureArgs {
    /// Output WebM file path.
    #[arg(long, short = 'o', value_name = "PATH")]
    output: Option<PathBuf>,
    /// Stop automatically after this many seconds (otherwise record until
    /// SIGINT/SIGTERM).
    #[arg(long, value_name = "SECS")]
    duration: Option<f64>,
    /// Target capture frame rate (1-60, default 15).
    #[arg(long, value_name = "N")]
    fps: Option<u32>,
    /// Video codec (`vp9` or `vp8`, default `vp9`).
    #[arg(long, value_enum)]
    codec: Option<CodecArg>,
}

#[derive(Clone, Debug, clap::Args)]
struct RecordStartArgs {
    /// Output WebM file path.
    #[arg(long, short = 'o', value_name = "PATH")]
    output: PathBuf,
    /// Target capture frame rate (1-60).
    #[arg(long, default_value_t = DEFAULT_FPS, value_name = "N")]
    fps: u32,
    /// Video codec (`vp9` or `vp8`).
    #[arg(long, value_enum, default_value_t = CodecArg::Vp9)]
    codec: CodecArg,
}

#[derive(Clone, Debug, clap::Args)]
struct RecordWorkerArgs {
    #[arg(long, short = 'o')]
    output: PathBuf,
    #[arg(long)]
    fps: u32,
    #[arg(long, value_enum)]
    codec: CodecArg,
    #[arg(long)]
    ready_fd: RawFd,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecordState {
    pid: u32,
    output: PathBuf,
    fps: u32,
}

#[derive(Debug)]
struct RecordSummary {
    frames: u64,
    duration_ms: u64,
    bytes: u64,
}

fn validate_session_name(session: &str) -> Result<()> {
    ensure!(
        !session.is_empty()
            && session != "."
            && session != ".."
            && !session.contains('/')
            && !session.contains('\\'),
        "invalid desktop session name: {session:?}"
    );
    Ok(())
}

fn validate_fps(fps: u32) -> Result<()> {
    ensure!((1..=60).contains(&fps), "fps must be between 1 and 60");
    Ok(())
}

fn resolve_state_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is required when --state-dir is omitted")?;
    let base = runtime.join("rho-desktop");
    if let Some(agent) = std::env::var_os("RHO_AGENT_ID")
        && !agent.is_empty()
    {
        return Ok(base.join("agents").join(agent));
    }
    Ok(base)
}

fn session_socket(state_dir: &Path, session: &str) -> Result<String> {
    let descriptor_path = state_dir.join(format!("{session}.json"));
    let bytes = std::fs::read(&descriptor_path).with_context(|| {
        format!(
            "wayland session {session:?} is not running (missing {})",
            descriptor_path.display()
        )
    })?;
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid session descriptor {}", descriptor_path.display()))?;
    descriptor["socket"]
        .as_str()
        .map(str::to_owned)
        .context("session descriptor missing desktop socket")
}

fn record_state_path(state_dir: &Path, session: &str) -> PathBuf {
    state_dir.join(format!("{session}-record.json"))
}

fn record_lock_path(state_dir: &Path, session: &str) -> PathBuf {
    state_dir.join(format!("{session}-record.lock"))
}

fn record_log_path(state_dir: &Path, session: &str) -> PathBuf {
    state_dir.join(format!("{session}-record.log"))
}

fn try_lock_exclusive(file: &File) -> Result<bool> {
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(err).context("flock recording lock")
}

fn run_record(cli: RecordCli) -> Result<()> {
    validate_session_name(&cli.session)?;
    let state_dir = resolve_state_dir(cli.state_dir.as_deref())?;

    if cli.command.is_some()
        && (cli.capture.output.is_some()
            || cli.capture.duration.is_some()
            || cli.capture.fps.is_some()
            || cli.capture.codec.is_some())
    {
        bail!(
            "capture flags (--output, --duration, --fps, --codec) cannot be combined with a subcommand"
        );
    }

    match cli.command {
        Some(RecordSubcommand::Start(start)) => {
            validate_fps(start.fps)?;
            start_background(&state_dir, &cli.session, start)
        }
        Some(RecordSubcommand::Stop) => stop_background(&state_dir, &cli.session),
        Some(RecordSubcommand::Worker(worker)) => {
            validate_fps(worker.fps)?;
            run_worker(&state_dir, &cli.session, worker)
        }
        None => {
            let output = cli.capture.output.context(
                "--output <PATH> is required (or use `rho wayland record start` / `stop`)",
            )?;
            let fps = cli.capture.fps.unwrap_or(DEFAULT_FPS);
            let codec = cli.capture.codec.unwrap_or_default();
            validate_fps(fps)?;
            let duration = match cli.capture.duration {
                Some(secs) => {
                    ensure!(
                        secs.is_finite() && secs > 0.0,
                        "--duration must be a positive number of seconds"
                    );
                    Some(Duration::from_secs_f64(secs))
                }
                None => None,
            };
            let output = std::path::absolute(&output).unwrap_or(output);
            let socket = session_socket(&state_dir, &cli.session)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let summary = runtime.block_on(record_loop(
                &socket,
                &output,
                fps,
                codec.into(),
                duration,
                None,
            ))?;
            println!(
                "{}",
                serde_json::json!({
                    "bytes": summary.bytes,
                    "duration_ms": summary.duration_ms,
                    "frames": summary.frames,
                    "output": output,
                    "session": cli.session,
                })
            );
            Ok(())
        }
    }
}

fn start_background(state_dir: &Path, session: &str, start: RecordStartArgs) -> Result<()> {
    let _ = session_socket(state_dir, session)?;
    let lock_path = record_lock_path(state_dir, session);
    if lock_path.exists() {
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .context("open recording lock")?;
        if !try_lock_exclusive(&probe)? {
            bail!("recording already in progress for session {session:?}");
        }
    }

    let output = std::path::absolute(&start.output).unwrap_or(start.output);
    let log_path = record_log_path(state_dir, session);
    let log_file = File::create(&log_path)
        .with_context(|| format!("create recording log {}", log_path.display()))?;

    let (mut ready_read, ready_write) = std::io::pipe().context("create readiness pipe")?;
    let ready_fd = ready_write.into_raw_fd();

    let exe = std::env::current_exe().context("locate current rho executable")?;
    let mut cmd = Command::new(exe);
    cmd.args(["wayland", "record", "__worker"])
        .arg("--session")
        .arg(session)
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--output")
        .arg(&output)
        .arg("--fps")
        .arg(start.fps.to_string())
        .arg("--codec")
        .arg(match start.codec {
            CodecArg::Vp9 => "vp9",
            CodecArg::Vp8 => "vp8",
        })
        .arg("--ready-fd")
        .arg(ready_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file);

    // SAFETY: `setsid` and `fcntl(F_SETFD, 0)` are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let _ = libc::setsid();
            if libc::fcntl(ready_fd, libc::F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().context("spawn wayland recording worker")?;
    // Close parent's copy of the write end so EOF arrives if the worker exits
    // early.
    unsafe {
        drop(File::from_raw_fd(ready_fd));
    }

    let mut status_msg = String::new();
    ready_read
        .read_to_string(&mut status_msg)
        .context("read recording worker startup status")?;

    if status_msg.trim() != "OK" {
        let _ = child.wait();
        let detail = status_msg
            .trim()
            .strip_prefix("ERR:")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                std::fs::read_to_string(&log_path)
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            });
        if detail.is_empty() {
            bail!("recording worker failed to start");
        }
        bail!("{detail}");
    }

    println!(
        "{}",
        serde_json::json!({
            "output": output,
            "pid": child.id(),
            "recording": true,
            "session": session,
        })
    );
    Ok(())
}

fn run_worker(state_dir: &Path, session: &str, worker: RecordWorkerArgs) -> Result<()> {
    // SAFETY: `ready_fd` was inherited from `start_background` with CLOEXEC
    // cleared.
    let ready = std::rc::Rc::new(std::cell::RefCell::new(Some(unsafe {
        File::from_raw_fd(worker.ready_fd)
    })));
    let result = (|| -> Result<()> {
        let lock_path = record_lock_path(state_dir, session);
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("open recording lock {}", lock_path.display()))?;
        ensure!(
            try_lock_exclusive(&lock_file)?,
            "recording already in progress for session {session:?}"
        );

        let socket = session_socket(state_dir, session)?;
        let state_path = record_state_path(state_dir, session);
        let state = RecordState {
            pid: std::process::id(),
            output: worker.output.clone(),
            fps: worker.fps,
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        let ready_for_cb = ready.clone();
        let state_path_for_cb = state_path.clone();
        let on_ready = move || -> Result<()> {
            let payload = serde_json::to_vec(&state)?;
            std::fs::write(&state_path_for_cb, payload)?;
            if let Some(mut pipe) = ready_for_cb.borrow_mut().take() {
                pipe.write_all(b"OK\n")?;
                pipe.flush()?;
            }
            Ok(())
        };

        let run_result = runtime.block_on(record_loop(
            &socket,
            &worker.output,
            worker.fps,
            worker.codec.into(),
            None,
            Some(Box::new(on_ready)),
        ));
        let _ = std::fs::remove_file(&state_path);
        drop(lock_file);
        run_result.map(|_| ())
    })();

    if let Err(err) = &result
        && let Some(mut pipe) = ready.borrow_mut().take()
    {
        let _ = writeln!(pipe, "ERR:{err:#}");
        let _ = pipe.flush();
    }
    result
}

fn stop_background(state_dir: &Path, session: &str) -> Result<()> {
    let state_path = record_state_path(state_dir, session);
    let state_bytes = std::fs::read(&state_path)
        .with_context(|| format!("no active recording for session {session:?}"))?;
    let state: RecordState = serde_json::from_slice(&state_bytes)
        .with_context(|| format!("invalid recording state {}", state_path.display()))?;

    let lock_path = record_lock_path(state_dir, session);
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("missing recording lock {}", lock_path.display()))?;

    if try_lock_exclusive(&lock_file)? {
        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(&lock_path);
        bail!(
            "recording for session {session:?} is not running (pid {} already exited)",
            state.pid
        );
    }
    drop(lock_file);

    let pid = libc::pid_t::try_from(state.pid).context("invalid recorder pid")?;
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err).context("send SIGTERM to recording worker");
        }
    }

    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if let Ok(lock) = OpenOptions::new().read(true).write(true).open(&lock_path)
            && try_lock_exclusive(&lock)?
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for recording worker (pid {}) to finalize {}",
            state.pid,
            state.output.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let _ = std::fs::remove_file(&state_path);
    let _ = std::fs::remove_file(&lock_path);

    let meta = std::fs::metadata(&state.output).with_context(|| {
        format!(
            "recorded video missing at {} (check {})",
            state.output.display(),
            record_log_path(state_dir, session).display()
        )
    })?;
    ensure!(meta.len() > 0, "recorded video file is empty");

    println!(
        "{}",
        serde_json::json!({
            "bytes": meta.len(),
            "output": state.output,
            "session": session,
            "stopped": true,
        })
    );
    Ok(())
}

type ReadyCallback = Box<dyn FnOnce() -> Result<()>>;

async fn record_loop(
    socket: &str,
    output_path: &Path,
    fps: u32,
    codec: WebmCodec,
    duration: Option<Duration>,
    on_ready: Option<ReadyCallback>,
) -> Result<RecordSummary> {
    let mut control = BufReader::new(
        rho_desktop_proto::local::connect(socket)
            .await
            .with_context(|| format!("connect to wayland desktop socket {socket}"))?,
    );
    let rho_desktop_proto::Response::Hello {
        version: rho_desktop_proto::VERSION,
        outputs,
        ..
    } = rho_desktop_proto::local::request(
        &mut control,
        rho_desktop_proto::Request::Hello {
            version: rho_desktop_proto::VERSION,
        },
    )
    .await?
    else {
        bail!("desktop protocol version mismatch");
    };
    let output_name = outputs
        .first()
        .context("wayland desktop reported no outputs")?
        .name
        .clone();

    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(output_path)
        .with_context(|| format!("create video output {}", output_path.display()))?;

    let mut pixels = Vec::new();
    let (width, height) =
        rho_desktop_proto::local::capture(&mut control, &output_name, &mut pixels).await?;

    let mut recorder = WebmRecorder::new(BufWriter::new(file), width, height, fps, codec)?;
    let started = Instant::now();
    recorder.write_bgra_frame(0, &pixels)?;

    if let Some(notify) = on_ready {
        notify()?;
    }

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let frame_period = Duration::from_secs_f64(1.0 / f64::from(fps));
    let end_at = duration.map(|d| started + d);
    let mut next_tick = started + frame_period;
    let mut disconnected = false;

    loop {
        let wake_at = match end_at {
            Some(end) => next_tick.min(end),
            None => next_tick,
        };
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at));
        tokio::select! {
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
            _ = sleep => {}
        }

        if end_at.is_some_and(|end| Instant::now() >= end) {
            break;
        }

        let capture_result = tokio::select! {
            _ = sigint.recv() => {
                disconnected = true;
                break;
            }
            _ = sigterm.recv() => {
                disconnected = true;
                break;
            }
            res = rho_desktop_proto::local::capture(&mut control, &output_name, &mut pixels) => res,
        };

        match capture_result {
            Ok((w, h)) => {
                if (w, h) != recorder.dimensions() {
                    bail!(
                        "desktop output dimensions changed mid-recording ({}x{} -> {w}x{h})",
                        recorder.dimensions().0,
                        recorder.dimensions().1
                    );
                }
                let ts_ms = started.elapsed().as_millis() as u64;
                recorder.write_bgra_frame(ts_ms, &pixels)?;
            }
            Err(_) => {
                disconnected = true;
                break;
            }
        }

        next_tick += frame_period;
        let now = Instant::now();
        if now > next_tick + frame_period {
            next_tick = now + frame_period;
        }
    }

    // Capture one final frame on clean stop so the last UI state is preserved.
    if !disconnected
        && let Ok((w, h)) =
            rho_desktop_proto::local::capture(&mut control, &output_name, &mut pixels).await
        && (w, h) == recorder.dimensions()
    {
        let ts_ms = started.elapsed().as_millis() as u64;
        recorder.write_bgra_frame(ts_ms, &pixels)?;
    }

    let total_ms = started.elapsed().as_millis() as u64;
    let (writer, stats) = recorder.finish(Some(total_ms))?;
    let file = writer.into_inner()?;
    file.sync_all()?;

    Ok(RecordSummary {
        frames: stats.frames,
        duration_ms: stats.duration_ms,
        bytes: stats.bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn classifies_forward_vs_record_invocations() -> Result<()> {
        assert!(matches!(
            classify_invocation(&os(&["start", "--", "rho-gui"]))?,
            Invocation::Forward {
                print_record_help: false
            }
        ));
        assert!(matches!(
            classify_invocation(&os(&["--session", "record", "start"]))?,
            Invocation::Forward {
                print_record_help: false
            }
        ));
        assert!(matches!(
            classify_invocation(&os(&["--help"]))?,
            Invocation::Forward {
                print_record_help: true
            }
        ));

        let Invocation::Record(argv) = classify_invocation(&os(&[
            "--session",
            "gui",
            "--state-dir",
            "/tmp/s",
            "record",
            "start",
            "--output",
            "/tmp/out.webm",
            "--fps",
            "20",
        ]))?
        else {
            panic!("expected Record");
        };
        let parsed = RecordCli::try_parse_from(argv)?;
        assert_eq!(parsed.session, "gui");
        assert_eq!(parsed.state_dir, Some(PathBuf::from("/tmp/s")));
        assert!(matches!(
            parsed.command,
            Some(RecordSubcommand::Start(RecordStartArgs {
                fps: 20,
                codec: CodecArg::Vp9,
                ..
            }))
        ));

        let Invocation::Record(argv) = classify_invocation(&os(&[
            "record",
            "--session=demo",
            "--output",
            "clip.webm",
            "--duration",
            "2.5",
            "--codec",
            "vp8",
        ]))?
        else {
            panic!("expected Record");
        };
        let parsed = RecordCli::try_parse_from(argv)?;
        assert_eq!(parsed.session, "demo");
        assert!(parsed.command.is_none());
        assert_eq!(parsed.capture.output, Some(PathBuf::from("clip.webm")));
        assert_eq!(parsed.capture.duration, Some(2.5));
        assert_eq!(parsed.capture.fps, None);
        assert_eq!(parsed.capture.codec, Some(CodecArg::Vp8));

        let Invocation::Record(argv) =
            classify_invocation(&os(&["--session", "gui", "record", "stop"]))?
        else {
            panic!("expected Record");
        };
        let parsed = RecordCli::try_parse_from(argv)?;
        assert_eq!(parsed.session, "gui");
        assert!(matches!(parsed.command, Some(RecordSubcommand::Stop)));
        Ok(())
    }
}
