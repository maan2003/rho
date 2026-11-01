//! Temporary: carries what the user said about agents before chat onto the
//! topics their conversations became (`temp-migration` skill). Remove it
//! once the user's devices have run it against their migrated hosts.
//!
//! A host moved from `main` puts each conversation between the user and an
//! agent into one topic, and every message to the user that asked or
//! answered becomes a mention of them. Unread, those mentions would deal
//! every old conversation again. What the GUI holds about them is the
//! ledger's facts on the agent itself (`NodeId::Agent`, from `main`): when
//! the user settled it or made it a todo, and whether they muted it. Their seen
//! positions count in logs that are gone, so the time each was said is what
//! carries over: a mention from before it was handled.

use rho_agent_hosts::HostId;
use rho_agent_types::AgentId;
use rho_agents_client::ChatMap;
use rho_agents_client::from_main::MIGRATED_STREAM;
use rho_agents_client::protocol::worksets::ChangeTopic;
use rho_dealer::NodeId;
use rho_dealer::facts::{Entry, Fact, Seen};
use rho_dealer::marks::Marks;

/// The facts that settle each topic the user already handled through its
/// agents. A topic the user said anything about is theirs and left alone,
/// which also makes this once per topic.
pub(crate) fn settle_migrated(chat: &ChatMap, marks: &Marks) -> Vec<Fact> {
    let mut facts = Vec::new();
    for (topic, _, held) in chat.topics() {
        let node = NodeId::Topic(topic);
        if !marks.get(&node).facts.is_empty() {
            continue;
        }
        let said = marks
            .nodes()
            .filter(|(old, _)| match old {
                NodeId::Agent(agent) => held.agents().any(|held| held == *agent),
                _ => false,
            })
            .collect::<Vec<_>>();
        if said.is_empty() {
            continue;
        }
        // An agent the user muted reached them with nothing; its topics
        // do the same.
        let muted = held
            .agents()
            .all(|agent| marks.get(&NodeId::Agent(agent)).facts().muted());
        if muted {
            facts.push(Fact::Mute { node });
            continue;
        }
        let Some(handled) = said
            .iter()
            .flat_map(|(_, marks)| &marks.facts)
            .filter(|entry| matches!(entry.fact, Fact::Settled { .. } | Fact::Todo { .. }))
            .max_by_key(|entry| entry.id())
        else {
            continue;
        };
        let handled_ms = handled.at.timestamp().as_millisecond();
        let Some(seen) = held
            .mentions
            .iter()
            .filter(|mention| mention.at.0 as i64 <= handled_ms)
            .map(|mention| mention.message)
            .max()
        else {
            continue;
        };
        // Past the user's own words in it, it says nothing new.
        if held.seen(None) >= Some(seen) {
            continue;
        }
        let seen = Seen::Topic(seen.0.0);
        facts.push(match handled {
            Entry {
                fact: Fact::Todo { start, .. },
                ..
            } => Fact::Todo {
                node,
                start: *start,
                seen,
            },
            _ => Fact::Settled { node, seen },
        });
    }
    facts
}

/// Each migrated topic named as the user named its agent in `main`. The
/// host named it as the host knows the agent (`host_name`); a topic whose
/// name is anything else was renamed since, and is left alone, which also
/// makes this once per topic.
pub(crate) fn names_from_desk(
    chat: &ChatMap,
    marks: &Marks,
    host_name: impl Fn(AgentId) -> Option<String>,
) -> Vec<(HostId, ChangeTopic)> {
    let mut changes = Vec::new();
    for (topic, host, held) in chat.topics() {
        if chat.stream_name(topic) != Some(MIGRATED_STREAM) {
            continue;
        }
        let Some(&agent) = held.members_in_order().first() else {
            continue;
        };
        let Some(name) = marks
            .get(&NodeId::Agent(agent))
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        if name == held.name || host_name(agent).as_deref().map(str::trim) != Some(held.name.trim()) {
            continue;
        }
        changes.push((
            host,
            ChangeTopic {
                topic: topic.topic,
                stream: held.stream,
                name: name.to_owned(),
                resolved: held.resolved,
            },
        ));
    }
    changes
}

#[cfg(test)]
mod tests {
    use rho_agent_hosts::HostId;
    use rho_agent_types::{
        Address, AgentId, AgentIdDomain, ChatEvent, ChatRecord, ChatSeq, ContentPart, Mention,
        MentionKind, StreamId, TopicId, UnixMs,
    };
    use rho_agents_client::{HostChat, HostTopic};
    use rho_dealer::facts::Device;

    use super::*;

    const SEED: u64 = 7;
    const HOUR: u64 = 3_600_000;

