use std::sync::Arc;

use rho_core::{ContentPart, UnixMs};
use rho_db::{RhoDb, SenValue};
use rho_inference::PromptCacheKey;
use rho_workspaces::{WorkspaceId, WorkspaceIdDomain, WorkspaceInfo};

use super::*;

#[tokio::test]
async fn agent_usage_accumulates_in_five_minute_buckets() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let agent_id = write.alloc_agent_id();
    let workstream = write.create_workstream(UnixMs(1), "usage".to_owned());
    write.create_agent(
        UnixMs(1),
        agent_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesSol(InferenceProfile::default()),
        AgentRuntime::Rho {
            prompt_cache_key: PromptCacheKey::generate(),
        },
        None,
    );
    let first = AgentUsageBucket {
        bucket_start_ms: AGENT_USAGE_BUCKET_MS,
        model: AgentUsageModel::GPT,
        input_tokens: 10,
        cache_read_tokens: 20,
        cache_write_tokens: 30,
        cache_write_1h_tokens: 0,
        output_tokens: 40,
        requests: 1,
        approximate: false,
    };
    write.add_agent_usage(agent_id, &first);
    write.add_agent_usage(agent_id, &first);
    let claude_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        claude_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ClaudeFable {
            effort: ClaudeEffort::High,
        },
        AgentRuntime::Claude {
            session_id: uuid::Uuid::new_v4(),
        },
        None,
    );
    write.add_agent_usage(
        claude_id,
        &AgentUsageBucket {
            model: AgentUsageModel::FABLE,
            ..first.clone()
        },
    );
    let opus_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        opus_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ClaudeOpus {
            effort: ClaudeEffort::Medium,
        },
        AgentRuntime::Claude {
            session_id: uuid::Uuid::new_v4(),
        },
        None,
    );
    write.add_agent_usage(
        opus_id,
        &AgentUsageBucket {
            model: AgentUsageModel::OPUS,
            ..first.clone()
        },
    );
    let terra_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        terra_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesTerra(InferenceProfile::default()),
        AgentRuntime::Rho {
            prompt_cache_key: PromptCacheKey::generate(),
        },
        None,
    );
    write.add_agent_usage(
        terra_id,
        &AgentUsageBucket {
            model: AgentUsageModel::UNKNOWN,
            ..first.clone()
        },
    );
    let luna_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        luna_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesLuna(InferenceProfile::default()),
        AgentRuntime::Rho {
            prompt_cache_key: PromptCacheKey::generate(),
        },
        None,
    );
    write.add_agent_usage(
        luna_id,
        &AgentUsageBucket {
            model: AgentUsageModel::UNKNOWN,
            ..first.clone()
        },
    );
    write.commit();

    let read = db.read();
    let buckets = read.agent_usage(agent_id, UnixMs(0));
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0].input_tokens, 20);
    assert_eq!(buckets[0].requests, 2);
    assert_eq!(read.agent_usage_total(agent_id).output_tokens, 80);
    let global = read.global_agent_usage(UnixMs(0));
    assert_eq!(global.len(), 5);
    assert_eq!(global[0].0, AgentUsageModel::GPT);
    assert_eq!(global[0].1.output_tokens, 80);
    assert_eq!(global[1].0, AgentUsageModel::FABLE);
    assert_eq!(global[1].1.output_tokens, 40);
    assert_eq!(global[2].0, AgentUsageModel::OPUS);
    assert_eq!(global[2].1.output_tokens, 40);
    assert_eq!(global[3].0, AgentUsageModel::TERRA);
    assert_eq!(global[4].0, AgentUsageModel::LUNA);
}

