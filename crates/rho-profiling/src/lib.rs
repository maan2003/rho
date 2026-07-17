//! Opt-in, unprivileged CPU profiling shared by Rho executables.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context as _;

pub struct CpuProfiler {
    path: PathBuf,
    guard: pprof::ProfilerGuard<'static>,
}

/// A domain span to place on the same monotonic timeline as CPU samples.
pub struct TimelineSpan {
    pub name: String,
    pub category: &'static str,
    pub start: Instant,
    pub end: Instant,
    pub tid: u64,
    pub args: serde_json::Map<String, serde_json::Value>,
}

impl CpuProfiler {
    pub fn start(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = absolute_path(path.into())?;
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(100)
            .build()
            .context("start in-process CPU profiler")?;
        Ok(Self { path, guard })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn finish(self) -> anyhow::Result<PathBuf> {
        self.finish_with_spans([])
    }

    pub fn finish_with_spans(
        mut self,
        spans: impl IntoIterator<Item = TimelineSpan>,
    ) -> anyhow::Result<PathBuf> {
        self.guard.stop().context("stop CPU profiler")?;
        let report = self.guard.report().build().context("build CPU profile")?;
        let temporal = self.guard.report().build_temporal();
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

        let file = File::create(&self.path)
            .with_context(|| format!("create CPU profile {}", self.path.display()))?;
        let mut writer = BufWriter::new(file);
        for (stack, count) in stacks {
            writeln!(writer, "{stack} {count}")
                .with_context(|| format!("write CPU profile {}", self.path.display()))?;
        }
        writer
            .flush()
            .with_context(|| format!("flush CPU profile {}", self.path.display()))?;
        write_timeline(
            &sidecar_path(&self.path, ".trace.json"),
            &report.timing,
            temporal,
            spans,
        )?;
        Ok(self.path)
    }
}

fn write_timeline(
    path: &Path,
    timing: &pprof::ReportTiming,
    temporal: pprof::TemporalReport,
    spans: impl IntoIterator<Item = TimelineSpan>,
) -> anyhow::Result<()> {
    use std::collections::HashMap;

    use serde_json::{Map, Value, json};

    let mut symbol_names = HashMap::<usize, String>::new();
    let mut frame_ids = HashMap::<(Option<u64>, String), u64>::new();
    let mut stack_frames = Map::new();
    let mut next_frame_id = 0_u64;
    let mut temporal_stack_ids = Vec::with_capacity(temporal.stacks.len());
    let mut temporal_stack_names = Vec::with_capacity(temporal.stacks.len());

    for stack in &temporal.stacks {
        let mut names = Vec::with_capacity(stack.instruction_pointers.len());
        let mut index = 0;
        while index < stack.instruction_pointers.len() {
            let instruction_pointer = stack.instruction_pointers[index];
            let name = symbol_names
                .entry(instruction_pointer)
                .or_insert_with(|| symbol_name(instruction_pointer))
                .clone();
            if name.contains("perf_signal_handler") {
                // Match pprof's aggregate report: omit the handler and the
                // signal trampoline immediately above it.
                index += 2;
            } else {
                names.push(name);
                index += 1;
            }
        }
        temporal_stack_names.push(names.iter().rev().cloned().collect::<Vec<_>>().join(";"));
        let mut parent = None;
        for name in names.into_iter().rev() {
            let key = (parent, name.clone());
            let frame_id = *frame_ids.entry(key).or_insert_with(|| {
                let frame_id = next_frame_id;
                next_frame_id += 1;
                let mut frame = Map::new();
                frame.insert("name".to_owned(), Value::String(name));
                if let Some(parent) = parent {
                    frame.insert("parent".to_owned(), Value::String(parent.to_string()));
                }
                stack_frames.insert(frame_id.to_string(), Value::Object(frame));
                frame_id
            });
            parent = Some(frame_id);
        }
        temporal_stack_ids.push(parent);
    }

    let pid = std::process::id();
    let file = File::create(path)
        .with_context(|| format!("create timeline profile {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(b"{\"traceEvents\":[")?;
    {
        let mut first_event = true;
        let mut write_event = |event: Value| -> anyhow::Result<()> {
            if !first_event {
                writer.write_all(b",")?;
            }
            first_event = false;
            serde_json::to_writer(&mut writer, &event)?;
            Ok(())
        };
        write_event(json!({
        "name": "process_name",
        "ph": "M",
        "pid": pid,
        "tid": 0,
        "args": {"name": std::env::args().next().unwrap_or_else(|| "rho".to_owned())}
        }))?;
        for (tid, name) in &temporal.thread_names {
            write_event(json!({
            "name": "thread_name",
            "ph": "M",
            "pid": pid,
            "tid": tid,
            "args": {"name": name}
            }))?;
        }
        for sample in &temporal.samples {
            let mut event = json!({
            "name": "CPU sample",
            "cat": "cpu",
            "ph": "i",
            "s": "t",
            "pid": pid,
            "tid": sample.tid,
            "ts": relative_us(sample.monotonic_ns, timing.start_monotonic_ns),
            "args": {
                "stack_id": sample.stack_id,
                "stack": &temporal_stack_names[sample.stack_id as usize],
                "truncated": temporal.stacks[sample.stack_id as usize].truncated,
            },
            });
            if let Some(stack_id) = temporal_stack_ids[sample.stack_id as usize] {
                event["sf"] = Value::String(stack_id.to_string());
            }
            write_event(event)?;
        }
        for span in spans {
            let start_ns = instant_ns(span.start, timing);
            let end_ns = instant_ns(span.end, timing).max(start_ns);
            write_event(json!({
            "name": span.name,
            "cat": span.category,
            "ph": "X",
            "pid": pid,
            "tid": span.tid,
            "ts": relative_us(start_ns, timing.start_monotonic_ns),
            "dur": (end_ns - start_ns) as f64 / 1_000.0,
            "args": span.args,
            }))?;
        }
    }

    let truncated_stacks = temporal
        .stacks
        .iter()
        .filter(|stack| stack.truncated)
        .count();
    writer.write_all(b"],\"stackFrames\":")?;
    serde_json::to_writer(&mut writer, &stack_frames)?;
    writer.write_all(b",\"rhoProfile\":")?;
    serde_json::to_writer(
        &mut writer,
        &json!({
            "version": 1,
            "clock": "CLOCK_MONOTONIC",
            "frequency_hz": timing.frequency,
            "duration_ns": duration_ns(timing.duration),
            "dropped_samples": temporal.dropped_samples,
            "truncated_stacks": truncated_stacks,
            "perfetto_stack_encoding": "CPU sample args.stack, root-to-leaf",
        }),
    )?;
    writer.write_all(b"}")?;
    writer
        .flush()
        .with_context(|| format!("write timeline profile {}", path.display()))
}

fn symbol_name(instruction_pointer: usize) -> String {
    let mut names = Vec::new();
    backtrace::resolve(instruction_pointer as *mut std::ffi::c_void, |symbol| {
        if let Some(name) = symbol.name() {
            names.push(name.to_string());
        }
    });
    if names.is_empty() {
        format!("0x{instruction_pointer:x}")
    } else {
        names.join(" <- ")
    }
}

fn instant_ns(instant: Instant, timing: &pprof::ReportTiming) -> u64 {
    if let Some(elapsed) = instant.checked_duration_since(timing.start_instant) {
        timing
            .start_monotonic_ns
            .saturating_add(duration_ns(elapsed))
    } else {
        timing
            .start_monotonic_ns
            .saturating_sub(duration_ns(timing.start_instant.duration_since(instant)))
    }
}

fn relative_us(timestamp_ns: u64, anchor_ns: u64) -> f64 {
    timestamp_ns.saturating_sub(anchor_ns) as f64 / 1_000.0
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Return the calling thread's operating-system thread id.
pub fn current_tid() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // pprof uses pthread identity as the temporal thread id off Linux.
        unsafe { libc::pthread_self() as u64 }
    }
}

pub fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    path.into()
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

fn push_folded_name(output: &mut String, name: &str) {
    output.extend(name.chars().map(|character| match character {
        ';' | '\n' | '\r' => ' ',
        character => character,
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_an_unprivileged_folded_profile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cpu.folded");
        let profiler = super::CpuProfiler::start(&path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while std::time::Instant::now() < deadline {
            std::hint::black_box((1..100).sum::<u64>());
        }
        let span_start = std::time::Instant::now();
        assert_eq!(
            profiler
                .finish_with_spans([super::TimelineSpan {
                    name: "test.span.v1".to_owned(),
                    category: "test",
                    start: span_start,
                    end: span_start + std::time::Duration::from_millis(1),
                    tid: super::current_tid(),
                    args: serde_json::Map::new(),
                }])
                .unwrap(),
            path
        );
        assert!(!std::fs::read_to_string(path).unwrap().is_empty());
        let trace_path = directory.path().join("cpu.folded.trace.json");
        let trace: serde_json::Value =
            serde_json::from_reader(std::fs::File::open(&trace_path).unwrap()).unwrap();
        assert_eq!(trace["rhoProfile"]["version"], 1);
        assert!(
            trace["traceEvents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["name"] == "CPU sample")
        );
        assert!(
            trace["traceEvents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["name"] == "test.span.v1")
        );
        assert!(!trace["stackFrames"].as_object().unwrap().is_empty());
        if let Ok(output) = std::process::Command::new("trace_processor_shell")
            .arg(&trace_path)
            .args([
                "-Q",
                "select \
                   (select count(*) from args where key = 'args.stack') as stack_args, \
                   (select count(*) from slice where name = 'test.span.v1') as spans;",
            ])
            .output()
        {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let output = String::from_utf8_lossy(&output.stdout);
            let values = output
                .lines()
                .nth(1)
                .unwrap()
                .split(',')
                .collect::<Vec<_>>();
            assert_ne!(values[0], "0", "{output}");
            assert_eq!(values[1], "1", "{output}");
        }
    }
}
