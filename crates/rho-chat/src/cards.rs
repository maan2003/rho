//! What the chat claims of the user: the threads with a mention of them
//! they have not seen.
//!
//! Agents and the user talk in threads. A message goes to a thread, not to
//! a person, and draws the user only by mentioning them; a thread is the
//! card, never an agent, and whether an agent is running never ranks; only
//! a stop it did not choose does (A14). A mention of the user names its
//! kind, picked by whether the user needs to read it now:
//!
//! ```text
//! 1. Yes: it asks them for something the work needs: a decision, an
//!    approval, information, or an action, even while the agent keeps
//!    working. An offer of work beyond what they asked is not one. → ask
//! 2. Yes: what they asked for is done, or it answers their question.
//!    Not while part of it is still pending.                       → result
//! 3. No, but it is worth keeping: a finding, an assumption, a report
//!    while follow-through is pending.                             → fyi
//! 4. No, and it is passing: "on it", "86% done".                  → no mention
//! ```
//!
//! The user sees a thread explicitly: a done, or a message of their own in
//! it, sees it through to its newest message. Opening it sees nothing.
//! Done is also how the user says they read a thread, through its newest
//! message or through one they pick: there is no separate read. A card is
//! titled by the thread's name, and its context is its root, the stream.
//!
//! | # | History and facts | Card |
//! |---|---|---|
//! | A1 | An unseen ask | "asks", rises |
//! | A2 | An unseen result | "result", below an ask, fades, gone after ~3 days |
//! | A3 | An unseen fyi | nothing, and it hides no ask or result; the thread is unread on Home |
//! | A4 | A message that mentions nobody, or another agent | nothing, and it hides nothing |
//! | A5 | Several unseen mentions | one card: an ask if any, counting from the first ask unseen, else the newest result |
//! | A6 | Seen | nothing until a newer ask or result |
//! | A7 | The user wrote in the thread within the hour before the mention | a bonus, fading over the hour; a reply within the hour comes through a snooze |
//! | A8 | Done through a message before the mention | the mention still deals |
//! | A9 | A mention in a reply deeper in the thread | the thread's card |
//! | A10 | Another agent's message in the thread, mentioning the thread's agent | context, never unread; the thread deals by mentions of the user |
//! | A11 | An agent made by another agent | deals only by mentioning the user, like any other |
//! | A12 | Its agents running, waiting, idle, finished, archived, or retrying for less than an hour | changes nothing: those are the agent's own choices, or soon over |
//! | A13 | The thread muted, its host gone, or every one of its agents deleted | nothing |
//! | A14 | Its agent failed, retried for an hour or more, or was lost by its host | "failed", "stuck" or "lost", ranked as an ask, counting from when it stopped |
//! | A15 | A stop seen: done after it began, the user writing in the thread, or steering the agent | nothing until a newer stop |
//! | A16 | An unseen ask and an unseen stop | one card, by whichever has waited longer |
//!
//! A stop the agent did not choose says "you, now" as an ask does, without
//! a word: it is the card of the agent's own thread, the first it joined.
//! The agents system says what each agent is doing and since when; a stop
//! is no message, so how far the user has seen one is the instant its host
//! gave it ([`rho_dealer::facts::Seen::Thread`]).
//!
//! Each row is a test here under its name.

use std::collections::BTreeMap;

use jiff::Timestamp;
use rho_agent_types::{
    Address as Party, AgentId, ChatSeq, Doing, EndpointId, MentionKind, Message, MessageId, Stop,
    UnixMs,
};
use rho_dealer::curve::{self, Curve};
use rho_dealer::rank::Trace;
use rho_dealer::{Addressed, Claim, Marks};

use crate::fold::HostChat;
use crate::{AgentRef, ThreadId};

impl Addressed for ThreadId {
    const KIND: &'static str = Self::KIND;

    fn rest(&self) -> String {
        self.key()
    }

    fn from_rest(rest: &str) -> Option<Self> {
        Self::parse(rest)
    }
}

impl Addressed for AgentRef {
    const KIND: &'static str = "agent";

    fn rest(&self) -> String {
        self.0.encoded()
    }

    fn from_rest(rest: &str) -> Option<Self> {
        AgentId::from_encoded(rest).ok().map(Self)
    }
}

