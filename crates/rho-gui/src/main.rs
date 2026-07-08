//! rho-gui: a native GUI attached to a running rho daemon.

mod agent_view;
mod banner;
mod commands;
mod connection;
mod draft_view;
mod editor_config;
mod highlights;
mod registry;
mod render;
mod store;
mod style;
#[cfg(test)]
mod tests;
mod topic_rail;
mod transcript;
mod voice_audio;
mod workspace;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use gpui::{App, AppContext as _, KeyBinding, WindowOptions, actions};
use settings::SettingsStore;

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
    #[arg(long)]
    socket: Option<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("rho-gui: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let attach_target = attach_target_from_args(Args::parse())?;

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
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

fn attach_target_from_args(args: Args) -> Result<AttachTarget> {
    let socket_path = match args.socket {
        Some(socket_path) => socket_path,
        None => rho_daemon::default_socket_path()?,
    };
    Ok(AttachTarget { socket_path })
}

fn init_app(cx: &mut App) -> Result<()> {
    gpui_tokio::init(cx);
    assets::Assets.load_fonts(cx)?;
    let settings_path = settings_path()?;
    let user_settings = load_or_create_settings(&settings_path)?;
    let mut store = SettingsStore::new(cx, settings::default_settings().as_ref());
    store
        .set_user_settings(&user_settings, cx)
        .result()
        .with_context(|| format!("failed to load settings from {}", settings_path.display()))?;
    cx.set_global(store);
    theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
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
        // bound here rather than in an asset.
        KeyBinding::new("ctrl-shift-j", AgentJumpAttention, Some("RhoGui")),
        KeyBinding::new("ctrl-shift-d", AgentDone, Some("RhoGui")),
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
{}
"#;

fn settings_path() -> Result<PathBuf> {
    let config_dir = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(config_home) => PathBuf::from(config_home),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".config"))
            .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?,
    };
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
