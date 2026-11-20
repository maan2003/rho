//! The palette: every place and every command by name, in one prompt.
//!
//! The menus are data ([`Rows`]), so the commands are the same rows read
//! another way: flattened to one list, found by a fuzzy query instead of
//! a key. A command keeps the key it has in the menus as a hint beside
//! its name, so the palette teaches the keys it replaces. The places are
//! what every system's `find` lists, each with where it is beside it.
//! `:` and `ctrl-shift-f` open the same list, and differ only in which
//! of the two comes first until the query tells them apart.

use std::cmp::Reverse;

use crate::menu::{Pick, Rows};
use crate::system::Found;

/// The rows the palette shows at most: eight are in view at once and the
/// selection scrolls through the rest, so past this the reader types more
/// rather than scrolls.
const LISTED: usize = 100;

/// One command in the palette.
pub(crate) struct Entry {
    /// What the reader types toward: the row's words, under the menus
    /// above it (`hosts › attach`).
    pub label: String,
    /// The keys that reach it through the menus (`shift-h a`).
    pub keys: String,
    /// Other words it is found by (see [`crate::menu::Row::also`]).
    pub also: Vec<String>,
    pub pick: Pick,
}

/// Every row of `rows` and of the menus under them, each menu as an
/// entry of its own too, since some menus are the information (the agents
/// to join, the times to snooze until).
pub(crate) fn flatten(rows: &Rows) -> Vec<Entry> {
    let mut entries = Vec::new();
    walk(rows, "", "", &mut entries);
    entries
}

fn walk(rows: &Rows, label_above: &str, keys_above: &str, into: &mut Vec<Entry>) {
    for row in &rows.rows {
        let label = match label_above.is_empty() {
            true => row.description.clone(),
            false => format!("{label_above} › {}", row.description),
        };
        let keys = match keys_above.is_empty() {
            true => row.key.clone(),
            false => format!("{keys_above} {}", row.key),
        };
        into.push(Entry {
            label: label.clone(),
            keys: keys.clone(),
            also: row.also.clone(),
            pick: row.pick.clone(),
        });
        if let Pick::Menu(rows) = &row.pick {
            walk(rows, &label, &keys, into);
        }
    }
}

/// Which half of the list the reader opened the palette for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum First {
    /// `:`.
    Commands,
    /// `ctrl-shift-f`.
    Places,
}

/// One row of the palette.
pub(crate) enum Choice {
    Command(Entry),
    Place(Found),
}

impl Choice {
    /// The row's name, which the query is matched against.
    pub(crate) fn name(&self) -> &str {
        match self {
            Choice::Command(entry) => &entry.label,
            Choice::Place(found) => &found.title,
        }
    }

    /// What the row shows beside its name: a command's keys, a place's
    /// where.
    pub(crate) fn beside(&self) -> &str {
        match self {
            Choice::Command(entry) => &entry.keys,
            Choice::Place(found) => &found.description,
        }
    }

    fn half(&self) -> First {
        match self {
            Choice::Command(_) => First::Commands,
            Choice::Place(_) => First::Places,
        }
    }

    /// How well the row answers `query`, best first: by the starts of the
    /// words of its name (or, for a command, of a word it is also found
    /// by); for a place, by the starts of the words of its name and where
    /// it is together, so `rho` finds the threads of the channel `rho` and
    /// `waiting` the agents waiting; and last by letters inside a word of
    /// its name. `None` when it does not answer at all, so letters that
    /// only happen to be in it in order do not list it.
    fn answers(&self, query: &str) -> Option<(u8, i32)> {
        let by_name = match self {
            Choice::Command(entry) => std::iter::once(entry.label.as_str())
                .chain(entry.also.iter().map(String::as_str))
                .filter_map(|words| word_starts(query, words))
                .max(),
            Choice::Place(found) => word_starts(query, &found.title),
        };
        if let Some(score) = by_name {
            return Some((3, score));
        }
        if let Choice::Place(found) = self
            && let Some(score) =
                word_starts(query, &format!("{} {}", found.title, found.description))
        {
            return Some((2, score));
        }
        inside(query, self.name()).then_some((1, 0))
    }

