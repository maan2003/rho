//! Dial9 CPU profiling shared by Rho executables.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;
use dial9_tokio_telemetry::telemetry::{
    RotatingWriter, TelemetryCore, TelemetryGuard, TelemetryHandle, clock_monotonic_ns,
    record_event,
};
use dial9_trace_format::TraceEvent;

pub struct CpuProfiler {
    path: PathBuf,
    output_path: PathBuf,
    start_instant: Instant,
    start_monotonic_ns: u64,
    guard: TelemetryGuard,
    handle: TelemetryHandle,
}

/// A GPUI frame span to place on the same monotonic timeline as CPU samples.
pub struct GpuiFrameSpan {
    pub kind: GpuiFrameSpanKind,
    pub start: Instant,
    pub end: Instant,
    pub tid: u64,
    pub frame: u64,
    pub window: u64,
    pub invalidations: u64,
}

pub enum GpuiFrameSpanKind {
    Latency,
    Draw,
}

pub struct EditorStageSpan {
    pub kind: u64,
    pub start: Instant,
    pub end: Instant,
    pub tid: u64,
    pub input_edits: u64,
    pub input_start: u64,
    pub input_rows: u64,
    pub output_edits: u64,
    pub output_start: u64,
    pub output_rows: u64,
    pub old_rows: u64,
    pub new_rows: u64,
    pub pending_batches: u64,
    pub flags: u64,
}

#[derive(TraceEvent)]
struct RhoProfileSession {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    frequency_hz: u64,
}

#[derive(TraceEvent)]
struct RhoGpuiLatencyV1 {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    duration_ns: u64,
    tid: u64,
    frame: u64,
    window: u64,
    invalidations: u64,
}

#[derive(TraceEvent)]
struct RhoGpuiDrawV1 {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    duration_ns: u64,
    tid: u64,
    frame: u64,
    window: u64,
    invalidations: u64,
}

#[derive(TraceEvent)]
struct RhoEditorStageV1 {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    duration_ns: u64,
    tid: u64,
    kind: u64,
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

impl CpuProfiler {
    pub fn start(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = absolute_path(path.into())?;
        let output_path = dial9_output_path(&path);
        remove_if_exists(&output_path)?;
        remove_if_exists(&dial9_raw_output_path(&path))?;
        let writer = RotatingWriter::single_file(&path)
            .with_context(|| format!("create Dial9 trace {}", path.display()))?;
        Self::start_with_writer(path, output_path, writer)
    }

    /// Starts a bounded profiler whose self-contained segments rotate by time.
    pub fn start_rolling(
        path: impl Into<PathBuf>,
        rotation_period: Duration,
        max_total_size: u64,
    ) -> anyhow::Result<Self> {
        let path = absolute_path(path.into())?;
        let output_path = dial9_output_path(&path);
        let writer = RotatingWriter::builder()
            .base_path(&path)
            .max_file_size(max_total_size)
            .max_total_size(max_total_size)
            .rotation_period(rotation_period)
            .build()
            .with_context(|| format!("create rolling Dial9 trace {}", path.display()))?;
        Self::start_with_writer(path, output_path, writer)
    }

    fn start_with_writer(
        path: PathBuf,
        output_path: PathBuf,
        writer: RotatingWriter,
    ) -> anyhow::Result<Self> {
        let guard = TelemetryCore::builder()
            .writer(writer)
            .trace_path(path.clone())
            .cpu_profiling(CpuProfilingConfig::default().frequency_hz(100))
            .build()
            .context("start Dial9 profiler")?;
        let handle = guard.handle();
        guard.enable();
        let start_instant = Instant::now();
        let start_monotonic_ns = clock_monotonic_ns();
        record_event(
            RhoProfileSession {
                timestamp_ns: start_monotonic_ns,
                frequency_hz: 100,
            },
            &handle,
        );
        Ok(Self {
            path,
            output_path,
            start_instant,
            start_monotonic_ns,
            guard,
            handle,
        })
    }

