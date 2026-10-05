use harper_core::{Document, Token, linting::Lint};

use crate::{ctx::Ctx, tags::Tags};

pub mod contraction;
pub mod dictionary;
pub mod noun_cluster;
pub mod sentence;
pub mod verb_form;

pub type Rule = fn(&Document, &Tags, &Ctx) -> Vec<Lint>;

pub const RULES: &[(&str, Rule)] = &[
	("unapproved-word", dictionary::unapproved_word),
	("wrong-pos", dictionary::wrong_pos),
	("unknown-word", dictionary::unknown_word),
	("sentence-length", sentence::length),
	("noun-cluster", noun_cluster::cluster),
	("passive-voice", verb_form::passive),
	("compound-tense", verb_form::compound_tense),
	("ing-verb", verb_form::ing_as_verb),
	("contraction", contraction::contraction),
];

/// Word tokens outside headings, with their index into `doc.get_tokens()` — which is what
/// [`Tags`] is keyed by.
pub(crate) fn prose_words<'a>(doc: &'a Document, tags: &'a Tags) -> impl Iterator<Item = (usize, &'a Token)> {
	doc.get_tokens().iter().enumerate().filter(move |(_, t)| t.kind.is_word() && !tags.in_heading(t.span))
}
