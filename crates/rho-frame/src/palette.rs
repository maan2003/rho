//! `:`: every command and every place by name, in one prompt.
//!
//! The menus are data ([`Rows`]), so the commands are the same rows read
//! another way: flattened to one list, found by a query instead of a key.
//! A command keeps the key it has in the menus as a hint beside its name,
//! so the palette teaches the keys it replaces. The places are what every
//! system's `find` lists, each with where it is beside it.

use std::cmp::Reverse;

use crate::menu::{Pick, Row, Rows, Scope};
use crate::system::Found;

/// The rows the palette shows at most: eight are in view at once and the
/// selection scrolls through the rest, so past this the reader types more
/// rather than scrolls.
const LISTED: usize = 100;

/// One command in the palette.
pub(crate) struct Entry {
    /// What the reader types toward: the row's words, under the menus
    /// above it (`hosts › attach`). Unique in the list.
    pub label: String,
    /// Other words it is found by (see [`Row::also`]).
    pub also: Vec<String>,
    /// The row's shortcut, or its keys inside the menus above it once
    /// the first of them is open (`a` in `hosts › attach`).
    pub keys: String,
    pub pick: Pick,
    /// Whose it is: a row under a menu is its top row's.
    pub scope: Scope,
}

/// Every row of `top` and of the menus under them, each menu as an entry
/// of its own too, since some menus are the information (the agents to
/// join, the times to snooze until).
pub(crate) fn flatten(top: &[Row]) -> Vec<Entry> {
    let mut entries = Vec::new();
    for row in top {
        entries.push(Entry {
            label: row.description.clone(),
            also: row.also.clone(),
            keys: row.shortcut.clone().unwrap_or_default(),
            pick: row.pick.clone(),
            scope: row.scope,
        });
        if let Pick::Menu(rows) = &row.pick {
            walk(rows, &row.description, "", row.scope, &mut entries);
        }
    }
    let mut seen: Vec<String> = Vec::new();
    for entry in &mut entries {
        let base = entry.label.clone();
        let mut nth = 1;
        while seen.contains(&entry.label) {
            nth += 1;
            entry.label = format!("{base} ({nth})");
        }
        seen.push(entry.label.clone());
    }
    entries
}

