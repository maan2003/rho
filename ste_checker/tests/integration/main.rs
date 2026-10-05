//! The single integration binary, per <https://matklad.github.io/2021/02/27/delete-cargo-integration-tests.html>.
//!
//! `tests/corpus/` is the text this checker was calibrated against; see its README. The counts
//! below are a snapshot, not a target: any change to the wordset, the POS-equivalence table or a
//! rule moves them, and that movement is the regression signal.
use std::{collections::HashSet, path::Path};

use ste_checker::{Ctx, config::AppConfig, glossary::Glossary};

const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");
const GLOSSARY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus.glossary.nix");
const TRUTH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus.truth");

#[test]
fn corpus_calibration() {
	let counts = corpus_counts(Glossary::default());
	assert_eq!(
		counts,
		vec![
			("unapproved-word", 381),
			("wrong-pos", 113),
			("unknown-word", 1156),
			("sentence-length", 74),
			("noun-cluster", 3),
			("passive-voice", 40),
			("compound-tense", 3),
			("ing-verb", 58),
			("contraction", 10),
		]
	);
}

/// What a project's own Technical Names and Technical Verbs buy back. The remainder is real
/// ASD-STE100 debt in the corpus, so `--deny` still exits non-zero over it.
#[test]
fn glossary_absorbs_technical_vocabulary() {
	let counts = corpus_counts(Glossary::read(Path::new(GLOSSARY)).unwrap());
	assert_eq!(
		counts,
		vec![
			("unapproved-word", 370),
			("wrong-pos", 90),
			("unknown-word", 978),
			("sentence-length", 74),
			("noun-cluster", 3),
			("passive-voice", 40),
			("compound-tense", 3),
			("ing-verb", 58),
			("contraction", 10),
		]
	);
}

/// The headline tagger error: `cover` reads as a verb, which both invents a finding and splits
/// the nominal phrase the cluster rule used to be counted over.
#[test]
fn noun_cluster_survives_a_mistagged_stack() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	let stack = "The main landing gear housing cover plate assembly bolt is loose.";
	assert_eq!(flagged(stack, "noun-cluster", &ctx).len(), 1);
	assert!(!flagged(stack, "unapproved-word", &ctx).contains(&"cover".to_string()));
	assert_eq!(flagged("Remove the wing tip fuel tank access panel.", "noun-cluster", &ctx).len(), 1);
}

/// Every one of these was a surviving `wrong-pos` finding on the corpus, and none of them is a
/// writing fault — the tagger and openSTE disagree about the tagset, not about the sentence.
#[test]
fn tagger_artifacts_are_not_wrong_pos() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	for text in [
		"Standard logic that most providers share.",
		"When used as a lib, import with the toml.",
		"Delete the temporary files when the option is enabled.",
		"Auto-fix violations where possible.",
		"Run it on your local machine (where you can open a browser).",
		"Only a subset of rules know how to audit.",
		"Complete the OAuth authentication flow.",
	] {
		assert_eq!(flagged(text, "wrong-pos", &ctx), Vec::<String>::new(), "{text}");
	}
	assert_eq!(flagged("The report is a manual.", "wrong-pos", &ctx), vec!["manual"]);
}

/// The tokenizer splits hyphenated compounds, and the halves are not words anyone wrote.
#[test]
fn hyphen_compounds_do_not_reach_the_dictionary_rules() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	assert_eq!(flagged("Argument-less pub fn new is the rule.", "wrong-pos", &ctx), Vec::<String>::new());
	assert_eq!(flagged("Wrap impl blocks in vim 1-fold markers.", "wrong-pos", &ctx), Vec::<String>::new());
	// The price of the rule: `well` is an unapproved adverb, and immunity cannot see that.
	assert_eq!(flagged("This is a well-known problem.", "unapproved-word", &ctx), Vec::<String>::new());
	assert_eq!(flagged("The system runs well.", "unapproved-word", &ctx), vec!["well"]);
}

/// `passive-voice` owns the span; the dictionary rules would only say the same thing again.
#[test]
fn passive_participle_is_reported_once() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	let rules: Vec<&str> = ste_checker::check("Groups are specified by the user.", &ctx).iter().map(|f| f.rule).collect();
	assert_eq!(rules, vec!["passive-voice"]);
}

