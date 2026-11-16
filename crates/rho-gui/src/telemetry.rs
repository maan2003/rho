//! Where a slow frame goes.
//!
//! Always on and bounded: GPUI's frame, editor-stage and main-thread-work
//! rings, and a rolling CPU profile of the last seconds, cost little and
//! are what a report from the user's own device is made of. The palette
//! row `upload performance report` sends a snapshot of them to an attached
//! host, which stores it in its state directory. `--cpu-profile FILE`
//! writes one whole run to a file instead, for a rig.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use gpui::profiler::{EditorTiming, EditorTimingCollector, FrameTiming, FrameTimingCollector};
use gpui::{App, AppContext as _, Context, Window};
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_frame::{Address, AnySurface, EndpointId, FrameLink, Point, Row, System};
use rho_window::style::StyleClass;

/// Seconds a rolling segment covers before the next starts.
const ROLLING_PERIOD: Duration = Duration::from_secs(2);
/// Disk the rolling profile may hold, and so the most a report carries.
const ROLLING_BUDGET: u64 = 8 * 1024 * 1024;
/// Segments a report takes: the newest, about half a minute.
const SNAPSHOT_SEGMENTS: usize = 16;
/// Samples a second while always on. A frame is milliseconds, so fewer
/// would see one in ten; more costs the slow device the report is about.
const PASSIVE_HZ: u64 = 250;
/// Samples a second under `--cpu-profile`, on a rig.
const RIG_HZ: u64 = 1000;
const MAX_SNAPSHOT_FRAMES: usize = 8192;
const MAX_SNAPSHOT_EDITOR_EVENTS: usize = 4096;
const MAX_SNAPSHOT_WORK: usize = 4096;

static STARTED: OnceLock<Instant> = OnceLock::new();
static PASSIVE: Mutex<Option<Passive>> = Mutex::new(None);

struct Passive {
    profiler: rho_profiling::CpuProfiler,
    _directory: tempfile::TempDir,
}

/// Turns the always-on collection on: GPUI's traces and, unless a
/// `--cpu-profile` run holds the one Dial9 recorder a process has, the
/// rolling CPU profile.
pub fn enable(rolling_cpu: bool) {
    STARTED.get_or_init(Instant::now);
    gpui::profiler::set_frame_trace_enabled(true);
    gpui::profiler::set_editor_trace_enabled(true);
    if !rolling_cpu {
        return;
    }
    match start_passive() {
        Ok(passive) => *PASSIVE.lock().unwrap() = Some(passive),
        Err(error) => tracing::warn!(%error, "the rolling GUI CPU profile is unavailable"),
    }
}

fn start_passive() -> Result<Passive> {
    let directory = tempfile::Builder::new().prefix("rho-gui-cpu-").tempdir()?;
    let profiler = rho_profiling::CpuProfiler::start_rolling(
        directory.path(),
        ROLLING_PERIOD,
        ROLLING_BUDGET,
        PASSIVE_HZ,
    )?;
    Ok(Passive {
        profiler,
        _directory: directory,
    })
}

/// Stops the rolling profile, before the process goes.
pub fn shutdown() {
    if let Some(passive) = PASSIVE.lock().unwrap().take()
        && let Err(error) = passive.profiler.shutdown()
    {
        tracing::warn!(%error, "failed to stop the rolling GUI CPU profile");
    }
}

