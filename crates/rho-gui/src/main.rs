//! rho-gui: a native GUI attached to a running rho daemon.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use gpui::{App, AppContext as _, WindowOptions};
use rho_gui::rho_assets::RhoAssets;
use rho_gui::workspace::{AttachTarget, HostSpec, Workspace};
use rho_gui::*;
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use tracing_subscriber::EnvFilter;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser)]
#[command(
    name = "rho-gui",
    about = "Attach a native GUI to one or more running Rho daemons"
)]
struct Args {
    /// Attach a daemon as `<name>=unix:<socket>` or
    /// `<name>=iroh:<endpoint-id>@<ssh-dest>`. Repeatable; the name labels
    /// the host's agents and projects once more than one is attached.
    /// Defaults to the local daemon socket.
    #[arg(long, value_name = "NAME=TARGET")]
    attach: Vec<String>,

    /// Rho executable on SSH hosts.
    #[arg(long, value_name = "PATH", default_value = "rho")]
    remote_rho: String,

    /// Write a Dial9 CPU/frame trace on exit (requires a frame-pointer build).
    #[arg(long, value_name = "FILE")]
    cpu_profile: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Read the client-only action journal.
    Journal {
        #[command(subcommand)]
        command: JournalCommand,
    },
}

#[derive(Subcommand)]
enum JournalCommand {
    /// Print faithful JSONL records after the GUI has exited.
    Dump {
        #[arg(long)]
        kind: Option<String>,
    },
}

#[derive(RegisterSetting)]
struct TextRenderingModeSetting(settings::TextRenderingMode);

impl Settings for TextRenderingModeSetting {
    fn from_settings(content: &SettingsContent) -> Self {
        Self(content.workspace.text_rendering_mode.unwrap())
    }
}

fn apply_text_rendering_mode(cx: &mut App) {
    let mode = match TextRenderingModeSetting::get_global(cx).0 {
        settings::TextRenderingMode::PlatformDefault => gpui::TextRenderingMode::PlatformDefault,
        settings::TextRenderingMode::Subpixel => gpui::TextRenderingMode::Subpixel,
        settings::TextRenderingMode::Grayscale => gpui::TextRenderingMode::Grayscale,
    };
    cx.set_text_rendering_mode(mode);
}

struct GuiProfiler {
    cpu: rho_profiling::CpuProfiler,
    checkpoint: Arc<ProfileCheckpoint>,
    draw_tid: u64,
}

struct ProfileCheckpoint {
    state: Mutex<ProfileState>,
    writer: Mutex<()>,
    finalized: AtomicBool,
    final_written: AtomicBool,
    frame_path: PathBuf,
    editor_path: PathBuf,
    work_path: PathBuf,
}

struct ProfileState {
    frames: gpui::profiler::FrameTimingCollector,
    editor: gpui::profiler::EditorTimingCollector,
    collected_frames: Vec<gpui::profiler::FrameTiming>,
    collected_editor: Vec<gpui::profiler::EditorTiming>,
}

struct ProfileSnapshot {
    frames: Vec<gpui::profiler::FrameTiming>,
    editor: Vec<gpui::profiler::EditorTiming>,
}

thread_local! {
    static WRITING_PROFILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
    prepaint_ms: Distribution,
    paint_ms: Distribution,
    finish_ms: Distribution,
    dirty_to_draw_ms: Distribution,
    invalidations: Distribution,
}

#[derive(serde::Serialize)]
struct FrameRecord {
    window_id: u64,
    draw_start_ns: u64,
    draw_ns: u64,
    prepaint_ns: u64,
    paint_ns: u64,
    finish_ns: u64,
    dirty_to_draw_ns: Option<u64>,
    invalidations: u64,
}