fn walk(rows: &Rows, label_above: &str, keys_above: &str, scope: Scope, into: &mut Vec<Entry>) {
    for row in &rows.rows {
        let label = format!("{label_above} › {}", row.description);
        let keys = match keys_above.is_empty() {
            true => row.key.clone(),
            false => format!("{keys_above} {}", row.key),
        };
        into.push(Entry {
            label: label.clone(),
            also: row.also.clone(),
            keys: keys.clone(),
            pick: row.pick.clone(),
            scope,
        });
        if let Pick::Menu(rows) = &row.pick {
            walk(rows, &label, &keys, scope, into);
        }
    }
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

    /// Where the row stands when it answers no better than another: the
    /// screen's commands, then the places, then the window's own commands
    /// (`window.Palette`).
    fn group(&self) -> u8 {
        match self {
            Choice::Command(Entry {
                scope: Scope::Here, ..
            }) => 0,
            Choice::Place(_) => 1,
            Choice::Command(Entry {
                scope: Scope::Window,
                ..
            }) => 2,
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

    /// The order among rows of one group that answer alike: commands as
    /// they were offered, the newer place first.
    fn order(&self) -> u64 {
        match self {
            Choice::Command(_) => 0,
            Choice::Place(found) => u64::MAX - found.recency,
        }
    }
}

/// What one opening of the palette lists.
pub(crate) struct Listing {
    pub choices: Vec<Choice>,
}

impl Listing {
    pub(crate) fn new(commands: Vec<Entry>, places: Vec<Found>) -> Self {
        let choices = commands
            .into_iter()
            .map(Choice::Command)
            .chain(places.into_iter().map(Choice::Place))
            .collect();
        Self { choices }
    }

    /// The rows that answer `query`, as indices into `choices`, best
    /// first: the better way of answering (see [`Choice::answers`]), then
    /// the closer match, then the screen's commands, the places and the
    /// window's commands in that order, then the order within each. With
    /// nothing typed every row answers alike, so the screen's commands
    /// come first, then the places newest first, then the window's.
    pub(crate) fn rank(&self, query: &str) -> Vec<usize> {
        let mut found: Vec<_> = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                let (how, score) = choice.answers(query)?;
                Some((
                    Reverse(how),
                    Reverse(score),
                    choice.group(),
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
/// query's letters, in order, in runs that each begin a word of the label.
/// Runs typed together begin words next to each other, so `sa` answers
/// `start agent here` and `sales`, while `4pm` does not answer `build
/// #4103 passed on main`; a space in the query begins a run at any later
/// word, so `fix flaky` answers `fix the flaky test`. Higher is better:
/// each run costs, and each run that is a whole word earns more back, so
/// `ops` answers `ops` better than `opsgenie` and `sales` better than
/// `start agent`. An empty query answers all alike.
fn word_starts(query: &str, label: &str) -> Option<i32> {
    const RUN: i32 = 3;
    const WHOLE_WORD: i32 = 4;
    let label: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let starts_word = |at: usize| at == 0 || !label[at - 1].is_alphanumeric();
    let ends_word = |at: usize| at + 1 == label.len() || !label[at + 1].is_alphanumeric();
    // Which word each letter is in, counting from one.
    let word: Vec<usize> = label
        .iter()
        .enumerate()
        .scan(0, |words, (at, letter)| {
            if letter.is_alphanumeric() && starts_word(at) {
                *words += 1;
            }
            Some(*words)
        })
        .collect();
    let words = word.last().copied().unwrap_or(0);
    // `best[at]`: the best score for the letters so far with the last of
    // them at `at`, counting the whole words of every run but its own.
    let mut best: Option<Vec<Option<i32>>> = None;
    let mut after_space = false;
    for wanted in query.chars().flat_map(char::to_lowercase) {
        if wanted.is_whitespace() {
            after_space = true;
            continue;
        }
        // The best score with the run before ended in each word, and
        // anywhere before each letter.
        let mut ended_in = vec![None; words + 1];
        let mut ended_before: Vec<Option<i32>> = vec![None; label.len() + 1];
        if let Some(best) = &best {
            for at in 0..label.len() {
                let ended =
                    best[at].map(|score| score + if ends_word(at) { WHOLE_WORD } else { 0 });
                ended_in[word[at]] = ended_in[word[at]].max(ended);
                ended_before[at + 1] = ended_before[at].max(ended);
            }
        }
        let mut next = vec![None; label.len()];
        for at in 0..label.len() {
            if label[at] != wanted {
                continue;
            }
            let begun = match &best {
                None => Some(0),
                Some(_) if after_space => ended_before[at],
                Some(_) => ended_in[word[at].saturating_sub(1)],
            }
            .filter(|_| starts_word(at))
            .map(|score| score - RUN);
            let continued = match &best {
                Some(best) if at > 0 && !after_space => best[at - 1],
                _ => None,
            };
            next[at] = begun.max(continued);
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

    #[test]
    fn the_top_and_nested_rows_show_their_keys() {
        let mut hosts = Rows::new("hosts");
        hosts.push(Row::run("a", "attach", |_, _, _| {}));
        let mut deeper = Rows::new("deeper");
        deeper.push(Row::run("x", "leaf", |_, _, _| {}));
        hosts.push(Row::menu("d", "deeper", deeper));
        // Two top rows on one key, and one name twice: none is dropped.
        let top = vec![
            Row::run("h", "home", |_, _, _| {}).with_shortcut("space h".to_owned()),
            Row::menu("shift-h", "hosts", hosts),
            Row::run("h", "pair this device", |_, _, _| {}),
            Row::run("", "home", |_, _, _| {}),
        ];
        let entries = flatten(&top);
        let seen: Vec<(&str, &str)> = entries
            .iter()
            .map(|entry| (entry.label.as_str(), entry.keys.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("home", "space h"),
                ("hosts", ""),
                ("hosts › attach", "a"),
                ("hosts › deeper", "d"),
                ("hosts › deeper › leaf", "d x"),
                ("pair this device", ""),
                ("home (2)", ""),
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
        // A space begins a new run, at any later word; runs typed
        // together begin words next to each other.
        assert_eq!(word_starts("ne w", "new"), None);
        assert_eq!(word_starts("new ch", "new channel"), Some(-2));
        assert_eq!(word_starts("fix flaky", "fix the flaky test"), Some(2));
        assert_eq!(word_starts("ff", "fix the flaky test"), None);
        assert_eq!(
            word_starts(
                "4pm",
                "deploy event 4: build #4103 of rho-web passed on main"
            ),
            None
        );
        assert_eq!(word_starts("ha", "hosts › attach"), Some(-6));
    }

    fn command(label: &str) -> Row {
        Row::run("", label, |_, _, _| {})
    }

    fn place(title: &str, description: &str, recency: u64) -> Found {
        Found {
            address: Address::new("thread", title),
            title: title.to_owned(),
            description: description.to_owned(),
            recency,
        }
    }

    fn listing() -> Listing {
        let mut hosts = Rows::new("hosts");
        hosts.push(Row::run("a", "attach", |_, _, _| {}));
        let commands = flatten(&[
            command("home").window_wide(),
            Row::menu("", "hosts", hosts).window_wide(),
            command("show my secret phrase").window_wide(),
            command("start agent here"),
            command("start agent, choosing where and runtime…"),
            command("name this thread").also("rename"),
        ]);
        Listing::new(
            commands,
            vec![
                place("privacy VM", "rho › infra", 10),
                place("#sales-8", "slack · acme", 30),
                place("debug", "rho · channel", 20),
                place("debug", "infra › deploys", 40),
                place("sol", "rho · waiting", 5),
            ],
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
    fn nothing_typed_lists_the_screens_commands_then_places_then_the_windows() {
        assert_eq!(
            names(&listing(), ""),
            [
                "start agent here | ",
                "start agent, choosing where and runtime… | ",
                "name this thread | ",
                "debug | infra › deploys",
                "#sales-8 | slack · acme",
                "debug | rho · channel",
                "privacy VM | rho › infra",
                "sol | rho · waiting",
                "home | ",
                "hosts | ",
                "hosts › attach | a",
                "show my secret phrase | ",
            ]
        );
    }

    #[test]
    fn typing_narrows_across_commands_and_places() {
        // A thread's name finds the thread, and nothing else.
        assert_eq!(names(&listing(), "privacy"), ["privacy VM | rho › infra"]);
        // One run that begins a word answers better than two; between
        // two rows that answer alike, the screen's command comes first.
        assert_eq!(
            names(&listing(), "sa"),
            [
                "#sales-8 | slack · acme",
                "start agent here | ",
                "start agent, choosing where and runtime… | ",
            ]
        );
        assert_eq!(names(&listing(), "ho")[..2], ["home | ", "hosts | "]);
    }

    #[test]
    fn a_rare_command_is_found_by_its_word_and_nothing_else_answers() {
        assert_eq!(names(&listing(), "rename"), ["name this thread | "]);
        assert_eq!(names(&listing(), "secret"), ["show my secret phrase | "]);
        assert!(names(&listing(), "pair").is_empty());
    }

    #[test]
    fn letters_inside_a_word_answer_last() {
        let listing = Listing::new(
            flatten(&[command("deploy now")]),
            vec![place("redeploy", "rho › infra", 1)],
        );
        // `deploy` begins a word of the command and is inside the place's.
        assert_eq!(
            names(&listing, "deploy"),
            ["deploy now | ", "redeploy | rho › infra"]
        );
        // Two letters inside a word are chance, not an answer.
        assert!(names(&listing, "pl").is_empty());
    }

    #[test]
    fn a_place_answers_by_where_it_is_when_its_name_does_not() {
        assert_eq!(names(&listing(), "waiting"), ["sol | rho · waiting"]);
        // Both threads called `debug` are listed, newest first, each with
        // its channel beside it.
        assert_eq!(
            names(&listing(), "debug"),
            ["debug | infra › deploys", "debug | rho · channel"]
        );
    }

    #[test]
    fn a_name_that_answers_beats_a_where_that_answers() {
        // `ops` begins no word of `@Rob Pike` and is not in order in where
        // it is; `deploy` answers only by where it is, after `ops`.
        let listing = Listing::new(
            Vec::new(),
            vec![
                place("@Rob Pike", "slack · acme", 50),
                place("deploy", "rho › ops", 40),
                place("ops", "qa · channel", 1),
            ],
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
        let listing = Listing::new(Vec::new(), places);
        let ranked = listing.rank("");
        assert_eq!(ranked.len(), LISTED);
        assert_eq!(
            listing.choices[ranked[0]].name(),
            format!("thread {}", LISTED + 19)
        );
    }
}
