//! `:`: every command by name or by what the user means
//! (`window.Commands`). Places and remembered words are never here; they
//! are `space f`'s (`window.Places`).
//!
//! A line is the command's words, then, for a command that takes one, its
//! argument: `model sol-medium` runs `set agent runtime` on `sol-medium`
//! in one shot. Words that name a command outright (its name, word by
//! word or abbreviated, or one of its aliases) end where its argument
//! begins; anything less is a search for a command.

use std::cmp::Reverse;
use std::ops::Range;

use memchr::memmem::Finder;

use crate::command::{Command, NAME_WEIGHT, Spelling};
use crate::matching::{Folded, best_hit, merge_marks, ordered_starts};

/// How surely the typed words mean a command, best first. A name beats an
/// alias at the same reach, but an alias said outright beats a name the
/// words only touch the middle of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// The whole name, or the name word by word with the argument after.
    Name,
    /// The start of the name: `set ag`.
    NamePrefix,
    /// An alias, whole or begun: `kill`, `mod`.
    Alias,
    /// The starts of the name's words, in order: `ag run`.
    NameWords,
    AliasWords,
    /// Somewhere inside the name.
    InName,
    InAlias,
}

pub(crate) struct Entry {
    pub command: Command,
    /// The keys that run it where the user is, resolved.
    pub keys: String,
    /// Its place among the commands the user ran last, most recent first.
    recent: Option<usize>,
}

struct Words {
    name: Folded,
    name_words: Vec<String>,
    aliases: Vec<(String, Vec<String>, u8)>,
}

impl Words {
    fn new(command: &Command) -> Self {
        let name = Folded::new(command.name());
        let name_words = name.text.split_whitespace().map(str::to_owned).collect();
        Self {
            name,
            name_words,
            aliases: command
                .spelling()
                .aliases
                .iter()
                .map(|(alias, weight)| {
                    let alias = alias.to_lowercase();
                    let words = alias.split_whitespace().map(str::to_owned).collect();
                    (alias, words, *weight)
                })
                .collect(),
        }
    }
}

/// A command the line picks, and the argument the line gives it.
pub(crate) struct Matched {
    pub index: usize,
    pub argument: Option<String>,
    /// Display bytes of the name the words matched.
    pub marks: Vec<Range<usize>>,
    tier: Tier,
    weight: u8,
}

pub(crate) struct Listing {
    pub entries: Vec<Entry>,
    words: Vec<Words>,
}

