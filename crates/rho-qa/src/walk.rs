use std::time::Instant;

use anyhow::Result;
use clap::Args;
use rho_gui::walk::{WalkConfig, WalkEvent, WalkHarness, WalkMode};

/// The wrap map's two whole-document drives, on a transcript already worth
/// scrolling: a width change and a jump to the top, with keystrokes beside
/// them so the cost of one edit on a long document is on the same line as
/// the cost of a rewrap. A keystroke never follows the jump: an insert
/// scrolls the cursor back into view, and the repaint that follows is the
/// scroll's, not the keystroke's.
const LARGE_TRANSCRIPT_DRIVE: &[WalkEvent] = &[
    WalkEvent::ComposerKey { character: 'a' },
    WalkEvent::Resize {
        width: 700,
        height: 600,
    },
    WalkEvent::ComposerKey { character: 'b' },
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::Resize {
        width: 1100,
        height: 900,
    },
    WalkEvent::Idle,
];

/// Paging history in, one chunk at a time.
///
/// Each pair is a jump to the top and the idle that composes the next
/// forty-row chunk of history, so the run climbs the document a chunk at a
/// time while the drawn screen stays the same size. What it pins is that
/// composing a chunk costs the chunk and not the document: over eight pairs
/// the composed window more than doubles, from 299 rows to 635, and every
/// stage's walk holds flat - the multibuffer at 152 to 154 items for a
/// 48-row chunk, the wrap map at 91. A stage that starts growing with
/// `total_rows` here is a per-event O(document) on the reader's own path.
const HISTORY_PAGING_DRIVE: &[WalkEvent] = &[
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
];

/// A transcript whose settled turns are elided, typed into and paged
/// through.
///
/// The keystroke first, on the composed screen, then five pairs of a jump
/// to the top and the idle that composes the next chunk of history - the
/// same climb the paging run makes, over a document that hides most of
/// itself. What it is here to read is the `fold:` count, which is elision's
/// line in a step's walk: the paging run's document is concealed markup
/// only, and markup's number is not elision's.
///
/// A streamed chunk stands at each end of that climb, and it is the same
/// event both times, so a reader of the gate can compare a step against
/// itself rather than against a different kind of step. Everything the
/// climb changes is between them: the first runs on the composed screen and
/// the second on a document twice as long. A keystroke cannot take that
/// place - an insert scrolls the cursor into view, and the oracle is right
/// to refuse a second one after a jump - but a chunk arriving in the turn
/// does not move the viewport, and it is the event a reader watching a
/// turn actually gets.
const ELIDED_HISTORY_DRIVE: &[WalkEvent] = &[
    WalkEvent::ComposerKey { character: 'a' },
    WalkEvent::Idle,
    WalkEvent::AgentChunk { bytes: 64 },
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::ScrollToTop,
    WalkEvent::Idle,
    WalkEvent::AgentChunk { bytes: 64 },
    WalkEvent::Idle,
];

/// Settled turns seeded before the drive runs.
///
/// Forty already saturates what the transcript composes: seeding 0, 40 and
/// 400 turns gives 38, 273 and 273 rows, because the transcript hands the
/// editor a screen and pages history in forty-row chunks. Four hundred
/// costs nothing in the composed window and buys the jump to the top a real
/// chunk to page in - `gg` touches 49 rows at 400 where it touched 6 at 40.
const LARGE_TRANSCRIPT_TURNS: usize = 400;