/// One host's chat as the dealer reads it.
pub struct Host<'a> {
    pub endpoint: EndpointId,
    pub chat: &'a HostChat,
    /// The host's name, said before a card's context when the window holds
    /// more than one host; `None` otherwise.
    pub name: Option<&'a str>,
    /// Whether an agent still exists on the host.
    pub alive: &'a dyn Fn(AgentId) -> bool,
    /// What an agent is doing and since when, as its host last said;
    /// `None` while it has said nothing.
    pub doing: &'a dyn Fn(AgentId) -> Option<Doing>,
}

/// One agent's stop, and when it began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stopped {
    pub agent: AgentId,
    pub stop: Stop,
    pub since: UnixMs,
}

/// The stops the agents of a host are in now, by the thread each draws
/// the user to: the first each agent joined.
pub fn stops(host: &Host<'_>, now: Timestamp) -> BTreeMap<MessageId, Vec<Stopped>> {
    let now = UnixMs(now.as_millisecond().max(0) as u64);
    let mut stops: BTreeMap<MessageId, Vec<Stopped>> = BTreeMap::new();
    for (agent, joined) in host.chat.agent_threads() {
        let Some(doing) = (host.doing)(agent) else {
            continue;
        };
        let Some(stop) = doing.stop(now) else {
            continue;
        };
        stops
            .entry(host.chat.thread_of(joined))
            .or_default()
            .push(Stopped {
                agent,
                stop,
                since: doing.since,
            });
    }
    stops
}

fn unix(ms: u64) -> Timestamp {
    Timestamp::from_millisecond(ms as i64).expect("a message's time")
}

