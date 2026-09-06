//! Reading back what a rig's GUI wrote while it ran.
//!
//! Every `rig up` runs the GUI with the profiler on, and every `rig down`
//! leaves three files behind: the frame log, the editor log and the CPU
//! profile. The numbers a landing note needs are all in them, and until now
//! getting at them meant standing a viewer up over the directory. So `rig
//! down` reads them itself and prints one line: the worst frame gap, how many
//! frames went over budget, and where the main thread actually was.
//!
//! The frame and editor logs are JSON the GUI writes on the way out. The CPU
//! profile is a Dial9 trace, which is symbolized where it is written — the
//! writer resolves addresses against its own `/proc/self/maps` before
//! compressing — so the names are in the file and reading it needs the trace
//! decoder rather than the binary it came from.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use dial9_trace_format::types::FieldValueRef;
use serde::{Deserialize, Serialize};

/// What the frame log says a frame cost. A frame that misses at 60 Hz is one
/// the user sees; `dirty_to_draw` is the wait between something changing and
/// the pixels moving, which is the one they feel.
const DRAW_BUDGET_MS: f64 = 8.0;

/// The summary of one session's profile, printed on `rig down` and kept in
/// the rig's session entry so a landing note can quote it verbatim.
#[derive(Serialize, Deserialize)]
pub struct Summary {
    pub profile: String,
    pub frames: u64,
    pub draw_p99_ms: f64,
    pub draw_max_ms: f64,
    /// Frames whose draw went over `DRAW_BUDGET_MS`.
    pub over_budget: usize,
    /// The longest wait between an invalidation and the pixels moving.
    pub worst_gap_ms: f64,
    pub gap_p99_ms: f64,
    /// The editor stage with the worst p99, and the rows it had in hand —
    /// the pair is the per-event side of the cost rule.
    pub slowest_stage: Option<Stage>,
    pub events: u64,
    /// Where the samples landed, deepest frame first, richest three.
    pub top: Vec<Symbol>,
    pub samples: usize,
    /// The thread the symbols are from, when the profile named one.
    pub thread: Option<String>,
    /// The rendered line, so a note can quote it without re-deriving it.
    pub line: String,
}

#[derive(Serialize, Deserialize)]
pub struct Stage {
    pub name: String,
    pub p99_ms: f64,
    pub rows_p99: f64,
}

#[derive(Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub share: f64,
}

/// Read the three sidecars beside `profile` — the path `rig up` passed to
/// `--cpu-profile`, whose own file is never written.
pub fn summarize(profile: &Path) -> Result<Summary> {
    let frames: FrameLog = read_json(&sidecar(profile, ".frames.json"))?;
    let editor: EditorLog = read_json(&sidecar(profile, ".editor.json")).unwrap_or_default();
    let cpu = symbols(&cpu_path(profile)).unwrap_or_default();

    let over_budget = frames
        .frames
        .iter()
        .filter(|frame| frame.draw_ns as f64 / 1e6 > DRAW_BUDGET_MS)
        .count();
    let worst_gap_ms = frames
        .frames
        .iter()
        .map(|frame| frame.dirty_to_draw_ns)
        .max()
        .unwrap_or_default() as f64
        / 1e6;

    let slowest_stage = editor
        .stages
        .iter()
        .max_by(|left, right| left.1.duration_ms.p99.total_cmp(&right.1.duration_ms.p99))
        .map(|(name, stage)| Stage {
            name: name.clone(),
            p99_ms: stage.duration_ms.p99,
            rows_p99: stage.input_rows.p99,
        });

    let mut summary = Summary {
        profile: profile
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        frames: frames.summary.frame_count,
        draw_p99_ms: frames.summary.draw_ms.p99,
        draw_max_ms: frames.summary.draw_ms.max,
        over_budget,
        worst_gap_ms,
        gap_p99_ms: frames.summary.dirty_to_draw_ms.p99,
        slowest_stage,
        events: editor.event_count,
        top: cpu.top,
        samples: cpu.samples,
        thread: cpu.thread,
        line: String::new(),
    };
    summary.line = summary.render();
    Ok(summary)
}

