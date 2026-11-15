//! The cases in `cases.md`, each under its row's name, and the rules
//! every history keeps.
//!
//! The sources here are stand-ins: a thread that asks and answers, and a
//! Slack-shaped conversation that is read or not. Each claims the way its
//! real source does for the one or two cases the dealer's own rules need;
//! what a real source must claim for each of its cases is that source's
//! own test, beside its code.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp, Zoned};

use super::*;
use crate::curve::*;
use crate::facts::{Device, Entry, Fact, Seen};
use crate::notes::NoteRev;
use crate::until::Until;

/// What the user says in these scenarios, about the address it is said to.
enum Said {
    Done,
    Mute,
    Snooze { until: Until },
    Deadline { by: Until, lead_days: u32 },
}

/// One thing a stand-in source holds.
enum Thing {
    /// A chat thread between the user and one agent: posts in order, each
    /// numbered the way a host numbers them.
    Thread { posts: Vec<Post> },
    /// A Slack conversation: a direct one, or a thread in a room with
    /// this many people in it.
    Slack {
        reason: String,
        head_start: f64,
        room: Option<Address>,
        /// A conversation with the user: what it slept through counts.
        direct: bool,
        messages: Vec<Msg>,
        read_through: Option<String>,
    },
}

struct Post {
    seq: u64,
    at: Timestamp,
    from_user: bool,
    /// A mention of the user: an ask or a result; `None` says nothing.
    ask: Option<bool>,
}

struct Msg {
    ts: String,
    at: Timestamp,
    from_me: bool,
}

