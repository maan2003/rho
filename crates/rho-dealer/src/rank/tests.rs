//! The cases in `cases.md`, each under its row's name, and the rules
//! every history keeps.
//!
//! The source is a stand-in thread that asks and answers. What the real
//! source claims for each of its cases is tested beside that source.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp, Zoned};

use super::*;
use crate::curve::*;
use crate::facts::{Device, Entry, Fact, Seen};
use crate::until::Until;

/// What the user says in these scenarios, about the address it is said to.
enum Said {
    Done,
    Mute,
    Snooze { until: Until },
    Deadline { by: Until, lead_days: u32 },
}

/// One thing a stand-in source holds.
struct Thread {
    posts: Vec<Post>,
    channel: Option<Address>,
}

struct Post {
    seq: u64,
    at: Timestamp,
    from_user: bool,
    /// A mention of the user: an ask or a result; `None` says nothing.
    ask: Option<bool>,
}

/// Everything `rank` reads, and a clock to move.
struct World {
    things: BTreeMap<Address, Thread>,
    seq: u64,
    marks: Marks,
    skips: Skips,
    now: Zoned,
    names: HashMap<Address, String>,
}

fn world() -> World {
    world_in(TimeZone::UTC)
}

fn world_in(zone: TimeZone) -> World {
    World {
        things: BTreeMap::new(),
        seq: 0,
        marks: Marks::default(),
        skips: Skips::default(),
        now: "2026-08-23T09:00:00Z"
            .parse::<Timestamp>()
            .unwrap()
            .to_zoned(zone),
        names: HashMap::new(),
    }
}

fn mins(n: i64) -> SignedDuration {
    SignedDuration::from_mins(n)
}

fn hours(n: i64) -> SignedDuration {
    SignedDuration::from_hours(n)
}

impl World {
    fn pass(&mut self, by: SignedDuration) {
        self.now = self.now.checked_add(by).unwrap();
    }

    fn at(&self) -> Timestamp {
        self.now.timestamp()
    }

    // Threads

    fn agent(&mut self, name: &str) -> Address {
        let address = Address::new("thread", format!("0/{}", self.things.len() + 1));
        self.things.insert(
            address.clone(),
            Thread {
                posts: Vec::new(),
                channel: None,
            },
        );
        self.names.insert(address.clone(), name.into());
        address
    }

    fn post_in(&mut self, address: &Address, from_user: bool, ask: Option<bool>) {
        self.seq += 1;
        let at = self.at();
        let Some(Thread { posts, .. }) = self.things.get_mut(address) else {
            panic!("not a thread")
        };
        posts.push(Post {
            seq: self.seq,
            at,
            from_user,
            ask,
        });
    }

    fn finishes(&mut self, address: &Address) {
        self.post_in(address, false, Some(false));
    }

    fn asks(&mut self, address: &Address) {
        self.post_in(address, false, Some(true));
    }

    /// Says what it is doing, mentioning nobody: nothing the dealer reads.
    fn works(&mut self, address: &Address) {
        self.post_in(address, false, None);
    }

    fn writes_to(&mut self, address: &Address) {
        self.post_in(address, true, None);
    }

    // The user

    fn tell(&mut self, fact: Fact) {
        // Said now, or just after the last thing said.
        let at = self.marks.next_at(&self.now);
        self.now = at.clone();
        self.marks.apply([Entry {
            device: Device([0; 16]),
            at,
            fact,
        }]);
    }

    /// How far the user has seen `address`: everything its source has.
    fn seen(&self, address: &Address) -> Seen {
        match self.things.get(address) {
            Some(Thread { posts, .. }) => Seen::Topic(posts.last().map_or(0, |post| post.seq)),

            None => Seen::Whole,
        }
    }

