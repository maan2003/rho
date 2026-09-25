use rho_agent_types::{ContentPart, MessageDelivery, Place, TurnOutcome, UnixMs};
use rho_db::RhoDb;
use rho_inference::PromptCacheKey;

use super::*;

#[test]
fn canonical_bindings_round_trip() {
    for binding in [
        SessionBinding::ResponsesLuna(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: false,
        }),
        SessionBinding::ResponsesSol(InferenceProfile {
            effort: ReasoningEffort::High,
            fast_mode: false,
        }),
        SessionBinding::ResponsesAstra(InferenceProfile {
            effort: ReasoningEffort::Medium,
            fast_mode: false,
        }),
        SessionBinding::AdvisorSol(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: false,
        }),
        SessionBinding::AdvisorAstra(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: false,
        }),
    ] {
        let mut encoded = bytes::BytesMut::new();
        senax_encoder::encode_to(&binding, &mut encoded).unwrap();
        assert_eq!(
            <SessionBinding as senax_encoder::Decoder>::decode(&mut encoded).unwrap(),
            binding
        );
    }
}

#[test]
fn agent_id_tables_keep_the_type_name_they_were_written_with() {
    assert_eq!(
        <AgentId as redb::Value>::type_name().name(),
        "prefix_id::PrefixId<rho_core::AgentIdDomain>"
    );
}

#[test]
fn sol_binding_is_the_medium_engineer() {
    let binding = SessionBinding::ResponsesSol(InferenceProfile {
        effort: ReasoningEffort::High,
        fast_mode: false,
    });
    assert_eq!(binding.deep_model(), Some(InferenceModel::Gpt6Sol));
    assert_eq!(
        binding.agent_role(),
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
        }
    );
}

#[test]
fn quota_observation_decodes_before_auth_namespaces() {
    #[derive(senax_encoder::Encode)]
    struct LegacyQuotaObservationRecord {
        provider: QuotaProvider,
        model: QuotaModel,
        observed_at: UnixMs,
        used_percent: u8,
        reset_at_unix: Option<i64>,
    }

    let legacy = LegacyQuotaObservationRecord {
        provider: QuotaProvider::ChatGpt,
        model: QuotaModel::GPT,
        observed_at: UnixMs(1),
        used_percent: 20,
        reset_at_unix: Some(100),
    };
    let mut encoded = bytes::BytesMut::new();
    senax_encoder::encode_to(&legacy, &mut encoded).unwrap();
    let decoded = <QuotaObservationRecord as senax_encoder::Decoder>::decode(&mut encoded).unwrap();
    assert_eq!(decoded.auth_namespace, None);
}

#[tokio::test]
async fn agent_usage_accumulates_in_five_minute_buckets() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        None,
        test_workspace(),
        AgentRole::default(),
        SessionBinding::ResponsesSol(InferenceProfile::default()),
        AgentRuntime::Rho {
            prompt_cache_key: PromptCacheKey::generate(),
        },
        crate::db::AgentOrigin::User,
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
        None,
        test_workspace(),
        AgentRole::default(),
        SessionBinding::ClaudeFable {
            effort: ClaudeEffort::High,
        },
        AgentRuntime::Claude {
            session_id: uuid::Uuid::new_v4(),
        },
        crate::db::AgentOrigin::User,
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
        None,
        test_workspace(),
        AgentRole::default(),
        SessionBinding::ClaudeOpus {
            effort: ClaudeEffort::Medium,
        },
        AgentRuntime::Claude {
            session_id: uuid::Uuid::new_v4(),
        },
        crate::db::AgentOrigin::User,
    );
    write.add_agent_usage(
        opus_id,
        &AgentUsageBucket {
            model: AgentUsageModel::OPUS,
            ..first.clone()
        },
    );
    let astra_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        astra_id,
        None,
        test_workspace(),
        AgentRole::default(),
        SessionBinding::ResponsesAstra(InferenceProfile::default()),
        AgentRuntime::Rho {
            prompt_cache_key: PromptCacheKey::generate(),
        },
        crate::db::AgentOrigin::User,
    );
    write.add_agent_usage(
        astra_id,
        &AgentUsageBucket {
            model: AgentUsageModel::UNKNOWN,
            ..first.clone()
        },
    );
    let luna_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        luna_id,
        None,
        test_workspace(),
        AgentRole::default(),
        SessionBinding::ResponsesLuna(InferenceProfile::default()),
        AgentRuntime::Rho {
            prompt_cache_key: PromptCacheKey::generate(),
        },
        crate::db::AgentOrigin::User,
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
    assert_eq!(global[3].0, AgentUsageModel::LUNA);
    assert_eq!(global[4].0, AgentUsageModel::ASTRA);
}

