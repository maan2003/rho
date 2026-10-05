//! Deterministic metamorphic fuzzer for `ste_checker`, driven entirely through the facade.
//!
//! A finding is a judgement about English and there is no oracle for one — that is what the corpus
//! counts and the precision/recall floor in `tests/integration/` are for. What is checkable without
//! a reader is everything the crate claims *around* a finding, and every such claim here is either
//! an invariance (the same text twice, a knob that only subtracts) or a structural one (a span
//! indexes the source, a fence is invisible, a written file reads back).
//!
//! The FRNG, the minimizer, the corpus format and the scan/record/replay loop are
//! [`v_utils::fuzz`] — shared with `trading_data` and `dockviewers_core`. What is local is the table
//! below and the generators in `draw.rs`. Env-var replay:
//! `FUZZ_SEED=… FUZZ_SIZE=… FUZZ_TARGET=… cargo t -p ste_checker --test fuzz -- --nocapture`
//! verbose-replays one case.

/// An oracle violation, as the value a target returns rather than a panic — a panic is reserved for
/// production code blowing up under the trace, and the two want telling apart in the report.
macro_rules! check {
	($cond:expr, $($arg:tt)*) => {
		if !$cond {
			return Err(format!($($arg)*));
		}
	};
}

mod draw;
mod nix;
mod text;

use v_utils::fuzz::{Frng, Suite, Target};

const TARGETS: &[Target] = &[
	Target {
		name: "spans",
		version: text::VERSION,
		run: |s, z, v| text::run_spans(&mut Frng::new(s, z), v),
	},
	Target {
		name: "determinism",
		version: text::VERSION,
		run: |s, z, v| text::run_determinism(&mut Frng::new(s, z), v),
	},
	Target {
		name: "fences",
		version: text::VERSION,
		run: |s, z, v| text::run_fences(&mut Frng::new(s, z), v),
	},
	Target {
		name: "monotone",
		version: text::VERSION,
		run: |s, z, v| text::run_monotone(&mut Frng::new(s, z), v),
	},
	Target {
		name: "suggest",
		version: text::VERSION,
		run: |s, z, v| text::run_suggest(&mut Frng::new(s, z), v),
	},
	Target {
		name: "nix_round_trip",
		version: nix::VERSION,
		run: |s, z, v| nix::run_round_trip(&mut Frng::new(s, z), v),
	},
	Target {
		name: "nix_mutation",
		version: nix::VERSION,
		run: |s, z, v| nix::run_mutation(&mut Frng::new(s, z), v),
	},
];

const SUITE: Suite = Suite {
	targets: TARGETS,
	corpus: concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fuzz/CORPUS.txt"),
};

#[test]
fn fuzz() {
	SUITE.fuzz();
}

#[test]
fn regressions() {
	SUITE.regressions();
}
