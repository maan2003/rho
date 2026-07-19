use std::collections::HashSet;
use std::sync::Arc;

use rho_core::{ContentPart, UnixMs};
use rho_db::{RhoDb, SenValue};
use rho_inference::PromptCacheKey;
use rho_workspaces::WorkspaceInfo;

use super::*;

#[test]
fn agent_role_resolves_opinionated_bindings() {
    let profile = |intelligence| {
        AgentRole::Engineer { intelligence }
            .session_profile()
            .unwrap()
    };
    assert!(matches!(
        profile(EngineerIntelligence::Mini),
        SessionBinding::ResponsesLuna(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: true,
            code_mode: false,
        })
    ));
    assert!(matches!(
        profile(EngineerIntelligence::Low),
        SessionBinding::ResponsesTerra(InferenceProfile {
            effort: ReasoningEffort::Low,
            ..
        })
    ));
    assert!(matches!(
        profile(EngineerIntelligence::Medium),
        SessionBinding::ResponsesSol(InferenceProfile {
            effort: ReasoningEffort::Medium,
            ..
        })
    ));
    assert!(matches!(
        AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Medium,
            workflow: AgentWorkflow::PrFriendly,
        }
        .session_profile()
        .unwrap(),
        SessionBinding::ResponsesSol(InferenceProfile {
            effort: ReasoningEffort::High,
            ..
        })
    ));
    assert!(matches!(
        profile(EngineerIntelligence::High),
        SessionBinding::ResponsesSol(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            ..
        })
    ));
    for intelligence in [
        EngineerIntelligence::Low,
        EngineerIntelligence::Medium,
        EngineerIntelligence::High,
    ] {
        assert!(profile(intelligence).deep_config().unwrap().code_mode);
    }
    assert_eq!(
        profile(EngineerIntelligence::Ultra),
        SessionBinding::ClaudeFable {
            effort: ClaudeEffort::High
        }
    );
    assert_eq!(
        AgentRole::Advisor {
            intelligence: AdvisorIntelligence::High,
        }
        .session_profile()
        .unwrap(),
        SessionBinding::ClaudeAdvisor {
            effort: ClaudeEffort::High
        }
    );
    assert!(matches!(
        AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Medium,
        }
        .session_profile()
        .unwrap(),
        SessionBinding::AdvisorSol(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: false,
            ..
        })
    ));
    assert!(matches!(
        AgentRole::pm().session_profile().unwrap(),
        SessionBinding::CoordinatorSol(InferenceProfile {
            effort: ReasoningEffort::Low,
            code_mode: false,
            ..
        })
    ));
}

use crate::{MessageDelivery, MessageSender, QueuedItem, QueuedItemKind};

fn user_event(text: &str) -> AgentEvent<'static> {
    AgentEvent::Queued(QueuedItem {
        kind: QueuedItemKind::UserMessage {
            sender: MessageSender::User,
            content: Arc::new(vec![ContentPart::Text {
                text: text.to_owned(),
            }]),
            source_id: None,
        },
        delivery: MessageDelivery::Immediate,
    })
}

fn event_text(event: &AgentEvent<'_>) -> String {
    match event {
        AgentEvent::Queued(QueuedItem {
            kind: QueuedItemKind::UserMessage { content, .. },
            ..
        }) => match &content[0] {
            ContentPart::Text { text } => text.clone(),
        },
        _ => unreachable!(),
    }
}

/// Tests exercise agent records only; any workspace info will do.
fn test_workspace() -> WorkspaceInfo {
    WorkspaceInfo::Workspace {
        repo: "/home/user/src/rho".into(),
        id: WorkspaceId::from_counter(1, &WorkspaceIdDomain(0)).unwrap(),
    }
}

fn test_agent_runtime() -> AgentRuntime {
    AgentRuntime::Rho {
        prompt_cache_key: PromptCacheKey::generate(),
    }
}

#[tokio::test]
async fn workstream_names_are_uniquified_by_suffix() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let first = write.create_workstream(UnixMs(1), "team".to_owned());
    let second = write.create_workstream(UnixMs(2), "team".to_owned());
    let third = write.create_workstream(UnixMs(3), "crew".to_owned());
    // Renaming onto a taken name suffixes too; renaming to your own name
    // does not.
    write.set_workstream_name(UnixMs(4), third, "team".to_owned());
    write.set_workstream_name(UnixMs(5), first, "team".to_owned());
    write.commit();

    let read = db.read();
    assert_eq!(read.get_workstream(first).name, "team");
    assert_eq!(read.get_workstream(second).name, "team-2");
    assert_eq!(read.get_workstream(third).name, "team-3");
}

