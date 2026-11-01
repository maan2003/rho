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
    /// The agent that posted it.
    pub by: AgentId,
    pub kind: MentionKind,
    pub at: UnixMs,
}

/// One topic, folded from its records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topic {
    pub stream: StreamId,
    pub name: String,
    /// Done with: a resolved topic's work is finished.
    pub resolved: bool,
    pub started_at: UnixMs,
    /// Its members, each with the `Joined` that made it one: it receives
    /// every message after that. Posting or being mentioned does not make
    /// an agent a member.
    pub members: BTreeMap<AgentId, ChatSeq>,
    /// The newest message in it.
    pub newest: Option<(MessageId, UnixMs)>,
    /// The newest message an agent posted in it.
    pub agent_wrote: Option<(MessageId, UnixMs)>,
    /// The user's newest message in it.
    pub user_wrote: Option<(MessageId, UnixMs)>,
    /// Every mention of the user in it, oldest first.
    pub mentions: Vec<UserMention>,
    /// Every message in it not from the user, oldest first: what can be
    /// unread.
    pub others: Vec<MessageId>,
}

/// What the fold keeps of each message of a topic, so a move can work out
/// both topics again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Brief {
    topic: TopicId,
    at: UnixMs,
    from: Address,
    user: Option<MentionKind>,
}

impl Topic {
    /// Every agent in it: its members, and the agents that mentioned the
    /// user in it.
    pub fn agents(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.members
            .keys()
            .copied()
            .chain(self.mentions.iter().map(|mention| mention.by))
    }