impl Summary {
    fn render(&self) -> String {
        let mut line = format!(
            "{} frames, draw p99 {:.1} ms, {} over {DRAW_BUDGET_MS:.0} ms; worst gap {:.0} ms, p99 {:.0} ms",
            self.frames, self.draw_p99_ms, self.over_budget, self.worst_gap_ms, self.gap_p99_ms
        );
        if let Some(stage) = &self.slowest_stage {
            line.push_str(&format!(
                "; {} events, slowest stage {} p99 {:.2} ms at {:.0} rows",
                self.events, stage.name, stage.p99_ms, stage.rows_p99
            ));
        }
        if self.top.is_empty() {
            return line;
        }
        let thread = self.thread.as_deref().unwrap_or("all threads");
        let top = self
            .top
            .iter()
            .map(|symbol| format!("{} {:.0}%", symbol.name, symbol.share * 100.0))
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!("; {} samples on {thread}: {top}", self.samples));
        line
    }
}

fn sidecar(profile: &Path, suffix: &str) -> PathBuf {
    let mut path = profile.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

/// The trace segment the writer compressed: `gui-….bin` becomes
/// `gui-….0.bin.gz`, the same rule `rho-profiling` writes it under.
fn cpu_path(profile: &Path) -> PathBuf {
    let stem = profile.file_stem().unwrap_or_default().to_string_lossy();
    profile
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.0.bin.gz"))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

#[derive(Deserialize)]
struct FrameLog {
    summary: FrameSummary,
    #[serde(default)]
    frames: Vec<Frame>,
}

#[derive(Deserialize)]
struct FrameSummary {
    frame_count: u64,
    draw_ms: Distribution,
    dirty_to_draw_ms: Distribution,
}

#[derive(Deserialize)]
struct Frame {
    draw_ns: u64,
    dirty_to_draw_ns: u64,
}

#[derive(Default, Deserialize)]
struct EditorLog {
    #[serde(default)]
    event_count: u64,
    #[serde(default)]
    stages: std::collections::BTreeMap<String, StageLog>,
}

#[derive(Deserialize)]
struct StageLog {
    duration_ms: Distribution,
    input_rows: Distribution,
}

#[derive(Default, Deserialize)]
struct Distribution {
    #[serde(default)]
    p99: f64,
    #[serde(default)]
    max: f64,
}

#[derive(Default)]
struct Cpu {
    samples: usize,
    thread: Option<String>,
    top: Vec<Symbol>,
}

/// Where the samples landed. A sample is attributed to its leaf frame — the
/// function that was running, not the ones waiting on it — which is what
/// "the main thread is doing X" means.
fn symbols(path: &Path) -> Result<Cpu> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("open the CPU profile {}", path.display()))?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(file)
        .read_to_end(&mut bytes)
        .with_context(|| format!("decompress {}", path.display()))?;
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&bytes)
        .with_context(|| format!("invalid trace header in {}", path.display()))?;

    // Address to name. An address inlined into another carries several
    // entries; the deepest is the function that was actually running.
    let mut names: HashMap<u64, (u64, String)> = HashMap::new();
    // Leaf address per sample, with the thread it came from.
    let mut leaves: Vec<(u64, Option<String>)> = Vec::new();
    decoder
        .for_each_event(|event| match event.name {
            "SymbolTableEntry" => {
                let mut addr = None;
                let mut depth = 0;
                let mut name = None;
                for (field, value) in event.field_names().zip(event.fields) {
                    match (field, value) {
                        ("addr", FieldValueRef::Varint(held)) => addr = Some(*held),
                        ("inline_depth", FieldValueRef::Varint(held)) => depth = *held,
                        ("symbol_name", FieldValueRef::PooledString(held)) => {
                            name = event.string_pool.get(*held);
                        }
                        _ => {}
                    }
                }
                if let (Some(addr), Some(name)) = (addr, name)
                    && names.get(&addr).is_none_or(|(held, _)| depth >= *held)
                {
                    names.insert(addr, (depth, name.to_owned()));
                }
            }
            "CpuSampleEvent" => {
                let mut leaf = None;
                let mut thread = None;
                for (field, value) in event.field_names().zip(event.fields) {
                    match (field, value) {
                        ("callchain", FieldValueRef::PooledStackFrames(held)) => {
                            leaf = event
                                .stack_pool
                                .get(*held)
                                .and_then(|frames| frames.iter().find(|addr| **addr != 0))
                                .copied();
                        }
                        ("thread_name", FieldValueRef::PooledString(held)) => {
                            thread = event.string_pool.get(*held).map(str::to_owned);
                        }
                        _ => {}
                    }
                }
                if let Some(leaf) = leaf {
                    leaves.push((leaf, thread));
                }
            }
            _ => {}
        })
        .map_err(|error| anyhow::anyhow!("decode {}: {error:?}", path.display()))?;

    // The main thread is the one the cost rule is about: the GUI names it
    // after itself. Failing that, the busiest thread that is not the
    // profiler's own — its flush and worker threads are always in the file
    // and are never the answer. Failing that, everything, and the line says
    // so rather than implying a thread it did not have.
    let mut per_thread: HashMap<&str, usize> = HashMap::new();
    for (_, thread) in &leaves {
        if let Some(thread) = thread {
            *per_thread.entry(thread.as_str()).or_default() += 1;
        }
    }
    let thread = ["rho-gui", "main"]
        .into_iter()
        .find(|name| per_thread.contains_key(name))
        .or_else(|| {
            per_thread
                .iter()
                .filter(|(name, _)| !name.starts_with("dial9-"))
                .max_by_key(|(name, count)| (**count, std::cmp::Reverse(*name)))
                .map(|(name, _)| *name)
        })
        .map(str::to_owned);
    let mine: Vec<u64> = leaves
        .iter()
        .filter(|(_, held)| thread.is_none() || held.as_deref() == thread.as_deref())
        .map(|(leaf, _)| *leaf)
        .collect();

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for leaf in &mine {
        let name = names.get(leaf).map_or("unknown", |(_, name)| name.as_str());
        *counts.entry(name).or_default() += 1;
    }
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    let total = mine.len().max(1) as f64;
    Ok(Cpu {
        samples: mine.len(),
        thread,
        top: ranked
            .into_iter()
            .take(3)
            .map(|(name, count)| Symbol {
                name: short(name),
                share: count as f64 / total,
            })
            .collect(),
    })
}