/// Everything `rank` reads, and a clock to move.
struct World {
    things: BTreeMap<Address, Thing>,
    seq: u64,
    ts: u64,
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
        ts: 0,
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
        self.things
            .insert(address.clone(), Thing::Thread { posts: Vec::new() });
        self.names.insert(address.clone(), name.into());
        address
    }

    fn post_in(&mut self, address: &Address, from_user: bool, ask: Option<bool>) {
        self.seq += 1;
        let at = self.at();
        let Some(Thing::Thread { posts }) = self.things.get_mut(address) else {
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

    // Slack

    fn next_ts(&mut self) -> String {
        self.ts += 1;
        format!("{}.{:06}", self.now.timestamp().as_second(), self.ts)
    }

    fn slack(
        &mut self,
        address: Address,
        name: &str,
        reason: &str,
        head_start: f64,
        room: Option<Address>,
        direct: bool,
    ) -> Address {
        if !self.things.contains_key(&address) {
            self.things.insert(
                address.clone(),
                Thing::Slack {
                    reason: reason.into(),
                    head_start,
                    room,
                    direct,
                    messages: Vec::new(),
                    read_through: None,
                },
            );
        }
        self.names.insert(address.clone(), name.into());
        address
    }

    fn post(&mut self, address: &Address, from_me: bool) {
        let ts = self.next_ts();
        let at = self.at();
        let Some(Thing::Slack { messages, .. }) = self.things.get_mut(address) else {
            panic!("not slack")
        };
        messages.push(Msg { ts, at, from_me });
    }

    /// A direct message from the user `channel` is with.
    fn dm(&mut self, channel: &str, user: &str) -> Address {
        let address = self.slack(
            Address::new("slack", format!("acme/{channel}")),
            channel,
            &format!("unread in @{user} · needs reply · {{age}}"),
            SLACK_DM_HEAD_START,
            None,
            true,
        );
        self.post(&address, false);
        address
    }

    /// A thread in #design the user follows, with a reply from each of
    /// `users` in turn.
    fn thread(&mut self, users: &[&str]) -> Address {
        let root = self.next_ts();
        let room = Address::new("slack", "acme/C1");
        let address = self.slack(
            Address::new("slack", format!("acme/C1/{root}")),
            "thread",
            "#design · needs reply · {age}",
            SLACK_THREAD_HEAD_START,
            Some(room),
            users.len() < SLACK_SMALL_THREAD_PEOPLE,
        );
        self.post(&address, true);
        for _ in users {
            self.post(&address, false);
        }
        address
    }

    fn read(&mut self, address: &Address) {
        let Some(Thing::Slack {
            messages,
            read_through,
            ..
        }) = self.things.get_mut(address)
        else {
            panic!("not slack")
        };
        *read_through = messages.last().map(|message| message.ts.clone());
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
            Some(Thing::Thread { posts }) => Seen::Topic(posts.last().map_or(0, |post| post.seq)),
            Some(Thing::Slack { messages, .. }) => messages
                .last()
                .map_or(Seen::Whole, |message| Seen::Slack(message.ts.clone())),
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

    fn todo(&mut self, address: &Address, start: Option<Until>) {
        self.tell(Fact::Todo {
            node: address.clone(),
            start,
            seen: self.seen(address),
        });
    }

    fn write_note(&mut self, address: &Address, body: &str, deleted: bool) {
        let note = address.as_note().expect("a note");
        let at = self.marks.next_at(&self.now);
        self.now = at.clone();
        self.marks.apply_notes([NoteRev {
            note,
            device: Device([0; 16]),
            created: at.timestamp(),
            at,
            body: body.into(),
            deleted,
        }]);
    }

    // The deal

    /// What the stand-in sources claim right now, the way the real ones
    /// do for these cases.
    fn claims(&self, trace: Option<&mut Trace>) -> Vec<Claim> {
        let at = self.at();
        let mut claims = Vec::new();
        let mut trace = trace;
        for (address, thing) in &self.things {
            let facts = self.marks.get(address).facts();
            match thing {
                Thing::Thread { posts } => {
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
                                Curve::Fading {
                                    head_start: AGENT_FINISHED_HEAD_START,
                                    since: result.at,
                                    gone_days: AGENT_FINISHED_GONE_DAYS,
                                },
                                "result · {age} ago",
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
                    claims.push(claim);
                }
                Thing::Slack {
                    reason,
                    head_start,
                    room,
                    direct,
                    messages,
                    read_through,
                } => {
                    let Some(newest) = messages.last() else {
                        continue;
                    };
                    if read_through.as_deref() == Some(newest.ts.as_str())
                        || facts
                            .seen_slack()
                            .is_some_and(|seen| seen >= newest.ts.as_str())
                    {
                        if let Some(trace) = trace.as_deref_mut() {
                            trace.outcome(address, "no card: read".to_owned());
                        }
                        continue;
                    }
                    let mut claim = Claim::new(
                        address.clone(),
                        Curve::Waiting {
                            head_start: *head_start,
                            since: newest.at,
                        },
                        reason.clone(),
                        newest.ts.clone(),
                    );
                    claim.title = self.names[address].clone();
                    claim.under = room.clone();
                    if *direct {
                        claim.others = messages
                            .iter()
                            .filter(|message| !message.from_me)
                            .map(|message| message.at)
                            .collect();
                    }
                    claims.push(claim);
                }
            }
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

fn pile(w: &mut World, address: &Address, pile: &str) {
    w.tell(Fact::Piled {
        node: address.clone(),
        pile: pile.into(),
    });
}

// Snoozes

#[test]
fn z1_a_snooze_holds_until_it_ends() {
    let mut w = world();
    let d = w.dm("D1", "u1");
    w.snooze(&d, hours(1));
    assert_eq!(w.hand(), "");
    w.pass(mins(59));
    assert_eq!(w.hand(), "");
    w.pass(mins(1));
    assert_eq!(w.hand(), "D1 · unread in @u1 · needs reply · 0m");
}

#[test]
fn z2_a_node_back_from_a_snooze_counts_from_its_end() {
    let mut w = world();
    let d = w.dm("D1", "u1");
    w.snooze(&d, hours(1));
    w.pass(mins(90));
    assert_eq!(w.hand(), "D1 · unread in @u1 · needs reply · 30m");
    assert!(close(
        w.priority(&d).unwrap(),
        SLACK_DM_HEAD_START + WAITING_SLOPE_PER_DAY / 48.0
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
    let d = w.dm("D1", "u1");
    w.snooze(&d, hours(1));
    w.read(&d);
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
fn z5_a_direct_conversation_comes_back_higher_for_what_it_slept_through() {
    let mut w = world();
    let d = w.dm("D1", "u1");
    w.snooze(&d, hours(2));
    let big = w.thread(&["U1", "U2", "U3", "U4"]);
    w.snooze(&big, hours(2));
    w.pass(hours(1));
    w.post(&d, false);
    w.post(&d, false);
    w.post(&big, false);
    w.pass(hours(1));
    assert!(close(
        w.priority(&d).unwrap(),
        SLACK_DM_HEAD_START + 2.0 * SNOOZED_MESSAGE_BUMP
    ));
    assert!(close(w.priority(&big).unwrap(), SLACK_THREAD_HEAD_START));
}

#[test]
fn z6_every_snooze_is_kept() {
    let mut w = world();
    let d = w.dm("D1", "u1");
    for _ in 0..3 {
        w.snooze(&d, hours(1));
        w.pass(hours(1));
    }
    assert_eq!(w.marks.get(&d).facts().snoozes(), 3);
    assert_eq!(w.hand(), "D1 · unread in @u1 · needs reply · 0m");
}

#[test]
fn z7_a_snoozed_room_holds_what_is_under_it() {
    let mut w = world();
    let room = Address::new("slack", "acme/C1");
    w.snooze(&room, hours(1));
    let thread = w.thread(&["U1"]);
    assert_eq!(w.hand(), "");
    w.pass(hours(1));
    assert!(w.priority(&thread).is_some());
}

// Piles

#[test]
fn p1_a_card_on_a_pile_stays_there_whatever_its_source_says() {
    let mut w = world();
    let d = w.dm("D1", "u1");
    let a = w.agent("a");
    w.asks(&a);
    pile(&mut w, &d, "ask ada");
    pile(&mut w, &a, "ask ada");
    w.pass(mins(1));
    w.post(&d, false);
    w.writes_to(&a);
    w.works(&a);
    w.pass(mins(5));
    w.asks(&a);
    w.pass(SignedDuration::from_hours(24 * 30));
    assert_eq!(
        w.hand(),
        "",
        "a reply within the hour does not come through"
    );
    let (_, trace) = w.traced();
    assert_eq!(trace.nodes[&d].outcome, "no card: on the pile ask ada");
}

#[test]
fn p2_a_todo_done_or_a_snooze_takes_the_card_off_its_pile() {
    let mut w = world();
    let n = note(&mut w, "n");
    w.todo(&n, None);
    pile(&mut w, &n, "friday");
    assert_eq!(w.hand(), "");
    w.snooze(&n, hours(1));
    assert_eq!(
        w.marks.get(&n).facts().pile(),
        None,
        "the snooze replaced it"
    );
    w.pass(hours(2));
    assert_eq!(w.hand(), "n · todo · 2.0h", "back as the todo it was");

    let d = w.dm("D2", "u2");
    pile(&mut w, &d, "friday");
    w.done(&d);
    assert_eq!(w.marks.get(&d).facts().pile(), None);
}

#[test]
fn p3_the_piles_are_named_oldest_first_and_then_everything_snoozed() {
    let mut w = world();
    let a = w.dm("D1", "u1");
    let b = w.dm("D2", "u2");
    let c = w.dm("D3", "u3");
    let later = w.dm("D4", "u4");
    let sooner = w.dm("D5", "u5");
    let over = w.dm("D6", "u6");
    let muted = w.dm("D7", "u7");
    pile(&mut w, &b, "on laptop");
    w.pass(mins(1));
    pile(&mut w, &a, "on laptop");
    pile(&mut w, &c, "ask ada");
    pile(&mut w, &muted, "ask ada");
    w.say(&muted, Said::Mute);
    w.snooze(&later, hours(3));
    w.snooze(&sooner, hours(2));
    w.snooze(&over, mins(1));
    w.pass(mins(2));
    let now = w.now.timestamp();
    let piles = w.marks.piles(now);
    let shown: Vec<(Option<&str>, Vec<&Address>)> = piles
        .iter()
        .map(|pile| (pile.name.as_deref(), pile.nodes.iter().collect()))
        .collect();
    assert_eq!(
        shown,
        vec![
            (Some("ask ada"), vec![&c]),
            (Some("on laptop"), vec![&b, &a]),
            (None, vec![&sooner, &later]),
        ]
    );
    assert_eq!(
        piles[0].back, None,
        "a named pile comes back only when opened"
    );
    let sooner_until = w.marks.get(&sooner).facts().snooze().unwrap().until;
    assert_eq!(piles[2].back, Some(sooner_until));
    assert!(sooner_until < w.marks.get(&later).facts().snooze().unwrap().until);
}

// Todos and deadlines

#[test]
fn t1_a_todo_stays_on_the_plate_rising_slowly_until_done() {
    let mut w = world();
    let n = note(&mut w, "n");
    w.todo(&n, None);
    assert_eq!(w.hand(), "n · todo · 0m");
    assert!(w.priority(&n).unwrap() < LAMP_THRESHOLD);
    w.pass(hours(24 * 60));
    assert_eq!(w.hand(), "n · todo · 60.0d");
    w.done(&n);
    assert_eq!(w.hand(), "");
}

#[test]
fn t2_a_todo_with_a_start_waits_for_it() {
    let mut w = world();
    let n = note(&mut w, "n");
    w.todo(&n, Some(Until::Day(jiff::civil::date(2026, 8, 25))));
    assert_eq!(w.hand(), "");
    assert_eq!(
        w.deal().next_change,
        Some("2026-08-25T00:00:00Z".parse().unwrap())
    );
    w.pass(hours(39));
    assert_eq!(w.hand(), "n · todo · 0m");
}

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
fn t4_a_todo_on_a_thread_stays_whatever_its_source_says() {
    let mut w = world();
    let a = w.agent("a");
    w.finishes(&a);
    w.todo(&a, None);
    assert_eq!(w.hand(), "a · todo · 0m");
    w.writes_to(&a);
    w.works(&a);
    assert_eq!(w.hand(), "a · todo · 0m");
    // Named by the source, since no claim names it.
    assert_eq!(w.deal().cards[0].title, "a");
}

#[test]
fn t5_writing_to_a_thread_keeps_its_todo() {
    let mut w = world();
    let a = w.agent("a");
    w.finishes(&a);
    w.todo(&a, None);
    w.writes_to(&a);
    w.finishes(&a);
    assert!(w.marks.get(&a).facts().todo().is_some());
}

#[test]
fn t6_done_or_mute_takes_back_the_todo_the_deadline_and_the_snooze() {
    for take_back in [Said::Done, Said::Mute] {
        let mut w = world();
        let n = note(&mut w, "n");
        w.todo(&n, None);
        w.say(
            &n,
            Said::Deadline {
                by: Until::In(hours(1)),
                lead_days: 1,
            },
        );
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
    w.todo(&gone, None);
    w.write_note(&gone, "gone", true);
    assert_eq!(w.hand(), "");
}

#[test]
fn a_note_and_a_label_name_themselves_and_file_under_their_labels() {
    let mut w = world();
    let n = note(&mut w, "buy milk\nand eggs");
    let label = uuid::Uuid::new_v4();
    w.tell(Fact::Label {
        label,
        name: "home".into(),
        parent: None,
    });
    w.tell(Fact::Labeled {
        node: n.clone(),
        label,
        present: true,
    });
    w.todo(&n, None);
    w.todo(&Address::label(label), None);
    let hand = w.deal();
    let card = |address: &Address| {
        hand.cards
            .iter()
            .find(|card| &card.address == address)
            .unwrap()
            .clone()
    };
    assert_eq!(card(&n).heading(), "home › buy milk");
    assert_eq!(card(&Address::label(label)).heading(), "home");
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
    let a = w.dm("D1", "u1");
    w.pass(hours(3));
    w.dm("D2", "u2");
    w.pass(hours(1));
    w.dm("D3", "u3");
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

    w.post(&a, false);
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
    let d = w.dm("D1", "u1");
    w.snooze(&d, hours(1));
    assert_eq!(w.deal().next_change, Some(w.now.timestamp() + hours(1)));
    w.pass(hours(1));
    assert_eq!(w.deal().next_change, None);
    let cursor = w.deal().cards[0].cursor.clone();
    w.skips.skip(d, cursor, w.now.timestamp());
    assert_eq!(w.deal().next_change, Some(w.now.timestamp() + SKIP_HOLD));
}

#[test]
fn x4_tomorrow_is_the_users_own_midnight() {
    let mut w = world_in(TimeZone::fixed(
        Offset::from_seconds(5 * 3600 + 1800).unwrap(),
    ));
    let d = w.dm("D1", "u1");
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
    Dm(usize),
    Read(usize),
    Snooze(usize, i64),
    Done(usize),
    Mute(usize),
    Todo(usize, Option<i64>),
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
        (0usize..2).prop_map(Step::Dm),
        (0usize..2).prop_map(Step::Read),
        ((0usize..5), (1i64..600)).prop_map(|(node, mins)| Step::Snooze(node, mins)),
        (0usize..5).prop_map(Step::Done),
        (0usize..5).prop_map(Step::Mute),
        ((0usize..5), proptest::option::of(0i64..3))
            .prop_map(|(node, days)| Step::Todo(node, days)),
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
        let dms = ["D1", "D2"];
        let dm_nodes: Vec<Address> = dms.iter().map(|channel| Address::new("slack", format!("acme/{channel}"))).collect();
        let all: Vec<Address> = agents.iter().chain(&dm_nodes).cloned().collect();
        for step in steps {
            match step {
                Step::Pass(minutes) => w.pass(mins(minutes)),
                Step::Finishes(i) => w.finishes(&agents[i]),
                Step::Asks(i) => w.asks(&agents[i]),
                Step::Works(i) => w.works(&agents[i]),
                Step::WritesTo(i) => w.writes_to(&agents[i]),
                Step::Dm(i) => { w.dm(dms[i], ["u1", "u2"][i]); }
                Step::Read(i) => {
                    if w.things.contains_key(&dm_nodes[i]) {
                        w.read(&dm_nodes[i]);
                    }
                }
                Step::Snooze(i, minutes) => w.snooze(&all[i], mins(minutes)),
                Step::Done(i) => w.done(&all[i]),
                Step::Mute(i) => w.say(&all[i], Said::Mute),
                Step::Todo(i, days) => w.todo(&all[i], days.map(|days| Until::In(hours(24 * days)))),
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
                    proptest::prop_assert_eq!(card.address.kind(), "thread");
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
    let asking = w.dm("D1", "u1");
    let snoozed = w.dm("D2", "u2");
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
