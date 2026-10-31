//! Chat, as the client folds it: each host's log into its streams and
//! topics ([`HostChat`]), and the topics of every host the window holds
//! ([`ChatMap`]).
//!
//! The fold is the same on the model thread, which tells it each record as
//! it comes, and on the main thread, which is handed the topics that moved.
//! It never depends on the order topics are told in, only on each topic's
//! own records being told oldest first, so the disk copy is read back a
//! topic at a time.

use std::collections::BTreeMap;

use rho_agent_hosts::HostId;
use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ChatSeq, MentionKind, Message, MessageId, StreamId,
    TopicId, UnixMs,
};

use crate::fold::one_line;
use crate::state::{UiAgentState, UiAgentStatus, UiAgentUsage, UiBlock};

/// A topic wherever it is: the database of the host that keeps it, and its
/// id in that host's log. The seed names the log on every device, where a
/// host's name is only what one device calls it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostTopic {
    pub machine_seed: u64,
    pub topic: TopicId,
}

/// A mention of the user: what it asks, in which message, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserMention {
    pub message: MessageId,
    pub kind: MentionKind,
    pub at: UnixMs,
}

/// One topic, folded from its records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topic {
    pub stream: StreamId,
    pub name: String,
    pub started_at: UnixMs,
    /// Its agents, each with the event that made it one: it receives every
    /// message after that.
    pub agents: BTreeMap<AgentId, ChatSeq>,
    /// The newest message in it.
    pub newest: Option<(MessageId, UnixMs)>,
    /// The newest message an agent posted in it.
    pub agent_wrote: Option<(MessageId, UnixMs)>,
    /// The user's newest message in it.
    pub user_wrote: Option<(MessageId, UnixMs)>,
    /// Every mention of the user in it, oldest first.
    pub mentions: Vec<UserMention>,
}

impl Topic {
    /// The agent that has been in it longest: whose screen a reader of the
    /// topic is on.
    pub fn first_agent(&self) -> Option<AgentId> {
        self.agents
            .iter()
            .min_by_key(|(agent, since)| (**since, **agent))
            .map(|(agent, _)| *agent)
    }

    /// The agents in the order they joined.
    pub fn agents_in_order(&self) -> Vec<AgentId> {
        let mut agents = self.agents.iter().collect::<Vec<_>>();
        agents.sort_by_key(|(agent, since)| (**since, **agent));
        agents.into_iter().map(|(agent, _)| *agent).collect()
    }

    /// How far the user has seen it: as far as they said they had, and
    /// everything before their own newest message in it.
    pub fn seen(&self, said: Option<MessageId>) -> Option<MessageId> {
        said.max(self.user_wrote.map(|(message, _)| message))
    }

    /// The mentions of the user after `seen`, oldest first.
    pub fn unseen(&self, seen: Option<MessageId>) -> impl Iterator<Item = &UserMention> {
        self.mentions
            .iter()
            .filter(move |mention| Some(mention.message) > seen)
    }
}

/// What telling one record moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Told {
    /// Nothing: it was held already.
    Nothing,
    Stream(StreamId),
    /// A topic moved. A message comes with the agents it is from or reaches.
    Topic {
        topic: TopicId,
        message: Option<(Message, Vec<AgentId>)>,
    },
}

/// One host's chat, folded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostChat {
    pub streams: BTreeMap<StreamId, String>,
    pub topics: BTreeMap<TopicId, Topic>,
}