/// One step's walk written as `stage:count`, or `-` where it walked nothing.
fn stage_walks(walks: &[(&'static str, u64)]) -> String {
    if walks.is_empty() {
        return "-".to_owned();
    }
    walks
        .iter()
        .map(|(stage, walked)| format!("{stage}:{walked}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// What one frame may cost the reader.
const FRAME_BOUND_US: u64 = 4_000;

#[derive(Args)]
pub struct WalkArgs {
    /// Run the fixed landing corpus and check byte-identical replay.
    #[arg(long)]
    gate: bool,
    /// Seed for a single exploratory run.
    #[arg(long, default_value_t = 0)]
    seed: u64,
    /// Generated events in a single exploratory run.
    #[arg(long, default_value_t = 64)]
    steps: usize,
    /// Report draws above the secondary 4 ms wall-clock threshold.
    #[arg(long)]
    profiling: bool,
}

pub fn run(args: WalkArgs) -> Result<()> {
    let started = Instant::now();
    let seeds: &[u64] = if args.gate {
        &[0, 1, 7, 42, 0x5eed_5eed]
    } else {
        std::slice::from_ref(&args.seed)
    };
    let steps = if args.gate { 64 } else { args.steps };
    let mode = if args.gate || args.profiling {
        WalkMode::Profiling
    } else {
        WalkMode::Debug
    };

    let mut frames = 0;
    let mut distinct = 0;
    let mut max_changed = 0;
    let mut max_touched_rows = 0;
    let mut max_walked_items = 0;
    let mut max_drawn_rows = 0;
    let mut max_draw = 0;
    let mut max_best = 0;
    // Every line is labelled by run rather than by seed: the drive is a
    // script and not a generated sequence, and it carries seed zero for its
    // dispatcher, so a seed would name two different runs the same.
    let mut runs = seeds
        .iter()
        .map(|seed| {
            (
                format!("seed-{seed}"),
                WalkConfig {
                    seed: *seed,
                    steps,
                    mode,
                    prefill_turns: 0,
                    prefill_tools: false,
                    script: None,
                },
            )
        })
        .collect::<Vec<_>>();
    if args.gate {
        runs.push((
            "drive".to_owned(),
            WalkConfig {
                seed: 0,
                steps: LARGE_TRANSCRIPT_DRIVE.len(),
                mode,
                prefill_turns: LARGE_TRANSCRIPT_TURNS,
                prefill_tools: false,
                script: Some(LARGE_TRANSCRIPT_DRIVE),
            },
        ));
        runs.push((
            "paging".to_owned(),
            WalkConfig {
                seed: 0,
                steps: HISTORY_PAGING_DRIVE.len(),
                mode,
                prefill_turns: LARGE_TRANSCRIPT_TURNS,
                prefill_tools: false,
                script: Some(HISTORY_PAGING_DRIVE),
            },
        ));
        runs.push((
            "elided".to_owned(),
            WalkConfig {
                seed: 0,
                steps: ELIDED_HISTORY_DRIVE.len(),
                mode,
                prefill_turns: LARGE_TRANSCRIPT_TURNS,
                prefill_tools: true,
                script: Some(ELIDED_HISTORY_DRIVE),
            },
        ));
    }
    let total_events = runs.iter().map(|(_, config)| config.steps).sum::<usize>();
    for (run, config) in &runs {
        let config = *config;
        let report = WalkHarness::new(config).run().map_err(|failure| {
            let script =
                serde_json::to_string_pretty(&failure.events).unwrap_or_else(|_| "[]".to_owned());
            let details = failure.scene_details.join("\n");
            let event = failure
                .event
                .as_ref()
                .and_then(|event| serde_json::to_string(event).ok())
                .unwrap_or_else(|| "null".to_owned());
            anyhow::anyhow!(
                "oracle={} run={run}\nstep={} event={event}\ncold_draw_us={} warm_draw_us={} event_draw_us={} touched_rows={} walked_items={}\nshrunk sequence:\n{script}\nsub-scene changes (capped at 32):\n{details}",
                failure.oracle,
                failure.step,
                failure.cold_draw_micros,
                failure.warm_draw_micros,
                failure.draw_micros,
                failure.touched_rows,
                failure.walked_items,
            )
        })?;
        frames += report.frames;
        distinct += report.distinct_scenes;
        max_changed = max_changed.max(report.max_changed_primitives);
        max_touched_rows = max_touched_rows.max(report.max_touched_rows);
        max_walked_items = max_walked_items.max(report.max_walked_items);
        max_drawn_rows = max_drawn_rows.max(report.max_drawn_rows);
        max_draw = max_draw.max(report.max_draw_micros);
        max_best = max_best.max(
            report
                .draw_samples
                .iter()
                .copied()
                .min()
                .unwrap_or(report.cold_draw_micros),
        );
        if !report.wall_clock_findings.is_empty() {
            let mut baseline = report.baseline_owners.iter().collect::<Vec<_>>();
            baseline.sort_by_key(|owner| std::cmp::Reverse(owner.paint_nanos));
            for owner in baseline.into_iter().take(12) {
                println!(
                    "WALL_CLOCK_BASELINE_OWNER run={run} owner={} paint_ns={} primitives={} bounds={:?}",
                    owner.owner, owner.paint_nanos, owner.primitives, owner.bounds
                );
            }
        }
        for finding in &report.wall_clock_findings {
            let sequence =
                serde_json::to_string(&finding.sequence).unwrap_or_else(|_| "[]".to_owned());
            println!(
                "WALL_CLOCK_FINDING run={run} step={} draw_us={} touched_rows={} walked_items={} sequence={sequence}",
                finding.step, finding.draw_micros, finding.touched_rows, finding.walked_items
            );
            let mut owners = report.step_owners[finding.step].iter().collect::<Vec<_>>();
            owners.sort_by_key(|owner| std::cmp::Reverse(owner.paint_nanos));
            for owner in owners.into_iter().take(12) {
                println!(
                    "WALL_CLOCK_OWNER run={run} step={} owner={} paint_ns={} primitives={} changed={} bounds={:?}",
                    finding.step,
                    owner.owner,
                    owner.paint_nanos,
                    owner.primitives,
                    owner.changed_primitives,
                    owner.bounds,
                );
            }
        }
        let best = report
            .draw_samples
            .iter()
            .copied()
            .min()
            .unwrap_or(report.cold_draw_micros);
        println!(
            "run={run} unrecorded draws_us={} best_draw_us={best}",
            report
                .draw_samples
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
        for (step, ((((draw, touched_rows), walked_items), drawn_rows), total_rows)) in report
            .step_draw_micros
            .iter()
            .zip(&report.step_touched_rows)
            .zip(&report.step_walked_items)
            .zip(&report.step_drawn_rows)
            .zip(&report.step_total_rows)
            .enumerate()
        {
            let owners = &report.step_owners[step];
            let paint_us = owners.iter().map(|owner| owner.paint_nanos).sum::<u64>() / 1_000;
            let primitives = owners.iter().map(|owner| owner.primitives).sum::<usize>();
            println!(
                "run={run} step={step} draw_us={draw} paint_us={paint_us} owners={} primitives={primitives} touched_rows={touched_rows} walked_items={walked_items} walk={} drawn_rows={drawn_rows} total_rows={total_rows}",
                owners.len(),
                stage_walks(&report.step_stage_walks[step]),
            );
        }
    }

    let elapsed = started.elapsed();
    if args.gate {
        anyhow::ensure!(elapsed.as_secs() < 300, "walk gate exceeded five minutes");
    }
    println!(
        "seeds={} events={} frames={} distinct={} changed_max={} touched_rows_max={} walked_items_max={} drawn_rows_max={} best_draw_max_us={} draw_max_us={} wall_ms={}",
        runs.len(),
        total_events,
        frames,
        distinct,
        max_changed,
        max_touched_rows,
        max_walked_items,
        max_drawn_rows,
        max_best,
        max_draw,
        elapsed.as_millis(),
    );

    // The frame bound is asked of the draws a run takes before its recorder
    // attaches, because those are the only frames here without the
    // recording in them. A per-step draw carries the fingerprinting, the
    // two record clones, the owner clone and the clock read that recording
    // costs, which measured four fifths of the frame; a bound on that
    // number would be a bound on the harness.
    //
    // And it is asked of the best of them rather than of one. A single draw
    // measures the machine as much as the frame: the same frame read 1290
    // us quiet and 5922 with four crates compiling beside it. The smallest
    // of several draws is still work the frame did; the largest is the
    // scheduler's. Every sample is printed, so a wide spread says the host
    // was busy instead of being read as a regression.
    if args.gate {
        anyhow::ensure!(
            max_best <= FRAME_BOUND_US,
            "a frame is over the {FRAME_BOUND_US} us bound: best_draw_max_us={max_best}"
        );
    }
    Ok(())
}
