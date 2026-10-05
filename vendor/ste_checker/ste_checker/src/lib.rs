//! Procedural ASD-STE100 checks over [`harper_core`]'s markdown parser and POS tagger.
//!
//! Roughly ten of the standard's 53 rules are decidable without understanding the text;
//! the rest are semantic. See docs/ARCHITECTURE.md.
use std::collections::HashSet;

use harper_brill::UPOS;
use harper_core::{Document, TokenKind, linting::Lint};

pub mod config;
pub mod ctx;
pub mod glossary;
pub mod report;
mod rules;
mod tags;
pub mod wordset;

pub use ctx::Ctx;

pub struct Finding {
	pub rule: &'static str,
	pub lint: Lint,
}

/// Char spans in `lint.span` index `text`, not the rendered markdown.
pub fn check(text: &str, ctx: &Ctx) -> Vec<Finding> {
	let doc = Document::new_markdown_default_curated(text);
	let tags = tags::Tags::new(&doc);
	let mut findings: Vec<Finding> = rules::RULES
		.iter()
		.filter(|(name, _)| ctx.rule_enabled(name))
		.flat_map(|(name, rule)| rule(&doc, &tags, ctx).into_iter().map(move |lint| Finding { rule: name, lint }))
		.collect();
	findings.sort_by_key(|f| (f.lint.span.start, f.lint.span.end, f.rule));
	findings
}

/// What `--suggest-glossary` reads: the out-of-vocabulary words of one file, one entry per
/// occurrence, and the identifiers that occur inside code in the same file.
pub struct Vocabulary {
	pub unknown: Vec<(String, UPOS)>,
	/// Weak evidence that a word names a software artifact rather than being ordinary English:
	/// measured 0.6/0.6 against a hand-drawn line, which is enough to annotate a list a human
	/// edits and not enough to filter one.
	pub in_code: HashSet<String>,
}

pub fn vocabulary(text: &str, ctx: &Ctx) -> Vocabulary {
	let doc = Document::new_markdown_default_curated(text);
	let tags = tags::Tags::new(&doc);
	let src = doc.get_source();
	Vocabulary {
		unknown: rules::dictionary::unknown(&doc, &tags, ctx).into_iter().map(|(_, _, base, pos)| (base, pos)).collect(),
		in_code: doc
			.get_tokens()
			.iter()
			.filter(|t| t.kind == TokenKind::Unlintable)
			.flat_map(|t| {
				t.span
					.get_content(src)
					.iter()
					.collect::<String>()
					.split(|c: char| !c.is_alphanumeric())
					.map(str::to_lowercase)
					.filter(|w| !w.is_empty())
					.collect::<Vec<_>>()
			})
			.collect(),
	}
}

pub fn rule_names() -> impl Iterator<Item = &'static str> {
	rules::RULES.iter().map(|(name, _)| *name)
}
