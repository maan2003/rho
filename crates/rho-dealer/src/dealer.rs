//! The ranking: every want, read against the clock.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};

use crate::curve::{self, Curve, DEAL_QUEUE_FLOOR, SKIP_COOLDOWN};
use crate::node::NodeId;

/// What kind of thing a card is, which decides how it opens and what
/// answering it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CardKind {
    /// An agent whose turn ended with something for the user.
    Agent,
    /// A Slack conversation or thread.
    Slack,
    /// A date the user put on a node: a todo coming due, a deadline.
    Dated,
}

/// A node asking for the user, as its source reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    pub kind: CardKind,
    /// What the node is called.
    pub title: String,
    /// Where it is: the conversation, or the labels it carries.
    pub context: String,
    /// Why it wants the user, with `{age}` where the time since goes.
    pub reason: String,
    pub curve: Curve,
    /// When the user last turned to this node themselves, which lifts it
    /// for a while ([`curve::recency_bonus`]).
    pub touched_ms: Option<i64>,
    /// Where the source stood when it said this. A skip holds on to it,
    /// and the source moving past it voids the skip.
    pub cursor: String,
}

/// A want, ranked.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub node: NodeId,
    pub kind: CardKind,
    pub title: String,
    pub context: String,
    pub label: String,
    pub priority: f64,
    pub cursor: String,
    /// Passed over by a pull and still inside the cooldown, with nothing
    /// new from its source since. It is still owed, so Home shows it and
    /// says so; only the next pull skips over it.
    pub skipped: bool,
}

#[derive(Clone, Debug)]
struct Skip {
    at: DateTime<FixedOffset>,
    cursor: String,
}

/// Every want, and the skips the user made. Nothing here is scored until
/// it is read.
#[derive(Default)]
pub struct Dealer {
    wants: HashMap<NodeId, Vec<Want>>,
    skips: HashMap<NodeId, Skip>,
}

impl Dealer {
    /// Replaces what a node wants. A node may want the user for more than
    /// one reason (an agent that finished and has a todo on it); its card
    /// is whichever presses hardest.
    pub fn set(&mut self, node: NodeId, wants: Vec<Want>) {
        if wants.is_empty() {
            self.wants.remove(&node);
        } else {
            self.wants.insert(node, wants);
        }
    }

    /// Keeps only the nodes `keep` says still exist.
    pub fn retain(&mut self, mut keep: impl FnMut(&NodeId) -> bool) {
        self.wants.retain(|node, _| keep(node));
    }

    pub fn wants(&self, node: &NodeId) -> &[Want] {
        self.wants.get(node).map_or(&[], Vec::as_slice)
    }

    /// Every card above the floor, pressing hardest first.
    pub fn hand(&self, now: DateTime<FixedOffset>) -> Vec<Card> {
        let mut cards: Vec<Card> = self
            .wants
            .iter()
            .filter_map(|(node, wants)| {
                wants
                    .iter()
                    .map(|want| (want, score(want, now)))
                    .filter(|(_, priority)| *priority > DEAL_QUEUE_FLOOR)
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(want, priority)| Card {
                        node: node.clone(),
                        kind: want.kind,
                        title: want.title.clone(),
                        context: want.context.clone(),
                        label: want.curve.label(&want.reason, now),
                        priority,
                        cursor: want.cursor.clone(),
                        skipped: self.skips.get(node).is_some_and(|skip| {
                            now < skip.at + SKIP_COOLDOWN && skip.cursor == want.cursor
                        }),
                    })
            })
            .collect();
        cards.sort_by(|a, b| {
            b.priority
                .total_cmp(&a.priority)
                // An agent wins an exact tie: it is the user's own work
                // coming back.
                .then_with(|| (b.kind == CardKind::Agent).cmp(&(a.kind == CardKind::Agent)))
                // Last, the node: two cards that tie on everything else
                // must still come out in the same order every read.
                .then_with(|| a.node.cmp(&b.node))
        });
        cards
    }

    /// The card a pull opens: the top of the hand that is not skipped and
    /// is not the one already in view.
    pub fn top(&self, now: DateTime<FixedOffset>, exclude: Option<&NodeId>) -> Option<Card> {
        self.hand(now)
            .into_iter()
            .filter(|card| !card.skipped)
            .find(|card| exclude != Some(&card.node))
    }

    /// Passes over a card: the next pull opens something else until the
    /// cooldown runs out or the card's source moves past `cursor`.
    pub fn skip(&mut self, node: NodeId, cursor: String, now: DateTime<FixedOffset>) {
        self.skips.insert(node, Skip { at: now, cursor });
    }

    pub fn clear_skip(&mut self, node: &NodeId) -> bool {
        self.skips.remove(node).is_some()
    }

    pub fn is_skipped(&self, node: &NodeId) -> bool {
        self.skips.contains_key(node)
    }

    /// How long until the hand changes without anybody touching anything:
    /// a date arriving, or a skip running out. Priorities also slide with
    /// the clock in between, so a caller keeps a ceiling of its own.
    pub fn next_change(&self, now: DateTime<FixedOffset>) -> Option<chrono::TimeDelta> {
        let local = now.naive_local();
        let steps = self
            .wants
            .values()
            .flatten()
            .flat_map(|want| want.curve.steps());
        let skips = self
            .skips
            .values()
            .map(|skip| (skip.at + SKIP_COOLDOWN).naive_local());
        steps
            .chain(skips)
            .filter(|at| *at > local)
            .min()
            .map(|at| at - local)
    }
}

