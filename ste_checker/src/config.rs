#[derive(Clone, Debug, Default)]
pub struct AppConfig {
	pub text_type: TextType,
	/// Rule names to switch off entirely; `--help` lists them.
	pub disable: Vec<String>,
}

/// ASD-STE100 allows longer sentences in descriptive text than in procedures.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum TextType {
	#[default]
	#[value(alias = "procedural")]
	Procedure,
	#[value(alias = "descriptive")]
	Description,
}

impl TextType {
	pub fn max_sentence_words(self) -> usize {
		match self {
			Self::Procedure => 20,
			Self::Description => 25,
		}
	}
}