/// What `Commands` lists for `input` among commands spelled `spellings`,
/// best first, each with the argument the line gives it. Every system
/// exports its spellings, so one test can rank them all together without
/// the systems behind them.
pub fn rank_spellings(
    spellings: &[&'static Spelling],
    input: &str,
) -> Vec<(&'static str, Option<String>)> {
    let listing = Listing::new(
        spellings
            .iter()
            .map(|spelling| (Command::new(spelling, |_, _, _, _| {}), String::new()))
            .collect(),
        &[],
    );
    listing
        .rank(input)
        .into_iter()
        .map(|matched| (spellings[matched.index].name, matched.argument))
        .collect()
}

/// How `typed` names `words` outright, if it does: as many words, each
/// the start of its own (`set proj run` for `set project runtime`).
/// `Some(true)` when every word is whole.
fn names(typed: &[String], words: &[String]) -> Option<bool> {
    (typed.len() == words.len()
        && typed
            .iter()
            .zip(words)
            .all(|(typed, word)| word.starts_with(typed.as_str())))
    .then(|| typed == words)
}

/// How `terms` reach `text`, whose words are `words`: the whole of it, its
/// start, the starts of its words in order, or anywhere inside.
fn reach(text: &str, terms: &[String]) -> Option<Reach> {
    let joined = terms.join(" ");
    if text == joined {
        return Some(Reach::Whole);
    }
    if text.starts_with(&joined) {
        return Some(Reach::Start);
    }
    let finders: Vec<_> = terms.iter().map(Finder::new).collect();
    if ordered_starts(text, &finders) {
        return Some(Reach::WordStarts);
    }
    finders
        .iter()
        .all(|finder| finder.find(text.as_bytes()).is_some())
        .then_some(Reach::Inside)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    Whole,
    Start,
    WordStarts,
    Inside,
}

impl Listing {
    /// `recent` names the commands the user ran last, most recent first.
    pub fn new(commands: Vec<(Command, String)>, recent: &[String]) -> Self {
        let entries: Vec<Entry> = commands
            .into_iter()
            .map(|(command, keys)| Entry {
                recent: recent.iter().position(|name| name == command.name()),
                command,
                keys,
            })
            .collect();
        let words = entries
            .iter()
            .map(|entry| Words::new(&entry.command))
            .collect();
        Self { entries, words }
    }

    /// The commands the line means, best first. Nothing typed lists the
    /// commands run last, then every other command by name.
    pub fn rank(&self, input: &str) -> Vec<Matched> {
        let tokens: Vec<(usize, &str)> = input
            .split_whitespace()
            .map(|token| (token.as_ptr() as usize - input.as_ptr() as usize, token))
            .collect();
        if tokens.is_empty() {
            let mut all: Vec<_> = (0..self.entries.len()).collect();
            all.sort_by_key(|&index| {
                let entry = &self.entries[index];
                (entry.recent.unwrap_or(usize::MAX), entry.command.name())
            });
            return all
                .into_iter()
                .map(|index| Matched {
                    index,
                    argument: None,
                    marks: Vec::new(),
                    tier: Tier::Name,
                    weight: NAME_WEIGHT,
                })
                .collect();
        }
        let terms: Vec<String> = tokens
            .iter()
            .map(|(_, token)| token.to_lowercase())
            .collect();
        let mut rows: Vec<Matched> = self
            .words
            .iter()
            .enumerate()
            .filter_map(|(index, words)| {
                let takes = self.entries[index].command.spelling().argument.is_some();
                let searched =
                    Self::searched(words, &terms).map(|(tier, weight)| (tier, weight, None));
                // The most words that name it outright, then the argument.
                // Abbreviated words name it less surely than whole ones,
                // so `re ca` is a search before it is `rename` on `ca`.
                let named = (takes && terms.len() > 1)
                    .then(|| {
                        (1..terms.len()).rev().find_map(|used| {
                            let typed = &terms[..used];
                            let (tier, weight) = match names(typed, &words.name_words) {
                                Some(true) => (Tier::Name, NAME_WEIGHT),
                                Some(false) => (Tier::NameWords, NAME_WEIGHT),
                                None => words
                                    .aliases
                                    .iter()
                                    .filter_map(|(_, alias, weight)| {
                                        let tier = match names(typed, alias)? {
                                            true => Tier::Alias,
                                            false => Tier::AliasWords,
                                        };
                                        Some((tier, *weight))
                                    })
                                    .min_by_key(|(tier, weight)| (*tier, Reverse(*weight)))?,
                            };
                            let argument = input[tokens[used].0..].trim_end().to_owned();
                            Some((tier, weight, Some((used, argument))))
                        })
                    })
                    .flatten();
                let (tier, weight, argument) = [searched, named]
                    .into_iter()
                    .flatten()
                    .min_by_key(|(tier, weight, _)| (*tier, Reverse(*weight)))?;
                let named_terms = match &argument {
                    Some((used, _)) => &terms[..*used],
                    None => &terms[..],
                };
                let marks = match tier {
                    Tier::Name | Tier::NamePrefix | Tier::NameWords | Tier::InName => {
                        Self::marks(&words.name, named_terms)
                    }
                    Tier::Alias | Tier::AliasWords | Tier::InAlias => Vec::new(),
                };
                Some(Matched {
                    index,
                    argument: argument.map(|(_, argument)| argument),
                    marks,
                    tier,
                    weight,
                })
            })
            .collect();
        rows.sort_by_key(|row| {
            let entry = &self.entries[row.index];
            (
                row.tier,
                Reverse(row.weight),
                entry.recent.unwrap_or(usize::MAX),
                entry.command.name(),
            )
        });
        rows
    }

    /// How the whole line, as a search, reaches the name or an alias.
    fn searched(words: &Words, terms: &[String]) -> Option<(Tier, u8)> {
        let by_name = reach(&words.name.text, terms).map(|reach| {
            let tier = match reach {
                Reach::Whole => Tier::Name,
                Reach::Start => Tier::NamePrefix,
                Reach::WordStarts => Tier::NameWords,
                Reach::Inside => Tier::InName,
            };
            (tier, NAME_WEIGHT)
        });
        let by_alias = words.aliases.iter().filter_map(|(alias, _, weight)| {
            let tier = match reach(alias, terms)? {
                Reach::Whole | Reach::Start => Tier::Alias,
                Reach::WordStarts => Tier::AliasWords,
                Reach::Inside => Tier::InAlias,
            };
            Some((tier, *weight))
        });
        by_name
            .into_iter()
            .chain(by_alias)
            .min_by_key(|(tier, weight)| (*tier, Reverse(*weight)))
    }

    fn marks(name: &Folded, terms: &[String]) -> Vec<Range<usize>> {
        let mut marks: Vec<_> = terms
            .iter()
            .filter_map(|term| {
                let (_, range) = best_hit(&name.text, &Finder::new(term))?;
                Some(name.display(range))
            })
            .collect();
        merge_marks(&mut marks);
        marks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Spelling;

    macro_rules! spelled {
        ($name:literal $(, $alias:literal = $weight:literal)* $(; $argument:literal)?) => {{
            const SPELLING: Spelling = Spelling {
                name: $name,
                aliases: &[$(($alias, $weight)),*],
                argument: spelled!(@argument $($argument)?),
            };
            Command::new(&SPELLING, |_, _, _, _| {})
        }};
        (@argument) => { None };
        (@argument $argument:literal) => { Some($argument) };
    }

    /// The intents the product names (`window.Commands`), spelled the way
    /// the systems spell them; the real tables are ranked together in
    /// `rho-gui`.
    fn catalogue() -> Vec<Command> {
        vec![
            spelled!("stop agent", "kill" = 9, "halt" = 7, "end" = 4),
            spelled!(
                "cancel agent request",
                "kill" = 6,
                "abort" = 9,
                "interrupt" = 9,
                "stop" = 5
            ),
            spelled!("set agent runtime", "model" = 9, "runtime" = 8, "switch model" = 8; "runtime"),
            spelled!("set project runtime", "model" = 5, "runtime" = 5; "runtime"),
            spelled!(
                "reset agent cache",
                "clear cache" = 9,
                "clear" = 6,
                "flush" = 7
            ),
            spelled!("open terminal", "term" = 9, "shell" = 3),
            spelled!("new terminal", "term" = 4),
            spelled!("open shell", "sh" = 8),
            spelled!("open page", "open" = 8, "url" = 9, "web" = 8, "go" = 7; "address"),
            spelled!("rename thread", "rename" = 9, "title" = 8, "subject" = 8; "subject"),
            spelled!("snooze"; "time"),
            spelled!("quit", "exit" = 9),
            spelled!("bookmark page", "bookmark" = 9; "name"),
            spelled!("remove bookmark"; "name"),
            spelled!("set project place"; "place"),
        ]
    }

    fn listing(recent: &[&str]) -> Listing {
        let recent: Vec<String> = recent.iter().map(|name| (*name).to_owned()).collect();
        Listing::new(
            catalogue()
                .into_iter()
                .map(|command| (command, String::new()))
                .collect(),
            &recent,
        )
    }

    fn names(listing: &Listing, input: &str) -> Vec<&'static str> {
        listing
            .rank(input)
            .into_iter()
            .map(|row| listing.entries[row.index].command.name())
            .collect()
    }

    fn first(listing: &Listing, input: &str) -> (&'static str, Option<String>) {
        let row = listing.rank(input).into_iter().next().expect("a command");
        (listing.entries[row.index].command.name(), row.argument)
    }

    #[test]
    fn kill_means_stop_before_cancel() {
        let listing = listing(&[]);
        assert_eq!(
            names(&listing, "kill"),
            ["stop agent", "cancel agent request"]
        );
        // Even when cancel was run last: the weight is the intent.
        let listing = super::tests::listing(&["cancel agent request"]);
        assert_eq!(names(&listing, "kill")[0], "stop agent");
    }

    #[test]
    fn interrupting_words_mean_cancel() {
        let listing = listing(&[]);
        for word in ["abort", "interrupt", "abo", "inter"] {
            assert_eq!(first(&listing, word).0, "cancel agent request", "{word}");
        }
    }

    /// A name begun beats an alias that is a whole word: `stop` is the
    /// agent's stop, not the alias on cancel.
    #[test]
    fn a_name_begun_beats_an_alias_said_outright() {
        let listing = listing(&[]);
        assert_eq!(
            names(&listing, "stop"),
            ["stop agent", "cancel agent request"]
        );
        assert_eq!(names(&listing, "sto")[0], "stop agent");
        assert_eq!(first(&listing, "qu").0, "quit");
        assert_eq!(first(&listing, "q").0, "quit");
    }

    #[test]
    fn model_means_the_agents_runtime_then_the_projects() {
        let listing = listing(&[]);
        assert_eq!(
            names(&listing, "model"),
            ["set agent runtime", "set project runtime"]
        );
        assert_eq!(
            names(&listing, "runtime")[..2],
            ["set agent runtime", "set project runtime"]
        );
        // The name, begun, is still the name.
        assert_eq!(
            names(&listing, "set proj"),
            ["set project place", "set project runtime"]
        );
        assert_eq!(names(&listing, "set ag")[0], "set agent runtime");
    }

    #[test]
    fn clearing_words_mean_reset_agent_cache() {
        let listing = listing(&[]);
        for word in ["clear", "flush", "clear cache", "cache"] {
            assert_eq!(first(&listing, word).0, "reset agent cache", "{word}");
        }
    }

    #[test]
    fn short_and_web_words_find_their_commands() {
        let listing = listing(&[]);
        assert_eq!(
            names(&listing, "term")[..2],
            ["open terminal", "new terminal"]
        );
        for word in ["url", "web", "go"] {
            assert_eq!(first(&listing, word).0, "open page", "{word}");
        }
        for word in ["title", "subject", "rename"] {
            assert_eq!(first(&listing, word).0, "rename thread", "{word}");
        }
    }

    /// The starts of a name's words, in order, find it before a name the
    /// words only touch inside.
    #[test]
    fn word_starts_in_order_beat_words_inside() {
        let listing = listing(&[]);
        assert_eq!(first(&listing, "ag run").0, "set agent runtime");
        assert_eq!(first(&listing, "re ca").0, "reset agent cache");
        // Abbreviated, `ren` still names `rename` before its argument when
        // nothing else fits the line.
        assert_eq!(
            first(&listing, "ren Fix clock"),
            ("rename thread", Some("Fix clock".into()))
        );
        assert!(names(&listing, "zzz").is_empty());
    }

    #[test]
    fn an_argument_follows_the_words_that_name_the_command() {
        let listing = listing(&[]);
        assert_eq!(
            first(&listing, "model sol-medium"),
            ("set agent runtime", Some("sol-medium".into()))
        );
        assert_eq!(
            names(&listing, "model sol-medium"),
            ["set agent runtime", "set project runtime"],
            "both runtimes take it, the agent's first"
        );
        assert_eq!(
            first(&listing, "rename Fix   clock race "),
            ("rename thread", Some("Fix   clock race".into())),
            "the argument keeps its case and inner spaces"
        );
        assert_eq!(first(&listing, "snooze 3h"), ("snooze", Some("3h".into())));
        assert_eq!(
            names(&listing, "open https://example.com/a?b=C"),
            ["open page"],
            "only a command that takes an address takes it"
        );
        assert_eq!(
            first(&listing, "open https://example.com/a?b=C")
                .1
                .as_deref(),
            Some("https://example.com/a?b=C")
        );
        assert_eq!(
            names(&listing, "set project runtime sol-high"),
            ["set project runtime"]
        );
        assert_eq!(
            first(&listing, "set proj run sol-high"),
            ("set project runtime", Some("sol-high".into())),
            "each word abbreviated"
        );
        assert_eq!(
            first(&listing, "bookmark Slack"),
            ("bookmark page", Some("Slack".into()))
        );
    }

    /// Words that do not name a command outright are a search, never the
    /// start of an argument; a command that takes nothing takes nothing.
    #[test]
    fn no_argument_without_a_command_that_takes_one() {
        let listing = listing(&[]);
        assert!(names(&listing, "stop agent now").is_empty());
        assert!(names(&listing, "quit please").is_empty());
        // `set` alone names none of the `set` commands.
        assert!(
            listing
                .rank("set sol-high")
                .iter()
                .all(|row| row.argument.is_none())
        );
        // The whole line naming the command is no argument.
        assert_eq!(
            first(&listing, "set project runtime"),
            ("set project runtime", None)
        );
    }

    /// The more of the line that names a command, the better it fits:
    /// `remove bookmark Slack` is the removal, though `bookmark` alone
    /// would be the page's.
    #[test]
    fn the_longest_naming_wins() {
        let listing = listing(&[]);
        assert_eq!(
            first(&listing, "remove bookmark Slack"),
            ("remove bookmark", Some("Slack".into()))
        );
    }

    #[test]
    fn nothing_typed_lists_the_commands_run_last_then_every_other_by_name() {
        let listing = listing(&["stop agent", "quit"]);
        let all = names(&listing, "  ");
        assert_eq!(all[..3], ["stop agent", "quit", "bookmark page"]);
        assert_eq!(all.len(), catalogue().len(), "each once");
        let mut rest = all[2..].to_vec();
        rest.sort();
        assert_eq!(rest, all[2..], "the rest by name");
    }

    /// Among equal reach and weight, what was run last comes first.
    #[test]
    fn recent_use_breaks_a_tie() {
        assert_eq!(
            names(&listing(&[]), "open"),
            ["open page", "open shell", "open terminal"]
        );
        assert_eq!(
            names(&listing(&["open terminal"]), "open")[..2],
            ["open terminal", "open page"]
        );
    }

    #[test]
    fn marks_fall_on_the_name_the_words_matched() {
        let listing = listing(&[]);
        let rows = listing.rank("ag run");
        assert_eq!(rows[0].marks, [4..6, 10..13]);
        let rows = listing.rank("kill");
        assert!(
            rows[0].marks.is_empty(),
            "an alias marks nothing it does not show"
        );
        let rows = listing.rank("model sol-medium");
        assert!(rows[0].marks.is_empty());
        let rows = listing.rank("rename thread New name");
        assert_eq!(rows[0].marks, [0..6, 7..13], "the argument marks nothing");
    }
}
