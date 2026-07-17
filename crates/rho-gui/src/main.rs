//! rho-gui: a native GUI attached to a running rho daemon.

mod agent_view;
mod banner;
mod chime;
mod commands;
mod connection;
mod draft_view;
mod editor_config;
mod highlights;
mod registry;
mod render;
mod rho_assets;
mod store;
mod style;
#[cfg(test)]
mod tests;
mod topic_rail;
mod transcript;
mod voice_audio;
mod workspace;

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::Parser;
use gpui::{App, AppContext as _, KeyBinding, WindowOptions, actions};
use settings::SettingsStore;
use tracing_subscriber::EnvFilter;

use crate::rho_assets::RhoAssets;
use crate::workspace::{AttachTarget, Workspace};

// Keep the `rho_gui` action namespace: the bundled keymaps bind these under
// the `RhoGui > Editor` context.
actions!(
    rho_gui,
    [
        SubmitPrompt,
        AgentPrevious,
        AgentNext,
        AgentNew,
        AgentJumpAttention,
        AgentDone,
        RoleCycle,
        RoleCycleGroup,
        TaskBoard
    ]
);

#[derive(Parser)]
#[command(
    name = "rho-gui",
    about = "Attach a native GUI to a running Rho daemon"
)]
struct Args {
    /// Connect directly to this rho daemon Unix socket.
    #[arg(long, conflicts_with = "endpoint")]
    socket: Option<PathBuf>,

    /// Connect to this rho daemon iroh endpoint id.
    #[arg(
        long,
        visible_alias = "iroh",
        value_name = "ENDPOINT_ID",
        requires = "ssh"
    )]
    endpoint: Option<iroh::EndpointId>,

    /// Approve the in-memory iroh key by running rho through this SSH
    /// destination.
    #[arg(long, value_name = "DESTINATION", requires = "endpoint")]
    ssh: Option<String>,

    /// Rho executable on the SSH host.
    #[arg(long, value_name = "PATH", default_value = "rho")]
    remote_rho: String,

    /// Sample CPU stacks in-process and write folded stacks when the GUI exits.
    #[arg(long, value_name = "FILE")]
    cpu_profile: Option<PathBuf>,
}

