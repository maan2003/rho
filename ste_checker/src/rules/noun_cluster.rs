use harper_brill::UPOS;
use harper_core::{
	Document, Span,
	linting::{Lint, LintKind},
};

use crate::{ctx::Ctx, tags::Tags};

const MAX_NOUNS: usize = 3;

/// Maximal runs of nouns over the disambiguated tags. Harper's `iter_nominal_phrases` cannot be
/// used here: it splits on the chunker's `np_member`, which the same tagger error that mislabels
/// `cover` also breaks, and which the cascade does not rewrite.
pub fn cluster(doc: &Document, tags: &Tags, _ctx: &Ctx) -> Vec<Lint> {
	let mut out = Vec::new();
	let mut run: Vec<Span<char>> = Vec::new();
	for (i, t) in doc.get_tokens().iter().enumerate() {
		if t.kind.is_whitespace() {
			continue;
		}
		if t.kind.is_word() && tags.pos(i) == Some(UPOS::NOUN) && !tags.in_heading(t.span) {
			run.push(t.span);
			continue;
		}
		report(&mut run, &mut out);
	}
	report(&mut run, &mut out);
	out
}

fn report(run: &mut Vec<Span<char>>, out: &mut Vec<Lint>) {
	if run.len() > MAX_NOUNS {
		out.push(Lint {
			span: Span::new(run[0].start, run[run.len() - 1].end),
			lint_kind: LintKind::Readability,
			message: format!("{} nouns in a row; ASD-STE100 allows at most {MAX_NOUNS}. Break the cluster up with a preposition.", run.len()),
			..Default::default()
		});
	}
	run.clear();
}
