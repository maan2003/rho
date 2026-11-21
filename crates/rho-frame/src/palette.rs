//! `:`: commands and places by name, and excerpts of remembered words.

use std::cmp::Reverse;
use std::ops::Range;

use memchr::memmem::Finder;
use rho_dealer::Card;

use crate::menu::{Pick, Row, Scope, Shortcut};
use crate::system::{Found, FoundText};
use crate::text_search::{TextQuery, TextScope};

// Eight rows are visible. Beyond this many results the user types more.
pub(crate) const LISTED: usize = 100;
const DAY_MS: u64 = 86_400_000;

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
    pub visited_ms: Option<u64>,
    pub current: bool,
    pub shortcut: Option<String>,
}

pub(crate) enum Choice {
    Command(Entry),
    Place(Place),
    Words(FoundText),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    ExactCommand,
    ExactPlace,
    CommandAtWordStarts,
    OtherPlace,
    Text,
    CurrentPlace,
    OtherCommand,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Strength {
    ExactName,
    NamePrefix,
    WordStarts,
    WordsInOrder,
    Fuzzy,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Hit {
    WholeWord,
    WordStart,
    InsideWord,
}

impl Choice {
    pub fn name(&self) -> &str {
        match self {
            Self::Command(entry) => entry.pick.label().trim_end_matches('…'),
            Self::Place(place) => &place.found.title,
            Self::Words(found) => &found.title,
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Command(_) => "",
            Self::Words(found) => &found.line,
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
            Self::Words(_) => "",
            Self::Place(place) => place
                .shortcut
                .as_deref()
                .unwrap_or(&place.found.description),
        }
    }

    fn aliases(&self) -> &[String] {
        match self {
            Self::Command(entry) => &entry.also,
            Self::Place(_) | Self::Words(_) => &[],
        }
    }

    // Blank input lists only places we have been, places that wait, and
    // commands. A visited waiting place keeps its card words in both cases.
    fn blank(&self, desk: bool) -> Option<(u8, usize)> {
        match self {
            Self::Place(Place { current: true, .. }) | Self::Words(_) => None,
            Self::Place(Place { card: Some(_), .. }) if desk => None,
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

    fn group(&self, strength: Strength) -> Group {
        match (self, strength) {
            (Self::Place(place), _) if place.current => Group::CurrentPlace,
            (Self::Place(_), Strength::ExactName) => Group::ExactPlace,
            (Self::Place(_), _) => Group::OtherPlace,
            (Self::Command(_), Strength::ExactName) => Group::ExactCommand,
            (Self::Command(_), Strength::WordStarts | Strength::NamePrefix) => {
                Group::CommandAtWordStarts
            }
            (Self::Command(_), _) => Group::OtherCommand,
            (Self::Words(_), _) => unreachable!("text is ranked by its matching line"),
        }
    }

    fn activity(&self) -> Option<u64> {
        match self {
            Self::Place(place) if place.found.thread.is_some() => {
                Some(place.found.recency.max(place.visited_ms.unwrap_or(0)))
            }
            _ => None,
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
            Self::Words(_) => false,
        }
    }
}

type FieldHit = (Hit, usize, Range<usize>);

struct Named {
    fields: [Folded; 3],
    aliases: Vec<Folded>,
    searchable_fields: usize,
}

impl Named {
    fn new(choice: &Choice) -> Self {
        Self {
            fields: choice.fields().map(Folded::new),
            aliases: choice
                .aliases()
                .iter()
                .map(|word| Folded::new(word))
                .collect(),
            searchable_fields: if choice.has_keys() { 1 } else { 3 },
        }
    }

    fn hits(&self, finder: &Finder<'_>) -> (Option<Hit>, Option<FieldHit>) {
        let name = best_hit(&self.fields[0].text, finder);
        let name_quality = name.as_ref().map(|(quality, _)| *quality);
        let mut best = name.map(|(hit, range)| (hit, 0, range));
        if name_quality == Some(Hit::WholeWord) {
            return (name_quality, best);
        }
        for (field, text) in self.fields[..self.searchable_fields]
            .iter()
            .enumerate()
            .skip(1)
            .chain(self.aliases.iter().map(|alias| (3, alias)))
        {
            if let Some((hit, range)) = best_hit(&text.text, finder)
                && best.as_ref().is_none_or(|(prior, _, _)| hit < *prior)
            {
                best = Some((hit, field, range));
                if hit == Hit::WholeWord {
                    break;
                }
            }
        }
        (name_quality, best)
    }

    fn qualities(&self, byte: u8) -> Option<(Option<Hit>, Hit, usize)> {
        let name = self.fields[0].quality(byte);
        let mut best = name.map(|hit| (hit, 0));
        if name != Some(Hit::WholeWord) {
            for (field, text) in self.fields[..self.searchable_fields]
                .iter()
                .enumerate()
                .skip(1)
                .chain(self.aliases.iter().map(|alias| (3, alias)))
            {
                if let Some(hit) = text.quality(byte)
                    && best.is_none_or(|(prior, _)| hit < prior)
                {
                    best = Some((hit, field));
                    if hit == Hit::WholeWord {
                        break;
                    }
                }
            }
        }
        let (hit, field) = best?;
        Some((name, hit, field))
    }

    fn score(&self, query: &TextQuery<'_>, command: bool) -> Option<(Strength, usize)> {
        let name = self.fields[0].text.as_str();
        let mut score = 0;
        let mut all_in_name = true;
        let mut all_at_starts = true;
        // The last word is the one being typed; reject it before rescoring
        // the preceding terms. Ordered name strength is checked separately.
        for term in query.terms.iter().rev() {
            let (name_hit, quality, field) = if let &[byte] = term.needle() {
                self.qualities(byte)?
            } else {
                let (name_hit, best) = self.hits(term);
                let (quality, field, _) = best?;
                (name_hit, quality, field)
            };
            score += quality as usize * 2 + usize::from(field != 0 && field < 3);
            all_in_name &= name_hit.is_some();
            all_at_starts &= name_hit.is_some_and(|hit| hit != Hit::InsideWord);
        }
        let one_term = query.terms.len() == 1;
        let strength = if name == query.text {
            Strength::ExactName
        } else if name.starts_with(query.text) {
            Strength::NamePrefix
        } else if all_at_starts && (command || one_term || ordered_starts(name, &query.terms)) {
            Strength::WordStarts
        } else if all_in_name
            && (one_term
                || query
                    .terms
                    .iter()
                    .try_fold(name.as_bytes(), |rest, term| {
                        let at = term.find(rest)?;
                        Some(&rest[at + term.needle().len()..])
                    })
                    .is_some())
        {
            Strength::WordsInOrder
        } else {
            Strength::Fuzzy
        };
        Some((strength, score))
    }

    fn marks(&self, query: &TextQuery<'_>) -> [Vec<Range<usize>>; 3] {
        let mut marks: [Vec<Range<usize>>; 3] = Default::default();
        for term in &query.terms {
            if let Some((_, field, range)) = self.hits(term).1
                && let Some(ranges) = marks.get_mut(field)
            {
                ranges.push(self.fields[field].display(range));
            }
        }
        marks.iter_mut().for_each(merge_marks);
        marks
    }
}

fn ordered_starts(name: &str, terms: &[Finder<'_>]) -> bool {
    let mut end = 0;
    for term in terms {
        let mut from = end;
        let at = loop {
            let Some(offset) = term.find(&name.as_bytes()[from..]) else {
                return false;
            };
            let at = from + offset;
            if word_start(name, at) {
                break at;
            }
            from = at + 1;
        };
        end = at + term.needle().len();
    }
    true
}

fn word_start(text: &str, at: usize) -> bool {
    match at.checked_sub(1).map(|last| text.as_bytes()[last]) {
        None => true,
        Some(byte) if byte.is_ascii() => !byte.is_ascii_alphanumeric(),
        Some(_) => !text[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric),
    }
}

fn word_end(text: &str, at: usize) -> bool {
    match text.as_bytes().get(at).copied() {
        None => true,
        Some(byte) if byte.is_ascii() => !byte.is_ascii_alphanumeric(),
        Some(_) => !text[at..].chars().next().is_some_and(char::is_alphanumeric),
    }
}

pub(crate) fn best_hit(text: &str, term: &Finder<'_>) -> Option<(Hit, Range<usize>)> {
    let mut best = None;
    let mut from = 0;
    while let Some(offset) = term.find(&text.as_bytes()[from..]) {
        let start = from + offset;
        let end = start + term.needle().len();
        let quality = if !word_start(text, start) {
            Hit::InsideWord
        } else if word_end(text, end) {
            return Some((Hit::WholeWord, start..end));
        } else {
            Hit::WordStart
        };
        if best.as_ref().is_none_or(|(old, _)| quality < *old) {
            best = Some((quality, start..end));
        }
        // Both strings are UTF-8, so a nonempty needle never begins in a
        // continuation byte even when we advance only one byte.
        from = start + 1;
    }
    best
}

fn merge_marks(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut kept = 0;
    for index in 0..ranges.len() {
        if kept != 0 && ranges[index].start <= ranges[kept - 1].end {
            ranges[kept - 1].end = ranges[kept - 1].end.max(ranges[index].end);
        } else {
            ranges.swap(kept, index);
            kept += 1;
        }
    }
    ranges.truncate(kept);
}

// For length-changing folds, map every folded byte to the display character.
// ASCII and length-preserving Unicode need no offset table.
struct Folded {
    text: String,
    display: Vec<Range<usize>>,
    ascii_hits: [u128; 3],
}

impl Folded {
    fn new(text: &str) -> Self {
        let mut folded = Self::for_display(text);
        folded.ascii_hits = ascii_hits(&folded.text);
        folded
    }

    fn for_display(text: &str) -> Self {
        if text.is_ascii() {
            let text = text.to_ascii_lowercase();
            return Self {
                text,
                display: Vec::new(),
                ascii_hits: [0; 3],
            };
        }
        let lower = text.to_lowercase();
        let preserving = text.chars().all(|character| {
            let mut folded = character.to_lowercase();
            folded
                .next()
                .is_some_and(|lower| lower.len_utf8() == character.len_utf8())
                && folded.next().is_none()
        });
        let mut display = Vec::new();
        if !preserving {
            for (offset, character) in text.char_indices() {
                let count = character.to_lowercase().map(char::len_utf8).sum();
                display.extend(std::iter::repeat_n(
                    offset..offset + character.len_utf8(),
                    count,
                ));
            }
        }
        Self {
            text: lower,
            display,
            ascii_hits: [0; 3],
        }
    }

    fn quality(&self, byte: u8) -> Option<Hit> {
        let bit = 1 << byte;
        if self.ascii_hits[Hit::WholeWord as usize] & bit != 0 {
            Some(Hit::WholeWord)
        } else if self.ascii_hits[Hit::WordStart as usize] & bit != 0 {
            Some(Hit::WordStart)
        } else if self.ascii_hits[Hit::InsideWord as usize] & bit != 0 {
            Some(Hit::InsideWord)
        } else {
            None
        }
    }

    fn display(&self, range: Range<usize>) -> Range<usize> {
        if self.display.is_empty() {
            range
        } else {
            self.display[range.start].start..self.display[range.end - 1].end
        }
    }
}

fn ascii_hits(text: &str) -> [u128; 3] {
    let mut hits = [0; 3];
    for (at, byte) in text.bytes().enumerate().filter(|(_, byte)| byte.is_ascii()) {
        let quality = if !word_start(text, at) {
            Hit::InsideWord
        } else if word_end(text, at + 1) {
            Hit::WholeWord
        } else {
            Hit::WordStart
        };
        hits[quality as usize] |= 1 << byte;
    }
    hits
}

pub(crate) fn text_marks(line: &str, query: &TextQuery<'_>) -> Vec<Range<usize>> {
    let folded = Folded::for_display(line);
    let mut marks: Vec<_> = query
        .terms
        .iter()
        .filter_map(|term| {
            let (_, range) = best_hit(&folded.text, term)?;
            Some(folded.display(range))
        })
        .collect();
    merge_marks(&mut marks);
    marks
}

pub(crate) struct Matched {
    pub index: usize,
    group: Group,
    pub marks: [Vec<Range<usize>>; 3],
}

pub(crate) struct Listing {
    pub choices: Vec<Choice>,
    named: Vec<Named>,
    now_ms: u64,
    pub(crate) desk: bool,
}

impl Listing {
    pub fn new(commands: Vec<Entry>, places: Vec<Place>) -> Self {
        let choices: Vec<_> = places
            .into_iter()
            .map(Choice::Place)
            .chain(commands.into_iter().map(Choice::Command))
            .collect();
        let named = choices.iter().map(Named::new).collect();
        Self {
            named,
            choices,
            desk: false,
            now_ms: jiff::Timestamp::now().as_millisecond().max(0) as u64,
        }
    }

    pub fn clear_text(&mut self) {
        self.choices.truncate(self.named.len());
    }

    pub fn append_text(
        &mut self,
        rows: &mut Vec<Matched>,
        hits: Vec<(FoundText, Vec<Range<usize>>)>,
    ) {
        let at = self.text_position(rows);
        let shown = (LISTED - at).min(hits.len());
        rows.truncate(LISTED - shown);
        rows.splice(
            at..at,
            hits.into_iter().take(shown).map(|(hit, marks)| {
                let index = self.choices.len();
                self.choices.push(Choice::Words(hit));
                Matched {
                    index,
                    group: Group::Text,
                    marks: [Vec::new(), marks, Vec::new()],
                }
            }),
        );
    }

    pub fn text_position(&self, rows: &[Matched]) -> usize {
        rows.partition_point(|row| row.group < Group::Text)
    }

    #[cfg(test)]
    pub fn rank(&self, query: &str) -> Vec<Matched> {
        self.rank_query(&TextQuery::parse(&query.trim().to_lowercase()))
    }

    pub fn rank_query(&self, query: &TextQuery<'_>) -> Vec<Matched> {
        if matches!(query.scope, TextScope::Text(_)) {
            return Vec::new();
        }
        let allowed = |choice: &Choice| {
            query.scope != TextScope::Commands || matches!(choice, Choice::Command(_))
        };
        let choices = || {
            self.choices[..self.named.len()]
                .iter()
                .enumerate()
                .filter(|(_, choice)| allowed(choice))
        };
        if query.terms.is_empty() {
            let mut rows: Vec<_> = choices()
                .filter_map(|(index, choice)| Some((choice.blank(self.desk)?, index)))
                .collect();
            keep_best(&mut rows);
            return rows
                .into_iter()
                .map(|(_, index)| Matched {
                    index,
                    group: Group::OtherPlace,
                    marks: Default::default(),
                })
                .collect();
        }
        let mut rows: Vec<_> = choices()
            .filter_map(|(index, choice)| {
                let named = &self.named[index];
                let (strength, score) = named.score(query, matches!(choice, Choice::Command(_)))?;
                let active = choice.activity();
                let effective = strength
                    .with_recency(active.is_some_and(|at| self.now_ms.saturating_sub(at) < DAY_MS));
                Some((
                    choice.group(strength),
                    effective,
                    effective == strength,
                    score,
                    !choice.attention(),
                    Reverse(active.unwrap_or(match choice {
                        Choice::Place(place) => place.found.recency,
                        _ => 0,
                    })),
                    index,
                ))
            })
            .collect();
        keep_best(&mut rows);
        rows.into_iter()
            .map(|(group, _, _, _, _, _, index)| Matched {
                index,
                group,
                marks: self.named[index].marks(query),
            })
            .collect()
    }
}

fn keep_best<T: Ord>(rows: &mut Vec<T>) {
    if rows.len() > LISTED {
        rows.select_nth_unstable(LISTED);
        rows.truncate(LISTED);
    }
    rows.sort_unstable();
}

impl Strength {
    // A day of activity can move a thread one rung, without turning a
    // contained word into the start of its name or crossing rank groups.
    fn with_recency(self, recent: bool) -> Self {
        match (self, recent) {
            (Self::Fuzzy, true) => Self::WordsInOrder,
            (Self::WordsInOrder, true) => Self::WordStarts,
            _ => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_dealer::Address;

    use super::*;
    use crate::menu::Rows;
    use crate::system::{FoundThread, SearchPoint};

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
                seen: Default::default(),
                skipped: false,
            }),
            visit,
            visited_ms: visit.map(|rank| 100 - rank as u64),
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
        let mut listing = Listing::new(rows, places);
        assert_eq!(ids(&listing, " "), [4, 0, 3, 5]);
        listing.desk = true;
        assert_eq!(ids(&listing, ""), [4, 5]);
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
    fn ordinary_menus_flatten_but_verdict_choices_stay_nested() {
        let mut top = Rows::new("start agent")
            .row(Row::run("h", "here", |_, _, _| {}))
            .into_commands();
        top.push(
            Row::menu(
                "s",
                Rows::new("snooze").row(Row::run("m", "minutes", |_, _, _| {})),
            )
            .bound_to(crate::keys::VerdictMenu),
        );
        let rows = commands(&top, |_| Some("space s".to_owned()));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pick.label(), "start agent › here");
        assert!(rows[0].keys.is_empty());
        assert_eq!(rows[1].keys, "space s");
        assert!(
            matches!(&rows[1].pick, Pick::Menu(menu) if menu.rows[0].pick.label() == "minutes")
        );
        let listing = Listing::new(rows, vec![]);
        assert_eq!(ids(&listing, "here"), [0]);
        assert_eq!(ids(&listing, "snooze"), [1]);
        assert!(ids(&listing, "minutes").is_empty());
    }

    #[test]
    fn scoped_results_find_unprinted_words_and_rank_older_draft_first() {
        let mut listing = Listing::new(
            vec![],
            vec![thread("Brass pins", "reef@lab", Some(0), false, None)],
        );
        let mut results = Vec::new();
        for point in [SearchPoint::Draft(2), SearchPoint::Line(19)] {
            results.push(FoundText {
                address: Address::new("thread", "reef@lab"),
                title: "Mail · Failing lever · reef@lab".into(),
                line: "a brass pin sticks".into(),
                point,
            });
        }
        let query = TextQuery::parse("mail: pin brass");
        let mut rows = listing.rank_query(&query);
        listing.append_text(
            &mut rows,
            results.into_iter().map(|hit| (hit, Vec::new())).collect(),
        );
        assert_eq!(rows.iter().map(|row| row.index).collect::<Vec<_>>(), [1, 2]);
        assert!(ids(&listing, "mail:").is_empty());
        assert_eq!(ids(&listing, "brass pin"), [0]);
    }

    #[test]
    fn commands_follow_places_and_the_current_place_is_last_in_its_group() {
        let listing = Listing::new(
            commands(&[Row::run("", "repair fastclock", |_, _, _| {})], |_| None),
            vec![
                thread("clock", "wave@lab", Some(0), true, Some("asks · 1h")),
                thread("Broken Clock Test", "ice@lab", None, false, None),
                thread("Clockwork Build", "reef@lab", None, false, None),
            ],
        );
        assert_eq!(ids(&listing, "clock"), [2, 1, 0, 3]);
        assert_eq!(ids(&listing, "wave"), [0]);
    }

    #[test]
    fn a_command_at_word_starts_beats_a_note_matching_inside_renewal() {
        let mut renewal = thread("Ask about the lease renewal", "note", Some(0), false, None);
        renewal.found.thread = None;
        let listing = Listing::new(
            commands(&[Row::run("", "new note", |_, _, _| {})], |_| None),
            vec![renewal, thread("New notebook", "local", None, false, None)],
        );
        assert_eq!(ids(&listing, "new note"), [2, 1, 0]);
        assert_eq!(ids(&listing, "ne no"), [2, 1, 0]);
        assert_eq!(ids(&listing, "ew ote"), [1, 0, 2]);
    }

    #[test]
    fn a_whole_place_name_beats_a_command_that_merely_contains_it() {
        let listing = Listing::new(
            commands(
                &[
                    Row::run("", "new note", |_, _, _| {}),
                    Row::run("", "renew key", |_, _, _| {}),
                ],
                |_| None,
            ),
            vec![
                thread("Note", "local", None, false, None),
                thread("New", "workspace", None, false, None),
            ],
        );
        assert_eq!(ids(&listing, "NOTE"), [0, 2]);
        assert_eq!(ids(&listing, "new"), [1, 2, 3]);
    }

    #[test]
    fn activity_lifts_a_weaker_match_one_rung_but_not_over_a_name_start_or_command() {
        let now = DAY_MS * 20;
        let week_ago = now - DAY_MS * 7;
        let at = |title, mail, visit| {
            let mut place = thread(title, "tide@lab", None, false, None);
            place.found.recency = mail;
            place.visited_ms = visit;
            place
        };
        let rows = commands(
            &[
                Row::run("", "lock desk", |_, _, _| {}),
                Row::run("", "unlock", |_, _, _| {}),
            ],
            |_| None,
        );
        let mut listing = Listing::new(
            rows,
            vec![
                at("Check the lock later", week_ago, None),
                at("Airlock cleanup", week_ago, Some(now - 1)),
                at("Lock repair", week_ago, None),
                at("Lock", week_ago, None),
            ],
        );
        listing.now_ms = now;
        assert_eq!(ids(&listing, "lock"), [3, 4, 2, 1, 0, 5]);
        listing.now_ms = now + DAY_MS - 1;
        assert_eq!(ids(&listing, "lock"), [3, 4, 2, 0, 1, 5]);
        assert_eq!(ids(&listing, "> lock"), [4, 5]);
        assert_eq!(ids(&listing, ">"), [4, 5]);
        assert!(ids(&listing, "/ lock").is_empty());
    }

    #[test]
    fn equal_thread_matches_rank_waiting_then_the_later_of_visit_or_project_mail() {
        let mut waiting = thread("Clock check", "pond@lab", None, false, Some("asks · 7d"));
        let mut most_recent_mail = thread("Clock check", "tide@lab", None, false, None);
        let mut most_recent_visit = thread("Clock check", "reef@lab", None, false, None);
        let mut newer_of_two_old_events = thread("Clock check", "wave@lab", None, false, None);
        waiting.found.recency = 1;
        most_recent_mail.found.recency = 93;
        most_recent_mail.visited_ms = Some(8);
        most_recent_visit.found.recency = 3;
        most_recent_visit.visited_ms = Some(95);
        newer_of_two_old_events.found.recency = 39;
        newer_of_two_old_events.visited_ms = Some(80);
        let listing = Listing::new(
            vec![],
            vec![
                newer_of_two_old_events,
                most_recent_mail,
                waiting,
                most_recent_visit,
            ],
        );
        assert_eq!(ids(&listing, "clock check"), [2, 3, 1, 0]);
    }

    #[test]
    fn a_serial_word_start_cannot_begin_again_inside_the_same_word() {
        let listing = Listing::new(
            vec![],
            vec![
                thread("Fix notebook handling", "wave@lab", None, false, None),
                thread("Fix note bookkeeping", "reef@lab", None, false, None),
                thread("Fix Bookkeeping by note", "clock@lab", None, false, None),
            ],
        );
        assert_eq!(ids(&listing, "note book"), [1, 0, 2]);
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
            visited_ms: None,
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

    #[test]
    #[ignore = "performance benchmark"]
    fn bench_palette_rank() {
        let places = (0..2_000)
            .map(|index| {
                let title = match index % 4 {
                    0 => format!("Clock behavior followup {index}"),
                    1 => format!("Repairing the Clockwork {index}"),
                    2 => format!("Budget report {index}"),
                    _ => format!("Stopwatch clock readings {index}"),
                };
                thread(&title, &format!("project-{index}@lab"), None, false, None)
            })
            .collect();
        let listing = Listing::new(
            commands(&[Row::run("", "close window", |_, _, _| {})], |_| None),
            places,
        );
        let mut bench = criterion::Criterion::default().without_plots();
        for query in ["c", "clock", "clock beh"] {
            bench.bench_function(&format!("palette/rank-2000/{query}"), |b| {
                b.iter(|| listing.rank(std::hint::black_box(query)))
            });
        }
        bench.final_summary();
    }
}