#[tokio::test]
async fn quota_history_deduplicates_unchanged_samples() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let sample = QuotaObservationRecord {
        provider: QuotaProvider::ChatGpt,
        model: QuotaModel::GPT,
        auth_namespace: Some("work".to_owned()),
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
    assert!(write.record_quota_observation(QuotaObservationRecord {
        auth_namespace: Some("personal".to_owned()),
        observed_at: UnixMs(1),
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
        auth_namespace: None,
        observed_at: UnixMs(3),
        used_percent: 30,
        ..sample.clone()
    }));
    assert!(write.record_quota_observation(QuotaObservationRecord {
        model: QuotaModel::FABLE,
        auth_namespace: None,
        observed_at: UnixMs(3),
        used_percent: 40,
        ..sample
    }));
    write.commit();

    let history = db.read().quota_observations(QuotaModel::GPT, UnixMs(0));
    assert_eq!(history.len(), 4);
    assert_eq!(history[0].used_percent, 20);
    assert_eq!(history[3].used_percent, 22);
    assert_eq!(
        db.read().quota_observations(QuotaModel::OPUS, UnixMs(0))[0].used_percent,
        30
    );
    assert_eq!(
        db.read().quota_observations(QuotaModel::FABLE, UnixMs(0))[0].used_percent,
        40
    );

    // A bounded read retains one baseline per auth namespace without crossing
    // into another model's key range.
    let recent = db.read().quota_observations(QuotaModel::GPT, UnixMs(4));
    assert_eq!(
        recent
            .iter()
            .map(|sample| sample.observed_at)
            .collect::<Vec<_>>(),
        vec![UnixMs(1), UnixMs(3), UnixMs(4)]
    );
}

#[test]
fn agent_roles_resolve_the_current_model_matrix() {
    let profile = |intelligence| AgentRole::Engineer { intelligence }.session_profile();
    assert_eq!(
        profile(EngineerIntelligence::Mini),
        SessionBinding::ResponsesLuna(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: false,
        })
    );
    assert_eq!(
        profile(EngineerIntelligence::Medium),
        SessionBinding::ResponsesSol(InferenceProfile {
            effort: ReasoningEffort::High,
            fast_mode: false,
        })
    );
    assert_eq!(
        profile(EngineerIntelligence::High),
        SessionBinding::ResponsesAstra(InferenceProfile {
            effort: ReasoningEffort::Medium,
            fast_mode: false,
        })
    );
    assert_eq!(
        profile(EngineerIntelligence::Medium1),
        SessionBinding::ClaudeOpus {
            effort: ClaudeEffort::Medium,
        }
    );
    assert_eq!(
        profile(EngineerIntelligence::High1),
        SessionBinding::ClaudeFable {
            effort: ClaudeEffort::Medium,
        }
    );
    assert_eq!(
        AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Low,
        }
        .session_profile(),
        SessionBinding::AdvisorSol(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: false,
        })
    );
    assert_eq!(
        AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Medium,
        }
        .session_profile(),
        SessionBinding::AdvisorAstra(InferenceProfile {
            effort: ReasoningEffort::Xhigh,
            fast_mode: false,
        })
    );
    assert_eq!(
        AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Medium1,
        }
        .session_profile(),
        SessionBinding::ClaudeAdvisor {
            effort: ClaudeEffort::Xhigh,
        }
    );
}

