//! The generators every target draws from: one Markdown document, and one glossary with its Nix
//! rendering. They live together because a target's fingerprint is over the sources that decide
//! what its `(seed, size)` means, and that is this file plus the target's own.
//!
//! The word pools are chosen for what they make the rules do, not for realism: each one is a class
//! some rule or some part of `tags.rs` treats differently, so a document is a mix of the cases that
//! disagree rather than of English.

use std::collections::HashMap;

use ste_checker::glossary::Glossary;
use v_utils::fuzz::Frng;

const APPROVED: &[&str] = &[
	"remove", "install", "the", "file", "do", "not", "work", "lever", "system", "start", "unit", "make", "value", "test", "a", "and", "to",
];
/// Out-of-vocabulary in every content part of speech, adverbs included: the glossary has a list for
/// a noun and a list for a verb and none for anything else, so a pool of nouns would leave the arm
/// that decides where a suggested word goes untested.
const UNKNOWN: &[&str] = &["browser", "socket", "kubernetes", "repo", "cache", "daemon", "webhook", "quicker", "requires", "asynchronously", "natively", "programmatically"];
const AUXILIARY: &[&str] = &["is", "are", "was", "were", "have", "has", "had", "been", "never"];
const PARTICIPLE: &[&str] = &["specified", "configured", "chosen", "written", "found", "enabled"];
const PROGRESSIVE: &[&str] = &["watching", "running", "forwarding", "choosing", "monitoring"];
const CONTRACTED: &[&str] = &["can't", "won't", "it's", "isn\u{2019}t", "we\u{2019}re", "let's"];
const HYPHENATED: &[&str] = &["well-known", "argument-less", "1-fold", "read-only"];
/// Multi-byte, zero-width and combining forms — the char/byte boundary between Harper's spans and
/// the reported offsets is the one thing here that is not a rule.
const ODD: &[&str] = &[
	"\u{dc}nicode",
	"cafe\u{301}",
	"\u{65e5}\u{672c}\u{8a9e}",
	"\u{1f680}",
	"\u{2014}",
	"na\u{200b}me",
	"\u{ff21}\u{ff22}",
];
/// Nouns that stack, so `noun-cluster` and the cascade's noun-run rule have something to resolve.
const NOUNS: &[&str] = &["gear", "housing", "cover", "plate", "assembly", "bolt", "panel", "tank"];

pub fn word(f: &mut Frng) -> &'static str {
	let pool = match f.weighted(&[10, 6, 4, 3, 3, 2, 2, 2, 4]) {
		0 => APPROVED,
		1 => UNKNOWN,
		2 => AUXILIARY,
		3 => PARTICIPLE,
		4 => PROGRESSIVE,
		5 => CONTRACTED,
		6 => HYPHENATED,
		7 => ODD,
		_ => NOUNS,
	};
	pool[f.below(pool.len() as u32) as usize]
}

fn words(f: &mut Frng, n: usize) -> String {
	let mut out = String::new();
	for i in 0..n {
		if i > 0 {
			out.push_str(if f.below(8) == 0 { ", " } else { " " });
		}
		out.push_str(word(f));
	}
	out
}

/// Up to 30 words, which is over both sentence-length ceilings, so the rule is reachable.
fn sentence(f: &mut Frng) -> String {
	let n = 1 + f.below(30) as usize;
	format!("{}{}", words(f, n), [".", "!", "?", "."][f.below(4) as usize])
}

fn block(f: &mut Frng) -> String {
	let kind = f.weighted(&[8, 2, 3, 2, 1, 1, 2, 1]);
	let (n, pick) = (1 + f.below(6) as usize, f.below(3) as usize);
	match kind {
		0 => format!("{}\n", sentence(f)),
		1 => format!("{} {}\n", ["#", "##", "###"][pick], words(f, n)),
		2 => format!("- {}\n", sentence(f)),
		3 => format!("```{}\n{}\n```\n", ["", "rust", "sh"][pick], words(f, n)),
		4 => format!("| {} | {} |\n|---|---|\n| {} | {} |\n", words(f, 2), words(f, 2), words(f, 2), words(f, 2)),
		5 => format!("> {}\n", sentence(f)),
		6 => format!("Use `{}` for the {}.\n", words(f, n), word(f)),
		_ => format!("[{}](https://example.com/{}).\n", words(f, 2), word(f)),
	}
}

