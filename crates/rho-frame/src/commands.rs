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
use std::rc::Rc;

use memchr::memmem::Finder;
use rho_dealer::Address;

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
    /// A whole alias: `kill`.
    Alias,
    /// The start of an alias: `mod`. An alias said outright beats one
    /// begun, so `focus` is `focus`, not `focus off`.
    AliasPrefix,
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

/// The place words typed after a command name: what the command acts
/// from instead of the place in view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub address: Address,
    /// What the row calls it: `Flaky clock test · tide`.
    pub name: String,
}

/// Finds the place typed words name, as `Places` would rank it first.
pub(crate) type FindPlace = Rc<dyn Fn(&str) -> Option<Resolved>>;

/// What the line gives a command after the words that name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Inline {
    /// A value, for a command that takes one: `sol-medium`.
    Value(String),
    /// A place to act from, for a command that acts on the place in view.
    Place { words: String, place: Resolved },
}

impl Inline {
    /// The words as typed, which the row shows apart from the name.
    pub(crate) fn words(&self) -> &str {
        match self {
            Self::Value(words) | Self::Place { words, .. } => words,
        }
    }
}

/// A command the line picks, and what the line gives it.
pub(crate) struct Matched {
    pub index: usize,
    pub inline: Option<Inline>,
    /// Display bytes of the name the words matched.
    pub marks: Vec<Range<usize>>,
    /// How many of the typed words name the command.
    naming: usize,
    tier: Tier,
    weight: u8,
}

impl Matched {
    pub(crate) fn argument(&self) -> Option<&str> {
        match &self.inline {
            Some(Inline::Value(value)) => Some(value),
            Some(Inline::Place { .. }) | None => None,
        }
    }
}

