//! Reading a telemetry report the user sent.
//!
//! `rho wayland` and `rho-qa profile` read what a rig produced, in the shape
//! a rig writes it. A report from the user's own machine is a different
//! thing: one JSON file with the frames, the editor stages and the CPU
//! profile inside it, and until now nothing could open it. That meant a
//! report was read by eye, the CPU profile in it — the only record of what
//! was actually running — was never read at all, and every report cost the
//! same hour of hand-work as the one before it.
//!
//! Three things this says that reading by eye does not.
//!
//! The frames and the stages do not cover the same time. Frames are kept for
//! the whole session; the editor stages are a ring of the last few thousand,
//! which on a busy transcript is the last few seconds. Adding up stage time
//! and comparing it with a session's frame time compares minutes with
//! seconds, and makes a stage that is eating the main thread look small.
//! Everything here is reported against the window it was actually measured
//! in.
//!
//! A stage is not the same as a frame's cost. A stage that runs between
//! frames is main-thread time that delays the next frame without appearing
//! in any frame's `draw_ns`. So stages are attributed: inside a draw, or
//! between draws, and the two are never added together.
//!
//! And the report is read per surface, because "the GUI is slow" is almost
//! never true of the whole GUI. It is one surface, and which one is the
//! first thing worth knowing.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Deserialize;

/// The per-frame draw budget. The user set it to 4 ms, down from 8: the bar
/// is zero frames over it, on every surface.
const BUDGET_NS: u64 = 4_000_000;

#[derive(Deserialize)]
struct Report {
    #[serde(default)]
    captured_unix_ms: u64,
    #[serde(default)]
    build: Build,
    #[serde(default)]
    frames: Vec<Frame>,
    #[serde(default)]
    editor: Vec<Stage>,
    #[serde(default)]
    cpu_profile: Option<CpuProfile>,
}

#[derive(Default, Deserialize)]
struct Build {
    #[serde(default)]
    profile: String,
    #[serde(default)]
    target: String,
}

#[derive(Deserialize)]
struct Frame {
    start_ns: u64,
    draw_ns: u64,
    prepaint_ns: u64,
    paint_ns: u64,
    #[serde(default)]
    invalidations: u64,
    #[serde(default)]
    focused_surface: String,
}

#[derive(Deserialize)]
struct Stage {
    stage: String,
    start_ns: u64,
    duration_ns: u64,
    #[serde(default)]
    tid: u64,
    #[serde(default)]
    input_rows: u64,
    #[serde(default)]
    new_rows: u64,
}

#[derive(Deserialize)]
struct CpuProfile {
    #[serde(default)]
    segments: Vec<String>,
}

