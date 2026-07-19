//! rho-gui: a native GUI attached to a running rho daemon.

mod agent_view;
mod banner;
mod chime;
mod commands;
mod connection;
mod draft_view;
mod editor_config;
mod highlights;
mod minibuffer;
mod pane;
mod registry;
mod render;
mod rho_assets;
mod store;
mod style;
mod terminal_view;
#[cfg(test)]
mod tests;
mod dashboard;
mod transcript;
mod transient;
mod workspace;
mod zed_remote;

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
        DashboardReply,
        RoleCycle,
        RoleCycleGroup,
        TaskBoard,
        FileSave,
        PaneSplitRight,
        PaneSplitDown,
        PaneClose,
        PaneFocusNext,
        PaneBack,
        RailFocus,
        RailOpen,
        RootTransient,
        MinibufferConfirm,
        MinibufferCancel,
        MinibufferNext,
        MinibufferPrevious,
        MinibufferComplete,
        TerminalPaste,
        TerminalNormalMode,
        TerminalRawMode,
        TerminalScrollLineUp,
        TerminalScrollLineDown,
        TerminalScrollHalfPageUp,
        TerminalScrollHalfPageDown,
        TerminalScrollTop,
        TerminalScrollBottom
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

    /// Write a Dial9 CPU/frame trace on exit (requires a frame-pointer build).
    #[arg(long, value_name = "FILE")]
    cpu_profile: Option<PathBuf>,
}

struct GuiProfiler {
    cpu: rho_profiling::CpuProfiler,
    frames: gpui::profiler::FrameTimingCollector,
    frame_path: PathBuf,
    draw_tid: u64,
    collected_frames: Arc<Mutex<Vec<gpui::profiler::FrameTiming>>>,
}

#[derive(serde::Serialize)]
struct FrameProfile {
    summary: FrameSummary,
    frames: Vec<FrameRecord>,
}

#[derive(serde::Serialize)]
struct FrameSummary {
    frame_count: usize,
    draw_ms: Distribution,
    dirty_to_draw_ms: Distribution,
    invalidations: Distribution,
}

#[derive(serde::Serialize)]
struct Distribution {
    count: usize,
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

#[derive(serde::Serialize)]
struct FrameRecord {
    window_id: u64,
    draw_start_ns: u64,
    draw_ns: u64,
    dirty_to_draw_ns: Option<u64>,
    invalidations: u64,
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
    rho_daemon::install_crypto_provider()?;
    let args = Args::parse();
    let profiler = args
        .cpu_profile
        .clone()
        .map(|path| {
            let cpu = rho_profiling::CpuProfiler::start(path)?;
            let frame_path = rho_profiling::sidecar_path(cpu.path(), ".frames.json");
            gpui::profiler::set_frame_trace_enabled(true);
            Ok::<_, anyhow::Error>(GuiProfiler {
                cpu,
                frames: gpui::profiler::FrameTimingCollector::new(),
                frame_path,
                draw_tid: 0,
                collected_frames: Arc::default(),
            })
        })
        .transpose()?;
    let attach_target = attach_target_from_args(args)?;