/// Every thread on the host that wants the user, as a claim each.
pub fn claims(
    host: &Host<'_>,
    marks: &Marks,
    now: Timestamp,
    mut trace: Option<&mut Trace>,
) -> Vec<Claim> {
    let chat = host.chat;
    // One walk over the chat, not one per thread: every thread's mentions
    // of the user in log order, and the newest message the user wrote in
    // it. Ranking happens on every change, and a host's chat is long.
    let mut threads: BTreeMap<MessageId, ThreadMessages<'_>> = BTreeMap::new();
    for message in chat.messages() {
        let mentioned = message.mentions(Party::User).is_some();
        let from_user = message.from == Party::User;
        if !mentioned && !from_user {
            continue;
        }
        let thread = threads.entry(chat.thread_of(message.id)).or_default();
        if mentioned {
            thread.mentions.push(message);
        }
        if from_user
            && thread
                .user_wrote
                .is_none_or(|newest| message.id > newest.id)
        {
            thread.user_wrote = Some(message);
        }
    }
    let mut stopped = stops(host, now);
    for thread in stopped.keys() {
        threads.entry(*thread).or_default();
    }
    let mut claims = Vec::new();
    for (thread, messages) in threads {
        let stops = stopped.remove(&thread).unwrap_or_default();
        // A thread nobody mentioned the user in, and no agent of which
        // stopped, is only the user's own.
        if messages.mentions.is_empty() && stops.is_empty() {
            continue;
        }
        let address = ThreadId {
            host: host.endpoint,
            message: thread,
        }
        .address();
        // A13: nobody is left to answer, and what they said went with them.
        let members = chat.members_under(Some(thread));
        if !members.is_empty() && !members.iter().any(|agent| (host.alive)(*agent)) {
            if let Some(trace) = trace.as_deref_mut() {
                trace.outcome(&address, "no card: its agents are gone".to_owned());
            }
            continue;
        }
        let facts = marks.get(&address).facts();
        // A mention is handled once the user is done with the thread
        // through it, or wrote after it.
        let user_wrote = messages.user_wrote;
        let seen = facts
            .seen_topic()
            .map(|seen| MessageId(ChatSeq(seen)))
            .max(user_wrote.map(|message| message.id));
        // A15: a stop is seen by a done or a steer said after it began,
        // or by the user's words in the thread after it began. Both are on
        // the host's clock.
        let seen_stopped = facts
            .seen_stopped()
            .max(user_wrote.map(|message| message.at.0));
        let mentions = messages.mentions;
        if let Some(trace) = trace.as_deref_mut() {
            trace.input(
                &address,
                "mentions",
                format!("{:?}", mentions.iter().map(|m| m.id).collect::<Vec<_>>()),
            );
            trace.input(&address, "seen through", format!("{seen:?}"));
            if !stops.is_empty() {
                trace.input(&address, "stops", format!("{stops:?}"));
                trace.input(&address, "stops seen through", format!("{seen_stopped:?}"));
            }
        }
        let unseen: Vec<&Message> = mentions
            .into_iter()
            .filter(|message| seen.is_none_or(|seen| message.id > seen))
            .collect();
        // The stop that has waited longest of those not seen.
        let stop = stops
            .iter()
            .filter(|stop| seen_stopped.is_none_or(|seen| stop.since.0 > seen))
            .min_by_key(|stop| stop.since);
        let kind = |message: &&Message| message.mentions(Party::User);
        let ask = unseen
            .iter()
            .find(|message| kind(message) == Some(MentionKind::Ask));
        let result = unseen
            .iter()
            .rfind(|message| kind(message) == Some(MentionKind::Result));
        // An ask waits from the first one unanswered, a stop from when it
        // began, and of the two the one that has waited longer is the
        // card (A16); a result fades from the newest.
        let (since, curve, reason) = match (ask, stop) {
            (Some(ask), Some(stop)) if stop.since.0 < ask.at.0 => waiting_on(stop),
            (Some(ask), _) => {
                let since = unix(ask.at.0);
                (
                    since,
                    Curve::Waiting {
                        head_start: curve::AGENT_BLOCKED_HEAD_START,
                        since,
                    },
                    "asks · {age}".to_owned(),
                )
            }
            (None, Some(stop)) => waiting_on(stop),
            (None, None) => match result {
                Some(result) => {
                    let since = unix(result.at.0);
                    (
                        since,
                        Curve::Fading {
                            head_start: curve::AGENT_FINISHED_HEAD_START,
                            since,
                            gone_days: curve::AGENT_FINISHED_GONE_DAYS,
                        },
                        "result · {age} ago".to_owned(),
                    )
                }
                None => {
                    if let Some(trace) = trace.as_deref_mut() {
                        trace.outcome(&address, "no card: nothing unread".to_owned());
                    }
                    continue;
                }
            },
        };
        // Where the card's source last moved: the newest mention unseen,
        // and the newest stop unseen, which a skip holds on to as well.
        let mut cursor = unseen
            .last()
            .map_or_else(String::new, |newest| newest.id.0.0.to_string());
        if let Some(newest) = stops
            .iter()
            .filter(|stop| seen_stopped.is_none_or(|seen| stop.since.0 > seen))
            .map(|stop| stop.since)
            .max()
        {
            cursor = format!("{cursor}+{}", newest.0);
        }
        let mut claim = Claim::new(address, curve, reason, cursor);
        claim.title = chat.name(thread).unwrap_or_default();
        claim.context = context(host, thread);
        // The room the thread is in: a snooze on it holds the thread too,
        // and Home ranks the room by the thread's card.
        let root = chat.root_of(thread);
        if root != thread {
            claim.under = Some(
                ThreadId {
                    host: host.endpoint,
                    message: root,
                }
                .address(),
            );
        }
        if let Some(wrote) = user_wrote {
            let spoke = unix(wrote.at.0);
            claim.bonus = curve::recency_bonus(spoke, now);
            if since.duration_since(spoke) <= curve::REPLY_BREAKTHROUGH {
                claim.breaks_snooze_set_by = Some(spoke);
            }
        }
        claims.push(claim);
    }
    claims
}

/// A card for a stop: it waits as an ask does, from when the stop began,
/// in the stop's word.
fn waiting_on(stop: &Stopped) -> (Timestamp, Curve, String) {
    let since = unix(stop.since.0);
    (
        since,
        Curve::Waiting {
            head_start: curve::AGENT_BLOCKED_HEAD_START,
            since,
        },
        format!("{} · {{age}}", stop.stop.word()),
    )
}

/// What a thread is called on a card, and where it is, for a thread no
/// claim named: one the user's own dates brought up.
pub fn name(host: &Host<'_>, thread: MessageId) -> Option<(String, String)> {
    let title = host.chat.name(thread)?;
    Some((title, context(host, thread)))
}

/// Where a thread is: its root, and its host when the window holds more
/// than one. A root is in nothing but its host.
fn context(host: &Host<'_>, thread: MessageId) -> String {
    let root = host.chat.root_of(thread);
    let stream = if root == thread {
        String::new()
    } else {
        host.chat.name(root).unwrap_or_default()
    };
    match host.name {
        Some(name) if stream.is_empty() => name.to_owned(),
        Some(name) => format!("{name}/{stream}"),
        None => stream,
    }
}