#[tokio::test]
async fn labels_toggle_without_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let workstream = write.create_workstream(UnixMs(1), "team".to_owned());
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    write.workstream_label(UnixMs(2), workstream, "pin", true);
    write.workstream_label(UnixMs(3), workstream, "pin", true);
    write.workstream_label(UnixMs(4), workstream, "group:slack", true);
    write.agent_label(UnixMs(5), agent_id, "urgent", true);
    write.agent_label(UnixMs(6), agent_id, "urgent", true);
    write.agent_label(UnixMs(7), agent_id, "review", true);
    write.agent_label(UnixMs(8), agent_id, "urgent", false);
    write.commit();

    let read = db.read();
    assert_eq!(
        read.get_workstream(workstream).labels,
        ["pin", "group:slack"]
    );
    assert_eq!(read.get_agent(agent_id).labels, ["review"]);
}

#[tokio::test]
async fn agent_spawned_by_is_stored_at_creation() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let workstream = write.create_workstream(UnixMs(1), "team".to_owned());
    let pm = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        pm,
        workstream,
        None,
        vec![test_workspace()],
        AgentRole::pm().session_profile().unwrap(),
        test_agent_runtime(),
        None,
    );
    let engineer = write.alloc_agent_id();
    write.create_agent(
        UnixMs(2),
        engineer,
        workstream,
        None,
        vec![test_workspace()],
        AgentRole::default().session_profile().unwrap(),
        test_agent_runtime(),
        Some(pm),
    );
    write.commit();

    assert_eq!(db.read().get_agent(pm).spawned_by, AgentSpawnedBy::Direct);
    assert_eq!(db.read().get_agent(engineer).spawned_by, AgentSpawnedBy::PM);
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

/// The pre-workstream on-disk shape of [`AgentRecord`]; senax field ids
/// hash field names, so a same-named-fields struct produces bytes the
/// current decoder must accept.
#[derive(Encode)]
struct PreWorkstreamAgentRecord {
    display_name: Option<String>,
    workdirs: Vec<WorkspaceInfo>,
    status: Status,
    created_at: UnixMillis,
    updated_at: UnixMillis,
    current_lineage: AgentLineageId,
    parent_agent: Option<AgentId>,
    spawned_by: AgentSpawnedBy,
    role: AgentRole,
    binding: SessionBinding,
    runtime: AgentRuntime,
    last_user_message: UnixMillis,
    disposition: AgentDisposition,
    tags: Vec<TagId>,
}

fn pre_workstream_agent(
    display_name: Option<&str>,
    parent_agent: Option<AgentId>,
    status: Status,
    tags: Vec<TagId>,
) -> PreWorkstreamAgentRecord {
    PreWorkstreamAgentRecord {
        display_name: display_name.map(str::to_owned),
        workdirs: vec![test_workspace()],
        status,
        created_at: UnixMs(1),
        updated_at: UnixMs(1),
        current_lineage: AgentLineageId(1),
        parent_agent,
        spawned_by: AgentSpawnedBy::Direct,
        role: AgentRole::default(),
        binding: SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        runtime: test_agent_runtime(),
        last_user_message: UnixMs(1),
        disposition: AgentDisposition::Pending,
        tags,
    }
}

