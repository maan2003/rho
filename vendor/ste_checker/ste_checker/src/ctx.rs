use crate::{
	config::AppConfig,
	glossary::Glossary,
	wordset::{WORDSET, Wordset},
};

/// Everything the rules read besides the document itself.
pub struct Ctx {
	pub wordset: &'static Wordset,
	/// Per-project Technical Names and Technical Verbs. ASD-STE100 is a whitelist, so this is
	/// what keeps a repo's own vocabulary out of `unknown-word`.
	pub glossary: Glossary,
	pub config: AppConfig,
}

impl Ctx {
	pub fn new(config: AppConfig, glossary: Glossary) -> Self {
		Self {
			wordset: &WORDSET,
			glossary,
			config,
		}
	}

	pub fn rule_enabled(&self, rule: &str) -> bool {
		!self.config.disable.iter().any(|d| d == rule)
	}
}