    fn say(&mut self, address: &Address, said: Said) {
        let node = address.clone();
        let fact = match said {
            Said::Done => Fact::Settled {
                seen: self.seen(&node),
                node,
            },
            Said::Mute => Fact::Mute { node },
            Said::Snooze { until } => Fact::Snooze { node, until },
            Said::Deadline { by, lead_days } => Fact::Deadline {
                node,
                by,
                lead_days,
            },
        };
        self.tell(fact);
    }

    fn snooze(&mut self, address: &Address, until: SignedDuration) {
        self.say(
            address,
            Said::Snooze {
                until: Until::In(until),
            },
        );
    }

    fn done(&mut self, address: &Address) {
        self.say(address, Said::Done);
    }

    /// A deadline `hours` from now, pressing from a day before.
    fn due_in(&mut self, address: &Address, hours: i64) {
        self.say(
            address,
            Said::Deadline {
                by: Until::In(self::hours(hours)),
                lead_days: 1,
            },
        );
    }

    fn write_note(&mut self, address: &Address, body: &str, deleted: bool) {
        let note = address.as_note().expect("a note");
        let created = self.now.timestamp();
        self.tell(Fact::Wrote {
            note,
            text: body.into(),
            created,
        });
        if deleted {
            self.tell(Fact::Deleted { note });
        }
    }

    // The deal

    /// What the stand-in sources claim right now, the way the real ones
    /// do for these cases.
    fn claims(&self, trace: Option<&mut Trace>) -> Vec<Claim> {
        let at = self.at();
        let mut claims = Vec::new();
        let mut trace = trace;
        for (address, Thread { posts, channel }) in &self.things {
            let facts = self.marks.get(address).facts();
            // Done, or the user's own words, see through to there.
            let seen = facts.seen_topic().unwrap_or(0).max(
                posts
                    .iter()
                    .filter(|post| post.from_user)
                    .map(|post| post.seq)
                    .max()
                    .unwrap_or(0),
            );
            let unseen: Vec<&Post> = posts
                .iter()
                .filter(|post| post.ask.is_some() && post.seq > seen)
                .collect();
            let (mention, curve, reason) =
                if let Some(ask) = unseen.iter().find(|post| post.ask == Some(true)) {
                    (
                        *ask,
                        Curve::Waiting {
                            head_start: AGENT_BLOCKED_HEAD_START,
                            since: ask.at,
                        },
                        "asks · {age}",
                    )
                } else if let Some(result) = unseen.last() {
                    (
                        *result,
                        Curve::Waiting {
                            head_start: AGENT_BLOCKED_HEAD_START,
                            since: result.at,
                        },
                        "result · {age}",
                    )
                } else {
                    if let Some(trace) = trace.as_deref_mut() {
                        trace.outcome(address, "no card: nothing unread".to_owned());
                    }
                    continue;
                };
            let mut claim = Claim::new(
                address.clone(),
                curve,
                reason,
                unseen.last().unwrap().seq.to_string(),
            );
            claim.title = self.names[address].clone();
            if let Some(spoke) = posts
                .iter()
                .filter(|post| post.from_user)
                .map(|post| post.at)
                .max()
            {
                claim.bonus = recency_bonus(spoke, at);
                if mention.at.duration_since(spoke) <= REPLY_BREAKTHROUGH {
                    claim.breaks_snooze_set_by = Some(spoke);
                }
            }
            claim.under.clone_from(channel);
            claims.push(claim);
        }
        claims
    }

    fn deal(&self) -> Hand {
        let claims = self.claims(None);
        let names = |address: &Address| {
            self.names
                .get(address)
                .map(|name| (name.clone(), String::new()))
        };
        rank(
            &Sources {
                claims: &claims,
                marks: &self.marks,
                skips: &self.skips,
                names: &names,
            },
            &self.now,
        )
    }

    fn traced(&self) -> (Hand, Trace) {
        let mut trace = Trace::default();
        let claims = self.claims(Some(&mut trace));
        let names = |address: &Address| {
            self.names
                .get(address)
                .map(|name| (name.clone(), String::new()))
        };
        rank_traced(
            &Sources {
                claims: &claims,
                marks: &self.marks,
                skips: &self.skips,
                names: &names,
            },
            &self.now,
            trace,
        )
    }