/// The report: what the rings and the rolling profile hold now, as JSON.
pub fn snapshot() -> Result<Vec<u8>> {
    let started = *STARTED.get_or_init(Instant::now);
    let cpu_profile = PASSIVE
        .lock()
        .unwrap()
        .as_ref()
        .map(|passive| passive.profiler.snapshot_segments(SNAPSHOT_SEGMENTS))
        .transpose()?
        .map(|segments| CpuProfileSnapshot {
            sampling_hz: PASSIVE_HZ,
            tail_unsealed: segments.tail_unsealed,
            format: "dial9-trace",
            encoding: "base64; a segment starting with 1f8b is gzip",
            segments: segments
                .segments
                .iter()
                .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
                .collect(),
        });
    let frames = gpui::profiler::snapshot_frame_timings();
    let frames = &frames[frames.len().saturating_sub(MAX_SNAPSHOT_FRAMES)..];
    let editor = gpui::profiler::snapshot_editor_timings();
    let editor = &editor[editor.len().saturating_sub(MAX_SNAPSHOT_EDITOR_EVENTS)..];
    let (work, work_pushed) = gpui::profiler::snapshot_main_thread_work();
    let work = &work[work.len().saturating_sub(MAX_SNAPSHOT_WORK)..];
    let presents = gpui::profiler::snapshot_present_timings();
    let snapshot = Snapshot {
        schema: "rho-gui-telemetry",
        version: 3,
        captured_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        build: BuildInfo {
            version: env!("CARGO_PKG_VERSION"),
            debug_assertions: cfg!(debug_assertions),
            arch: std::env::consts::ARCH,
            os: std::env::consts::OS,
        },
        uptime_ms: ms(started.elapsed()),
        executable: std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        maps: std::fs::read_to_string("/proc/self/maps").unwrap_or_default(),
        cpu_profile,
        frames: frames
            .iter()
            .map(|frame| frame_record(frame, &presents, started))
            .collect(),
        frames_pushed: gpui::profiler::frame_timings_pushed(),
        editor: editor
            .iter()
            .map(|event| stage_record(event, started))
            .collect(),
        editor_pushed: gpui::profiler::editor_timings_pushed(),
        main_thread_work: work
            .iter()
            .map(|work| WorkRecord {
                at_ms: ms(work.start.saturating_duration_since(started)),
                ms: ms(work.end.saturating_duration_since(work.start)),
                owner: format!("{:?}", work.owner),
                work_units: work.work_units,
            })
            .collect(),
        main_thread_work_pushed: work_pushed,
    };
    serde_json::to_vec(&snapshot).context("encode the performance snapshot")
}

#[derive(serde::Serialize)]
struct Snapshot {
    schema: &'static str,
    version: u32,
    captured_unix_ms: u64,
    build: BuildInfo,
    uptime_ms: f64,
    executable: String,
    /// `/proc/self/maps`: what symbolizes the profile's addresses against
    /// the build's debug info when the device could not.
    maps: String,
    cpu_profile: Option<CpuProfileSnapshot>,
    frames: Vec<FrameRecord>,
    /// How many frames were ever recorded, against those in `frames`: the
    /// ring drops its oldest, and a total over an unknown window misleads.
    frames_pushed: u64,
    editor: Vec<StageRecord>,
    editor_pushed: u64,
    main_thread_work: Vec<WorkRecord>,
    main_thread_work_pushed: u64,
}

#[derive(serde::Serialize)]
struct BuildInfo {
    version: &'static str,
    debug_assertions: bool,
    arch: &'static str,
    os: &'static str,
}

#[derive(serde::Serialize)]
struct CpuProfileSnapshot {
    sampling_hz: u64,
    tail_unsealed: bool,
    format: &'static str,
    encoding: &'static str,
    segments: Vec<String>,
}

#[derive(serde::Serialize)]
struct WorkRecord {
    at_ms: f64,
    ms: f64,
    owner: String,
    work_units: u64,
}

/// `--cpu-profile FILE`: one run, to a file, with GPUI's frame and editor
/// spans on the CPU timeline and the frames and stages as JSON beside it.
pub struct Profile {
    cpu: rho_profiling::CpuProfiler,
    started: Instant,
    collected: Arc<Mutex<Collected>>,
}

/// What GPUI's bounded rings held, drained before they wrap.
struct Collected {
    frames: FrameTimingCollector,
    editor: EditorTimingCollector,
    all_frames: Vec<FrameTiming>,
    all_editor: Vec<EditorTiming>,
}

impl Collected {
    fn collect(&mut self) {
        self.all_frames.extend(self.frames.collect_unseen());
        self.all_editor.extend(self.editor.collect_unseen());
    }
}

impl Profile {
    pub fn start(path: PathBuf) -> Result<Self> {
        let cpu = rho_profiling::CpuProfiler::start_at(path, RIG_HZ)?;
        enable(false);
        Ok(Self {
            cpu,
            started: *STARTED.get_or_init(Instant::now),
            collected: Arc::new(Mutex::new(Collected {
                frames: FrameTimingCollector::new(),
                editor: EditorTimingCollector::new(),
                all_frames: Vec::new(),
                all_editor: Vec::new(),
            })),
        })
    }

