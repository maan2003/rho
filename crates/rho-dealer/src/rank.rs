//! The hand: every address that wants the user, ranked, read off what the
//! sources claim and what the user said about them.
//!
//! [`rank`] is the whole algorithm. A source hands it [`Claim`]s: for each
//! thing of its own that wants the user, where that stands in time and how
//! it presses. The dealer adds the user's own dates from their facts,
//! holds back what they snoozed, muted or piled, and works each card out
//! from scratch; nothing it decides is kept between calls. What a source
//! must claim for each of its cases is that source's own test; the cases
//! the dealer itself must meet are in `cases.md`, and each has a test
//! under its name.

use std::collections::{BTreeMap, HashMap};

use jiff::{Timestamp, Zoned};

use crate::address::Address;
use crate::curve::{self, Curve, DEAL_QUEUE_FLOOR};
use crate::facts::Snooze;
use crate::marks::Marks;

/// One thing a source says wants the user.
#[derive(Clone, Debug, PartialEq)]
pub struct Claim {
    pub address: Address,
    /// What the card is called, and where it is: `context › title`.
    pub title: String,
    pub context: String,
    pub curve: Curve,
    /// The words beside the age, `asks · {age}`; `{age}` is filled in
    /// when the hand is read.
    pub reason: String,
    /// Where the source stands: a skip holds on to it, and the source
    /// moving past it voids the skip.
    pub cursor: String,
    /// What the user having just written to it adds.
    pub bonus: f64,
    /// A reply this soon after the user's own words comes through a
    /// snooze said before them (Z4).
    pub breaks_snooze_set_by: Option<Timestamp>,
    /// When the other side wrote: a snooze that slept through some of it
    /// brings the card back a little higher (Z5).
    pub others: Vec<Timestamp>,
    /// An address a snooze on which holds this one too: the room a thread
    /// is in (S7).
    pub under: Option<Address>,
}

impl Claim {
    pub fn new(
        address: Address,
        curve: Curve,
        reason: impl Into<String>,
        cursor: impl Into<String>,
    ) -> Self {
        Self {
            address,
            title: String::new(),
            context: String::new(),
            curve,
            reason: reason.into(),
            cursor: cursor.into(),
            bonus: 0.0,
            breaks_snooze_set_by: None,
            others: Vec::new(),
            under: None,
        }
    }
}

/// An address wanting the user, ranked.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub address: Address,
    pub title: String,
    pub context: String,
    pub label: String,
    pub priority: f64,
    /// Where the source stood: a skip holds on to it, and the source
    /// moving past it voids the skip.
    pub cursor: String,
    /// Passed over this sitting: behind every card that was not, until its
    /// source moves or the skip runs out.
    pub skipped: bool,
}

impl Card {
    /// What the card is called on one line: where it is and what it is
    /// about, or only what it is about.
    pub fn heading(&self) -> String {
        let title = self.title.lines().next().unwrap_or_default();
        match (self.context.is_empty(), title.is_empty()) {
            (true, _) => title.to_owned(),
            (false, true) => self.context.clone(),
            (false, false) => format!("{} › {title}", self.context),
        }
    }
}

/// Every card above the floor, pressing hardest first, and when the hand
/// next changes with nobody touching anything.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hand {
    pub cards: Vec<Card>,
    pub next_change: Option<Timestamp>,
}

impl Hand {
    /// The card a pull deals: the top one that is not already in view.
    pub fn top(&self, exclude: Option<&Address>) -> Option<&Card> {
        self.cards
            .iter()
            .find(|card| exclude != Some(&card.address))
    }
}

