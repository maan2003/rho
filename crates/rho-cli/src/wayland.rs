//! Generic agent-friendly control of applications in a headless Sway session.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{io, thread};

use anyhow::{Context as _, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_OUTPUT_WIDTH: u32 = 2560;
const DEFAULT_OUTPUT_HEIGHT: u32 = 1664;
const DEFAULT_OUTPUT_SCALE: u32 = 2;
const SWAY: &str = match option_env!("RHO_WAYLAND_SWAY") {
    Some(path) => path,
    None => "sway",
};
const SWAYMSG: &str = match option_env!("RHO_WAYLAND_SWAYMSG") {
    Some(path) => path,
    None => "swaymsg",
};
const GRIM: &str = match option_env!("RHO_WAYLAND_GRIM") {
    Some(path) => path,
    None => "grim",
};
const WTYPE: &str = match option_env!("RHO_WAYLAND_WTYPE") {
    Some(path) => path,
    None => "wtype",
};

#[derive(Clone, clap::Args)]
pub(crate) struct WaylandArgs {
    /// Session name. Names may contain ASCII letters, digits, `-`, and `_`.
    #[arg(long, global = true, default_value = "default")]
    session: String,

    /// Directory containing driver sessions.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: DriverCommand,
}

#[derive(Clone, Subcommand)]
enum DriverCommand {
    /// Start a headless compositor, optionally followed by an application.
    Start {
        #[arg(long, default_value_t = DEFAULT_OUTPUT_WIDTH)]
        width: u32,
        #[arg(long, default_value_t = DEFAULT_OUTPUT_HEIGHT)]
        height: u32,
        #[arg(long, default_value_t = DEFAULT_OUTPUT_SCALE)]
        scale: u32,
        /// Application and arguments to launch in the Wayland session.
        #[arg(last = true)]
        command: Vec<OsString>,
    },
    /// Report whether the compositor and application are still running.
    Status,
    /// Print Sway's JSON window tree.
    Tree,
    /// Capture the virtual output as a PNG.
    Screenshot {
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Move the pointer to absolute output coordinates.
    Move { x: i32, y: i32 },
    /// Move the pointer and click a mouse button.
    Click {
        x: i32,
        y: i32,
        #[arg(long, value_enum, default_value_t = MouseButton::Left)]
        button: MouseButton,
    },
    /// Type literal text through the virtual keyboard protocol.
    Type { text: String },
    /// Send a key or chord, for example `enter` or `ctrl+shift+p`.
    Key { chord: String },
    /// Stop the application and compositor and remove the session directory.
    Stop,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn sway_name(self) -> &'static str {
        match self {
            Self::Left => "button1",
            Self::Right => "button3",
            Self::Middle => "button2",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Session {
    name: String,
    root: PathBuf,
    runtime_dir: PathBuf,
    ipc_socket: PathBuf,
    wayland_display: String,
    output: String,
    width: u32,
    height: u32,
    #[serde(default = "default_output_scale")]
    scale: u32,
    compositor: ProcessIdentity,
    application: Option<ProcessIdentity>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
}

#[derive(Serialize)]
struct StartResult<'a> {
    session: &'a str,
    state_dir: &'a Path,
    output: &'a str,
    width: u32,
    height: u32,
    scale: u32,
    logical_width: u32,
    logical_height: u32,
    compositor_pid: u32,
    application_pid: Option<u32>,
}

#[derive(Serialize)]
struct StatusResult<'a> {
    session: &'a str,
    state_dir: &'a Path,
    output: &'a str,
    width: u32,
    height: u32,
    scale: u32,
    logical_width: u32,
    logical_height: u32,
    compositor_running: bool,
    application_running: Option<bool>,
}

pub(crate) fn run(args: WaylandArgs) -> Result<()> {
    validate_session_name(&args.session)?;
    let base = match args.state_dir {
        Some(path) => path,
        None => default_state_dir()?,
    };
    let root = base.join(&args.session);

    match args.command {
        DriverCommand::Start {
            width,
            height,
            scale,
            command,
        } => start(&args.session, &root, width, height, scale, command),
        DriverCommand::Status => status(&root),
        DriverCommand::Tree => {
            let session = load_live_session(&root)?;
            let output = swaymsg(&session, ["-t", "get_tree", "-r"])?;
            print!("{output}");
            Ok(())
        }
        DriverCommand::Screenshot { output } => screenshot(&root, &output),
        DriverCommand::Move { x, y } => {
            let session = load_live_session(&root)?;
            move_pointer(&session, x, y)
        }
        DriverCommand::Click { x, y, button } => {
            let session = load_live_session(&root)?;
            move_pointer(&session, x, y)?;
            sway_command(
                &session,
                &format!("seat seat0 cursor press {}", button.sway_name()),
            )?;
            sway_command(
                &session,
                &format!("seat seat0 cursor release {}", button.sway_name()),
            )
        }
        DriverCommand::Type { text } => {
            let session = load_live_session(&root)?;
            run_wayland_command(&session, WTYPE, [OsString::from("--"), text.into()])
        }
        DriverCommand::Key { chord } => {
            let session = load_live_session(&root)?;
            let args = wtype_key_args(&chord)?;
            run_wayland_command(&session, WTYPE, args)
        }
        DriverCommand::Stop => stop(&root),
    }
}

fn default_state_dir() -> Result<PathBuf> {
    let runtime = dirs::runtime_dir().context("XDG_RUNTIME_DIR is not available")?;
    Ok(runtime.join("rho-wayland"))
}

fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid session name {name:?}; use ASCII letters, digits, `-`, or `_`");
    }
    Ok(())
}

