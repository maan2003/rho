//! ASD-STE100 is a whitelist: the approved words, plus the Technical Names and Technical Verbs
//! the project declares. openSTE ships both halves, so a word with no row at all is out of
//! vocabulary — that is `unknown-word`, and it is the arm that makes the check a whitelist.
use harper_brill::UPOS;
use harper_core::{
	Document, IrregularNouns, IrregularVerbs, Token,
	linting::{Lint, LintKind, Suggestion},
	spell::{Dictionary, FstDictionary},
};

use super::prose_words;
use crate::{
	ctx::Ctx,
	tags::Tags,
	wordset::{Entry, describe, equivalent, is_content},
};

/// Each wordset row restricts one (word, part of speech) pair, not the word: `work` is
/// unapproved as a verb and fine as a noun. Matching without the tag flags both.
pub fn unapproved_word(doc: &Document, tags: &Tags, ctx: &Ctx) -> Vec<Lint> {
	let src = doc.get_source();
	prose_words(doc, tags)
		.filter_map(|(i, t)| {
			let Verdict::Unapproved { word, row, used } = verdict(t, i, tags, ctx, src)? else {
				return None;
			};
			// An alternative equal to the word itself is how the wordset says "this word is
			// approved, but in another part of speech".
			let alternatives: Vec<&String> = ctx.wordset.alternatives(&row).iter().filter(|a| a.to_lowercase() != row).collect();
			// Some of the remaining ones are whole sentences of advice: worth reading, not applying.
			let replacements = alternatives.iter().filter(|a| !a.contains(' '));
			let named = match row == word {
				true => format!("`{word}`"),
				false => format!("`{word}` (`{row}`)"),
			};
			Some(Lint {
				span: t.span,
				lint_kind: LintKind::WordChoice,
				suggestions: replacements.map(|a| Suggestion::replace_with_match_case_str(a, t.span.get_content(src))).collect(),
				message: match alternatives.is_empty() {
					true => format!("{named} is not approved as {}; ASD-STE100 approves it only in another part of speech.", describe(used)),
					false => format!(
						"{named} is not approved as {}; use {}.",
						describe(used),
						alternatives.iter().map(|a| a.as_str()).collect::<Vec<_>>().join("; ")
					),
				},
				..Default::default()
			})
		})
		.collect()
}

pub fn wrong_pos(doc: &Document, tags: &Tags, ctx: &Ctx) -> Vec<Lint> {
	let src = doc.get_source();
	prose_words(doc, tags)
		.filter_map(|(i, t)| {
			let Verdict::WrongPos { word, approved, used } = verdict(t, i, tags, ctx, src)? else {
				return None;
			};
			Some(Lint {
				span: t.span,
				lint_kind: LintKind::Usage,
				message: format!(
					"`{word}` is approved only as {}, but is used here as {}. Add it to the glossary if it is a Technical Name or Technical Verb.",
					describe(approved),
					describe(used)
				),
				..Default::default()
			})
		})
		.collect()
}

pub fn unknown_word(doc: &Document, tags: &Tags, ctx: &Ctx) -> Vec<Lint> {
	unknown(doc, tags, ctx)
		.into_iter()
		.map(|(t, word, _, _)| Lint {
			span: t.span,
			lint_kind: LintKind::WordChoice,
			message: format!("`{word}` is not in the ASD-STE100 approved vocabulary. Add it to `docs/glossary.nix` if it is a Technical Name or Technical Verb."),
			..Default::default()
		})
		.collect()
}

/// The `--suggest-glossary` input: one entry per out-of-vocabulary occurrence.
pub(crate) fn unknown<'a>(doc: &'a Document, tags: &'a Tags, ctx: &Ctx) -> Vec<(&'a Token, String, String, UPOS)> {
	let src = doc.get_source();
	prose_words(doc, tags)
		.filter_map(|(i, t)| match verdict(t, i, tags, ctx, src)? {
			Verdict::Unknown { word, base, used } => Some((t, word, base, used)),
			_ => None,
		})
		.collect()
}

enum Verdict {
	Unapproved {
		word: String,
		row: String,
		used: UPOS,
	},
	WrongPos {
		word: String,
		approved: UPOS,
		used: UPOS,
	},
	/// `base` is the form to put in the glossary: one entry there covers every inflection.
	Unknown {
		word: String,
		base: String,
		used: UPOS,
	},
}

