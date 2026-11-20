//! Each card is a thread's first waiting ask/stop or newest waiting result.
//! A visible mail is the only source of a claim; a stop remains after its
//! agent moves on, and no addressed result fades unread. Marks still belong
//! to the user, and a reply settles the mail that preceded it.

use std::collections::{BTreeMap, HashSet};

use jiff::Timestamp;
use rho_agent_types::{
    Address as Party, AgentId, ChatSeq, EndpointId, MentionKind, Message, MessageId,
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
) -> Vec<Claim> {
    let chat = host.chat;
    // One walk over the chat, not one per thread: every thread's mentions
    // of the user in log order, and the newest message the user wrote in
    // it. Ranking happens on every change, and a host's chat is long.
    let mut threads: BTreeMap<MessageId, ThreadMessages<'_>> = BTreeMap::new();
    let mut answered = HashSet::new();
    for message in chat.messages() {
        let mentioned = message.mentions(Party::User).is_some();
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
        let seen = facts
            .seen_topic()
            .map(|seen| MessageId(ChatSeq(seen)))
            .max(messages.plain_user_mail);
        let unseen = messages
            .mentions
            .into_iter()
            .filter(|message| {
                !answered.contains(&message.id) && seen.is_none_or(|seen| message.id > seen)
            })
            .collect::<Vec<_>>();
        if let Some(trace) = trace.as_deref_mut() {
            trace.input(
                &address,
                "unseen mentions",
                format!(
                    "{:?}",
                    unseen.iter().map(|mail| mail.id).collect::<Vec<_>>()
                ),
            );
            trace.input(&address, "seen through", format!("{seen:?}"));
        }

        let waiting = unseen
            .iter()
            .copied()
            .find(|mail| mail.mentions(Party::User) == Some(MentionKind::Ask));
        let result = unseen
            .iter()
            .copied()
            .rfind(|mail| mail.mentions(Party::User) == Some(MentionKind::Result));
        let (mail, reason) = match (waiting, result) {
            (Some(mail), _) => {
                let reason = match mail.from {
                    Party::System { stop, .. } => format!("{} · {{age}}", stop.word()),
                    _ => "asks · {age}".to_owned(),
                };
                (mail, reason)
            }
            (None, Some(mail)) => (mail, "result · {age}".to_owned()),
            (None, None) => {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.outcome(&address, "no card: nothing unread".to_owned());
                }
                continue;
            }
        };
        let since = unix(mail.at.0);
        let curve = Curve::Waiting {
            head_start: curve::AGENT_BLOCKED_HEAD_START,
            since,
        };
        // The first part is where to open the card; the last part changes
        // when more mail arrives, even if the trigger is still the same ask.
        let newest = unseen.last().expect("the trigger is unseen");
        let cursor = format!("{}/{}", mail.id.0.0, newest.id.0.0);
        let mut claim = Claim::new(address, curve, reason, cursor);
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
pub(crate) fn context(chat: &HostChat, thread: MessageId, host_name: &str) -> String {
    chat.project(chat.root_of(thread)).map_or_else(
        || host_name.to_owned(),
        |project| project.address(host_name),
    )
}

/// The mail a project card opens on, decoded from its source cursor.
pub fn attention_mail(cursor: &str) -> Option<MessageId> {
    let (trigger, _) = cursor.split_once('/')?;
    Some(MessageId(ChatSeq(trigger.parse().ok()?)))
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
    use rho_dealer::facts::{Device, Entry, Fact, Seen};
    use rho_dealer::rank::{Hand, Sources};
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
            now: "2026-08-23T09:00:00Z"
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

        fn answers(&mut self, thread: MessageId, mail: MessageId) {
            self.record(ChatEvent::Posted {
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
            Seen::Topic(self.chat.newest_in(thread).map_or(0, |(id, _)| id.0.0))
        }

        fn done(&mut self, thread: MessageId) {
            self.done_through(thread, self.newest(thread));
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
                vec![Mention {
                    to: Party::User,
                    kind: MentionKind::Ask,
                }],
                "lost connection",
            )
        }

        fn deal(&self) -> Hand {
            let host = Host {
                endpoint: EndpointId([0; 32]),
                chat: &self.chat,
                name: "lab",
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
        assert_eq!(cursor(root).as_deref().and_then(attention_mail), Some(root));
        assert_eq!(cursor(a).as_deref().and_then(attention_mail), Some(ask));
        w.writes_to(root);
        assert_eq!(
            w.hand(),
            "rho@lab › a · asks · 0m",
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
                .and_then(|card| attention_mail(&card.cursor))
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
        assert_eq!(w.hand(), "rho@lab › a · asks · 0m");
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
        assert_eq!(card.label, "result · 6.0d");
        assert_eq!(attention_mail(&card.cursor), Some(result));
        w.writes_to(a);
        assert_eq!(w.hand(), "rho@lab › b · asks · 6.0d");
    }

    #[test]
    fn a3_an_fyi_is_no_card() {
        let mut w = world();
        let a = w.agent("a");
        w.sends(a, MentionKind::Fyi);
        assert_eq!(w.hand(), "");
        w.asks(a);
        w.sends(a, MentionKind::Fyi);
        assert_eq!(w.hand(), "rho@lab › a · asks · 0m", "an fyi hides no ask");
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
            "rho@lab › a · result · 0m",
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
            "rho@lab › a · asks · 20m",
            "from the first ask unseen"
        );
        w.finishes(b);
        w.pass(mins(30));
        w.finishes(b);
        assert_eq!(
            w.hand(),
            "rho@lab › a · asks · 50m\nrho@lab › b · result · 0m",
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
        assert_eq!(w.hand(), "rho@lab › a · asks · 0m");
        w.writes_to(a);
        assert_eq!(w.hand(), "", "the user's own words see the thread");
        w.finishes(a);
        assert_eq!(w.hand(), "rho@lab › a · result · 0m");
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
        assert!(w.hand().starts_with("rho@lab › a ·"), "{}", w.hand());
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
            "rho@lab › a · asks · 0m",
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
        let reply = w.sends(a, MentionKind::Fyi);
        let trigger = w.sends_under(a, Some(reply), MentionKind::Ask);
        let hand = w.deal();
        assert_eq!(hand.cards[0].address, w.address(reply));
        assert_eq!(attention_mail(&hand.cards[0].cursor), Some(trigger));
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
            vec![Mention {
                to: Party::Agent(w.agents[&parent]),
                kind: MentionKind::Result,
            }],
            "done, parent",
        );
        assert_eq!(w.hand(), "");
        w.asks(parent);
        assert_eq!(w.hand(), "rho@lab › parent · asks · 0m");
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
        assert_eq!(w.hand(), "rho@lab › child · asks · 0m");
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
            "rho@lab › a · failed · 2m\nrho@lab › b · lost · 0m"
        );
        let card = &w.deal().cards[0];
        assert_eq!(attention_mail(&card.cursor), Some(first));
        w.done(a);
        w.writes_to(b);
        assert_eq!(w.hand(), "");
        w.pass(mins(1));
        let second = w.stop(a, Stop::Stuck);
        w.done_through(a, Seen::Topic(first.0.0));
        assert_eq!(w.hand(), "rho@lab › a · stuck · 0m");
        assert_eq!(attention_mail(&w.deal().cards[0].cursor), Some(second));
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
        assert_eq!(card.label, "asks · 2m");
        assert_eq!(attention_mail(&card.cursor), Some(ask));
        w.mute(a);
        assert_eq!(w.hand(), "");
    }

    #[test]
    fn even_a_single_host_card_names_the_project_address() {
        let mut w = world();
        let a = w.agent("a");
        w.asks(a);
        assert_eq!(w.hand(), "rho@lab › a · asks · 0m");
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