impl HostChat {
    /// Folds one record. One already held moves nothing, so a replayed
    /// page is harmless.
    pub fn tell(&mut self, record: &ChatRecord) -> Told {
        match &record.event {
            ChatEvent::StreamCreated { name } => {
                let stream = StreamId(record.seq);
                if self.streams.contains_key(&stream) {
                    return Told::Nothing;
                }
                self.streams.insert(stream, name.clone());
                Told::Stream(stream)
            }
            ChatEvent::TopicStarted {
                stream,
                name,
                agents,
                ..
            } => {
                let topic = TopicId(record.seq);
                if self.topics.contains_key(&topic) {
                    return Told::Nothing;
                }
                self.topics.insert(
                    topic,
                    Topic {
                        stream: *stream,
                        name: name.clone(),
                        started_at: record.at,
                        agents: agents.iter().map(|agent| (*agent, record.seq)).collect(),
                        newest: None,
                        agent_wrote: None,
                        user_wrote: None,
                        mentions: Vec::new(),
                    },
                );
                Told::Topic {
                    topic,
                    message: None,
                }
            }
            ChatEvent::Joined { topic, agent } => {
                let Some(held) = self.topics.get_mut(topic) else {
                    tracing::warn!(topic = topic.0.0, "a join to a topic not heard of");
                    return Told::Nothing;
                };
                if !join(held, *agent, record.seq) {
                    return Told::Nothing;
                }
                Told::Topic {
                    topic: *topic,
                    message: None,
                }
            }
            ChatEvent::Posted { topic, .. } => {
                let Some(message) = Message::from_record(record) else {
                    return Told::Nothing;
                };
                let Some(held) = self.topics.get_mut(topic) else {
                    tracing::warn!(topic = topic.0.0, "a message in a topic not heard of");
                    return Told::Nothing;
                };
                if held.newest.is_some_and(|(newest, _)| newest >= message.id) {
                    return Told::Nothing;
                }
                // Every agent of the topic but its sender receives it, and
                // so does each agent it mentions.
                let mut parties = held
                    .agents
                    .iter()
                    .filter(|(agent, since)| {
                        **since < record.seq && message.from != Address::Agent(**agent)
                    })
                    .map(|(agent, _)| *agent)
                    .collect::<Vec<_>>();
                for mention in &message.mentions {
                    if let Address::Agent(agent) = mention.to
                        && !parties.contains(&agent)
                        && message.from != mention.to
                    {
                        parties.push(agent);
                    }
                }
                held.newest = Some((message.id, message.at));
                match message.from {
                    Address::User => held.user_wrote = Some((message.id, message.at)),
                    Address::Agent(agent) => {
                        // Posting makes the sender one of the topic's agents.
                        join(held, agent, record.seq);
                        held.agent_wrote = Some((message.id, message.at));
                        parties.push(agent);
                    }
                }
                if let Some(kind) = message.mentions(Address::User) {
                    held.mentions.push(UserMention {
                        message: message.id,
                        kind,
                        at: message.at,
                    });
                }
                Told::Topic {
                    topic: *topic,
                    message: Some((message, parties)),
                }
            }
        }
    }

    /// Takes the streams and topics the model says moved, as they now
    /// stand.
    pub fn merge(&mut self, moved: HostChat) {
        self.streams.extend(moved.streams);
        self.topics.extend(moved.topics);
    }
}

/// Makes `agent` one of the topic's agents from `seq` on, unless it
/// already is from earlier. Says whether anything moved.
fn join(topic: &mut Topic, agent: AgentId, seq: ChatSeq) -> bool {
    match topic.agents.get(&agent) {
        Some(since) if *since <= seq => false,
        _ => {
            topic.agents.insert(agent, seq);
            true
        }
    }
}

/// The topics of every host the window holds.
#[derive(Default)]
pub struct ChatMap {
    hosts: BTreeMap<HostId, Held>,
}

#[derive(Default)]
struct Held {
    /// Which database the host's ids count in; `None` until it says.
    machine_seed: Option<u64>,
    chat: HostChat,
    /// The host has told its chat up to its head since the window held
    /// it: what is here is all it has, not the disk copy alone.
    told: bool,
}

impl ChatMap {
    /// What there is of a host's chat now: the disk copy, or nothing after
    /// the copy started over.
    pub fn load(&mut self, host: HostId, machine_seed: Option<u64>, chat: HostChat) {
        self.hosts.insert(
            host,
            Held {
                machine_seed,
                chat,
                told: false,
            },
        );
    }

    /// Which database a host's ids count in, once it has said.
    pub fn machine_seed(&self, host: HostId) -> Option<u64> {
        self.hosts.get(&host)?.machine_seed
    }

    pub fn set_machine_seed(&mut self, host: HostId, machine_seed: u64) {
        self.hosts.entry(host).or_default().machine_seed = Some(machine_seed);
    }

    /// Streams and topics as they now stand, told once the host's chat
    /// has caught up with its head.
    pub fn merge(&mut self, host: HostId, moved: HostChat) {
        let held = self.hosts.entry(host).or_default();
        held.chat.merge(moved);
        held.told = true;
    }