#[tokio::test]
async fn migration_turns_tags_into_workstreams_and_labels() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let agent_id = |counter| AgentId::from_counter(counter, &AgentIdDomain(0)).unwrap();
    let (tagged, child, orphan) = (agent_id(1), agent_id(2), agent_id(3));

    // A pre-migration database: a pinned workstream tag inside a group, a
    // label tag, and three agents — one tagged, one child inheriting via
    // its parent, one with no workstream tag at all.
    let mut write = db.write().await;
    write.open_table(FORMAT).insert(&(), &"f3b8d24a".to_owned());
    write
        .open_table(COUNTERS)
        .insert(&CounterKey::LAST_WORKSTREAM_ID, &3u64);
    {
        let mut tags = write.open_table(LEGACY_TAGS);
        let tag = |name: &str, kind, parent, status| TagRecord {
            name: name.to_owned(),
            kind,
            parent,
            created_at: UnixMs(1),
            updated_at: UnixMs(2),
            status,
        };
        tags.insert(
            &TagId(1),
            SenValue::borrowed(&tag(
                "team",
                TagKind::Workstream,
                Some(TagId(3)),
                Status::Pinned,
            )),
        );
        tags.insert(
            &TagId(2),
            SenValue::borrowed(&tag("urgent", TagKind::Label, None, Status::Normal)),
        );
        tags.insert(
            &TagId(3),
            SenValue::borrowed(&tag(
                "slack",
                TagKind::WorkstreamGroup,
                None,
                Status::Normal,
            )),
        );
    }
    {
        let mut agents = write.open_table(AGENTS);
        agents.insert(
            &tagged,
            SenValue::borrowed(&pre_workstream_agent(
                None,
                None,
                Status::Pinned,
                vec![TagId(1), TagId(2)],
            )),
        );
        agents.insert(
            &child,
            SenValue::borrowed(&pre_workstream_agent(
                None,
                Some(tagged),
                Status::Normal,
                Vec::new(),
            )),
        );
        agents.insert(
            &orphan,
            SenValue::borrowed(&pre_workstream_agent(
                Some("solo"),
                None,
                Status::Normal,
                Vec::new(),
            )),
        );
    }
    write.commit();

    let mut write = db.write().await;
    write.init_agent_tables();
    write.commit();

    let read = db.read();
    let team = read.get_workstream(WorkstreamId(1));
    assert_eq!(team.name, "team");
    assert_eq!(team.labels, ["group:slack", "pin"]);

    let tagged = read.get_agent(tagged);
    assert_eq!(tagged.workstream, WorkstreamId(1));
    assert_eq!(tagged.labels, ["urgent", "pin"]);
    assert_eq!(tagged.legacy, LegacyAgentFields::default());
    assert_eq!(tagged.disposition, AgentDisposition::Pending);
    assert_eq!(read.get_agent(child).workstream, WorkstreamId(1));

    // The orphan founds its own workstream past the persisted counter.
    let orphan = read.get_agent(orphan);
    assert_eq!(orphan.workstream, WorkstreamId(4));
    assert_eq!(read.get_workstream(WorkstreamId(4)).name, "solo");

    assert!(!read.has_table("tags"));
    let format = read.open_table(FORMAT).get(&()).unwrap().value();
    assert_eq!(format, CURRENT_AGENT_DB_FORMAT);
}

#[test]
fn deep_default_uses_default_deep_config() {
    assert_eq!(
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        SessionBinding::ResponsesGpt55(InferenceProfile::default())
    );
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
                SenValue::owned(user_event("seq")),
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
    let workstream = write.create_workstream(UnixMs(1), "default".to_owned());
    let agent_id = write.alloc_agent_id();
    let next = write.create_agent(
        UnixMs(1),
        agent_id,
        workstream,
        Some("main".to_owned()),
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    let next = write.append_agent_event(next, &user_event("hello"));
    write.append_agent_event(next, &user_event("again"));
    write.commit();

    let read = db.read();
    let agent = read.get_agent(agent_id);
    assert_eq!(agent.display_name.as_deref(), Some("main"));
    assert_eq!(agent.workstream, workstream);

    let (next, events) = read.agent_events(agent_id);
    assert_eq!(next.seq, 2);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], user_event("hello"));
}

#[tokio::test]
async fn agent_events_read_lineage_parents() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let workstream = write.create_workstream(UnixMs(1), "default".to_owned());
    let agent_id = write.alloc_agent_id();
    let next = write.create_agent(
        UnixMs(1),
        agent_id,
        workstream,
        Some("main".to_owned()),
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    let fork_at = write.append_agent_event(next, &user_event("parent"));
    write.append_agent_event(fork_at, &user_event("sibling"));

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
    write.append_agent_event(AgentEventPos::root(child_lineage), &user_event("child"));
    write.commit();

    let read = db.read();
    let (next, events) = read.agent_events(agent_id);
    assert_eq!(next.lineage_id, child_lineage);
    assert_eq!(next.seq, 1);
    let texts = events
        .into_iter()
        .map(|event| event_text(&event))
        .collect::<Vec<_>>();
    assert_eq!(texts, ["parent", "child"]);
}

