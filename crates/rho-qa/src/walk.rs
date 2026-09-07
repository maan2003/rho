use std::time::Instant;

use anyhow::Result;
use clap::Args;
use rho_gui::walk::{WalkConfig, WalkHarness, WalkMode};

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
    let mut max_cold = 0;
    let mut max_warm = 0;
    for seed in seeds {
        let config = WalkConfig {
            seed: *seed,
            steps,
            mode,
        };
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
                "oracle={} seed={seed}\nstep={} event={event}\ncold_draw_us={} warm_draw_us={} event_draw_us={} touched_rows={} walked_items={}\nshrunk sequence:\n{script}\nsub-scene changes (capped at 32):\n{details}",
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
        max_cold = max_cold.max(report.cold_draw_micros);
        max_warm = max_warm.max(report.warm_draw_micros);
        if !report.wall_clock_findings.is_empty() {
            let mut baseline = report.baseline_owners.iter().collect::<Vec<_>>();
            baseline.sort_by_key(|owner| std::cmp::Reverse(owner.paint_nanos));
            for owner in baseline.into_iter().take(12) {
                println!(
                    "WALL_CLOCK_BASELINE_OWNER seed={seed} owner={} paint_ns={} primitives={} bounds={:?}",
                    owner.owner, owner.paint_nanos, owner.primitives, owner.bounds
                );
            }
        }
        for finding in &report.wall_clock_findings {
            let sequence =
                serde_json::to_string(&finding.sequence).unwrap_or_else(|_| "[]".to_owned());
            println!(
                "WALL_CLOCK_FINDING seed={seed} step={} draw_us={} touched_rows={} walked_items={} sequence={sequence}",
                finding.step, finding.draw_micros, finding.touched_rows, finding.walked_items
            );
            let mut owners = report.step_owners[finding.step].iter().collect::<Vec<_>>();
            owners.sort_by_key(|owner| std::cmp::Reverse(owner.paint_nanos));
            for owner in owners.into_iter().take(12) {
                println!(
                    "WALL_CLOCK_OWNER seed={seed} step={} owner={} paint_ns={} primitives={} changed={} bounds={:?}",
                    finding.step,
                    owner.owner,
                    owner.paint_nanos,
                    owner.primitives,
                    owner.changed_primitives,
                    owner.bounds,
                );
            }
        }
        for (step, ((((draw, touched_rows), walked_items), drawn_rows), total_rows)) in report
            .step_draw_micros
            .iter()
            .zip(&report.step_touched_rows)
            .zip(&report.step_walked_items)
            .zip(&report.step_drawn_rows)
            .zip(&report.step_total_rows)
            .enumerate()
        {
            println!(
                "seed={seed} step={step} draw_us={draw} touched_rows={touched_rows} walked_items={walked_items} drawn_rows={drawn_rows} total_rows={total_rows}"
            );
        }
    }

    let elapsed = started.elapsed();
    if args.gate {
        anyhow::ensure!(elapsed.as_secs() < 300, "walk gate exceeded five minutes");
    }
    println!(
        "seeds={} events={} frames={} distinct={} changed_max={} touched_rows_max={} walked_items_max={} drawn_rows_max={} cold_draw_max_us={} warm_draw_max_us={} draw_max_us={} wall_ms={}",
        seeds.len(),
        seeds.len() * steps,
        frames,
        distinct,
        max_changed,
        max_touched_rows,
        max_walked_items,
        max_drawn_rows,
        max_cold,
        max_warm,
        max_draw,
        elapsed.as_millis(),
    );
    Ok(())
}