/// One walk of the cascade the three dictionary rules share. The arms are separate rules so that
/// `--disable unknown-word` can switch off the whitelist arm alone.
fn verdict(t: &Token, i: usize, tags: &Tags, ctx: &Ctx, src: &[char]) -> Option<Verdict> {
	if tags.immune(i) {
		return None;
	}
	let used = tags.pos(i)?;
	let word = t.get_str(src).to_lowercase();
	let lemma = lemma(t, &word);
	if ctx.glossary.allows(&word, used) || lemma.as_deref().is_some_and(|l| ctx.glossary.allows(l, used)) {
		return None;
	}

	let Some((row, entry)) = ctx.wordset.get(&word).map(|e| (word.clone(), e)).or_else(|| inflected_row(ctx, t, lemma.as_deref())) else {
		// openSTE carries exactly one row per word, and `work/VERB/unapproved` is how it says
		// *work is fine as a noun*. So a lemma row the form flags rejected still proves the word
		// is vocabulary: only this (word, part of speech) pair is undecided, and undecided is not
		// unknown.
		if lemma.as_ref().is_some_and(|l| ctx.wordset.get(l).is_some()) {
			return None;
		}
		// Only a form reduced to its base is evidence about the vocabulary: an inflection we could
		// not reduce says nothing, and openSTE has no inflections of its own to catch it. `chosen`
		// and `gone` are the residue Harper's two irregular tables do not cover.
		let inflected = inflected(t);
		if lemma.is_none() && inflected {
			return None;
		}
		// Only for an inflection. `derived_from` is derivational too, and a glossary entry for
		// `browse` would not be the word anyone wrote.
		let base = match inflected {
			true => lemma.unwrap_or_else(|| word.clone()),
			false => word.clone(),
		};
		// A proper noun, a numeral or a symbol is not a vocabulary violation.
		return is_content(used).then_some(Verdict::Unknown { word, base, used });
	};
	let approved_as = entry.pos?;
	match (entry.approved, equivalent(used, approved_as)) {
		(false, true) => Some(Verdict::Unapproved { word, row, used }),
		// Only on the surface row. An inflection's part of speech is its lemma's, so a form flag
		// that agrees with the row while the tagger does not is a disagreement about the form
		// (`monitoring` as a gerund), which `ing-verb` owns, not about the vocabulary.
		(true, false) if is_content(used) && row == word => Some(Verdict::WrongPos { word, approved: approved_as, used }),
		_ => None,
	}
}

fn inflected(t: &Token) -> bool {
	t.kind.is_verb_past_participle_form()
		|| t.kind
			.as_word()
			.and_then(|m| m.as_ref())
			.is_some_and(|m| m.is_verb_past_form() || m.is_verb_progressive_form() || m.is_verb_third_person_singular_present_form() || m.is_non_singular_noun())
}

/// Harper's `derived_from` is derivational as well as inflectional (`currently` → `current`), and
/// it only covers the regular affixes: `found` and `mice` are their own dictionary entries.
fn lemma(t: &Token, word: &str) -> Option<String> {
	let meta = t.kind.as_word()?.as_ref()?;
	let Some(id) = meta.derived_from else {
		let irregular = IrregularVerbs::curated().get_lemma_for_preterite(word).map(str::to_string);
		return irregular.or_else(|| IrregularNouns::curated().get_singular_for_plural(word).map(str::to_string));
	};
	Some(FstDictionary::curated().get_word_from_id(&id)?.iter().collect::<String>().to_lowercase())
}

/// openSTE has one row per lemma and no inflections, so a surface-form lookup is blind to most
/// of a real text. Because the derivation may cross parts of speech, the token's own form flags
/// have to agree with the row's before the row may speak for it.
fn inflected_row(ctx: &Ctx, t: &Token, lemma: Option<&str>) -> Option<(String, Entry)> {
	let meta = t.kind.as_word()?.as_ref()?;
	let lemma = lemma?;
	let entry = ctx.wordset.get(lemma)?;
	let agrees = match entry.pos? {
		UPOS::VERB => meta.is_verb_past_form() || meta.is_verb_progressive_form() || meta.is_verb_third_person_singular_present_form(),
		UPOS::NOUN => meta.is_non_singular_noun(),
		_ => false,
	};
	agrees.then_some((lemma.to_string(), entry))
}
