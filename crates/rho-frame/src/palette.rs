//! `:`: every command by name.
//!
//! The menus are data ([`Rows`]), so the palette is the same rows read
//! another way: flattened to one list, found by a fuzzy query instead of
//! a key. A row keeps the key it has in the menus as a hint beside its
//! name, so the palette teaches the keys it replaces.

use std::cmp::Reverse;

use crate::menu::{Pick, Row, Rows, Scope};

/// One command in the palette.
pub(crate) struct Entry {
    /// What the reader types toward: the row's words, under the menus
    /// above it (`hosts › attach`). Unique in the list.
    pub label: String,
    /// The keys that reach it inside the menus above it once the first of
    /// them is open (`a` in `hosts › attach`); none for the top of the list,
    /// which no menu holds.
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
            keys: String::new(),
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
            keys: keys.clone(),
            pick: row.pick.clone(),
            scope,
        });
        if let Pick::Menu(rows) = &row.pick {
            walk(rows, &label, &keys, scope, into);
        }
    }
}

/// The entries that answer `query`, best first, by index. With nothing
/// typed, the screen's commands and the thing's under the cursor come
/// first in the order they were offered, and the window's own after them
/// (`window.Palette`). With a query, the best match comes first, and a tie
/// goes the same way.
pub(crate) fn rank(entries: &[Entry], query: &str) -> Vec<usize> {
    let typed = !query.trim().is_empty();
    let mut found: Vec<_> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let score = score(query, &entry.label)?;
            Some((Reverse(typed.then_some(score)), entry.scope, index))
        })
        .collect();
    found.sort_unstable();
    found.into_iter().map(|(.., index)| index).collect()
}

/// How well `label` answers `query`: `None` when some letter of the query
/// is not in the label in order. Higher is better: letters found at the
/// start of a word count most, letters right after the last one found
/// next, and a shorter label wins a tie. An empty query matches all alike.
pub(crate) fn score(query: &str, label: &str) -> Option<u32> {
    let label: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0u32;
    let mut at = 0usize;
    let mut previous: Option<usize> = None;
    for wanted in query.chars().flat_map(char::to_lowercase) {
        if wanted.is_whitespace() {
            continue;
        }
        let found = (at..label.len()).find(|index| label[*index] == wanted)?;
        let word_start = found == 0 || !label[found - 1].is_alphanumeric();
        score += match (word_start, previous == Some(found.wrapping_sub(1))) {
            (true, _) => 3,
            (false, true) => 2,
            (false, false) => 1,
        };
        previous = Some(found);
        at = found + 1;
    }
    Some(score * 100 + 100u32.saturating_sub(label.len() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_top_shows_no_key_and_a_menu_shows_its_own() {
        let mut hosts = Rows::new("hosts");
        hosts.push(Row::run("a", "attach", |_, _, _| {}));
        let mut deeper = Rows::new("deeper");
        deeper.push(Row::run("x", "leaf", |_, _, _| {}));
        hosts.push(Row::menu("d", "deeper", deeper));
        // Two top rows on one key, and one name twice: none is dropped.
        let top = vec![
            Row::run("h", "home", |_, _, _| {}),
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
                ("home", ""),
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
    fn the_screens_commands_come_before_the_windows() {
        let mut hosts = Rows::new("hosts");
        hosts.push(Row::run("a", "attach", |_, _, _| {}));
        let top = vec![
            Row::run("", "home", |_, _, _| {}).window_wide(),
            Row::menu("", "hosts", hosts).window_wide(),
            Row::run("", "quit", |_, _, _| {}).window_wide(),
            Row::run("x", "shell", |_, _, _| {}),
            Row::run("t", "terminal", |_, _, _| {}),
            Row::run("shift-w", "notes", |_, _, _| {}).window_wide(),
            Row::run("c", "cancel the request", |_, _, _| {}),
        ];
        let entries = flatten(&top);
        let listed = |query: &str| -> Vec<&str> {
            rank(&entries, query)
                .into_iter()
                .map(|index| entries[index].label.as_str())
                .collect()
        };
        assert_eq!(
            listed(""),
            [
                "shell",
                "terminal",
                "cancel the request",
                "home",
                "hosts",
                "hosts › attach",
                "quit",
                "notes",
            ]
        );
        assert_eq!(listed("ho")[0], "home");
        assert_eq!(listed("te"), ["terminal", "cancel the request", "notes"]);
    }

    #[test]
    fn a_query_is_letters_in_order_and_word_starts_win() {
        assert_eq!(score("", "anything"), Some(100 - 8));
        assert!(score("sa", "start agent").unwrap() > score("sa", "messages").unwrap());
        assert!(score("det", "hosts › detach").unwrap() > score("det", "delete…").unwrap());
        assert_eq!(score("z", "home"), None);
        assert!(score("home", "home").unwrap() > score("home", "hosts › home").unwrap());
    }
}
