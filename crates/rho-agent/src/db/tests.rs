use std::borrow::Cow;
use std::collections::HashSet;

use rho_core::{ContentPart, UnixMs};
use rho_db::{RhoDb, SenValue};
use rho_inference::PromptCacheKey;
use rho_workspaces::WorkspaceInfo;

use super::*;

/// Tests exercise agent records only; any workspace info will do.
fn test_workspace() -> WorkspaceInfo {
    WorkspaceInfo::Workspace {
        repo: "/home/user/src/rho".into(),
        name: "test-agent".to_owned(),
    }
}

fn test_agent_runtime() -> AgentRuntime {
    AgentRuntime::Rho {
        prompt_cache_key: PromptCacheKey::generate(),
    }
}

#[test]
fn agent_db_migrations_eventually_reach_current_format() {
    let current = CURRENT_AGENT_DB_FORMAT;
    let mut starts = HashSet::new();
    for migration in AGENT_DB_MIGRATIONS {
        assert!(
            starts.insert(migration.from),
            "duplicate agent db migration from {}",
            migration.from
        );
    }

    for &start in &starts {
        let mut seen = HashSet::new();
        let mut format = start;
        while format != current {
            assert!(
                seen.insert(format),
                "agent db migrations cycle before reaching current format: {format}"
            );
            let next = AGENT_DB_MIGRATIONS
                .iter()
                .find(|candidate| candidate.from == format)
                .unwrap_or_else(|| {
                    panic!("agent db migration chain from {start} stops at {format}")
                });
            format = next.to;
        }
    }
}

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
async fn init_agent_tables_stamps_current_db_format() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    write.commit();

    let format = db.read().open_table(FORMAT).get(&()).unwrap().value();
    assert_eq!(format, CURRENT_AGENT_DB_FORMAT);
}

#[tokio::test]
#[should_panic(expected = "Update rho one version at a time")]
async fn init_agent_tables_rejects_unsupported_db_format() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.open_table(FORMAT).insert(&(), &"deadbeef".to_owned());
    write.init_agent_tables();
}

#[tokio::test]
async fn create_agent_and_append_events_with_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let topic_id = write.create_topic(UnixMs(1), "default".to_owned(), Status::Normal);
    let agent_id = write.alloc_agent_id();
    let next = write.create_agent(
        UnixMs(1),
        agent_id,
        topic_id,
        Some("main".to_owned()),
        test_workspace(),
        AgentMode::deep_default(),
        test_agent_runtime(),
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
    write.init_agent_tables();
    let topic_id = write.create_topic(UnixMs(1), "default".to_owned(), Status::Normal);
    let agent_id = write.alloc_agent_id();
    let next = write.create_agent(
        UnixMs(1),
        agent_id,
        topic_id,
        Some("main".to_owned()),
        test_workspace(),
        AgentMode::deep_default(),
        test_agent_runtime(),
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

#[tokio::test]
async fn move_agent_to_topic_repoints_membership() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let default_topic = write.create_topic(UnixMs(1), "default".to_owned(), Status::Normal);
    let named_topic = write.create_topic(UnixMs(1), "infra".to_owned(), Status::Normal);
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        default_topic,
        None,
        test_workspace(),
        AgentMode::deep_default(),
        test_agent_runtime(),
    );
    write.move_agent_to_topic(agent_id, named_topic);
    write.commit();

    let read = db.read();
    assert_eq!(read.list_topic_agents(default_topic), []);
    assert_eq!(read.list_topic_agents(named_topic), [agent_id]);

    // Moving to the topic it is already in is a no-op, not a duplicate.
    let mut write = db.write().await;
    write.move_agent_to_topic(agent_id, named_topic);
    write.commit();
    assert_eq!(db.read().list_topic_agents(named_topic), [agent_id]);
}

#[tokio::test]
async fn topic_and_agent_statuses_are_settable() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let topic_id = write.create_topic(UnixMs(1), "infra".to_owned(), Status::Normal);
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        topic_id,
        None,
        test_workspace(),
        AgentMode::deep_default(),
        test_agent_runtime(),
    );
    write.set_topic_status(UnixMs(2), topic_id, Status::Pinned);
    write.set_topic_name(UnixMs(3), topic_id, "platform".to_owned());
    write.set_agent_status(UnixMs(2), agent_id, Status::Archived);
    write.set_agent_display_name(UnixMs(4), agent_id, "builder".to_owned());
    write.commit();

    let read = db.read();
    let topic = read.get_topic(topic_id);
    assert_eq!(topic.name, "platform");
    assert_eq!(topic.status, Status::Pinned);
    assert_eq!(topic.updated_at, UnixMs(3));
    let agent = read.get_agent(agent_id);
    assert_eq!(agent.status, Status::Archived);
    assert_eq!(agent.display_name.as_deref(), Some("builder"));
    assert_eq!(agent.updated_at, UnixMs(4));
}

#[tokio::test]
async fn workdirs_upsert_by_path_and_remove() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    write.upsert_workdir(UnixMs(1), "/home/user/src/rho", "rho".to_owned());
    write.upsert_workdir(UnixMs(2), "/home/user/src/zed", "zed".to_owned());
    // Re-adding the same path renames it and keeps created_at.
    write.upsert_workdir(UnixMs(3), "/home/user/src/rho", "rho-main".to_owned());
    write.commit();

    let workdirs = db.read().list_workdirs();
    assert_eq!(workdirs.len(), 2);
    let rho = workdirs
        .iter()
        .find(|(path, _)| path == std::path::Path::new("/home/user/src/rho"))
        .unwrap();
    assert_eq!(rho.1.name, "rho-main");
    assert_eq!(rho.1.created_at, UnixMs(1));

    let mut write = db.write().await;
    write.remove_workdir("/home/user/src/zed");
    write.commit();
    assert_eq!(db.read().list_workdirs().len(), 1);
}

#[tokio::test]
async fn agent_ids_allocate_before_records_exist() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    // Only the second allocation gets a record, as when the first jj
    // checkout fails.
    let leaked_id = write.alloc_agent_id();
    let agent_id = write.alloc_agent_id();
    assert_ne!(leaked_id, agent_id);
    let topic_id = write.create_topic(UnixMs(1), "default".to_owned(), Status::Normal);
    write.create_agent(
        UnixMs(2),
        agent_id,
        topic_id,
        None,
        test_workspace(),
        AgentMode::deep_default(),
        test_agent_runtime(),
    );
    write.commit();

    let read = db.read();
    assert_eq!(read.get_agent(agent_id).workspace, test_workspace());
    assert_eq!(read.list_agents().len(), 1);
}