use rho_inference::types::MessageSender;

use crate::{InputKind, QueuedInput};

pub(crate) fn user_event(text: &str) -> AgentEvent<'static> {
    AgentEvent::Accepted(QueuedInput {
        source: MessageSender::User,
        kind: InputKind::Message {
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
        },
        delivery: MessageDelivery::Immediate,
        at: UnixMs(0),
    })
}

fn event_text(event: &AgentEvent<'_>) -> String {
    match event {
        AgentEvent::Accepted(QueuedInput {
            kind: InputKind::Message { content },
            ..
        }) => match &content[0] {
            ContentPart::Text { text } => text.clone(),
            ContentPart::Image { .. } => panic!("expected text content"),
        },
        // Every agent's log opens with its creation.
        AgentEvent::Created { .. } => "created".to_owned(),
        AgentEvent::Rewound { .. } => "rewound".to_owned(),
        _ => unreachable!(),
    }
}

/// Tests exercise agent records only; any workspace info will do.
pub(crate) fn test_workspace() -> Place {
    Place {
        workset: "0123456789ab".into(),
        cwd: "/src/rho".into(),
        mode: Default::default(),
        origin: None,
    }
}

pub(crate) fn test_agent_runtime() -> AgentRuntime {
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
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        None,
        test_workspace(),
        AgentRole::default(),
        SessionBinding::ResponsesSol(InferenceProfile::default()),
        AgentRuntime::Claude {
            session_id: source_session_id,
        },
        crate::db::AgentOrigin::User,
    );
    let rewind = ClaudeRewind {
        source_session_id,
        session_id,
        resume_at: Some(resume_at),
    };
    write.set_agent_claude_rewind(agent_id, Some(rewind.clone()));
    write.commit();

    assert_eq!(
        db.read().get_agent(agent_id).config.claude_rewind,
        Some(rewind)
    );

    let mut write = db.write().await;
    write.complete_agent_claude_rewind(agent_id, session_id);
    write.commit();
    let record = db.read().get_agent(agent_id);
    assert_eq!(record.config.runtime, AgentRuntime::Claude { session_id });
    assert_eq!(record.config.claude_rewind, None);
}

#[tokio::test]
async fn a_mode_change_folds_into_the_agents_place() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        None,
        test_workspace(),
        AgentRole::default(),
        AgentRole::default().session_profile(),
        test_agent_runtime(),
        crate::db::AgentOrigin::User,
    );
    write.commit();
    let before = db.read().get_agent(agent_id).config.place.clone();
    assert_eq!(before.mode, WorksetMode::View);

    let mut write = db.write().await;
    write.set_agent_mode(agent_id, WorksetMode::Exposed);
    write.commit();
    let after = db.read().get_agent(agent_id).config.place.clone();
    assert_eq!(after.mode, WorksetMode::Exposed);
    // Only the mode moved: the workset and directory are the same place.
    assert_eq!(
        Place {
            mode: WorksetMode::View,
            ..after
        },
        before
    );
}

