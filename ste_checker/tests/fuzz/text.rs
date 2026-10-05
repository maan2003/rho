//! Targets over `check`, `vocabulary` and `report` — everything a document goes through.
//!
//! There is no oracle for "is this finding right": the standard needs a reader for that, and the
//! corpus counts in the integration binary are where the judgement lives. What is checkable without
//! one is what the crate claims *around* the findings — that spans index the source, that the same
//! text reads the same way twice, that a fence is invisible, and that the two suppression knobs only
//! ever subtract.

use ste_checker::{
	Ctx, Finding,
	config::{AppConfig, TextType},
	glossary::Glossary,
	report,
};
use v_utils::fuzz::Frng;

use crate::draw;

pub const VERSION: u32 = v_utils::fuzz::fnv(&[v_utils::fuzz::FRNG_SRC, include_str!("draw.rs"), include_str!("text.rs")]);

const DICTIONARY: [&str; 3] = ["unapproved-word", "wrong-pos", "unknown-word"];

fn ctx(glossary: Glossary) -> Ctx {
	Ctx::new(AppConfig::default(), glossary)
}

/// A finding's identity for comparison: the rule and the span it owns.
fn keys(findings: &[Finding]) -> Vec<(&'static str, usize, usize)> {
	findings.iter().map(|f| (f.rule, f.lint.span.start, f.lint.span.end)).collect()
}

fn preview(text: &str, findings: &[Finding], verbose: bool) {
	if verbose {
		eprintln!(
			"--- {} bytes, {} chars ---\n{text}--- {} finding(s): {:?}",
			text.len(),
			text.chars().count(),
			findings.len(),
			keys(findings)
		);
	}
}

/// **Spans index the original text.** The claim an editor acts on, and the one place a `char` count
/// and a byte offset have to agree: Harper counts characters, miette and every editor count bytes.
pub fn run_spans(f: &mut Frng, verbose: bool) -> Result<(), String> {
	let text = draw::markdown(f);
	let findings = ste_checker::check(&text, &ctx(Glossary::default()));
	preview(&text, &findings, verbose);
	let chars: Vec<char> = text.chars().collect();
	for finding in &findings {
		let (start, end) = (finding.lint.span.start, finding.lint.span.end);
		check!(start <= end, "{}: span {start}..{end} is inverted", finding.rule);
		check!(end <= chars.len(), "{}: span {start}..{end} runs past the {} chars of the source", finding.rule, chars.len());
	}
	// The human report asserts the boundary claim from inside the crate; the JSON one is where the
	// offsets escape, so it is checked against the source itself.
	check!(
		report::human("fuzz.md", &text, &findings).is_some() == !findings.is_empty(),
		"the human report disagrees with itself about having anything to say"
	);
	for (json, finding) in report::json("fuzz.md", &text, &findings).iter().zip(&findings) {
		check!(
			text.is_char_boundary(json.start) && text.is_char_boundary(json.end),
			"{}: byte range {}..{} splits a character",
			json.rule,
			json.start,
			json.end
		);
		let by_char: String = chars[finding.lint.span.start..finding.lint.span.end].iter().collect();
		check!(
			text[json.start..json.end] == by_char,
			"{}: bytes {}..{} read {:?}, the char span reads {by_char:?}",
			json.rule,
			json.start,
			json.end,
			&text[json.start..json.end]
		);
	}
	Ok(())
}

/// The tagger cascade, the wordset and the glossary are all maps, and a rule that reads one in
/// iteration order would report a different set of findings per run while every test above still
/// passes.
pub fn run_determinism(f: &mut Frng, verbose: bool) -> Result<(), String> {
	let text = draw::markdown(f);
	let (first, again) = (ste_checker::check(&text, &ctx(Glossary::default())), ste_checker::check(&text, &ctx(Glossary::default())));
	preview(&text, &first, verbose);
	check!(keys(&first) == keys(&again), "two runs over one text disagree:\n  {:?}\n  {:?}", keys(&first), keys(&again));
	for (a, b) in first.iter().zip(&again) {
		check!(
			a.lint.message == b.lint.message,
			"two runs disagree about a message:\n  {:?}\n  {:?}",
			a.lint.message,
			b.lint.message
		);
	}
	Ok(())
}

/// **Code never reaches a rule.** Harper parses the Markdown, so a fenced block is `Unlintable` and
/// a whole document wrapped in one has nothing to say. `tedi__usage.md` pins this for one real file;
/// here it is the same claim over anything the generator can write.
pub fn run_fences(f: &mut Frng, verbose: bool) -> Result<(), String> {
	let body = draw::markdown(f);
	if body.trim().is_empty() {
		return Ok(());
	}
	let text = format!("````text\n{body}\n````\n");
	let findings = ste_checker::check(&text, &ctx(Glossary::default()));
	preview(&text, &findings, verbose);
	check!(findings.is_empty(), "a fenced document produced {:?}", keys(&findings));
	Ok(())
}