#[tokio::test]
async fn fork_agent_lineage_repoints_current_branch() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let workstream = write.create_workstream(UnixMs(1), "default".to_owned());
    let agent_id = write.alloc_agent_id();
    let next = write.create_agent(
        UnixMs(1),
        agent_id,
        workstream,
        Some("main".to_owned()),
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    let fork_at = write.append_agent_event(next, &user_event("parent"));
    write.append_agent_event(fork_at, &user_event("old branch"));

    let child_next = write.fork_agent_lineage(UnixMs(2), agent_id, fork_at);
    write.append_agent_event(child_next, &user_event("new branch"));
    write.commit();

    let (_, events) = db.read().agent_events(agent_id);
    let texts = events
        .into_iter()
        .map(|event| event_text(&event))
        .collect::<Vec<_>>();
    assert_eq!(texts, ["parent", "new branch"]);
}

#[tokio::test]
async fn set_agent_workstream_moves_the_agent() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let first = write.create_workstream(UnixMs(1), "default".to_owned());
    let second = write.create_workstream(UnixMs(1), "infra".to_owned());
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        first,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    write.set_agent_workstream(UnixMs(2), agent_id, second);
    write.commit();

    assert_eq!(db.read().get_agent(agent_id).workstream, second);
}

#[tokio::test]
async fn turn_end_and_user_message_set_dispositions() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let workstream = write.create_workstream(UnixMs(1), "default".to_owned());
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    write.record_agent_turn_end(agent_id);
    write.commit();
    assert_eq!(
        db.read().get_agent(agent_id).disposition,
        AgentDisposition::Pending
    );

    let mut write = db.write().await;
    write.record_agent_user_message(UnixMs(5), agent_id, "  please\ncheck the   claims  ");
    write.commit();
    let agent = db.read().get_agent(agent_id);
    assert_eq!(agent.disposition, AgentDisposition::Done);
    assert_eq!(agent.last_user_message, UnixMs(5));
    assert_eq!(agent.last_user_message_text, "please check the claims");
}

#[tokio::test]
async fn view_config_round_trips_and_defaults_empty() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    write.commit();
    assert_eq!(db.read().view_config(), Vec::<u8>::new());

    let mut write = db.write().await;
    write.set_view_config(vec![1, 2, 3]);
    write.commit();
    assert_eq!(db.read().view_config(), [1, 2, 3]);
}

#[tokio::test]
async fn projects_upsert_by_path_and_remove() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    write.upsert_project(
        UnixMs(1),
        "/home/user/src/rho",
        "rho".to_owned(),
        "agents".to_owned(),
    );
    write.upsert_project(
        UnixMs(2),
        "/home/user/src/zed",
        "zed".to_owned(),
        "editor".to_owned(),
    );
    // Re-adding the same path renames it and keeps created_at.
    write.upsert_project(
        UnixMs(3),
        "/home/user/src/rho",
        "rho-main".to_owned(),
        "runtime".to_owned(),
    );
    write.commit();

    let projects = db.read().list_projects();
    assert_eq!(projects.len(), 2);
    let rho = projects
        .iter()
        .find(|(path, _)| path == std::path::Path::new("/home/user/src/rho"))
        .unwrap();
    assert_eq!(rho.1.name, "rho-main");
    assert_eq!(rho.1.description, "runtime");
    assert_eq!(rho.1.created_at, UnixMs(1));

    let mut write = db.write().await;
    write.remove_project("/home/user/src/zed");
    write.commit();
    assert_eq!(db.read().list_projects().len(), 1);
}

#[tokio::test]
async fn agent_ids_allocate_before_records_exist() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let workstream = write.create_workstream(UnixMs(1), "default".to_owned());
    // Only the second allocation gets a record, as when the first jj
    // checkout fails.
    let leaked_id = write.alloc_agent_id();
    let agent_id = write.alloc_agent_id();
    assert_ne!(leaked_id, agent_id);
    write.create_agent(
        UnixMs(2),
        agent_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    write.commit();

    let read = db.read();
    assert_eq!(read.get_agent(agent_id).workdirs, vec![test_workspace()]);
    assert_eq!(read.list_agents().len(), 1);
}
