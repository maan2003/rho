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

    /// How well the row answers `query`: by its name, or, for a place
    /// whose name does not answer, by its name and where it is together,
    /// so `rho` finds the threads of the channel `rho` and `waiting` the
    /// agents waiting. A row that answers by its name is a better match
    /// than any that answers only by where it is.
    fn answers(&self, query: &str) -> Option<(bool, u32)> {
        if let Some(quality) = quality(query, self.name()) {
            return Some((true, quality));
        }
        match self {
            Choice::Command(_) => None,
            Choice::Place(found) => {
                quality(query, &format!("{} {}", found.title, found.description))
                    .map(|quality| (false, quality))
            }
        }
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
                let answers = choice.answers(query)?;
                Some((
                    Reverse(answers),
                    choice.half() != self.first,
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

/// How well `label` answers `query`: `None` when some letter of the query
/// is not in the label in order. Higher is better: a letter found at the
/// start of a word counts most, a letter right after the one before it
/// next, and the best way to place the letters is the one counted, so
/// `sa` answers `start agent here` by its two word starts. An empty query
/// matches all alike.
fn quality(query: &str, label: &str) -> Option<u32> {
    let label: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let word_start = |at: usize| at == 0 || !label[at - 1].is_alphanumeric();
    // `best[at]`: the best count for the letters so far with the last of
    // them at `at`.
    let mut best: Option<Vec<Option<u32>>> = None;
    for wanted in query.chars().flat_map(char::to_lowercase) {
        if wanted.is_whitespace() {
            continue;
        }
        let mut next = vec![None; label.len()];
        let mut before: Option<u32> = None;
        for at in 0..label.len() {
            // The best placing of the letters before, ending before `at`.
            let ended_before = match &best {
                None => Some(0),
                Some(_) => before,
            };
            if label[at] == wanted {
                let apart = ended_before.map(|count| count + if word_start(at) { 3 } else { 1 });
                let adjacent = match &best {
                    Some(best) if at > 0 => {
                        best[at - 1].map(|count| count + if word_start(at) { 3 } else { 2 })
                    }
                    _ => None,
                };
                next[at] = apart.max(adjacent);
            }
            if let Some(best) = &best {
                before = before.max(best[at]);
            }
        }
        best = Some(next);
    }
    match best {
        None => Some(0),
        Some(best) => best.into_iter().flatten().max(),
    }
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
    fn a_query_is_letters_in_order_and_word_starts_win() {
        assert_eq!(quality("", "anything"), Some(0));
        assert!(quality("sa", "start agent").unwrap() > quality("sa", "messages").unwrap());
        assert!(quality("det", "hosts › detach").unwrap() > quality("det", "delete…").unwrap());
        assert_eq!(quality("z", "home"), None);
        // The best placing counts, not the first: `a` of `agent`, not of
        // `start`.
        assert_eq!(quality("sa", "start agent here"), Some(6));
        assert_eq!(quality("sa", "standup"), Some(4));
        assert_eq!(quality("ag", "agent"), Some(5));
    }

    fn command(label: &str) -> Entry {
        let mut rows = Rows::new("rho");
        rows.push(Row::run("k", label, |_, _, _| {}));
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
            ],
            vec![
                place("privacy VM", "rho › infra", 10),
                place("standup", "slack · acme", 30),
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
                "start agent here | k",
                "debug | infra › deploys",
                "standup | slack · acme",
                "debug | rho · channel",
                "privacy VM | rho › infra",
                "sol | rho · waiting",
            ]
        );
        assert_eq!(
            names(&listing(First::Places), "")[..6],
            [
                "debug | infra › deploys",
                "standup | slack · acme",
                "debug | rho · channel",
                "privacy VM | rho › infra",
                "sol | rho · waiting",
                "home | k",
            ]
        );
    }

    #[test]
    fn typing_narrows_across_both_halves() {
        // `:` then a thread's name finds the thread.
        assert_eq!(
            names(&listing(First::Commands), "privacy"),
            ["privacy VM | rho › infra"]
        );
        // `ctrl-shift-f` then `sa` finds the command, ahead of a place
        // that answers worse (`standup`: s at a word start, a inside).
        let found = names(&listing(First::Places), "sa");
        assert_eq!(found[0], "start agent here | k");
        assert!(found.contains(&"standup | slack · acme".to_owned()));
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
    fn a_name_that_answers_beats_a_where_that_answers_better() {
        // `ops` is the channel's whole name, but `@Rob Pike slack · acme`
        // has the letters at three word starts once its where is read.
        let listing = Listing::new(
            Vec::new(),
            vec![
                place("@Rob Pike", "slack · acme", 50),
                place("ops", "qa · channel", 1),
            ],
            First::Places,
        );
        assert_eq!(
            names(&listing, "ops"),
            ["ops | qa · channel", "@Rob Pike | slack · acme"]
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
