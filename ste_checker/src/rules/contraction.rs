use harper_core::{
	Document,
	linting::{Lint, LintKind},
};

use super::prose_words;
use crate::{ctx::Ctx, tags::Tags};

/// `'s` is only a contraction after these; everywhere else it is a possessive.
const PRONOUN_STEMS: &[&str] = &["it", "he", "she", "that", "this", "there", "here", "what", "who", "let"];

pub fn contraction(doc: &Document, tags: &Tags, _ctx: &Ctx) -> Vec<Lint> {
	let src = doc.get_source();
	prose_words(doc, tags)
		.filter_map(|(_, t)| {
			let word = t.get_str(src);
			let (stem, suffix) = word.split_once(['\'', '\u{2019}'])?;
			let contracted = match suffix.to_lowercase().as_str() {
				"t" | "re" | "ve" | "ll" | "d" | "m" => true,
				"s" => PRONOUN_STEMS.contains(&stem.to_lowercase().as_str()),
				_ => false,
			};
			contracted.then(|| Lint {
				span: t.span,
				lint_kind: LintKind::Style,
				message: format!("`{word}` is a contraction. ASD-STE100 wants the words written out in full."),
				..Default::default()
			})
		})
		.collect()
}
