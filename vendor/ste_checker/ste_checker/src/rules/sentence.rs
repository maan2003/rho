use harper_core::{
	Document, Token, TokenStringExt,
	linting::{Lint, LintKind},
};

use crate::{ctx::Ctx, tags::Tags};

/// Table cells and list fragments each parse as their own one- or two-word "sentence".
const MIN_WORDS: usize = 3;

pub fn length(doc: &Document, tags: &Tags, ctx: &Ctx) -> Vec<Lint> {
	let max = ctx.config.text_type.max_sentence_words();
	let src = doc.get_source();
	doc.iter_sentences()
		.flat_map(|s| split_after_letters(s, src))
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
				message: format!("This sentence has {words} words. The limit is {max}. Split it into shorter sentences."),
				..Default::default()
			})
		})
		.collect()
}

/// Harper reads a lone letter before a period as an initial (`J. Smith`) and runs on into the
/// next sentence. In technical prose the letter is a unit or a name (`took 2 s. Then`, `date X.
/// Then`), so a period after it, followed by a capital, ends the sentence.
fn split_after_letters<'a>(sentence: &'a [Token], src: &[char]) -> Vec<&'a [Token]> {
	let word = |t: &Token| t.kind.is_word().then(|| t.get_str(src));
	// Harper keeps the period in the word: `X.` is one token.
	let ends = |i: usize| {
		word(&sentence[i]).is_some_and(|w| {
			let mut chars = w.chars();
			chars.next().is_some_and(char::is_alphabetic) && chars.as_str() == "."
		}) && sentence.get(i + 1).is_some_and(|t| t.kind.is_whitespace())
			&& sentence.get(i + 2).and_then(word).is_some_and(|w| w.starts_with(char::is_uppercase))
	};
	let mut parts = Vec::new();
	let mut start = 0;
	for i in 0..sentence.len() {
		if ends(i) {
			parts.push(&sentence[start..=i]);
			start = i + 2;
		}
	}
	parts.push(&sentence[start..]);
	parts
}