/// A sentence-initial word with a verb reading is only an imperative when nothing else in the
/// clause is finite.
#[test]
fn imperative_does_not_fire_on_a_finite_clause() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	assert_eq!(flagged("Complete the OAuth authentication flow.", "wrong-pos", &ctx), Vec::<String>::new());
	assert_eq!(flagged("Complete documentation is required.", "wrong-pos", &ctx), vec!["Complete"]);
}

/// openSTE has one row per lemma and no inflections, so the surface form alone is blind to most
/// of a real text.
#[test]
fn inflections_inherit_their_lemma_row() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	assert_eq!(flagged("The script requires a token.", "unapproved-word", &ctx), vec!["requires"]);
	// `quicker` derives from `quick`, an unapproved adjective, and is used as one — only the form
	// flags say it is a comparative rather than the row itself.
	assert!(!flagged("The build is quicker now.", "unapproved-word", &ctx).contains(&"quicker".to_string()));
	let glossed = Ctx::new(AppConfig::default(), verbs(&["require"]));
	assert_eq!(flagged("The script requires a token.", "unapproved-word", &glossed), Vec::<String>::new());
}

/// The case the standard is built around, and the one that needs a live POS tagger: the same
/// spelling is approved in one part of speech and not in another.
#[test]
fn part_of_speech_decides_approval() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	let flagged = |text: &str| ste_checker::check(text, &ctx).iter().any(|f| f.rule == "unapproved-word");
	assert!(flagged("Do not work the lever."));
	assert!(!flagged("The work is done."));
}

/// ASD-STE100 is a whitelist. Control: `work` has a row, so it is vocabulary whichever part of
/// speech it is used in.
#[test]
fn unknown_words_are_reported() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	assert_eq!(flagged("The browser opens a socket.", "unknown-word", &ctx), vec!["browser"]);
	assert_eq!(flagged("Do the work.", "unknown-word", &ctx), Vec::<String>::new());
	assert_eq!(
		flagged("The browser opens a socket.", "unknown-word", &Ctx::new(AppConfig::default(), names(&["browser"]))),
		Vec::<String>::new()
	);
}

/// openSTE carries exactly one row per word, and `work/VERB/unapproved` is how it says *work is
/// fine as a noun*. Reading the absence of a matching row as absence of the word would flag it.
#[test]
fn a_row_in_another_part_of_speech_is_not_unknown() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	assert_eq!(flagged("The work is done.", "unknown-word", &ctx), Vec::<String>::new());
	assert_eq!(flagged("The report is a manual.", "unknown-word", &ctx), Vec::<String>::new());
}

/// The whitelist arm is only as good as the lemmatizer under it, and `derived_from` covers the
/// regular affixes only. Every verb here has an approved lemma in openSTE.
#[test]
fn irregular_inflections_are_not_unknown() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	for text in [
		"The tool has the file.",
		"The tool found the file.",
		"The user broke it.",
		"The user wrote the file.",
		"We have chosen the option.",
		"The men saw it.",
	] {
		assert_eq!(flagged(text, "unknown-word", &ctx), Vec::<String>::new(), "{text}");
	}
}

/// A Technical Name is approved as a noun and nothing else. A flat glossary would mute the verb
/// reading too, which is the reading STE rejects.
#[test]
fn a_technical_name_is_not_a_technical_verb() {
	// openSTE approves `check` as a noun only, so the imperative is a `wrong-pos` the glossary
	// answers — as a Technical Verb, and only as a Technical Verb.
	assert_eq!(flagged("Check the output.", "wrong-pos", &Ctx::new(AppConfig::default(), Glossary::default())), vec!["Check"]);
	assert_eq!(flagged("Check the output.", "wrong-pos", &Ctx::new(AppConfig::default(), names(&["check"]))), vec!["Check"]);
	assert_eq!(
		flagged("Check the output.", "wrong-pos", &Ctx::new(AppConfig::default(), verbs(&["check"]))),
		Vec::<String>::new()
	);
}