fn main() {
    init_tracing();
    if let Err(error) = run() {
        eprintln!("rho-gui: {error:#}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
    {
        eprintln!("rho-gui: failed to initialize tracing: {error}");
    }
    tracing::info!("rho-gui tracing initialized");
}

fn run() -> Result<()> {
    let args = Args::parse();
    let cpu_profile = args.cpu_profile.clone();
    let profiler = cpu_profile
        .as_ref()
        .map(|_| {
            pprof::ProfilerGuardBuilder::default()
                .frequency(100)
                .build()
                .context("failed to start in-process CPU profiler")
        })
        .transpose()?;
    let cpu_profile = cpu_profile.zip(profiler);
    let attach_target = attach_target_from_args(args)?;

    gpui_platform::application()
        .with_assets(RhoAssets)
        .run(move |cx: &mut App| {
            let mut cpu_profile = cpu_profile;
            cx.on_app_quit(move |_| {
                if let Some((path, profiler)) = cpu_profile.take() {
                    match export_cpu_profile(&path, &profiler) {
                        Ok(()) => {
                            eprintln!("rho-gui: wrote CPU profile to {}", path.display());
                        }
                        Err(error) => {
                            eprintln!("rho-gui: failed to write CPU profile: {error:#}");
                        }
                    }
                }
                std::future::ready(())
            })
            .detach();

            if let Err(error) = init_app(cx) {
                eprintln!("rho-gui: {error:#}");
                cx.quit();
                return;
            }

            cx.activate(true);

            if let Err(error) = cx.open_window(WindowOptions::default(), move |window, cx| {
                cx.new(|cx| Workspace::new(attach_target.clone(), window, cx))
            }) {
                eprintln!("rho-gui: failed to open window: {error:#}");
                cx.quit();
            }
        });

    Ok(())
}

fn export_cpu_profile(path: &Path, profiler: &pprof::ProfilerGuard<'_>) -> Result<()> {
    let report = profiler
        .report()
        .build()
        .context("failed to build CPU profile")?;
    let mut stacks = report
        .data
        .iter()
        .map(|(frames, count)| {
            let mut stack = String::new();
            push_folded_name(&mut stack, &frames.thread_name_or_id());
            for frame in frames.frames.iter().rev() {
                for symbol in frame.iter().rev() {
                    stack.push(';');
                    push_folded_name(&mut stack, &symbol.name());
                    if symbol.lineno() != 0 {
                        write!(stack, " [{}:{}]", symbol.filename(), symbol.lineno()).unwrap();
                    }
                }
            }
            (stack, count)
        })
        .collect::<Vec<_>>();
    stacks.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

    let file = File::create(path)
        .with_context(|| format!("failed to create CPU profile {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for (stack, count) in stacks {
        writeln!(writer, "{stack} {count}")
            .with_context(|| format!("failed to write CPU profile {}", path.display()))?;
    }
    Ok(())
}

fn push_folded_name(output: &mut String, name: &str) {
    output.extend(name.chars().map(|character| match character {
        ';' | '\n' | '\r' => ' ',
        character => character,
    }));
}

fn attach_target_from_args(args: Args) -> Result<AttachTarget> {
    if let Some(endpoint_id) = args.endpoint {
        return Ok(AttachTarget::Iroh {
            endpoint_id,
            ssh_destination: args.ssh.context("--ssh is required with --endpoint")?,
            remote_rho: args.remote_rho,
        });
    }
    Ok(AttachTarget::Unix(
        args.socket.unwrap_or(rho_daemon::default_socket_path()?),
    ))
}

fn init_app(cx: &mut App) -> Result<()> {
    gpui_tokio::init(cx);
    RhoAssets.load_fonts(cx)?;
    let settings_path = settings_path()?;
    let user_settings = load_or_create_settings(&settings_path)?;
    let mut store = SettingsStore::new(cx, settings::default_settings().as_ref());
    store
        .set_user_settings(&user_settings, cx)
        .result()
        .with_context(|| format!("failed to load settings from {}", settings_path.display()))?;
    cx.set_global(store);
    theme_settings::init(theme::LoadThemes::All(Box::new(RhoAssets)), cx);
    release_channel::init(semver::Version::new(0, 1, 0), cx);
    editor::init(cx);
    command_palette::init(cx);
    search::init(cx);
    vim::init(cx);
    let default_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)
            .context("failed to load default keymap")?;
    cx.bind_keys(default_key_bindings);
    let vim_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::VIM_KEYMAP_PATH, cx)
            .context("failed to load vim keymap")?;
    cx.bind_keys(vim_key_bindings);
    bind_rho_key_overrides(cx);
    Ok(())
}

fn bind_rho_key_overrides(cx: &mut App) {
    // Keep draft field navigation available in vim normal mode. The bundled
    // vim keymap only binds the rho prompt keys for insert mode, while the
    // default keymap's Tab binding can lose to vim's normal-mode handling.
    cx.bind_keys([
        // Attention triage: jump to the most urgent agent, clear the current
        // one. The bundled zed keymaps don't know these actions, so they are
        // bound here rather than in an asset. The context must be at least as
        // deep as `Editor`: the bundled keymap binds these keys under plain
        // `Editor` (JoinLines, git::Diff), and gpui prefers the deeper match,
        // so a root-level `RhoGui` binding would lose while typing.
        KeyBinding::new("ctrl-shift-j", AgentJumpAttention, Some("RhoGui > Editor")),
        KeyBinding::new("ctrl-shift-d", AgentDone, Some("RhoGui > Editor")),
        KeyBinding::new(
            "tab",
            RoleCycle,
            Some("RhoGui > Editor && !showing_completions"),
        ),
        KeyBinding::new(
            "shift-tab",
            RoleCycleGroup,
            Some("RhoGui > Editor && !showing_completions"),
        ),
    ]);
}

const DEFAULT_SETTINGS: &str = r#"// Rho GUI user settings. Values here override bundled defaults.
{
  "theme": "Rho Monokai P3"
}
"#;

fn settings_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("config directory not available")?;
    Ok(config_dir.join("rho-gui").join("settings.json"))
}

fn load_or_create_settings(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(settings) => Ok(settings),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create settings directory {}", parent.display())
                })?;
            }
            fs::write(path, DEFAULT_SETTINGS).with_context(|| {
                format!("failed to write default settings to {}", path.display())
            })?;
            Ok(DEFAULT_SETTINGS.to_owned())
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to read settings from {}", path.display()))
        }
    }
}