#[tokio::test]
async fn agent_spawned_by_is_stored_at_creation() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let pm = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        pm,
        None,
        test_workspace(),
        AgentRole::default(),
        AgentRole::default().session_profile(),
        test_agent_runtime(),
        crate::db::AgentOrigin::User,
    );
    let engineer = write.alloc_agent_id();
    write.create_agent(
        UnixMs(2),
        engineer,
        None,
        test_workspace(),
        AgentRole::default(),
        AgentRole::default().session_profile(),
        test_agent_runtime(),
        crate::db::AgentOrigin::Child { parent: pm },
    );
    let owned = write.alloc_agent_id();
    write.create_agent(
        UnixMs(3),
        owned,
        None,
        test_workspace(),
        AgentRole::default(),
        AgentRole::default().session_profile(),
        test_agent_runtime(),
        crate::db::AgentOrigin::UserOwned { by: pm },
    );
    write.commit();

    let read = db.read();
    assert_eq!(read.agent_spawner(pm), None);
    assert_eq!(read.agent_spawner(engineer), Some(pm));
    assert_eq!(read.agent_spawner(owned), Some(pm));
    assert_eq!(
        db.read().get_agent(pm).config.spawned_by,
        AgentSpawnedBy::Direct
    );
    assert_eq!(
        db.read().get_agent(engineer).config.spawned_by,
        AgentSpawnedBy::Engineer
    );
}

#[test]
fn deep_default_uses_default_deep_config() {
    assert_eq!(
        SessionBinding::ResponsesSol(InferenceProfile::default()),
        SessionBinding::ResponsesSol(InferenceProfile::default())
    );
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
async fn migration_backfills_heads_from_all_log_rows() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let first = create(&mut write, None, None);
    let second = create(&mut write, None, None);
    write.set_agent_mode(first, WorksetMode::Exposed);
    write.append_agent_event(first, &user_event("hidden"));
    write.rewind_agent(UnixMs(2), first, AgentEventPos::new(1));
    write.append_agent_event(first, &user_event("visible"));
    write.set_agent_role(second, AgentRole::default());
    write.commit();
    let expected = [first, second]
        .into_iter()
        .map(|id| {
            let read = db.read();
            let log = read.open_table(AGENT_LOG);
            (id, fold_head(rows(log.range(agent_range(id)))).unwrap())
        })
        .collect::<Vec<_>>();
    let mut expected = expected;
    expected.sort_by_key(|(id, _)| *id);

    // Recreate a pre-projection store: only the event log survives.
    let mut write = db.write().await;
    write.delete_table("agent_heads");
    write.open_table(FORMAT).insert(&(), &"6bcd407c".to_owned());
    write.commit();

    prepare(&db).await;
    assert_eq!(db.read().list_agents(), expected);
    assert_eq!(
        db.read().list_agent_ids(),
        expected.iter().map(|(id, _)| *id).collect::<Vec<_>>()
    );

    let mut write = db.write().await;
    write.set_agent_mode(first, WorksetMode::View);
    write.commit();
    let read = db.read();
    assert_eq!(read.get_agent(first).config.place.mode, WorksetMode::View);
    assert_eq!(read.get_agent(first).next, AgentEventPos::new(6));
    assert_eq!(
        read.get_agent(second),
        expected.iter().find(|(id, _)| *id == second).unwrap().1
    );
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
async fn agent_ids_allocate_before_records_exist() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    // Only the second allocation gets a record, as when the first
    // checkout fails.
    let leaked_id = write.alloc_agent_id();
    let agent_id = write.alloc_agent_id();
    assert_ne!(leaked_id, agent_id);
    write.create_agent(
        UnixMs(2),
        agent_id,
        None,
        test_workspace(),
        AgentRole::default(),
        SessionBinding::ResponsesSol(InferenceProfile::default()),
        test_agent_runtime(),
        crate::db::AgentOrigin::User,
    );
    write.commit();

    let read = db.read();
    assert_eq!(read.get_agent(agent_id).config.place, test_workspace());
    assert_eq!(read.list_agents().len(), 1);
}

