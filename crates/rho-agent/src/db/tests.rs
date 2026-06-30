use std::borrow::Cow;

use rho_core::{ContentPart, UnixMs};
use rho_db::{RhoDb, SenValue};
use rho_inference::PromptCacheKey;
use rho_inference::config::InferenceConfig;

use super::*;

#[tokio::test]
async fn agent_event_positions_sort_by_lineage_then_seq() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    {
        let mut timeline = write.open_table(AGENT_EVENTS);
        for seq in [2, 0, 1] {
            timeline.insert(
                &AgentEventPos {
                    lineage_id: AgentLineageId(7),
                    seq,
                },
                SenValue::owned(AgentEvent::UserMessage {
                    content: Cow::Owned(Vec::new()),
                }),
            );
        }
    }
    write.commit();

    let read = db.read();
    let timeline = read.open_table(AGENT_EVENTS);
    let seqs = timeline
        .range(
            AgentEventPos {
                lineage_id: AgentLineageId(7),
                seq: 0,
            }..=AgentEventPos {
                lineage_id: AgentLineageId(7),
                seq: u32::MAX,
            },
        )
        .map(|(key, _)| key.value().seq)
        .collect::<Vec<_>>();

    assert_eq!(seqs, [0, 1, 2]);
}

#[tokio::test]
async fn create_agent_and_append_events_with_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    let topic_id = write.create_topic(UnixMs(1), None, TopicStatus::Normal);
    let (agent_id, next) = write.create_agent(
        UnixMs(1),
        topic_id,
        Some("main".to_owned()),
        PromptCacheKey::generate(),
        InferenceConfig::deep().protect(),
    );
    let next = write.append_agent_event(
        next,
        &AgentEvent::UserMessage {
            content: Cow::Owned(vec![ContentPart::Text {
                text: "hello".to_owned(),
            }]),
        },
    );
    write.append_agent_event(
        next,
        &AgentEvent::UserMessage {
            content: Cow::Owned(vec![ContentPart::Text {
                text: "again".to_owned(),
            }]),
        },
    );
    write.commit();

    let read = db.read();
    let agent = read.get_agent(agent_id);
    assert_eq!(agent.display_name.as_deref(), Some("main"));
    assert_eq!(read.list_topic_agents(topic_id), [agent_id]);

    let (next, events) = read.agent_events(agent_id);
    assert_eq!(next.seq, 2);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0],
        AgentEvent::UserMessage {
            content: Cow::Owned(vec![ContentPart::Text {
                text: "hello".to_owned(),
            }]),
        }
    );
}

#[tokio::test]
async fn agent_events_read_lineage_parents() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    let topic_id = write.create_topic(UnixMs(1), None, TopicStatus::Normal);
    let (agent_id, next) = write.create_agent(
        UnixMs(1),
        topic_id,
        Some("main".to_owned()),
        PromptCacheKey::generate(),
        InferenceConfig::deep().protect(),
    );
    let fork_at = write.append_agent_event(
        next,
        &AgentEvent::UserMessage {
            content: Cow::Owned(vec![ContentPart::Text {
                text: "parent".to_owned(),
            }]),
        },
    );
    write.append_agent_event(
        fork_at,
        &AgentEvent::UserMessage {
            content: Cow::Owned(vec![ContentPart::Text {
                text: "sibling".to_owned(),
            }]),
        },
    );

    let child_lineage = AgentLineageId(99);
    {
        write
            .open_table(LINEAGE_PARENTS)
            .insert(&child_lineage, &fork_at);
    }
    {
        let mut agents = write.open_table(AGENTS);
        let mut agent = agents.get(&agent_id).unwrap().value().into_owned();
        agent.current_lineage = child_lineage;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }
    write.append_agent_event(
        AgentEventPos::root(child_lineage),
        &AgentEvent::UserMessage {
            content: Cow::Owned(vec![ContentPart::Text {
                text: "child".to_owned(),
            }]),
        },
    );
    write.commit();

    let read = db.read();
    let (next, events) = read.agent_events(agent_id);
    assert_eq!(next.lineage_id, child_lineage);
    assert_eq!(next.seq, 1);
    let texts = events
        .into_iter()
        .map(|event| match event {
            AgentEvent::UserMessage { content } => match &content[0] {
                ContentPart::Text { text } => text.clone(),
            },
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["parent", "child"]);
}
