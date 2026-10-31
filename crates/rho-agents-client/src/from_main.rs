//! Temporary: carries the user's unsent words over from a GUI that ran
//! `main`, which kept them by agent: what they were writing to each agent
//! (`gui_agent_draft_v1`) and what no host had taken yet
//! (`gui_agent_outbox_v1`). Chat keeps them by topic. The host's migration
//! made one topic per pair of correspondents, so what the user wrote to an
//! agent belongs in the topic it started between the user and that agent.
//! Remove this once the user's GUIs have run it (`temp-migration` skill).

use std::collections::BTreeMap;

use rho_agent_hosts::HostId;
use rho_agent_types::{AgentId, ContentPart, StreamId, TopicId, UnixMs};

use crate::chat::{ChatMap, HostChat, HostTopic, default_topic_name};
use crate::outbox::{Destination, Outgoing as ChatOutgoing};

/// `main`'s unsent message to an agent, as its outbox stored it. The name
/// is the one `main`'s table records.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Outgoing {
    pub at: UnixMs,
    pub content: Vec<ContentPart>,
}

/// What is left of `main`'s tables: the user's words not carried yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Leftovers {
    pub drafts: BTreeMap<AgentId, String>,
    /// By agent and the id `main` gave each, oldest first per agent.
    pub outbox: BTreeMap<AgentId, Vec<(u64, Outgoing)>>,
}

impl Leftovers {
    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty() && self.outbox.is_empty()
    }
}

/// Where some of the leftovers go, written in one transaction with their
/// removal from `main`'s tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Carried {
    /// An agent's draft, now its topic's.
    pub drafts: Vec<(AgentId, HostTopic, String)>,
    /// An agent's unsent messages, by the ids `main` gave them, now one
    /// message to its host under the first of them.
    pub outbox: Vec<(AgentId, Vec<u64>, u64, u64, ChatOutgoing)>,
}

impl Carried {
    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty() && self.outbox.is_empty()
    }
}

/// The topic the host's migration started between the user and `agent`:
/// the oldest in the default stream that began with that agent alone. A
/// topic between two agents began with both.
pub fn user_topic(chat: &HostChat, agent: AgentId) -> Option<TopicId> {
    chat.topics
        .iter()
        .find(|(id, topic)| {
            topic.stream == StreamId::DEFAULT
                && topic
                    .agents
                    .iter()
                    .filter(|(_, since)| **since == id.0)
                    .map(|(agent, _)| *agent)
                    .eq([agent])
        })
        .map(|(id, _)| *id)
}

/// What can be carried now. An agent's words wait until its host is known
/// and has told its whole chat, so a topic not heard of yet is never taken
/// for one that does not exist. A draft goes to the agent's topic with the
/// user; unsent messages are posted there, or start one with the agent
/// when there is none. An agent's messages go as one, so they start one
/// topic, not one each.
pub fn carry(
    chat: &ChatMap,
    host_of: impl Fn(AgentId) -> Option<HostId>,
    leftovers: &Leftovers,
) -> Carried {
    let told = |agent: AgentId| {
        let (machine_seed, held) = chat.told_chat(host_of(agent)?)?;
        Some((machine_seed, user_topic(held, agent)))
    };
    let mut carried = Carried::default();
    for (agent, text) in &leftovers.drafts {
        if let Some((machine_seed, Some(topic))) = told(*agent) {
            carried.drafts.push((
                *agent,
                HostTopic {
                    machine_seed,
                    topic,
                },
                text.clone(),
            ));
        }
    }
    for (agent, unsent) in &leftovers.outbox {
        let Some((machine_seed, topic)) = told(*agent) else {
            continue;
        };
        let Some((first_id, first)) = unsent.first() else {
            continue;
        };
        let content = unsent
            .iter()
            .flat_map(|(_, outgoing)| outgoing.content.iter().cloned())
            .collect::<Vec<_>>();
        let to = match topic {
            Some(topic) => Destination::Topic(topic),
            None => Destination::NewTopic {
                stream: StreamId::DEFAULT,
                name: default_topic_name(&rho_agent_types::transcript::text_content(&content)),
                agents: vec![*agent],
            },
        };
        carried.outbox.push((
            *agent,
            unsent.iter().map(|(id, _)| *id).collect(),
            machine_seed,
            *first_id,
            ChatOutgoing {
                at: first.at,
                content,
                to,
            },
        ));
    }
    carried
}

impl Leftovers {
    /// Takes out what was carried.
    pub fn without(&mut self, carried: &Carried) {
        for (agent, ..) in &carried.drafts {
            self.drafts.remove(agent);
        }
        for (agent, ..) in &carried.outbox {
            self.outbox.remove(agent);
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{Address, AgentIdDomain, ChatEvent, ChatRecord, ChatSeq, Mention};

    use super::*;

    const SEED: u64 = 7;
    const HOST: HostId = HostId(1);
    const ELSEWHERE: HostId = HostId(2);

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &AgentIdDomain(0)).expect("an agent id")
    }

    fn text(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }

    fn started(seq: u64, by: Address, agents: Vec<AgentId>) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event: ChatEvent::TopicStarted {
                stream: StreamId::DEFAULT,
                name: format!("topic {seq}"),
                by,
                agents,
            },
        }
    }

    fn posted(seq: u64, topic: u64, from: Address, to: Address) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event: ChatEvent::Posted {
                topic: TopicId(ChatSeq(topic)),
                from,
                mentions: vec![Mention {
                    to,
                    kind: rho_agent_types::MentionKind::Result,
                }],
                content: text("hello"),
            },
        }
    }

    /// What the host's migration makes of agents 1 and 2 writing to each
    /// other first, then each to the user: the pair of agents gets the
    /// oldest topic, which both are in, and agent 2 later posts in agent
    /// 1's topic with the user. After the migration the user starts a
    /// newer topic with agent 1.
    fn migrated(told: bool) -> ChatMap {
        let (one, two) = (agent(1), agent(2));
        let mut chat = HostChat::default();
        for record in [
            ChatRecord {
                seq: ChatSeq(1),
                at: UnixMs(1),
                event: ChatEvent::StreamCreated { name: "rho".into() },
            },
            started(2, Address::Agent(one), vec![one, two]),
            started(3, Address::Agent(two), vec![two]),
            started(4, Address::User, vec![one]),
            posted(5, 4, Address::Agent(two), Address::User),
            started(6, Address::User, vec![one]),
        ] {
            chat.tell(&record);
        }
        let mut map = ChatMap::default();
        map.load(HOST, Some(SEED), chat);
        if told {
            map.merge(HOST, HostChat::default());
        }
        map
    }

    fn on_host(_: AgentId) -> Option<HostId> {
        Some(HOST)
    }

    fn in_topic(seq: u64) -> HostTopic {
        HostTopic {
            machine_seed: SEED,
            topic: TopicId(ChatSeq(seq)),
        }
    }

    /// Each agent's draft goes to the topic its pair with the user began
    /// in: not the agents' topic, the older one both are in, not one the
    /// other agent joined, and not the user's newer topic with it.
    #[test]
    fn a_draft_goes_to_the_agents_topic_with_the_user() {
        let leftovers = Leftovers {
            drafts: [(agent(1), "to one".into()), (agent(2), "to two".into())].into(),
            outbox: BTreeMap::new(),
        };
        let carried = carry(&migrated(true), on_host, &leftovers);
        assert_eq!(
            carried.drafts,
            vec![
                (agent(1), in_topic(4), "to one".into()),
                (agent(2), in_topic(3), "to two".into()),
            ]
        );
        assert!(carried.outbox.is_empty());
    }

    /// Unsent messages are posted in the agent's topic with the user, an
    /// agent's together under its first id, oldest first. An agent with
    /// no such topic gets one started with it alone.
    #[test]
    fn unsent_messages_are_posted_in_the_agents_topic_with_the_user() {
        let message = |at, words: &str| Outgoing {
            at: UnixMs(at),
            content: text(words),
        };
        let leftovers = Leftovers {
            drafts: BTreeMap::new(),
            outbox: [
                (
                    agent(1),
                    vec![(11, message(100, "one a")), (12, message(101, "one b"))],
                ),
                (agent(2), vec![(21, message(102, "two"))]),
                (agent(3), vec![(31, message(103, "three"))]),
            ]
            .into(),
        };
        let carried = carry(&migrated(true), on_host, &leftovers);
        let posted = |agent: AgentId| {
            carried
                .outbox
                .iter()
                .find(|(of, ..)| *of == agent)
                .cloned()
                .expect("carried")
        };
        assert_eq!(
            posted(agent(1)),
            (
                agent(1),
                vec![11, 12],
                SEED,
                11,
                ChatOutgoing {
                    at: UnixMs(100),
                    content: [text("one a"), text("one b")].concat(),
                    to: Destination::Topic(TopicId(ChatSeq(4))),
                }
            )
        );
        assert_eq!(
            posted(agent(2)).4.to,
            Destination::Topic(TopicId(ChatSeq(3)))
        );
        assert_eq!(
            posted(agent(3)).4.to,
            Destination::NewTopic {
                stream: StreamId::DEFAULT,
                name: "three".into(),
                agents: vec![agent(3)],
            }
        );
    }

    /// Nothing moves until the agent's host is known and has told its
    /// whole chat: the disk copy alone might lack the topic, and an
    /// unsent message would start a second one.
    #[test]
    fn words_wait_for_their_hosts_chat() {
        let leftovers = Leftovers {
            drafts: [(agent(1), "to one".into())].into(),
            outbox: [(
                agent(3),
                vec![(
                    31,
                    Outgoing {
                        at: UnixMs(1),
                        content: text("three"),
                    },
                )],
            )]
            .into(),
        };
        assert!(carry(&migrated(false), on_host, &leftovers).is_empty());
        assert!(carry(&migrated(true), |_| None, &leftovers).is_empty());
        assert!(carry(&migrated(true), |_| Some(ELSEWHERE), &leftovers).is_empty());
        let mut left = leftovers.clone();
        let carried = carry(&migrated(true), on_host, &leftovers);
        left.without(&carried);
        assert!(left.is_empty());
    }
}