/// What the report says, in the order a reader needs it.
pub fn summarize(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let report: Report =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    let mut out = String::new();
    let span = span_of(report.frames.iter().map(|frame| frame.start_ns));
    out.push_str(&format!(
        "{} {} · captured {} · {} frames over {:.0} s\n",
        report.build.profile,
        report.build.target,
        report.captured_unix_ms,
        report.frames.len(),
        span.map_or(0.0, |(lo, hi)| (hi - lo) as f64 / 1e9),
    ));

    out.push_str("\nby surface, because slow is almost never the whole GUI:\n");
    let mut by_surface: HashMap<&str, Vec<&Frame>> = HashMap::new();
    for frame in &report.frames {
        by_surface
            .entry(frame.focused_surface.as_str())
            .or_default()
            .push(frame);
    }
    let mut surfaces: Vec<(&str, Vec<&Frame>)> = by_surface.into_iter().collect();
    surfaces.sort_by_key(|(_, frames)| std::cmp::Reverse(frames.len()));
    for (surface, frames) in &surfaces {
        let draw: Vec<u64> = frames.iter().map(|frame| frame.draw_ns).collect();
        let over = draw.iter().filter(|held| **held > BUDGET_NS).count();
        // The budget was 8 ms until the user set it to 4. The old count is
        // printed beside the new one for one release, so a report read
        // today can still be compared with one read last week.
        let over_old = draw.iter().filter(|held| **held > 8_000_000).count();
        out.push_str(&format!(
            "  {surface:<20} {:>5} frames  {:>5} over 4 ms ({:.0}%)  [{:>5} over 8 ms]  draw p50 {:.1} p99 {:.1} max {:.1} ms  \
             prepaint p50 {:.1} p99 {:.1}  paint p50 {:.1} p99 {:.1}\n",
            frames.len(),
            over,
            100.0 * over as f64 / frames.len() as f64,
            over_old,
            ms(&draw, 0.50),
            ms(&draw, 0.99),
            draw.iter().copied().max().unwrap_or(0) as f64 / 1e6,
            ms(&collect(frames, |frame| frame.prepaint_ns), 0.50),
            ms(&collect(frames, |frame| frame.prepaint_ns), 0.99),
            ms(&collect(frames, |frame| frame.paint_ns), 0.50),
            ms(&collect(frames, |frame| frame.paint_ns), 0.99),
        ));
    }

    // A frame's own accounting: whatever `draw` is that prepaint and paint
    // are not, is work this report cannot name. If that residue is small,
    // the cost is in the two stages that are named and the question is what
    // they do; if it is large, the instrument is pointing at the wrong place.
    if let Some((surface, frames)) = surfaces.first() {
        let residue: Vec<u64> = frames
            .iter()
            .map(|frame| {
                frame
                    .draw_ns
                    .saturating_sub(frame.prepaint_ns + frame.paint_ns)
            })
            .collect();
        out.push_str(&format!(
            "\n  on {surface}, draw minus prepaint minus paint is p50 {:.2} p99 {:.2} ms: \
             the frame is accounted for by its two stages\n",
            ms(&residue, 0.50),
            ms(&residue, 0.99),
        ));
        let slow: Vec<&&Frame> = frames
            .iter()
            .filter(|frame| frame.draw_ns > BUDGET_NS)
            .collect();
        let fast: Vec<&&Frame> = frames
            .iter()
            .filter(|frame| frame.draw_ns <= BUDGET_NS)
            .collect();
        if !slow.is_empty() && !fast.is_empty() {
            let mean = |set: &[&&Frame]| {
                set.iter().map(|frame| frame.invalidations).sum::<u64>() as f64 / set.len() as f64
            };
            out.push_str(&format!(
                "  invalidations: {:.2} per slow frame against {:.2} per fast one — \
                 {}\n",
                mean(&slow),
                mean(&fast),
                if mean(&slow) > mean(&fast) {
                    "more invalidation on the slow frames"
                } else {
                    "slow frames are not the ones with more invalidation, so it is not how much was dirtied"
                },
            ));
        }
        out.push_str(&format!("\n{}", continuity(frames)));
    }

    out.push_str(&stages(&report));

    if let Some(cpu) = &report.cpu_profile {
        out.push_str(&self::cpu(cpu)?);
    }
    Ok(out)
}

/// Whether the slow frames arrive in bursts or all the way through. A burst
/// is an event with a cause to find; all the way through is the surface's
/// steady state, and a different kind of problem.
fn continuity(frames: &[&Frame]) -> String {
    let mut out = String::from("  slow frames over the run, in tenths:\n    ");
    let (Some(first), Some(last)) = (frames.first(), frames.last()) else {
        return String::new();
    };
    let (lo, hi) = (first.start_ns, last.start_ns.max(first.start_ns + 1));
    let mut buckets = [(0usize, 0usize); 10];
    for frame in frames {
        let bucket = ((frame.start_ns - lo) as u128 * 10 / (hi - lo) as u128).min(9) as usize;
        buckets[bucket].0 += 1;
        if frame.draw_ns > BUDGET_NS {
            buckets[bucket].1 += 1;
        }
    }
    for (total, slow) in buckets {
        out.push_str(&format!(
            "{:>4.0}% ",
            if total == 0 {
                0.0
            } else {
                100.0 * slow as f64 / total as f64
            }
        ));
    }
    let quiet = buckets
        .iter()
        .filter(|(total, slow)| *total > 0 && *slow * 5 < *total)
        .count();
    out.push_str(&format!(
        "\n    {}\n",
        if quiet <= 2 {
            "slow in every tenth of the run: this is the surface's steady state, not an event"
        } else {
            "slow in some tenths and not others: bursts, so look for what happens in them"
        }
    ));
    let mut runs = Vec::new();
    let mut run = 0usize;
    for frame in frames {
        if frame.draw_ns > BUDGET_NS {
            run += 1;
        } else if run > 0 {
            runs.push(run);
            run = 0;
        }
    }
    if run > 0 {
        runs.push(run);
    }
    if !runs.is_empty() {
        runs.sort_unstable();
        out.push_str(&format!(
            "    {} runs of consecutive slow frames, median {}, longest {}\n",
            runs.len(),
            runs[runs.len() / 2],
            runs[runs.len() - 1],
        ));
    }
    let back_to_back = frames
        .windows(2)
        .filter(|pair| pair[1].start_ns.saturating_sub(pair[0].start_ns) < 20_000_000)
        .count();
    out.push_str(&format!(
        "    {back_to_back} of {} frames follow the one before within 20 ms, so the surface \
         spends {:.0}% of its drawing back to back\n",
        frames.len(),
        100.0 * back_to_back as f64 / frames.len().max(1) as f64,
    ));
    out
}