/// Main-thread work outside any frame, as a rig session leaves it behind.
///
/// The frame log accounts for time inside `Window::draw`, so the work that
/// makes the *next* frame late is in neither it nor the editor log: a run
/// could say a desk sync was slow only through a telemetry report the user
/// sent, which is not something a rig can produce. `owners` against
/// `work_units` is the per-event side of the cost rule, the way
/// `input_rows` is for a stage.
#[derive(serde::Serialize)]
struct WorkProfile {
    span_count: usize,
    /// How many spans have ever been pushed, against however many the ring
    /// still holds: without it a total here is a total over an unknown
    /// window.
    pushed: u64,
    owners: BTreeMap<&'static str, WorkOwnerSummary>,
    spans: Vec<WorkRecord>,
}

#[derive(serde::Serialize)]
struct WorkOwnerSummary {
    count: usize,
    duration_ms: Distribution,
    work_units: Distribution,
    /// What the whole owner cost, which is the number a run is judged on:
    /// a cheap span run often is not cheap.
    total_ms: f64,
}

#[derive(serde::Serialize)]
struct WorkRecord {
    owner: &'static str,
    start_ns: u64,
    duration_ns: u64,
    work_units: u64,
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
    let arguments = std::env::args().collect::<Vec<_>>();
    if rho_browser::native_host::is_invocation(&arguments) {
        if let Err(error) = rho_browser::native_host::run() {
            eprintln!("rho browser native host: {error:#}");
            std::process::exit(1);
        }
        return;
    }
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

fn default_client_state_dir() -> Result<PathBuf> {
    let base = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("state directory not available"))?;
    Ok(base.join("rho"))
}