#[tokio::test]
async fn response_subscriptions_are_persistent_edges() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let mut ids = (0..5).map(|_| write.alloc_agent_id()).collect::<Vec<_>>();
    ids.sort();
    let [
        lower_target,
        lower_subscriber,
        target,
        upper_subscriber,
        upper_target,
    ] = ids.as_slice()
    else {
        unreachable!()
    };
    write.set_agent_response_subscription(*lower_subscriber, *lower_target, true);
    write.set_agent_response_subscription(*lower_subscriber, *target, true);
    write.set_agent_response_subscription(*upper_subscriber, *target, true);
    write.set_agent_response_subscription(*upper_subscriber, *upper_target, true);
    write.commit();

    let read = db.read();
    assert!(read.is_agent_response_subscribed(*lower_subscriber, *target));
    assert_eq!(
        read.agent_response_subscribers(*target),
        [*lower_subscriber, *upper_subscriber]
    );
    assert_eq!(
        read.agent_response_subscribers(*lower_target),
        [*lower_subscriber]
    );
    assert_eq!(
        read.agent_response_subscribers(*upper_target),
        [*upper_subscriber]
    );
    drop(read);

    let mut write = db.write().await;
    write.set_agent_response_subscription(*lower_subscriber, *target, false);
    write.commit();
    assert!(
        !db.read()
            .is_agent_response_subscribed(*lower_subscriber, *target)
    );
    assert_eq!(
        db.read().agent_response_subscribers(*target),
        [*upper_subscriber]
    );
}

pub(super) fn create(
    write: &mut rho_db::WriteTxn,
    spawn_name: Option<&str>,
    parent: Option<AgentId>,
) -> AgentId {
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        spawn_name.map(str::to_owned),
        test_workspace(),
        AgentRole::default(),
        SessionBinding::ResponsesSol(InferenceProfile::default()),
        test_agent_runtime(),
        parent.map_or(crate::db::AgentOrigin::User, |parent| {
            crate::db::AgentOrigin::Child { parent }
        }),
    );
    agent_id
}

#[tokio::test]
async fn positions_are_dense_per_agent_and_creation_is_row_zero() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let first = create(&mut write, Some("main"), None);
    let second = create(&mut write, None, Some(first));
    assert_eq!(
        write.append_agent_event(first, &user_event("hello")),
        AgentEventPos::new(1)
    );
    assert_eq!(
        write.append_agent_event(second, &user_event("hi")),
        AgentEventPos::new(1)
    );
    assert_eq!(
        write.append_agent_event(first, &user_event("again")),
        AgentEventPos::new(2)
    );
    write.commit();

    let read = db.read();
    let agent = read.get_agent(first);
    assert_eq!(agent.config.spawn_name.as_deref(), Some("main"));
    assert_eq!(agent.next, AgentEventPos::new(3));
    assert_eq!(read.get_agent(second).parent, Some(first));
    assert_eq!(read.agent_parent(second), Some(first));
    let mut ids = read.list_agent_ids();
    ids.sort();
    let mut expected = [first, second];
    expected.sort();
    assert_eq!(ids, expected);

    let (next, events) = read.agent_events(first);
    assert_eq!(next, AgentEventPos::new(3));
    let texts = events.iter().map(event_text).collect::<Vec<_>>();
    assert_eq!(texts, ["created", "hello", "again"]);
    assert_eq!(
        read.agent_event(first, AgentEventPos::new(2)),
        Some(user_event("again"))
    );
    assert_eq!(read.agent_event(first, AgentEventPos::new(3)), None);
}