    /// The members in the order they joined.
    pub fn members_in_order(&self) -> Vec<AgentId> {
        let mut members = self.members.iter().collect::<Vec<_>>();
        members.sort_by_key(|(agent, since)| (**since, **agent));
        members.into_iter().map(|(agent, _)| *agent).collect()
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

/// A direct conversation between two agents, folded: outside every stream,
/// and never a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Direct {
    pub members: [AgentId; 2],
    pub started_at: UnixMs,
    /// The newest message in it.
    pub newest: Option<(MessageId, UnixMs)>,
    /// Every message in it, oldest first: none is from the user.
    pub others: Vec<MessageId>,
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
    /// A direct moved, the same way.
    Direct {
        direct: TopicId,
        message: Option<(Message, Vec<AgentId>)>,
    },
    /// Messages moved to `to`: the topics they left, and `to`, moved.
    Moved {
        messages: Vec<MessageId>,
        to: TopicId,
        topics: Vec<TopicId>,
    },
}

/// One host's chat, folded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostChat {
    pub streams: BTreeMap<StreamId, String>,
    pub topics: BTreeMap<TopicId, Topic>,
    /// Direct conversations, by the seq that started each; messages name
    /// them as they name topics.
    pub directs: BTreeMap<TopicId, Direct>,
    /// Each message of a topic, where it is now.
    briefs: BTreeMap<MessageId, Brief>,
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
            ChatEvent::TopicStarted { stream, name, .. } => {
                let topic = TopicId(record.seq);
                if self.topics.contains_key(&topic) {
                    return Told::Nothing;
                }
                self.topics.insert(
                    topic,
                    Topic {
                        stream: *stream,
                        name: name.clone(),
                        resolved: false,
                        started_at: record.at,
                        members: BTreeMap::new(),
                        newest: None,
                        agent_wrote: None,
                        user_wrote: None,
                        mentions: Vec::new(),
                        others: Vec::new(),
                    },
                );
                Told::Topic {
                    topic,
                    message: None,
                }
            }
            ChatEvent::DirectStarted { members } => {
                let direct = TopicId(record.seq);
                if self.directs.contains_key(&direct) {
                    return Told::Nothing;
                }
                self.directs.insert(
                    direct,
                    Direct {
                        members: *members,
                        started_at: record.at,
                        newest: None,
                        others: Vec::new(),
                    },
                );
                Told::Direct {
                    direct,
                    message: None,
                }
            }
            ChatEvent::Joined { topic, agent } => {
                let Some(held) = self.topics.get_mut(topic) else {
                    tracing::warn!(topic = topic.0.0, "a join to a topic not heard of");
                    return Told::Nothing;
                };
                if held.members.contains_key(agent) {
                    return Told::Nothing;
                }
                held.members.insert(*agent, record.seq);
                Told::Topic {
                    topic: *topic,
                    message: None,
                }
            }
            ChatEvent::Left { topic, agent } => {
                let Some(held) = self.topics.get_mut(topic) else {
                    tracing::warn!(topic = topic.0.0, "a leave from a topic not heard of");
                    return Told::Nothing;
                };
                if held.members.remove(agent).is_none() {
                    return Told::Nothing;
                }
                Told::Topic {
                    topic: *topic,
                    message: None,
                }
            }
            ChatEvent::TopicChanged {
                topic,
                stream,
                name,
                resolved,
            } => {
                let Some(held) = self.topics.get_mut(topic) else {
                    tracing::warn!(topic = topic.0.0, "a change to a topic not heard of");
                    return Told::Nothing;
                };
                if (held.stream, held.name.as_str(), held.resolved) == (*stream, name, *resolved) {
                    return Told::Nothing;
                }
                held.stream = *stream;
                held.name = name.clone();
                held.resolved = *resolved;
                Told::Topic {
                    topic: *topic,
                    message: None,
                }
            }
            ChatEvent::Moved { messages, to } => {
                if !self.topics.contains_key(to) {
                    tracing::warn!(topic = to.0.0, "a move to a topic not heard of");
                    return Told::Nothing;
                }
                let mut topics = Vec::new();
                let mut moved = Vec::new();
                for message in messages {
                    // A message the fold has not heard of yet is posted where
                    // it is now: the disk copy keeps it there.
                    let Some(brief) = self.briefs.get_mut(message) else {
                        continue;
                    };
                    if brief.topic == *to {
                        continue;
                    }
                    if !topics.contains(&brief.topic) {
                        topics.push(brief.topic);
                    }
                    brief.topic = *to;
                    moved.push(*message);
                }
                if moved.is_empty() {
                    return Told::Nothing;
                }
                topics.push(*to);
                for topic in &topics {
                    self.refold(*topic);
                }
                Told::Moved {
                    messages: moved,
                    to: *to,
                    topics,
                }
            }
            ChatEvent::Posted { .. } => {
                let Some(message) = Message::from_record(record) else {
                    return Told::Nothing;
                };
                if self.briefs.contains_key(&message.id) {
                    return Told::Nothing;
                }
                let topic = &message.topic.clone();
                // Every member but the sender receives it, and so does each
                // agent it mentions.
                let mentioned = message
                    .mentions
                    .iter()
                    .filter_map(|mention| match mention.to {
                        Address::Agent(agent) => Some(agent),
                        Address::User => None,
                    });
                let reach = |members: &mut dyn Iterator<Item = AgentId>| {
                    let mut parties = Vec::new();
                    for agent in members.chain(mentioned.clone()) {
                        if message.from != Address::Agent(agent) && !parties.contains(&agent) {
                            parties.push(agent);
                        }
                    }
                    // Not a recipient, but the message is in the sender's
                    // own history.
                    if let Address::Agent(agent) = message.from {
                        parties.push(agent);
                    }
                    parties
                };
                if let Some(held) = self.directs.get_mut(topic) {
                    if held.newest.is_some_and(|(newest, _)| newest >= message.id) {
                        return Told::Nothing;
                    }
                    held.newest = Some((message.id, message.at));
                    held.others.push(message.id);
                    let parties = reach(&mut held.members.into_iter());
                    return Told::Direct {
                        direct: *topic,
                        message: Some((message, parties)),
                    };
                }
                let Some(held) = self.topics.get_mut(topic) else {
                    tracing::warn!(topic = topic.0.0, "a message in a topic not heard of");
                    return Told::Nothing;
                };
                if held.newest.is_some_and(|(newest, _)| newest >= message.id) {
                    return Told::Nothing;
                }
                self.briefs.insert(
                    message.id,
                    Brief {
                        topic: *topic,
                        at: message.at,
                        from: message.from,
                        user: message.mentions(Address::User),
                    },
                );
                let held = self.topics.get_mut(topic).expect("checked above");
                held.newest = Some((message.id, message.at));
                match message.from {
                    Address::User => held.user_wrote = Some((message.id, message.at)),
                    Address::Agent(_) => {
                        held.agent_wrote = Some((message.id, message.at));
                        held.others.push(message.id);
                    }
                }
                if let (Some(kind), Address::Agent(by)) =
                    (message.mentions(Address::User), message.from)
                {
                    held.mentions.push(UserMention {
                        message: message.id,
                        by,
                        kind,
                        at: message.at,
                    });
                }
                let parties = reach(
                    &mut held
                        .members
                        .iter()
                        .filter(|(_, since)| **since < record.seq)
                        .map(|(agent, _)| *agent),
                );
                Told::Topic {
                    topic: *topic,
                    message: Some((message, parties)),
                }
            }
        }
    }

    /// Works a topic's messages out again from where each now is.
    fn refold(&mut self, topic: TopicId) {
        let Some(held) = self.topics.get_mut(&topic) else {
            return;
        };
        held.newest = None;
        held.agent_wrote = None;
        held.user_wrote = None;
        held.mentions.clear();
        held.others.clear();
        for (id, brief) in self.briefs.iter().filter(|(_, brief)| brief.topic == topic) {
            held.newest = Some((*id, brief.at));
            match brief.from {
                Address::User => held.user_wrote = Some((*id, brief.at)),
                Address::Agent(by) => {
                    held.agent_wrote = Some((*id, brief.at));
                    held.others.push(*id);
                    if let Some(kind) = brief.user {
                        held.mentions.push(UserMention {
                            message: *id,
                            by,
                            kind,
                            at: brief.at,
                        });
                    }
                }
            }
        }
    }

    /// Takes the streams, topics and directs the model says moved, as they
    /// now stand.
    pub fn merge(&mut self, moved: HostChat) {
        self.streams.extend(moved.streams);
        self.topics.extend(moved.topics);
        self.directs.extend(moved.directs);
        self.briefs.extend(moved.briefs);
    }

    /// A stream by its name, if the host has one.
    pub fn stream_named(&self, name: &str) -> Option<StreamId> {
        self.streams
            .iter()
            .find(|(_, held)| *held == name)
            .map(|(stream, _)| *stream)
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

    /// The host of a conversation, a topic or a direct.
    pub fn conversation_host(&self, conversation: HostTopic) -> Option<HostId> {
        self.topic(conversation)
            .map(|(host, _)| host)
            .or_else(|| self.direct(conversation).map(|(host, _)| host))
    }

    /// A direct conversation's host as this window knows it, and the
    /// direct.
    pub fn direct(&self, direct: HostTopic) -> Option<(HostId, &Direct)> {
        self.hosts.iter().find_map(|(host, held)| {
            (held.machine_seed == Some(direct.machine_seed))
                .then(|| held.chat.directs.get(&direct.topic))
                .flatten()
                .map(|held| (*host, held))
        })
    }

    /// Every direct conversation of every host whose database is known.
    pub fn directs(&self) -> impl Iterator<Item = (HostTopic, HostId, &Direct)> {
        self.hosts.iter().flat_map(|(host, held)| {
            held.machine_seed.into_iter().flat_map(move |machine_seed| {
                held.chat.directs.iter().map(move |(direct, held)| {
                    (
                        HostTopic {
                            machine_seed,
                            topic: *direct,
                        },
                        *host,
                        held,
                    )
                })
            })
        })
    }

    /// The newest topic an agent is a member of.
    pub fn newest_topic_of(&self, host: HostId, agent: AgentId) -> Option<HostTopic> {
        let held = self.hosts.get(&host)?;
        let (topic, _) = held
            .chat
            .topics
            .iter()
            .filter(|(_, topic)| topic.members.contains_key(&agent))
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
        id: message.id,
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

    /// A stream at 1, `TOPIC` in it at 2, and `member` joining it at 3.
    fn started(member: AgentId) -> [ChatRecord; 3] {
        [
            record(1, ChatEvent::StreamCreated { name: "rho".into() }),
            record(
                2,
                ChatEvent::TopicStarted {
                    stream: StreamId(ChatSeq(1)),
                    name: "fix the flaky test".into(),
                    by: Address::User,
                },
            ),
            record(
                3,
                ChatEvent::Joined {
                    topic: TOPIC,
                    agent: member,
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

    /// A message reaches the topic's members, whom it mentions, and its
    /// sender. Only a join makes a member: neither posting nor being
    /// mentioned does, and a member hears only what comes after its join.
    #[test]
    fn a_message_reaches_the_members_and_whom_it_mentions() {
        let (a, b, c) = (agent(1), agent(2), agent(3));
        let mut chat = HostChat::default();
        for record in started(a) {
            chat.tell(&record);
        }
        assert_eq!(
            parties(chat.tell(&posted(4, Address::User, &[]))),
            [a],
            "the user's message reaches the member"
        );
        assert_eq!(
            parties(chat.tell(&posted(
                5,
                Address::Agent(b),
                &[(Address::Agent(c), MentionKind::Ask)]
            ))),
            sorted(vec![a, b, c]),
            "an outsider's post reaches the members, whom it mentions, and itself"
        );
        let topic = &chat.topics[&TOPIC];
        assert!(!topic.members.contains_key(&b), "posting does not join");
        assert!(!topic.members.contains_key(&c), "a mention does not join");
        assert_eq!(
            parties(chat.tell(&posted(6, Address::User, &[]))),
            [a],
            "neither the poster nor the mentioned hears what follows"
        );
        chat.tell(&record(
            7,
            ChatEvent::Joined {
                topic: TOPIC,
                agent: b,
            },
        ));
        assert_eq!(
            parties(chat.tell(&posted(8, Address::User, &[]))),
            sorted(vec![a, b]),
            "a join makes a member who hears what comes after"
        );
        assert_eq!(chat.topics[&TOPIC].members_in_order(), [a, b]);
    }

    /// An agent that left hears only what mentions it; a change renames,
    /// moves and resolves a topic whole; a move takes the mentions and the
    /// unread ids with the messages, and a replayed move moves nothing.
    #[test]
    fn leaving_changing_and_moving() {
        let (a, b) = (agent(1), agent(2));
        let other = TopicId(ChatSeq(5));
        let mut chat = HostChat::default();
        for record in started(a) {
            chat.tell(&record);
        }
        chat.tell(&posted(
            4,
            Address::Agent(b),
            &[(Address::User, MentionKind::Ask)],
        ));
        chat.tell(&record(
            5,
            ChatEvent::TopicStarted {
                stream: StreamId(ChatSeq(1)),
                name: "aside".into(),
                by: Address::User,
            },
        ));
        chat.tell(&record(
            6,
            ChatEvent::Left {
                topic: TOPIC,
                agent: a,
            },
        ));
        assert_eq!(
            parties(chat.tell(&posted(7, Address::User, &[]))),
            Vec::<AgentId>::new()
        );
        assert_eq!(
            parties(chat.tell(&posted(
                8,
                Address::User,
                &[(Address::Agent(a), MentionKind::Ask)]
            ))),
            [a],
            "a mention still reaches an agent that left"
        );
        chat.tell(&record(
            9,
            ChatEvent::TopicChanged {
                topic: TOPIC,
                stream: StreamId(ChatSeq(1)),
                name: "fixed".into(),
                resolved: true,
            },
        ));
        let held = &chat.topics[&TOPIC];
        assert_eq!((held.name.as_str(), held.resolved), ("fixed", true));
        let moved = record(
            10,
            ChatEvent::Moved {
                messages: vec![id(8), id(4)],
                to: other,
            },
        );
        assert_eq!(
            chat.tell(&moved),
            Told::Moved {
                messages: vec![id(8), id(4)],
                to: other,
                topics: vec![TOPIC, other],
            }
        );
        let (from, to) = (&chat.topics[&TOPIC], &chat.topics[&other]);
        assert_eq!(from.newest.map(|(message, _)| message), Some(id(7)));
        assert!(from.mentions.is_empty(), "the ask went with its message");
        assert_eq!(from.others, Vec::<MessageId>::new());
        assert_eq!(
            to.mentions.iter().map(|m| m.message).collect::<Vec<_>>(),
            [id(4)]
        );
        assert_eq!(to.others, [id(4)]);
        assert_eq!(to.user_wrote.map(|(message, _)| message), Some(id(8)));
        assert_eq!(chat.tell(&moved), Told::Nothing);
    }

    /// A direct conversation is its two agents': each message reaches
    /// both, and nothing in it is the user's to answer.
    #[test]
    fn a_direct_message_reaches_its_two_agents() {
        let (a, b, c) = (agent(1), agent(2), agent(3));
        let direct = TopicId(ChatSeq(1));
        let mut chat = HostChat::default();
        assert_eq!(
            chat.tell(&record(1, ChatEvent::DirectStarted { members: [a, b] })),
            Told::Direct {
                direct,
                message: None
            }
        );
        let told = chat.tell(&record(
            2,
            ChatEvent::Posted {
                topic: direct,
                from: Address::Agent(b),
                mentions: vec![Mention {
                    to: Address::User,
                    kind: MentionKind::Ask,
                }],
                content: Vec::new(),
            },
        ));
        let Told::Direct {
            message: Some((message, mut parties)),
            ..
        } = told
        else {
            panic!("not a direct message: {told:?}");
        };
        parties.sort();
        assert_eq!(parties, sorted(vec![a, b]));
        assert_eq!(message.id, id(2));
        assert!(chat.topics.is_empty(), "a direct is no topic");
        assert_eq!(chat.directs[&direct].newest, Some((id(2), UnixMs(20))));
        assert!(!chat.directs[&direct].members.contains(&c));
    }

    /// The user's mentions are kept in order with their kind; what the
    /// user has seen is what they said they had, or their own newest
    /// message if later.
    #[test]
    fn the_users_mentions_are_unseen_until_seen_or_answered() {
        let a = agent(1);
        let mut chat = HostChat::default();
        for record in started(a) {
            chat.tell(&record);
        }
        chat.tell(&posted(
            4,
            Address::Agent(a),
            &[(Address::User, MentionKind::Fyi)],
        ));
        chat.tell(&posted(
            5,
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
            [(id(4), MentionKind::Fyi), (id(5), MentionKind::Ask)]
        );
        assert_eq!(unseen(topic, Some(id(4))), [(id(5), MentionKind::Ask)]);
        assert_eq!(unseen(topic, Some(id(5))), []);
        assert_eq!(topic.mentions[1].at, UnixMs(50));

        chat.tell(&posted(6, Address::User, &[]));
        chat.tell(&posted(
            7,
            Address::Agent(a),
            &[(Address::User, MentionKind::Result)],
        ));
        let topic = &chat.topics[&TOPIC];
        assert_eq!(
            unseen(topic, None),
            [(id(7), MentionKind::Result)],
            "the user's own message reads what came before it"
        );
        assert_eq!(topic.user_wrote, Some((id(6), UnixMs(60))));
        assert_eq!(topic.newest, Some((id(7), UnixMs(70))));
        assert_eq!(topic.agent_wrote, Some((id(7), UnixMs(70))));
    }

    /// Telling a record again moves nothing, and a host's whole log told
    /// a topic at a time folds to what it folds to in log order.
    #[test]
    fn replays_move_nothing_and_order_across_topics_does_not_matter() {
        let (a, b) = (agent(1), agent(2));
        let other = TopicId(ChatSeq(4));
        let direct = TopicId(ChatSeq(10));
        let mut log = started(a).to_vec();
        log.push(record(
            4,
            ChatEvent::TopicStarted {
                stream: StreamId(ChatSeq(1)),
                name: "other".into(),
                by: Address::Agent(a),
            },
        ));
        log.push(record(
            5,
            ChatEvent::Joined {
                topic: other,
                agent: a,
            },
        ));
        log.push(posted(6, Address::User, &[]));
        log.push(record(
            7,
            ChatEvent::Joined {
                topic: TOPIC,
                agent: b,
            },
        ));
        log.push(record(
            8,
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
            9,
            Address::Agent(b),
            &[(Address::User, MentionKind::Result)],
        ));
        log.push(record(10, ChatEvent::DirectStarted { members: [a, b] }));
        log.push(record(
            11,
            ChatEvent::Posted {
                topic: direct,
                from: Address::Agent(a),
                mentions: Vec::new(),
                content: Vec::new(),
            },
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
        for record in started(a) {
            chat.tell(&record);
        }
        chat.tell(&posted(4, Address::Agent(a), &[]));
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
        for record in started(a) {
            chat.tell(&record);
        }
        let messages = [
            posted(4, Address::User, &[]),
            posted(5, Address::Agent(a), &[(Address::User, MentionKind::Ask)]),
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
                    id: id(4),
                    from: Address::User,
                    mentions: Vec::new(),
                    text: "message 4".into()
                },
                UiBlock::ChatMessage {
                    id: id(5),
                    from: Address::Agent(a),
                    mentions: vec![Mention {
                        to: Address::User,
                        kind: MentionKind::Ask
                    }],
                    text: "message 5".into()
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
