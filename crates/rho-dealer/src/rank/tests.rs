//! The cases in `cases.md`, each under its row's name, and the rules
//! every history keeps.

use std::collections::{BTreeSet, HashMap};

use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp, Zoned};
use rho_agent_types::{AgentId, AgentIdDomain, AgentPos, AgentWant, TurnEdge, TurnOutcome, UnixMs};
use rho_agents_client::fold::MirroredAgent;
use rho_agents_client::protocol::transcript::{RuntimeKind, SpawnedBy, Speaker, TranscriptEvent};
use rho_agents_client::{AgentMap, HostId};
use rho_slack::config::WorkspaceName;
use rho_slack::model::Model;
use rho_slack::types::{ChannelId, Conversation, ConversationKind, Message, Ts, User, UserId};

use super::*;
use crate::curve::*;
use crate::facts::{Fact, Said};
use crate::until::Until;

/// Everything `rank` reads, and a clock to move.
struct World {
    agents: AgentMap,
    host: HostId,
    pos: HashMap<AgentId, u64>,
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
    let mut agents = AgentMap::default();
    let host = HostId::default();
    agents.set_host_data(host, 0, 0);
    World {
        agents,
        host,
        pos: HashMap::new(),
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

    /// One row of an agent's story, folded the way the client folds it.
    fn log(&mut self, agent_id: AgentId, event: TranscriptEvent) {
        let pos = self.pos.entry(agent_id).or_insert(0);
        let mirrored = match self.agents.mirrored(agent_id) {
            Some(mirrored) => {
                let mut mirrored = mirrored.clone();
                mirrored.tell(AgentPos(*pos), &event);
                mirrored
            }
            None => MirroredAgent::new(self.host, agent_id, &event).unwrap(),
        };
        *pos += 1;
        self.agents.told(vec![mirrored]);
    }

    fn agent_under(&mut self, name: &str, parent: Option<AgentId>) -> NodeId {
        let agent_id = AgentId::from_counter(self.pos.len() as u64 + 1, &AgentIdDomain(0)).unwrap();
        let at = UnixMs(self.ms());
        self.log(
            agent_id,
            TranscriptEvent::Created {
                role: rho_agent_types::AgentRole::default(),
                runtime: RuntimeKind::Rho,
                place: rho_agent_types::Place {
                    workset: "0123456789ab".into(),
                    cwd: "/src/repo".into(),
                    mode: Default::default(),
                    origin: None,
                },
                spawned_by: SpawnedBy::Direct,
                spawn_name: None,
                parent,
                model: "sol".into(),
                at,
            },
        );
        let node = NodeId::Agent(agent_id);
        self.names.insert(node.clone(), name.into());
        node
    }

    fn agent(&mut self, name: &str) -> NodeId {
        self.agent_under(name, None)
    }

    fn id(node: &NodeId) -> AgentId {
        node.agent().unwrap()
    }

    fn turn(&mut self, node: &NodeId, edge: TurnEdge) {
        let at = UnixMs(self.ms());
        self.log(Self::id(node), TranscriptEvent::Turn { edge, at });
    }

    fn starts(&mut self, node: &NodeId) {
        self.turn(node, TurnEdge::Started);
    }

    fn ends(&mut self, node: &NodeId, outcome: TurnOutcome) {
        self.turn(node, TurnEdge::Ended(outcome));
    }

    /// A whole turn, finished.
    fn finishes(&mut self, node: &NodeId) {
        self.starts(node);
        self.ends(node, TurnOutcome::Completed);
    }

    fn asks(&mut self, node: &NodeId) {
        self.starts(node);
        let at = UnixMs(self.ms());
        self.log(
            Self::id(node),
            TranscriptEvent::Wants {
                want: AgentWant::Ask,
                summary: None,
                at,
            },
        );
        self.ends(node, TurnOutcome::Completed);
    }

    fn errors(&mut self, node: &NodeId) {
        self.starts(node);
        self.ends(
            node,
            TurnOutcome::Errored {
                message: "boom".into(),
            },
        );
    }

    fn writes_to(&mut self, node: &NodeId) {
        let at = UnixMs(self.ms());
        self.log(
            Self::id(node),
            TranscriptEvent::ClaudeMessage {
                speaker: Speaker::User,
                text: "go on".into(),
                at,
            },
        );
    }

    fn newest(&self, node: &NodeId) -> u64 {
        self.agents.agent_digest(Self::id(node)).unwrap().newest.0
    }

    fn say(&mut self, node: &NodeId, said: Said) {
        let fact = Fact {
            at: self.now.clone(),
            said,
        };
        self.marks.apply([crate::facts::record(node, &fact)]);
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
        let through = match node {
            NodeId::Agent(_) => Cursor::Story(self.newest(node)),
            _ => Cursor::Done,
        };
        self.say(node, Said::Done { through });
    }

    fn todo(&mut self, node: &NodeId, start: Option<Until>) {
        let through = match node {
            NodeId::Agent(_) => Cursor::Story(self.newest(node)),
            _ => Cursor::Done,
        };
        self.say(node, Said::Todo { through, start });
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

// Agents

#[test]
fn a1_a_finished_agent_is_low_and_gone_in_three_days() {
    let mut w = world();
    let a = w.agent("a");
    w.finishes(&a);
    assert_eq!(w.hand(), "a · finished · 0m ago");
    assert!(w.priority(&a).unwrap() < LAMP_THRESHOLD);
    w.pass(hours(71));
    assert_eq!(w.hand(), "a · finished · 3.0d ago");
    w.pass(hours(1));
    assert_eq!(w.hand(), "");
}

#[test]
fn a2_an_agent_asking_rises() {
    let mut w = world();
    let a = w.agent("a");
    w.asks(&a);
    assert_eq!(w.hand(), "a · waiting on reply · 0m");
    let first = w.priority(&a).unwrap();
    w.pass(hours(1));
    assert!(w.priority(&a).unwrap() > first);
}

#[test]
fn a3_an_errored_agent_waits_on_the_user() {
    let mut w = world();
    let a = w.agent("a");
    w.errors(&a);
    assert_eq!(w.hand(), "a · errored · 0m ago");
    assert!(w.priority(&a).unwrap() >= AGENT_BLOCKED_HEAD_START);
}

#[test]
fn a4_an_agent_at_work_or_holding_the_users_message_has_no_card() {
    let mut w = world();
    let a = w.agent("a");
    w.starts(&a);
    assert_eq!(w.hand(), "");
    let b = w.agent("b");
    w.finishes(&b);
    w.pass(mins(1));
    w.writes_to(&b);
    assert_eq!(w.hand(), "", "the message is queued, the ball is with b");
}

#[test]
fn a5_an_agent_the_user_just_wrote_to_comes_back_on_top_and_chimes() {
    let mut w = world();
    let a = w.agent("a");
    w.writes_to(&a);
    w.starts(&a);
    w.pass(mins(5));
    let d = w.dm("D1", "U1");
    w.ends(&a, TurnOutcome::Completed);
    assert_eq!(w.top().as_deref(), Some("a"));
    assert!(w.priority(&a).unwrap() >= CHIME_THRESHOLD);
    assert!(w.priority(&d).is_some());
}

#[test]
fn a6_done_holds_until_a_newer_turn_ends() {
    let mut w = world();
    let a = w.agent("a");
    w.finishes(&a);
    w.done(&a);
    assert_eq!(w.hand(), "");
    w.pass(hours(2));
    w.finishes(&a);
    assert_eq!(w.hand(), "a · finished · 0m ago");
}

#[test]
fn a7_an_agent_made_by_an_agent_has_no_card_of_its_own() {
    let mut w = world();
    let parent = w.agent("parent");
    let child = w.agent_under("child", Some(World::id(&parent)));
    w.asks(&child);
    assert_eq!(w.hand(), "");
}

#[test]
fn a8_a_muted_agent_or_one_whose_host_is_gone_has_nothing() {
    let mut w = world();
    let a = w.agent("a");
    w.asks(&a);
    w.say(&a, Said::Mute);
    assert_eq!(w.hand(), "");
    w.say(&a, Said::Unmute);
    assert_eq!(w.hand(), "a · waiting on reply · 0m");
    w.agents.detach_host(w.host);
    assert_eq!(w.hand(), "");
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
    w.finishes(&a);
    w.snooze(&a, hours(1));
    w.pass(mins(5));
    w.writes_to(&a);
    w.starts(&a);
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
    w.finishes(&a);
    w.snooze(&a, hours(4));
    w.pass(mins(30));
    w.writes_to(&a);
    w.starts(&a);
    w.pass(mins(30));
    w.asks(&a);
    assert_eq!(w.hand(), "a · waiting on reply · 0m");

    let b = w.agent("b");
    w.finishes(&b);
    w.snooze(&b, hours(4));
    w.pass(mins(30));
    w.writes_to(&b);
    w.starts(&b);
    w.pass(mins(61));
    w.ends(&b, TurnOutcome::Completed);
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
    w.marks.apply([crate::marks::body(&node, name)]);
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
fn t4_a_todo_on_an_agent_hides_while_it_works() {
    let mut w = world();
    let a = w.agent("a");
    w.finishes(&a);
    w.todo(&a, None);
    assert_eq!(w.hand(), "a · todo · 0m");
    w.writes_to(&a);
    w.starts(&a);
    assert_eq!(w.hand(), "");
    w.pass(hours(1));
    w.ends(&a, TurnOutcome::Completed);
    assert_eq!(w.deal().cards.len(), 1);
}

#[test]
fn t5_writing_to_an_agent_keeps_its_todo() {
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
    for take_back in [
        Said::Done {
            through: Cursor::Done,
        },
        Said::Mute,
    ] {
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
    w.marks.apply([crate::marks::deleted(&gone, true)]);
    assert_eq!(w.hand(), "");
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
    assert_eq!(w.hand(), "a · waiting on reply · 0m");
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
    Finishes(usize),
    Asks(usize),
    Starts(usize),
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
        (0usize..3).prop_map(Step::Starts),
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
                Step::Finishes(i) => w.finishes(&agents[i]),
                Step::Asks(i) => w.asks(&agents[i]),
                Step::Starts(i) => w.starts(&agents[i]),
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