/// The editor stages, against the window they were actually recorded in and
/// split by whether they ran inside a frame's draw or between frames.
fn stages(report: &Report) -> String {
    let Some((lo, hi)) = span_of(report.editor.iter().map(|stage| stage.start_ns)) else {
        return String::from("\nno editor stages in this report\n");
    };
    let window = (hi - lo) as f64 / 1e9;
    let run = span_of(report.frames.iter().map(|frame| frame.start_ns))
        .map_or(0.0, |(lo, hi)| (hi - lo) as f64 / 1e9);
    let in_window: Vec<&Frame> = report
        .frames
        .iter()
        .filter(|frame| (lo..=hi).contains(&frame.start_ns))
        .collect();
    let drawn: u64 = in_window.iter().map(|frame| frame.draw_ns).sum();
    let total: u64 = report.editor.iter().map(|stage| stage.duration_ns).sum();
    let mut out = format!(
        "\neditor stages, over the {window:.2} s the stage ring covers — not the {run:.0} s of \
         frames. {} records; the ring holds the last few thousand, so on a busy surface it is \
         seconds, and adding these to a session's frame time compares seconds with minutes.\n",
        report.editor.len(),
    );
    // Inside a draw or between draws: a stage between draws is main-thread
    // time that never appears in any frame's number and delays the next one.
    let mut inside = 0u64;
    for stage in &report.editor {
        if in_window
            .iter()
            .any(|frame| (frame.start_ns..frame.start_ns + frame.draw_ns).contains(&stage.start_ns))
        {
            inside += stage.duration_ns;
        }
    }
    out.push_str(&format!(
        "  {:.0} ms of stage time in that window: {:.1}% of the wall clock, against {:.0} ms drawn \
         in {} frames. {:.0} ms of it ({:.1}%) falls inside a frame's draw; the rest runs between \
         frames, where it costs the main thread without appearing in any frame's number.\n",
        total as f64 / 1e6,
        100.0 * total as f64 / (window * 1e9),
        drawn as f64 / 1e6,
        in_window.len(),
        inside as f64 / 1e6,
        100.0 * inside as f64 / total.max(1) as f64,
    ));
    let threads: std::collections::BTreeSet<u64> =
        report.editor.iter().map(|stage| stage.tid).collect();
    out.push_str(&format!(
        "  on {} thread{}: {}\n",
        threads.len(),
        if threads.len() == 1 { "" } else { "s" },
        threads
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    ));
    let mut by_stage: HashMap<&str, Vec<&Stage>> = HashMap::new();
    for stage in &report.editor {
        by_stage
            .entry(stage.stage.as_str())
            .or_default()
            .push(stage);
    }
    let mut ranked: Vec<(&str, Vec<&Stage>)> = by_stage.into_iter().collect();
    ranked.sort_by_key(|(_, held)| {
        std::cmp::Reverse(held.iter().map(|stage| stage.duration_ns).sum::<u64>())
    });
    for (name, held) in ranked {
        let durations: Vec<u64> = held.iter().map(|stage| stage.duration_ns).collect();
        let worst = held.iter().max_by_key(|stage| stage.duration_ns);
        out.push_str(&format!(
            "  {name:<20} {:>5} × p50 {:>7.1} µs p99 {:>8.1} µs, {:>7.1} ms total ({:>4.1}% of the window), \
             worst {:.2} ms at {} rows\n",
            held.len(),
            ms(&durations, 0.50) * 1000.0,
            ms(&durations, 0.99) * 1000.0,
            durations.iter().sum::<u64>() as f64 / 1e6,
            100.0 * durations.iter().sum::<u64>() as f64 / (window * 1e9),
            worst.map_or(0.0, |stage| stage.duration_ns as f64 / 1e6),
            worst.map_or(0, |stage| stage.input_rows.max(stage.new_rows)),
        ));
    }
    out
}