    fn agent(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &AgentIdDomain(SEED)).expect("an agent id")
    }

    /// A migrated host: one topic per agent, each with a question at hour
    /// 1 and an answer at hour 3, both mentioning the user.
    fn migrated(agents: &[AgentId]) -> ChatMap {
        let mut log = HostChat::default();
        let mut seq = 1;
        let mut tell = |at: u64, event: ChatEvent| {
            log.tell(&ChatRecord {
                seq: ChatSeq(seq),
                at: UnixMs(at),
                event,
            });
            seq += 1;
            ChatSeq(seq - 1)
        };
        let stream = StreamId(tell(0, ChatEvent::StreamCreated { name: "rho".into() }));
        for agent in agents {
            let topic = TopicId(tell(
                0,
                ChatEvent::TopicStarted {
                    stream,
                    name: "old".into(),
                    by: Address::User,
                },
            ));
            tell(
                0,
                ChatEvent::Joined {
                    topic,
                    agent: *agent,
                },
            );
            for (at, kind) in [(HOUR, MentionKind::Ask), (3 * HOUR, MentionKind::Result)] {
                tell(
                    at,
                    ChatEvent::Posted {
                        topic,
                        from: Address::Agent(*agent),
                        mentions: vec![Mention {
                            to: Address::User,
                            kind,
                        }],
                        content: vec![ContentPart::Text { text: "hi".into() }],
                    },
                );
            }
        }
        let mut chat = ChatMap::default();
        chat.load(HostId::default(), Some(SEED), log);
        chat
    }

    fn said(at_ms: u64, fact: Fact) -> Entry {
        Entry {
            device: Device([1; 16]),
            at: jiff::Timestamp::from_millisecond(at_ms as i64)
                .expect("a time")
                .to_zoned(jiff::tz::TimeZone::UTC),
            fact,
        }
    }

    fn topic(chat: &ChatMap, agent: AgentId) -> HostTopic {
        chat.newest_topic_of(HostId::default(), agent)
            .expect("the agent's topic")
    }

    fn message(chat: &ChatMap, agent: AgentId, nth: usize) -> u64 {
        chat.topic(topic(chat, agent))
            .expect("the topic")
            .1
            .mentions[nth]
            .message
            .0
            .0
    }

    /// Settled between the question and the answer: the question is seen,
    /// the answer that came after still deals. Settled after both: both.
    #[test]
    fn a_mention_from_before_the_agent_was_settled_is_seen() {
        let (early, late) = (agent(1), agent(2));
        let chat = migrated(&[early, late]);
        let mut marks = Marks::default();
        marks.apply([
            said(
                2 * HOUR,
                Fact::Settled {
                    node: NodeId::Agent(early),
                    seen: Seen::Agent(40),
                },
            ),
            said(
                4 * HOUR,
                Fact::Settled {
                    node: NodeId::Agent(late),
                    seen: Seen::Agent(9),
                },
            ),
        ]);
        let mut facts = settle_migrated(&chat, &marks);
        facts.sort_by_key(|fact| format!("{fact:?}"));
        let mut expected = vec![
            Fact::Settled {
                node: NodeId::Topic(topic(&chat, early)),
                seen: Seen::Topic(message(&chat, early, 0)),
            },
            Fact::Settled {
                node: NodeId::Topic(topic(&chat, late)),
                seen: Seen::Topic(message(&chat, late, 1)),
            },
        ];
        expected.sort_by_key(|fact| format!("{fact:?}"));
        assert_eq!(facts, expected);
    }

    /// The last thing said wins: a todo after a settle carries over as a
    /// todo, and a settle that was taken back (retracted entries never
    /// reach the marks) carries nothing.
    #[test]
    fn the_last_handling_carries_over_as_itself() {
        let todo = agent(1);
        let chat = migrated(&[todo, agent(2)]);
        let mut marks = Marks::default();
        marks.apply([
            said(
                HOUR / 2,
                Fact::Settled {
                    node: NodeId::Agent(todo),
                    seen: Seen::Agent(1),
                },
            ),
            said(
                5 * HOUR,
                Fact::Todo {
                    node: NodeId::Agent(todo),
                    start: None,
                    seen: Seen::Agent(2),
                },
            ),
        ]);
        assert_eq!(
            settle_migrated(&chat, &marks),
            vec![Fact::Todo {
                node: NodeId::Topic(topic(&chat, todo)),
                start: None,
                seen: Seen::Topic(message(&chat, todo, 1)),
            }],
            "the agent nobody said anything about keeps its cards"
        );
    }

    /// A muted agent's topic is muted; a topic the user already said
    /// something about is left as they said it, so a second run writes
    /// nothing.
    #[test]
    fn a_muted_agent_mutes_its_topic_and_a_handled_topic_is_left_alone() {
        let (muted, handled) = (agent(1), agent(2));
        let chat = migrated(&[muted, handled]);
        let mut marks = Marks::default();
        marks.apply([
            said(
                HOUR,
                Fact::Mute {
                    node: NodeId::Agent(muted),
                },
            ),
            said(
                4 * HOUR,
                Fact::Settled {
                    node: NodeId::Agent(handled),
                    seen: Seen::Agent(3),
                },
            ),
            said(
                5 * HOUR,
                Fact::Snooze {
                    node: NodeId::Topic(topic(&chat, handled)),
                    until: rho_dealer::Until::In(jiff::SignedDuration::from_hours(1)),
                },
            ),
        ]);
        let facts = settle_migrated(&chat, &marks);
        assert_eq!(
            facts,
            vec![Fact::Mute {
                node: NodeId::Topic(topic(&chat, muted)),
            }]
        );
        marks.apply(facts.into_iter().map(|fact| said(7 * HOUR, fact)));
        assert_eq!(settle_migrated(&chat, &marks), Vec::new());
    }

    /// A topic takes the agent's name from the desk only while it still has
    /// the name the host gave it.
    #[test]
    fn a_topic_takes_the_name_the_user_gave_its_agent() {
        let (named, renamed, unnamed) = (agent(1), agent(2), agent(3));
        let chat = migrated(&[named, renamed, unnamed]);
        let mut marks = Marks::default();
        for agent in [named, renamed] {
            marks.apply([said(
                HOUR,
                Fact::Named {
                    node: NodeId::Agent(agent),
                    name: Some(" python-core ".into()),
                },
            )]);
        }
        // The host still calls `renamed` "old", but the user renamed its
        // topic since: here, the host's name for it moved on.
        let host_name = |agent: AgentId| Some(if agent == renamed { "new" } else { "old" }.to_owned());
        let changes = names_from_desk(&chat, &marks, host_name);
        assert_eq!(
            changes
                .iter()
                .map(|(_, change)| (change.topic, change.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(topic(&chat, named).topic, "python-core")]
        );
    }
}
