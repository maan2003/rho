//! A thread with pinging mail the user has not read is a card. Done or a
//! reply reads the thread through the newest mail in view: what came
//! before is settled, and only a newer message or edit deals it again.
//! A stop remains after its agent moves on, and no addressed result fades
//! unread. Marks still belong to the user.

use std::collections::BTreeMap;

use jiff::Timestamp;
use rho_agent_types::{
    Address as Party, AgentId, ChatSeq, EndpointId, Message, MessageId, MessageVersion,
};
use rho_dealer::curve::{self, Curve};
use rho_dealer::facts::{Facts, Seen};
use rho_dealer::rank::{CardKind, Trace};
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
    /// The device's name for the host, used in the project address.
    pub name: &'a str,
}

fn unix(ms: u64) -> Timestamp {
    Timestamp::from_millisecond(ms as i64).expect("a message's time")
}

/// Every thread on the host that wants the user, as a claim each.
/// `local_answers` are this device's replies the host has not logged yet:
/// each settles its thread as a logged one does.
pub fn claims(
    host: &Host<'_>,
    marks: &Marks,
    now: Timestamp,
    mut trace: Option<&mut Trace>,
    local_answers: impl IntoIterator<Item = MessageVersion>,
) -> Vec<Claim> {
    let chat = host.chat;
    // One walk over the chat, not one per thread: every thread's mentions
    // of the user in log order, and the newest message the user wrote in
    // it. Ranking happens on every change, and a host's chat is long.
    let mut threads: BTreeMap<MessageId, ThreadMessages<'_>> = BTreeMap::new();
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
            thread.replied = thread.replied.max(replied_through(message));
        }
    }
    for answer in local_answers {
        if let Some(thread) = threads.get_mut(&chat.subthread_of(answer.message)) {
            thread.replied = thread.replied.max(Some(answer.version));
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
        let seen = SeenMail::new(facts, messages.replied);
        let through = |mail| seen.through(mail);
        let mut unseen = messages
            .mentions
            .iter()
            .copied()
            .filter(|message| {
                through(message.id).is_none_or(|version| message.version().0 > version)
            })
            .collect::<Vec<_>>();
        let waiting_since = |message: &Message| message.ping_since(through(message.id));
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
        // The card waits from the oldest mail not yet read.
        let Some(mail) = unseen.first().copied() else {
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
        // The cursor locates the oldest unread mail and notices new pings,
        // not edits of a ping the user hasn't settled yet.
        let newest = unseen
            .iter()
            .map(|mail| waiting_since(mail).0)
            .max()
            .expect("the oldest unread is unseen");
        let cursor = format!("{}/{}", mail.id.0.0, newest.0);
        let mut claim = Claim::new(address, curve, reason, cursor);
        // Done or a reply reads the thread through its newest mail in
        // view, so every ping in it now is settled at once.
        claim.seen = Seen::Topic(
            messages
                .mentions
                .iter()
                .map(|mail| mail.version().0.0)
                .max()
                .expect("the thread has unread mail"),
        );
        claim.title = chat.name(thread).unwrap_or_default();
        claim.context = context(host.chat, thread, host.name);
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

/// What a reply on a card tells the host it answers: the newest ping the
/// user could see in the thread, and how far they read, which settles
/// every ping in the thread at or before it.
pub fn answered(chat: &HostChat, thread: MessageId, seen: Seen) -> Option<MessageVersion> {
    let Seen::Topic(through) = seen else {
        return None;
    };
    let through = ChatSeq(through);
    chat.reading_order(thread)
        .filter(|mail| chat.subthread_of(mail.id) == thread)
        .filter(|mail| mail.pings_user() && mail.version().0 <= through)
        .max_by_key(|mail| mail.version().0)
        .map(|mail| MessageVersion {
            message: mail.id,
            version: through,
        })
}

/// What a thread is called, and where it is: its subject and the address
/// of its project.
pub fn name(host: &Host<'_>, thread: MessageId) -> Option<(String, String)> {
    let title = host.chat.name(thread)?;
    Some((title, context(host.chat, thread, host.name)))
}

/// Where a thread is: the address of the project above it.
pub(crate) fn context(chat: &HostChat, thread: MessageId, host_name: &str) -> String {
    context_at_root(chat, chat.root_of(thread), host_name)
}

pub(crate) fn context_at_root(chat: &HostChat, root: MessageId, host_name: &str) -> String {
    chat.project(root).map_or_else(
        || host_name.to_owned(),
        |project| project.address(host_name),
    )
}

/// How far a user's post read its thread. A reply records it. An older
/// post that does not, addressed or not, was written with everything
/// before it in view.
fn replied_through(post: &Message) -> Option<ChatSeq> {
    match post.thread_reply {
        Some(reply) => reply.settles.map(|seen| seen.version),
        None => Some(post.id.0),
    }
}

/// How much of a thread is read: how far done and the user's replies
/// read it, and the verdicts older builds said on one mail each.
#[derive(Debug)]
struct SeenMail {
    through: Option<ChatSeq>,
    exact: BTreeMap<MessageId, ChatSeq>,
}

impl SeenMail {
    fn new(facts: Facts<'_>, replied: Option<ChatSeq>) -> Self {
        let mut exact = BTreeMap::new();
        for (message, version) in facts.seen_mail() {
            let through = exact
                .entry(MessageId(ChatSeq(message)))
                .or_insert(ChatSeq(version));
            *through = (*through).max(ChatSeq(version));
        }
        Self {
            through: facts.seen_topic().map(ChatSeq).max(replied),
            exact,
        }
    }

    fn through(&self, mail: MessageId) -> Option<ChatSeq> {
        self.through.max(self.exact.get(&mail).copied())
    }
}

#[derive(Default)]
struct ThreadMessages<'a> {
    mentions: Vec<&'a Message>,
    user_wrote: Option<&'a Message>,
    /// How far the user's replies read the thread.
    replied: Option<ChatSeq>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use jiff::{SignedDuration, Zoned};
    use rho_agent_types::{
        AgentIdDomain, ChatEvent, ChatRecord, ContentPart, Mention, Stop, UnixMs,
    };
    use rho_dealer::curve::*;
    use rho_dealer::facts::{Device, Entry, EntryId, Fact};
    use rho_dealer::rank::{Card, Hand, Sources};
    use rho_dealer::{Address, Skips, Until, rank};

    use super::*;

    /// One host's chat, the user's facts and a clock to move.
    struct World {
        chat: HostChat,
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
            chat: HostChat::default(),
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
                thread_reply: None,
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
                thread_reply: None,
                from: Party::User,
                mentions: Vec::new(),
                content: vec![ContentPart::Text {
                    text: "answer".into(),
                }],
            });
        }

        fn tell(&mut self, fact: Fact) -> EntryId {
            let at = self.marks.next_at(&self.now);
            self.now = at.clone();
            let entry = Entry {
                device: Device([0; 16]),
                at,
                fact,
            };
            let id = entry.id();
            self.marks.apply([entry]);
            id
        }

        fn newest(&self, thread: MessageId) -> Seen {
            Seen::Topic(self.chat.newest_in(thread).map_or(0, |(seq, _)| seq.0))
        }

        /// Done on the card as the hand holds it, as the frame says it.
        fn done(&mut self, thread: MessageId) -> EntryId {
            let seen = self
                .deal()
                .cards
                .iter()
                .find(|card| card.address == self.address(thread))
                .map_or(Seen::Whole, |card| card.seen);
            self.done_through(thread, seen)
        }

        fn done_through(&mut self, thread: MessageId, seen: Seen) -> EntryId {
            self.tell(Fact::Settled {
                node: self.address(thread),
                seen,
            })
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
            self.deal_with([])
        }

        /// The hand with replies this device has queued and the host has
        /// not logged.
        fn deal_with(&self, local_answers: impl IntoIterator<Item = MessageVersion>) -> Hand {
            let host = Host {
                endpoint: EndpointId([0; 32]),
                chat: &self.chat,
                name: "lab",
            };
            let claims = claims(
                &host,
                &self.marks,
                self.now.timestamp(),
                None,
                local_answers,
            );
            rank(
                &Sources {
                    claims: &claims,
                    marks: &self.marks,
                    skips: &self.skips,
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

        /// The mail a reply on the thread's card says it answers.
        fn answered(&self, thread: MessageId) -> Option<MessageId> {
            let address = self.address(thread);
            let card = self
                .deal()
                .cards
                .into_iter()
                .find(|card| card.address == address)?;
            Some(answered(&self.chat, thread, card.seen)?.message)
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

    /// The oldest unread mail, which the card waits from.
    fn unread_from(card: &Card) -> MessageId {
        let (mail, _) = card.cursor.split_once('/').expect("a thread cursor");
        MessageId(ChatSeq(mail.parse().unwrap()))
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
                .map(unread_from)
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

    /// A reply reads the whole thread through how far it records the
    /// user saw, whichever mail it names; mail past that still deals.
    #[test]
    fn a16_a_reply_settles_the_thread_through_what_was_in_view() {
        let mut w = world();
        let thread = w.agent("Clock");
        let early = w.asks(thread);
        w.pass(mins(2));
        let later = w.asks(thread);
        let say = |w: &mut World, settles| {
            w.record(ChatEvent::Posted {
                parent: Some(thread),
                reply_to: None,
                thread_reply: Some(rho_agent_types::ThreadReply { settles }),
                from: Party::User,
                kind: rho_agent_types::MessageKind::Kept,
                mentions: Vec::new(),
                content: vec![ContentPart::Text {
                    text: "Try the second clock.".into(),
                }],
            });
        };
        say(&mut w, None);
        assert_eq!(
            w.hand(),
            "rho@lab › Clock · waits · 2m",
            "a reply that saw nothing waiting settles nothing"
        );
        say(
            &mut w,
            Some(MessageVersion {
                message: early,
                version: early.0,
            }),
        );
        assert_eq!(
            w.hand(),
            "rho@lab › Clock · waits · now",
            "the later ask was not in view, though the reply came after it"
        );
        assert_eq!(unread_from(&w.deal().cards[0]), later);
        w.pass(mins(1));
        let edit = w.record(ChatEvent::Revised {
            message: early,
            author: w.agents[&thread],
            ping: None,
            content: vec![ContentPart::Text {
                text: "Use the monotonic clock instead?".into(),
            }],
        });
        assert_eq!(
            w.answered(thread),
            Some(early),
            "a reply answers the newest ping in view, the edit"
        );
        // Names the edited ask, and reads through the later one too.
        say(
            &mut w,
            Some(MessageVersion {
                message: early,
                version: edit.0,
            }),
        );
        assert_eq!(w.hand(), "");
        w.asks(thread);
        assert_eq!(w.hand(), "rho@lab › Clock · waits · now");
    }

    /// What a reply tells the host it answers: the newest ping in the
    /// thread at or before how far the card was read, never a ping on a
    /// tangent or one that came later.
    #[test]
    fn a_reply_answers_the_newest_ping_in_view() {
        let mut w = world();
        let a = w.agent("a");
        let first = w.asks(a);
        let second = w.asks(a);
        let progress = w.sends(a, false);
        let tangent = w.sends_under(a, Some(progress), true);
        let third = w.asks(a);
        let read = |through: MessageId| answered(&w.chat, a, Seen::Topic(through.0.0));
        let answer = |message: MessageId, through: MessageId| MessageVersion {
            message,
            version: through.0,
        };
        assert_eq!(read(first), Some(answer(first, first)));
        assert_eq!(read(tangent), Some(answer(second, tangent)));
        assert_eq!(read(third), Some(answer(third, third)));
        assert_eq!(read(MessageId(ChatSeq(0))), None);
        assert_eq!(answered(&w.chat, a, Seen::Whole), None);
        // The tangent is its own thread, with its own answer.
        assert_eq!(
            answered(&w.chat, progress, Seen::Topic(third.0.0)),
            Some(answer(tangent, third))
        );
    }

    /// A reply this device has queued settles the thread as the logged one
    /// will, through how far it saw: nothing comes back while it is on its
    /// way, and mail after it still deals.
    #[test]
    fn a_queued_reply_settles_the_thread_through_what_was_in_view() {
        let mut w = world();
        let a = w.agent("a");
        let first = w.asks(a);
        let second = w.stop(a, Stop::Failed);
        let saw = |message: MessageId, through: MessageId| MessageVersion {
            message,
            version: through.0,
        };
        assert_eq!(w.deal_with([saw(first, second)]).cards, []);
        assert_eq!(
            w.deal_with([saw(first, first)]).cards[0].kind,
            CardKind::Stop("failed"),
            "the stop came after what the reply saw"
        );
        w.pass(mins(1));
        let third = w.asks(a);
        let hand = w.deal_with([saw(second, second)]);
        assert_eq!(hand.cards.len(), 1);
        assert_eq!(unread_from(&hand.cards[0]), third);
    }

    /// An older build's reply to one mail was written with the thread in
    /// view, so it reads the thread through itself.
    #[test]
    fn an_older_addressed_reply_reads_the_thread_through_itself() {
        let mut w = world();
        let a = w.agent("a");
        w.asks(a);
        let second = w.asks(a);
        w.finishes(a);
        w.answers(a, second);
        assert_eq!(w.hand(), "");
        w.asks(a);
        assert_eq!(w.hand(), "rho@lab › a · waits · now");
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
        assert_eq!(unread_from(card), result);
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

    /// Mail and stops deal by priority: older waits first, and the answer
    /// in the thread the user just wrote in ahead of them all.
    #[test]
    fn pinging_mail_and_stops_deal_by_priority() {
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
            [new_ask, old_result, new_result, old_ask, stop].map(|thread| w.address(thread)),
        );
        // Half a unit an hour, and the full bonus for words just written.
        let waited = |mins: f64| AGENT_BLOCKED_HEAD_START + 0.5 * mins / 60.0;
        for (thread, priority) in [
            (new_ask, AGENT_BLOCKED_HEAD_START + AGENT_RECENCY_BONUS),
            (old_result, waited(47.0)),
            (stop, waited(12.0)),
        ] {
            assert!((w.priority(thread).unwrap() - priority).abs() < 1e-6);
        }
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
                    assert_eq!(unread_from(&hand.cards[0]), ask);
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
        assert_eq!(unread_from(card), ask);
        assert_ne!(unread_from(card), older);
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
        assert_eq!(unread_from(card), failure);
    }

    #[test]
    fn a7_an_answer_soon_after_the_users_words_deals_first_and_chimes() {
        let mut w = world();
        let a = w.agent("a");
        let b = w.agent("b");
        w.asks(b);
        w.writes_to(a);
        w.pass(mins(5));
        w.finishes(a);
        assert_eq!(
            w.hand(),
            "rho@lab › a · waits · now\nrho@lab › b · waits · 5m"
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
        assert_eq!(unread_from(&hand.cards[0]), trigger);
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
        assert_eq!(unread_from(card), first);
        w.done(a);
        w.writes_to(b);
        assert_eq!(w.hand(), "");
        w.pass(mins(1));
        let second = w.stop(a, Stop::Stuck);
        w.done_through(a, Seen::Topic(first.0.0));
        assert_eq!(w.hand(), "rho@lab › a · stuck · now");
        assert_eq!(unread_from(&w.deal().cards[0]), second);
    }

    /// Done reads the whole thread through its newest mail: both pings
    /// go at once, and only a newer ping or an edit deals it again.
    #[test]
    fn a15_done_settles_every_ping_in_view_and_only_newer_mail_deals_again() {
        let mut w = world();
        let thread = w.agent("two asks");
        let first = w.asks(thread);
        w.pass(mins(3));
        let second = w.stop(thread, Stop::Failed);
        let dealt = w.deal().cards.remove(0);
        assert_eq!(unread_from(&dealt), first);
        assert_eq!(dealt.seen, Seen::Topic(second.0.0));

        w.done(thread);
        assert_eq!(w.hand(), "", "the stop was in view too");
        w.pass(mins(5));
        w.record(ChatEvent::Revised {
            message: first,
            author: w.agents[&thread],
            ping: None,
            content: vec![ContentPart::Text {
                text: "A different question about the same clock.".into(),
            }],
        });
        assert_eq!(w.hand(), "rho@lab › two asks · waits · now");
        w.done(thread);
        assert_eq!(w.hand(), "");
        w.pass(mins(1));
        let third = w.asks(thread);
        let next = w.deal().cards.remove(0);
        assert_eq!(
            (unread_from(&next), next.label.as_str()),
            (third, "waits · now")
        );
    }

    /// Undo takes the done back: the card returns as it was, waiting from
    /// its oldest unread mail.
    #[test]
    fn undoing_done_brings_the_thread_back_as_it_was() {
        let mut w = world();
        let thread = w.agent("two asks");
        w.asks(thread);
        w.pass(mins(3));
        w.asks(thread);
        let before = w.hand();
        assert_eq!(before, "rho@lab › two asks · waits · 3m");
        let done = w.done(thread);
        assert_eq!(w.hand(), "");
        w.tell(Fact::Retract { of: done });
        assert_eq!(w.hand(), before);
    }

    /// A card read before an edit, as an inspection holds it, settles
    /// only what it saw: the edit and the ping after it still deal.
    #[test]
    fn done_on_what_was_seen_before_an_edit_leaves_the_edit() {
        let mut w = world();
        let thread = w.agent("the clock");
        let first = w.asks(thread);
        let dealt = w.deal().cards.remove(0);
        w.pass(mins(4));
        let second = w.asks(thread);
        w.pass(mins(7));
        let edit = w.record(ChatEvent::Revised {
            message: first,
            author: w.agents[&thread],
            ping: None,
            content: vec![ContentPart::Text {
                text: "A different question about the same clock.".into(),
            }],
        });
        let edited = w.deal().cards.remove(0);
        assert_eq!(unread_from(&edited), first);
        assert_eq!(edited.seen, Seen::Topic(edit.0.0));
        assert_eq!(edited.cursor, format!("{}/{}", first.0.0, second.0.0));

        w.done_through(thread, dealt.seen);
        let next = w.deal().cards.remove(0);
        assert_eq!(unread_from(&next), second);
        assert_eq!(next.label, "waits · 7m");
        w.done_through(thread, next.seen);
        assert_eq!(w.hand(), "", "the live card saw the edit");
    }

    /// The user's replies read the thread: later mail deals from where
    /// the newest one left off.
    #[test]
    fn a_reply_reads_the_thread_and_only_later_mail_deals() {
        let mut w = world();
        let a = w.agent("a");
        w.stop(a, Stop::Failed);
        let answered = w.asks(a);
        w.answers(a, answered);
        w.sends(a, false);
        let next = w.finishes(a);
        w.pass(mins(3));
        w.asks(a);

        let card = &w.deal().cards[0];
        assert_eq!(unread_from(card), next);
        assert_eq!(card.label, "waits · 3m");
        w.answers(a, next);
        assert_eq!(w.hand(), "", "the reply came after the newest ask too");
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
        assert_eq!(unread_from(card), ask);
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
    fn an_old_deadline_on_a_thread_deals_nothing() {
        let mut w = world();
        let a = w.agent("a");
        w.tell(Fact::Deadline {
            node: w.address(a),
            by: Until::In(SignedDuration::from_hours(-1)),
            lead_days: 1,
        });
        assert_eq!(w.hand(), "");
    }
}