    /// The order among rows of one half that answer alike: the shorter
    /// command first, the newer place first.
    fn order(&self) -> u64 {
        match self {
            Choice::Command(entry) => entry.label.chars().count() as u64,
            Choice::Place(found) => u64::MAX - found.recency,
        }
    }
}

/// What one opening of the palette lists.
pub(crate) struct Listing {
    pub choices: Vec<Choice>,
    first: First,
}

impl Listing {
    pub(crate) fn new(commands: Vec<Entry>, places: Vec<Found>, first: First) -> Self {
        let choices = commands
            .into_iter()
            .map(Choice::Command)
            .chain(places.into_iter().map(Choice::Place))
            .collect();
        Self { choices, first }
    }

    /// The rows that answer `query`, as indices into `choices`, best
    /// first: the better match (see [`Choice::answers`]), then the half the
    /// palette was opened for, then the shorter command or the newer place.
    /// With nothing typed every row answers alike, so the opened-for half
    /// comes first, in its own order.
    pub(crate) fn rank(&self, query: &str) -> Vec<usize> {
        let mut found: Vec<_> = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                let (how, score) = choice.answers(query)?;
                Some((
                    Reverse(how),
                    choice.half() != self.first,
                    Reverse(score),
                    choice.order(),
                    index,
                ))
            })
            .collect();
        found.sort_unstable();
        found
            .into_iter()
            .take(LISTED)
            .map(|(.., index)| index)
            .collect()
    }
}

/// How well `label` answers `query` by the starts of its words: the
/// query's letters, in order, in runs that each begin a word of the label,
/// so `sa` answers `start agent here` and `sales`, and `rename` does not
/// answer `start agent, choosing where and runtime…`. A space in the query
/// begins a new run. Higher is better: each run costs, and each run that
/// is a whole word earns more back, so `ops` answers `ops` better than
/// `opsgenie` and `sales` better than `start agent`. An empty query
/// answers all alike.
fn word_starts(query: &str, label: &str) -> Option<i32> {
    const RUN: i32 = 3;
    const WHOLE_WORD: i32 = 4;
    let label: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let starts_word = |at: usize| at == 0 || !label[at - 1].is_alphanumeric();
    let ends_word = |at: usize| at + 1 == label.len() || !label[at + 1].is_alphanumeric();
    // `best[at]`: the best score for the letters so far with the last of
    // them at `at`, counting the whole words of every run but its own.
    let mut best: Option<Vec<Option<i32>>> = None;
    let mut after_space = false;
    for wanted in query.chars().flat_map(char::to_lowercase) {
        if wanted.is_whitespace() {
            after_space = true;
            continue;
        }
        let mut next = vec![None; label.len()];
        let mut ended_before: Option<i32> = None;
        for at in 0..label.len() {
            if label[at] == wanted {
                let begun = match &best {
                    None => Some(0),
                    Some(_) => ended_before,
                }
                .filter(|_| starts_word(at))
                .map(|score| score - RUN);
                let continued = match &best {
                    Some(best) if at > 0 && !after_space => best[at - 1],
                    _ => None,
                };
                next[at] = begun.max(continued);
            }
            if let Some(best) = &best
                && let Some(score) = best[at]
            {
                let run_ends = score + if ends_word(at) { WHOLE_WORD } else { 0 };
                ended_before = ended_before.max(Some(run_ends));
            }
        }
        best = Some(next);
        after_space = false;
    }
    match best {
        None => Some(0),
        Some(best) => (0..label.len())
            .filter_map(|at| Some(best[at]? + if ends_word(at) { WHOLE_WORD } else { 0 }))
            .max(),
    }
}

/// Whether `query` is in `label` whole, inside a word: the last way a row
/// answers, for a query long enough that this is not chance.
fn inside(query: &str, label: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.chars().count() >= 3 && label.to_lowercase().contains(&query)
}