/// A Rust symbol with its generic arguments taken off and cut to something
/// that fits on a line beside two others.
fn short(name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for character in name.chars() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(character),
            _ => {}
        }
    }
    let out = out.trim().trim_end_matches("::").to_owned();
    if out.chars().count() > 48 {
        out.chars().take(47).chain(['…']).collect()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symbol_loses_its_generics_and_fits_on_a_line() {
        assert_eq!(
            short("update_entity<rho_gui::workspace::Workspace, (), {closure_env#0}>"),
            "update_entity"
        );
        assert_eq!(
            short("gpui::window::Window::draw"),
            "gpui::window::Window::draw"
        );
        assert_eq!(short(&"a".repeat(60)).chars().count(), 48);
    }

    #[test]
    fn the_line_says_what_the_run_had_and_nothing_it_did_not() {
        let bare = Summary {
            profile: "gui.bin".to_owned(),
            frames: 81,
            draw_p99_ms: 3.574,
            draw_max_ms: 3.574,
            over_budget: 0,
            worst_gap_ms: 10.8,
            gap_p99_ms: 10.8,
            slowest_stage: None,
            events: 0,
            top: Vec::new(),
            samples: 0,
            thread: None,
            line: String::new(),
        };
        assert_eq!(
            bare.render(),
            "81 frames, draw p99 3.6 ms, 0 over 8 ms; worst gap 11 ms, p99 11 ms"
        );

        let full = Summary {
            slowest_stage: Some(Stage {
                name: "buffer_edit".to_owned(),
                p99_ms: 0.0366,
                rows_p99: 2.0,
            }),
            events: 11471,
            top: vec![Symbol {
                name: "memcpy".to_owned(),
                share: 0.1333,
            }],
            samples: 90,
            thread: Some("rho-gui".to_owned()),
            ..bare
        };
        assert_eq!(
            full.render(),
            "81 frames, draw p99 3.6 ms, 0 over 8 ms; worst gap 11 ms, p99 11 ms; \
             11471 events, slowest stage buffer_edit p99 0.04 ms at 2 rows; \
             90 samples on rho-gui: memcpy 13%"
        );
    }
}
