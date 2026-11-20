//! `:`: every command and place by name. The row's fields are also the
//! search fields, so the match can point back to the letters the user sees.

use std::cmp::Reverse;
use std::ops::Range;

use rho_dealer::Card;

use crate::menu::{Pick, Row, Scope, Shortcut};
use crate::system::Found;

// Eight rows are visible. Beyond this many results the user types more.
const LISTED: usize = 100;

pub(crate) struct Entry {
    also: Vec<String>,
    keys: String,
    pub pick: Pick,
    scope: Scope,
}

pub(crate) fn commands(
    rows: &[Row],
    resolve: impl Fn(&dyn gpui::Action) -> Option<String>,
) -> Vec<Entry> {
    rows.iter()
        .map(|row| Entry {
            also: row.also.clone(),
            keys: match &row.shortcut {
                Some(Shortcut::Menu(keys)) => Some(keys.clone()),
                Some(Shortcut::Action(action)) => resolve(action.as_ref()),
                None => None,
            }
            .unwrap_or_default(),
            pick: row.pick.clone(),
            scope: row.scope,
        })
        .collect()
}

pub(crate) struct Place {
    pub found: Found,
    pub card: Option<Card>,
    // Index in the window's visit history (most recent first).
    pub visit: Option<usize>,
    pub current: bool,
    pub shortcut: Option<String>,
}

pub(crate) enum Choice {
    Command(Entry),
    Place(Place),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    OtherPlace,
    CurrentPlace,
    Command,
}

impl Choice {
    pub fn name(&self) -> &str {
        match self {
            Self::Command(entry) => entry.pick.label().trim_end_matches('…'),
            Self::Place(place) => &place.found.title,
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Command(_) => "",
            Self::Place(place) => match &place.card {
                Some(card) => &card.label,
                None => place
                    .found
                    .thread
                    .as_ref()
                    .and_then(|thread| thread.status.as_deref())
                    .unwrap_or(""),
            },
        }
    }

    fn beside(&self) -> &str {
        match self {
            Self::Command(entry) => &entry.keys,
            Self::Place(place) => place
                .shortcut
                .as_deref()
                .unwrap_or(&place.found.description),
        }
    }

    fn aliases(&self) -> &[String] {
        match self {
            Self::Command(entry) => &entry.also,
            Self::Place(_) => &[],
        }
    }

    // Blank input lists only places we have been, places that wait, and
    // commands. A visited waiting place keeps its card words in both cases.
    fn blank(&self) -> Option<(u8, usize)> {
        match self {
            Self::Place(Place { current: true, .. }) => None,
            Self::Place(Place {
                visit: Some(visit), ..
            }) => Some((0, *visit)),
            Self::Place(Place {
                visit: None,
                card: Some(_),
                ..
            }) => Some((1, 0)),
            Self::Place(_) => None,
            Self::Command(entry) => Some((if entry.scope == Scope::Here { 2 } else { 3 }, 0)),
        }
    }

    fn group(&self) -> Group {
        match self {
            Self::Place(place) if place.current => Group::CurrentPlace,
            Self::Place(_) => Group::OtherPlace,
            Self::Command(_) => Group::Command,
        }
    }

    fn query_order(&self) -> (bool, usize, Reverse<u64>) {
        match self {
            Self::Place(place) => (
                place.card.is_none(),
                // Only threads break ties by visits; the chart catalogue,
                // for example, always offers the 7-day rate limit first.
                place
                    .found
                    .thread
                    .as_ref()
                    .and(place.visit)
                    .unwrap_or(usize::MAX),
                Reverse(place.found.recency),
            ),
            Self::Command(_) => (true, usize::MAX, Reverse(0)),
        }
    }

    pub fn fields(&self) -> [&str; 3] {
        [self.name(), self.detail(), self.beside()]
    }

    pub fn attention(&self) -> bool {
        matches!(self, Self::Place(Place { card: Some(_), .. }))
    }

    pub fn has_keys(&self) -> bool {
        match self {
            Self::Command(_) => true,
            Self::Place(place) => place.shortcut.is_some(),
        }
    }

