//! Always-on, bounded GUI timing snapshot serialization.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const CPU_ROTATION_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(feature = "native")]
const CPU_TRACE_DISK_BUDGET: u64 = 2 * 1024 * 1024;
const CPU_SNAPSHOT_SEGMENTS: usize = 5;

const MAX_SNAPSHOT_FRAMES: usize = 8_192;
const MAX_SNAPSHOT_EDITOR_EVENTS: usize = 4_096;
static STARTED: OnceLock<Instant> = OnceLock::new();
#[cfg(feature = "native")]
static MONOTONIC_ORIGIN_NS: OnceLock<u64> = OnceLock::new();
static SURFACES: OnceLock<Mutex<VecDeque<(Instant, SurfaceState)>>> = OnceLock::new();
#[cfg(feature = "native")]
static PASSIVE_CPU: OnceLock<Mutex<Option<PassiveCpuProfiler>>> = OnceLock::new();

#[derive(Clone, Copy)]
pub(crate) enum SurfaceKind {
    Dashboard,
    Draft,
    Transcript,
    File,
    Shell,
    Diff,
    Terminal,
    Browser,
    ZulipInbox,
    ZulipNarrow,
}

impl SurfaceKind {
    pub(crate) const fn bit(self) -> u16 {
        1 << self as u16
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Draft => "draft",
            Self::Transcript => "transcript",
            Self::File => "file",
            Self::Shell => "shell",
            Self::Diff => "diff",
            Self::Terminal => "terminal",
            Self::Browser => "browser",
            Self::ZulipInbox => "zulip_inbox",
            Self::ZulipNarrow => "zulip_narrow",
        }
    }
}

const SURFACE_KINDS: [SurfaceKind; 10] = [
    SurfaceKind::Dashboard,
    SurfaceKind::Draft,
    SurfaceKind::Transcript,
    SurfaceKind::File,
    SurfaceKind::Shell,
    SurfaceKind::Diff,
    SurfaceKind::Terminal,
    SurfaceKind::Browser,
    SurfaceKind::ZulipInbox,
    SurfaceKind::ZulipNarrow,
];

#[derive(Clone, Copy)]
struct SurfaceState {
    focused: SurfaceKind,
    visible: u16,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    schema: &'static str,
    version: u32,
    captured_unix_ms: u64,
    application: &'static str,
    application_version: &'a str,
    build: BuildInfo,
    monotonic_origin_ns: Option<u64>,
    cpu_profile: Option<CpuProfileSnapshot>,
    frames: Vec<FrameRecord>,
    editor: Vec<EditorRecord>,
    browser: Vec<BrowserRecord>,
    browser_frames: Vec<BrowserFrameRecord>,
    browser_commands: Vec<BrowserCommandRecord>,
}

#[derive(Serialize)]
struct CpuProfileSnapshot {
    sampling_hz: u64,
    history_seconds: u64,
    maximum_tail_gap_ms: u64,
    format: &'static str,
    encoding: &'static str,
    segments: Vec<String>,
}

#[derive(Serialize)]
struct BuildInfo {
    profile: &'static str,
    opt_level: &'static str,
    target: &'static str,
    debug_assertions: bool,
}