    gpui_platform::application()
        .with_assets(RhoAssets)
        .run(move |cx: &mut App| {
            let mut profiler = profiler;
            if let Some(profiler) = &mut profiler {
                // Window drawing and this application callback share GPUI's
                // foreground thread.
                profiler.draw_tid = rho_profiling::current_tid();
                let collected_frames = profiler.collected_frames.clone();
                cx.spawn(async move |cx| {
                    let mut collector = gpui::profiler::FrameTimingCollector::new();
                    loop {
                        cx.background_executor().timer(Duration::from_secs(5)).await;
                        collected_frames
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .extend(collector.collect_unseen());
                    }
                })
                .detach();
            }
            cx.on_app_quit(move |_| {
                if let Some(profiler) = profiler.take() {
                    finish_profiling(profiler);
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

fn finish_profiling(mut profiler: GuiProfiler) {
    let mut frames = std::mem::take(
        &mut *profiler
            .collected_frames
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    );
    frames.extend(profiler.frames.collect_unseen());
    frames.sort_unstable_by_key(|frame| (frame.draw_start, frame.window_id.as_u64()));
    frames.dedup_by_key(|frame| (frame.draw_start, frame.window_id.as_u64()));
    gpui::profiler::set_frame_trace_enabled(false);
    match profiler
        .cpu
        .finish_with_gpui_spans(frame_timeline_spans(&frames, profiler.draw_tid))
    {
        Ok(path) => eprintln!("rho-gui: wrote CPU profile to {}", path.display()),
        Err(error) => eprintln!("rho-gui: failed to write CPU profile: {error:#}"),
    }
    match export_frame_profile(&profiler.frame_path, frames) {
        Ok(()) => eprintln!(
            "rho-gui: wrote frame profile to {}",
            profiler.frame_path.display()
        ),
        Err(error) => eprintln!("rho-gui: failed to write frame profile: {error:#}"),
    }
}

fn frame_timeline_spans(
    frames: &[gpui::profiler::FrameTiming],
    draw_tid: u64,
) -> Vec<rho_profiling::GpuiFrameSpan> {
    let mut spans = Vec::with_capacity(frames.len() * 2);
    for (frame_index, frame) in frames.iter().enumerate() {
        let span = |kind, start| rho_profiling::GpuiFrameSpan {
            kind,
            start,
            end: frame.draw_end,
            tid: draw_tid,
            frame: frame_index as u64,
            window: frame.window_id.as_u64(),
            invalidations: frame.invalidations,
        };
        if let Some(dirty_at) = frame.dirty_at {
            spans.push(span(rho_profiling::GpuiFrameSpanKind::Latency, dirty_at));
        }
        spans.push(span(
            rho_profiling::GpuiFrameSpanKind::Draw,
            frame.draw_start,
        ));
    }
    spans
}

fn export_frame_profile(path: &Path, timings: Vec<gpui::profiler::FrameTiming>) -> Result<()> {
    let anchor = timings.first().map(|timing| timing.draw_start);
    let frames = timings
        .into_iter()
        .map(|timing| FrameRecord {
            window_id: timing.window_id.as_u64(),
            draw_start_ns: anchor
                .map(|anchor| duration_ns(timing.draw_start.duration_since(anchor)))
                .unwrap_or(0),
            draw_ns: duration_ns(timing.draw_duration()),
            dirty_to_draw_ns: timing.dirty_to_draw_duration().map(duration_ns),
            invalidations: timing.invalidations,
        })
        .collect::<Vec<_>>();
    let summary = FrameSummary {
        frame_count: frames.len(),
        draw_ms: distribution(frames.iter().map(|frame| frame.draw_ns), 1_000_000.0),
        dirty_to_draw_ms: distribution(
            frames.iter().filter_map(|frame| frame.dirty_to_draw_ns),
            1_000_000.0,
        ),
        invalidations: distribution(frames.iter().map(|frame| frame.invalidations), 1.0),
    };
    let file = File::create(path)
        .with_context(|| format!("failed to create frame profile {}", path.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(file), &FrameProfile { summary, frames })
        .with_context(|| format!("failed to write frame profile {}", path.display()))
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn distribution(values: impl IntoIterator<Item = u64>, scale: f64) -> Distribution {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_unstable();
    let count = values.len();
    if count == 0 {
        return Distribution {
            count,
            mean: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        };
    }
    let percentile = |percent: usize| {
        let index = (count * percent).div_ceil(100).saturating_sub(1);
        values[index] as f64 / scale
    };
    Distribution {
        count,
        mean: values.iter().map(|value| *value as f64).sum::<f64>() / count as f64 / scale,
        p50: percentile(50),
        p95: percentile(95),
        p99: percentile(99),
        max: values[count - 1] as f64 / scale,
    }
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
    let mut store = SettingsStore::new(cx, rho_assets::RHO_DEFAULT_SETTINGS);
    store
        .set_user_settings(&user_settings, cx)
        .result()
        .with_context(|| format!("failed to load settings from {}", settings_path.display()))?;
    // Rho is vim-first: the pane vocabulary and the `:` command line assume
    // modal editing, so vim mode is forced rather than left to settings.
    store.override_global(vim_mode_setting::VimModeSetting(true));
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
        KeyBinding::new("ctrl-s", FileSave, Some("RhoFileView")),
        // Terminal surface, raw mode: every unbound key becomes terminal
        // input, so its few chrome bindings use chords shells don't see
        // anyway. `ctrl-\ ctrl-n` is vim's terminal escape; `ctrl-shift-n`
        // is the discoverable chord for the same thing.
        KeyBinding::new("ctrl-shift-v", TerminalPaste, Some("RhoTerminal")),
        KeyBinding::new("ctrl-shift-;", RootTransient, Some("RhoTerminal")),
        KeyBinding::new("ctrl-\\ ctrl-n", TerminalNormalMode, Some("RhoTerminal")),
        KeyBinding::new("ctrl-shift-n", TerminalNormalMode, Some("RhoTerminal")),
        // Terminal normal mode: the keyboard belongs to rho again. Insert
        // returns to raw; plain vim keys browse scrollback.
        KeyBinding::new("i", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("a", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("enter", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("j", TerminalScrollLineDown, Some("RhoTerminalNormal")),
        KeyBinding::new("k", TerminalScrollLineUp, Some("RhoTerminalNormal")),
        KeyBinding::new("down", TerminalScrollLineDown, Some("RhoTerminalNormal")),
        KeyBinding::new("up", TerminalScrollLineUp, Some("RhoTerminalNormal")),
        KeyBinding::new(
            "ctrl-d",
            TerminalScrollHalfPageDown,
            Some("RhoTerminalNormal"),
        ),
        KeyBinding::new(
            "ctrl-u",
            TerminalScrollHalfPageUp,
            Some("RhoTerminalNormal"),
        ),
        KeyBinding::new("g g", TerminalScrollTop, Some("RhoTerminalNormal")),
        KeyBinding::new("shift-g", TerminalScrollBottom, Some("RhoTerminalNormal")),
    ]);
    // The space leader: one binding, opening the root transient at once
    // (invisible until the reveal delay). Every chord beneath it — panes,
    // agent verbs, workstream triage — is a transient item, so practiced
    // sequences run at full speed without the menu ever flashing. Bound for
    // normal-mode editors (vim or helix flavor — helix reports
    // `vim_mode == helix_normal`); the dashboard is an editor too, so the
    // same contexts cover it.
    for context in [
        "RhoTerminalNormal",
        "RhoGui > Editor && vim_mode == normal",
        "RhoGui > Editor && vim_mode == helix_normal",
    ] {
        cx.bind_keys([KeyBinding::new("space", RootTransient, Some(context))]);
    }
    // Minibuffer keys. The input is a single-line editor (vim skips those),
    // but enter/escape/tab still need to beat the editor's own bindings, so
    // they are scoped under the minibuffer context and loaded last.
    cx.bind_keys([
        KeyBinding::new("enter", MinibufferConfirm, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("escape", MinibufferCancel, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("tab", MinibufferComplete, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-n", MinibufferNext, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-p", MinibufferPrevious, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("down", MinibufferNext, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("up", MinibufferPrevious, Some("RhoMinibuffer > Editor")),
    ]);
    // Dashboard: the listing is read-only, so normal-mode letters are free
    // for acting on the row under the cursor — the magit trick. `enter`
    // binds for every mode: on a row it opens, in a reply draft it sends
    // (so enter in insert mode submits, like the transcript prompt).
    // Bound after the vim keymaps so they beat vim's own motions.
    cx.bind_keys([KeyBinding::new(
        "enter",
        RailOpen,
        Some("RhoDashboard > Editor"),
    )]);
    for context in [
        "RhoDashboard > Editor && vim_mode == normal",
        "RhoDashboard > Editor && vim_mode == helix_normal",
    ] {
        cx.bind_keys([
            KeyBinding::new("r", DashboardReply, Some(context)),
            KeyBinding::new("d", AgentDone, Some(context)),
        ]);
    }
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