    /// Copies the newest sealed rolling segments, oldest first.
    pub fn snapshot_segments(&self, limit: usize) -> anyhow::Result<Vec<Vec<u8>>> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy();
        let mut segments = BTreeMap::<u32, (Option<PathBuf>, Option<PathBuf>)>::new();
        for entry in std::fs::read_dir(parent)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(rest) = name
                .strip_prefix(stem.as_ref())
                .and_then(|name| name.strip_prefix('.'))
            else {
                continue;
            };
            let Some((index, suffix)) = rest.split_once(".bin") else {
                continue;
            };
            if !suffix.is_empty() && suffix != ".gz" {
                continue;
            }
            let Ok(index) = index.parse() else {
                continue;
            };
            let paths = segments.entry(index).or_default();
            if suffix == ".gz" {
                paths.1 = Some(path);
            } else {
                paths.0 = Some(path);
            }
        }
        // Active files end in `.bin.active`, which the suffix filter above
        // intentionally excludes. Only self-contained sealed segments remain.
        let skip = segments.len().saturating_sub(limit);
        segments
            .into_values()
            .skip(skip)
            .filter_map(|(raw, compressed)| {
                for path in compressed.into_iter().chain(raw) {
                    match std::fs::read(&path) {
                        Ok(bytes) => return Some(Ok(bytes)),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            return Some(Err(error).with_context(|| {
                                format!("read Dial9 segment {}", path.display())
                            }));
                        }
                    }
                }
                // The background processor may replace or evict a segment
                // between read_dir and read. A missing segment is harmless.
                None
            })
            .collect()
    }

    /// Stops a rolling profiler and flushes its current segment.
    pub fn shutdown(self) -> anyhow::Result<()> {
        self.guard
            .graceful_shutdown(Duration::from_secs(30))
            .context("finish Dial9 trace")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn finish(self) -> anyhow::Result<PathBuf> {
        self.finish_with_gpui_spans([])
    }

    pub fn finish_with_gpui_spans(
        self,
        spans: impl IntoIterator<Item = GpuiFrameSpan>,
    ) -> anyhow::Result<PathBuf> {
        self.finish_with_gui_spans(spans, [])
    }

    pub fn finish_with_gui_spans(
        self,
        frame_spans: impl IntoIterator<Item = GpuiFrameSpan>,
        editor_spans: impl IntoIterator<Item = EditorStageSpan>,
    ) -> anyhow::Result<PathBuf> {
        for span in frame_spans {
            let timestamp_ns = instant_ns(span.start, self.start_instant, self.start_monotonic_ns);
            let duration_ns = duration_ns(span.end.saturating_duration_since(span.start));
            match span.kind {
                GpuiFrameSpanKind::Latency => record_event(
                    RhoGpuiLatencyV1 {
                        timestamp_ns,
                        duration_ns,
                        tid: span.tid,
                        frame: span.frame,
                        window: span.window,
                        invalidations: span.invalidations,
                    },
                    &self.handle,
                ),
                GpuiFrameSpanKind::Draw => record_event(
                    RhoGpuiDrawV1 {
                        timestamp_ns,
                        duration_ns,
                        tid: span.tid,
                        frame: span.frame,
                        window: span.window,
                        invalidations: span.invalidations,
                    },
                    &self.handle,
                ),
            }
        }
        for span in editor_spans {
            record_event(
                RhoEditorStageV1 {
                    timestamp_ns: instant_ns(
                        span.start,
                        self.start_instant,
                        self.start_monotonic_ns,
                    ),
                    duration_ns: duration_ns(span.end.saturating_duration_since(span.start)),
                    tid: span.tid,
                    kind: span.kind,
                    input_edits: span.input_edits,
                    input_start: span.input_start,
                    input_rows: span.input_rows,
                    output_edits: span.output_edits,
                    output_start: span.output_start,
                    output_rows: span.output_rows,
                    old_rows: span.old_rows,
                    new_rows: span.new_rows,
                    pending_batches: span.pending_batches,
                    flags: span.flags,
                },
                &self.handle,
            );
        }
        self.guard
            .graceful_shutdown(Duration::from_secs(30))
            .context("finish Dial9 trace")?;
        let raw_output = dial9_raw_output_path(&self.path);
        if raw_output.exists() {
            return Ok(raw_output);
        }
        if self.output_path.exists() {
            validate_compressed_trace(&self.output_path)?;
            return Ok(self.output_path);
        }
        anyhow::bail!(
            "Dial9 did not produce trace output at {} or {}",
            self.output_path.display(),
            raw_output.display()
        )
    }
}