    /// The host's chat as it now stands, once the host has told all of
    /// it; `None` while only the disk copy is held.
    pub fn told_chat(&self, host: HostId) -> Option<(u64, &HostChat)> {
        let held = self.hosts.get(&host).filter(|held| held.told)?;
        Some((held.machine_seed?, &held.chat))
    }

    pub fn detach_host(&mut self, host: HostId) {
        self.hosts.remove(&host);
    }

    pub fn host_chat(&self, host: HostId) -> Option<&HostChat> {
        self.hosts.get(&host).map(|held| &held.chat)
    }

    /// The topic's host as this window knows it, and the topic.
    pub fn topic(&self, topic: HostTopic) -> Option<(HostId, &Topic)> {
        self.hosts.iter().find_map(|(host, held)| {
            (held.machine_seed == Some(topic.machine_seed))
                .then(|| held.chat.topics.get(&topic.topic))
                .flatten()
                .map(|held| (*host, held))
        })
    }

    /// A topic of a host, by its id there.
    pub fn host_topic(&self, host: HostId, topic: TopicId) -> Option<HostTopic> {
        let held = self.hosts.get(&host)?;
        held.chat.topics.contains_key(&topic).then_some(HostTopic {
            machine_seed: held.machine_seed?,
            topic,
        })
    }

    /// Every topic of every host whose database is known.
    pub fn topics(&self) -> impl Iterator<Item = (HostTopic, HostId, &Topic)> {
        self.hosts.iter().flat_map(|(host, held)| {
            held.machine_seed.into_iter().flat_map(move |machine_seed| {
                held.chat.topics.iter().map(move |(topic, held)| {
                    (
                        HostTopic {
                            machine_seed,
                            topic: *topic,
                        },
                        *host,
                        held,
                    )
                })
            })
        })
    }

    /// The name of the stream a topic is in.
    pub fn stream_name(&self, topic: HostTopic) -> Option<&str> {
        let (host, held) = self.topic(topic)?;
        self.hosts[&host]
            .chat
            .streams
            .get(&held.stream)
            .map(String::as_str)
    }

    /// The newest topic an agent is in.
    pub fn newest_topic_of(&self, host: HostId, agent: AgentId) -> Option<HostTopic> {
        let held = self.hosts.get(&host)?;
        let (topic, _) = held
            .chat
            .topics
            .iter()
            .filter(|(_, topic)| topic.agents.contains_key(&agent))
            .max_by_key(|(id, topic)| (topic.newest.map(|(newest, _)| newest), **id))?;
        Some(HostTopic {
            machine_seed: held.machine_seed?,
            topic: *topic,
        })
    }
}

/// A topic's messages as they were written, oldest first, then what the
/// user wrote in it that no host has taken yet.
pub fn topic_state(messages: &[Message], unsent: &[String]) -> UiAgentState {
    let said = messages.iter().map(|message| UiBlock::ChatMessage {
        from: message.from,
        mentions: message.mentions.clone(),
        text: rho_agent_types::transcript::text_content(&message.content),
    });
    let waiting = unsent
        .iter()
        .map(|text| UiBlock::Unsent { text: text.clone() });
    UiAgentState {
        exec_timings: std::sync::Arc::default(),
        blocks: said.chain(waiting).map(std::sync::Arc::new).collect(),
        status: UiAgentStatus::Idle,
        runtime: None,
        context_used: None,
        usage: UiAgentUsage::default(),
    }
}