#[cfg(test)]
mod tests {
    use rho_dealer::Address;

    use super::*;
    use crate::menu::Row;

    #[test]
    fn menus_flatten_to_paths_with_their_keys() {
        let mut rows = Rows::new("rho");
        rows.push(Row::run("h", "home", |_, _, _| {}));
        let mut hosts = Rows::new("hosts");
        hosts.push(Row::run("a", "attach", |_, _, _| {}));
        hosts.push(Row::run("d", "detach", |_, _, _| {}));
        rows.push(Row::menu("shift-h", "hosts", hosts));
        rows.push(Row::run("x", "home", |_, _, _| {}));
        let entries = flatten(&rows);
        let seen: Vec<(&str, &str)> = entries
            .iter()
            .map(|entry| (entry.label.as_str(), entry.keys.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("home", "h"),
                ("hosts", "shift-h"),
                ("hosts › attach", "shift-h a"),
                ("hosts › detach", "shift-h d"),
                ("home", "x"),
            ]
        );
    }

    #[test]
    fn a_query_answers_by_runs_that_begin_words() {
        assert_eq!(word_starts("", "anything"), Some(0));
        // Two runs, `s` and `a`, neither a whole word.
        assert_eq!(word_starts("sa", "start agent here"), Some(-6));
        // One run that begins a word.
        assert_eq!(word_starts("sa", "sales"), Some(-3));
        // One run that is the whole word earns back more than it costs.
        assert_eq!(word_starts("ops", "ops"), Some(1));
        assert_eq!(word_starts("ops", "opsgenie"), Some(-3));
        // The best runs count, not the first letters found: `a` of
        // `agent`, not of `start`.
        assert_eq!(word_starts("sag", "start agent"), Some(-6));
        // Letters that are in the label in order but do not begin words
        // do not answer.
        assert_eq!(word_starts("standup", "start agent, choosing where"), None);
        assert_eq!(
            word_starts("rename", "start agent, choosing where and runtime…"),
            None
        );
        assert_eq!(word_starts("det", "delete…"), None);
        assert_eq!(word_starts("det", "hosts › detach"), Some(-3));
        // A space begins a new run.
        assert_eq!(word_starts("ne w", "new"), None);
        assert_eq!(word_starts("new ch", "new channel"), Some(-2));
    }

    fn command(label: &str) -> Entry {
        let mut rows = Rows::new("rho");
        rows.push(Row::run("k", label, |_, _, _| {}));
        flatten(&rows).remove(0)
    }

    fn command_also(label: &str, also: &str) -> Entry {
        let mut rows = Rows::new("rho");
        rows.push(Row::run("k", label, |_, _, _| {}).also(also));
        flatten(&rows).remove(0)
    }

    fn place(title: &str, description: &str, recency: u64) -> Found {
        Found {
            address: Address::new("thread", title),
            title: title.to_owned(),
            description: description.to_owned(),
            recency,
        }
    }

    fn listing(first: First) -> Listing {
        Listing::new(
            vec![
                command("start agent here"),
                command("home"),
                command("hosts › attach"),
                command("start agent, choosing where and runtime…"),
                command_also("name this thread", "rename"),
                command("show my secret phrase"),
            ],
            vec![
                place("privacy VM", "rho › infra", 10),
                place("#sales-8", "slack · acme", 30),
                place("debug", "rho · channel", 20),
                place("debug", "infra › deploys", 40),
                place("sol", "rho · waiting", 5),
            ],
            first,
        )
    }

    fn names(listing: &Listing, query: &str) -> Vec<String> {
        listing
            .rank(query)
            .into_iter()
            .map(|index| {
                let choice = &listing.choices[index];
                format!("{} | {}", choice.name(), choice.beside())
            })
            .collect()
    }