fn start(
    name: &str,
    root: &Path,
    width: u32,
    height: u32,
    scale: u32,
    command: Vec<OsString>,
) -> Result<()> {
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        bail!("output dimensions must be between 1 and 16384 pixels");
    }
    if !(1..=4).contains(&scale) {
        bail!("output scale must be between 1 and 4");
    }
    if root.exists() {
        if load_session(root)
            .ok()
            .is_some_and(|session| process_is_running(session.compositor))
        {
            bail!("session {name:?} is already running");
        }
        fs::remove_dir_all(root).context("remove stale session directory")?;
    }

    let runtime_dir = root.join("runtime");
    fs::create_dir_all(&runtime_dir).context("create session runtime directory")?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;

    let config_path = root.join("sway.conf");
    fs::write(
        &config_path,
        format!(
            "output * mode {width}x{height}@60Hz scale {scale}\n\
             seat seat0 fallback enable\n\
             focus_follows_mouse no\n\
             default_border none\n\
             default_floating_border none\n\
             xwayland disable\n"
        ),
    )?;

    let sway_log = log_file(&root.join("sway.log"))?;
    let sway_err = sway_log.try_clone()?;
    let mut sway = Command::new(SWAY);
    sway.args([OsString::from("--config"), config_path.into_os_string()])
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("WLR_BACKENDS", "headless")
        .env("WLR_HEADLESS_OUTPUTS", "1")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env("WLR_RENDERER", "pixman")
        .stdin(Stdio::null())
        .stdout(Stdio::from(sway_log))
        .stderr(Stdio::from(sway_err));
    set_new_session(&mut sway);
    let mut sway = sway.spawn().context("start sway")?;

    let ready = wait_for_sway(&mut sway, &runtime_dir);
    let (ipc_socket, wayland_display, output) = match ready {
        Ok(ready) => ready,
        Err(error) => {
            terminate_pid(sway.id(), libc::SIGKILL);
            return Err(error).context(format!("see {}", root.join("sway.log").display()));
        }
    };
    let compositor = process_identity(sway.id()).context("identify sway process")?;

    let application = (|| -> Result<Option<ProcessIdentity>> {
        let Some((program, args)) = command.split_first() else {
            return Ok(None);
        };
        let app_log = log_file(&root.join("application.log"))?;
        let app_err = app_log.try_clone()?;
        let mut app = Command::new(program);
        app.args(args)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("WAYLAND_DISPLAY", &wayland_display)
            .env("SWAYSOCK", &ipc_socket)
            .stdin(Stdio::null())
            .stdout(Stdio::from(app_log))
            .stderr(Stdio::from(app_err));
        configure_software_rendering(&mut app);
        set_new_session(&mut app);
        let app = app
            .spawn()
            .with_context(|| format!("start application {program:?}"))?;
        Ok(Some(
            process_identity(app.id()).context("identify application process")?,
        ))
    })();
    let application = match application {
        Ok(application) => application,
        Err(error) => {
            terminate_process_group(compositor.pid, libc::SIGTERM);
            return Err(error);
        }
    };

    let session = Session {
        name: name.to_owned(),
        root: root.to_owned(),
        runtime_dir,
        ipc_socket,
        wayland_display,
        output,
        width,
        height,
        scale,
        compositor,
        application,
    };
    save_session(&session)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&StartResult {
            session: &session.name,
            state_dir: &session.root,
            output: &session.output,
            width,
            height,
            scale,
            logical_width: width / scale,
            logical_height: height / scale,
            compositor_pid: session.compositor.pid,
            application_pid: session.application.map(|process| process.pid),
        })?
    );
    Ok(())
}

