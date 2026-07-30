//! rho-gui: a native GUI attached to a running rho daemon.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use gpui::{App, AppContext as _, WindowOptions};
use rho_gui::rho_assets::RhoAssets;
use rho_gui::workspace::{AttachTarget, Workspace};
use rho_gui::*;
use settings::SettingsStore;
use tracing_subscriber::EnvFilter;

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
    editor: Arc<Mutex<gpui::profiler::EditorTimingCollector>>,
    frame_path: PathBuf,
    editor_path: PathBuf,
    draw_tid: u64,
    collected_frames: Arc<Mutex<Vec<gpui::profiler::FrameTiming>>>,
    collected_editor: Arc<Mutex<Vec<gpui::profiler::EditorTiming>>>,
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
struct FrameRecord {
    window_id: u64,
    draw_start_ns: u64,
    draw_ns: u64,
    dirty_to_draw_ns: Option<u64>,
    invalidations: u64,
}

#[derive(serde::Serialize)]
struct EditorProfile {
    event_count: usize,
    stages: BTreeMap<&'static str, EditorStageSummary>,
    events: Vec<EditorRecord>,
}

#[derive(serde::Serialize)]
struct EditorStageSummary {
    count: usize,
    duration_ms: Distribution,
    input_rows: Distribution,
    output_rows: Distribution,
}

