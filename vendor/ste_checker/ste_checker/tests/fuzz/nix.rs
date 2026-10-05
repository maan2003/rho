//! Targets over the glossary parser — the one piece of this crate that reads a file byte by byte
//! and is written by hand rather than derived. `glossary_nix_round_trips` pins the seven rejections
//! that were thought of; these two are for the ones that were not.

use ste_checker::glossary::Glossary;
use v_utils::fuzz::Frng;

use crate::draw;

pub const VERSION: u32 = v_utils::fuzz::fnv(&[v_utils::fuzz::FRNG_SRC, include_str!("draw.rs"), include_str!("nix.rs")]);

/// Whatever a glossary holds, the file can hold: writing one out and reading it back is the
/// identity. The two spellings of an entry with no definition are drawn, so this also says they
/// mean the same thing.
pub fn run_round_trip(f: &mut Frng, verbose: bool) -> Result<(), String> {
	let glossary = draw::glossary(f);
	let src = draw::render(&glossary, f);
	if verbose {
		eprintln!("--- rendered ---\n{src}");
	}
	let parsed = Glossary::parse(&src).map_err(|e| format!("a rendered glossary does not parse: {e}\n{src}"))?;
	check!(parsed == glossary, "round trip lost something:\n  wrote {glossary:?}\n  read  {parsed:?}\n{src}");
	Ok(())
}

/// A damaged file is a rejection, never a panic and never a partial reading. The parser indexes its
/// source by byte while the damage is by character, which is exactly where the two would disagree —
/// and a `bail!` reports a line and a column, so every error path slices the source too.
pub fn run_mutation(f: &mut Frng, verbose: bool) -> Result<(), String> {
	let glossary = draw::glossary(f);
	let rendered = draw::render(&glossary, f);
	let src = draw::mutate(f, &rendered);
	if verbose {
		eprintln!("--- mutated ---\n{src}");
	}
	let Ok(parsed) = Glossary::parse(&src) else {
		return Ok(()); // a rejection is the designed outcome; that it is a rejection and not a panic is the claim
	};
	// A reading that succeeded is a whole reading, so it survives being written back out. An
	// exhausted `Frng` draws zeros, which makes `render` canonical.
	let again = draw::render(&parsed, &mut Frng::new(0, 0));
	let reparsed = Glossary::parse(&again).map_err(|e| format!("a parsed glossary does not re-parse: {e}\n{again}"))?;
	check!(reparsed == parsed, "reading a mutated file is not stable:\n  {parsed:?}\n  {reparsed:?}\n{src}");
	Ok(())
}