    /// Drains the rings every few seconds, off the foreground thread.
    pub fn keep_collecting(&self, cx: &mut App) {
        let collected = Arc::clone(&self.collected);
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            loop {
                executor.timer(Duration::from_secs(2)).await;
                collected.lock().unwrap().collect();
            }
        })
        .detach();
    }

    /// Writes everything. `draw_tid` is the thread that draws windows, so
    /// frame spans land on its CPU samples.
    pub fn finish(self, draw_tid: u64) -> Result<()> {
        let (frames, editor) = {
            let mut collected = self.collected.lock().unwrap();
            collected.collect();
            (
                std::mem::take(&mut collected.all_frames),
                std::mem::take(&mut collected.all_editor),
            )
        };
        let frames_path = rho_profiling::sidecar_path(self.cpu.path(), ".frames.json");
        let editor_path = rho_profiling::sidecar_path(self.cpu.path(), ".editor.json");
        let presents = gpui::profiler::snapshot_present_timings();
        write_json(
            &frames_path,
            &frame_profile(&frames, &presents, self.started),
        )?;
        write_json(&editor_path, &editor_profile(&editor, self.started))?;
        let cpu_path = self
            .cpu
            .finish_with_gui_spans(frame_spans(&frames, draw_tid), editor_spans(&editor))?;
        eprintln!(
            "rho-gui: wrote CPU profile to {}, frames to {}, editor stages to {}",
            cpu_path.display(),
            frames_path.display(),
            editor_path.display()
        );
        Ok(())
    }
}

/// The palette row that sends a report, and the host links to send it by.
pub struct TelemetrySystem {
    frame: FrameLink,
    hosts: HashMap<HostId, (String, Link)>,
}

impl TelemetrySystem {
    pub fn new(frame: FrameLink) -> Self {
        Self {
            frame,
            hosts: HashMap::new(),
        }
    }

    fn upload(&mut self, cx: &mut Context<Self>) {
        let Some((name, link)) = self.hosts.values().min_by(|a, b| a.0.cmp(&b.0)).cloned() else {
            self.frame.echo(
                "performance report: no host is attached",
                StyleClass::StatusError,
                cx,
            );
            return;
        };
        let snapshot = match snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.frame.echo(
                    format!("performance report failed: {error:#}"),
                    StyleClass::StatusError,
                    cx,
                );
                return;
            }
        };
        self.frame.echo(
            format!(
                "uploading a {} KiB performance report to {name}…",
                snapshot.len() / 1024
            ),
            StyleClass::SystemInfo,
            cx,
        );
        let upload = link.upload_gui_telemetry(snapshot);
        let frame = self.frame.clone();
        cx.spawn(async move |_, cx| {
            let result = upload.await;
            cx.update(|cx| match result {
                Ok(path) => frame.log(
                    format!("performance report stored on {name} at {path}"),
                    StyleClass::SystemInfo,
                    cx,
                ),
                Err(error) => frame.log(
                    format!("performance report upload to {name} failed: {error:#}"),
                    StyleClass::StatusError,
                    cx,
                ),
            });
        })
        .detach();
    }
}

impl System for TelemetrySystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn open(
        &mut self,
        _address: &Address,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        None
    }

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        _endpoint: Option<EndpointId>,
        link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        self.hosts.insert(host, (name.to_owned(), link.clone()));
        Vec::new()
    }

    fn detached(&mut self, host: HostId, _cx: &mut Context<Self>) {
        self.hosts.remove(&host);
    }

    fn rows(&mut self, _point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        vec![Row::run(
            "",
            "upload performance report",
            move |_, _, cx| {
                this.update(cx, |system, cx| system.upload(cx));
            },
        )]
    }
}

#[derive(serde::Serialize)]
struct FrameProfile {
    frame_count: usize,
    /// Frames whose draw alone took longer than a 60 Hz frame.
    over_16ms: usize,
    draw_ms: Distribution,
    prepaint_ms: Distribution,
    paint_ms: Distribution,
    dirty_to_draw_ms: Distribution,
    frames: Vec<FrameRecord>,
}

#[derive(serde::Serialize)]
struct FrameRecord {
    /// Since the profile started.
    at_ms: f64,
    draw_ms: f64,
    prepaint_ms: f64,
    paint_ms: f64,
    dirty_to_draw_ms: Option<f64>,
    /// The platform's presentation of this frame, when it reported one.
    present_ms: Option<f64>,
    invalidations: u64,
    /// What the frame had to draw, as the elements reported it.
    work: String,
}

#[derive(serde::Serialize)]
struct EditorProfile {
    stages: Vec<StageTotal>,
    events: Vec<StageRecord>,
}

#[derive(serde::Serialize)]
struct StageTotal {
    kind: String,
    count: usize,
    total_ms: f64,
    max_ms: f64,
}

