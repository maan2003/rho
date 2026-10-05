use std::{collections::HashMap, sync::LazyLock};

use harper_brill::UPOS;
use serde::{Deserialize, Deserializer};

const RAW: &str = include_str!("../vendor/openste.json");

pub static WORDSET: LazyLock<Wordset> = LazyLock::new(Wordset::load);

pub struct Wordset {
	entries: HashMap<String, Entry>,
	alternatives: HashMap<String, Vec<String>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Entry {
	pub approved: bool,
	/// `None` for the handful of upstream rows that carry no part of speech.
	pub pos: Option<UPOS>,
}

impl Wordset {
	fn load() -> Self {
		let raw: RawWordset = serde_json::from_str(RAW).expect("vendored wordset is valid JSON of the expected shape");
		assert!(raw.words.len() > 1900, "openste.json shrank to {} entries; upstream shape changed", raw.words.len());

		let entries = raw
			.words
			.iter()
			.map(|w| {
				(
					w.title.to_lowercase(),
					Entry {
						approved: w.wordstatus == "approved",
						pos: w.spacypos,
					},
				)
			})
			.collect();
		let mut alternatives: HashMap<String, Vec<String>> = HashMap::new();
		for a in &raw.alternatives {
			alternatives.entry(a.title.to_lowercase()).or_default().push(a.alt_title.clone());
		}
		Self { entries, alternatives }
	}

	pub fn get(&self, word: &str) -> Option<Entry> {
		self.entries.get(word).copied()
	}

	pub fn alternatives(&self, word: &str) -> &[String] {
		self.alternatives.get(word).map_or(&[], Vec::as_slice)
	}
}

/// STE's parts of speech are coarser than Universal Dependencies': modals count as verbs,
/// possessives as adjectives, and it splits neither particles nor subordinators out. Each pair is
/// a claim about the two tagsets, which is why `tags.rs` does not replace it — the cascade
/// normalises the tag a rule sees, this maps the categories.
pub fn equivalent(seen: UPOS, approved: UPOS) -> bool {
	use UPOS::*;
	seen == approved
		|| matches!(
			(seen, approved),
			(AUX, VERB) | (PART, ADV) | (PART, ADP) | (PRON, ADJ) | (PRON, DET) | (DET, PRON) | (SCONJ, ADP) | (SCONJ, CCONJ) | (PROPN, NOUN)
		)
}

/// Function words are where the tagger and STE disagree most and matter least.
pub fn is_content(pos: UPOS) -> bool {
	matches!(pos, UPOS::NOUN | UPOS::VERB | UPOS::ADJ | UPOS::ADV)
}

/// Carries its own article, so messages read as English.
pub fn describe(pos: UPOS) -> &'static str {
	use UPOS::*;
	match pos {
		ADJ => "an adjective",
		ADP => "a preposition",
		ADV => "an adverb",
		AUX => "an auxiliary verb",
		CCONJ | SCONJ => "a conjunction",
		DET => "a determiner",
		INTJ => "an interjection",
		NOUN => "a noun",
		NUM => "a numeral",
		PART => "a particle",
		PRON => "a pronoun",
		PROPN => "a proper noun",
		PUNCT => "a punctuation mark",
		SYM => "a symbol",
		VERB => "a verb",
	}
}

#[derive(Deserialize)]
struct RawWordset {
	words: Vec<RawWord>,
	alternatives: Vec<RawAlt>,
}

#[derive(Deserialize)]
struct RawWord {
	title: String,
	wordstatus: String,
	#[serde(deserialize_with = "upos_or_blank")]
	spacypos: Option<UPOS>,
}

#[derive(Deserialize)]
struct RawAlt {
	title: String,
	alt_title: String,
}

fn upos_or_blank<'de, D: Deserializer<'de>>(d: D) -> Result<Option<UPOS>, D::Error> {
	let s = String::deserialize(d)?;
	if s.is_empty() {
		return Ok(None);
	}
	UPOS::deserialize(serde::de::value::StrDeserializer::new(s.as_str())).map(Some)
}