#[tokio::test]
async fn a_rewind_hides_rows_and_is_itself_visible() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let agent_id = create(&mut write, None, None);
    write.append_agent_event(agent_id, &user_event("parent"));
    write.append_agent_event(agent_id, &user_event("old branch"));
    // Take back everything from row 2 on; the rewind lands at row 3.
    assert_eq!(
        write.rewind_agent(UnixMs(2), agent_id, AgentEventPos::new(2)),
        AgentEventPos::new(3)
    );
    write.append_agent_event(agent_id, &user_event("new branch"));
    write.commit();

    let read = db.read();
    let (next, records) = read.agent_event_records(agent_id);
    assert_eq!(next, AgentEventPos::new(5));
    let seen = records
        .iter()
        .map(|(pos, event)| (pos.pos, event_text(event)))
        .collect::<Vec<_>>();
    assert_eq!(
        seen,
        [
            (0, "created".to_owned()),
            (1, "parent".to_owned()),
            (3, "rewound".to_owned()),
            (4, "new branch".to_owned()),
        ]
    );
    // The hidden row is still there for anyone who asks by position.
    assert_eq!(
        read.agent_event(agent_id, AgentEventPos::new(2)),
        Some(user_event("old branch"))
    );
    // The tail walk backward skips it too.
    drop(read);

    let mut write = db.write().await;
    write.rewind_agent(UnixMs(3), agent_id, AgentEventPos::new(3));
    write.append_agent_event(agent_id, &user_event("latest branch"));
    write.commit();
    let (next, records) = db.read().agent_event_records(agent_id);
    assert_eq!(next, AgentEventPos::new(7));
    assert_eq!(
        records
            .iter()
            .map(|(pos, event)| (pos.pos, event_text(event)))
            .collect::<Vec<_>>(),
        [
            (0, "created".to_owned()),
            (1, "parent".to_owned()),
            (5, "rewound".to_owned()),
            (6, "latest branch".to_owned()),
        ]
    );
}

#[tokio::test]
async fn the_head_folds_title_turns_and_user_contact() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let root = create(&mut write, None, None);
    let child = create(&mut write, Some("named"), Some(root));
    write.tell_turn(UnixMs(2), child, TurnEdge::Started);
    write.append_agent_event(
        child,
        &AgentEvent::Titled {
            title: Some("story-log".to_owned()),
            at: UnixMs(3),
        },
    );
    write.commit();

    let head = db.read().get_agent(child);
    // The spawner's name beats the sidecar's title.
    assert_eq!(head.title(), Some("named"));
    assert_eq!(head.generated_title.as_deref(), Some("story-log"));
    assert!(head.title_attempted);
    assert!(head.turn_running);
    assert!(!head.user_interacted);
    assert_eq!(head.last_turn_ended, None);

    let mut write = db.write().await;
    write.tell_turn(UnixMs(5), child, TurnEdge::Ended(TurnOutcome::Completed));
    write.append_agent_event(child, &user_event("the user speaks"));
    write.tell_wants(UnixMs(6), child, AgentWant::Ask, Some("which?".to_owned()));
    write.commit();

    // The label described work that just stopped.
    let head = db.read().get_agent(child);
    assert!(!head.turn_running);
    assert_eq!(head.activity, None);
    assert_eq!(head.last_turn_ended, Some(UnixMs(5)));
    assert!(head.user_interacted);
    assert!(!db.read().get_agent(root).user_interacted);
}

#[tokio::test]
async fn deleting_an_agent_removes_every_row_it_owns() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let doomed = create(&mut write, None, None);
    let kept = create(&mut write, None, None);
    write.append_agent_event(doomed, &user_event("one"));
    write.append_agent_event(kept, &user_event("two"));
    write.set_agent_response_subscription(kept, doomed, true);
    write.set_agent_response_subscription(doomed, kept, true);
    let bucket = AgentUsageBucket {
        bucket_start_ms: AGENT_USAGE_BUCKET_MS,
        requests: 1,
        ..Default::default()
    };
    write.add_agent_usage(doomed, &bucket);
    write.add_agent_usage(kept, &bucket);
    write.commit();

    assert_eq!(delete_agents(&db, &[doomed]).await, [(doomed, 2)]);

    let read = db.read();
    assert_eq!(read.list_agent_ids(), [kept]);
    let journal = read
        .journal_since(Seq(0), 100)
        .into_iter()
        .map(|(_, agent_id, pos, _)| (agent_id, pos.pos))
        .collect::<Vec<_>>();
    assert_eq!(journal, [(kept, 0), (kept, 1)]);
    assert!(read.agent_response_subscribers(doomed).is_empty());
    assert!(read.agent_response_subscribers(kept).is_empty());
    assert!(read.agent_usage(doomed, UnixMs(0)).is_empty());
    assert_eq!(read.agent_usage_total(doomed).requests, 0);
    assert_eq!(read.agent_usage(kept, UnixMs(0)).len(), 1);
}