/// Where the samples landed, on the thread the cost rule is about. The
/// segments are the same trace the rig writes, gzipped and base64'd into the
/// report rather than left beside it.
fn cpu(profile: &CpuProfile) -> Result<String> {
    use base64::Engine as _;
    // Each segment is a whole trace with its own header and its own symbol
    // table, not a slice of one stream — and the last is the tail the
    // profiler had not sealed when the report was taken, so it arrives
    // uncompressed. Decoded one at a time and merged, because a symbol table
    // is only valid for the segment it came in.
    let mut names: HashMap<u64, (u64, String)> = HashMap::new();
    let mut stacks: Vec<(Vec<u64>, Option<String>)> = Vec::new();
    let mut unreadable = 0usize;
    for segment in &profile.segments {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(segment)
            .context("decode a CPU profile segment")?;
        let bytes = if raw.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw.as_slice())
                .read_to_end(&mut out)
                .context("decompress a CPU profile segment")?;
            out
        } else {
            raw
        };
        let Some(mut decoder) = dial9_trace_format::decoder::Decoder::new(&bytes) else {
            unreadable += 1;
            continue;
        };
        let decoded = decoder.for_each_event(|event| {
            use dial9_trace_format::types::FieldValueRef;
            match event.name {
                "SymbolTableEntry" => {
                    let (mut addr, mut depth, mut name) = (None, 0, None);
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
                    let (mut stack, mut thread) = (Vec::new(), None);
                    for (field, value) in event.field_names().zip(event.fields) {
                        match (field, value) {
                            ("callchain", FieldValueRef::PooledStackFrames(held)) => {
                                if let Some(frames) = event.stack_pool.get(*held) {
                                    stack =
                                        frames.iter().copied().filter(|addr| *addr != 0).collect();
                                }
                            }
                            ("thread_name", FieldValueRef::PooledString(held)) => {
                                thread = event.string_pool.get(*held).map(str::to_owned);
                            }
                            _ => {}
                        }
                    }
                    if !stack.is_empty() {
                        stacks.push((stack, thread));
                    }
                }
                _ => {}
            }
        });
        if decoded.is_err() {
            unreadable += 1;
        }
    }
    if stacks.is_empty() {
        return Ok(format!(
            "\nno CPU samples in this report ({} of {} segments unreadable)\n",
            unreadable,
            profile.segments.len()
        ));
    }
    let leaves: Vec<(u64, Option<String>)> = stacks
        .iter()
        .filter_map(|(stack, thread)| Some((*stack.first()?, thread.clone())))
        .collect();
    let mut per_thread: HashMap<&str, usize> = HashMap::new();
    for (_, thread) in &leaves {
        if let Some(thread) = thread {
            *per_thread.entry(thread.as_str()).or_default() += 1;
        }
    }
    let thread = ["rho-gui", "main"]
        .into_iter()
        .find(|name| per_thread.contains_key(name))
        .map(str::to_owned);
    let mut out = format!(
        "\nCPU profile: {} samples across {} threads, from {} segments{}{}\n",
        leaves.len(),
        per_thread.len(),
        profile.segments.len(),
        if unreadable > 0 {
            format!(" ({unreadable} unreadable)")
        } else {
            String::new()
        },
        thread
            .as_deref()
            .map_or(String::new(), |name| format!(", reported for `{name}`")),
    );
    let mine: Vec<&(Vec<u64>, Option<String>)> = stacks
        .iter()
        .filter(|(_, held)| thread.is_none() || held.as_deref() == thread.as_deref())
        .collect();
    if mine.is_empty() {
        return Ok(out);
    }
    // The leaf says what was running; the whole stack says what asked for
    // it, and on a question like "what does prepaint do" the second is the
    // answer. Both, so a name that is everywhere as a leaf (a memcpy) can be
    // told apart from a name that is everywhere as a caller.
    let mut leaf_counts: HashMap<&str, usize> = HashMap::new();
    let mut stack_counts: HashMap<&str, usize> = HashMap::new();
    for (stack, _) in &mine {
        if let Some(leaf) = stack.first() {
            *leaf_counts
                .entry(names.get(leaf).map_or("unknown", |(_, name)| name.as_str()))
                .or_default() += 1;
        }
        let mut seen = std::collections::HashSet::new();
        for addr in stack {
            let name = names.get(addr).map_or("unknown", |(_, name)| name.as_str());
            if seen.insert(name) {
                *stack_counts.entry(name).or_default() += 1;
            }
        }
    }
    let total = mine.len() as f64;
    let top_leaf: Option<String> = leaf_counts
        .iter()
        .max_by_key(|(name, count)| (**count, std::cmp::Reverse(**name)))
        .map(|(name, _)| (*name).to_owned());
    let mut ranked: Vec<(&str, usize)> = leaf_counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    out.push_str("  running (the leaf frame — what the thread was actually in):\n");
    for (name, count) in ranked.iter().take(10) {
        out.push_str(&format!(
            "    {:>5.1}%  {}\n",
            100.0 * *count as f64 / total,
            short(name)
        ));
    }
    // Everything above the interesting part is in every stack: `main`,
    // `lang_start`, the dispatcher. Naming them is noise, so anything on
    // nearly every stack is dropped and what is left is what varies.
    let mut ranked: Vec<(&str, usize)> = stack_counts
        .into_iter()
        .filter(|(_, count)| (*count as f64) < 0.95 * total)
        .collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    out.push_str("  on the stack, ignoring the frames that are on every stack:\n");
    for (name, count) in ranked.iter().take(10) {
        out.push_str(&format!(
            "    {:>5.1}%  {}\n",
            100.0 * *count as f64 / total,
            short(name)
        ));
    }
    // Who calls the hot leaf. A leaf name says what is expensive; its
    // callers say which feature is paying for it, which is the thing a fix
    // has to change.
    if let Some(hot) = top_leaf.as_deref() {
        let mut callers: HashMap<&str, usize> = HashMap::new();
        let mut seen = 0usize;
        for (stack, _) in &mine {
            let Some(at) = stack
                .iter()
                .position(|addr| names.get(addr).map(|(_, name)| name.as_str()) == Some(hot))
            else {
                continue;
            };
            seen += 1;
            for addr in stack.iter().skip(at + 1).take(16) {
                let name = names.get(addr).map_or("unknown", |(_, name)| name.as_str());
                *callers.entry(name).or_default() += 1;
            }
        }
        let mut ranked: Vec<(&str, usize)> = callers.into_iter().collect();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
        out.push_str(&format!(
            "  callers of `{}`, within sixteen frames of it ({seen} samples) — the leaf \
says what is expensive, this says which feature is paying for it:\n",
            short(hot)
        ));
        for (name, count) in ranked.iter().take(16) {
            out.push_str(&format!(
                "    {:>5.1}%  {}\n",
                100.0 * *count as f64 / seen.max(1) as f64,
                short(name)
            ));
        }
    }
    Ok(out)
}