/// What a new topic is called by default: the first line of its first
/// message.
pub fn default_topic_name(text: &str) -> String {
    one_line(text)
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{ContentPart, Mention};

    use super::*;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &rho_agent_types::AgentIdDomain(0)).expect("an agent id")
    }

    fn record(seq: u64, event: ChatEvent) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq * 10),
            event,
        }
    }

    const TOPIC: TopicId = TopicId(ChatSeq(2));

    fn started(agents: Vec<AgentId>) -> [ChatRecord; 2] {
        [
            record(1, ChatEvent::StreamCreated { name: "rho".into() }),
            record(
                2,
                ChatEvent::TopicStarted {
                    stream: StreamId(ChatSeq(1)),
                    name: "fix the flaky test".into(),
                    by: Address::User,
                    agents,
                },
            ),
        ]
    }

    fn posted(seq: u64, from: Address, mentions: &[(Address, MentionKind)]) -> ChatRecord {
        record(
            seq,
            ChatEvent::Posted {
                topic: TOPIC,
                from,
                mentions: mentions
                    .iter()
                    .map(|(to, kind)| Mention {
                        to: *to,
                        kind: *kind,
                    })
                    .collect(),
                content: vec![ContentPart::Text {
                    text: format!("message {seq}"),
                }],
            },
        )
    }

    fn sorted(mut agents: Vec<AgentId>) -> Vec<AgentId> {
        agents.sort();
        agents
    }

    fn parties(told: Told) -> Vec<AgentId> {
        match told {
            Told::Topic {
                message: Some((_, mut parties)),
                ..
            } => {
                parties.sort();
                parties
            }
            other => panic!("not a message: {other:?}"),
        }
    }

    fn id(seq: u64) -> MessageId {
        MessageId(ChatSeq(seq))
    }

    /// A message reaches every agent of the topic but its sender, and each
    /// agent it mentions; posting makes the sender one of the topic's
    /// agents from then on, so it hears the next message but not its own
    /// or any before it.
    #[test]
    fn a_message_reaches_the_topics_agents_and_whom_it_mentions() {
        let (a, b, c) = (agent(1), agent(2), agent(3));
        let mut chat = HostChat::default();
        for record in started(vec![a]) {
            chat.tell(&record);
        }
        assert_eq!(
            parties(chat.tell(&posted(3, Address::User, &[]))),
            [a],
            "the user's message reaches the topic's agent"
        );
        assert_eq!(
            parties(chat.tell(&posted(
                4,
                Address::Agent(b),
                &[(Address::Agent(c), MentionKind::Fyi)]
            ))),
            sorted(vec![a, b, c]),
            "an outsider's post reaches the topic, whom it mentions, and is its own"
        );
        let topic = &chat.topics[&TOPIC];
        assert_eq!(topic.agents[&b], ChatSeq(4), "posting joined it");
        assert!(
            !topic.agents.contains_key(&c),
            "a mention reaches but does not join"
        );
        assert_eq!(
            parties(chat.tell(&posted(5, Address::Agent(a), &[]))),
            sorted(vec![a, b]),
            "the joined sender hears what comes after"
        );
        assert_eq!(chat.topics[&TOPIC].first_agent(), Some(a));
        assert_eq!(chat.topics[&TOPIC].agents_in_order(), [a, b]);
    }

    /// The user's mentions are kept in order with their kind; what the
    /// user has seen is what they said they had, or their own newest
    /// message if later.
    #[test]
    fn the_users_mentions_are_unseen_until_seen_or_answered() {
        let a = agent(1);
        let mut chat = HostChat::default();
        for record in started(vec![a]) {
            chat.tell(&record);
        }
        chat.tell(&posted(
            3,
            Address::Agent(a),
            &[(Address::User, MentionKind::Fyi)],
        ));
        chat.tell(&posted(
            4,
            Address::Agent(a),
            &[(Address::User, MentionKind::Ask)],
        ));
        let topic = &chat.topics[&TOPIC];
        let unseen = |topic: &Topic, said| {
            topic
                .unseen(topic.seen(said))
                .map(|mention| (mention.message, mention.kind))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            unseen(topic, None),
            [(id(3), MentionKind::Fyi), (id(4), MentionKind::Ask)]
        );
        assert_eq!(unseen(topic, Some(id(3))), [(id(4), MentionKind::Ask)]);
        assert_eq!(unseen(topic, Some(id(4))), []);
        assert_eq!(topic.mentions[1].at, UnixMs(40));

        chat.tell(&posted(5, Address::User, &[]));
        chat.tell(&posted(
            6,
            Address::Agent(a),
            &[(Address::User, MentionKind::Result)],
        ));
        let topic = &chat.topics[&TOPIC];
        assert_eq!(
            unseen(topic, None),
            [(id(6), MentionKind::Result)],
            "the user's own message reads what came before it"
        );
        assert_eq!(topic.user_wrote, Some((id(5), UnixMs(50))));
        assert_eq!(topic.newest, Some((id(6), UnixMs(60))));
        assert_eq!(topic.agent_wrote, Some((id(6), UnixMs(60))));
    }

    /// Telling a record again moves nothing, and a host's whole log told
    /// a topic at a time folds to what it folds to in log order.
    #[test]
    fn replays_move_nothing_and_order_across_topics_does_not_matter() {
        let (a, b) = (agent(1), agent(2));
        let other = TopicId(ChatSeq(3));
        let mut log = started(vec![a]).to_vec();
        log.push(record(
            3,
            ChatEvent::TopicStarted {
                stream: StreamId(ChatSeq(1)),
                name: "other".into(),
                by: Address::Agent(a),
                agents: vec![a],
            },
        ));
        log.push(posted(4, Address::User, &[]));
        log.push(record(
            5,
            ChatEvent::Joined {
                topic: TOPIC,
                agent: b,
            },
        ));
        log.push(record(
            6,
            ChatEvent::Posted {
                topic: other,
                from: Address::Agent(a),
                mentions: vec![Mention {
                    to: Address::User,
                    kind: MentionKind::Ask,
                }],
                content: Vec::new(),
            },
        ));
        log.push(posted(
            7,
            Address::Agent(b),
            &[(Address::User, MentionKind::Result)],
        ));

        let mut in_order = HostChat::default();
        for record in &log {
            in_order.tell(record);
        }
        for record in &log {
            assert_eq!(in_order.tell(record), Told::Nothing, "{record:?}");
        }
        // What the disk copy reads back: the events, then each topic's
        // messages.
        let mut by_topic = HostChat::default();
        let (posts, events): (Vec<_>, Vec<_>) = log
            .iter()
            .partition(|record| matches!(record.event, ChatEvent::Posted { .. }));
        for record in events {
            by_topic.tell(record);
        }
        let mut posts = posts;
        posts.sort_by_key(|record| match &record.event {
            ChatEvent::Posted { topic, .. } => (*topic, record.seq),
            _ => unreachable!(),
        });
        for record in posts {
            by_topic.tell(record);
        }
        assert_eq!(by_topic, in_order);
    }

    /// The window finds a topic by its host's database and id, and a host
    /// whose database it does not know yet has none to find.
    #[test]
    fn the_map_finds_a_topic_by_its_hosts_database() {
        let a = agent(1);
        let mut chat = HostChat::default();
        for record in started(vec![a]) {
            chat.tell(&record);
        }
        chat.tell(&posted(3, Address::Agent(a), &[]));
        let mut map = ChatMap::default();
        map.merge(HostId(4), chat.clone());
        assert_eq!(map.topics().count(), 0, "no database, no topic");
        map.set_machine_seed(HostId(4), 9);
        let topic = HostTopic {
            machine_seed: 9,
            topic: TOPIC,
        };
        assert_eq!(map.host_topic(HostId(4), TOPIC), Some(topic));
        assert_eq!(map.topic(topic).map(|(host, _)| host), Some(HostId(4)));
        assert_eq!(
            map.topic(HostTopic {
                machine_seed: 8,
                topic: TOPIC
            }),
            None
        );
        assert_eq!(map.stream_name(topic), Some("rho"));
        assert_eq!(map.newest_topic_of(HostId(4), a), Some(topic));
        map.load(HostId(4), Some(9), HostChat::default());
        assert_eq!(map.topic(topic), None, "a load replaces the host's chat");
    }

    /// A topic reads as its messages, each with who posted it and whom it
    /// mentions, then what waits to be sent.
    #[test]
    fn a_topic_reads_as_its_messages_then_what_waits() {
        let a = agent(1);
        let mut chat = HostChat::default();
        for record in started(vec![a]) {
            chat.tell(&record);
        }
        let messages = [
            posted(3, Address::User, &[]),
            posted(4, Address::Agent(a), &[(Address::User, MentionKind::Ask)]),
        ]
        .iter()
        .filter_map(Message::from_record)
        .collect::<Vec<_>>();
        let state = topic_state(&messages, &["later".to_owned()]);
        assert_eq!(
            state
                .blocks
                .iter()
                .map(|block| (**block).clone())
                .collect::<Vec<_>>(),
            [
                UiBlock::ChatMessage {
                    from: Address::User,
                    mentions: Vec::new(),
                    text: "message 3".into()
                },
                UiBlock::ChatMessage {
                    from: Address::Agent(a),
                    mentions: vec![Mention {
                        to: Address::User,
                        kind: MentionKind::Ask
                    }],
                    text: "message 4".into()
                },
                UiBlock::Unsent {
                    text: "later".into()
                },
            ]
        );
        assert_eq!(
            default_topic_name("\n  fix   the\ttest \nmore"),
            "fix the test"
        );
    }
}
