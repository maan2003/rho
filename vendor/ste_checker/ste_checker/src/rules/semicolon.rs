use harper_core::{
	Document, Punctuation, TokenKind,
	linting::{Lint, LintKind},
};

use crate::{ctx::Ctx, tags::Tags};

/// Rule 8.1 bans the mark outright, not only as a join between two clauses.
pub fn semicolon(doc: &Document, tags: &Tags, _ctx: &Ctx) -> Vec<Lint> {
	doc.get_tokens()
		.iter()
		.filter(|t| t.kind == TokenKind::Punctuation(Punctuation::Semicolon) && !tags.in_heading(t.span))
		.map(|t| Lint {
			span: t.span,
			lint_kind: LintKind::Style,
			message: "ASD-STE100 allows no semicolons. Write two sentences.".to_owned(),
			..Default::default()
		})
		.collect()
}
