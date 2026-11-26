//! `space f`: places by name, the most recently visited first, and
//! excerpts of remembered words (`window.Places`). Commands are never
//! here; they are `:`'s (`window.Commands`).

use std::cmp::Reverse;
use std::ops::Range;

use memchr::memmem::Finder;
use rho_agent_types::ThreadWork;
use rho_dealer::Card;

use crate::matching::{Folded, Hit, best_hit, merge_marks, ordered_starts};
use crate::system::{Found, FoundText, FoundThread};
use crate::text_search::{TextQuery, TextScope};

// Eight rows are visible. Beyond this many results the user types more.
pub(crate) const LISTED: usize = 100;
const DAY_MS: u64 = 86_400_000;

pub(crate) struct Place {
    pub found: Found,
    pub card: Option<Card>,
    // Index in the window's visit history (most recent first).
    pub visit: Option<usize>,
    pub visited_ms: Option<u64>,
    pub current: bool,
    /// The states a state prefix finds it by.
    pub states: Vec<PlaceState>,
    /// `lab away`, when the host it lives on is away: said in place of
    /// `working`, which the host can no longer vouch for.
    pub away: Option<String>,
}

/// A state `Places` narrows to by a prefix (`window.Places`): the threads
/// at work, those stopped or failed, and those the user muted or snoozed,
/// so a muted thread is found again without its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaceState {
    Working,
    Stopped,
    Muted,
    Snoozed,
}

impl PlaceState {
    pub(crate) const ALL: [Self; 4] = [Self::Working, Self::Stopped, Self::Muted, Self::Snoozed];

    /// What the query starts with to narrow to it.
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Working => "working:",
            Self::Stopped => "stopped:",
            Self::Muted => "muted:",
            Self::Snoozed => "snoozed:",
        }
    }
}

pub(crate) enum Choice {
    Place(Place),
    Words(FoundText),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    ExactPlace,
    OtherPlace,
    Text,
    CurrentPlace,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Strength {
    ExactName,
    NamePrefix,
    WordStarts,
    WordsInOrder,
    Fuzzy,
}

impl Choice {
    pub fn name(&self) -> &str {
        match self {
            Self::Place(place) => &place.found.title,
            Self::Words(found) => &found.title,
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Words(found) => &found.line,
            Self::Place(place) => match (&place.card, &place.away, &place.found.thread) {
                (Some(card), _, _) => card.label.as_str(),
                (
                    None,
                    Some(away),
                    Some(FoundThread {
                        work: Some(ThreadWork::Working),
                        ..
                    }),
                ) => away.as_str(),
                (None, _, thread) => thread
                    .as_ref()
                    .and_then(|thread| thread.status.as_deref())
                    .unwrap_or(""),
            },
        }
    }

    fn beside(&self) -> &str {
        match self {
            Self::Words(_) => "",
            Self::Place(place) => &place.found.description,
        }
    }

    fn aliases(&self) -> &[String] {
        match self {
            Self::Place(place) => &place.found.also,
            Self::Words(_) => &[],
        }
    }