#[derive(serde::Serialize)]
struct StageRecord {
    at_ms: f64,
    kind: String,
    ms: f64,
    input_rows: u64,
    touched_rows: u64,
    walked_items: u64,
    transforms: u64,
    affected_offsets: u64,
}

#[derive(serde::Serialize, Default)]
struct Distribution {
    p50: f64,
    p90: f64,
    p99: f64,
    max: f64,
}

fn distribution(values: impl Iterator<Item = f64>) -> Distribution {
    let mut values: Vec<f64> = values.collect();
    if values.is_empty() {
        return Distribution::default();
    }
    values.sort_by(f64::total_cmp);
    let at = |fraction: f64| values[((values.len() - 1) as f64 * fraction).round() as usize];
    Distribution {
        p50: at(0.5),
        p90: at(0.9),
        p99: at(0.99),
        max: values[values.len() - 1],
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn frame_record(
    frame: &FrameTiming,
    presents: &[gpui::profiler::PresentTiming],
    started: Instant,
) -> FrameRecord {
    let present = presents
        .iter()
        .skip(presents.partition_point(|present| present.start < frame.draw_end))
        .find(|present| present.window_id == frame.window_id);
    FrameRecord {
        at_ms: ms(frame.draw_start.saturating_duration_since(started)),
        draw_ms: ms(frame.draw_duration()),
        prepaint_ms: ms(frame.prepaint_duration()),
        paint_ms: ms(frame.paint_duration()),
        dirty_to_draw_ms: frame.dirty_to_draw_duration().map(ms),
        present_ms: present.map(|present| ms(present.end.saturating_duration_since(present.start))),
        invalidations: frame.invalidations,
        work: format!("{:?}", frame.work),
    }
}

fn frame_profile(
    frames: &[FrameTiming],
    presents: &[gpui::profiler::PresentTiming],
    started: Instant,
) -> FrameProfile {
    let records: Vec<FrameRecord> = frames
        .iter()
        .map(|frame| frame_record(frame, presents, started))
        .collect();
    FrameProfile {
        frame_count: records.len(),
        over_16ms: records.iter().filter(|frame| frame.draw_ms > 16.0).count(),
        draw_ms: distribution(records.iter().map(|frame| frame.draw_ms)),
        prepaint_ms: distribution(records.iter().map(|frame| frame.prepaint_ms)),
        paint_ms: distribution(records.iter().map(|frame| frame.paint_ms)),
        dirty_to_draw_ms: distribution(records.iter().filter_map(|frame| frame.dirty_to_draw_ms)),
        frames: records,
    }
}

fn stage_record(event: &EditorTiming, started: Instant) -> StageRecord {
    StageRecord {
        at_ms: ms(event.start.saturating_duration_since(started)),
        kind: format!("{:?}", event.kind),
        ms: ms(event.end.saturating_duration_since(event.start)),
        input_rows: event.input_rows,
        touched_rows: event.touched_rows,
        walked_items: event.walked_items,
        transforms: event.transforms,
        affected_offsets: event.affected_offsets,
    }
}

fn editor_profile(events: &[EditorTiming], started: Instant) -> EditorProfile {
    let mut totals: std::collections::BTreeMap<String, StageTotal> = Default::default();
    let mut records = Vec::with_capacity(events.len());
    for event in events {
        let record = stage_record(event, started);
        let total = totals
            .entry(record.kind.clone())
            .or_insert_with(|| StageTotal {
                kind: record.kind.clone(),
                count: 0,
                total_ms: 0.0,
                max_ms: 0.0,
            });
        total.count += 1;
        total.total_ms += record.ms;
        total.max_ms = total.max_ms.max(record.ms);
        records.push(record);
    }
    let mut stages: Vec<StageTotal> = totals.into_values().collect();
    stages.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
    EditorProfile {
        stages,
        events: records,
    }
}

fn frame_spans(frames: &[FrameTiming], draw_tid: u64) -> Vec<rho_profiling::GpuiFrameSpan> {
    let mut spans = Vec::with_capacity(frames.len() * 2);
    for (index, frame) in frames.iter().enumerate() {
        let span = |kind, start| rho_profiling::GpuiFrameSpan {
            kind,
            start,
            end: frame.draw_end,
            tid: draw_tid,
            frame: index as u64,
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

fn editor_spans(events: &[EditorTiming]) -> Vec<rho_profiling::EditorStageSpan> {
    events
        .iter()
        .map(|event| rho_profiling::EditorStageSpan {
            kind: event.kind as u64,
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

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(std::io::BufWriter::new(file), value)
        .with_context(|| format!("write {}", path.display()))
}
