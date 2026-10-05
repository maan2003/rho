use harper_core::linting::Suggestion;
use miette::{LabeledSpan, NamedSource, Severity};
use serde::Serialize;

use crate::Finding;

/// Harper spans are char indices; miette wants byte offsets.
struct ByteOffsets(Vec<usize>);

impl ByteOffsets {
	fn new(text: &str) -> Self {
		Self(text.char_indices().map(|(b, _)| b).chain(std::iter::once(text.len())).collect())
	}

	fn range(&self, start: usize, end: usize) -> std::ops::Range<usize> {
		self.0[start]..self.0[end]
	}
}

pub fn human(path: &str, text: &str, findings: &[Finding]) -> Option<miette::Report> {
	if findings.is_empty() {
		return None;
	}
	let offsets = ByteOffsets::new(text);
	let labels: Vec<LabeledSpan> = findings
		.iter()
		.map(|f| {
			let range = offsets.range(f.lint.span.start, f.lint.span.end);
			assert!(
				text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
				"span {range:?} splits a character in {path}"
			);
			LabeledSpan::new(Some(format!("{}: {}", f.rule, f.lint.message)), range.start, range.len())
		})
		.collect();

	Some(
		miette::miette!(severity = Severity::Warning, code = "ste_checker", labels = labels, "{} ASD-STE100 finding(s)", findings.len())
			.with_source_code(NamedSource::new(path, text.to_string()).with_language("Markdown")),
	)
}

#[derive(Serialize)]
pub struct JsonFinding<'a> {
	pub file: &'a str,
	pub rule: &'a str,
	pub message: &'a str,
	pub start: usize,
	pub end: usize,
	pub suggestions: Vec<String>,
}

pub fn json<'a>(path: &'a str, text: &str, findings: &'a [Finding]) -> Vec<JsonFinding<'a>> {
	let offsets = ByteOffsets::new(text);
	findings
		.iter()
		.map(|f| {
			let range = offsets.range(f.lint.span.start, f.lint.span.end);
			JsonFinding {
				file: path,
				rule: f.rule,
				message: &f.lint.message,
				start: range.start,
				end: range.end,
				suggestions: f
					.lint
					.suggestions
					.iter()
					.filter_map(|s| match s {
						Suggestion::ReplaceWith(chars) => Some(chars.iter().collect()),
						Suggestion::InsertAfter(_) | Suggestion::Remove => None,
					})
					.collect(),
			}
		})
		.collect()
}
