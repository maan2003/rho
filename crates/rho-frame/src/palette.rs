//! `:`: every command by name.
//!
//! The menus are data ([`Rows`]), so the palette is the same rows read
//! another way: flattened to one list, found by a fuzzy query instead of
//! a key. A row keeps the key it has in the menus as a hint beside its
//! name, so the palette teaches the keys it replaces.

use crate::menu::{Pick, Rows};

/// One command in the palette.
pub(crate) struct Entry {
    /// What the reader types toward: the row's words, under the menus
    /// above it (`hosts › attach`). Unique in the list.
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
    let mut seen: Vec<String> = Vec::new();
    for entry in &mut entries {
        if seen.contains(&entry.label) {
            entry.label = format!("{} ({})", entry.label, entry.keys);
        }
        seen.push(entry.label.clone());
    }
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
                ("home (x)", "x"),
            ]
        );
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
