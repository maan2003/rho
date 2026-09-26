//! The cases in `cases.md`, each under its row's name, and the rules
//! every history keeps.

use std::collections::{BTreeSet, HashMap};

use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp, Zoned};
use rho_agent_types::{AgentId, AgentIdDomain, AgentRole, Place, UnixMs};
use rho_agents2_client::protocol::{AgentInfo, ChatEvent, ChatKind, Effort, MessageId, Party};
use rho_slack::config::WorkspaceName;
use rho_slack::model::Model;
use rho_slack::types::{ChannelId, Conversation, ConversationKind, Message, Ts, User, UserId};

use super::*;
use crate::curve::*;
use crate::facts::{Device, Entry, Fact, Seen};
use crate::notes::NoteRev;
use crate::until::Until;

/// What the user says in these scenarios, about the node it is said to.
enum Said {
    Done,
    Mute,
    Unmute,
    Snooze { until: Until },
    Deadline { by: Until, lead_days: u32 },
}

/// Everything `rank` reads, and a clock to move.
struct World {
    agents: HashMap<AgentId, AgentInfo>,
    next_id: u64,
    slack: Model,
    ts: u64,
    marks: Marks,
    skips: Skips,
    cache: Cache,
    now: Zoned,
    names: HashMap<NodeId, String>,
}

fn world() -> World {
    world_in(TimeZone::UTC)
}