/// Neither half of `won't` is a word openSTE has a row for, so without the immunize rule the
/// whitelist arm reports every contraction a second time.
#[test]
fn contractions_are_reported_once() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	let rules: Vec<&str> = ste_checker::check("You can't do that.", &ctx).iter().map(|f| f.rule).collect();
	assert_eq!(rules, vec!["contraction"]);
}

/// Harper counts characters; miette and editors count bytes.
#[test]
fn reported_offsets_index_bytes() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	let text = "Ünicode — do not work the lever.\n";
	let findings = ste_checker::check(text, &ctx);
	let reported = ste_checker::report::json("t.md", text, &findings);
	let word = reported.iter().find(|f| f.rule == "unapproved-word").expect("`work` as a verb is unapproved");
	assert_eq!(&text[word.start..word.end], "work");
	assert!(ste_checker::report::human("t.md", text, &findings).is_some());
}

/// STE allows -ing inside a Technical Name, and a gerund after a noun is one. Silently eating
/// `ing-verb` hits is what the progressive guard on the noun-run rule exists to prevent, so the
/// three findings this rule takes off the corpus are named here.
#[test]
fn gerund_after_a_noun_is_a_technical_noun() {
	let ctx = Ctx::new(AppConfig::default(), Glossary::default());
	for taken in ["Telegram channel watching is enabled.", "The poll info forwarding is on.", "It needs manual fixing."] {
		assert_eq!(flagged(taken, "ing-verb", &ctx), Vec::<String>::new(), "{taken}");
	}
	// A progressive verb has its auxiliary in front of it, so it never matches.
	assert_eq!(flagged("It is watching the channel.", "ing-verb", &ctx), vec!["watching"]);
	assert_eq!(flagged("The model is chosen by choosing a tier.", "ing-verb", &ctx), vec!["choosing"]);
}

/// Both numbers, printed and floored rather than targeted. A change that trades one for the other
/// has to be visible; netting out to the same total is the failure mode this replaces.
#[test]
fn precision_and_recall() {
	let truth: HashSet<(String, usize, usize, String)> = std::fs::read_to_string(TRUTH)
		.unwrap()
		.lines()
		.filter(|l| !l.starts_with('#') && !l.trim().is_empty())
		.map(|l| {
			let mut field = l.split('\t');
			let mut next = || field.next().expect("four tab-separated fields per row").to_string();
			(next(), next().parse().unwrap(), next().parse().unwrap(), next())
		})
		.collect();

	let ctx = Ctx::new(AppConfig::default(), Glossary::read(Path::new(GLOSSARY)).unwrap());
	let mut found = HashSet::new();
	for file in truth.iter().map(|(f, ..)| f.clone()).collect::<HashSet<_>>() {
		let text = std::fs::read_to_string(format!("{CORPUS}/{file}")).unwrap();
		let findings = ste_checker::check(&text, &ctx);
		found.extend(
			ste_checker::report::json(&file, &text, &findings)
				.into_iter()
				.map(|f| (file.clone(), f.start, f.end, f.rule.to_string())),
		);
	}
	// `tedi__usage.md` is entirely a code fence: it carries no rows, so it is invisible above and
	// has to be checked for silence by name.
	assert!(ste_checker::check(&std::fs::read_to_string(format!("{CORPUS}/tedi__usage.md")).unwrap(), &ctx).is_empty());

	let hits = found.intersection(&truth).count();
	let (precision, recall) = (hits as f64 / found.len() as f64, hits as f64 / truth.len() as f64);
	println!("precision {precision:.3} ({hits}/{}), recall {recall:.3} ({hits}/{})", found.len(), truth.len());
	for missed in truth.difference(&found) {
		println!("  missed {missed:?}");
	}
	for spurious in found.difference(&truth) {
		println!("  spurious {spurious:?}");
	}
	assert!(precision >= 0.90 && recall >= 0.90, "precision {precision:.3}, recall {recall:.3}");
}