/// The user's newest message in a thread or under it.
/// What one thread holds that a card is made of, in log order.
#[derive(Default)]
struct ThreadMessages<'a> {
    mentions: Vec<&'a Message>,
    user_wrote: Option<&'a Message>,
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use jiff::{SignedDuration, Zoned};
    use rho_agent_types::{Activity, AgentIdDomain, ChatEvent, ChatRecord, ContentPart, Mention};
    use rho_dealer::curve::*;
    use rho_dealer::facts::{Device, Entry, Fact, Seen};
    use rho_dealer::rank::{Hand, Sources};
    use rho_dealer::{Address, Skips, Until, rank};

    use super::*;

    /// One host's chat, the user's facts and a clock to move.
    struct World {
        chat: HostChat,
        seq: u64,
        made: u64,
        gone: BTreeSet<AgentId>,
        /// The one agent each thread was started with.
        agents: HashMap<MessageId, AgentId>,
        marks: Marks,
        skips: Skips,
        now: Zoned,
        hosts: usize,
        /// What each agent is doing, as its host last said.
        doing: HashMap<AgentId, Doing>,
    }

    fn mins(n: i64) -> SignedDuration {
        SignedDuration::from_mins(n)
    }

    fn hours(n: i64) -> SignedDuration {
        SignedDuration::from_hours(n)
    }

    fn world() -> World {
        World {
            chat: HostChat::default(),
            seq: 0,
            made: 0,
            gone: BTreeSet::new(),
            agents: HashMap::new(),
            marks: Marks::default(),
            skips: Skips::default(),
            now: "2026-08-23T09:00:00Z"
                .parse::<Timestamp>()
                .unwrap()
                .to_zoned(jiff::tz::TimeZone::UTC),
            hosts: 1,
            doing: HashMap::new(),
        }
    }

    impl World {
        fn pass(&mut self, by: SignedDuration) {
            self.now = self.now.checked_add(by).unwrap();
        }

        fn ms(&self) -> u64 {
            self.now.timestamp().as_millisecond() as u64
        }

        fn record(&mut self, event: ChatEvent) -> MessageId {
            self.seq += 1;
            self.chat.tell(&ChatRecord {
                seq: ChatSeq(self.seq),
                at: UnixMs(self.ms()),
                event,
            });
            MessageId(ChatSeq(self.seq))
        }

        fn post(
            &mut self,
            parent: Option<MessageId>,
            from: Party,
            mentions: Vec<Mention>,
            text: &str,
        ) -> MessageId {
            self.record(ChatEvent::Posted {
                parent,
                from,
                mentions,
                content: vec![ContentPart::Text { text: text.into() }],
            })
        }

        fn root(&mut self) -> MessageId {
            match self.chat.roots().first() {
                Some(root) => *root,
                None => self.post(None, Party::User, Vec::new(), "rho"),
            }
        }

        /// A thread the user started under the root with an agent, which
        /// joined it. With a parent, an agent that agent made.
        fn agent_under(&mut self, name: &str, parent: Option<MessageId>) -> MessageId {
            self.made += 1;
            let agent = AgentId::from_counter(self.made, &AgentIdDomain(0)).unwrap();
            let (from, under) = match parent {
                Some(parent) => (Party::Agent(self.agents[&parent]), parent),
                None => (Party::User, self.root()),
            };
            let thread = self.post(Some(under), from, Vec::new(), name);
            self.record(ChatEvent::Joined { thread, agent });
            self.agents.insert(thread, agent);
            thread
        }

        /// A thread started two hours ago: the user's own words that start
        /// it are out of the recency window, so a case measures only what
        /// it does.
        fn agent(&mut self, name: &str) -> MessageId {
            let thread = self.agent_under(name, None);
            self.pass(hours(2));
            thread
        }

        fn address(&self, thread: MessageId) -> Address {
            ThreadId {
                host: EndpointId([0; 32]),
                message: thread,
            }
            .address()
        }

        /// The thread's agent mentions the user, in the thread or under
        /// `reply`.
        fn sends_under(
            &mut self,
            thread: MessageId,
            reply: Option<MessageId>,
            kind: MentionKind,
        ) -> MessageId {
            let from = Party::Agent(self.agents[&thread]);
            self.post(
                Some(reply.unwrap_or(thread)),
                from,
                vec![Mention {
                    to: Party::User,
                    kind,
                }],
                "update",
            )
        }

        fn sends(&mut self, thread: MessageId, kind: MentionKind) -> MessageId {
            self.sends_under(thread, None, kind)
        }

        fn finishes(&mut self, thread: MessageId) -> MessageId {
            self.sends(thread, MentionKind::Result)
        }

        fn asks(&mut self, thread: MessageId) -> MessageId {
            self.sends(thread, MentionKind::Ask)
        }

        /// Says what it is doing, mentioning nobody.
        fn works(&mut self, thread: MessageId) {
            let from = Party::Agent(self.agents[&thread]);
            self.post(Some(thread), from, Vec::new(), "working");
        }

        fn writes_to(&mut self, thread: MessageId) {
            self.post(Some(thread), Party::User, Vec::new(), "go on");
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

        fn newest(&self, thread: MessageId) -> Seen {
            Seen::Topic(self.chat.newest_in(thread).map_or(0, |(id, _)| id.0.0))
        }

        fn done(&mut self, thread: MessageId) {
            let seen = self.seen_now(thread);
            self.done_through(thread, seen);
        }

        fn done_through(&mut self, thread: MessageId, seen: Seen) {
            self.tell(Fact::Settled {
                node: self.address(thread),
                seen,
            });
        }

        /// What an older build wrote for "marked read".
        fn reads(&mut self, thread: MessageId) {
            let seen = self.newest(thread);
            self.tell(Fact::Read {
                node: self.address(thread),
                seen,
            });
        }

        fn snooze(&mut self, thread: MessageId, until: SignedDuration) {
            self.tell(Fact::Snooze {
                node: self.address(thread),
                until: Until::In(until),
            });
        }

        fn mute(&mut self, thread: MessageId) {
            self.tell(Fact::Mute {
                node: self.address(thread),
            });
        }

        /// The thread's agent starts doing `activity` now.
        fn turns(&mut self, thread: MessageId, activity: Activity) {
            let since = UnixMs(self.ms());
            self.doing
                .insert(self.agents[&thread], Doing { activity, since });
        }

        /// What a done on the thread sees: through its newest message, and
        /// through the stops its agents are in.
        fn seen_now(&self, thread: MessageId) -> Seen {
            let alive = |_: AgentId| true;
            let doing = |agent: AgentId| self.doing.get(&agent).copied();
            let host = Host {
                endpoint: EndpointId([0; 32]),
                chat: &self.chat,
                name: None,
                alive: &alive,
                doing: &doing,
            };
            let newest = self.chat.newest_in(thread).map_or(0, |(id, _)| id.0.0);
            match stops(&host, self.now.timestamp())
                .remove(&thread)
                .and_then(|stops| stops.iter().map(|stop| stop.since.0).max())
            {
                Some(stopped) => Seen::Thread {
                    message: newest,
                    stopped,
                },
                None => Seen::Topic(newest),
            }
        }

        /// The user steers the thread's agent: the stop it is in is seen.
        fn steers(&mut self, thread: MessageId) {
            let doing = self.doing[&self.agents[&thread]];
            self.tell(Fact::Read {
                node: self.address(thread),
                seen: Seen::Thread {
                    message: 0,
                    stopped: doing.since.0,
                },
            });
        }

        fn deal(&self) -> Hand {
            let alive = |agent: AgentId| !self.gone.contains(&agent);
            let doing = |agent: AgentId| self.doing.get(&agent).copied();
            let host = Host {
                endpoint: EndpointId([0; 32]),
                chat: &self.chat,
                name: (self.hosts > 1).then_some("laptop"),
                alive: &alive,
                doing: &doing,
            };
            let claims = claims(&host, &self.marks, self.now.timestamp(), None);
            let names = |address: &Address| {
                let id = ThreadId::from_address(address)?;
                name(&host, id.message)
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

        /// The hand as lines of `heading · label`, top first.
        fn hand(&self) -> String {
            self.deal()
                .cards
                .iter()
                .map(|card| format!("{} · {}", card.heading(), card.label))
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn priority(&self, thread: MessageId) -> Option<f64> {
            let address = self.address(thread);
            self.deal()
                .cards
                .into_iter()
                .find(|card| card.address == address)
                .map(|card| card.priority)
        }
    }

    /// A card is a thread's: a mention in the root message itself is the
    /// root's card, and a mention in a thread under it is that thread's,
    /// never counted twice.
    #[test]
    fn a_mention_in_the_root_itself_is_the_roots_card_alone() {
        let mut w = world();
        let agent = AgentId::from_counter(99, &AgentIdDomain(0)).unwrap();
        let root = w.post(
            None,
            Party::Agent(agent),
            vec![Mention {
                to: Party::User,
                kind: MentionKind::Ask,
            }],
            "root asks",
        );
        let a = w.agent("a");
        let ask = w.asks(a);
        let cards = w.deal().cards;
        let cursor = |thread: MessageId| {
            cards
                .iter()
                .find(|card| card.address == w.address(thread))
                .map(|card| card.cursor.clone())
        };
        assert_eq!(cursor(root).as_deref(), Some(root.0.0.to_string().as_str()));
        assert_eq!(cursor(a).as_deref(), Some(ask.0.0.to_string().as_str()));
    }

    #[test]
    fn a1_an_unread_ask_rises() {
        let mut w = world();
        let a = w.agent("a");
        w.asks(a);
        assert_eq!(w.hand(), "rho › a · asks · 0m");
        let first = w.priority(a).unwrap();
        assert!(first >= AGENT_BLOCKED_HEAD_START);
        w.pass(hours(1));
        assert!(w.priority(a).unwrap() > first);
    }

    #[test]
    fn a2_a_result_is_below_an_ask_and_gone_in_three_days() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.finishes(a);
        w.asks(b);
        assert_eq!(w.hand(), "rho › b · asks · 0m\nrho › a · result · 0m ago");
        assert!(w.priority(a).unwrap() < LAMP_THRESHOLD);
        w.pass(hours(71));
        assert!(w.hand().ends_with("a · result · 3.0d ago"), "{}", w.hand());
        w.pass(hours(1));
        assert_eq!(w.hand(), "rho › b · asks · 3.0d");
    }

    #[test]
    fn a3_an_fyi_is_no_card() {
        let mut w = world();
        let a = w.agent("a");
        w.sends(a, MentionKind::Fyi);
        assert_eq!(w.hand(), "");
        w.asks(a);
        w.sends(a, MentionKind::Fyi);
        assert_eq!(w.hand(), "rho › a · asks · 0m", "an fyi hides no ask");
    }

    #[test]
    fn a4_a_message_that_mentions_nobody_or_another_agent_is_no_card() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.works(a);
        let from = Party::Agent(w.agents[&a]);
        w.post(
            Some(a),
            from,
            vec![Mention {
                to: Party::Agent(w.agents[&b]),
                kind: MentionKind::Ask,
            }],
            "b, help",
        );
        assert_eq!(w.hand(), "");
        w.finishes(a);
        w.works(a);
        assert_eq!(
            w.hand(),
            "rho › a · result · 0m ago",
            "and it hides nothing"
        );
    }

    #[test]
    fn a5_several_unseen_mentions_are_one_card_an_ask_above_a_result() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.finishes(a);
        w.pass(mins(10));
        w.asks(a);
        w.pass(mins(10));
        w.finishes(a);
        w.pass(mins(10));
        w.asks(a);
        assert_eq!(
            w.hand(),
            "rho › a · asks · 20m",
            "from the first ask unseen"
        );
        w.finishes(b);
        w.pass(mins(30));
        w.finishes(b);
        assert_eq!(
            w.hand(),
            "rho › a · asks · 50m\nrho › b · result · 0m ago",
            "from the newest result"
        );
    }

    #[test]
    fn a6_done_or_writing_holds_until_a_newer_mention() {
        let mut w = world();
        let a = w.agent("a");
        w.asks(a);
        w.done(a);
        assert_eq!(w.hand(), "");
        w.works(a);
        assert_eq!(w.hand(), "");
        w.asks(a);
        assert_eq!(w.hand(), "rho › a · asks · 0m");
        w.writes_to(a);
        assert_eq!(w.hand(), "", "the user's own words see the thread");
        w.finishes(a);
        assert_eq!(w.hand(), "rho › a · result · 0m ago");
    }

    #[test]
    fn a7_a_thread_the_user_just_wrote_in_comes_back_on_top_and_chimes() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.asks(b);
        w.writes_to(a);
        w.pass(mins(5));
        w.finishes(a);
        assert!(w.hand().starts_with("rho › a ·"), "{}", w.hand());
        assert!(w.priority(a).unwrap() >= CHIME_THRESHOLD);
        // Through a snooze said before the user wrote.
        w.snooze(a, hours(4));
        w.pass(mins(30));
        w.writes_to(a);
        w.pass(mins(30));
        w.asks(a);
        assert!(
            w.priority(a).is_some(),
            "a reply within the hour comes through"
        );
        w.snooze(b, hours(4));
        w.writes_to(b);
        w.pass(mins(61));
        w.asks(b);
        assert!(
            w.priority(b).is_none(),
            "an hour and more is not a conversation"
        );
    }

    #[test]
    fn a8_done_through_a_message_leaves_the_mentions_after_it() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.finishes(a);
        let result = w.newest(a);
        w.pass(mins(1));
        w.asks(a);
        w.done_through(a, result);
        assert_eq!(
            w.hand(),
            "rho › a · asks · 0m",
            "the ask came after the done"
        );
        w.done(a);
        assert_eq!(w.hand(), "");
        // An older build's "marked read" is a done that takes nothing back:
        // it handles an ask as well as a result.
        w.asks(b);
        w.reads(b);
        assert_eq!(w.hand(), "");
    }

    #[test]
    fn a9_a_mention_deeper_in_the_thread_is_the_threads_card() {
        let mut w = world();
        let a = w.agent("a");
        let reply = w.sends(a, MentionKind::Fyi);
        w.sends_under(a, Some(reply), MentionKind::Ask);
        assert_eq!(w.hand(), "rho › a · asks · 0m");
        let hand = w.deal();
        assert_eq!(hand.cards[0].address, w.address(a));
        w.done(a);
        assert_eq!(
            w.hand(),
            "",
            "done with the thread is done with what is under it"
        );
    }

    #[test]
    fn a10_another_agents_message_is_context_never_unread() {
        let mut w = world();
        let parent = w.agent("parent");
        let child = w.agent_under("child", Some(parent));
        // The child reports to its parent in the parent's thread.
        let from = Party::Agent(w.agents[&child]);
        w.post(
            Some(parent),
            from,
            vec![Mention {
                to: Party::Agent(w.agents[&parent]),
                kind: MentionKind::Result,
            }],
            "done, parent",
        );
        assert_eq!(w.hand(), "");
        w.asks(parent);
        assert_eq!(w.hand(), "rho › parent · asks · 0m");
    }

    #[test]
    fn a11_an_agent_made_by_an_agent_deals_only_by_mentioning_the_user() {
        let mut w = world();
        let parent = w.agent("parent");
        let child = w.agent_under("child", Some(parent));
        w.works(child);
        assert_eq!(w.hand(), "");
        w.asks(child);
        // The child works in a subthread of its parent's thread, which is
        // what the user opens: the ask is that thread's card.
        assert_eq!(w.hand(), "rho › parent · asks · 0m");
    }

    #[test]
    fn a12_what_its_agents_choose_to_do_changes_nothing() {
        // What a running agent says without a mention is A4.
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.asks(a);
        let before = w.deal();
        for activity in [
            Activity::Working,
            Activity::Waiting,
            Activity::Idle,
            Activity::Archived,
        ] {
            w.turns(a, activity);
            w.turns(b, activity);
            w.works(a);
            assert_eq!(w.hand(), "rho › a · asks · 0m", "{activity:?}");
        }
        assert_eq!(w.deal().cards, before.cards);
        // Retrying is not a stop until it has gone on for an hour.
        w.turns(b, Activity::Retrying);
        w.pass(mins(59));
        assert_eq!(w.hand(), "rho › a · asks · 59m");
    }

    #[test]
    fn a14_a_stop_the_agent_did_not_choose_is_its_threads_card() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        let c = w.agent("c");
        w.turns(a, Activity::Failed);
        w.pass(mins(30));
        w.turns(c, Activity::Lost);
        w.turns(b, Activity::Retrying);
        w.pass(hours(1));
        assert_eq!(
            w.hand(),
            "rho › a · failed · 1.5h\nrho › b · stuck · 1.0h\nrho › c · lost · 1.0h",
            "each counts from when it stopped, and ranks as an ask does"
        );
        // A stop is ranked with the asks: an ask as old is level with it.
        let d = w.agent("d");
        w.asks(d);
        w.turns(a, Activity::Failed);
        assert_eq!(w.priority(a), w.priority(d));
        // Once the agent moves on, the stop is gone.
        w.turns(b, Activity::Working);
        assert!(w.priority(b).is_none());
    }

    #[test]
    fn a14_the_card_is_the_first_thread_the_agent_joined() {
        let mut w = world();
        let a = w.agent("a");
        let elsewhere = w.agent("elsewhere");
        let agent = w.agents[&a];
        w.record(ChatEvent::Joined {
            thread: elsewhere,
            agent,
        });
        w.turns(a, Activity::Failed);
        assert_eq!(w.hand(), "rho › a · failed · 0m");
    }

    #[test]
    fn a15_done_writing_or_steering_sees_the_stop_until_a_newer_one() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        let c = w.agent("c");
        for thread in [a, b, c] {
            w.turns(thread, Activity::Failed);
        }
        w.pass(mins(5));
        w.done(a);
        w.writes_to(b);
        w.steers(c);
        assert_eq!(w.hand(), "");
        // Failing again is a stop the user has not seen.
        w.pass(mins(5));
        for thread in [a, b, c] {
            w.turns(thread, Activity::Working);
            w.turns(thread, Activity::Failed);
        }
        // b is on top: the user wrote in it minutes ago (A7).
        assert_eq!(
            w.hand(),
            "rho › b · failed · 0m\nrho › a · failed · 0m\nrho › c · failed · 0m"
        );
        // A done through an older message, before the stop, sees nothing
        // of it.
        let old = w.newest(a);
        w.done_through(a, old);
        assert!(w.priority(a).is_some());
    }

    #[test]
    fn a15_steering_leaves_an_ask_after_what_it_saw() {
        let mut w = world();
        let a = w.agent("a");
        w.turns(a, Activity::Failed);
        w.pass(mins(5));
        w.asks(a);
        w.steers(a);
        assert_eq!(w.hand(), "rho › a · asks · 0m", "the ask still waits");
        w.mute(a);
        assert_eq!(w.hand(), "");
    }

    #[test]
    fn a16_an_ask_and_a_stop_are_one_card_by_whichever_waited_longer() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.asks(a);
        w.turns(b, Activity::Failed);
        w.pass(mins(10));
        w.turns(a, Activity::Failed);
        w.asks(b);
        assert_eq!(w.hand(), "rho › a · asks · 10m\nrho › b · failed · 10m",);
        // A done sees both.
        w.done(b);
        assert_eq!(w.hand(), "rho › a · asks · 10m");
    }

    #[test]
    fn a13_a_gone_agents_stop_is_no_card() {
        let mut w = world();
        let a = w.agent("a");
        w.turns(a, Activity::Failed);
        w.gone.insert(w.agents[&a]);
        assert_eq!(w.hand(), "");
    }

    #[test]
    fn a13_a_muted_thread_or_one_whose_agents_are_gone_has_nothing() {
        let mut w = world();
        let a = w.agent("a");
        w.asks(a);
        w.mute(a);
        assert_eq!(w.hand(), "");
        let b = w.agent("b");
        w.asks(b);
        assert_eq!(w.hand(), "rho › b · asks · 0m");
        w.gone.insert(w.agents[&b]);
        assert_eq!(w.hand(), "", "nobody is left to answer");
    }

    #[test]
    fn a_card_says_its_host_when_there_is_more_than_one() {
        let mut w = world();
        let a = w.agent("a");
        w.asks(a);
        w.hosts = 2;
        assert_eq!(w.hand(), "laptop/rho › a · asks · 0m");
        // A message right under the root is a thread of its own.
        let root = w.root();
        let from = Party::Agent(w.agents[&a]);
        w.post(
            Some(root),
            from,
            vec![Mention {
                to: Party::User,
                kind: MentionKind::Ask,
            }],
            "at the root",
        );
        let hand = w.deal();
        assert!(
            hand.cards
                .iter()
                .any(|card| card.heading() == "laptop/rho › at the root"),
            "{hand:?}"
        );
    }

    #[test]
    fn a_thread_with_only_the_users_dates_is_named_by_the_chat() {
        let mut w = world();
        let a = w.agent("a");
        w.tell(Fact::Deadline {
            node: w.address(a),
            by: Until::In(SignedDuration::from_hours(1)),
            lead_days: 1,
        });
        assert_eq!(w.hand(), "rho › a · deadline · 1d");
    }
}