fn world_in(zone: TimeZone) -> World {
    let mut slack = Model::new(WorkspaceName("acme".into()));
    slack.set_self(UserId("ME".into()));
    slack.add_users(["ME", "U1", "U2", "U3", "U4"].map(|id| User {
        id: UserId(id.into()),
        name: id.to_lowercase(),
        handle: id.to_lowercase(),
    }));
    slack.add_conversations([
        Conversation {
            id: ChannelId("C1".into()),
            kind: ConversationKind::Channel,
            name: "design".into(),
            user: None,
            members: Vec::new(),
        },
        Conversation {
            id: ChannelId("D1".into()),
            kind: ConversationKind::DirectMessage,
            name: "u1".into(),
            user: Some(UserId("U1".into())),
            members: Vec::new(),
        },
        Conversation {
            id: ChannelId("D2".into()),
            kind: ConversationKind::DirectMessage,
            name: "u2".into(),
            user: Some(UserId("U2".into())),
            members: Vec::new(),
        },
    ]);
    World {
        agents: HashMap::new(),
        next_id: 0,
        slack,
        ts: 0,
        marks: Marks::default(),
        skips: Skips::default(),
        cache: Cache::default(),
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

    fn ms(&self) -> u64 {
        self.now.timestamp().as_millisecond() as u64
    }

    fn agent(&mut self, name: &str) -> NodeId {
        self.next_id += 1;
        let id = AgentId::from_counter(self.next_id, &AgentIdDomain(0)).unwrap();
        self.agents.insert(
            id,
            AgentInfo {
                id,
                place: Place {
                    workset: "fixture-workset".into(),
                    cwd: "/src/repo".into(),
                    mode: Default::default(),
                    origin: None,
                },
                role: AgentRole::default(),
                parent: None,
                user_owned: false,
                model: "gpt-6-sol".into(),
                effort: Effort::Medium,
                archived: false,
                status: None,
                running_since: None,
                chat: Vec::new(),
            },
        );
        let node = NodeId::Agent(id);
        self.names.insert(node.clone(), name.into());
        self.tell(Fact::Named {
            node: node.clone(),
            name: Some(name.into()),
        });
        node
    }

    fn push(&mut self, node: &NodeId, kind: ChatKind) -> u64 {
        let at = UnixMs(self.ms());
        let agent = self.agents.get_mut(&node.agent().unwrap()).unwrap();
        if let ChatKind::Status(text) = &kind {
            agent.status = Some(text.clone());
        }
        let chat = &mut agent.chat;
        let seq = chat.last().map_or(1, |event| event.seq + 1);
        chat.push(ChatEvent { seq, at, kind });
        seq
    }

    fn reply(&mut self, node: &NodeId) {
        let id = node.agent().unwrap();
        self.push(
            node,
            ChatKind::Message {
                id: MessageId(self.newest(node) + 1),
                from: Party::Agent(id),
                to: Party::Human,
                text: "agent reply".into(),
            },
        );
    }

    fn writes_to(&mut self, node: &NodeId) {
        let id = node.agent().unwrap();
        self.push(
            node,
            ChatKind::Message {
                id: MessageId(self.newest(node) + 1),
                from: Party::Human,
                to: Party::Agent(id),
                text: "human request".into(),
            },
        );
    }

    fn newest(&self, node: &NodeId) -> u64 {
        self.agents[&node.agent().unwrap()]
            .chat
            .iter()
            .map(|event| event.seq)
            .max()
            .unwrap_or(0)
    }

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

    /// How far the user has seen `node`: everything its source has.
    fn seen(&self, node: &NodeId) -> Seen {
        match node {
            NodeId::Agent(_) => Seen::Agent(self.newest(node)),
            NodeId::Slack(unit) => self
                .slack
                .unit(&model_unit(unit))
                .map_or(Seen::Whole, |facts| Seen::Slack(facts.newest.0.clone())),
            _ => Seen::Whole,
        }
    }

    fn say(&mut self, node: &NodeId, said: Said) {
        let node = node.clone();
        let fact = match said {
            Said::Done => Fact::Settled {
                seen: self.seen(&node),
                node,
            },
            Said::Mute => Fact::Mute { node },
            Said::Unmute => Fact::Unmute { node },
            Said::Snooze { until } => Fact::Snooze { node, until },
            Said::Deadline { by, lead_days } => Fact::Deadline {
                node,
                by,
                lead_days,
            },
        };
        self.tell(fact);
    }

    fn snooze(&mut self, node: &NodeId, until: SignedDuration) {
        self.say(
            node,
            Said::Snooze {
                until: Until::In(until),
            },
        );
    }

    fn done(&mut self, node: &NodeId) {
        self.say(node, Said::Done);
    }

    fn todo(&mut self, node: &NodeId, start: Option<Until>) {
        self.tell(Fact::Todo {
            node: node.clone(),
            start,
            seen: self.seen(node),
        });
    }

    fn write_note(&mut self, node: &NodeId, body: &str, deleted: bool) {
        let NodeId::Note(note) = node else {
            panic!("not a note")
        };
        let at = self.marks.next_at(&self.now);
        self.now = at.clone();
        self.marks.apply_notes([NoteRev {
            note: *note,
            device: Device([0; 16]),
            created: at.timestamp(),
            at,
            body: body.into(),
            deleted,
        }]);
    }

    fn next_ts(&mut self) -> Ts {
        // Slack's timestamps are seconds with a sequence in the micros, so
        // two messages in one second are still two.
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
        self.slack.note_message(&message, self.ms() as i64);
        ts
    }

    fn slack_node(&mut self, channel: &str, thread: Option<&Ts>, name: &str) -> NodeId {
        let node = NodeId::Slack(SlackUnit {
            workspace: "acme".into(),
            channel: channel.into(),
            thread: thread.map(|ts| ts.0.clone()),
        });
        self.names.insert(node.clone(), name.into());
        node
    }

    /// A direct message from `user`.
    fn dm(&mut self, channel: &str, user: &str) -> NodeId {
        self.post(channel, None, user, "hey");
        self.slack_node(channel, None, channel)
    }

    /// A thread in #design the user follows, with a reply from each of
    /// `users` in turn.
    fn thread(&mut self, users: &[&str]) -> (NodeId, Ts) {
        let root = self.post("C1", None, "ME", "a question");
        self.slack.follow(&ChannelId("C1".into()), &root);
        for user in users {
            self.post("C1", Some(&root), user, "an answer");
        }
        let node = self.slack_node("C1", Some(&root), "thread");
        (node, root)
    }

    fn read(&mut self, node: &NodeId) {
        let unit = model_unit(node.slack().unwrap());
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

    fn deal(&mut self) -> Hand {
        let sources = Sources {
            agents: &self.agents,
            slack: Some(Slack {
                model: &self.slack,
                mirror: None,
            }),
            marks: &self.marks,
            skips: &self.skips,
        };
        rank(&sources, &self.now, &mut self.cache)
    }

    /// The hand as lines of `name · label`, top first.
    fn traced(&mut self) -> (Hand, Trace) {
        let sources = Sources {
            agents: &self.agents,
            slack: Some(Slack {
                model: &self.slack,
                mirror: None,
            }),
            marks: &self.marks,
            skips: &self.skips,
        };
        rank_traced(&sources, &self.now, &mut self.cache)
    }

    fn hand(&mut self) -> String {
        let hand = self.deal();
        hand.cards
            .iter()
            .map(|card| {
                let name = self
                    .names
                    .get(&card.node)
                    .cloned()
                    .unwrap_or(card.node.key());
                format!("{name} · {}", card.label)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn priority(&mut self, node: &NodeId) -> Option<f64> {
        self.deal()
            .cards
            .into_iter()
            .find(|card| &card.node == node)
            .map(|card| card.priority)
    }

    fn top(&mut self) -> Option<String> {
        let hand = self.deal();
        hand.top(None).map(|card| self.names[&card.node].clone())
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-3
}

// Agents: raw physical chat seq, not turn envelopes or status messages.

#[test]
fn agent_reply_requires_output_to_human_and_fades() {
    let mut w = world();
    let a = w.agent("a");
    w.writes_to(&a);
    assert_eq!(w.hand(), "");
    w.pass(mins(5));
    w.reply(&a);
    assert_eq!(w.hand(), "a · finished · 0m ago");
    assert!(w.priority(&a).unwrap() >= CHIME_THRESHOLD);
    w.pass(hours(72));
    assert_eq!(w.hand(), "");
}

#[test]
fn human_reply_after_agent_output_suppresses_card_until_another_agent_reply() {
    let mut w = world();
    let a = w.agent("a");
    w.reply(&a);
    w.writes_to(&a);
    assert_eq!(w.hand(), "");
    w.reply(&a);
    assert_eq!(w.hand(), "a · finished · 0m ago");
}

#[test]
fn unnamed_agent_card_uses_latest_human_request_but_name_wins() {
    let mut w = world();
    let a = w.agent("given");
    let id = a.agent().unwrap();
    w.tell(Fact::Named {
        node: a.clone(),
        name: None,
    });
    w.push(
        &a,
        ChatKind::Message {
            id: MessageId(1),
            from: Party::Human,
            to: Party::Agent(id),
            text: "investigate retries\nextra detail".into(),
        },
    );
    w.reply(&a);
    let sources = Sources {
        agents: &w.agents,
        slack: None,
        marks: &w.marks,
        skips: &w.skips,
    };
    assert_eq!(title(&sources, &a, &mut w.cache), "investigate retries");
    w.push(&a, ChatKind::Rewound { to: 1 });
    w.push(
        &a,
        ChatKind::Message {
            id: MessageId(2),
            from: Party::Human,
            to: Party::Agent(id),
            text: "try another approach".into(),
        },
    );
    let sources = Sources {
        agents: &w.agents,
        slack: None,
        marks: &w.marks,
        skips: &w.skips,
    };
    assert_eq!(title(&sources, &a, &mut w.cache), "try another approach");
    w.tell(Fact::Named {
        node: a.clone(),
        name: Some("preferred".into()),
    });
    let sources = Sources {
        agents: &w.agents,
        slack: None,
        marks: &w.marks,
        skips: &w.skips,
    };
    assert_eq!(title(&sources, &a, &mut w.cache), "preferred");
}

#[test]
fn physical_seen_boundary_and_status_rows_do_not_invent_output() {
    let mut w = world();
    let a = w.agent("a");
    w.push(&a, ChatKind::Status("working".into()));
    assert_eq!(w.hand(), "");
    w.reply(&a);
    let seq = w.newest(&a);
    w.done(&a);
    assert_eq!(w.hand(), "");
    w.push(&a, ChatKind::Status("waiting".into()));
    assert_eq!(
        w.hand(),
        "",
        "status cannot revive an already-seen agent reply"
    );
    w.reply(&a);
    assert_eq!(w.hand(), "a · waiting · 0m ago");
    assert_eq!(w.deal().cards[0].cursor, (seq + 2).to_string());
}

#[test]
fn rewound_branch_remains_in_physical_unread_chat() {
    let mut w = world();
    let a = w.agent("a");
    w.reply(&a);
    w.done(&a);
    w.reply(&a);
    w.push(&a, ChatKind::Rewound { to: 1 });
    assert_eq!(w.hand(), "a · finished · 0m ago");
    assert_eq!(
        w.deal().cards[0].cursor,
        "3",
        "rewind has its own physical seq"
    );
    w.done(&a);
    assert_eq!(w.hand(), "");
}

#[test]
fn delegated_children_do_not_become_user_cards_unless_handed_to_user() {
    let mut w = world();
    let parent = w.agent("parent");
    let child = w.agent("child");
    let parent_id = parent.agent().unwrap();
    let child_id = child.agent().unwrap();
    let child_info = w.agents.get_mut(&child_id).unwrap();
    child_info.parent = Some(parent_id);
    w.reply(&child);
    assert_eq!(
        w.hand(),
        "",
        "the delegated child speaks to its parent, not the user's dealer"
    );
    w.agents.get_mut(&child_id).unwrap().user_owned = true;
    assert_eq!(w.hand(), "child · finished · 0m ago");
}

#[test]
fn muted_and_archived_agents_have_no_source_card_but_dated_marks_stay() {
    let mut w = world();
    let a = w.agent("a");
    w.reply(&a);
    w.say(&a, Said::Mute);
    assert_eq!(w.hand(), "");
    w.say(&a, Said::Unmute);
    assert_eq!(w.hand(), "a · finished · 0m ago");
    w.agents.get_mut(&a.agent().unwrap()).unwrap().archived = true;
    assert_eq!(w.hand(), "");
    w.todo(&a, None);
    assert_eq!(w.hand(), "a · todo · 0m");
}

#[test]
fn agent_messages_to_other_agents_do_not_count_as_a_human_reply() {
    let mut w = world();
    let a = w.agent("a");
    let b = w.agent("b");
    w.push(
        &a,
        ChatKind::Message {
            id: MessageId(3),
            from: Party::Agent(a.agent().unwrap()),
            to: Party::Agent(b.agent().unwrap()),
            text: "side-channel".into(),
        },
    );
    assert_eq!(w.hand(), "");
    w.reply(&a);
    assert_eq!(w.hand(), "a · finished · 0m ago");
}

// Snoozes

#[test]
fn z1_a_snooze_holds_until_it_ends() {
    let mut w = world();
    let d = w.dm("D1", "U1");
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
    let d = w.dm("D1", "U1");
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
    w.reply(&a);
    w.snooze(&a, hours(1));
    w.pass(mins(5));
    w.writes_to(&a);
    let d = w.dm("D1", "U1");
    w.snooze(&d, hours(1));
    w.read(&d);
    w.pass(hours(2));
    assert_eq!(w.hand(), "");
}

#[test]
fn z4_a_reply_within_the_hour_of_the_users_message_comes_through_a_snooze() {
    let mut w = world();
    let a = w.agent("a");
    w.reply(&a);
    w.snooze(&a, hours(4));
    w.pass(mins(30));
    w.writes_to(&a);
    w.pass(mins(30));
    w.reply(&a);
    assert_eq!(w.hand(), "a · finished · 0m ago");

    let b = w.agent("b");
    w.reply(&b);
    w.snooze(&b, hours(4));
    w.pass(mins(30));
    w.writes_to(&b);
    w.pass(mins(61));
    w.reply(&b);
    assert!(
        w.priority(&b).is_none(),
        "an hour and more is not a conversation"
    );
}

#[test]
fn z5_a_direct_conversation_comes_back_higher_for_what_it_slept_through() {
    let mut w = world();
    let d = w.dm("D1", "U1");
    w.snooze(&d, hours(2));
    let (big, root) = w.thread(&["U1", "U2", "U3", "U4"]);
    w.snooze(&big, hours(2));
    w.pass(hours(1));
    w.post("D1", None, "U1", "still there?");
    w.post("D1", None, "U1", "hello?");
    w.post("C1", Some(&root), "U2", "and another");
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
    let d = w.dm("D1", "U1");
    for _ in 0..3 {
        w.snooze(&d, hours(1));
        w.pass(hours(1));
    }
    assert_eq!(w.marks.get(&d).facts().snoozes(), 3);
    assert_eq!(w.hand(), "D1 · unread in @u1 · needs reply · 0m");
}

// Todos and deadlines

fn note(w: &mut World, name: &str) -> NodeId {
    let node = NodeId::Note(uuid::Uuid::new_v4());
    w.write_note(&node, name, false);
    w.names.insert(node.clone(), name.into());
    node
}

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
fn t4_a_todo_on_an_agent_stays_while_status_changes() {
    let mut w = world();
    let a = w.agent("a");
    w.reply(&a);
    w.todo(&a, None);
    w.writes_to(&a);
    w.push(&a, ChatKind::Status("working".into()));
    assert_eq!(w.hand(), "a · todo · 0m");
    w.pass(hours(1));
    w.reply(&a);
    assert_eq!(w.deal().cards.len(), 1);
}

#[test]
fn t5_writing_to_an_agent_keeps_its_todo() {
    let mut w = world();
    let a = w.agent("a");
    w.reply(&a);
    w.todo(&a, None);
    w.writes_to(&a);
    w.reply(&a);
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

// Slack

#[test]
fn s1_a_direct_message_asks_before_a_mention_before_a_big_thread() {
    let mut w = world();
    let (big, _) = w.thread(&["U1", "U2", "U3"]);
    let (small, _) = w.thread(&["U1"]);
    w.post("C1", None, "U2", "<@ME> can you look");
    let mention = w.slack_node("C1", None, "mention");
    let dm = w.dm("D1", "U1");
    assert!(close(w.priority(&dm).unwrap(), SLACK_DM_HEAD_START));
    assert!(close(w.priority(&small).unwrap(), SLACK_DM_HEAD_START));
    assert!(close(
        w.priority(&mention).unwrap(),
        SLACK_MENTION_HEAD_START
    ));
    assert!(close(w.priority(&big).unwrap(), SLACK_THREAD_HEAD_START));
    w.pass(hours(2));
    assert!(close(
        w.priority(&big).unwrap(),
        SLACK_THREAD_HEAD_START + WAITING_SLOPE_PER_DAY / 12.0
    ));
}

#[test]
fn s2_channel_traffic_is_barely_there_and_gone_within_the_day() {
    let mut w = world();
    w.post("C1", None, "U1", "lunch?");
    let room = w.slack_node("C1", None, "room");
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
    let room = w.slack_node("C1", None, "room");
    w.pass(hours(12));
    assert!(w.priority(&room).is_none(), "someone else answered");
}

#[test]
fn s3_s4_read_or_replied_is_nothing_until_a_new_message() {
    let mut w = world();
    let d = w.dm("D1", "U1");
    w.read(&d);
    assert_eq!(w.hand(), "");
    w.pass(hours(1));
    w.post("D1", None, "U1", "one more thing");
    assert_eq!(w.hand(), "D1 · unread in @u1 · needs reply · 0m");
    w.post("D1", None, "ME", "sure");
    assert_eq!(w.hand(), "");
}

#[test]
fn s3_s4_done_on_another_device_holds_until_a_newer_message() {
    let mut w = world();
    let d = w.dm("D1", "U1");
    // Said on the phone: this device's Slack still has it unread.
    w.done(&d);
    assert_eq!(w.hand(), "");
    w.post("D1", None, "U1", "one more thing");
    assert_eq!(w.hand(), "D1 · unread in @u1 · needs reply · 0m");
}

#[test]
fn s5_a_todo_on_a_thread_outlasts_reading_it() {
    let mut w = world();
    let (thread, _) = w.thread(&["U1", "U2", "U3"]);
    w.read(&thread);
    w.todo(&thread, None);
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
    let room = w.slack_node("C1", None, "room");
    w.snooze(&room, hours(1));
    let (thread, _) = w.thread(&["U1"]);
    assert_eq!(w.hand(), "");
    w.pass(hours(1));
    assert!(w.priority(&thread).is_some());
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

// Every history

#[test]
fn x1_a_node_is_one_card_at_its_strongest() {
    let mut w = world();
    let a = w.agent("a");
    w.reply(&a);
    w.say(
        &a,
        Said::Deadline {
            by: Until::In(hours(24)),
            lead_days: 3,
        },
    );
    assert_eq!(w.hand(), "a · finished · 0m ago");
}

#[test]
fn x2_a_skip_lowers_a_card_for_minutes_and_its_source_moving_ends_it() {
    let mut w = world();
    let a = w.dm("D1", "U1");
    w.pass(hours(1));
    let b = w.dm("D2", "U2");
    assert_eq!(w.top().as_deref(), Some("D1"));
    let cursor = w.deal().cards[0].cursor.clone();
    w.skips.skip(a.clone(), cursor, w.now.timestamp());
    assert_eq!(w.top().as_deref(), Some("D2"));
    w.pass(mins(5));
    assert_eq!(
        w.top().as_deref(),
        Some("D2"),
        "still down after five minutes"
    );
    w.pass(mins(25));
    assert_eq!(w.top().as_deref(), Some("D1"), "back by thirty");

    let cursor = w.deal().cards[0].cursor.clone();
    w.skips.skip(a.clone(), cursor, w.now.timestamp());
    assert!(
        w.deal()
            .cards
            .iter()
            .any(|card| card.node == a && card.skipped)
    );
    w.post("D1", None, "U1", "urgent");
    assert!(
        w.deal()
            .cards
            .iter()
            .any(|card| card.node == a && !card.skipped),
        "something new voids the skip"
    );
    let _ = b;
}

#[test]
fn x3_the_hand_says_when_it_next_changes() {
    let mut w = world();
    let d = w.dm("D1", "U1");
    w.snooze(&d, hours(1));
    assert_eq!(w.deal().next_change, Some(w.now.timestamp() + hours(1)));
    w.pass(hours(1));
    assert_eq!(w.deal().next_change, None);
    let cursor = w.deal().cards[0].cursor.clone();
    w.skips.skip(d, cursor, w.now.timestamp());
    assert_eq!(w.deal().next_change, Some(w.now.timestamp() + SKIP_FADE));
}

#[test]
fn x4_tomorrow_is_the_users_own_midnight() {
    let mut w = world_in(TimeZone::fixed(
        Offset::from_seconds(5 * 3600 + 1800).unwrap(),
    ));
    let d = w.dm("D1", "U1");
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

// The rules every history keeps (X1, X3, X5, Z1), over random ones.

#[derive(Clone, Debug)]
enum Step {
    Pass(i64),
    Reply(usize),
    Status(usize),
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
        (0usize..3).prop_map(Step::Reply),
        (0usize..3).prop_map(Step::Status),
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

fn nodes(hand: &Hand) -> BTreeSet<NodeId> {
    hand.cards.iter().map(|card| card.node.clone()).collect()
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(200))]

    #[test]
    fn every_history_keeps_the_rules(steps in proptest::collection::vec(step(), 1..40)) {
        let mut w = world();
        let agents: Vec<NodeId> = ["a0", "a1", "a2"].iter().map(|name| w.agent(name)).collect();
        let dms = ["D1", "D2"];
        let dm_nodes: Vec<NodeId> = dms.iter().map(|channel| w.slack_node(channel, None, channel)).collect();
        let all: Vec<NodeId> = agents.iter().chain(&dm_nodes).cloned().collect();
        for step in steps {
            match step {
                Step::Pass(minutes) => w.pass(mins(minutes)),
                Step::Reply(i) => w.reply(&agents[i]),
                Step::Status(i) => { w.push(&agents[i], ChatKind::Status("working".into())); },
                Step::WritesTo(i) => w.writes_to(&agents[i]),
                Step::Dm(i) => { w.dm(dms[i], ["U1", "U2"][i]); }
                Step::Read(i) => {
                    if w.slack.unit(&model_unit(dm_nodes[i].slack().unwrap())).is_some() {
                        w.read(&dm_nodes[i]);
                    }
                }
                Step::Snooze(i, minutes) => w.snooze(&all[i], mins(minutes)),
                Step::Done(i) => w.done(&all[i]),
                Step::Mute(i) => w.say(&all[i], Said::Mute),
                Step::Todo(i, days) => w.todo(&all[i], days.map(|days| Until::In(hours(24 * days)))),
                Step::SkipTop => {
                    if let Some(card) = w.deal().cards.first().cloned() {
                        w.skips.skip(card.node, card.cursor, w.now.timestamp());
                    }
                }
            }
            let warm = w.deal();
            // X5: a warm cache changes nothing.
            w.cache = Cache::default();
            let fresh = w.deal();
            proptest::prop_assert_eq!(&warm, &fresh);
            // X1: one card per node.
            proptest::prop_assert_eq!(nodes(&fresh).len(), fresh.cards.len());
            let at = w.now.timestamp();
            for card in &fresh.cards {
                let facts = w.marks.get(&card.node).facts();
                proptest::prop_assert!(!facts.muted());
                // Z1: nothing snoozed shows, but for an agent's quick reply.
                if let Some(snooze) = facts.snooze() && at < snooze.until {
                    proptest::prop_assert!(matches!(card.node, NodeId::Agent(_)));
                }
            }
            // X3: nothing arrives by itself before the next change.
            let before = fresh.next_change.map_or(at + hours(24), |next| next - SignedDuration::from_millis(1));
            if before > at {
                let now = w.now.clone();
                w.now = before.to_zoned(now.time_zone().clone());
                let later = w.deal();
                w.now = now;
                proptest::prop_assert!(nodes(&later).is_subset(&nodes(&fresh)), "{:?} arrived", nodes(&later).difference(&nodes(&fresh)).collect::<Vec<_>>());
            }
        }
    }
}

#[test]
fn a_traced_deal_is_the_same_deal_and_says_what_it_left_out() {
    let mut w = world();
    let asking = w.dm("D1", "U1");
    let snoozed = w.dm("D2", "U2");
    w.snooze(&snoozed, hours(1));
    let quiet = w.agent("a");
    w.reply(&quiet);
    w.done(&quiet);
    let (hand, trace) = w.traced();
    assert_eq!(hand, w.deal());
    let outcome = |node: &NodeId| trace.nodes[node].outcome.clone();
    assert!(
        outcome(&asking).starts_with("card at "),
        "{}",
        outcome(&asking)
    );
    assert!(outcome(&snoozed).starts_with("no card: snoozed until"));
    assert_eq!(outcome(&quiet), "no card: seen through its last reply");
    assert!(
        trace.nodes[&asking]
            .inputs
            .iter()
            .any(|(key, _)| *key == "unit facts")
    );
    assert!(!trace.nodes[&asking].parts.is_empty());
}