    fn matches(&self, words: &[String]) -> Option<(usize, [Vec<Range<usize>>; 3])> {
        let fields = self.fields();
        let folded = fields.map(Folded::new);
        let aliases: Vec<_> = self
            .aliases()
            .iter()
            .map(|word| Folded::new(word))
            .collect();
        let mut marks: [Vec<Range<usize>>; 3] = Default::default();
        let mut score = 0;
        for word in words {
            let (quality, field, range) = folded
                .iter()
                // Command key hints are metadata, not words to match.
                .take(if self.has_keys() { 1 } else { 3 })
                .chain(aliases.iter())
                .enumerate()
                .filter_map(|(field, text)| {
                    let (quality, range) = text.find(word)?;
                    Some((quality, field, range))
                })
                .min_by_key(|(quality, field, _)| (*quality, *field))?;
            // Whole words before prefixes before substrings; a name
            // match beats an equally good match in the words beside it.
            score += quality * 2 + usize::from(field != 0 && field < 3);
            if let Some(ranges) = marks.get_mut(field)
                && (!self.has_keys() || field == 0)
            {
                ranges.push(range);
            }
        }
        for ranges in &mut marks {
            ranges.sort_unstable_by_key(|range| range.start);
            let mut merged: Vec<Range<usize>> = Vec::new();
            for range in ranges.drain(..) {
                if let Some(last) = merged.last_mut()
                    && range.start <= last.end
                {
                    last.end = last.end.max(range.end);
                } else {
                    merged.push(range);
                }
            }
            *ranges = merged;
        }
        Some((score, marks))
    }
}

// For each byte of the folded text, keep the corresponding display character
// span. Lowercasing can change the number of bytes (İ -> i + combining dot).
struct Folded {
    text: String,
    display: Vec<Range<usize>>,
}

impl Folded {
    fn new(text: &str) -> Self {
        let mut folded = Self {
            text: String::new(),
            display: Vec::new(),
        };
        for (offset, character) in text.char_indices() {
            for lower in character.to_lowercase() {
                folded.text.push(lower);
                folded.display.extend(std::iter::repeat_n(
                    offset..offset + character.len_utf8(),
                    lower.len_utf8(),
                ));
            }
        }
        folded
    }

    fn find(&self, word: &str) -> Option<(usize, Range<usize>)> {
        self.text
            .char_indices()
            .filter(|(start, _)| self.text[*start..].starts_with(word))
            .map(|(start, _)| {
                let end = start + word.len();
                let boundary_before = !self.text[..start].ends_with(char::is_alphanumeric);
                let boundary_after = !self.text[end..].starts_with(char::is_alphanumeric);
                let quality = match (boundary_before, boundary_after) {
                    (true, true) => 0,
                    (true, false) => 1,
                    (false, _) => 2,
                };
                (
                    quality,
                    self.display[start].start..self.display[end - 1].end,
                )
            })
            .min_by_key(|(quality, range)| (*quality, range.start))
    }
}

pub(crate) struct Matched {
    pub index: usize,
    pub marks: [Vec<Range<usize>>; 3],
}

pub(crate) struct Listing {
    pub choices: Vec<Choice>,
}

impl Listing {
    pub fn new(commands: Vec<Entry>, places: Vec<Place>) -> Self {
        let choices = places
            .into_iter()
            .map(Choice::Place)
            .chain(commands.into_iter().map(Choice::Command))
            .collect();
        Self { choices }
    }

    pub fn rank(&self, query: &str) -> Vec<Matched> {
        let query = query.trim();
        let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            let mut rows: Vec<_> = self
                .choices
                .iter()
                .enumerate()
                .filter_map(|(index, choice)| Some((choice.blank()?, index)))
                .collect();
            rows.sort_unstable();
            return rows
                .into_iter()
                .take(LISTED)
                .map(|(_, index)| Matched {
                    index,
                    marks: Default::default(),
                })
                .collect();
        }
        let mut rows: Vec<_> = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                let (score, marks) = choice.matches(&words)?;
                Some((
                    (
                        choice.group(),
                        !choice.name().eq_ignore_ascii_case(query),
                        score,
                        choice.query_order(),
                        index,
                    ),
                    Matched { index, marks },
                ))
            })
            .collect();
        rows.sort_unstable_by_key(|(key, _)| *key);
        rows.into_iter().take(LISTED).map(|(_, row)| row).collect()
    }
}

#[cfg(test)]
mod tests {
    use rho_dealer::Address;

    use super::*;
    use crate::menu::Rows;
    use crate::system::FoundThread;

    fn thread(
        title: &str,
        project: &str,
        visit: Option<usize>,
        current: bool,
        card: Option<&str>,
    ) -> Place {
        let address = Address::new("thread", project);
        Place {
            found: Found {
                address: address.clone(),
                title: title.into(),
                description: project.into(),
                thread: Some(FoundThread {
                    status: Some("Checking timing".into()),
                }),
                recency: 0,
            },
            card: card.map(|words| Card {
                address,
                title: title.into(),
                context: project.into(),
                label: words.into(),
                kind: rho_dealer::rank::CardKind::Other,
                priority: 1.,
                cursor: "".into(),
                skipped: false,
            }),
            visit,
            current,
            shortcut: None,
        }
    }

