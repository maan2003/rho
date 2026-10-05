use harper_core::{
	Document, TokenStringExt,
	linting::{Lint, LintKind},
};

use crate::{ctx::Ctx, tags::Tags};

/// Table cells and list fragments each parse as their own one- or two-word "sentence".
const MIN_WORDS: usize = 3;

pub fn length(doc: &Document, tags: &Tags, ctx: &Ctx) -> Vec<Lint> {
	let max = ctx.config.text_type.max_sentence_words();
	doc.iter_sentences()
		.filter_map(|s| {
			let span = s.span()?;
			if tags.in_heading(span) {
				return None;
			}
			let words = s.iter().filter(|t| t.kind.is_word()).count();
			if words < MIN_WORDS || words <= max {
				return None;
			}
			Some(Lint {
				span,
				lint_kind: LintKind::Readability,
				message: format!("{words} words; ASD-STE100 allows at most {max} here. Split the sentence."),
				..Default::default()
			})
		})
		.collect()
}