/// Everything one deal weighed, for the journal: every address that came
/// up, what became of it, and what it was weighed from. Free-form text,
/// for reading back a bad deal, never for deciding anything.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Trace {
    pub nodes: BTreeMap<Address, Weighed>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Weighed {
    pub outcome: String,
    pub inputs: Vec<(&'static str, String)>,
    pub parts: Vec<String>,
}

impl Trace {
    /// What a source weighed on the way to a claim, or to none.
    pub fn input(&mut self, address: &Address, key: &'static str, value: String) {
        self.nodes
            .entry(address.clone())
            .or_default()
            .inputs
            .push((key, value));
    }

    pub fn outcome(&mut self, address: &Address, outcome: String) {
        self.nodes.entry(address.clone()).or_default().outcome = outcome;
    }
}

/// What a thing is called when no claim says: a title and where it is.
pub type Namer<'a> = &'a dyn Fn(&Address) -> Option<(String, String)>;

/// Everything [`rank`] reads.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub claims: &'a [Claim],
    pub marks: &'a Marks,
    pub skips: &'a Skips,
    /// Names for the addresses only the user's own dates bring up, which
    /// no claim named. Notes and labels name themselves.
    pub names: Namer<'a>,
}

#[derive(Clone, Debug)]
struct Skip {
    at: Timestamp,
    cursor: String,
}

/// The cards the user passed over, in memory only: a skip is about this
/// sitting, not something to remember.
#[derive(Default)]
pub struct Skips {
    skips: HashMap<Address, Skip>,
}

impl Skips {
    pub fn skip(&mut self, address: Address, cursor: String, now: Timestamp) {
        self.skips.insert(address, Skip { at: now, cursor });
    }

    pub fn clear(&mut self, address: &Address) -> bool {
        self.skips.remove(address).is_some()
    }

    pub fn contains(&self, address: &Address) -> bool {
        self.skips.contains_key(address)
    }

    /// Whether a skip still holds the address, given where its source
    /// stands now: when it was skipped and when the skip runs out.
    fn held(
        &self,
        address: &Address,
        cursor: &str,
        now: Timestamp,
    ) -> Option<(Timestamp, Timestamp)> {
        let skip = self.skips.get(address)?;
        let gone = skip.at + curve::SKIP_HOLD;
        (skip.cursor == cursor && now < gone).then_some((skip.at, gone))
    }
}

/// One reason an address wants the user, as weighed.
struct Part {
    curve: Curve,
    reason: String,
    cursor: String,
    bonus: f64,
    breaks_snooze_set_by: Option<Timestamp>,
    others: Vec<Timestamp>,
    /// From a source, which a snooze's end moves; the user's own dates
    /// stand where they were said.
    from_source: bool,
}

impl Part {
    fn claimed(claim: &Claim) -> Self {
        Self {
            curve: claim.curve,
            reason: claim.reason.clone(),
            cursor: claim.cursor.clone(),
            bonus: claim.bonus,
            breaks_snooze_set_by: claim.breaks_snooze_set_by,
            others: claim.others.clone(),
            from_source: true,
        }
    }

    fn dated(curve: Curve, cursor: String) -> Self {
        Self {
            curve,
            reason: String::new(),
            cursor,
            bonus: 0.0,
            breaks_snooze_set_by: None,
            others: Vec::new(),
            from_source: false,
        }
    }
}

/// The hand right now.
pub fn rank(sources: &Sources<'_>, now: &Zoned) -> Hand {
    rank_into(sources, now, None)
}

/// [`rank`], with everything it weighed, added to what the sources wrote
/// into `trace` on their way to their claims.
pub fn rank_traced(sources: &Sources<'_>, now: &Zoned, mut trace: Trace) -> (Hand, Trace) {
    let hand = rank_into(sources, now, Some(&mut trace));
    (hand, trace)
}