#[derive(Serialize)]
struct FrameRecord {
    window_id: u64,
    start_ns: u64,
    draw_ns: u64,
    prepaint_ns: u64,
    paint_ns: u64,
    finish_ns: u64,
    present_ns: Option<u64>,
    draw_to_present_ns: Option<u64>,
    dirty_to_draw_ns: Option<u64>,
    invalidations: u64,
    focused_surface: &'static str,
    visible_surfaces: Vec<&'static str>,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
struct BrowserRecord {
    stage: &'static str,
    scene_id: u64,
    barrier: u64,
    related_scene_id: Option<u64>,
    at_ns: u64,
    duration_ns: Option<u64>,
}

#[derive(Serialize)]
struct BrowserCommandRecord {
    method: String,
    at_ns: u64,
    round_trip_us: u32,
    handler_us: Option<u32>,
    ok: bool,
}

#[derive(Serialize)]
struct BrowserFrameRecord {
    tab_id: i64,
    at_ns: u64,
    frames: u32,
    window_ms: u32,
    mean_interval_us: u32,
    p95_interval_us: u32,
    max_interval_us: u32,
    long_frames: u32,
}

pub fn enable() {
    STARTED.get_or_init(Instant::now);
    #[cfg(feature = "native")]
    MONOTONIC_ORIGIN_NS.get_or_init(rho_profiling::monotonic_ns);
    gpui::profiler::set_frame_trace_enabled(true);
    gpui::profiler::set_editor_trace_enabled(true);
}

#[cfg(feature = "native")]
struct PassiveCpuProfiler {
    profiler: rho_profiling::CpuProfiler,
    _directory: tempfile::TempDir,
}

#[cfg(feature = "native")]
pub fn enable_passive_cpu_profile() -> anyhow::Result<()> {
    let mut passive = PASSIVE_CPU
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if passive.is_some() {
        return Ok(());
    }
    let directory = tempfile::Builder::new().prefix("rho-gui-cpu-").tempdir()?;
    let profiler = rho_profiling::CpuProfiler::start_rolling(
        directory.path().join("trace.bin"),
        CPU_ROTATION_PERIOD,
        CPU_TRACE_DISK_BUDGET,
    )?;
    *passive = Some(PassiveCpuProfiler {
        profiler,
        _directory: directory,
    });
    Ok(())
}

#[cfg(feature = "native")]
pub fn shutdown_passive_cpu_profile() {
    let passive = PASSIVE_CPU
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(passive) = passive
        && let Err(error) = passive.profiler.shutdown()
    {
        tracing::warn!(%error, "failed to shut down passive GUI CPU profiler");
    }
}

/// Records the privacy-safe kind of content being composed into the frame.
pub(crate) fn record_surfaces(focused: SurfaceKind, visible: u16) {
    let mut surfaces = SURFACES
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if surfaces.len() == MAX_SNAPSHOT_FRAMES {
        surfaces.pop_front();
    }
    surfaces.push_back((Instant::now(), SurfaceState { focused, visible }));
}

pub(crate) fn snapshot() -> anyhow::Result<Vec<u8>> {
    #[cfg(feature = "native")]
    let cpu_profiles = PASSIVE_CPU
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|passive| passive.profiler.snapshot_segments(CPU_SNAPSHOT_SEGMENTS))
        .transpose()?
        .unwrap_or_default();
    #[cfg(not(feature = "native"))]
    let cpu_profiles = Vec::new();
    snapshot_with_cpu_profiles(&cpu_profiles)
}