    /// The hand as lines of `name · label`, top first.
    fn hand(&self) -> String {
        self.deal()
            .cards
            .iter()
            .map(|card| format!("{} · {}", card.heading(), card.label))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn priority(&self, address: &Address) -> Option<f64> {
        self.deal()
            .cards
            .into_iter()
            .find(|card| &card.address == address)
            .map(|card| card.priority)
    }

    fn top(&self) -> Option<String> {
        self.deal().top(None).map(|card| card.title.clone())
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-3
}

fn note(w: &mut World, name: &str) -> Address {
    let address = Address::note(uuid::Uuid::new_v4());
    w.write_note(&address, name, false);
    address
}

// Snoozes

#[test]
fn z1_a_snooze_holds_until_it_ends() {
    let mut w = world();
    let d = w.agent("D1");
    w.asks(&d);
    w.snooze(&d, hours(1));
    assert_eq!(w.hand(), "");
    w.pass(mins(59));
    assert_eq!(w.hand(), "");
    w.pass(mins(1));
    assert_eq!(w.hand(), "D1 · asks · 0m");
}

#[test]
fn z2_a_node_back_from_a_snooze_counts_from_its_end() {
    let mut w = world();
    let d = w.agent("D1");
    w.asks(&d);
    w.snooze(&d, hours(1));
    w.pass(mins(90));
    assert_eq!(w.hand(), "D1 · asks · 30m");
    assert!(close(
        w.priority(&d).unwrap(),
        AGENT_BLOCKED_HEAD_START + WAITING_SLOPE_PER_DAY / 48.0
    ));
}

#[test]
fn z3_a_snooze_ending_on_a_node_that_wants_nothing_brings_nothing() {
    let mut w = world();
    let a = w.agent("a");
    w.finishes(&a);
    w.snooze(&a, hours(1));
    w.pass(mins(5));
    w.writes_to(&a);
    w.works(&a);
    w.pass(hours(2));
    assert_eq!(w.hand(), "");
}

#[test]
fn z4_a_reply_within_the_hour_of_the_users_message_comes_through_a_snooze() {
    let mut w = world();
    let a = w.agent("a");
    w.finishes(&a);
    w.snooze(&a, hours(4));
    w.pass(mins(30));
    w.writes_to(&a);
    w.works(&a);
    w.pass(mins(30));
    w.asks(&a);
    assert_eq!(w.hand(), "a · asks · 0m");

    let b = w.agent("b");
    w.finishes(&b);
    w.snooze(&b, hours(4));
    w.pass(mins(30));
    w.writes_to(&b);
    w.works(&b);
    w.pass(mins(61));
    w.finishes(&b);
    assert!(
        w.priority(&b).is_none(),
        "an hour and more is not a conversation"
    );
}

#[test]
fn z6_every_snooze_is_kept() {
    let mut w = world();
    let d = w.agent("D1");
    w.asks(&d);
    for _ in 0..3 {
        w.snooze(&d, hours(1));
        w.pass(hours(1));
    }
    assert_eq!(w.marks.get(&d).facts().snoozes(), 3);
    assert_eq!(w.hand(), "D1 · asks · 0m");
}

#[test]
fn z7_a_snoozed_room_holds_what_is_under_it() {
    let mut w = world();
    let room = Address::new("topic", "0/100");
    w.snooze(&room, hours(1));
    let thread = w.agent("thread");
    w.things.get_mut(&thread).unwrap().channel = Some(room);
    w.asks(&thread);
    assert_eq!(w.hand(), "");
    w.pass(hours(1));
    assert!(w.priority(&thread).is_some());
}

// What older builds wrote

/// An old pile is done said when it was piled, having seen nothing: it
/// takes back the snooze and the deadline before it, and holds nothing
/// back itself.
#[test]
fn p1_an_old_pile_clears_what_came_before_and_holds_nothing() {
    let mut w = world();
    let d = w.agent("D1");
    w.asks(&d);
    let n = note(&mut w, "n");
    w.snooze(&d, hours(3));
    w.due_in(&n, 2);
    for address in [&d, &n] {
        w.tell(Fact::Piled {
            node: address.clone(),
            pile: "friday".into(),
        });
    }
    assert_eq!(
        w.hand(),
        "D1 · asks · 0m",
        "the snooze is gone and the pile saw nothing"
    );
    w.pass(hours(3));
    assert_eq!(w.priority(&n), None, "the deadline is gone");
}

/// An old todo is a read: what it saw is handled, it deals no card of its
/// own, and the snooze before it holds.
#[test]
fn t1_an_old_todo_reads_as_a_read() {
    let mut w = world();
    let a = w.agent("a");
    w.asks(&a);
    let d = w.agent("D1");
    w.asks(&d);
    w.snooze(&d, hours(1));
    for address in [&a, &d] {
        let seen = w.seen(address);
        w.tell(Fact::Todo {
            node: address.clone(),
            start: None,
            seen,
        });
    }
    assert_eq!(w.hand(), "");
    w.pass(hours(24 * 30));
    assert_eq!(w.hand(), "", "nothing on a plate rises");
    w.asks(&a);
    assert_eq!(w.hand(), "a · asks · 0m", "a newer ask still deals");
}

// Deadlines

#[test]
fn t3_a_deadline_shows_its_lead_ahead_and_jumps_once_late() {
    let mut w = world();
    let n = note(&mut w, "n");
    w.say(
        &n,
        Said::Deadline {
            by: Until::In(hours(24 * 5)),
            lead_days: DEADLINE_LEAD_DAYS,
        },
    );
    assert_eq!(w.hand(), "");
    w.pass(hours(24 * 2 + 1));
    assert_eq!(w.hand(), "n · deadline · 3d");
    w.pass(hours(24 * 4));
    assert_eq!(w.hand(), "n · deadline · 1d late");
    assert!(w.priority(&n).unwrap() > 1_000_000.0);
}

#[test]
fn t6_done_or_mute_takes_back_the_deadline_and_the_snooze() {
    for take_back in [Said::Done, Said::Mute] {
        let mut w = world();
        let n = note(&mut w, "n");
        w.due_in(&n, 1);
        w.snooze(&n, hours(1));
        w.say(&n, take_back);
        w.pass(hours(48));
        assert_eq!(w.hand(), "");
    }
}

// Notes

#[test]
fn n1_n3_a_plain_or_deleted_note_has_no_card() {
    let mut w = world();
    note(&mut w, "plain");
    let gone = note(&mut w, "gone");
    w.due_in(&gone, 1);
    w.write_note(&gone, "gone", true);
    assert_eq!(w.hand(), "");
}

#[test]
fn a_note_names_itself_by_its_first_line() {
    let mut w = world();
    let n = note(&mut w, "buy milk\nand eggs");
    w.due_in(&n, 1);
    let hand = w.deal();
    assert_eq!(hand.cards[0].address, n);
    assert_eq!(hand.cards[0].heading(), "buy milk");
}

// Every history

#[test]
fn x1_a_node_is_one_card_at_its_strongest() {
    let mut w = world();
    let a = w.agent("a");
    w.asks(&a);
    w.say(
        &a,
        Said::Deadline {
            by: Until::In(hours(24)),
            lead_days: 3,
        },
    );
    assert_eq!(w.hand(), "a · asks · 0m");
}

#[test]
fn x2_a_skip_sends_a_card_to_the_back_until_its_source_moves_or_it_runs_out() {
    let mut w = world();
    let a = w.agent("D1");
    w.asks(&a);
    w.pass(hours(3));
    let b = w.agent("D2");
    w.asks(&b);
    w.pass(hours(1));
    let c = w.agent("D3");
    w.asks(&c);
    let skip_top = |w: &mut World| {
        let card = w.deal().cards[0].clone();
        w.skips.skip(card.address, card.cursor, w.now.timestamp());
        w.pass(mins(1));
        w.top()
    };
    // D1 has waited hours longer than the others: a penalty would leave
    // it on top. Each is dealt once before any comes round again, and
    // then the longest-skipped first.
    assert_eq!(w.top().as_deref(), Some("D1"));
    assert_eq!(skip_top(&mut w).as_deref(), Some("D2"));
    assert_eq!(skip_top(&mut w).as_deref(), Some("D3"));
    assert_eq!(skip_top(&mut w).as_deref(), Some("D1"));
    assert!(w.deal().cards.iter().all(|card| card.skipped));

    w.asks(&a);
    let first = w.deal().cards[0].clone();
    assert!(
        first.address == a && !first.skipped,
        "something new voids the skip"
    );
    w.pass(SKIP_HOLD);
    assert!(
        w.deal().cards.iter().all(|card| !card.skipped),
        "a skip runs out"
    );
}

#[test]
fn x3_the_hand_says_when_it_next_changes() {
    let mut w = world();
    let d = w.agent("D1");
    w.asks(&d);
    w.snooze(&d, hours(1));
    assert_eq!(w.deal().next_change, Some(w.now.timestamp() + hours(1)));
    w.pass(hours(1));
    // Back from its snooze, it counts from the snooze's end: `0m` until it
    // says `1m` half a minute on.
    assert_eq!(w.deal().cards[0].label, "asks · 0m");
    let back = w.now.timestamp();
    assert_eq!(
        w.deal().next_change,
        Some(back + SignedDuration::from_secs(30))
    );
    w.pass(hours(2));
    assert_eq!(w.deal().cards[0].label, "asks · 2.0h");
    assert_eq!(w.deal().next_change, Some(w.now.timestamp() + mins(3)));
    let cursor = w.deal().cards[0].cursor.clone();
    let skipped = w.now.timestamp();
    w.skips.skip(d, cursor, skipped);
    w.pass(SKIP_HOLD - SignedDuration::from_secs(10));
    assert_eq!(
        w.deal().next_change,
        Some(skipped + SKIP_HOLD),
        "the skip runs out first"
    );
}

#[test]
fn x4_tomorrow_is_the_users_own_midnight() {
    let mut w = world_in(TimeZone::fixed(
        Offset::from_seconds(5 * 3600 + 1800).unwrap(),
    ));
    let d = w.agent("D1");
    w.asks(&d);
    // 09:00 UTC is 14:30 on the user's clock; tomorrow starts at 18:30 UTC.
    w.say(
        &d,
        Said::Snooze {
            until: Until::Day(jiff::civil::date(2026, 8, 24)),
        },
    );
    w.pass(hours(9) + mins(29));
    assert_eq!(w.hand(), "");
    w.pass(mins(1));
    assert!(w.priority(&d).is_some());
}

// The rules every history keeps (X1, X3, Z1), over random ones.

#[derive(Clone, Debug)]
enum Step {
    Pass(i64),
    Finishes(usize),
    Asks(usize),
    Works(usize),
    WritesTo(usize),
    Snooze(usize, i64),
    Done(usize),
    Mute(usize),
    SkipTop,
}

fn step() -> impl proptest::strategy::Strategy<Value = Step> {
    use proptest::prelude::*;
    prop_oneof![
        (1i64..600).prop_map(Step::Pass),
        (0usize..3).prop_map(Step::Finishes),
        (0usize..3).prop_map(Step::Asks),
        (0usize..3).prop_map(Step::Works),
        (0usize..3).prop_map(Step::WritesTo),
        ((0usize..3), (1i64..600)).prop_map(|(node, mins)| Step::Snooze(node, mins)),
        (0usize..3).prop_map(Step::Done),
        (0usize..3).prop_map(Step::Mute),
        Just(Step::SkipTop),
    ]
}

fn addresses(hand: &Hand) -> BTreeSet<Address> {
    hand.cards.iter().map(|card| card.address.clone()).collect()
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(200))]