/// Blocks until the buffer runs out. No pool holds a backtick, so a document only ever opens a
/// fence deliberately — which is what lets `fences` wrap one in another.
pub fn markdown(f: &mut Frng) -> String {
	let mut out = String::new();
	while f.remaining() > 4 {
		out.push_str(&block(f));
		out.push('\n');
	}
	out
}

/// Names are drawn already lowercased, because the parser lowercases what it reads and a
/// round-trip has to compare against what the file can hold.
pub fn glossary(f: &mut Frng) -> Glossary {
	let entry = |f: &mut Frng, into: &mut HashMap<String, Option<String>>| {
		let name = word(f).to_lowercase();
		let desc = match f.below(3) {
			0 => None,
			_ => Some(desc(f)),
		};
		into.insert(name, desc);
	};
	let mut out = Glossary::default();
	while f.remaining() > 8 {
		match f.below(2) {
			0 => entry(f, &mut out.names),
			_ => entry(f, &mut out.verbs),
		}
	}
	out
}

/// A definition, as a Nix double-quoted string can hold it: `#` does not open a comment inside one
/// and a newline needs no escape, so both belong here; `"`, `\` and `${` are rejected by design and
/// so are drawn only by [`mutate`].
fn desc(f: &mut Frng) -> String {
	const PIECES: &[&str] = &["", " ", "#", ";", "{", "}", "[", "]", "\n", "=", "//", "the ", "\u{65e5}", "'", "-"];
	let mut out = String::new();
	for _ in 0..f.below(6) {
		out.push_str(PIECES[f.below(PIECES.len() as u32) as usize]);
		out.push_str(word(f));
	}
	out
}

/// The two spellings of an entry with no definition are the same entry, so which one a glossary is
/// written in is drawn rather than fixed. An exhausted `Frng` draws zeros, so
/// `render(g, &mut Frng::new(0, 0))` is the canonical rendering of `g`.
pub fn render(g: &Glossary, f: &mut Frng) -> String {
	let mut out = String::from("{\n");
	for (key, entries) in [("names", &g.names), ("verbs", &g.verbs)] {
		out.push_str(&format!("  {key} = [\n"));
		let mut sorted: Vec<_> = entries.iter().collect();
		sorted.sort();
		for (name, desc) in sorted {
			out.push_str(&match desc {
				Some(d) => format!("    {{ name = \"{name}\"; desc = \"{d}\"; }}\n"),
				None if f.below(2) == 0 => format!("    {{ name = \"{name}\"; }}\n"),
				None => format!("    \"{name}\"\n"),
			});
		}
		out.push_str("  ];\n");
	}
	out.push_str("}\n");
	out
}

/// A few character edits, drawn from the alphabet the parser has rules about. Edits are over
/// `char`s rather than bytes so the result is still a `&str` — the parser indexes it by byte, and
/// whether those two agree is what this feeds.
pub fn mutate(f: &mut Frng, src: &str) -> String {
	const NASTY: &[char] = &['{', '}', '[', ']', '"', '\\', '$', '#', ';', '=', '\n', '\u{e9}', '\u{1f680}', '\'', '\u{4e2d}'];
	let mut chars: Vec<char> = src.chars().collect();
	for _ in 0..1 + f.below(4) {
		if chars.is_empty() {
			break;
		}
		// `below` spends one byte and so collapses past 256; a rendering is longer than that.
		let at = f.span(0.0, (chars.len() - 1) as f64) as usize;
		let nasty = NASTY[f.below(NASTY.len() as u32) as usize];
		match f.below(3) {
			0 => {
				chars.remove(at);
			}
			1 => chars.insert(at, nasty),
			_ => chars[at] = nasty,
		}
	}
	chars.into_iter().collect()
}
