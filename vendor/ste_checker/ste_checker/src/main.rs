use std::{
	collections::{BTreeMap, HashSet},
	path::{Path, PathBuf},
};

use clap::Parser;
use color_eyre::eyre::{Result, WrapErr, bail};
use harper_brill::UPOS;
use ste_checker::{
	Ctx,
	config::{AppConfig, TextType},
	glossary::Glossary,
	report,
};

const DEFAULT_GLOSSARY: &str = "docs/glossary.nix";

const SCOPE: &str = "\
Checks Markdown against the procedurally decidable part of ASD-STE100: the approved
wordlist, part-of-speech restrictions, sentence length, noun clusters, passive voice,
compound tenses, -ing forms and contractions. The other ~43 rules of the standard are
semantic and are not checked.

Findings are warnings; the exit code stays 0 unless --deny is given. Neither ASD nor
STEMG endorse this tool, and it certifies nothing.

ASD-STE100 is a whitelist: a word that is neither approved nor declared by the project
is reported as unknown-word. Words your project owns (Technical Names and Technical
Verbs) belong in docs/glossary.nix, which --suggest-glossary writes a skeleton of.";

#[derive(Parser)]
#[command(author, version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"), about, long_about = SCOPE)]
struct Cli {
	/// Markdown files to check. Pass them all at once: model load dominates per-file cost.
	#[arg(required = true)]
	paths: Vec<PathBuf>,
	/// Exit 1 when anything is found.
	#[arg(long)]
	deny: bool,
	#[arg(long, value_enum, default_value_t = Format::Human)]
	format: Format,
	/// Technical Names and Technical Verbs. Defaults to `docs/glossary.nix` when present, except under --suggest-glossary.
	#[arg(long)]
	glossary: Option<PathBuf>,
	/// Print a `docs/glossary.nix` skeleton of every out-of-vocabulary word, instead of checking.
	#[arg(long)]
	suggest_glossary: bool,
	/// Descriptive text is allowed longer sentences than a procedure.
	#[arg(long, value_enum, default_value_t = TextType::Procedure)]
	text_type: TextType,
	/// Rule names to switch off entirely.
	#[arg(long)]
	disable: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
enum Format {
	Human,
	Json,
}

fn main() -> Result<()> {
	color_eyre::install()?;
	let cli = Cli::parse();
	for rule in &cli.disable {
		if !ste_checker::rule_names().any(|r| r == rule) {
			bail!("`{rule}` is not a rule; known rules: {}", ste_checker::rule_names().collect::<Vec<_>>().join(", "));
		}
	}
	let config = AppConfig {
		text_type: cli.text_type,
		disable: cli.disable,
	};

	let glossary = match &cli.glossary {
		Some(path) => Glossary::read(path)?,
		// `--suggest-glossary > docs/glossary.nix` is the documented bootstrap, and the shell
		// truncates the file before this process starts. Reading the file being written is the
		// one place the default must not apply; `--glossary` still asks for it explicitly.
		None => {
			let path = Path::new(DEFAULT_GLOSSARY);
			match path.exists() && !cli.suggest_glossary {
				true => Glossary::read(path)?,
				false => Glossary::default(),
			}
		}
	};
	let ctx = Ctx::new(config, glossary);

	let read = |path: &PathBuf| std::fs::read_to_string(path).wrap_err_with(|| format!("failed to read {}", path.display()));
	if cli.suggest_glossary {
		let mut counts: BTreeMap<(bool, String), usize> = BTreeMap::new();
		let mut in_code = HashSet::new();
		for path in &cli.paths {
			let found = ste_checker::vocabulary(&read(path)?, &ctx);
			for (word, pos) in found.unknown {
				*counts.entry((pos == UPOS::VERB, word)).or_default() += 1;
			}
			in_code.extend(found.in_code);
		}
		print!("{}", skeleton(&counts, &in_code));
		return Ok(());
	}

	let mut json = Vec::new();
	let mut found = 0usize;
	for path in &cli.paths {
		let display = path.display().to_string();
		let text = read(path)?;
		let findings = ste_checker::check(&text, &ctx);
		found += findings.len();
		match cli.format {
			Format::Human =>
				if let Some(report) = report::human(&display, &text, &findings) {
					eprintln!("{report:?}");
				},
			Format::Json => json.extend(report::json(&display, &text, &findings).into_iter().map(|f| serde_json::to_value(f).expect("plain data"))),
		}
	}
	if cli.format == Format::Json {
		println!("{}", serde_json::to_string_pretty(&json)?);
	}

	if cli.deny && found > 0 {
		std::process::exit(1);
	}
	Ok(())
}

/// Every out-of-vocabulary word, for the human to delete from. Deliberately unfiltered: the
/// in-code signal is too weak to decide with, and pre-filtering on it would bury the
/// `browser`/`cache`/`command` class that is the point of the whitelist.
fn skeleton(counts: &BTreeMap<(bool, String), usize>, in_code: &HashSet<String>) -> String {
	let mut out = String::from(
		"# Written by `ste_checker --suggest-glossary`. Every word below is absent from the\n\
		 # ASD-STE100 approved vocabulary. Delete the ones that are ordinary English — only\n\
		 # Technical Names and Technical Verbs belong here — and define the ones you keep.\n{\n",
	);
	for (list, verbs) in [("names", false), ("verbs", true)] {
		let mut words: Vec<(&String, usize)> = counts.iter().filter(|((v, _), _)| *v == verbs).map(|((_, w), n)| (w, *n)).collect();
		words.sort_by_key(|(w, n)| (std::cmp::Reverse(*n), *w));
		let width = words.iter().map(|(w, _)| w.len()).max().unwrap_or(0);
		out.push_str(&format!("  {list} = [\n"));
		for (word, count) in words {
			let entry = format!("{{ name = \"{word}\"; desc = \"\"; }}");
			let code = match in_code.contains(word) {
				true => ", in code",
				false => "",
			};
			out.push_str(&format!("    {entry:<pad$}  # {count}{code}\n", pad = width + 25));
		}
		out.push_str("  ];\n");
	}
	out.push_str("}\n");
	out
}