fn rank_into(sources: &Sources<'_>, now: &Zoned, mut trace: Option<&mut Trace>) -> Hand {
    let at = now.timestamp();
    let marks = sources.marks;
    let mut parts: BTreeMap<Address, Vec<Part>> = BTreeMap::new();
    let mut named: HashMap<Address, (String, String)> = HashMap::new();
    let mut under: HashMap<Address, Address> = HashMap::new();

    for claim in sources.claims {
        parts
            .entry(claim.address.clone())
            .or_default()
            .push(Part::claimed(claim));
        named
            .entry(claim.address.clone())
            .or_insert_with(|| (claim.title.clone(), claim.context.clone()));
        if let Some(room) = &claim.under {
            under.insert(claim.address.clone(), room.clone());
        }
    }

    // The user's own dates (T1–T6, N2), on any address.
    for (address, held) in marks.nodes() {
        let facts = held.facts();
        if let Some(todo) = facts.todo() {
            parts.entry(address.clone()).or_default().push(Part::dated(
                Curve::Plate { since: todo.start },
                format!("todo {}", todo.set.timestamp().as_millisecond()),
            ));
        }
        if let Some(deadline) = facts.deadline() {
            parts.entry(address.clone()).or_default().push(Part::dated(
                Curve::Deadline {
                    by: deadline.by,
                    lead_days: deadline.lead_days,
                },
                format!("deadline {}", deadline.set.timestamp().as_millisecond()),
            ));
        }
    }

    // One card per address (X1): what the user said about it decides
    // whether it shows, and its strongest part is the card.
    let mut steps = Vec::new();
    let mut cards = Vec::new();
    for (address, mut address_parts) in parts {
        let held = marks.get(&address);
        if let Some(trace) = trace.as_deref_mut() {
            let facts = &held.facts;
            let recent = facts.len().saturating_sub(12);
            for entry in &facts[recent..] {
                trace.input(&address, "fact", format!("{} {:?}", entry.at, entry.fact));
            }
            let weighed = trace.nodes.entry(address.clone()).or_default();
            weighed.parts = address_parts
                .iter()
                .map(|part| {
                    format!(
                        "{:?} {:?} priority {:.3} bonus {:.3} cursor {} from_source {} others {} breaks_snooze_set_by {:?}",
                        part.reason,
                        part.curve,
                        part.curve.priority(at),
                        part.bonus,
                        part.cursor,
                        part.from_source,
                        part.others.len(),
                        part.breaks_snooze_set_by,
                    )
                })
                .collect();
        }
        if held.deleted || held.facts().muted() {
            if let Some(trace) = trace.as_deref_mut() {
                let why = if held.deleted { "deleted" } else { "muted" };
                trace.outcome(&address, format!("no card: {why}"));
            }
            continue;
        }
        if let Some((pile, _)) = held.facts().pile() {
            if let Some(trace) = trace.as_deref_mut() {
                trace.outcome(&address, format!("no card: on the pile {pile}"));
            }
            continue;
        }
        // A snooze on the room holds everything in it too.
        let snoozes: Vec<Snooze> = std::iter::once(&address)
            .chain(under.get(&address))
            .filter_map(|address| marks.get(address).facts().snooze())
            .collect();
        // Z1, and Z4's way through.
        if let Some(snooze) = snoozes
            .iter()
            .filter(|snooze| at < snooze.until)
            .max_by_key(|snooze| snooze.until)
        {
            steps.push(snooze.until);
            let set = snooze.set.timestamp();
            address_parts
                .retain(|part| part.breaks_snooze_set_by.is_some_and(|spoke| set <= spoke));
            if address_parts.is_empty() {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.outcome(
                        &address,
                        format!(
                            "no card: snoozed until {} (said {})",
                            snooze.until, snooze.set
                        ),
                    );
                }
                continue;
            }
        }
        // Z2 and Z5: an address back from a snooze counts from the
        // snooze's end, a little higher for every message it slept through.
        if let Some(snooze) = snoozes
            .iter()
            .filter(|snooze| snooze.until <= at)
            .max_by_key(|snooze| snooze.until)
        {
            let set = snooze.set.timestamp();
            for part in address_parts.iter_mut().filter(|part| part.from_source) {
                match &mut part.curve {
                    Curve::Waiting { since, .. } | Curve::Fading { since, .. }
                        if *since < snooze.until =>
                    {
                        *since = snooze.until;
                        let slept = part
                            .others
                            .iter()
                            .filter(|wrote| set < **wrote && **wrote <= snooze.until)
                            .count();
                        part.bonus += (curve::SNOOZED_MESSAGE_BUMP * slept as f64)
                            .min(curve::SNOOZED_MESSAGE_BUMP_CAP);
                    }
                    _ => {}
                }
            }
        }
        for part in &address_parts {
            steps.extend(part.curve.steps());
        }
        let Some((part, priority)) = address_parts
            .iter()
            .map(|part| (part, part.curve.priority(at) + part.bonus))
            .filter(|(_, priority)| *priority > DEAL_QUEUE_FLOOR)
            .max_by(|a, b| a.1.total_cmp(&b.1))
        else {
            if let Some(trace) = trace.as_deref_mut() {
                let best = address_parts
                    .iter()
                    .map(|part| part.curve.priority(at) + part.bonus)
                    .fold(f64::NEG_INFINITY, f64::max);
                trace.outcome(&address, format!("no card: below the floor at {best:.3}"));
            }
            continue;
        };
        let skip = sources.skips.held(&address, &part.cursor, at);
        if let Some(trace) = trace.as_deref_mut() {
            let snoozes: Vec<String> = snoozes
                .iter()
                .map(|snooze| format!("until {} (said {})", snooze.until, snooze.set))
                .collect();
            trace.input(&address, "snoozes", format!("{snoozes:?}"));
            trace.outcome(
                &address,
                format!(
                    "card at {:.3}{}",
                    priority,
                    skip.map_or(String::new(), |(_, gone)| format!(
                        ", at the back for a skip until {gone}"
                    ))
                ),
            );
        }
        if let Some((_, gone)) = skip {
            steps.push(gone);
        }
        let (title, context) = named
            .remove(&address)
            .unwrap_or_else(|| name(sources, &address));
        cards.push((
            skip.map(|(skipped, _)| skipped),
            Card {
                title,
                context,
                label: part.curve.label(&part.reason, at),
                priority,
                cursor: part.cursor.clone(),
                skipped: skip.is_some(),
                address,
            },
        ));
    }
    // A skip is a turn at the back, not a lower score: a card waiting for
    // hours outscores any fixed penalty, and skipping went back and forth
    // between the same two cards. Skipped cards go behind the rest, the
    // longest-skipped first, so every card is dealt once before any comes
    // round again.
    cards.sort_by(
        |(a_skipped, a), (b_skipped, b)| match (a_skipped, b_skipped) {
            (Some(a_skipped), Some(b_skipped)) => a_skipped
                .cmp(b_skipped)
                .then_with(|| a.address.cmp(&b.address)),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => b
                .priority
                .total_cmp(&a.priority)
                .then_with(|| a.address.cmp(&b.address)),
        },
    );
    Hand {
        cards: cards.into_iter().map(|(_, card)| card).collect(),
        next_change: steps.into_iter().filter(|step| *step > at).min(),
    }
}

/// What an address only the user's dates brought up is called: a note by
/// its first line, a label by its path, anything else as its system says,
/// filed under its labels.
fn name(sources: &Sources<'_>, address: &Address) -> (String, String) {
    let marks = sources.marks;
    let title = if address.as_note().is_some() {
        match marks.get(address).title() {
            "" => "untitled note".to_owned(),
            title => title.to_owned(),
        }
    } else if let Some(label) = address.as_label() {
        marks.label_path(label)
    } else if let Some((title, context)) = (sources.names)(address) {
        return (title, context);
    } else {
        address.key().to_owned()
    };
    let context = marks
        .get(address)
        .labels
        .iter()
        .filter(|label| !marks.get(&Address::label(**label)).deleted)
        .map(|label| marks.label_path(*label))
        .collect::<Vec<_>>()
        .join(", ");
    (title, context)
}

#[cfg(test)]
mod tests;