fn score(want: &Want, now: DateTime<FixedOffset>) -> f64 {
    want.curve.priority(now)
        + want
            .touched_ms
            .map_or(0.0, |touched| curve::recency_bonus(touched, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{DateMark, THREAD_REPLY_HEAD_START};

    fn noon() -> DateTime<FixedOffset> {
        chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset()
    }

    fn note(n: u128) -> NodeId {
        NodeId::Note(uuid::Uuid::from_u128(n))
    }

    fn waiting(kind: CardKind, hours: i64, cursor: &str) -> Want {
        Want {
            kind,
            title: "t".into(),
            context: String::new(),
            reason: "needs reply · {age}".into(),
            curve: Curve::Waiting {
                head_start: THREAD_REPLY_HEAD_START,
                since_ms: noon().timestamp_millis() - hours * 3_600_000,
            },
            touched_ms: None,
            cursor: cursor.into(),
        }
    }

    #[test]
    fn the_longest_wait_comes_first_and_the_floor_drops_the_rest() {
        let mut dealer = Dealer::default();
        dealer.set(note(1), vec![waiting(CardKind::Slack, 1, "a")]);
        dealer.set(note(2), vec![waiting(CardKind::Slack, 5, "b")]);
        dealer.set(
            note(3),
            vec![Want {
                curve: Curve::Wakes {
                    at: DateMark::day(noon().date_naive() + chrono::TimeDelta::days(1)),
                    pace_days: 0,
                },
                ..waiting(CardKind::Dated, 0, "c")
            }],
        );
        let hand = dealer.hand(noon());
        let nodes: Vec<_> = hand.iter().map(|card| card.node.clone()).collect();
        assert_eq!(nodes, [note(2), note(1)]);
        assert_eq!(hand[0].label, "needs reply · 5.0h");
    }

    #[test]
    fn a_node_is_one_card_at_its_loudest() {
        let mut dealer = Dealer::default();
        dealer.set(
            note(1),
            vec![
                waiting(CardKind::Agent, 1, "a"),
                waiting(CardKind::Agent, 9, "a"),
            ],
        );
        let hand = dealer.hand(noon());
        assert_eq!(hand.len(), 1);
        assert_eq!(hand[0].label, "needs reply · 9.0h");
    }

    #[test]
    fn a_skip_holds_until_the_cooldown_or_the_source_moves() {
        let mut dealer = Dealer::default();
        dealer.set(note(1), vec![waiting(CardKind::Slack, 5, "a")]);
        dealer.set(note(2), vec![waiting(CardKind::Slack, 1, "b")]);
        dealer.skip(note(1), "a".into(), noon());
        assert_eq!(dealer.top(noon(), None).unwrap().node, note(2));
        assert!(dealer.hand(noon())[0].skipped, "still shown, marked");
        // The cooldown ends.
        let later = noon() + SKIP_COOLDOWN;
        assert_eq!(dealer.top(later, None).unwrap().node, note(1));
        assert_eq!(dealer.next_change(noon()), Some(SKIP_COOLDOWN));
        // Something new arrives on the skipped card.
        dealer.set(note(1), vec![waiting(CardKind::Slack, 5, "a2")]);
        assert_eq!(dealer.top(noon(), None).unwrap().node, note(1));
        assert_eq!(
            dealer.top(noon(), Some(&note(1))).unwrap().node,
            note(2),
            "the card in view is not dealt again"
        );
    }
}
