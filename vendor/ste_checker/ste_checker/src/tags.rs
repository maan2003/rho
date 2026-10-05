//! An ordered, cascading disambiguator over Harper's tags, after LanguageTool's
//! `en/disambiguation.xml`. Harper's tokens are immutable from outside its crate, so this is a
//! side table the rules read instead of `pos_tag`.
use harper_brill::UPOS;
use harper_core::{Document, Punctuation, Span, Token, TokenKind, TokenStringExt};

use crate::rules::verb_form::{BE, NEGATION};

pub(crate) struct Tags {
	pos: Vec<Option<UPOS>>,
	/// LT's `action="immunize"`: the tagger and openSTE are not comparable over these tokens.
	immune: Vec<bool>,
	headings: Vec<Span<char>>,
}

impl Tags {
	pub(crate) fn new(doc: &Document) -> Self {
		let tokens = doc.get_tokens();
		let src = doc.get_source();
		let mut tags = Self {
			pos: tokens.iter().map(|t| t.kind.as_word().and_then(|m| m.as_ref()).and_then(|m| m.pos_tag)).collect(),
			immune: tokens
				.iter()
				.enumerate()
				.map(|(i, t)| t.kind.is_word() && (hyphenated(tokens, i) || contracted(t, src)))
				.collect(),
			headings: doc.iter_headings().filter_map(|h| h.span()).collect(),
		};
		let mut start = 0;
		for end in tokens
			.iter()
			.enumerate()
			.filter(|(_, t)| t.kind.is_sentence_terminator())
			.map(|(i, _)| i + 1)
			.chain([tokens.len()])
		{
			if end <= start {
				continue;
			}
			let words: Vec<usize> = (start..end).filter(|&i| tokens[i].kind.is_word()).collect();
			for k in 0..words.len() {
				if governed_participle(tokens, src, &tags.pos, &words, k) {
					tags.immune[words[k]] = true;
				}
				if let Some(p) = tags.retag(tokens, src, &words, k) {
					tags.pos[words[k]] = Some(p);
				}
			}
			start = end;
		}
		tags
	}

	pub(crate) fn pos(&self, i: usize) -> Option<UPOS> {
		self.pos[i]
	}

	pub(crate) fn immune(&self, i: usize) -> bool {
		self.immune[i]
	}

	/// Section titles are not sentences, and in the README case `readme_fw` generates them.
	pub(crate) fn in_heading(&self, span: Span<char>) -> bool {
		self.headings.iter().any(|h| h.contains(span.start))
	}

	/// Ordered: the first rule that matches owns the token, so no token is written twice and one
	/// left-to-right pass is enough. A rule may only move a token to a part of speech the curated
	/// dictionary already admits for it.
	fn retag(&self, tokens: &[Token], src: &[char], words: &[usize], k: usize) -> Option<UPOS> {
		use UPOS::*;
		let i = words[k];
		let meta = tokens[i].kind.as_word()?.as_ref()?;
		let seen = self.pos[i]?;
		let word = tokens[i].get_str(src).to_lowercase();
		let prev = k.checked_sub(1).and_then(|p| self.pos[words[p]]);
		let next = words.get(k + 1);

		if k == 0 && matches!(seen, ADJ | NOUN | PROPN) && meta.is_verb() && !words.iter().any(|&j| matches!(self.pos[j], Some(VERB | AUX))) {
			return Some(VERB);
		}
		if matches!(word.as_str(), "when" | "where") && seen == ADV && meta.is_conjunction() {
			return Some(SCONJ);
		}
		if matches!(word.as_str(), "most" | "less" | "only") && seen == ADV && meta.is_adjective() && matches!(next.and_then(|&n| self.pos[n]), Some(DET | NOUN | ADJ | NUM)) {
			return Some(ADJ);
		}
		// The right-hand test reads the dictionary rather than the current tag: that is what lets
		// `housing cover plate` resolve in one pass.
		if seen == VERB
			&& !meta.is_verb_progressive_form()
			&& meta.is_noun()
			&& matches!(prev, Some(NOUN | ADJ | DET | NUM))
			&& next.is_some_and(|&n| tokens[n].kind.as_word().and_then(|m| m.as_ref()).is_some_and(|m| m.is_noun()))
		{
			return Some(NOUN);
		}
		// STE permits an -ing word inside a Technical Name, and a gerund after a noun is one
		// (`channel watching`, `poll forwarding`). A progressive verb has an auxiliary in front of
		// it, so the left-hand test is also the governance test.
		if seen == VERB && meta.is_verb_progressive_form() && matches!(prev, Some(NOUN | ADJ | PROPN)) {
			return Some(NOUN);
		}
		None
	}
}

/// Half of a compound the tokenizer split (`Argument-less`, `1-fold`). Whitespace is its own
/// token, so adjacency in the token vector is adjacency in the source.
fn hyphenated(tokens: &[Token], i: usize) -> bool {
	let hyphen = |t: Option<&Token>| matches!(t.map(|t| &t.kind), Some(TokenKind::Punctuation(Punctuation::Hyphen)));
	hyphen(i.checked_sub(1).map(|p| &tokens[p])) || hyphen(tokens.get(i + 1))
}

/// A contraction. `contraction` already owns the span, and neither half of `won't` is a word the
/// wordset has a row for.
fn contracted(t: &Token, src: &[char]) -> bool {
	t.get_str(src).contains(['\'', '\u{2019}'])
}

/// A past participle under a form of `be`. `passive-voice` already owns the span.
fn governed_participle(tokens: &[Token], src: &[char], pos: &[Option<UPOS>], words: &[usize], k: usize) -> bool {
	let i = words[k];
	if pos[i] != Some(UPOS::VERB) || !tokens[i].kind.is_verb_past_participle_form() {
		return false;
	}
	let mut j = k;
	while j > 0 && NEGATION.contains(&tokens[words[j - 1]].get_str(src).to_lowercase().as_str()) {
		j -= 1;
	}
	j > 0 && pos[words[j - 1]] == Some(UPOS::AUX) && BE.contains(&tokens[words[j - 1]].get_str(src).to_lowercase().as_str())
}