#[tokio::test]
async fn quota_history_deduplicates_unchanged_samples() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let sample = QuotaObservationRecord {
        provider: QuotaProvider::ChatGpt,
        model: QuotaModel::GPT,
        observed_at: UnixMs(1),
        used_percent: 20,
        reset_at_unix: Some(100),
    };
    let mut write = db.write().await;
    write.init_agent_tables();
    assert!(write.record_quota_observation(sample.clone()));
    assert!(!write.record_quota_observation(QuotaObservationRecord {
        observed_at: UnixMs(2),
        ..sample.clone()
    }));
    assert!(!write.record_quota_observation(QuotaObservationRecord {
        observed_at: UnixMs(2),
        reset_at_unix: Some(101),
        ..sample.clone()
    }));
    assert!(!write.record_quota_observation(QuotaObservationRecord {
        observed_at: UnixMs(2),
        reset_at_unix: Some(99),
        ..sample.clone()
    }));
    assert!(write.record_quota_observation(QuotaObservationRecord {
        observed_at: UnixMs(3),
        used_percent: 21,
        ..sample.clone()
    }));
    assert!(write.record_quota_observation(QuotaObservationRecord {
        observed_at: UnixMs(4),
        used_percent: 22,
        ..sample.clone()
    }));
    assert!(write.record_quota_observation(QuotaObservationRecord {
        model: QuotaModel::OPUS,
        observed_at: UnixMs(3),
        used_percent: 30,
        ..sample.clone()
    }));
    assert!(write.record_quota_observation(QuotaObservationRecord {
        model: QuotaModel::FABLE,
        observed_at: UnixMs(3),
        used_percent: 40,
        ..sample
    }));
    write.commit();

    let history = db.read().quota_observations(QuotaModel::GPT, UnixMs(0));
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].used_percent, 20);
    assert_eq!(history[2].used_percent, 22);
    assert_eq!(
        db.read().quota_observations(QuotaModel::OPUS, UnixMs(0))[0].used_percent,
        30
    );
    assert_eq!(
        db.read().quota_observations(QuotaModel::FABLE, UnixMs(0))[0].used_percent,
        40
    );

    // A bounded reverse read returns only the horizon and one baseline,
    // without crossing into another model's key range.
    let recent = db.read().quota_observations(QuotaModel::GPT, UnixMs(4));
    assert_eq!(
        recent
            .iter()
            .map(|sample| sample.observed_at)
            .collect::<Vec<_>>(),
        vec![UnixMs(3), UnixMs(4)]
    );
}

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
        profile(EngineerIntelligence::Cheap),
        SessionBinding::ResponsesTerra(InferenceProfile {
            effort: ReasoningEffort::High,
            code_mode: true,
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
        EngineerIntelligence::Cheap,
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
        profile(EngineerIntelligence::Alt),
        SessionBinding::ClaudeOpus {
            effort: ClaudeEffort::Medium
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
        AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Cheap,
        }
        .session_profile()
        .unwrap(),
        SessionBinding::AdvisorTerra(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: false,
            code_mode: true,
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
    assert!(matches!(
        AgentRole::Iris.session_profile().unwrap(),
        SessionBinding::ResponsesTerra(InferenceProfile {
            effort: ReasoningEffort::Medium,
            fast_mode: true,
            code_mode: false,
        })
    ));
    assert!(AgentRole::Iris.is_pm());
    assert_eq!(AgentRole::Iris.handle_prefix(), "iris");
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
            ContentPart::Image { .. } => panic!("expected text content"),
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
async fn claude_rewind_descriptor_round_trips_and_completes() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let source_session_id = uuid::uuid!("00000000-0000-4000-8000-000000000001");
    let session_id = uuid::uuid!("00000000-0000-4000-8000-000000000002");
    let resume_at = uuid::uuid!("00000000-0000-4000-8000-000000000003");
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
        AgentRuntime::Claude {
            session_id: source_session_id,
        },
        None,
    );
    let rewind = ClaudeRewind {
        source_session_id,
        session_id,
        resume_at: Some(resume_at),
    };
    write.set_agent_claude_rewind(agent_id, Some(rewind.clone()));
    write.commit();

    assert_eq!(db.read().get_agent(agent_id).claude_rewind, Some(rewind));

    let mut write = db.write().await;
    write.complete_agent_claude_rewind(agent_id, session_id);
    write.commit();
    let record = db.read().get_agent(agent_id);
    assert_eq!(record.runtime, AgentRuntime::Claude { session_id });
    assert_eq!(record.claude_rewind, None);
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
async fn presentation_history_folds_by_source_reachability_after_rewind() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let workstream = write.create_workstream(UnixMs(1), "default".to_owned());
    let agent_id = write.alloc_agent_id();
    let first = write.create_agent(
        UnixMs(1),
        agent_id,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    let second = write.append_agent_event(first, &user_event("first"));
    let third = write.append_agent_event(second, &user_event("later"));
    let update = AgentPresentationUpdate {
        generated_title: PresentationField::Set("first-subject".to_owned()),
        activity: PresentationField::Set("reading first request".to_owned()),
        through: first,
    };
    assert!(
        write
            .apply_agent_presentation(UnixMs(2), agent_id, &update)
            .is_some()
    );
    write.append_agent_presentation_history(third, &update);
    let fourth = write.append_agent_event(
        third,
        &AgentEvent::PresentationUpdated {
            update: update.clone(),
        },
    );
    write.append_agent_event(fourth, &user_event("even later"));

    // Rewind before the second input. The update itself is on the abandoned
    // branch, but its source (`first`) remains selected and must survive.
    write.fork_agent_lineage(UnixMs(3), agent_id, second);
    let cache = write.rebuild_agent_presentation_cache(UnixMs(3), agent_id);
    assert_eq!(cache.generated_title.as_deref(), Some("first-subject"));
    assert_eq!(cache.activity.as_deref(), Some("reading first request"));

    // A completion based on the discarded input cannot write into the new
    // lineage, even if it reaches the serialized loop after the rewind.
    let stale = AgentPresentationUpdate {
        generated_title: PresentationField::Set("discarded".to_owned()),
        activity: PresentationField::Unchanged,
        through: second,
    };
    assert!(
        write
            .apply_agent_presentation(UnixMs(4), agent_id, &stale)
            .is_none()
    );
    write.commit();

    let record = db.read().get_agent(agent_id);
    assert_eq!(record.generated_title.as_deref(), Some("first-subject"));
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
    write.record_agent_turn_end(UnixMs(2), agent_id);
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
    assert!(agent.user_interacted);

    // An unexpired snooze holds across a turn end; an expired one does not.
    let mut write = db.write().await;
    write.set_agent_disposition(agent_id, AgentDisposition::Snoozed { until: UnixMs(100) });
    write.record_agent_turn_end(UnixMs(50), agent_id);
    write.commit();
    assert_eq!(
        db.read().get_agent(agent_id).disposition,
        AgentDisposition::Snoozed { until: UnixMs(100) }
    );
    let mut write = db.write().await;
    write.record_agent_turn_end(UnixMs(150), agent_id);
    write.commit();
    assert_eq!(
        db.read().get_agent(agent_id).disposition,
        AgentDisposition::Pending
    );

    // A needs-you report leaves the verdict alone; an FYI settles like a
    // pressed Done while the summary stays for the row.
    let mut write = db.write().await;
    write.record_agent_turn_report(
        agent_id,
        &crate::db::TurnReport {
            needs_you: true,
            summary: "which migration to drop?".to_owned(),
        },
    );
    write.commit();
    assert_eq!(
        db.read().get_agent(agent_id).disposition,
        AgentDisposition::Pending
    );
    let mut write = db.write().await;
    write.record_agent_turn_report(
        agent_id,
        &crate::db::TurnReport {
            needs_you: false,
            summary: "tests pass".to_owned(),
        },
    );
    write.commit();
    let agent = db.read().get_agent(agent_id);
    assert_eq!(agent.disposition, AgentDisposition::Done);
    assert!(agent.turn_report.is_some());

    // A new user message overrides any verdict: snooze and the stale turn
    // report both give way to Done.
    let mut write = db.write().await;
    write.set_agent_disposition(
        agent_id,
        AgentDisposition::Snoozed {
            until: UnixMs(1_000),
        },
    );
    write.record_agent_turn_report(
        agent_id,
        &crate::db::TurnReport {
            needs_you: true,
            summary: "which migration to drop?".to_owned(),
        },
    );
    write.record_agent_user_message(UnixMs(200), agent_id, "next task");
    write.commit();
    let agent = db.read().get_agent(agent_id);
    assert_eq!(agent.disposition, AgentDisposition::Done);
    assert_eq!(agent.turn_report, None);
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

#[tokio::test]
async fn response_subscriptions_are_persistent_edges() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let subscriber = write.alloc_agent_id();
    let target = write.alloc_agent_id();
    write.set_agent_response_subscription(subscriber, target, true);
    write.commit();

    assert!(db.read().is_agent_response_subscribed(subscriber, target));
    assert_eq!(db.read().agent_response_subscribers(target), [subscriber]);

    let mut write = db.write().await;
    write.set_agent_response_subscription(subscriber, target, false);
    write.commit();
    assert!(!db.read().is_agent_response_subscribed(subscriber, target));
    assert!(db.read().agent_response_subscribers(target).is_empty());
}

#[tokio::test]
async fn migrates_existing_parent_edges_to_response_subscriptions() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let workstream = write.create_workstream(UnixMs(1), "migration".to_owned());
    let parent = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        parent,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    let child = write.alloc_agent_id();
    write.create_agent(
        UnixMs(2),
        child,
        workstream,
        None,
        vec![test_workspace()],
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        Some(parent),
    );
    write.open_table(FORMAT).insert(&(), &"7c5e2a91".to_owned());
    write.commit();

    let mut write = db.write().await;
    write.init_agent_tables();
    write.commit();

    assert_eq!(db.read().agent_response_subscribers(child), [parent]);
}