/// **Both suppression knobs only subtract.** A glossary entry is read in `verdict` before any
/// wordset lookup and no other rule sees one, so declaring a word may drop a dictionary finding and
/// may not touch anything else; `Description` raises the sentence-length ceiling and nothing more.
/// A knob that adds a finding somewhere else is a knob nobody can reason about.
pub fn run_monotone(f: &mut Frng, verbose: bool) -> Result<(), String> {
	let text = draw::markdown(f);
	let base = ste_checker::check(&text, &ctx(Glossary::default()));
	preview(&text, &base, verbose);

	// Drawn from the words the checker itself named, so the glossary is about this document rather
	// than a random list that would suppress nothing.
	let mut glossary = Glossary::default();
	for (word, _) in ste_checker::vocabulary(&text, &ctx(Glossary::default())).unknown {
		match f.below(2) {
			0 => glossary.names.insert(word, None),
			_ => glossary.verbs.insert(word, None),
		};
	}
	if verbose {
		eprintln!(
			"glossary: names {:?} verbs {:?}",
			glossary.names.keys().collect::<Vec<_>>(),
			glossary.verbs.keys().collect::<Vec<_>>()
		);
	}
	let glossed = ste_checker::check(&text, &ctx(glossary));
	let (before, after) = (keys(&base), keys(&glossed));
	for k in &after {
		check!(before.contains(k), "the glossary invented {k:?}; it may only take findings away");
	}
	let outside = |ks: &[(&'static str, usize, usize)]| ks.iter().filter(|(r, ..)| !DICTIONARY.contains(r)).copied().collect::<Vec<_>>();
	check!(
		outside(&before) == outside(&after),
		"the glossary moved a rule that never reads one:\n  {:?}\n  {:?}",
		outside(&before),
		outside(&after)
	);

	let described = keys(&ste_checker::check(
		&text,
		&Ctx::new(
			AppConfig {
				text_type: TextType::Description,
				..Default::default()
			},
			Glossary::default(),
		),
	));
	for k in &described {
		check!(before.contains(k), "the longer sentence ceiling invented {k:?}");
	}
	Ok(())
}

/// **The bootstrap loop closes.** `--suggest-glossary` writes a skeleton of every out-of-vocabulary
/// word, and the next run reads it: the skeleton has to parse, and it has to absorb the words it was
/// written from. `suggest_glossary_emits_parseable_nix` pins the first half over the corpus; the
/// second half is what makes the file worth writing.
pub fn run_suggest(f: &mut Frng, verbose: bool) -> Result<(), String> {
	let text = draw::markdown(f);
	let found = ste_checker::vocabulary(&text, &ctx(Glossary::default())).unknown;
	preview(&text, &[], verbose);
	if found.is_empty() {
		return Ok(());
	}

	// The shape `main.rs::skeleton` prints, minus the counts and the in-code annotation: a comment
	// and a column width cannot change whether it parses.
	let mut src = String::from("{\n");
	for (key, verbs) in [("names", false), ("verbs", true)] {
		src.push_str(&format!("  {key} = [\n"));
		for (word, _) in found.iter().filter(|(_, pos)| (*pos == harper_brill::UPOS::VERB) == verbs) {
			src.push_str(&format!("    {{ name = \"{word}\"; desc = \"\"; }}\n"));
		}
		src.push_str("  ];\n");
	}
	src.push_str("}\n");
	if verbose {
		eprintln!("--- suggested ---\n{src}");
	}

	let parsed = Glossary::parse(&src).map_err(|e| format!("the suggested skeleton does not parse: {e}\n{src}"))?;
	for (word, _) in &found {
		check!(
			parsed.names.contains_key(word) || parsed.verbs.contains_key(word),
			"`{word}` was suggested and the skeleton does not hold it"
		);
	}
	let chars: Vec<char> = text.chars().collect();
	let left: Vec<String> = ste_checker::check(&text, &ctx(parsed))
		.into_iter()
		.filter(|f| f.rule == "unknown-word")
		.map(|f| chars[f.lint.span.start..f.lint.span.end].iter().collect::<String>().to_lowercase())
		.collect();

	// QUARANTINE — a product finding, reported and not fixed here. `main.rs::skeleton` sends
	// everything that is not a VERB to `names`, and `Glossary::allows` honours a name only as
	// NOUN/PROPN/ADJ, so an out-of-vocabulary adverb comes back at the next run carrying the same
	// "add it to `docs/glossary.nix`" it already was added to. The standard gives an adverb neither
	// of the two lists it has, so which way that goes is a design call rather than a bug in this
	// target. The count check below fails the moment it is decided, so nobody has to remember this.
	let adverbs: Vec<&String> = found.iter().filter(|(_, pos)| *pos == harper_brill::UPOS::ADV).map(|(word, _)| word).collect();
	for word in &left {
		check!(adverbs.contains(&word), "the suggested glossary left `{word}` behind, and it is not the adverb case: {left:?}");
	}
	check!(
		adverbs.is_empty() == left.is_empty(),
		"the adverb quarantine is stale: {} adverb(s) were suggested and {} came back — delete it and restore the plain claim",
		adverbs.len(),
		left.len()
	);
	Ok(())
}