    #[test]
    fn nothing_typed_lists_the_opened_half_first_each_in_its_own_order() {
        assert_eq!(
            names(&listing(First::Commands), ""),
            [
                "home | k",
                "hosts › attach | k",
                // As long as `name this thread`: the order they came in.
                "start agent here | k",
                "name this thread | k",
                "show my secret phrase | k",
                "start agent, choosing where and runtime… | k",
                "debug | infra › deploys",
                "#sales-8 | slack · acme",
                "debug | rho · channel",
                "privacy VM | rho › infra",
                "sol | rho · waiting",
            ]
        );
        assert_eq!(
            names(&listing(First::Places), "")[..6],
            [
                "debug | infra › deploys",
                "#sales-8 | slack · acme",
                "debug | rho · channel",
                "privacy VM | rho › infra",
                "sol | rho · waiting",
                "home | k",
            ]
        );
    }

    #[test]
    fn typing_narrows_across_both_halves() {
        // `:` then a thread's name finds the thread, and nothing else.
        assert_eq!(
            names(&listing(First::Commands), "privacy"),
            ["privacy VM | rho › infra"]
        );
        // `sa` answers a command and a place alike; the half the palette
        // was opened for comes first.
        assert_eq!(
            names(&listing(First::Commands), "sa"),
            [
                "start agent here | k",
                "start agent, choosing where and runtime… | k",
                "#sales-8 | slack · acme",
            ]
        );
        assert_eq!(
            names(&listing(First::Places), "sa"),
            [
                "#sales-8 | slack · acme",
                "start agent here | k",
                "start agent, choosing where and runtime… | k",
            ]
        );
    }

    #[test]
    fn a_rare_command_is_found_by_its_word_and_nothing_else_answers() {
        assert_eq!(
            names(&listing(First::Commands), "rename"),
            ["name this thread | k"]
        );
        assert_eq!(
            names(&listing(First::Commands), "secret"),
            ["show my secret phrase | k"]
        );
        assert!(names(&listing(First::Commands), "pair").is_empty());
    }

    #[test]
    fn letters_inside_a_word_answer_last() {
        let listing = Listing::new(
            vec![command("deploy now")],
            vec![place("redeploy", "rho › infra", 1)],
            First::Places,
        );
        // `deploy` begins a word of the command and is inside the place's.
        assert_eq!(
            names(&listing, "deploy"),
            ["deploy now | k", "redeploy | rho › infra"]
        );
        // Two letters inside a word are chance, not an answer.
        assert_eq!(names(&listing, "pl"), Vec::<String>::new());
    }

    #[test]
    fn a_place_answers_by_where_it_is_when_its_name_does_not() {
        assert_eq!(
            names(&listing(First::Places), "waiting"),
            ["sol | rho · waiting"]
        );
        // Both threads called `debug` are listed, newest first, each with
        // its channel beside it.
        assert_eq!(
            names(&listing(First::Places), "debug"),
            ["debug | infra › deploys", "debug | rho · channel"]
        );
    }

    #[test]
    fn a_name_that_answers_beats_a_where_that_answers() {
        // `ops` begins no word of `@Rob Pike` and only `slack` and `acme`
        // of where it is, which is not in order; `sol` answers `sol` by
        // name, and `rho · ops` answers it only by where it is.
        let listing = Listing::new(
            Vec::new(),
            vec![
                place("@Rob Pike", "slack · acme", 50),
                place("deploy", "rho › ops", 40),
                place("ops", "qa · channel", 1),
            ],
            First::Places,
        );
        assert_eq!(
            names(&listing, "ops"),
            ["ops | qa · channel", "deploy | rho › ops"]
        );
    }

    #[test]
    fn the_list_stops_at_its_cap() {
        let places = (0..LISTED as u64 + 20)
            .map(|at| place(&format!("thread {at}"), "rho", at))
            .collect();
        let listing = Listing::new(Vec::new(), places, First::Places);
        let ranked = listing.rank("");
        assert_eq!(ranked.len(), LISTED);
        assert_eq!(
            listing.choices[ranked[0]].name(),
            format!("thread {}", LISTED + 19)
        );
    }
}
