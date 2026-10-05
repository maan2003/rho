//! Procedural ASD-STE100 checks over [`harper_core`]'s markdown parser and POS tagger.
//!
//! Roughly ten of the standard's 53 rules are decidable without understanding the text;
//! the rest are semantic. See docs/ARCHITECTURE.md.
use harper_core::{Document, linting::Lint};

pub mod config;
pub mod ctx;
pub mod glossary;
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

pub fn rule_names() -> impl Iterator<Item = &'static str> {
	rules::RULES.iter().map(|(name, _)| *name)
}