fn run() -> Result<()> {
    let args = Args::parse();
    let client_state_dir = default_client_state_dir()?;
    if let Some(Command::Journal {
        command: JournalCommand::Dump { kind },
    }) = args.command.as_ref()
    {
        let stdout = std::io::stdout();
        return rho_journal::dump(&client_state_dir, kind.as_deref(), stdout.lock());
    }
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install the AWS-LC rustls crypto provider"))?;
    }
    let profiler = args
        .cpu_profile
        .clone()
        .map(|path| {
            let cpu = rho_profiling::CpuProfiler::start(path)?;
            let frame_path = rho_profiling::sidecar_path(cpu.path(), ".frames.json");
            let editor_path = rho_profiling::sidecar_path(cpu.path(), ".editor.json");
            let work_path = rho_profiling::sidecar_path(cpu.path(), ".work.json");
            Ok::<_, anyhow::Error>(GuiProfiler {
                checkpoint: Arc::new(ProfileCheckpoint {
                    state: Mutex::new(ProfileState {
                        frames: gpui::profiler::FrameTimingCollector::new(),
                        editor: gpui::profiler::EditorTimingCollector::new(),
                        collected_frames: Vec::new(),
                        collected_editor: Vec::new(),
                    }),
                    writer: Mutex::new(()),
                    finalized: AtomicBool::new(false),
                    final_written: AtomicBool::new(false),
                    frame_path,
                    editor_path,
                    work_path,
                }),
                cpu,
                draw_tid: 0,
            })
        })
        .transpose()?;
    if let Some(profiler) = &profiler {
        install_profile_panic_hook(Arc::clone(&profiler.checkpoint));
    }
    let specs = host_specs(&args)?;
    let local_socket = specs.iter().find_map(|spec| match &spec.target {
        AttachTarget::Unix(socket) => Some(socket),
        AttachTarget::Iroh { .. } => None,
    });
    let browser_socket = match local_socket {
        Some(socket) => rho_ui_proto::RuntimePaths::new(Some(socket.clone()))?,
        None => rho_ui_proto::RuntimePaths::new(None::<PathBuf>)?,
    }
    .browser_socket();
    rho_journal::init(&client_state_dir, rho_gui::dealer_policy_snapshot())
        .context("initialize client action journal")?;
    // The mirror is a cache: a session that cannot open it starts empty and
    // asks the daemon for everything, which is the old behaviour. Only the
    // path is settled here; the model thread opens it.
    rho_mirror::mirror::set_state_dir(client_state_dir.clone());
    rho_gui::telemetry::enable();
    if profiler.is_none()
        && let Err(error) = rho_gui::telemetry::enable_passive_cpu_profile()
    {
        tracing::warn!(%error, "passive GUI CPU profiling is unavailable");
    }

    gpui_platform::application()
        .with_assets(RhoAssets)
        .run(move |cx: &mut App| {
            const USER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
            cx.on_user_idle(USER_IDLE_TIMEOUT, move |event| match event {
                gpui::UserIdleEvent::Idle => {
                    rho_journal::record(rho_journal::Event::UserIdle {
                        timeout_s: USER_IDLE_TIMEOUT.as_secs(),
                    });
                }
                gpui::UserIdleEvent::Resumed => {
                    rho_journal::record(rho_journal::Event::UserResumed);
                }
            });
            let mut profiler = profiler;
            if let Some(profiler) = &mut profiler {
                // Window drawing and this application callback share GPUI's
                // foreground thread.
                profiler.draw_tid = rho_profiling::current_tid();
                let checkpoint = Arc::clone(&profiler.checkpoint);
                let executor = cx.background_executor().clone();
                cx.background_spawn(async move {
                    let mut ticks = 0u8;
                    loop {
                        executor.timer(Duration::from_secs(5)).await;
                        checkpoint.collect();
                        ticks += 1;
                        // Collect often enough that GPUI's bounded timing
                        // rings cannot wrap, but keep the full JSON rewrite
                        // out of the workload's hot path.
                        if ticks == 6 {
                            ticks = 0;
                            checkpoint.spawn_periodic_write();
                        }
                    }
                })
                .detach();
            }
            cx.on_app_quit(move |_| {
                if let Some(profiler) = profiler.take() {
                    finish_profiling(profiler);
                }
                rho_gui::telemetry::shutdown_passive_cpu_profile();
                rho_journal::flush();
                // Closing rather than flushing: a mirror left open is a
                // file redb finds unclean, and the next start rebuilds its
                // allocator from every page to be sure of it.
                rho_mirror::mirror::close();
                rho_mirror::desk::close();
                std::future::ready(())
            })
            .detach();

            if let Err(error) = init_app(cx) {
                eprintln!("rho-gui: {error:#}");
                cx.quit();
                return;
            }

            quit_on_termination_signal(cx);

            rho_browser::init(&client_state_dir, browser_socket.clone(), cx);
            cx.activate(true);

            // Wayland compositors resolve the launcher icon by matching this
            // app_id against rho-gui.desktop.
            let window_options = WindowOptions {
                app_id: Some("rho-gui".to_owned()),
                ..Default::default()
            };
            if let Err(error) = cx.open_window(window_options, move |window, cx| {
                cx.new(|cx| Workspace::new(specs.clone(), window, cx))
            }) {
                eprintln!("rho-gui: failed to open window: {error:#}");
                cx.quit();
            }
        });

    Ok(())
}

fn quit_on_termination_signal(cx: &mut App) {
    let signal = gpui_tokio::Tokio::spawn(cx, async {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let signal = tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        };
        Ok::<_, std::io::Error>(signal)
    });
    cx.spawn(async move |cx| match signal.await {
        Ok(Ok(signal)) => {
            tracing::info!(signal, "termination signal received; quitting");
            cx.update(|cx| cx.quit());
        }
        Ok(Err(error)) => {
            tracing::error!(%error, "failed to register termination signal handler")
        }
        Err(error) => tracing::error!(%error, "termination signal handler task failed"),
    })
    .detach();
}

