//! Deal the first waiting mail after the user's reply. When there is no
//! such mail, the oldest unanswered pinging mail still waits.
//! A visible mail is the only source of a claim; a stop remains after its
//! agent moves on, and no addressed result fades unread. Marks still belong
//! to the user, and a reply settles the mail that preceded it.

use std::collections::{BTreeMap, HashSet};

use jiff::Timestamp;
use rho_agent_types::{Address as Party, AgentId, ChatSeq, EndpointId, Message, MessageId};
use rho_dealer::curve::{self, Curve};
use rho_dealer::facts::{Facts, Seen};
use rho_dealer::rank::{CardKind, Trace};
use rho_dealer::{Addressed, Claim, Marks};

use crate::fold::HostMail;
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
    pub chat: &'a HostMail,
    /// The device's name for the host, used in the project address.
    pub name: &'a str,
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
    local_answers: impl IntoIterator<Item = MessageId>,
) -> Vec<Claim> {
    let chat = host.chat;
    // One walk over the chat, not one per thread: every thread's mentions
    // of the user in log order, and the newest message the user wrote in
    // it. Ranking happens on every change, and a host's chat is long.
    let mut threads: BTreeMap<MessageId, ThreadMessages<'_>> = BTreeMap::new();
    let mut answered: HashSet<_> = local_answers.into_iter().collect();
    for message in chat.messages() {
        let mentioned = message.pings_user();
        let from_user = message.from == Party::User;
        if !mentioned && !from_user {
            continue;
        }
        let thread = threads.entry(chat.subthread_of(message.id)).or_default();
        if mentioned {
            thread.mentions.push(message);
        }
        if from_user {
            thread.user_wrote = Some(message);
            if let Some(mail) = message.reply_to {
                answered.insert(mail);
            } else {
                thread.plain_user_mail = Some(message.id);
            }
        }
    }
    let mut claims = Vec::new();
    for (thread, messages) in threads {
        let address = ThreadId {
            host: host.endpoint,
            message: thread,
        }
        .address();
        let facts = marks.get(&address).facts();
        let user_wrote = messages.user_wrote;
        let seen = SeenMail::new(facts, messages.plain_user_mail.map(|id| id.0));
        let mut unseen = messages
            .mentions
            .into_iter()
            .filter(|message| !answered.contains(&message.id) && seen.is_unread(message))
            .collect::<Vec<_>>();
        let waiting_since = |message: &Message| message.ping_since(seen.through(message.id));
        unseen.sort_by_key(|message| waiting_since(message).0);
        if let Some(trace) = trace.as_deref_mut() {
            trace.input(
                &address,
                "unseen mentions",
                format!(
                    "{:?}",
                    unseen.iter().map(|mail| mail.id).collect::<Vec<_>>()
                ),
            );
            trace.input(&address, "read", format!("{seen:?}"));
        }

        // An addressed reply settles only its target. Older unanswered mail
        // still waits, but the next mail in the conversation is where to land.
        let next = user_wrote
            .filter(|reply| reply.id != thread)
            .and_then(|reply| {
                unseen
                    .iter()
                    .copied()
                    .find(|mail| mail.version().0 > reply.id.0)
            });
        let Some(mail) = next.or_else(|| unseen.first().copied()) else {
            if let Some(trace) = trace.as_deref_mut() {
                trace.outcome(&address, "no card: nothing unread".to_owned());
            }
            continue;
        };
        let reason = match mail.from {
            Party::System { stop, .. } => CardKind::Stop(stop.word()),
            Party::StartFailed => CardKind::Stop("failed"),
            _ => CardKind::Waits,
        };
        let since = unix(
            match mail.from {
                Party::System {
                    stop: rho_agent_types::Stop::Stuck,
                    since,
                    ..
                } => since,
                _ => waiting_since(mail).1,
            }
            .0,
        );
        let curve = Curve::Waiting {
            head_start: curve::AGENT_BLOCKED_HEAD_START,
            since,
        };
        // The cursor locates the trigger and notices new pings, not edits
        // of a ping the user hasn't settled yet.
        let newest = unseen
            .iter()
            .map(|mail| waiting_since(mail).0)
            .max()
            .expect("the trigger is unseen");
        let cursor = format!("{}/{}", mail.id.0.0, newest.0);
        let mut claim = Claim::new(address, curve, reason, cursor);
        claim.seen = Seen::Mail {
            message: mail.id.0.0,
            version: mail.version().0.0,
        };
        claim.title = chat.name(thread).unwrap_or_default();
        claim.context = context(host.chat, thread, host.name);
        // A snooze or mute on the project holds its threads too.
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

/// What a thread is called on a card, and where it is, for a thread no
/// claim named: one the user's own dates brought up.
pub fn name(host: &Host<'_>, thread: MessageId) -> Option<(String, String)> {
    let title = host.chat.name(thread)?;
    Some((title, context(host.chat, thread, host.name)))
}

/// Where a thread is: the address of the project above it.
pub(crate) fn context(chat: &HostMail, thread: MessageId, host_name: &str) -> String {
    context_at_root(chat, chat.root_of(thread), host_name)
}

pub(crate) fn context_at_root(chat: &HostMail, root: MessageId, host_name: &str) -> String {
    chat.project(root).map_or_else(
        || host_name.to_owned(),
        |project| project.address(host_name),
    )
}

/// The mail a project card opens on.
pub fn attention_mail(seen: Seen) -> Option<MessageId> {
    match seen {
        Seen::Mail { message, .. } => Some(MessageId(ChatSeq(message))),
        _ => None,
    }
}

/// How much of a thread is read: whole-thread cutoffs (including from old
/// devices) and the latest verdict on each specific mail.
#[derive(Debug)]
pub(crate) struct SeenMail {
    through: Option<ChatSeq>,
    exact: BTreeMap<MessageId, ChatSeq>,
}

impl SeenMail {
    pub(crate) fn new(facts: Facts<'_>, wrote: Option<ChatSeq>) -> Self {
        let mut exact = BTreeMap::new();
        for (message, version) in facts.seen_mail() {
            let latest = exact
                .entry(MessageId(ChatSeq(message)))
                .or_insert(ChatSeq(version));
            *latest = (*latest).max(ChatSeq(version));
        }
        Self {
            through: facts.seen_topic().map(ChatSeq).max(wrote),
            exact,
        }
    }

    fn through(&self, mail: MessageId) -> Option<ChatSeq> {
        self.through.max(self.exact.get(&mail).copied())
    }

    pub(crate) fn is_unread(&self, message: &Message) -> bool {
        self.through(message.id)
            .is_none_or(|seen| message.version().0 > seen)
    }
}

#[derive(Default)]
struct ThreadMessages<'a> {
    mentions: Vec<&'a Message>,
    user_wrote: Option<&'a Message>,
    plain_user_mail: Option<MessageId>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use jiff::{SignedDuration, Zoned};
    use rho_agent_types::{
        AgentIdDomain, ChatEvent, ChatRecord, ContentPart, Mention, Stop, UnixMs,
    };
    use rho_dealer::curve::*;
    use rho_dealer::facts::{Device, Entry, Fact};
    use rho_dealer::rank::{Hand, Sources};
    use rho_dealer::{Address, Skips, Until, rank};

    use super::*;

    /// One host's chat, the user's facts and a clock to move.
    struct World {
        chat: HostMail,
        seq: u64,
        made: u64,
        /// The one agent each thread was started with.
        agents: HashMap<MessageId, AgentId>,
        marks: Marks,
        skips: Skips,
        now: Zoned,
    }

    fn mins(n: i64) -> SignedDuration {
        SignedDuration::from_mins(n)
    }

    fn hours(n: i64) -> SignedDuration {
        SignedDuration::from_hours(n)
    }

    fn world() -> World {
        World {
            chat: HostMail::default(),
            seq: 0,
            made: 0,
            agents: HashMap::new(),
            marks: Marks::default(),
            skips: Skips::default(),
            now: "2026-11-23T09:00:00Z"
                .parse::<Timestamp>()
                .unwrap()
                .to_zoned(jiff::tz::TimeZone::UTC),
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
                kind: rho_agent_types::MessageKind::Kept,
                parent,
                reply_to: None,
                from,
                mentions,
                content: vec![ContentPart::Text { text: text.into() }],
            })
        }

        fn root(&mut self) -> MessageId {
            match self
                .chat
                .roots()
                .iter()
                .find(|id| self.chat.is_project(**id))
            {
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
            ping: bool,
        ) -> MessageId {
            let from = Party::Agent(self.agents[&thread]);
            self.post(
                Some(reply.unwrap_or(thread)),
                from,
                vec![Mention::User { ping }],
                "update",
            )
        }

        fn sends(&mut self, thread: MessageId, ping: bool) -> MessageId {
            self.sends_under(thread, None, ping)
        }

        fn finishes(&mut self, thread: MessageId) -> MessageId {
            self.sends(thread, true)
        }

        fn asks(&mut self, thread: MessageId) -> MessageId {
            self.sends(thread, true)
        }

        /// Says what it is doing, mentioning nobody.
        fn works(&mut self, thread: MessageId) {
            let from = Party::Agent(self.agents[&thread]);
            self.post(Some(thread), from, Vec::new(), "working");
        }

        fn writes_to(&mut self, thread: MessageId) {
            self.post(Some(thread), Party::User, Vec::new(), "go on");
        }

        fn answers(&mut self, thread: MessageId, mail: MessageId) {
            self.record(ChatEvent::Posted {
                kind: rho_agent_types::MessageKind::Kept,
                parent: Some(thread),
                reply_to: Some(mail),
                from: Party::User,
                mentions: Vec::new(),
                content: vec![ContentPart::Text {
                    text: "answer".into(),
                }],
            });
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
            Seen::Topic(self.chat.newest_in(thread).map_or(0, |(seq, _)| seq.0))
        }

        fn done(&mut self, thread: MessageId) {
            let seen = self
                .deal()
                .cards
                .iter()
                .find(|card| card.address == self.address(thread))
                .map_or(Seen::Whole, |card| card.seen);
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

        fn stop(&mut self, thread: MessageId, stop: Stop) -> MessageId {
            self.post(
                Some(thread),
                Party::System {
                    agent: self.agents[&thread],
                    stop,
                    since: UnixMs(self.ms()),
                },
                vec![Mention::User { ping: true }],
                "lost connection",
            )
        }

        fn deal(&self) -> Hand {
            let host = Host {
                endpoint: EndpointId([0; 32]),
                chat: &self.chat,
                name: "lab",
            };
            let claims = claims(&host, &self.marks, self.now.timestamp(), None, []);
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
            vec![Mention::User { ping: true }],
            "root asks",
        );
        let a = w.agent("a");
        let ask = w.asks(a);
        let cards = w.deal().cards;
        let trigger = |thread: MessageId| {
            cards
                .iter()
                .find(|card| card.address == w.address(thread))
                .and_then(|card| attention_mail(card.seen))
        };
        assert_eq!(trigger(root), Some(root));
        assert_eq!(trigger(a), Some(ask));
        w.writes_to(root);
        assert_eq!(
            w.hand(),
            "rho@lab › a · waits · now",
            "the reply settles the standalone ask without clearing an unrelated thread"
        );
    }

    #[test]
    fn an_addressed_reply_does_not_settle_other_mail() {
        let mut w = world();
        let a = w.agent("a");
        let first = w.asks(a);
        let second = w.asks(a);
        let result = w.finishes(a);
        let trigger = |w: &World| {
            w.deal()
                .cards
                .first()
                .and_then(|card| attention_mail(card.seen))
        };
        assert_eq!(trigger(&w), Some(first));
        w.answers(a, second);
        assert_eq!(trigger(&w), Some(first));
        w.answers(a, first);
        assert_eq!(trigger(&w), Some(result));
        w.writes_to(a);
        assert_eq!(trigger(&w), None);
    }

    #[test]
    fn a1_an_unread_ask_rises() {
        let mut w = world();
        let a = w.agent("a");
        w.asks(a);
        assert_eq!(w.hand(), "rho@lab › a · waits · now");
        let first = w.priority(a).unwrap();
        assert!(first >= AGENT_BLOCKED_HEAD_START);
        w.pass(hours(1));
        assert!(w.priority(a).unwrap() > first);
    }

    #[test]
    fn a2_a_result_waits_at_the_same_rate_as_an_ask_and_does_not_fade() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        let result = w.finishes(a);
        w.asks(b);
        assert_eq!(w.priority(a), w.priority(b));
        w.pass(hours(144));
        assert_eq!(w.priority(a), w.priority(b));
        let hand = w.deal();
        let card = hand
            .cards
            .iter()
            .find(|card| card.address == w.address(a))
            .unwrap();
        assert_eq!(card.label, "waits · 6.0d");
        assert_eq!(attention_mail(card.seen), Some(result));
        w.writes_to(a);
        assert_eq!(w.hand(), "rho@lab › b · waits · 6.0d");
    }

    #[test]
    fn a3_an_fyi_is_no_card() {
        let mut w = world();
        let a = w.agent("a");
        w.sends(a, false);
        assert_eq!(w.hand(), "");
        w.asks(a);
        w.sends(a, false);
        assert_eq!(w.hand(), "rho@lab › a · waits · now", "an fyi hides no ask");
    }

    #[test]
    fn a4_a_message_that_mentions_nobody_or_another_agent_is_no_card() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.works(a);
        let from = Party::Agent(w.agents[&a]);
        w.post(Some(a), from, vec![Mention::Agent(w.agents[&b])], "b, help");
        assert_eq!(w.hand(), "");
        w.finishes(a);
        w.works(a);
        assert_eq!(
            w.hand(),
            "rho@lab › a · waits · now",
            "and it hides nothing"
        );
    }

    #[test]
    fn pinging_mail_and_stops_deal_oldest_first() {
        let mut w = world();
        let old_result = w.agent("later-alphabetically");
        let new_result = w.agent("earlier-alphabetically");
        let old_ask = w.agent("old ask");
        let stop = w.agent("stop");
        let new_ask = w.agent("new ask");
        w.finishes(old_result);
        w.pass(mins(10));
        w.finishes(new_result);
        w.pass(mins(10));
        w.asks(old_ask);
        w.pass(mins(15));
        w.stop(stop, Stop::Failed);
        w.pass(mins(12));
        w.writes_to(new_ask);
        w.asks(new_ask);
        let hand = w.deal();
        assert_eq!(
            hand.cards
                .iter()
                .map(|card| card.address.clone())
                .collect::<Vec<_>>(),
            [old_result, new_result, old_ask, stop, new_ask].map(|thread| w.address(thread)),
        );
        assert!(w.priority(new_ask).unwrap() > w.priority(old_ask).unwrap());
    }

    #[test]
    fn a5_several_pings_are_one_card_anchored_at_the_oldest() {
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
            "rho@lab › a · waits · 30m",
            "from the first unseen ping"
        );
        w.finishes(b);
        w.pass(mins(30));
        w.finishes(b);
        assert_eq!(
            w.hand(),
            "rho@lab › a · waits · 1.0h\nrho@lab › b · waits · 30m",
            "both threads deal from their first ping"
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
        assert_eq!(w.hand(), "rho@lab › a · waits · now");
        w.writes_to(a);
        assert_eq!(w.hand(), "", "the user's own words see the thread");
        w.finishes(a);
        assert_eq!(w.hand(), "rho@lab › a · waits · now");
    }

    #[test]
    fn editing_while_it_waits_preserves_the_card_but_toggling_ping_starts_a_new_wait() {
        let mut w = world();
        let a = w.agent("a");
        let ask = w.asks(a);
        let original = w.deal().cards[0].cursor.clone();
        w.pass(mins(7));
        for ping in [None, Some(false), Some(true)] {
            w.record(ChatEvent::Revised {
                message: ask,
                author: w.agents[&a],
                ping,
                content: vec![ContentPart::Text {
                    text: "Clock clarification.".into(),
                }],
            });
            let hand = w.deal();
            match ping {
                None => {
                    assert_eq!(hand.cards[0].cursor, original);
                    assert_eq!(hand.cards[0].label, "waits · 7m");
                }
                Some(false) => assert!(hand.cards.is_empty()),
                Some(true) => {
                    assert_ne!(hand.cards[0].cursor, original);
                    assert_eq!(hand.cards[0].label, "waits · now");
                    assert_eq!(attention_mail(hand.cards[0].seen), Some(ask));
                }
            }
        }
    }

    #[test]
    fn new_words_after_done_redeal_the_same_mail() {
        let mut w = world();
        let a = w.agent("a");
        let older = w.finishes(a);
        w.done(a);
        let ask = w.asks(a);
        w.done(a);
        w.pass(mins(7));
        assert_eq!(w.hand(), "");
        w.record(ChatEvent::Revised {
            message: ask,
            author: w.agents[&a],
            ping: None,
            content: vec![ContentPart::Text {
                text: "I need a monotonic clock.".into(),
            }],
        });
        let hand = w.deal();
        let [card] = hand.cards.as_slice() else {
            panic!("{:?}", hand.cards)
        };
        assert_eq!(attention_mail(card.seen), Some(ask));
        assert_ne!(attention_mail(card.seen), Some(older));
        assert_eq!(card.label, "waits · now", "age is the revision's");
        w.done(a);
        assert_eq!(w.hand(), "");
        w.answers(a, ask);
        assert_eq!(w.hand(), "");
    }

    #[test]
    fn taken_back_words_and_recovered_stops_stay_readable_without_cards() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        let ask = w.asks(a);
        let stop = w.stop(b, Stop::Stuck);
        assert_eq!(w.deal().cards.len(), 2);
        w.record(ChatEvent::TakenBack {
            message: ask,
            version: ask.0,
            author: w.agents[&a],
        });
        assert_eq!(w.hand(), "rho@lab › b · stuck · now");
        w.record(ChatEvent::Recovered {
            message: stop,
            content: vec![ContentPart::Text {
                text: "Work carried on.".into(),
            }],
        });
        assert_eq!(w.hand(), "");
        assert!(!w.chat.message(ask).unwrap().text().is_empty());
        assert_eq!(w.chat.message(stop).unwrap().text(), "Work carried on.");
    }

    #[test]
    fn refused_start_is_a_stop_not_an_ask() {
        let mut w = world();
        let thread = w.agent("a");
        let failure = w.post(
            Some(thread),
            Party::StartFailed,
            vec![Mention::User { ping: true }],
            "Could not start.",
        );
        let hand = w.deal();
        let [card] = hand.cards.as_slice() else {
            panic!("{:?}", hand.cards)
        };
        assert_eq!(card.kind, CardKind::Stop("failed"));
        assert_eq!(card.label, "failed · now");
        assert_eq!(attention_mail(card.seen), Some(failure));
    }

    #[test]
    fn a7_a_recent_reply_raises_chime_pressure_without_jumping_the_hand() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.asks(b);
        w.writes_to(a);
        w.pass(mins(5));
        w.finishes(a);
        assert_eq!(
            w.hand(),
            "rho@lab › b · waits · 5m\nrho@lab › a · waits · now"
        );
        assert!(w.priority(a).unwrap() >= CHIME_THRESHOLD);
        assert!(w.priority(b).unwrap() < CHIME_THRESHOLD);
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
            "rho@lab › a · waits · now",
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
    fn a9_a_mention_on_a_tangent_is_the_tangent_threads_card() {
        let mut w = world();
        let a = w.agent("a");
        let reply = w.sends(a, false);
        let trigger = w.sends_under(a, Some(reply), true);
        let hand = w.deal();
        assert_eq!(hand.cards[0].address, w.address(reply));
        assert_eq!(attention_mail(hand.cards[0].seen), Some(trigger));
        w.done(a);
        assert_eq!(w.deal().cards.len(), 1);
        w.done(reply);
        assert!(w.deal().cards.is_empty());
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
            vec![Mention::Agent(w.agents[&parent])],
            "done, parent",
        );
        assert_eq!(w.hand(), "");
        w.asks(parent);
        assert_eq!(w.hand(), "rho@lab › parent · waits · now");
    }

    #[test]
    fn a11_an_agent_made_by_an_agent_deals_only_by_mentioning_the_user() {
        let mut w = world();
        let parent = w.agent("parent");
        let child = w.agent_under("child", Some(parent));
        w.works(child);
        assert_eq!(w.hand(), "");
        w.asks(child);
        let hand = w.deal();
        assert_eq!(hand.cards[0].address, w.address(child));
        assert_eq!(w.hand(), "rho@lab › child · waits · now");
    }

    #[test]
    fn a_stop_is_mail_until_answered_and_a_new_stop_waits_again() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        let first = w.stop(a, Stop::Failed);
        w.pass(mins(2));
        w.stop(b, Stop::Lost);
        assert_eq!(
            w.hand(),
            "rho@lab › a · failed · 2m\nrho@lab › b · lost · now"
        );
        let card = &w.deal().cards[0];
        assert_eq!(attention_mail(card.seen), Some(first));
        w.done(a);
        w.writes_to(b);
        assert_eq!(w.hand(), "");
        w.pass(mins(1));
        let second = w.stop(a, Stop::Stuck);
        w.done_through(a, Seen::Topic(first.0.0));
        assert_eq!(w.hand(), "rho@lab › a · stuck · now");
        assert_eq!(attention_mail(w.deal().cards[0].seen), Some(second));
    }

    #[test]
    fn done_on_one_ping_leaves_the_next_ping_in_the_same_thread() {
        let mut w = world();
        let thread = w.agent("two asks");
        let first = w.asks(thread);
        w.pass(mins(3));
        let second = w.stop(thread, Stop::Failed);
        let dealt = w.deal();
        assert_eq!(attention_mail(dealt.cards[0].seen), Some(first));

        w.done(thread);
        let remaining = w.deal();
        assert_eq!(remaining.cards.len(), 1, "the second ping still waits");
        assert_eq!(attention_mail(remaining.cards[0].seen), Some(second));
        assert_eq!(remaining.cards[0].kind, CardKind::Stop("failed"));
    }

    #[test]
    fn done_on_a_dealt_version_does_not_acknowledge_an_edit_made_while_it_was_open() {
        let mut w = world();
        let thread = w.agent("the clock");
        let first = w.asks(thread);
        let dealt = w.deal().cards.remove(0);
        w.pass(mins(4));
        let second = w.asks(thread);
        w.pass(mins(7));
        w.record(ChatEvent::Revised {
            message: first,
            author: w.agents[&thread],
            ping: None,
            content: vec![ContentPart::Text {
                text: "A different question about the same clock.".into(),
            }],
        });
        let edited = w.deal().cards.remove(0);
        assert_eq!(attention_mail(edited.seen), Some(first));
        assert_ne!(edited.seen, dealt.seen);
        assert_eq!(edited.cursor, format!("{}/{}", first.0.0, second.0.0));

        w.done_through(thread, dealt.seen);
        let next = w.deal().cards.remove(0);
        assert_eq!(attention_mail(next.seen), Some(second));
        assert_eq!(next.label, "waits · 7m");
        w.done_through(thread, next.seen);
        let revised = w.deal().cards.remove(0);
        assert_eq!(revised.seen, edited.seen);
        assert_eq!(revised.label, "waits · now");
    }

    #[test]
    fn a_dealt_thread_continues_after_the_latest_reply_without_losing_older_mail() {
        let mut w = world();
        let a = w.agent("a");
        let older = w.stop(a, Stop::Failed);
        let answered = w.asks(a);
        w.answers(a, answered);
        w.sends(a, false);
        let next = w.finishes(a);
        w.pass(mins(3));
        let newest = w.asks(a);

        let card = &w.deal().cards[0];
        assert_eq!(attention_mail(card.seen), Some(next));
        assert_eq!(card.label, "waits · 3m");
        w.answers(a, next);
        // This reply is after the latest ask; it did not answer that ask or
        // the older stop, and both still wait.
        assert_eq!(attention_mail(w.deal().cards[0].seen), Some(older));
        w.answers(a, older);
        assert_eq!(attention_mail(w.deal().cards[0].seen), Some(newest));
    }

    #[test]
    fn the_first_waiting_mail_anchors_the_card_even_as_new_mail_arrives() {
        let mut w = world();
        let a = w.agent("a");
        let ask = w.asks(a);
        w.pass(mins(2));
        w.stop(a, Stop::Failed);
        let deal = w.deal();
        let card = &deal.cards[0];
        assert_eq!(card.label, "waits · 2m");
        assert_eq!(attention_mail(card.seen), Some(ask));
        w.mute(a);
        assert_eq!(w.hand(), "");
    }

    #[test]
    fn even_a_single_host_card_names_the_project_address() {
        let mut w = world();
        let a = w.agent("a");
        w.asks(a);
        assert_eq!(w.hand(), "rho@lab › a · waits · now");
        // A message right under the root is a thread of its own.
        let root = w.root();
        let from = Party::Agent(w.agents[&a]);
        w.post(
            Some(root),
            from,
            vec![Mention::User { ping: true }],
            "at the root",
        );
        let hand = w.deal();
        assert!(
            hand.cards
                .iter()
                .any(|card| card.heading() == "rho@lab › at the root"),
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
        assert_eq!(w.hand(), "rho@lab › a · deadline · 1d");
    }
}
