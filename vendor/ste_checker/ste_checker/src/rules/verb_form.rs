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
			message: format!(
				"`{}` is an -ing verb form. Use a simple tense, for example `is running` -> `runs`. Put a quoted word in a code span.",
				t.get_str(src)
			),
			..Default::default()
		})
		.collect()
}

/// A gerund that a preposition takes as its object: `before selecting a fix`. It hides who
/// acts, so it reads worse than a clause with a subject: `before I select a fix`.
const PREPOSITIONS: &[&str] = &[
	"about", "after", "before", "by", "for", "from", "in", "into", "of", "on", "since", "through", "upon", "via", "when", "while", "with", "without",
];
/// -ing words that are adjectives in practice, whatever the tagger says.
const ADJECTIVES: &[&str] = &["existing", "missing", "pending", "remaining", "outstanding", "following", "upcoming", "ongoing"];

pub fn ing_after_preposition(doc: &Document, tags: &Tags, _ctx: &Ctx) -> Vec<Lint> {
	let src = doc.get_source();
	let tokens = doc.get_tokens();
	// The word `i` steps over whitespace to, if any: a token in between ends the phrase.
	let word_at = |i: usize| tokens.get(i).filter(|t| t.kind.is_word() && !tags.in_heading(t.span));
	let next_word = |i: usize, back: bool| {
		let ws = if back { i.checked_sub(1)? } else { i + 1 };
		tokens.get(ws).filter(|t| t.kind.is_whitespace())?;
		let at = if back { ws.checked_sub(1)? } else { ws + 1 };
		word_at(at).map(|t| (at, t))
	};
	prose_words(doc, tags)
		.filter(|(i, t)| tags.pos(*i) == Some(UPOS::VERB) && t.kind.is_verb_progressive_form())
		.filter(|(_, t)| !ADJECTIVES.contains(&t.get_str(src).to_lowercase().as_str()))
		.filter_map(|(i, t)| {
			let (_, prep) = next_word(i, true)?;
			let prep = prep.get_str(src).to_lowercase();
			// Without an object the word is a noun: `get the terms in writing`.
			(PREPOSITIONS.contains(&prep.as_str()) && next_word(i, false).is_some()).then(|| Lint {
				span: t.span,
				lint_kind: LintKind::Style,
				message: format!(
					"`{prep} {}` hides who acts. Write a clause with a subject, for example `before selecting a fix` -> `before I select a fix`.",
					t.get_str(src)
				),
				..Default::default()
			})
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