fn finish_profiling(profiler: GuiProfiler) {
    let snapshot = profiler.checkpoint.final_snapshot();
    match profiler.cpu.finish_with_gui_spans(
        frame_timeline_spans(&snapshot.frames, profiler.draw_tid),
        editor_timeline_spans(&snapshot.editor),
    ) {
        Ok(path) => eprintln!("rho-gui: wrote CPU profile to {}", path.display()),
        Err(error) => eprintln!("rho-gui: failed to write CPU profile: {error:#}"),
    }
    match profiler.checkpoint.install(&snapshot, true) {
        Ok(()) => {
            profiler
                .checkpoint
                .final_written
                .store(true, Ordering::Release);
            eprintln!(
                "rho-gui: wrote frame profile to {}",
                profiler.checkpoint.frame_path.display()
            );
            eprintln!(
                "rho-gui: wrote editor profile to {}",
                profiler.checkpoint.editor_path.display()
            );
            eprintln!(
                "rho-gui: wrote main-thread work profile to {}",
                profiler.checkpoint.work_path.display()
            );
        }
        Err(error) => eprintln!("rho-gui: failed to write GUI profile: {error:#}"),
    }
}

impl ProfileCheckpoint {
    fn collect(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let frames = state.frames.collect_unseen();
        state.collected_frames.extend(frames);
        let editor = state.editor.collect_unseen();
        state.collected_editor.extend(editor);
    }

    fn snapshot(&self) -> ProfileSnapshot {
        self.collect();
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut frames = state.collected_frames.clone();
        let mut editor = state.collected_editor.clone();
        drop(state);
        frames.sort_unstable_by_key(|frame| (frame.draw_start, frame.window_id.as_u64()));
        frames.dedup_by_key(|frame| (frame.draw_start, frame.window_id.as_u64()));
        editor.sort_unstable_by_key(|event| (event.start, event.kind as u8));
        ProfileSnapshot { frames, editor }
    }

    fn final_snapshot(&self) -> ProfileSnapshot {
        self.finalized.store(true, Ordering::Release);
        self.snapshot()
    }

    fn spawn_periodic_write(self: &Arc<Self>) {
        if self.finalized.load(Ordering::Acquire) {
            return;
        }
        let checkpoint = Arc::clone(self);
        if let Err(error) = std::thread::Builder::new()
            .name("rho-profile-checkpoint".to_owned())
            .spawn(move || {
                let Ok(_writer) = checkpoint.writer.try_lock() else {
                    return;
                };
                if checkpoint.finalized.load(Ordering::Acquire) {
                    return;
                }
                // If this thread itself panics while cloning or writing,
                // the hook must use the last complete atomic checkpoint
                // rather than trying to take the lock recursively.
                WRITING_PROFILE.set(true);
                let snapshot = checkpoint.snapshot();
                if let Err(error) = checkpoint.install_locked(&snapshot, false) {
                    eprintln!("rho-gui: failed to checkpoint GUI profile: {error:#}");
                }
            })
        {
            eprintln!("rho-gui: failed to start GUI profile checkpoint writer: {error}");
        }
    }

    fn install(&self, snapshot: &ProfileSnapshot, final_write: bool) -> Result<()> {
        let Ok(_writer) = self.writer.try_lock() else {
            if final_write {
                let _writer = self
                    .writer
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                return self.install_locked(snapshot, true);
            }
            return Ok(());
        };
        self.install_locked(snapshot, final_write)
    }

    fn install_locked(&self, snapshot: &ProfileSnapshot, final_write: bool) -> Result<()> {
        if !final_write && self.finalized.load(Ordering::Acquire) {
            return Ok(());
        }
        WRITING_PROFILE.set(true);
        // The work ring is global and keeps its own history, so it is read
        // here rather than collected into the snapshot: there is nothing
        // per-checkpoint to accumulate.
        let result = export_frame_profile(&self.frame_path, snapshot.frames.clone())
            .and_then(|()| export_editor_profile(&self.editor_path, snapshot.editor.clone()))
            .and_then(|()| export_work_profile(&self.work_path));
        WRITING_PROFILE.set(false);
        result
    }
}

fn install_profile_panic_hook(checkpoint: Arc<ProfileCheckpoint>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        flush_profile_after_panic(&checkpoint);
        previous(info);
    }));
}