#[tokio::test]
async fn the_journal_names_every_row_in_write_order() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut feed = crate::journal::feed(&db);

    let mut write = db.write().await;
    write.init_agent_tables();
    let first = create(&mut write, None, None);
    let second = create(&mut write, None, None);
    write.append_agent_event(first, &user_event("one"));
    write.append_agent_event(second, &user_event("two"));
    write.append_agent_event(first, &user_event("three"));
    write.commit();

    let read = db.read();
    assert_eq!(read.journal_head(), Seq(5));
    let named = read
        .journal_since(Seq(0), 100)
        .into_iter()
        .map(|(seq, agent_id, pos, event)| (seq.0, agent_id, pos.pos, event_text(&event)))
        .collect::<Vec<_>>();
    assert_eq!(
        named,
        [
            (1, first, 0, "created".to_owned()),
            (2, second, 0, "created".to_owned()),
            (3, first, 1, "one".to_owned()),
            (4, second, 1, "two".to_owned()),
            (5, first, 2, "three".to_owned()),
        ]
    );
    // Paged: `since` is exclusive, `limit` bounds the page.
    let page = read.journal_since(Seq(3), 1);
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].0, Seq(4));
    assert!(read.journal_since(Seq(5), 100).is_empty());

    // Every row was announced after commit, in the same order.
    let mut announced = Vec::new();
    while let Ok(crate::journal::Feed::Appended(appended)) = feed.try_recv() {
        announced.push((appended.seq.0, appended.agent_id, appended.pos.0));
    }
    assert_eq!(
        announced,
        [
            (1, first, 0),
            (2, second, 0),
            (3, first, 1),
            (4, second, 1),
            (5, first, 2)
        ]
    );
}

#[tokio::test]
async fn rewind_keeps_old_claude_admission_and_allows_the_same_id_again() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let agent_id = create(&mut write, None, None);
    let exec = rho_inference::types::ExecCall {
        id: "once".try_into().unwrap(),
        source: "side_effect()".into(),
    };
    write.append_agent_event(
        agent_id,
        &AgentEvent::ClaudeExecAdmitted {
            call: exec.clone(),
            at: UnixMs(1),
        },
    );
    write.rewind_agent(UnixMs(2), agent_id, AgentEventPos::new(1));
    write.append_agent_event(
        agent_id,
        &AgentEvent::ClaudeExecAdmitted {
            call: exec.clone(),
            at: UnixMs(3),
        },
    );
    write.commit();

    let read = db.read();
    assert_eq!(
        read.agent_recovery_records(agent_id).2,
        [exec.id.clone(), exec.id.clone()]
    );
    let (_, visible) = read.agent_events(agent_id);
    assert!(matches!(
        visible.as_slice(),
        [AgentEvent::Created { .. }, AgentEvent::Rewound { .. }, AgentEvent::ClaudeExecAdmitted { call, .. }]
            if call.id == exec.id
    ));
}

