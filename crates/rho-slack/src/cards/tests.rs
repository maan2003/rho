//! The S cases of the dealer's `cases.md`, each under its row's name: the
//! claims this source makes for a history, ranked by the dealer as the
//! frame ranks them.

use std::collections::HashMap;

use jiff::{SignedDuration, Zoned};
use rho_dealer::curve::*;
use rho_dealer::facts::{Device, Entry, Fact, Seen};
use rho_dealer::rank::{Hand, Sources, rank};
use rho_dealer::{Address, Marks, Skips, Until};

use super::*;
use crate::config::WorkspaceName;
use crate::types::{ChannelId, Conversation, ConversationKind, Message, Ts, User, UserId};

struct World {
    slack: Model,
    ts: u64,
    marks: Marks,
    skips: Skips,
    now: Zoned,
    names: HashMap<Address, String>,
}

fn world() -> World {
    let mut slack = Model::new(WorkspaceName("acme".into()));
    slack.set_self(UserId("ME".into()));
    slack.add_users(["ME", "U1", "U2", "U3", "U4"].map(|id| User {
        id: UserId(id.into()),
        name: id.to_lowercase(),
        handle: id.to_lowercase(),
    }));
    let conversation = |id: &str, kind, name: &str, user: Option<&str>| Conversation {
        id: ChannelId(id.into()),
        kind,
        name: name.into(),
        user: user.map(|user| UserId(user.into())),
        members: Vec::new(),
    };
    slack.add_conversations([
        conversation("C1", ConversationKind::Channel, "design", None),
        conversation("D1", ConversationKind::DirectMessage, "u1", Some("U1")),
        conversation("D2", ConversationKind::DirectMessage, "u2", Some("U2")),
    ]);
    World {
        slack,
        ts: 0,
        marks: Marks::default(),
        skips: Skips::default(),
        now: "2026-08-23T09:00:00Z"
            .parse::<Timestamp>()
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::UTC),
        names: HashMap::new(),
    }
}

fn hours(n: i64) -> SignedDuration {
    SignedDuration::from_hours(n)
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-3
}

impl World {
    fn pass(&mut self, by: SignedDuration) {
        self.now = self.now.checked_add(by).unwrap();
    }

    fn ms(&self) -> i64 {
        self.now.timestamp().as_millisecond()
    }

    fn next_ts(&mut self) -> Ts {
        // Seconds with a sequence in the micros, so two messages in one
        // second are still two.
        self.ts += 1;
        Ts(format!(
            "{}.{:06}",
            self.now.timestamp().as_second(),
            self.ts
        ))
    }

    fn post(&mut self, channel: &str, thread: Option<&Ts>, user: &str, text: &str) -> Ts {
        let ts = self.next_ts();
        let message = Message {
            ts: ts.clone(),
            thread_ts: thread.cloned(),
            channel: ChannelId(channel.into()),
            user: Some(UserId(user.into())),
            bot_name: None,
            bot_id: None,
            blocks: Vec::new(),
            text: text.into(),
            attachments: Vec::new(),
            files: Vec::new(),
            subtype: None,
            reply_count: 0,
            reply_users: Vec::new(),
            latest_reply: None,
            edited: false,
            reactions: Vec::new(),
        };
        self.slack.note_message(&message, self.ms());
        ts
    }

    fn node(&mut self, channel: &str, thread: Option<&Ts>, name: &str) -> Address {
        let channel = ChannelId(channel.into());
        let unit = match thread {
            Some(root) => Unit::thread(&channel, root),
            None => Unit::conversation(&channel),
        };
        let address = address(&self.slack, &unit);
        self.names.insert(address.clone(), name.into());
        address
    }

    fn unit(address: &Address) -> Unit {
        match SlackAddress::from_address(address) {
            Some(SlackAddress::Unit(_, unit)) => unit,
            _ => panic!("not a unit: {address}"),
        }
    }

    /// A direct message from `user`.
    fn dm(&mut self, channel: &str, user: &str) -> Address {
        self.post(channel, None, user, "hey");
        self.node(channel, None, channel)
    }

    /// A thread in #design the user follows, with a reply from each of
    /// `users` in turn.
    fn thread(&mut self, users: &[&str]) -> Address {
        let root = self.post("C1", None, "ME", "a question");
        self.slack.follow(&ChannelId("C1".into()), &root);
        for user in users {
            self.post("C1", Some(&root), user, "an answer");
        }
        self.node("C1", Some(&root), "thread")
    }

    /// Read in Slack, anywhere: Slack's own read cursor at the newest.
    fn read(&mut self, address: &Address) {
        let unit = Self::unit(address);
        let newest = self.slack.unit(&unit).unwrap().newest.clone();
        match &unit.thread {
            Some(root) => {
                let key = self.slack.key(&unit.channel, root);
                self.slack.mark_thread_read(&key, &newest);
            }
            None => {
                self.slack.mark_read(&unit.channel, &newest);
            }
        }
    }

    fn tell(&mut self, fact: Fact) {
        let at = self.marks.next_at(&self.now);
        self.now = at.clone();
        self.marks.apply([Entry {
            device: Device([0; 16]),
            at,
            fact,
        }]);
    }

    fn seen(&self, address: &Address) -> Seen {
        seen(&self.slack, &Self::unit(address))
    }

    fn done(&mut self, address: &Address) {
        let seen = self.seen(address);
        self.tell(Fact::Settled {
            node: address.clone(),
            seen,
        });
    }

    fn todo(&mut self, address: &Address) {
        let seen = self.seen(address);
        self.tell(Fact::Todo {
            node: address.clone(),
            start: None,
            seen,
        });
    }