fn snapshot_with_cpu_profiles(cpu_profiles: &[Vec<u8>]) -> anyhow::Result<Vec<u8>> {
    let started = *STARTED.get_or_init(Instant::now);
    let frames = gpui::profiler::snapshot_frame_timings();
    let editor = gpui::profiler::snapshot_editor_timings();
    let presents = gpui::profiler::snapshot_present_timings();
    let browser = rho_browser::snapshot_browser_timings();
    let surfaces = SURFACES
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let frames = frames
        .into_iter()
        .rev()
        .take(MAX_SNAPSHOT_FRAMES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|timing| {
            let present = presents
                .iter()
                .skip(presents.partition_point(|present| present.start < timing.draw_end))
                .find(|present| present.window_id == timing.window_id);
            let surface = surfaces
                .partition_point(|(at, _)| *at <= timing.draw_end)
                .checked_sub(1)
                .map(|index| surfaces[index].1);
            FrameRecord {
                present_ns: present.map(|present| {
                    duration_ns(present.end.saturating_duration_since(present.start))
                }),
                draw_to_present_ns: present.map(|present| {
                    duration_ns(present.start.saturating_duration_since(timing.draw_end))
                }),
                window_id: timing.window_id.as_u64(),
                start_ns: duration_ns(timing.draw_start.saturating_duration_since(started)),
                draw_ns: duration_ns(timing.draw_duration()),
                prepaint_ns: duration_ns(timing.prepaint_duration()),
                paint_ns: duration_ns(timing.paint_duration()),
                finish_ns: duration_ns(timing.finish_duration()),
                dirty_to_draw_ns: timing.dirty_to_draw_duration().map(duration_ns),
                invalidations: timing.invalidations,
                focused_surface: surface
                    .map(|surface| surface.focused.name())
                    .unwrap_or("unknown"),
                visible_surfaces: surface
                    .map(|surface| {
                        SURFACE_KINDS
                            .into_iter()
                            .filter(|kind| surface.visible & kind.bit() != 0)
                            .map(SurfaceKind::name)
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect();
    let editor = editor
        .into_iter()
        .rev()
        .take(MAX_SNAPSHOT_EDITOR_EVENTS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|timing| EditorRecord {
            stage: editor_stage_name(timing.kind),
            start_ns: duration_ns(timing.start.saturating_duration_since(started)),
            duration_ns: duration_ns(timing.end.saturating_duration_since(timing.start)),
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
        .collect();
    let browser = browser
        .into_iter()
        .map(|timing| BrowserRecord {
            stage: browser_stage_name(timing.kind),
            scene_id: timing.scene_id,
            barrier: timing.barrier,
            related_scene_id: timing.related_scene_id,
            at_ns: duration_ns(timing.at.saturating_duration_since(started)),
            duration_ns: timing.duration.map(duration_ns),
        })
        .collect();
    let browser_frames = rho_browser::snapshot_extension_frame_stats()
        .into_iter()
        .map(|stats| BrowserFrameRecord {
            tab_id: stats.tab_id,
            at_ns: duration_ns(stats.at.saturating_duration_since(started)),
            frames: stats.frames,
            window_ms: stats.window_ms,
            mean_interval_us: stats.mean_interval_us,
            p95_interval_us: stats.p95_interval_us,
            max_interval_us: stats.max_interval_us,
            long_frames: stats.long_frames,
        })
        .collect();
    let browser_commands = rho_browser::snapshot_extension_command_stats()
        .into_iter()
        .map(|stats| BrowserCommandRecord {
            at_ns: duration_ns(stats.at.saturating_duration_since(started)),
            method: stats.method,
            round_trip_us: stats.round_trip_us,
            handler_us: stats.handler_us,
            ok: stats.ok,
        })
        .collect();
    let bytes = serde_json::to_vec_pretty(&Snapshot {
        schema: "dev.rho.gui-performance-snapshot",
        version: 7,
        captured_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        application: "rho-gui",
        application_version: env!("CARGO_PKG_VERSION"),
        build: BuildInfo {
            profile: env!("RHO_BUILD_PROFILE"),
            opt_level: env!("RHO_BUILD_OPT_LEVEL"),
            target: env!("RHO_BUILD_TARGET"),
            debug_assertions: cfg!(debug_assertions),
        },
        monotonic_origin_ns: {
            #[cfg(feature = "native")]
            {
                MONOTONIC_ORIGIN_NS.get().copied()
            }
            #[cfg(not(feature = "native"))]
            {
                None
            }
        },
        cpu_profile: (!cpu_profiles.is_empty()).then(|| {
            use base64::Engine as _;
            CpuProfileSnapshot {
                sampling_hz: 100,
                history_seconds: CPU_ROTATION_PERIOD.as_secs() * CPU_SNAPSHOT_SEGMENTS as u64,
                maximum_tail_gap_ms: CPU_ROTATION_PERIOD.as_millis() as u64,
                format: "dial9-trace-v4",
                encoding: "base64",
                segments: cpu_profiles
                    .iter()
                    .map(|trace| base64::engine::general_purpose::STANDARD.encode(trace))
                    .collect(),
            }
        }),
        frames,
        editor,
        browser,
        browser_frames,
        browser_commands,
    })?;
    anyhow::ensure!(
        bytes.len() <= rho_ui_proto::MAX_GUI_TELEMETRY_BYTES,
        "GUI performance snapshot exceeds the upload limit"
    );
    Ok(bytes)
}

fn browser_stage_name(kind: rho_browser::BrowserTimingKind) -> &'static str {
    use rho_browser::BrowserTimingKind::*;
    match kind {
        SceneProduced => "scene_produced",
        SceneCoalesced => "scene_coalesced",
        SceneReceived => "scene_received",
        SceneScheduled => "scene_scheduled",
        ScenePainted => "scene_painted",
        FrameAcknowledged => "frame_acknowledged",
        FrameCallbackSent => "frame_callback_sent",
    }
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn editor_stage_name(kind: gpui::profiler::EditorTimingKind) -> &'static str {
    use gpui::profiler::EditorTimingKind::*;
    match kind {
        BufferEdit => "buffer_edit",
        MultiBufferSync => "multi_buffer_sync",
        InlayMapSync => "inlay_map_sync",
        FoldMapSync => "fold_map_sync",
        TabMapSync => "tab_map_sync",
        WrapMapSync => "wrap_map_sync",
        BlockMapSync => "block_map_sync",
        WrapMapUpdate => "wrap_map_update",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn snapshot_is_versioned_and_bounded() {
        super::enable();
        let draw_start = std::time::Instant::now();
        super::record_surfaces(
            super::SurfaceKind::Transcript,
            super::SurfaceKind::Transcript.bit() | super::SurfaceKind::Browser.bit(),
        );
        gpui::profiler::record_frame_timing(gpui::profiler::FrameTiming {
            window_id: gpui::WindowId::from(99),
            dirty_at: Some(draw_start),
            invalidations: 3,
            draw_start,
            prepaint_end: draw_start + std::time::Duration::from_millis(1),
            paint_end: draw_start + std::time::Duration::from_millis(3),
            draw_end: draw_start + std::time::Duration::from_millis(4),
        });
        gpui::profiler::record_present_timing(gpui::profiler::PresentTiming {
            window_id: gpui::WindowId::from(99),
            start: draw_start + std::time::Duration::from_millis(5),
            end: draw_start + std::time::Duration::from_millis(7),
        });
        rho_browser::record_browser_timing(
            rho_browser::BrowserTimingKind::ScenePainted,
            42,
            7,
            None,
            Some(std::time::Duration::from_millis(3)),
        );
        let bytes = super::snapshot_with_cpu_profiles(&[]).unwrap();
        assert!(bytes.len() <= rho_ui_proto::MAX_GUI_TELEMETRY_BYTES);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["schema"], "dev.rho.gui-performance-snapshot");
        assert_eq!(value["version"], 6);
        assert_eq!(value["build"]["profile"], env!("RHO_BUILD_PROFILE"));
        assert_eq!(value["build"]["opt_level"], env!("RHO_BUILD_OPT_LEVEL"));
        assert_eq!(value["build"]["target"], env!("RHO_BUILD_TARGET"));
        assert_eq!(value["build"]["debug_assertions"], cfg!(debug_assertions));
        assert!(value["cpu_profile"].is_null());
        assert!(value["frames"].is_array());
        let frame = value["frames"]
            .as_array()
            .unwrap()
            .iter()
            .find(|frame| frame["invalidations"] == 3)
            .unwrap();
        assert_eq!(frame["draw_ns"], 4_000_000);
        assert_eq!(frame["prepaint_ns"], 1_000_000);
        assert_eq!(frame["paint_ns"], 2_000_000);
        assert_eq!(frame["finish_ns"], 1_000_000);
        assert_eq!(frame["present_ns"], 2_000_000);
        assert_eq!(frame["draw_to_present_ns"], 1_000_000);
        assert_eq!(frame["focused_surface"], "transcript");
        assert_eq!(
            frame["visible_surfaces"],
            serde_json::json!(["transcript", "browser"])
        );
        assert!(value["editor"].is_array());
        assert!(value["browser"].is_array());
        assert!(value["browser"].as_array().unwrap().iter().any(|record| {
            record["stage"] == "scene_painted" && record["scene_id"] == 42 && record["barrier"] == 7
        }));
    }

    #[test]
    fn snapshot_embeds_dial9_profile() {
        let bytes = super::snapshot_with_cpu_profiles(&[vec![1, 2, 3]]).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["cpu_profile"]["format"], "dial9-trace-v4");
        assert_eq!(value["cpu_profile"]["encoding"], "base64");
        assert_eq!(
            value["cpu_profile"]["segments"],
            serde_json::json!(["AQID"])
        );
    }
}