    // Blank input lists only places we have been, then places that wait.
    // A visited waiting place keeps its card words in both cases.
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
        }
    }

    fn group(&self, strength: Strength) -> Group {
        match (self, strength) {
            (Self::Place(place), _) if place.current => Group::CurrentPlace,
            (Self::Place(_), Strength::ExactName) => Group::ExactPlace,
            (Self::Place(_), _) => Group::OtherPlace,
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
}

type FieldHit = (Hit, usize, Range<usize>);

struct Named {
    fields: [Folded; 3],
    aliases: Vec<Folded>,
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
        }
    }

    fn hits(&self, finder: &Finder<'_>) -> (Option<Hit>, Option<FieldHit>) {
        let name = best_hit(&self.fields[0].text, finder);
        let name_quality = name.as_ref().map(|(quality, _)| *quality);
        let mut best = name.map(|(hit, range)| (hit, 0, range));
        if name_quality == Some(Hit::WholeWord) {
            return (name_quality, best);
        }
        for (field, text) in self
            .fields
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
            for (field, text) in self
                .fields
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

    fn score(&self, query: &TextQuery<'_>) -> Option<(Strength, usize)> {
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
        // A word that finds a place but is not shown is another whole name
        // for it: `tide@lab` for `tide`, `usage` for a usage chart.
        let strength =
            if name == query.text || self.aliases.iter().any(|alias| alias.text == query.text) {
                Strength::ExactName
            } else if name.starts_with(query.text) {
                Strength::NamePrefix
            } else if all_at_starts && (one_term || ordered_starts(name, &query.terms)) {
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
    pub fn new(places: Vec<Place>) -> Self {
        let choices: Vec<_> = places.into_iter().map(Choice::Place).collect();
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
        let state = match query.scope {
            TextScope::Text(_) => return Vec::new(),
            TextScope::State(state) => Some(state),
            TextScope::All => None,
        };
        let choices = || {
            self.choices[..self.named.len()]
                .iter()
                .enumerate()
                .filter(move |(_, choice)| {
                    state.is_none_or(|state| match choice {
                        Choice::Place(place) => place.states.contains(&state),
                        Choice::Words(_) => false,
                    })
                })
        };
        if state.is_some() && query.terms.is_empty() {
            // Every place in the state, what waits first, then the most
            // recently active.
            let mut rows: Vec<_> = choices()
                .map(|(index, choice)| {
                    let active = choice.activity().unwrap_or(match choice {
                        Choice::Place(place) => place.found.recency,
                        Choice::Words(_) => 0,
                    });
                    (!choice.attention(), Reverse(active), index)
                })
                .collect();
            keep_best(&mut rows);
            return rows
                .into_iter()
                .map(|(.., index)| Matched {
                    index,
                    group: Group::OtherPlace,
                    marks: Default::default(),
                })
                .collect();
        }
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
                let (strength, score) = self.named[index].score(query)?;
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
                        Choice::Words(_) => 0,
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
    use crate::system::SearchPoint;

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
                    work: None,
                }),
                recency: 0,
                also: Vec::new(),
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
            states: Vec::new(),
            away: None,
        }
    }

    fn ids(listing: &Listing, query: &str) -> Vec<usize> {
        listing
            .rank(query)
            .into_iter()
            .map(|row| row.index)
            .collect()
    }

    /// Nothing typed: the places visited, most recent first, then what
    /// waits; never the place in view, and on Desk never what waits.
    #[test]
    fn blank_is_recent_places_then_what_waits() {
        let places = vec![
            thread("old", "ice@lab", Some(3), false, Some("asks · 1h")),
            thread("current", "reef@lab", None, true, Some("asks · 1h")),
            thread("unvisited", "floe@lab", None, false, None),
            thread("waiting", "wave@lab", None, false, Some("asks · 1h")),
            thread("recent", "foam@lab", Some(0), false, None),
        ];
        let mut listing = Listing::new(places);
        assert_eq!(ids(&listing, " "), [4, 0, 3]);
        listing.desk = true;
        assert_eq!(ids(&listing, ""), [4]);
        assert_eq!(ids(&listing, "urrent"), [1]);
    }

    #[test]
    fn waiting_beats_the_most_recent_visit_on_equal_subjects() {
        let listing = Listing::new(vec![
            thread("Flaky clock", "ice@lab", Some(0), false, None),
            thread("Flaky clock", "wave@lab", None, false, Some("asks · 2h")),
            thread("Flaky clock", "reef@lab", Some(4), false, None),
        ]);
        assert_eq!(ids(&listing, "fl"), [1, 0, 2]);
        assert_eq!(ids(&listing, "ck fla"), [1, 0, 2]);
        assert_eq!(ids(&listing, "ave flaky asks"), [1]);
        assert!(ids(&listing, "asks ice").is_empty());
        assert_eq!(ids(&listing, "timing ice"), [0]);
    }

    /// A project shown as `tide`, and its threads, are still found by the
    /// address with the host, without it shown.
    #[test]
    fn a_place_is_found_by_words_it_does_not_show() {
        let mut quiet = thread("Flaky clock", "tide", None, false, None);
        quiet.found.also = vec!["tide@lab".into()];
        let listing = Listing::new(vec![
            quiet,
            thread("Clock drift", "tide@box", None, false, None),
        ]);
        assert_eq!(ids(&listing, "tide@lab"), [0]);
        assert_eq!(ids(&listing, "tide@box"), [1]);
        assert_eq!(ids(&listing, "lab"), [0]);
        let mut both = ids(&listing, "tide");
        both.sort();
        assert_eq!(both, [0, 1]);
        let rows = listing.rank("tide@lab");
        assert!(
            rows[0].marks.iter().all(Vec::is_empty),
            "nothing shown matched"
        );
    }

    #[test]
    fn matching_marks_the_display_bytes_in_all_fields() {
        let listing = Listing::new(vec![thread(
            "İstanbul clock",
            "wave@lab",
            None,
            false,
            Some("asks · 2h"),
        )]);
        let rows = listing.rank("la 2h I clo lock");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].marks, [vec![0..2, 10..15], vec![8..10], vec![5..7]]);
    }

    #[test]
    fn scoped_results_find_unprinted_words_and_rank_older_draft_first() {
        let mut listing =
            Listing::new(vec![thread("Brass pins", "reef@lab", Some(0), false, None)]);
        let mut results = Vec::new();
        for point in [SearchPoint::Draft(2), SearchPoint::Line(19)] {
            results.push(FoundText {
                address: Address::new("thread", "reef@lab"),
                title: "Message · Failing lever · reef@lab".into(),
                line: "a brass pin sticks".into(),
                point,
            });
        }
        let query = TextQuery::parse("chat: pin brass");
        let mut rows = listing.rank_query(&query);
        listing.append_text(
            &mut rows,
            results.into_iter().map(|hit| (hit, Vec::new())).collect(),
        );
        assert_eq!(rows.iter().map(|row| row.index).collect::<Vec<_>>(), [1, 2]);
        assert!(ids(&listing, "chat:").is_empty());
        assert_eq!(ids(&listing, "brass pin"), [0]);
    }

    #[test]
    fn the_current_place_is_last_among_matches() {
        let listing = Listing::new(vec![
            thread("clock", "wave@lab", Some(0), true, Some("asks · 1h")),
            thread("Broken Clock Test", "ice@lab", None, false, None),
            thread("Clockwork Build", "reef@lab", None, false, None),
        ]);
        assert_eq!(ids(&listing, "clock"), [2, 1, 0]);
        assert_eq!(ids(&listing, "wave"), [0]);
    }

    /// Text that once picked commands (`>`) is ordinary words now: no
    /// place is called that.
    #[test]
    fn a_greater_than_sign_is_no_command_scope() {
        let listing = Listing::new(vec![thread("Lock repair", "tide@lab", None, false, None)]);
        assert!(ids(&listing, "> lock").is_empty());
        assert!(ids(&listing, "/ lock").is_empty(), "text only");
    }

    #[test]
    fn activity_lifts_a_weaker_match_one_rung_but_not_over_a_name_start() {
        let now = DAY_MS * 20;
        let week_ago = now - DAY_MS * 7;
        let at = |title, mail, visit| {
            let mut place = thread(title, "tide@lab", None, false, None);
            place.found.recency = mail;
            place.visited_ms = visit;
            place
        };
        let mut listing = Listing::new(vec![
            at("Check the lock later", week_ago, None),
            at("Airlock cleanup", week_ago, Some(now - 1)),
            at("Lock repair", week_ago, None),
            at("Lock", week_ago, None),
        ]);
        listing.now_ms = now;
        assert_eq!(ids(&listing, "lock"), [3, 2, 1, 0]);
        listing.now_ms = now + DAY_MS - 1;
        assert_eq!(ids(&listing, "lock"), [3, 2, 0, 1]);
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
        let listing = Listing::new(vec![
            newer_of_two_old_events,
            most_recent_mail,
            waiting,
            most_recent_visit,
        ]);
        assert_eq!(ids(&listing, "clock check"), [2, 3, 1, 0]);
    }

    #[test]
    fn a_serial_word_start_cannot_begin_again_inside_the_same_word() {
        let listing = Listing::new(vec![
            thread("Fix notebook handling", "wave@lab", None, false, None),
            thread("Fix note bookkeeping", "reef@lab", None, false, None),
            thread("Fix Bookkeeping by note", "clock@lab", None, false, None),
        ]);
        assert_eq!(ids(&listing, "note book"), [1, 0, 2]);
    }

    /// `usage` is another name for every chart, so it finds them all before
    /// a place whose shown name only holds the word, in the order the
    /// system lists them: `rate limit · 7d` first, even after another
    /// chart was visited.
    #[test]
    fn usage_finds_the_charts_with_rate_limit_first() {
        let chart = |title: &str, visit| Place {
            found: Found {
                address: Address::new("usage", title),
                title: title.into(),
                description: String::new(),
                thread: None,
                recency: 0,
                also: vec!["usage".into()],
            },
            card: None,
            visit,
            visited_ms: None,
            current: false,
            states: Vec::new(),
            away: None,
        };
        let mut listing = Listing::new(vec![
            chart("rate limit · 7d", None),
            chart("model usage share · 30d", Some(0)),
        ]);
        assert_eq!(ids(&listing, "usage"), [0, 1]);
        assert_eq!(ids(&listing, "usage 30d"), [1]);
        assert_eq!(ids(&listing, "rate"), [0]);
        listing = Listing::new(vec![
            thread("Usage alerts for tide", "tide@lab", Some(0), false, None),
            chart("rate limit · 7d", None),
        ]);
        assert_eq!(
            ids(&listing, "usage"),
            [1, 0],
            "the chart's own name beats a prefix"
        );
    }

    /// A place in `states`, at work as `work` says.
    fn stated(
        title: &str,
        project: &str,
        work: Option<ThreadWork>,
        states: &[PlaceState],
        recency: u64,
    ) -> Place {
        let mut place = thread(title, project, None, false, None);
        place.found.address = Address::new("thread", format!("{project}/{title}"));
        place.found.thread = Some(FoundThread {
            status: work.map(|work| work.words().to_owned()),
            work,
        });
        place.found.recency = recency;
        place.states = states.to_vec();
        place
    }

    fn states_listing() -> Listing {
        use PlaceState::{Muted, Snoozed, Stopped};
        use ThreadWork::StoppedByYou;
        const AT_WORK: Option<ThreadWork> = Some(ThreadWork::Working);
        const WORKING: PlaceState = PlaceState::Working;
        let mut note = stated("Call the plumber", "", None, &[Snoozed], 5);
        note.found.thread = None;
        Listing::new(vec![
            stated("Flaky clock test", "tide", AT_WORK, &[WORKING], 10),
            stated("Clock drift", "reef", AT_WORK, &[WORKING, Muted], 30),
            stated("Retry window", "tide", Some(StoppedByYou), &[Stopped], 20),
            stated("Lint the docs", "tide", None, &[Muted], 40),
            stated("Old clock idea", "reef", None, &[Snoozed], 1),
            note,
            stated("Quiet one", "kelp", None, &[], 50),
        ])
    }

    fn titles(listing: &Listing, query: &str) -> Vec<String> {
        listing
            .rank(query)
            .into_iter()
            .map(|row| listing.choices[row.index].name().to_owned())
            .collect()
    }

    /// Each prefix alone lists every place in its state, the most
    /// recently active first; nothing in another state.
    #[test]
    fn a_state_prefix_lists_the_places_in_that_state() {
        let listing = states_listing();
        assert_eq!(
            titles(&listing, "working:"),
            ["Clock drift", "Flaky clock test"]
        );
        assert_eq!(titles(&listing, "stopped:"), ["Retry window"]);
        assert_eq!(
            titles(&listing, "muted:"),
            ["Lint the docs", "Clock drift"],
            "a muted thread is found again without its name"
        );
        assert_eq!(
            titles(&listing, "snoozed:"),
            ["Call the plumber", "Old clock idea"],
            "notes as well as threads"
        );
        assert_eq!(titles(&listing, " Muted:  "), titles(&listing, "muted:"));
    }

    /// Words after the prefix narrow its places as any query does, by
    /// subject or by project.
    #[test]
    fn a_state_prefix_composes_with_words() {
        let listing = states_listing();
        assert_eq!(
            titles(&listing, "working: clock"),
            ["Clock drift", "Flaky clock test"]
        );
        assert_eq!(titles(&listing, "working: flaky"), ["Flaky clock test"]);
        assert_eq!(titles(&listing, "working:tide"), ["Flaky clock test"]);
        assert_eq!(titles(&listing, "muted: tide"), ["Lint the docs"]);
        assert_eq!(titles(&listing, "muted: reef drift"), ["Clock drift"]);
        assert_eq!(titles(&listing, "snoozed: clock"), ["Old clock idea"]);
        assert_eq!(titles(&listing, "stopped: retry"), ["Retry window"]);
        assert!(titles(&listing, "stopped: clock").is_empty());
        assert!(titles(&listing, "muted: kelp").is_empty());
        // Without the prefix, `clock` finds them all, whatever their state.
        assert_eq!(titles(&listing, "clock").len(), 3);
    }

    /// A state is about places, not remembered words: no text search
    /// runs behind it, and a prefix later in the line is ordinary words.
    #[test]
    fn a_state_prefix_searches_places_only() {
        for typed in ["working:", "working: clock", "muted: tide", "snoozed:x"] {
            let query = TextQuery::parse(typed);
            assert!(!query.wants_text(), "{typed}");
            assert!(matches!(query.scope, TextScope::State(_)), "{typed}");
        }
        assert!(TextQuery::parse("clock working:").wants_text());
        assert!(matches!(
            TextQuery::parse("/working: clock").scope,
            TextScope::Text(None)
        ));
        assert!(titles(&states_listing(), "clock working:").is_empty());
    }

    /// A thread its host last said is at work reads `lab away` while that
    /// host is away; its card's words, when it waits, still come first.
    #[test]
    fn work_on_an_away_host_reads_away() {
        let mut working = stated(
            "Flaky clock test",
            "tide",
            Some(ThreadWork::Working),
            &[],
            1,
        );
        working.away = Some("lab away".into());
        let mut idle = stated("Lint the docs", "tide", None, &[], 1);
        idle.found.thread = Some(FoundThread {
            status: Some("Docs pass".into()),
            work: None,
        });
        idle.away = Some("lab away".into());
        let mut waiting = thread("Retry window", "tide", None, false, Some("waits · 2h"));
        waiting.away = Some("lab away".into());
        waiting.found.thread = Some(FoundThread {
            status: Some("working".into()),
            work: Some(ThreadWork::Working),
        });
        let listing = Listing::new(vec![working, idle, waiting]);
        let details: Vec<&str> = listing
            .choices
            .iter()
            .map(|choice| choice.fields()[1])
            .collect();
        assert_eq!(details, ["lab away", "Docs pass", "waits · 2h"]);
    }

    #[test]
    #[ignore = "performance benchmark"]
    fn bench_places_rank() {
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
        let listing = Listing::new(places);
        let mut bench = criterion::Criterion::default().without_plots();
        for query in ["c", "clock", "clock beh"] {
            bench.bench_function(&format!("places/rank-2000/{query}"), |b| {
                b.iter(|| listing.rank(std::hint::black_box(query)))
            });
        }
        bench.final_summary();
    }
}