    fn ids(listing: &Listing, query: &str) -> Vec<usize> {
        listing
            .rank(query)
            .into_iter()
            .map(|row| row.index)
            .collect()
    }

    #[test]
    fn blank_leaves_out_current_and_unvisited_quiet_places() {
        let places = vec![
            thread("old", "ice@lab", Some(3), false, Some("asks · 1h")),
            thread("current", "reef@lab", None, true, Some("asks · 1h")),
            thread("unvisited", "floe@lab", None, false, None),
            thread("waiting", "wave@lab", None, false, Some("asks · 1h")),
            thread("recent", "foam@lab", Some(0), false, None),
        ];
        let rows = commands(&[Row::run("", "new project", |_, _, _| {})], |_| None);
        let listing = Listing::new(rows, places);
        assert_eq!(ids(&listing, " "), [4, 0, 3, 5]);
        assert_eq!(ids(&listing, "urrent"), [1]);
    }

    #[test]
    fn waiting_beats_the_most_recent_visit_on_equal_subjects() {
        let listing = Listing::new(
            vec![],
            vec![
                thread("Flaky clock", "ice@lab", Some(0), false, None),
                thread("Flaky clock", "wave@lab", None, false, Some("asks · 2h")),
                thread("Flaky clock", "reef@lab", Some(4), false, None),
            ],
        );
        assert_eq!(ids(&listing, "fl"), [1, 0, 2]);
        assert_eq!(ids(&listing, "ck fla"), [1, 0, 2]);
        assert_eq!(ids(&listing, "ave flaky asks"), [1]);
        assert!(ids(&listing, "asks ice").is_empty());
        assert_eq!(ids(&listing, "timing ice"), [0]);
    }

    #[test]
    fn matching_marks_the_display_bytes_in_all_fields() {
        let listing = Listing::new(
            vec![],
            vec![thread(
                "İstanbul clock",
                "wave@lab",
                None,
                false,
                Some("asks · 2h"),
            )],
        );
        let rows = listing.rank("la 2h I clo lock");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].marks, [vec![0..2, 10..15], vec![8..10], vec![5..7]]);
    }

    #[test]
    fn a_menu_is_one_command_whose_choices_are_not_search_results() {
        let top = [
            Row::menu(
                "a",
                Rows::new("start agent").row(Row::run("h", "here", |_, _, _| {})),
            ),
            Row::menu(
                "s",
                Rows::new("snooze").row(Row::run("m", "minutes", |_, _, _| {})),
            )
            .bound_to(crate::keys::VerdictMenu),
        ];
        let rows = commands(&top, |_| Some("space s".to_owned()));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pick.label(), "start agent");
        assert!(rows[0].keys.is_empty());
        assert_eq!(rows[1].keys, "space s");
        assert!(
            matches!(&rows[1].pick, Pick::Menu(menu) if menu.rows[0].pick.label() == "minutes")
        );
        let listing = Listing::new(rows, vec![]);
        assert_eq!(ids(&listing, "snooze"), [1]);
        assert!(ids(&listing, "minutes").is_empty());
    }

    #[test]
    fn commands_follow_places_and_the_current_place_is_last_in_its_group() {
        let listing = Listing::new(
            commands(&[Row::run("", "clock", |_, _, _| {})], |_| None),
            vec![
                thread("clock", "wave@lab", Some(0), true, Some("asks · 1h")),
                thread("Broken Clock Test", "ice@lab", None, false, None),
                thread("Clockwork Build", "reef@lab", None, false, None),
            ],
        );
        assert_eq!(ids(&listing, "clock"), [1, 2, 0, 3]);
        assert_eq!(ids(&listing, "wave"), [0]);
    }

    #[test]
    fn an_exact_command_beats_a_more_specific_command_offered_first() {
        let rows = commands(
            &[
                Row::run("", "done with this mail", |_, _, _| {}),
                Row::run("d", "done", |_, _, _| {}),
            ],
            |_| None,
        );
        let listing = Listing::new(rows, vec![]);
        assert_eq!(ids(&listing, "done"), [1, 0]);
    }

    #[test]
    fn usage_prefix_selects_the_first_chart_even_after_visiting_another() {
        let place = |title: &str, visit| Place {
            found: Found {
                address: Address::new("usage", title),
                title: title.into(),
                description: String::new(),
                thread: None,
                recency: 0,
            },
            card: None,
            visit,
            current: false,
            shortcut: None,
        };
        let listing = Listing::new(
            vec![],
            vec![
                place("usage › rate limit · 7d", None),
                place("usage › model usage share · 30d", Some(0)),
            ],
        );
        assert_eq!(ids(&listing, "usage"), [0, 1]);
        assert_eq!(ids(&listing, "usage 30d"), [1]);
    }
}