fn flush_profile_after_panic(checkpoint: &ProfileCheckpoint) {
    if checkpoint.final_written.load(Ordering::Acquire) || WRITING_PROFILE.get() {
        return;
    }
    let snapshot = checkpoint.final_snapshot();
    if let Err(error) = checkpoint.install(&snapshot, true) {
        eprintln!("rho-gui: failed to flush GUI profile after panic: {error:#}");
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
        gpui::profiler::EditorTimingKind::SyncTree => "sync_tree",
        gpui::profiler::EditorTimingKind::SpliceInlays => "splice_inlays",
        gpui::profiler::EditorTimingKind::MultiBufferBufferScan => "multi_buffer_buffer_scan",
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
    write_json_atomic(path, &profile)
}

fn export_work_profile(path: &Path) -> Result<()> {
    let (work, pushed) = gpui::profiler::snapshot_main_thread_work();
    let anchor = work.first().map(|work| work.start);
    let spans = work
        .into_iter()
        .map(|work| WorkRecord {
            owner: rho_gui::telemetry::main_thread_work_owner(work.owner),
            start_ns: anchor
                .map(|anchor| duration_ns(work.start.saturating_duration_since(anchor)))
                .unwrap_or(0),
            duration_ns: duration_ns(work.end.saturating_duration_since(work.start)),
            work_units: work.work_units,
        })
        .collect::<Vec<_>>();
    let mut grouped = BTreeMap::<_, Vec<&WorkRecord>>::new();
    for span in &spans {
        grouped.entry(span.owner).or_default().push(span);
    }
    let owners = grouped
        .into_iter()
        .map(|(owner, spans)| {
            (
                owner,
                WorkOwnerSummary {
                    count: spans.len(),
                    duration_ms: distribution(
                        spans.iter().map(|span| span.duration_ns),
                        1_000_000.0,
                    ),
                    work_units: distribution(spans.iter().map(|span| span.work_units), 1.0),
                    total_ms: spans.iter().map(|span| span.duration_ns).sum::<u64>() as f64 / 1e6,
                },
            )
        })
        .collect();
    let profile = WorkProfile {
        span_count: spans.len(),
        pushed,
        owners,
        spans,
    };
    write_json_atomic(path, &profile)
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
            prepaint_ns: duration_ns(timing.prepaint_duration()),
            paint_ns: duration_ns(timing.paint_duration()),
            finish_ns: duration_ns(timing.finish_duration()),
            dirty_to_draw_ns: timing.dirty_to_draw_duration().map(duration_ns),
            invalidations: timing.invalidations,
        })
        .collect::<Vec<_>>();
    let summary = FrameSummary {
        frame_count: frames.len(),
        draw_ms: distribution(frames.iter().map(|frame| frame.draw_ns), 1_000_000.0),
        prepaint_ms: distribution(frames.iter().map(|frame| frame.prepaint_ns), 1_000_000.0),
        paint_ms: distribution(frames.iter().map(|frame| frame.paint_ns), 1_000_000.0),
        finish_ms: distribution(frames.iter().map(|frame| frame.finish_ns), 1_000_000.0),
        dirty_to_draw_ms: distribution(
            frames.iter().filter_map(|frame| frame.dirty_to_draw_ns),
            1_000_000.0,
        ),
        invalidations: distribution(frames.iter().map(|frame| frame.invalidations), 1.0),
    };
    write_json_atomic(path, &FrameProfile { summary, frames })
}

fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("failed to create profile checkpoint {}", path.display()))?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, value)
            .with_context(|| format!("failed to write profile checkpoint {}", path.display()))?;
        writer
            .flush()
            .with_context(|| format!("failed to flush profile checkpoint {}", path.display()))?;
    }
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to install profile checkpoint {}", path.display()))?;
    Ok(())
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

