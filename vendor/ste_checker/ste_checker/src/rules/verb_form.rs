use harper_brill::UPOS;
use harper_core::{
	Document, Span, Token,
	linting::{Lint, LintKind},
};

use super::prose_words;
use crate::{ctx::Ctx, tags::Tags};

pub(crate) const BE: &[&str] = &["be", "am", "is", "are", "was", "were", "been", "being"];
const HAVE: &[&str] = &["have", "has", "had"];
/// Negation sits between the auxiliary and its verb without breaking the pair.
pub(crate) const NEGATION: &[&str] = &["not", "never"];

pub fn passive(doc: &Document, tags: &Tags, _ctx: &Ctx) -> Vec<Lint> {
	let src = doc.get_source();
	aux_pairs(doc, tags, BE)
		.into_iter()
		.filter(|(_, verb)| !verb.kind.is_verb_progressive_form())
		.map(|(aux, verb)| Lint {
			span: Span::new(aux.span.start, verb.span.end),
			lint_kind: LintKind::Style,
			message: format!(
				"Passive voice (`{} {}`). ASD-STE100 wants the active voice: name who or what does the action.",
				aux.get_str(src),
				verb.get_str(src)
			),
			..Default::default()
		})
		.collect()
}

pub fn compound_tense(doc: &Document, tags: &Tags, _ctx: &Ctx) -> Vec<Lint> {
	let src = doc.get_source();
	aux_pairs(doc, tags, HAVE)
		.into_iter()
		.map(|(aux, verb)| Lint {
			span: Span::new(aux.span.start, verb.span.end),
			lint_kind: LintKind::Style,
			message: format!(
				"Compound tense (`{} {}`). ASD-STE100 permits only the simple present, simple past and simple future.",
				aux.get_str(src),
				verb.get_str(src)
			),
			..Default::default()
		})
		.collect()
}

pub fn ing_as_verb(doc: &Document, tags: &Tags, _ctx: &Ctx) -> Vec<Lint> {
	let src = doc.get_source();
	prose_words(doc, tags)
		.filter(|(i, t)| tags.pos(*i) == Some(UPOS::VERB) && t.kind.is_verb_progressive_form())
		.map(|(_, t)| Lint {
			span: t.span,
			lint_kind: LintKind::Style,
			message: format!("`{}` is an -ing form used as a verb. ASD-STE100 allows -ing words only inside a Technical Name.", t.get_str(src)),
			..Default::default()
		})
		.collect()
}

/// Pairs an auxiliary from `auxiliaries` with the verb it governs. Only negation may sit
/// between the two, so `is to run` and `has to be` do not read as one construction.
fn aux_pairs<'a>(doc: &'a Document, tags: &'a Tags, auxiliaries: &[&str]) -> Vec<(&'a Token, &'a Token)> {
	let src = doc.get_source();
	let words: Vec<(usize, &Token)> = prose_words(doc, tags).collect();
	let mut out = Vec::new();
	for (i, (idx, t)) in words.iter().enumerate() {
		if tags.pos(*idx) != Some(UPOS::AUX) || !auxiliaries.contains(&t.get_str(src).to_lowercase().as_str()) {
			continue;
		}
		let mut j = i + 1;
		while j < words.len() && NEGATION.contains(&words[j].1.get_str(src).to_lowercase().as_str()) {
			j += 1;
		}
		if j < words.len() && tags.pos(words[j].0) == Some(UPOS::VERB) {
			out.push((*t, words[j].1));
		}
	}
	out
}