    fn snooze(&mut self, address: &Address, by: SignedDuration) {
        self.tell(Fact::Snooze {
            node: address.clone(),
            until: Until::In(by),
        });
    }

    fn deal(&self) -> Hand {
        let claims = claims(&self.slack, None, &self.marks, self.now.timestamp(), None);
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

    /// The hand as lines of `name · label`, top first.
    fn hand(&self) -> String {
        self.deal()
            .cards
            .iter()
            .map(|card| {
                let name = self
                    .names
                    .get(&card.address)
                    .cloned()
                    .unwrap_or_else(|| card.address.key().to_owned());
                format!("{name} · {}", card.label)
            })
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
}

#[test]
fn s1_a_direct_message_asks_before_a_mention_before_a_big_thread() {
    let mut w = world();
    let big = w.thread(&["U1", "U2", "U3"]);
    let small = w.thread(&["U1"]);
    w.post("C1", None, "U2", "<@ME> can you look");
    let mention = w.node("C1", None, "mention");
    let dm = w.dm("D1", "U1");
    assert!(close(w.priority(&dm).unwrap(), SLACK_DM_HEAD_START));
    assert!(
        close(w.priority(&small).unwrap(), SLACK_DM_HEAD_START),
        "a thread of three people or fewer asks like a direct message"
    );
    assert!(close(
        w.priority(&mention).unwrap(),
        SLACK_MENTION_HEAD_START
    ));
    assert!(close(w.priority(&big).unwrap(), SLACK_THREAD_HEAD_START));
    w.pass(hours(2));
    assert!(
        close(
            w.priority(&big).unwrap(),
            SLACK_THREAD_HEAD_START + WAITING_SLOPE_PER_DAY / 12.0
        ),
        "and they all rise alike"
    );
}

#[test]
fn s1_a_card_says_why_in_the_conversation_s_name() {
    let mut w = world();
    w.dm("D1", "U1");
    assert_eq!(w.hand(), "D1 · unread in @u1 · needs reply · 0m");
    let claim = claims(&w.slack, None, &w.marks, w.now.timestamp(), None).remove(0);
    assert_eq!(claim.context, "@u1", "the card is where it is");
    assert_eq!(claim.under, None, "a conversation hangs under nothing");
}

#[test]
fn s2_channel_traffic_is_barely_there_and_gone_within_the_day() {
    let mut w = world();
    w.post("C1", None, "U1", "lunch?");
    let room = w.node("C1", None, "room");
    assert!(close(
        w.priority(&room).unwrap(),
        CHANNEL_TRAFFIC_HEAD_START
    ));
    w.pass(hours(23));
    assert!(w.priority(&room).is_some());
    w.pass(hours(1));
    assert!(w.priority(&room).is_none());

    let mut w = world();
    w.post("C1", None, "U1", "lunch?");
    w.post("C1", None, "U2", "yes");
    let room = w.node("C1", None, "room");
    w.pass(hours(11));
    assert!(w.priority(&room).is_some());
    w.pass(hours(1));
    assert!(
        w.priority(&room).is_none(),
        "someone else answered, so half a day"
    );
}

#[test]
fn s3_s4_read_or_replied_is_nothing_until_a_new_message() {
    let mut w = world();
    let d = w.dm("D1", "U1");
    w.read(&d);
    assert_eq!(w.hand(), "", "read in Slack");
    w.pass(hours(1));
    w.post("D1", None, "U1", "one more thing");
    assert_eq!(w.hand(), "D1 · unread in @u1 · needs reply · 0m");
    w.post("D1", None, "ME", "sure");
    assert_eq!(w.hand(), "", "replied");
}

#[test]
fn s3_s4_done_on_another_device_holds_until_a_newer_message() {
    let mut w = world();
    let d = w.dm("D1", "U1");
    // Said on the phone: this device's Slack still has it unread.
    w.done(&d);
    assert_eq!(w.hand(), "");
    w.pass(hours(1));
    w.post("D1", None, "U1", "one more thing");
    assert_eq!(
        w.hand(),
        "D1 · unread in @u1 · needs reply · 0m",
        "counting from the new message"
    );
}

#[test]
fn s5_a_todo_on_a_thread_outlasts_reading_it() {
    let mut w = world();
    let thread = w.thread(&["U1", "U2", "U3"]);
    w.read(&thread);
    w.todo(&thread);
    assert_eq!(w.hand(), "thread · todo · 0m");
}

#[test]
fn s6_a_room_muted_in_slack_has_nothing() {
    let mut w = world();
    w.dm("D1", "U1");
    w.slack.set_muted([ChannelId("D1".into())]);
    assert_eq!(w.hand(), "");
}

#[test]
fn s7_a_snoozed_room_holds_its_threads() {
    let mut w = world();
    let room = w.node("C1", None, "room");
    w.snooze(&room, hours(1));
    let thread = w.thread(&["U1"]);
    assert_eq!(w.hand(), "", "the thread is in the snoozed room");
    w.pass(hours(1));
    assert!(w.priority(&thread).is_some());
}

/// A card's cursor is the unit's newest message: a skip holds while
/// nothing new arrives and goes when something does (X2).
#[test]
fn a_claim_s_cursor_is_its_newest_message() {
    let mut w = world();
    w.dm("D1", "U1");
    let newest = w.post("D1", None, "U1", "again");
    let claim = claims(&w.slack, None, &w.marks, w.now.timestamp(), None).remove(0);
    assert_eq!(claim.cursor, newest.0);
    assert_eq!(
        claim.others.len(),
        2,
        "a direct conversation counts what the other side wrote (Z5)"
    );
}
