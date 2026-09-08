//! Reading back what a rig's GUI wrote while it ran.
//!
//! Every `rig up` runs the GUI with the profiler on, and every `rig down`
//! leaves four files behind: the frame log, the editor log, the log of
//! main-thread work outside any frame, and the CPU profile. The numbers a
//! landing note needs are all in them, and until now getting at them meant
//! standing a viewer up over the directory. So `rig down` reads them itself and
//! prints one line: the worst frame gap, how many frames went over budget, and
//! where the main thread actually was.
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
///
/// The user set this to 4 ms, down from 8: the bar is zero frames over it,
/// on every surface, in the profiling profile.
const DRAW_BUDGET_MS: f64 = 4.0;

/// The label a span inside a frame carries. The editor's prepaint records its
/// passes under `prepaint/<pass>` when one of them goes long, and those are
/// part of a draw the frame log has already counted whole — so they are read
/// out of the between-frame work rather than beside it.
const IN_FRAME: &str = "prepaint/";

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
    /// What the main thread did between frames, by owner and costliest
    /// first. A frame number cannot see any of this, and it is what makes
    /// the next frame late.
    pub work: Vec<Work>,
    /// Named spans from *inside* a frame, costliest first — the passes of a
    /// prepaint that went long. They are a share of a draw the frame log has
    /// already counted, so they are kept apart from `work`: adding the two
    /// would count the same milliseconds twice.
    #[serde(default)]
    pub in_frame: Vec<Work>,
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

/// One owner's share of the work between frames. `units` is what the owner
/// counts — agents an event named, rows patched — so `per_unit` is the
/// per-event side of the cost rule for work that has no rows: an owner
/// whose span cost follows the desk rather than what the event named is
/// the failure this exists to show.
#[derive(Serialize, Deserialize)]
pub struct Work {
    pub owner: String,
    pub spans: usize,
    pub total_ms: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub units: f64,
}

#[derive(Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub share: f64,
}

/// Read the three sidecars beside `profile` — the path `rig up` passed to
/// `--cpu-profile`, whose own file is never written.
pub fn summarize(profile: &Path) -> Result<Summary> {
    summarize_with_chains(profile, None)
}