#[tokio::test]
async fn claude_output_survives_restart_and_rewind_until_handoff() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rho.redb");
    let mut batch = crate::ClaudeOutputBatch {
        id: uuid::Uuid::new_v4(),
        outputs: vec![(
            "exec-once".try_into().unwrap(),
            rho_inference::types::ToolOutput {
                output: std::sync::Arc::new("already ran".into()),
                full_output: None,
                images: Default::default(),
                status: rho_agent_types::ToolOutputStatus::Success,
            },
        )],
        wake: crate::WakeFacts::interrupt(),
        at: UnixMs(2),
    };
    let first_batch_id = batch.id;
    let agent_id = {
        let db = RhoDb::open(&path);
        let mut write = db.write().await;
        write.init_agent_tables();
        let id = create(&mut write, None, None);
        write.append_agent_event(
            id,
            &AgentEvent::ClaudeOutput {
                batch: batch.clone(),
            },
        );
        // An interrupted handoff is replaced only by a batch owning both
        // the retained contribution and the fresh exec's output.
        batch.id = uuid::Uuid::new_v4();
        batch.outputs.push((
            "exec-fresh".try_into().unwrap(),
            rho_inference::types::ToolOutput {
                output: std::sync::Arc::new("new output".into()),
                ..batch.outputs[0].1.clone()
            },
        ));
        write.append_agent_event(
            id,
            &AgentEvent::ClaudeOutput {
                batch: batch.clone(),
            },
        );
        write.rewind_agent(UnixMs(3), id, AgentEventPos::new(1));
        write.append_agent_event(
            id,
            &AgentEvent::ClaudeOutputHandedOff {
                id: first_batch_id,
                at: UnixMs(3),
            },
        );
        write.commit();
        id
    };
    let db = RhoDb::open(&path);
    assert_eq!(
        db.read().agent_pending_claude_output(agent_id),
        Some(batch.clone())
    );
    let mut write = db.write().await;
    write.append_agent_event(
        agent_id,
        &AgentEvent::ClaudeOutputHandedOff {
            id: batch.id,
            at: UnixMs(4),
        },
    );
    write.commit();
    assert_eq!(db.read().agent_pending_claude_output(agent_id), None);
    drop(db);
    assert_eq!(
        RhoDb::open(&path)
            .read()
            .agent_pending_claude_output(agent_id),
        None
    );
}

#[tokio::test]
async fn native_later_image_survives_reopen_and_provider_projection() {
    use rho_agent_types::ToolOutputStatus;
    use rho_inference::types::{ContextBlock, ExecOutput, ToolOutput};

    use crate::native::NativeEvent;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rho.redb");
    let first = ToolOutput {
        output: std::sync::Arc::new("running".into()),
        images: Default::default(),
        full_output: None,
        status: ToolOutputStatus::Success,
    };
    let image = rho_inference::types::ImageContent {
        media_type: "image/png".into(),
        data: vec![1, 2, 3],
        detail: rho_inference::types::ImageDetail::Original,
    };
    let later = ToolOutput {
        output: std::sync::Arc::new("later".into()),
        images: std::sync::Arc::new(vec![image.clone()]),
        ..first.clone()
    };
    let agent = {
        let db = RhoDb::open(&path);
        let mut write = db.write().await;
        write.init_agent_tables();
        let id = create(&mut write, None, None);
        for output in [
            ExecOutput::Reply {
                id: "exec-1".try_into().unwrap(),
                body: first,
                first_block_at: UnixMs(1),
                at: UnixMs(2),
            },
            ExecOutput::Report {
                id: "exec-1".try_into().unwrap(),
                body: later,
                at: UnixMs(3),
            },
        ] {
            write.append_agent_event(
                id,
                &AgentEvent::Native(NativeEvent::RequestStarted {
                    input: vec![rho_inference::exec::output(&output)],
                    context: None,
                    wake: None,
                    at: UnixMs(3),
                }),
            );
        }
        write.commit();
        id
    };
    let db = RhoDb::open(&path);
    let (_, events) = db.read().agent_events(agent);
    let native = events.last().unwrap().native_event().unwrap();
    let NativeEvent::RequestStarted { input, .. } = native else {
        panic!("request")
    };
    let ContextBlock::ToolUpdate(update) = input[0].clone() else {
        panic!("later output must not be another result")
    };
    assert_eq!(update.images.as_ref(), &[image]);
    assert_eq!(update.output.as_str(), "later");
}