fn instant_ns(instant: Instant, anchor: Instant, anchor_ns: u64) -> u64 {
    if let Some(elapsed) = instant.checked_duration_since(anchor) {
        anchor_ns.saturating_add(duration_ns(elapsed))
    } else {
        anchor_ns.saturating_sub(duration_ns(anchor.duration_since(instant)))
    }
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Return the calling thread's operating-system thread id.
pub fn current_tid() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: gettid has no arguments or memory-safety preconditions.
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // SAFETY: pthread_self has no arguments or memory-safety preconditions.
        unsafe { libc::pthread_self() as u64 }
    }
}

/// Returns Dial9's monotonic timestamp so external timing rings can align to
/// it.
pub fn monotonic_ns() -> u64 {
    clock_monotonic_ns()
}

pub fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

fn dial9_output_path(path: &Path) -> PathBuf {
    dial9_segment_path(path, ".bin.gz")
}

fn dial9_raw_output_path(path: &Path) -> PathBuf {
    dial9_segment_path(path, ".bin")
}

fn dial9_segment_path(path: &Path, suffix: &str) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.0{suffix}"))
}

fn validate_compressed_trace(path: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("open compressed Dial9 trace {}", path.display()))?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(file)
        .read_to_end(&mut bytes)
        .with_context(|| format!("validate compressed Dial9 trace {}", path.display()))?;
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&bytes)
        .with_context(|| format!("invalid Dial9 trace header in {}", path.display()))?;
    while decoder
        .next_frame()
        .with_context(|| format!("decode Dial9 trace {}", path.display()))?
        .is_some()
    {}
    Ok(())
}

fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove old Dial9 trace {}", path.display()))
        }
    }
}

fn absolute_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("read current directory for CPU profile")?
            .join(path))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_a_dial9_profile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cpu.bin");
        std::fs::write(directory.path().join("cpu.0.bin.gz"), b"stale").unwrap();
        let profiler = super::CpuProfiler::start(&path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while std::time::Instant::now() < deadline {
            std::hint::black_box((1..100).sum::<u64>());
        }
        let span_start = std::time::Instant::now();
        let output = profiler
            .finish_with_gui_spans(
                [super::GpuiFrameSpan {
                    kind: super::GpuiFrameSpanKind::Draw,
                    start: span_start,
                    end: span_start + std::time::Duration::from_millis(1),
                    tid: super::current_tid(),
                    frame: 1,
                    window: 2,
                    invalidations: 3,
                }],
                [super::EditorStageSpan {
                    kind: 5,
                    start: span_start,
                    end: span_start + std::time::Duration::from_millis(2),
                    tid: super::current_tid(),
                    input_edits: 1,
                    input_start: 172,
                    input_rows: 1,
                    output_edits: 1,
                    output_start: 0,
                    output_rows: 173,
                    old_rows: 172,
                    new_rows: 173,
                    pending_batches: 1,
                    flags: 0,
                }],
            )
            .unwrap();
        assert_eq!(output, directory.path().join("cpu.0.bin.gz"));
        assert!(std::fs::metadata(output).unwrap().len() > 0);
    }
}