/// The three entry forms mean the same thing, and anything outside the data subset of Nix is an
/// error rather than a partial parse.
#[test]
fn glossary_nix_round_trips() {
	let parsed = Glossary::parse(
		"# a comment\n{\n  names = [ \"server\"   { name = \"OAuth\"; }\n\t{ name = \"telegram\"; desc = \"the bot API\"; } ]; # trailing\n  verbs = [{name=\"parse\";}];\n}\n",
	)
	.unwrap();
	assert_eq!(parsed.names.get("server"), Some(&None));
	assert_eq!(parsed.names.get("oauth"), Some(&None));
	assert_eq!(parsed.names.get("telegram"), Some(&Some("the bot API".to_string())));
	assert_eq!(parsed.verbs.keys().collect::<Vec<_>>(), vec!["parse"]);

	for rejected in [
		"let x = 1; in { names = []; verbs = []; }",
		"import ./other.nix",
		"{ inputs }: { names = []; verbs = []; }",
		"{ names = [ \"${x}\" ]; verbs = []; }",
		"{ nouns = [ \"server\" ]; }",
		"{ names = [ { desc = \"no name\"; } ]; }",
		"{ names = [ \"unterminated ]; }",
	] {
		assert!(Glossary::parse(rejected).is_err(), "{rejected}");
	}
}

/// The bootstrap loop: the first run in a repo with no glossary has to produce a file the next
/// run can read.
#[test]
fn suggest_glossary_emits_parseable_nix() {
	let out = std::process::Command::new(env!("CARGO_BIN_EXE_ste_checker"))
		.arg("--suggest-glossary")
		.args(corpus_paths())
		.output()
		.unwrap();
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	let skeleton = Glossary::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap();
	assert!(skeleton.names.contains_key("browser") && skeleton.verbs.contains_key("forbid"));
	assert_eq!(skeleton.names.get("browser"), Some(&Some(String::new())), "every entry leaves `desc` for the human");
}

/// `--suggest-glossary > docs/glossary.nix` is the documented bootstrap, and the shell truncates
/// the file before the process starts: the default glossary must not be read on that path.
#[test]
fn suggest_glossary_ignores_the_file_it_writes() {
	let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("redirected");
	std::fs::create_dir_all(dir.join("docs")).unwrap();
	std::fs::write(dir.join("docs/glossary.nix"), "").unwrap();
	std::fs::write(dir.join("in.md"), "Restart the browser.\n").unwrap();
	let out = std::process::Command::new(env!("CARGO_BIN_EXE_ste_checker"))
		.current_dir(&dir)
		.args(["--suggest-glossary", "in.md"])
		.output()
		.unwrap();
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	assert!(Glossary::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap().names.contains_key("browser"));
}

/// A glossary of Technical Names only, for the tests that need one.
fn names(words: &[&str]) -> Glossary {
	Glossary {
		names: words.iter().map(|w| (w.to_string(), None)).collect(),
		..Default::default()
	}
}

/// A glossary of Technical Verbs only, for the tests that need one.
fn verbs(words: &[&str]) -> Glossary {
	Glossary {
		verbs: words.iter().map(|w| (w.to_string(), None)).collect(),
		..Default::default()
	}
}

/// The words `rule` fired on, in source order. Spans are char indices into `text`.
fn flagged(text: &str, rule: &str, ctx: &Ctx) -> Vec<String> {
	let chars: Vec<char> = text.chars().collect();
	ste_checker::check(text, ctx)
		.iter()
		.filter(|f| f.rule == rule)
		.map(|f| chars[f.lint.span.start..f.lint.span.end].iter().collect())
		.collect()
}

fn corpus_paths() -> Vec<std::path::PathBuf> {
	let mut paths: Vec<_> = std::fs::read_dir(CORPUS).unwrap().map(|e| e.unwrap().path()).filter(|p| p.file_name() != Some("README.md".as_ref())).collect();
	paths.sort();
	paths
}

fn corpus_counts(glossary: Glossary) -> Vec<(&'static str, usize)> {
	let ctx = Ctx::new(AppConfig::default(), glossary);
	let paths = corpus_paths();
	assert_eq!(paths.len(), 15, "corpus changed size; recalibrate before touching these numbers");

	let mut counts: Vec<(&'static str, usize)> = ste_checker::rule_names().map(|r| (r, 0)).collect();
	for path in paths {
		for finding in ste_checker::check(&std::fs::read_to_string(&path).unwrap(), &ctx) {
			counts.iter_mut().find(|(rule, _)| *rule == finding.rule).expect("rule names come from the same table").1 += 1;
		}
	}
	counts
}