/// A symbol without its generic soup, which is never what is being asked.
///
/// Dropping everything from the first `<` is wrong for the names that matter
/// most here: `<editor::Editor as gpui::Render>::render` begins with one, and
/// cutting there leaves nothing at all. Balanced groups are removed instead,
/// so the qualified name survives and only the type arguments go.
fn short(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    for character in name.chars() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(character),
            _ => {}
        }
    }
    let out = out.replace("::{{closure}}", "");
    let trimmed = out.trim_matches(|c| c == ':' || c == ' ');
    let parts: Vec<&str> = trimmed
        .split("::")
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() <= 3 {
        return trimmed.to_owned();
    }
    parts[parts.len() - 3..].join("::")
}

fn collect(frames: &[&Frame], of: impl Fn(&Frame) -> u64) -> Vec<u64> {
    frames.iter().map(|frame| of(frame)).collect()
}

fn span_of(values: impl Iterator<Item = u64>) -> Option<(u64, u64)> {
    let mut values = values.peekable();
    values.peek()?;
    let (mut lo, mut hi) = (u64::MAX, 0);
    for value in values {
        lo = lo.min(value);
        hi = hi.max(value);
    }
    Some((lo, hi))
}

fn ms(values: &[u64], quantile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[((sorted.len() - 1) as f64 * quantile) as usize] as f64 / 1e6
}