pub(crate) struct Listing {
    pub entries: Vec<Entry>,
    words: Vec<Words>,
    /// Where words after a command that acts on a place lead.
    find_place: Option<FindPlace>,
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
        .map(|matched| {
            let argument = matched.argument().map(str::to_owned);
            (spellings[matched.index].name, argument)
        })
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
        Self::finding_places(commands, recent, None)
    }

    /// As [`Self::new`], with `find_place` to read the words after a
    /// command that acts on a place.
    pub fn finding_places(
        commands: Vec<(Command, String)>,
        recent: &[String],
        find_place: Option<FindPlace>,
    ) -> Self {
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
        Self {
            entries,
            words,
            find_place,
        }
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
                    inline: None,
                    marks: Vec::new(),
                    naming: 0,
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
                let command = &self.entries[index].command;
                let takes_value = command.spelling().argument.is_some();
                let find_place = self.find_place.as_ref().filter(|_| command.takes_place());
                let searched = Self::searched(words, &terms)
                    .map(|(tier, weight)| (tier, weight, terms.len(), None));
                // The most words that name it outright, then what it is
                // given: a value, or a place to act from that the words
                // find. Abbreviated words name it less surely than whole
                // ones, so `re ca` is a search before it is `rename` on `ca`.
                let named = ((takes_value || find_place.is_some()) && terms.len() > 1)
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
                            let words = input[tokens[used].0..].trim_end().to_owned();
                            let inline = match find_place {
                                Some(find_place) => Inline::Place {
                                    place: find_place(&words)?,
                                    words,
                                },
                                None => Inline::Value(words),
                            };
                            Some((tier, weight, used, Some(inline)))
                        })
                    })
                    .flatten();
                let (tier, weight, naming, inline) = [searched, named]
                    .into_iter()
                    .flatten()
                    .min_by_key(|(tier, weight, ..)| (*tier, Reverse(*weight)))?;
                let marks = match tier {
                    Tier::Name | Tier::NamePrefix | Tier::NameWords | Tier::InName => {
                        Self::marks(&words.name, &terms[..naming])
                    }
                    Tier::Alias | Tier::AliasPrefix | Tier::AliasWords | Tier::InAlias => {
                        Vec::new()
                    }
                };
                Some(Matched {
                    index,
                    inline,
                    marks,
                    naming,
                    tier,
                    weight,
                })
            })
            .collect();
        // Command words first: the more of the line that names a command,
        // the surer it is, so `open term clock` opens a terminal from the
        // `clock` thread before it opens a page at `term clock`.
        rows.sort_by_key(|row| {
            let entry = &self.entries[row.index];
            (
                Reverse(row.naming),
                row.tier,
                Reverse(row.weight),
                entry.recent.unwrap_or(usize::MAX),
                entry.command.name(),
            )
        });
        rows
    }

    /// What a row shows, in order: the command's name, the words typed
    /// after it, kept apart from the name, and what it acts on, which is
    /// the place those words found when they name one.
    pub(crate) fn row_parts<'a>(
        &'a self,
        matched: &'a Matched,
    ) -> (&'static str, Option<&'a str>, Option<&'a str>) {
        let command = &self.entries[matched.index].command;
        let target = match &matched.inline {
            Some(Inline::Place { place, .. }) => Some(place.name.as_str()),
            Some(Inline::Value(_)) | None => command.target_text(),
        };
        (
            command.name(),
            matched.inline.as_ref().map(Inline::words),
            target,
        )
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
                Reach::Whole => Tier::Alias,
                Reach::Start => Tier::AliasPrefix,
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
        let argument = row.argument().map(str::to_owned);
        (listing.entries[row.index].command.name(), argument)
    }

    /// The catalogue, with the agent commands acting on the place in view,
    /// and words after them finding a place among `places`.
    fn placed() -> Listing {
        let here = [
            "stop agent",
            "cancel agent request",
            "open terminal",
            "new terminal",
        ];
        let places = [
            ("thread:lab/1", "Flaky clock test · tide"),
            ("thread:lab/2", "Retry window · reef"),
        ];
        let find_place: FindPlace = Rc::new(move |words: &str| {
            let words = words.to_lowercase();
            places
                .iter()
                .find(|(_, name)| {
                    words
                        .split_whitespace()
                        .all(|word| name.to_lowercase().contains(word))
                })
                .map(|(key, name)| Resolved {
                    address: Address::parse(key).unwrap(),
                    name: (*name).to_owned(),
                })
        });
        Listing::finding_places(
            catalogue()
                .into_iter()
                .map(|command| match here.contains(&command.name()) {
                    true => command.acts_on("tide · Flaky clock test · working"),
                    false => command,
                })
                .map(|command| (command, String::new()))
                .collect(),
            &[],
            Some(find_place),
        )
    }

    /// Each row: its name, and what the line gives it.
    fn inlines(listing: &Listing, input: &str) -> Vec<(&'static str, Option<Inline>)> {
        listing
            .rank(input)
            .into_iter()
            .map(|row| (listing.entries[row.index].command.name(), row.inline))
            .collect()
    }

    /// `stop agent clock` stops the agent of the thread `clock` finds,
    /// whatever is in view: the words after the name are a place.
    #[test]
    fn words_after_a_command_on_a_place_name_the_place() {
        let listing = placed();
        let rows = inlines(&listing, "stop agent clock");
        let Some(Inline::Place { words, place }) = &rows[0].1 else {
            panic!("{rows:?}");
        };
        assert_eq!(rows[0].0, "stop agent");
        assert_eq!(words, "clock");
        assert_eq!(place.address, Address::parse("thread:lab/1").unwrap());
        assert_eq!(place.name, "Flaky clock test · tide");
        // Its words keep their case and inner spaces; an alias names it too.
        let rows = inlines(&listing, "kill Retry  Window");
        assert_eq!(
            rows.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            ["stop agent", "cancel agent request"]
        );
        assert!(
            matches!(&rows[0].1, Some(Inline::Place { words, place })
                if words == "Retry  Window" && place.name == "Retry window · reef"),
            "{rows:?}"
        );
        // With nothing after it, the place in view stays.
        assert_eq!(inlines(&listing, "stop agent")[0], ("stop agent", None));
    }

    /// Words that find no place are no place: the line names no command,
    /// rather than one acting on the place in view with words ignored.
    #[test]
    fn words_that_find_no_place_name_no_command() {
        let listing = placed();
        assert!(inlines(&listing, "stop agent zzz").is_empty());
        // Without a way to find places, as without the frame, nothing does.
        let bare = listing_with_targets_but_no_places();
        assert!(names(&bare, "stop agent clock").is_empty());
        // A command that acts on nothing in particular takes no place.
        assert!(names(&listing, "quit clock").is_empty());
        // A command that takes a value takes the words as its value, even
        // where they would find a place.
        assert_eq!(
            inlines(&listing, "rename clock")[0],
            ("rename thread", Some(Inline::Value("clock".into())))
        );
    }

    fn listing_with_targets_but_no_places() -> Listing {
        Listing::new(
            catalogue()
                .into_iter()
                .map(|command| match command.name() {
                    "stop agent" => command.acts_on("here"),
                    _ => command,
                })
                .map(|command| (command, String::new()))
                .collect(),
            &[],
        )
    }

    /// Command words match first: `open term clock` is a terminal opened
    /// from the `clock` thread, and the page at `term clock` comes after,
    /// its words shown apart as an address.
    #[test]
    fn command_words_match_before_inline_words() {
        let listing = placed();
        let rows = inlines(&listing, "open term clock");
        assert_eq!(rows[0].0, "open terminal");
        assert!(matches!(&rows[0].1, Some(Inline::Place { words, .. }) if words == "clock"));
        let page = rows
            .iter()
            .find(|(name, _)| *name == "open page")
            .expect("the page still lists");
        assert_eq!(page.1, Some(Inline::Value("term clock".into())));
        // The marks fall on the name's words, never on the place.
        let row = &listing.rank("open term clock")[0];
        assert_eq!(row.marks, [0..4, 5..9]);
        assert_eq!(row.inline.as_ref().map(Inline::words), Some("clock"));
        // `open term` alone is the terminal, from the place in view.
        assert_eq!(inlines(&listing, "open term")[0], ("open terminal", None));
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
                .all(|row| row.inline.is_none())
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