fn wait_for_sway(
    child: &mut std::process::Child,
    runtime_dir: &Path,
) -> Result<(PathBuf, String, String)> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("sway exited during startup with {status}");
        }
        if let Some(ipc_socket) = find_sway_socket(runtime_dir)?
            && let Some(display) = find_wayland_display(runtime_dir)?
        {
            let result = Command::new(SWAYMSG)
                .args([
                    "--socket",
                    &ipc_socket.to_string_lossy(),
                    "-t",
                    "get_outputs",
                    "-r",
                ])
                .output();
            if let Ok(result) = result
                && result.status.success()
            {
                let outputs: Vec<serde_json::Value> =
                    serde_json::from_slice(&result.stdout).context("parse sway output list")?;
                if let Some(output) = outputs
                    .iter()
                    .find(|output| output["active"].as_bool() == Some(true))
                    .and_then(|output| output["name"].as_str())
                {
                    return Ok((ipc_socket, display, output.to_owned()));
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for sway to create an output");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn find_sway_socket(runtime_dir: &Path) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(runtime_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b"sway-ipc.") && entry.file_type()?.is_socket() {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn find_wayland_display(runtime_dir: &Path) -> Result<Option<String>> {
    for entry in fs::read_dir(runtime_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b"wayland-") && entry.file_type()?.is_socket() {
            return Ok(Some(name.to_string_lossy().into_owned()));
        }
    }
    Ok(None)
}

fn status(root: &Path) -> Result<()> {
    let session = load_session(root)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&StatusResult {
            session: &session.name,
            state_dir: &session.root,
            output: &session.output,
            width: session.width,
            height: session.height,
            scale: session.scale,
            logical_width: session.width / session.scale,
            logical_height: session.height / session.scale,
            compositor_running: process_is_running(session.compositor),
            application_running: session.application.map(process_is_running),
        })?
    );
    Ok(())
}

fn screenshot(root: &Path, output: &Path) -> Result<()> {
    let session = load_live_session(root)?;
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        bail!(
            "screenshot parent directory does not exist: {}",
            parent.display()
        );
    }
    run_wayland_command(
        &session,
        GRIM,
        [
            OsString::from("-o"),
            OsString::from(&session.output),
            output.as_os_str().to_owned(),
        ],
    )?;
    println!(
        "{}",
        serde_json::json!({
            "session": session.name,
            "output": output,
            "width": session.width,
            "height": session.height,
            "scale": session.scale,
            "logical_width": session.width / session.scale,
            "logical_height": session.height / session.scale,
        })
    );
    Ok(())
}

fn move_pointer(session: &Session, x: i32, y: i32) -> Result<()> {
    let logical_width = session.width / session.scale;
    let logical_height = session.height / session.scale;
    if x < 0 || y < 0 || x >= logical_width as i32 || y >= logical_height as i32 {
        bail!(
            "coordinates ({x}, {y}) are outside {}x{} output",
            logical_width,
            logical_height
        );
    }
    sway_command(session, &format!("seat seat0 cursor set {x} {y}"))
}

const fn default_output_scale() -> u32 {
    1
}

fn sway_command(session: &Session, command: &str) -> Result<()> {
    swaymsg(session, [command]).map(|_| ())
}

fn swaymsg<const N: usize>(session: &Session, args: [&str; N]) -> Result<String> {
    let output = Command::new(SWAYMSG)
        .args(["--socket", &session.ipc_socket.to_string_lossy()])
        .args(args)
        .output()
        .context("run swaymsg")?;
    checked_output("swaymsg", output.status, &output.stdout, &output.stderr)?;
    String::from_utf8(output.stdout).context("swaymsg returned non-UTF-8 output")
}

fn run_wayland_command(
    session: &Session,
    program: &str,
    args: impl IntoIterator<Item = OsString>,
) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .env("XDG_RUNTIME_DIR", &session.runtime_dir)
        .env("WAYLAND_DISPLAY", &session.wayland_display)
        .env("SWAYSOCK", &session.ipc_socket)
        .output()
        .with_context(|| format!("run {program}"))?;
    checked_output(program, output.status, &output.stdout, &output.stderr)
}

fn configure_software_rendering(command: &mut Command) {
    if std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
        command.env("LIBGL_ALWAYS_SOFTWARE", "1");
    }
    if std::env::var_os("VK_DRIVER_FILES").is_none()
        && let Some(driver) = option_env!("RHO_WAYLAND_VK_DRIVER_FILES")
    {
        command.env("VK_DRIVER_FILES", driver);
    }
}

fn checked_output(program: &str, status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    let detail = if stderr.is_empty() { stdout } else { stderr };
    bail!(
        "{program} exited with {status}: {}",
        String::from_utf8_lossy(detail).trim()
    )
}