    #[test]
    fn every_history_keeps_the_rules(steps in proptest::collection::vec(step(), 1..40)) {
        let mut w = world();
        let agents: Vec<Address> = ["a0", "a1", "a2"].iter().map(|name| w.agent(name)).collect();

        for step in steps {
            match step {
                Step::Pass(minutes) => w.pass(mins(minutes)),
                Step::Finishes(i) => w.finishes(&agents[i]),
                Step::Asks(i) => w.asks(&agents[i]),
                Step::Works(i) => w.works(&agents[i]),
                Step::WritesTo(i) => w.writes_to(&agents[i]),
                Step::Snooze(i, minutes) => w.snooze(&agents[i], mins(minutes)),
                Step::Done(i) => w.done(&agents[i]),
                Step::Mute(i) => w.say(&agents[i], Said::Mute),
                Step::SkipTop => {
                    if let Some(card) = w.deal().cards.first().cloned() {
                        w.skips.skip(card.address, card.cursor, w.now.timestamp());
                    }
                }
            }
            let hand = w.deal();
            // Dealing twice is dealing once: nothing is kept between deals.
            proptest::prop_assert_eq!(&hand, &w.deal());
            // X1: one card per address.
            proptest::prop_assert_eq!(addresses(&hand).len(), hand.cards.len());
            let at = w.now.timestamp();
            for card in &hand.cards {
                let facts = w.marks.get(&card.address).facts();
                proptest::prop_assert!(!facts.muted());
                // Z1: nothing snoozed shows, but for an agent's quick reply.
                if let Some(snooze) = facts.snooze() && at < snooze.until {
                    let posts = &w.things[&card.address].posts;
                    let spoke = posts.iter().rfind(|post| post.from_user).unwrap();
                    proptest::prop_assert!(spoke.at >= snooze.set.timestamp());
                    proptest::prop_assert!(posts.iter().any(|reply| reply.ask.is_some()
                        && reply.seq > spoke.seq && reply.at.duration_since(spoke.at) <= hours(1)));
                }
            }
            // X3: nothing arrives by itself before the next change.
            let before = hand.next_change.map_or(at + hours(24), |next| next - SignedDuration::from_millis(1));
            if before > at {
                let now = w.now.clone();
                w.now = before.to_zoned(now.time_zone().clone());
                let later = w.deal();
                w.now = now;
                proptest::prop_assert!(addresses(&later).is_subset(&addresses(&hand)), "{:?} arrived", addresses(&later).difference(&addresses(&hand)).collect::<Vec<_>>());
            }
        }
    }
}

#[test]
fn a_traced_deal_is_the_same_deal_and_says_what_it_left_out() {
    let mut w = world();
    let asking = w.agent("asking");
    w.asks(&asking);
    let snoozed = w.agent("snoozed");
    w.asks(&snoozed);
    w.snooze(&snoozed, hours(1));
    let quiet = w.agent("a");
    w.finishes(&quiet);
    w.done(&quiet);
    let (hand, trace) = w.traced();
    assert_eq!(hand, w.deal());
    let outcome = |address: &Address| trace.nodes[address].outcome.clone();
    assert!(
        outcome(&asking).starts_with("card at "),
        "{}",
        outcome(&asking)
    );
    assert!(outcome(&snoozed).starts_with("no card: snoozed until"));
    assert_eq!(
        outcome(&quiet),
        "no card: nothing unread",
        "what the source said stays"
    );
    assert!(
        trace.nodes[&asking]
            .inputs
            .iter()
            .any(|(key, _)| *key == "snoozes")
    );
    assert!(!trace.nodes[&asking].parts.is_empty());
}