/// The summary, and optionally the whole callchain behind each sample.
///
/// A leaf says what the thread was in; only the chain says what put it
/// there, and the two answers are different questions. `chains` names the
/// leaves to print - the empty string for all of them - and the reader
/// prints them richest first, one line each, deepest frame leftmost.
pub fn summarize_with_chains(profile: &Path, chains: Option<&str>) -> Result<Summary> {
    let frames: FrameLog = read_json(&sidecar(profile, ".frames.json"))?;
    let editor: EditorLog = read_json(&sidecar(profile, ".editor.json")).unwrap_or_default();
    let work: WorkLog = read_json(&sidecar(profile, ".work.json")).unwrap_or_default();
    let cpu = symbols(&cpu_path(profile), chains).unwrap_or_default();

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

    let (mut in_frame, mut work): (Vec<_>, Vec<_>) = work
        .owners
        .into_iter()
        .map(|(owner, log)| Work {
            owner,
            spans: log.count,
            total_ms: log.total_ms,
            p50_ms: log.duration_ms.p50,
            p99_ms: log.duration_ms.p99,
            units: log.work_units.p50,
        })
        .partition(|held| held.owner.starts_with(IN_FRAME));
    work.sort_by(|left, right| right.total_ms.total_cmp(&left.total_ms));
    in_frame.sort_by(|left, right| right.total_ms.total_cmp(&left.total_ms));

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
        work,
        in_frame,
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
        // Costliest owner only: the point of the line is that work between
        // frames exists and how much of it there is, and the sidecar holds
        // the rest for anyone who asks.
        if let Some(work) = self.work.first() {
            line.push_str(&format!(
                "; between frames {} {} spans {:.0} ms total, p50 {:.2} p99 {:.2} ms at {:.0} units",
                work.owner, work.spans, work.total_ms, work.p50_ms, work.p99_ms, work.units
            ));
            // A part of that owner, when it recorded any: `owner/part` is
            // inside the span above, so its total is a share of a number
            // already printed rather than another one beside it. The
            // costliest part is the whole point of asking — a floor with
            // no name is not actionable — and the rest are in the sidecar.
            let inside = format!("{}/", work.owner);
            if let Some(part) = self
                .work
                .iter()
                .find(|held| held.owner.starts_with(&inside))
            {
                line.push_str(&format!(
                    ", largest part {} at {:.0} ms total, p50 {:.2} p99 {:.2} ms",
                    part.owner, part.total_ms, part.p50_ms, part.p99_ms
                ));
            }
        }
        // Inside a frame, and so on the other side of the same accounting:
        // the draw is already in the numbers above, and this says which pass
        // of it spent the milliseconds on the frames that went long. Only
        // those frames record, so the span count is how many went long and
        // the rows are the visible range each of them drew.
        if let Some(pass) = self.in_frame.first() {
            line.push_str(&format!(
                "; longest prepaint pass {} {} spans {:.0} ms total, p50 {:.2} p99 {:.2} ms at {:.0} rows",
                pass.owner, pass.spans, pass.total_ms, pass.p50_ms, pass.p99_ms, pass.units
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
struct WorkLog {
    #[serde(default)]
    owners: std::collections::BTreeMap<String, WorkOwnerLog>,
}

#[derive(Deserialize)]
struct WorkOwnerLog {
    count: usize,
    total_ms: f64,
    duration_ms: Distribution,
    work_units: Distribution,
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
    p50: f64,
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
fn symbols(path: &Path, chains: Option<&str>) -> Result<Cpu> {
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
    // The whole chain per sample, kept only when it is asked for: a run of
    // any length holds tens of thousands of them.
    let mut callchains: Vec<(Vec<u64>, Option<String>)> = Vec::new();
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
                let mut stack = None;
                for (field, value) in event.field_names().zip(event.fields) {
                    match (field, value) {
                        ("callchain", FieldValueRef::PooledStackFrames(held)) => {
                            let frames = event.stack_pool.get(*held);
                            leaf = frames
                                .and_then(|frames| frames.iter().find(|addr| **addr != 0))
                                .copied();
                            if let (Some(frames), Some(_)) = (frames, chains) {
                                stack = Some(frames.to_vec());
                            }
                        }
                        ("thread_name", FieldValueRef::PooledString(held)) => {
                            thread = event.string_pool.get(*held).map(str::to_owned);
                        }
                        _ => {}
                    }
                }
                if let Some(stack) = stack {
                    callchains.push((stack, thread.clone()));
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

    if let Some(wanted) = chains {
        print_chains(&callchains, &names, thread.as_deref(), wanted);
    }

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

/// Every distinct callchain whose leaf matches `wanted`, most samples
/// first, on the thread the summary is about.
///
/// The names come from the trace's own symbol table, which is written
/// against the addresses the process had. Where a mapping failed to
/// symbolize the frame prints as an address, and a chain of addresses is
/// not an answer - see the handbook on what makes rho's own frames name
/// themselves.
fn print_chains(
    callchains: &[(Vec<u64>, Option<String>)],
    names: &HashMap<u64, (u64, String)>,
    thread: Option<&str>,
    wanted: &str,
) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (frames, held) in callchains {
        if thread.is_some() && held.as_deref() != thread {
            continue;
        }
        let named = frames
            .iter()
            .filter(|addr| **addr != 0)
            .map(|addr| {
                names
                    .get(addr)
                    .map_or_else(|| format!("{addr:#x}"), |(_, name)| name.clone())
            })
            .collect::<Vec<_>>();
        if !wanted.is_empty() && !named.first().is_some_and(|leaf| leaf.contains(wanted)) {
            continue;
        }
        *counts.entry(named.join(" < ")).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for (chain, count) in ranked {
        println!("{count} {chain}");
    }
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
            work: Vec::new(),
            in_frame: Vec::new(),
            events: 0,
            top: Vec::new(),
            samples: 0,
            thread: None,
            line: String::new(),
        };
        assert_eq!(
            bare.render(),
            "81 frames, draw p99 3.6 ms, 0 over 4 ms; worst gap 11 ms, p99 11 ms"
        );

        let full = Summary {
            slowest_stage: Some(Stage {
                name: "buffer_edit".to_owned(),
                p99_ms: 0.0366,
                rows_p99: 2.0,
            }),
            events: 11471,
            work: vec![Work {
                owner: "desk_sync".to_owned(),
                spans: 412,
                total_ms: 913.4,
                p50_ms: 1.84,
                p99_ms: 6.02,
                units: 1.0,
            }],
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
            "81 frames, draw p99 3.6 ms, 0 over 4 ms; worst gap 11 ms, p99 11 ms; \
             11471 events, slowest stage buffer_edit p99 0.04 ms at 2 rows; \
             between frames desk_sync 412 spans 913 ms total, p50 1.84 p99 6.02 ms at 1 units; \
             90 samples on rho-gui: memcpy 13%"
        );
    }

    /// A part of the costliest owner is named beside it, and a span that
    /// merely shares its first letters is not mistaken for one.
    ///
    /// The floor this was written for is a `desk_sync` that costs
    /// milliseconds between frames without any frame being late, so the
    /// question the line has to answer is which pass inside it spends
    /// them. A part is `owner/pass`; `desk_sync_extra` is a different
    /// owner and stays out of that clause.
    #[test]
    fn the_line_names_the_costliest_part_of_the_costliest_owner() {
        let bare = Summary {
            profile: "gui.bin".to_owned(),
            frames: 81,
            draw_p99_ms: 3.574,
            draw_max_ms: 3.574,
            over_budget: 0,
            worst_gap_ms: 10.8,
            gap_p99_ms: 10.8,
            slowest_stage: None,
            work: Vec::new(),
            in_frame: Vec::new(),
            events: 0,
            top: Vec::new(),
            samples: 0,
            thread: None,
            line: String::new(),
        };
        let part = |owner: &str, total_ms: f64| Work {
            owner: owner.to_owned(),
            spans: 412,
            total_ms,
            p50_ms: 1.84,
            p99_ms: 6.02,
            units: 1.0,
        };
        // As `summarize` sorts them: costliest first.
        let parted = Summary {
            work: vec![
                part("desk_sync", 913.4),
                part("desk_sync_extra", 700.0),
                part("desk_sync/refresh_sources", 512.5),
                part("desk_sync/sync_note_views", 90.0),
            ],
            ..bare
        };
        let line = parted.render();
        assert!(
            line.ends_with(
                "between frames desk_sync 412 spans 913 ms total, p50 1.84 p99 6.02 ms at \
                 1 units, largest part desk_sync/refresh_sources at 512 ms total, p50 1.84 \
                 p99 6.02 ms"
            ),
            "the costliest part is not named as the costliest owner's: {line}"
        );
        assert_eq!(
            line.matches("desk_sync_extra").count(),
            0,
            "an owner whose name begins with the costliest owner's was read as a part of \
             it: {line}"
        );

        // The same line without any part, so the clause is the parts'
        // doing and not the renderer's.
        let unparted = Summary {
            work: vec![part("desk_sync", 913.4), part("desk_sync_extra", 700.0)],
            ..parted
        };
        assert!(
            unparted.render().ends_with("at 1 units"),
            "a run whose owners recorded no parts got a part clause anyway: {}",
            unparted.render()
        );
    }

    /// A prepaint pass is inside a frame the frame log already timed, so it
    /// belongs on its own clause. Read as between-frame work it would be
    /// both the costliest owner — it only records when a frame went long,
    /// which is when it is large — and a second count of the same draw.
    #[test]
    fn a_prepaint_pass_is_not_between_frame_work() {
        let log = |owner: &str, total_ms: f64| {
            (
                owner.to_owned(),
                WorkOwnerLog {
                    count: 9,
                    total_ms,
                    duration_ms: Distribution {
                        p50: 4.1,
                        p99: 4.4,
                        max: 4.4,
                    },
                    work_units: Distribution {
                        p50: 50.0,
                        p99: 50.0,
                        max: 50.0,
                    },
                },
            )
        };
        let work = WorkLog {
            owners: [log("prepaint/lines", 37.0), log("desk_sync", 12.0)]
                .into_iter()
                .collect(),
        };
        let (in_frame, between): (Vec<_>, Vec<_>) = work
            .owners
            .into_iter()
            .map(|(owner, held)| Work {
                owner,
                spans: held.count,
                total_ms: held.total_ms,
                p50_ms: held.duration_ms.p50,
                p99_ms: held.duration_ms.p99,
                units: held.work_units.p50,
            })
            .partition(|held| held.owner.starts_with(IN_FRAME));
        assert_eq!(
            between
                .iter()
                .map(|held| held.owner.as_str())
                .collect::<Vec<_>>(),
            ["desk_sync"],
            "a span from inside a frame was counted as work between frames"
        );
        assert_eq!(
            in_frame
                .iter()
                .map(|held| held.owner.as_str())
                .collect::<Vec<_>>(),
            ["prepaint/lines"]
        );
    }

    /// The clause the answer is read off, on a summary holding both kinds:
    /// the between-frames number keeps its own words and the pass gets its
    /// own, with rows rather than units, because a pass counts the visible
    /// range and an owner counts whatever it named.
    #[test]
    fn the_line_says_the_longest_prepaint_pass_beside_the_work_between_frames() {
        let held = |owner: &str, spans: usize, total_ms: f64, units: f64| Work {
            owner: owner.to_owned(),
            spans,
            total_ms,
            p50_ms: 4.12,
            p99_ms: 4.41,
            units,
        };
        let summary = Summary {
            profile: "gui.bin".to_owned(),
            frames: 1588,
            draw_p99_ms: 2.92,
            draw_max_ms: 7.85,
            over_budget: 9,
            worst_gap_ms: 32.0,
            gap_p99_ms: 5.0,
            slowest_stage: None,
            work: vec![held("desk_sync", 48, 323.9, 1.0)],
            in_frame: vec![held("prepaint/lines", 9, 37.1, 50.0)],
            events: 0,
            top: Vec::new(),
            samples: 0,
            thread: None,
            line: String::new(),
        };
        let line = summary.render();
        assert!(
            line.ends_with(
                "; longest prepaint pass prepaint/lines 9 spans 37 ms total, p50 4.12                  p99 4.41 ms at 50 rows"
            ),
            "the pass clause is not on the line as written: {line}"
        );
        assert!(
            line.contains("between frames desk_sync 48 spans 324 ms total"),
            "the between-frames clause changed when a pass was there too: {line}"
        );
    }
}