/// The daemons to attach at startup, in the order they should be numbered.
/// With no `--attach`, the local daemon socket is the whole list.
fn host_specs(args: &Args) -> Result<Vec<HostSpec>> {
    if args.attach.is_empty() {
        return Ok(vec![HostSpec {
            name: "local".to_owned(),
            target: AttachTarget::Unix(rho_ui_proto::socket_path()?),
        }]);
    }
    let mut specs = Vec::new();
    for host in &args.attach {
        let spec = HostSpec::parse(host, &args.remote_rho)
            .map_err(|error| anyhow::anyhow!("--attach {host}: {error}"))?;
        anyhow::ensure!(
            !specs.iter().any(|other: &HostSpec| other.name == spec.name),
            "host {:?} is attached twice; names label agents and must be distinct",
            spec.name
        );
        specs.push(spec);
    }
    Ok(specs)
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
    cx.set_global(store);
    apply_text_rendering_mode(cx);
    cx.observe_global::<SettingsStore>(apply_text_rendering_mode)
        .detach();
    theme_settings::init(theme::LoadThemes::All(Box::new(RhoAssets)), cx);
    release_channel::init(semver::Version::new(0, 1, 0), cx);
    editor::init(cx);
    command_palette::init(cx);
    search::init(cx);
    rho_gui::init_vim_mode(cx).context("failed to initialize Rho Vim mode")?;
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

#[cfg(test)]
mod profile_checkpoint_tests {
    use super::*;

    fn checkpoint(directory: &Path) -> ProfileCheckpoint {
        let frame_path = directory.join("run.frames.json");
        let editor_path = directory.join("run.editor.json");
        let work_path = directory.join("run.work.json");
        ProfileCheckpoint {
            state: Mutex::new(ProfileState {
                frames: gpui::profiler::FrameTimingCollector::new(),
                editor: gpui::profiler::EditorTimingCollector::new(),
                collected_frames: Vec::new(),
                collected_editor: Vec::new(),
            }),
            writer: Mutex::new(()),
            finalized: AtomicBool::new(false),
            final_written: AtomicBool::new(false),
            frame_path: frame_path.clone(),
            editor_path: editor_path.clone(),
            work_path: work_path.clone(),
        }
    }

    #[test]
    fn panic_flush_leaves_complete_readable_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        let frame_path = directory.path().join("run.frames.json");
        let editor_path = directory.path().join("run.editor.json");
        let work_path = directory.path().join("run.work.json");
        fs::write(&frame_path, "incomplete").unwrap();
        fs::write(&editor_path, "incomplete").unwrap();
        fs::write(&work_path, "incomplete").unwrap();
        let checkpoint = checkpoint(directory.path());

        flush_profile_after_panic(&checkpoint);

        let frames: serde_json::Value = serde_json::from_slice(&fs::read(&frame_path).unwrap())
            .expect("the panic-side frame checkpoint is complete JSON");
        let editor: serde_json::Value = serde_json::from_slice(&fs::read(&editor_path).unwrap())
            .expect("the panic-side editor checkpoint is complete JSON");
        let work: serde_json::Value = serde_json::from_slice(&fs::read(&work_path).unwrap())
            .expect("the panic-side work checkpoint is complete JSON");
        assert!(frames["frames"].is_array());
        assert!(editor["events"].is_array());
        assert!(work["spans"].is_array());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 3);
    }

    #[test]
    fn panic_flush_waits_for_an_in_flight_periodic_writer() {
        let directory = tempfile::tempdir().unwrap();
        let checkpoint = Arc::new(checkpoint(directory.path()));
        let (locked, wait) = std::sync::mpsc::channel();
        let holder = Arc::clone(&checkpoint);
        let thread = std::thread::spawn(move || {
            let _writer = holder.writer.lock().unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
        });
        wait.recv().unwrap();

        flush_profile_after_panic(&checkpoint);
        thread.join().unwrap();

        assert!(checkpoint.frame_path.exists());
        assert!(checkpoint.editor_path.exists());
        assert!(checkpoint.work_path.exists());
    }
}