fn wtype_key_args(chord: &str) -> Result<Vec<OsString>> {
    let mut parts: Vec<_> = chord.split('+').collect();
    let key = parts
        .pop()
        .filter(|key| !key.is_empty())
        .context("key chord is empty")?;
    let mut modifiers = Vec::new();
    for modifier in parts {
        let modifier = match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "ctrl",
            "alt" => "alt",
            "shift" => "shift",
            "super" | "logo" | "meta" => "logo",
            _ => bail!("unsupported key modifier {modifier:?}"),
        };
        modifiers.push(modifier);
    }
    let key = match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => "Return".to_owned(),
        "esc" | "escape" => "Escape".to_owned(),
        "tab" => "Tab".to_owned(),
        "backspace" => "BackSpace".to_owned(),
        "delete" | "del" => "Delete".to_owned(),
        "space" => "space".to_owned(),
        "up" => "Up".to_owned(),
        "down" => "Down".to_owned(),
        "left" => "Left".to_owned(),
        "right" => "Right".to_owned(),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        "pageup" => "Prior".to_owned(),
        "pagedown" => "Next".to_owned(),
        other => other.to_owned(),
    };

    let mut args = Vec::new();
    for modifier in &modifiers {
        args.extend([OsString::from("-M"), OsString::from(modifier)]);
    }
    args.extend([OsString::from("-k"), OsString::from(key)]);
    for modifier in modifiers.iter().rev() {
        args.extend([OsString::from("-m"), OsString::from(modifier)]);
    }
    Ok(args)
}

fn stop(root: &Path) -> Result<()> {
    let session = load_session(root)?;
    if let Some(application) = session
        .application
        .filter(|process| process_is_running(*process))
    {
        terminate_process_group(application.pid, libc::SIGTERM);
    }
    if process_is_running(session.compositor) {
        terminate_process_group(session.compositor.pid, libc::SIGTERM);
    }

    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline
        && (process_is_running(session.compositor)
            || session.application.is_some_and(process_is_running))
    {
        thread::sleep(Duration::from_millis(50));
    }
    if let Some(application) = session
        .application
        .filter(|process| process_is_running(*process))
    {
        terminate_process_group(application.pid, libc::SIGKILL);
    }
    if process_is_running(session.compositor) {
        terminate_process_group(session.compositor.pid, libc::SIGKILL);
    }
    fs::remove_dir_all(root).context("remove session directory")?;
    println!(
        "{}",
        serde_json::json!({ "session": session.name, "stopped": true })
    );
    Ok(())
}

fn set_new_session(command: &mut Command) {
    // SAFETY: `setsid` is async-signal-safe and does not access memory shared
    // with the parent between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn terminate_process_group(pid: u32, signal: i32) {
    // Each launched process is a session and process-group leader.
    unsafe { libc::kill(-(pid as i32), signal) };
}

fn terminate_pid(pid: u32, signal: i32) {
    unsafe { libc::kill(pid as i32, signal) };
}

fn process_identity(pid: u32) -> Result<ProcessIdentity> {
    Ok(ProcessIdentity {
        pid,
        start_time: proc_state_and_start_time(pid)?.1,
    })
}

fn process_is_running(process: ProcessIdentity) -> bool {
    matches!(
        proc_state_and_start_time(process.pid),
        Ok((state, start_time)) if state != b'Z' && start_time == process.start_time
    )
}

fn proc_state_and_start_time(pid: u32) -> Result<(u8, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat.rsplit_once(") ").context("malformed /proc stat")?;
    let mut fields = fields.split_whitespace();
    let state = fields
        .next()
        .and_then(|state| state.as_bytes().first().copied())
        .context("missing process state")?;
    let start_time = fields
        .nth(18)
        .context("missing process start time")?
        .parse()
        .context("invalid process start time")?;
    Ok((state, start_time))
}

fn log_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("session.json")
}

fn save_session(session: &Session) -> Result<()> {
    let path = manifest_path(&session.root);
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(session)?)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn load_session(root: &Path) -> Result<Session> {
    let path = manifest_path(root);
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "session is not available at {}; run `rho wayland start` first",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).context("read session manifest")
}

fn load_live_session(root: &Path) -> Result<Session> {
    let session = load_session(root)?;
    if !process_is_running(session.compositor) {
        bail!("session {:?} compositor is not running", session.name);
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_are_path_components() {
        assert!(validate_session_name("gui-1_test").is_ok());
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("../escape").is_err());
    }

    #[test]
    fn key_chords_become_ordered_wtype_events() {
        let args = wtype_key_args("ctrl+shift+enter").unwrap();
        let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        assert_eq!(
            args,
            [
                "-M", "ctrl", "-M", "shift", "-k", "Return", "-m", "shift", "-m", "ctrl"
            ]
        );
    }
}