#[derive(serde::Serialize)]
struct EditorRecord {
    stage: &'static str,
    start_ns: u64,
    duration_ns: u64,
    tid: u64,
    input_edits: u64,
    input_start: u64,
    input_rows: u64,
    output_edits: u64,
    output_start: u64,
    output_rows: u64,
    old_rows: u64,
    new_rows: u64,
    pending_batches: u64,
    flags: u64,
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
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install the AWS-LC rustls crypto provider"))?;
    }
    let args = Args::parse();
    let profiler = args
        .cpu_profile
        .clone()
        .map(|path| {
            let cpu = rho_profiling::CpuProfiler::start(path)?;
            let frame_path = rho_profiling::sidecar_path(cpu.path(), ".frames.json");
            let editor_path = rho_profiling::sidecar_path(cpu.path(), ".editor.json");
            gpui::profiler::set_frame_trace_enabled(true);
            gpui::profiler::set_editor_trace_enabled(true);
            Ok::<_, anyhow::Error>(GuiProfiler {
                cpu,
                frames: gpui::profiler::FrameTimingCollector::new(),
                editor: Arc::new(Mutex::new(gpui::profiler::EditorTimingCollector::new())),
                frame_path,
                editor_path,
                draw_tid: 0,
                collected_frames: Arc::default(),
                collected_editor: Arc::default(),
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
                let collected_editor = profiler.collected_editor.clone();
                let editor = profiler.editor.clone();
                let executor = cx.background_executor().clone();
                cx.background_spawn(async move {
                    let mut collector = gpui::profiler::FrameTimingCollector::new();
                    loop {
                        executor.timer(Duration::from_secs(5)).await;
                        collected_frames
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .extend(collector.collect_unseen());
                        collected_editor
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .extend(
                                editor
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .collect_unseen(),
                            );
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
    gpui::profiler::set_editor_trace_enabled(false);
    let mut frames = std::mem::take(
        &mut *profiler
            .collected_frames
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    );
    frames.extend(profiler.frames.collect_unseen());
    frames.sort_unstable_by_key(|frame| (frame.draw_start, frame.window_id.as_u64()));
    frames.dedup_by_key(|frame| (frame.draw_start, frame.window_id.as_u64()));
    let mut collected_editor = profiler
        .collected_editor
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut editor_collector = profiler
        .editor
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut editor = std::mem::take(&mut *collected_editor);
    editor.extend(editor_collector.collect_unseen());
    drop(editor_collector);
    drop(collected_editor);
    editor.sort_unstable_by_key(|event| (event.start, event.kind as u8));
    gpui::profiler::set_frame_trace_enabled(false);
    match profiler.cpu.finish_with_gui_spans(
        frame_timeline_spans(&frames, profiler.draw_tid),
        editor_timeline_spans(&editor),
    ) {
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
    match export_editor_profile(&profiler.editor_path, editor) {
        Ok(()) => eprintln!(
            "rho-gui: wrote editor profile to {}",
            profiler.editor_path.display()
        ),
        Err(error) => eprintln!("rho-gui: failed to write editor profile: {error:#}"),
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

fn editor_timeline_spans(
    events: &[gpui::profiler::EditorTiming],
) -> Vec<rho_profiling::EditorStageSpan> {
    events
        .iter()
        .map(|event| rho_profiling::EditorStageSpan {
            kind: event.kind as u8 as u64,
            start: event.start,
            end: event.end,
            tid: event.tid,
            input_edits: event.input_edits,
            input_start: event.input_start,
            input_rows: event.input_rows,
            output_edits: event.output_edits,
            output_start: event.output_start,
            output_rows: event.output_rows,
            old_rows: event.old_rows,
            new_rows: event.new_rows,
            pending_batches: event.pending_batches,
            flags: event.flags,
        })
        .collect()
}

fn editor_stage_name(kind: gpui::profiler::EditorTimingKind) -> &'static str {
    match kind {
        gpui::profiler::EditorTimingKind::BufferEdit => "buffer_edit",
        gpui::profiler::EditorTimingKind::MultiBufferSync => "multi_buffer_sync",
        gpui::profiler::EditorTimingKind::InlayMapSync => "inlay_map_sync",
        gpui::profiler::EditorTimingKind::FoldMapSync => "fold_map_sync",
        gpui::profiler::EditorTimingKind::TabMapSync => "tab_map_sync",
        gpui::profiler::EditorTimingKind::WrapMapSync => "wrap_map_sync",
        gpui::profiler::EditorTimingKind::BlockMapSync => "block_map_sync",
        gpui::profiler::EditorTimingKind::WrapMapUpdate => "wrap_map_update",
    }
}

fn export_editor_profile(path: &Path, timings: Vec<gpui::profiler::EditorTiming>) -> Result<()> {
    let anchor = timings.first().map(|timing| timing.start);
    let events = timings
        .into_iter()
        .map(|timing| EditorRecord {
            stage: editor_stage_name(timing.kind),
            start_ns: anchor
                .map(|anchor| duration_ns(timing.start.duration_since(anchor)))
                .unwrap_or(0),
            duration_ns: duration_ns(timing.end.duration_since(timing.start)),
            tid: timing.tid,
            input_edits: timing.input_edits,
            input_start: timing.input_start,
            input_rows: timing.input_rows,
            output_edits: timing.output_edits,
            output_start: timing.output_start,
            output_rows: timing.output_rows,
            old_rows: timing.old_rows,
            new_rows: timing.new_rows,
            pending_batches: timing.pending_batches,
            flags: timing.flags,
        })
        .collect::<Vec<_>>();
    let mut grouped = BTreeMap::<_, Vec<&EditorRecord>>::new();
    for event in &events {
        grouped.entry(event.stage).or_default().push(event);
    }
    let stages = grouped
        .into_iter()
        .map(|(stage, events)| {
            (
                stage,
                EditorStageSummary {
                    count: events.len(),
                    duration_ms: distribution(
                        events.iter().map(|event| event.duration_ns),
                        1_000_000.0,
                    ),
                    input_rows: distribution(events.iter().map(|event| event.input_rows), 1.0),
                    output_rows: distribution(events.iter().map(|event| event.output_rows), 1.0),
                },
            )
        })
        .collect();
    let profile = EditorProfile {
        event_count: events.len(),
        stages,
        events,
    };
    let file = File::create(path)
        .with_context(|| format!("failed to create editor profile {}", path.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(file), &profile)
        .with_context(|| format!("failed to write editor profile {}", path.display()))
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

fn attach_target_from_args(args: Args) -> Result<AttachTarget> {
    if let Some(endpoint_id) = args.endpoint {
        return Ok(AttachTarget::Iroh {
            endpoint_id,
            ssh_destination: args.ssh.context("--ssh is required with --endpoint")?,
            remote_rho: args.remote_rho,
        });
    }
    Ok(AttachTarget::Unix(
        args.socket.unwrap_or(rho_ui_proto::socket_path()?),
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

const DEFAULT_SETTINGS: &str = r#"// Rho GUI user settings. Values here override bundled defaults.
{
  "theme": "Rho OKSolar P3"
}
"#;

const LEGACY_DEFAULT_SETTINGS: &str = r#"// Rho GUI user settings. Values here override bundled defaults.
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
        Ok(settings) if settings == LEGACY_DEFAULT_SETTINGS => {
            fs::write(path, DEFAULT_SETTINGS).with_context(|| {
                format!("failed to update default settings at {}", path.display())
            })?;
            Ok(DEFAULT_SETTINGS.to_owned())
        }
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
