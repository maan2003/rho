use rho_core::{ContentPart, UnixMs};
use rho_db::RhoDb;
use rho_inference::PromptCacheKey;
use rho_workset::WorkspaceInfo;

use super::*;

#[test]
fn astra_bindings_round_trip() {
    for binding in [
        SessionBinding::ResponsesAstra(InferenceProfile::default()),
        SessionBinding::AdvisorAstra(InferenceProfile::default()),
    ] {
        let mut encoded = bytes::BytesMut::new();
        senax_encoder::encode_to(&binding, &mut encoded).unwrap();
        let decoded = <SessionBinding as senax_encoder::Decoder>::decode(&mut encoded).unwrap();
        assert_eq!(decoded, binding);
    }
}

#[test]
fn python_suffixed_bindings_fold_into_their_models() {
    #[derive(Encode)]
    #[allow(dead_code)]
    enum LegacySessionBinding {
        ResponsesSolPython(InferenceProfile),
        ClaudeFablePython { effort: ClaudeEffort },
    }
    let profile = InferenceProfile {
        effort: ReasoningEffort::Medium,
        fast_mode: false,
    };
    for (legacy, expected) in [
        (
            LegacySessionBinding::ResponsesSolPython(profile),
            SessionBinding::ResponsesSol(profile),
        ),
        (
            LegacySessionBinding::ClaudeFablePython {
                effort: ClaudeEffort::High,
            },
            SessionBinding::ClaudeFable {
                effort: ClaudeEffort::High,
            },
        ),
    ] {
        let mut encoded = bytes::BytesMut::new();
        senax_encoder::encode_to(&legacy, &mut encoded).unwrap();
        let decoded = <SessionBinding as senax_encoder::Decoder>::decode(&mut encoded).unwrap();
        assert_eq!(decoded, expected);
    }
}

#[test]
fn legacy_high_engineer_binding_stays_on_sol() {
    let binding = SessionBinding::ResponsesSol(InferenceProfile {
        effort: ReasoningEffort::Xhigh,
        fast_mode: false,
    });
    let mut encoded = bytes::BytesMut::new();
    senax_encoder::encode_to(&binding, &mut encoded).unwrap();
    let decoded = <SessionBinding as senax_encoder::Decoder>::decode(&mut encoded).unwrap();

    assert_eq!(decoded, binding);
    assert_eq!(decoded.deep_model(), Some(InferenceModel::Gpt56Sol));
    assert_eq!(
        decoded.agent_role(),
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
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
        vec![test_workspace()],
        AgentRole::default(),
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
        None,
        vec![test_workspace()],
        AgentRole::default(),
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
        None,
        vec![test_workspace()],
        AgentRole::default(),
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
        None,
        vec![test_workspace()],
        AgentRole::default(),
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
        None,
        vec![test_workspace()],
        AgentRole::default(),
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
        profile(EngineerIntelligence::High),
        SessionBinding::ResponsesAstra(InferenceProfile {
            effort: ReasoningEffort::Medium,
            ..
        })
    ));
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
    for intelligence in [EngineerIntelligence::Ultra, EngineerIntelligence::Alt] {
        assert!(
            profile(intelligence).claude_python(),
            "every Claude engineer works in the Python notebook"
        );
    }
    assert!(matches!(
        profile(EngineerIntelligence::Gemini),
        SessionBinding::AntigravityFlashLow(InferenceProfile {
            effort: ReasoningEffort::Medium,
            fast_mode: false,
        })
    ));
    assert_eq!(
        profile(EngineerIntelligence::Gemini).deep_model(),
        Some(InferenceModel::Gemini37FlashLow)
    );
    assert!(matches!(
        AgentRole::Advisor {
            intelligence: AdvisorIntelligence::High,
        }
        .session_profile()
        .unwrap(),
        SessionBinding::AdvisorAstra(InferenceProfile {
            effort: ReasoningEffort::Medium,
            fast_mode: false,
        })
    ));
    assert!(matches!(
        AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Medium,
        }
        .session_profile()
        .unwrap(),
        SessionBinding::AdvisorSol(InferenceProfile {
            effort: ReasoningEffort::High,
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
        })
    ));
}

use crate::{InputKind, MessageDelivery, MessageSender, QueuedInput};

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
pub(crate) fn test_workspace() -> WorkspaceInfo {
    WorkspaceInfo::Workset {
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
        vec![test_workspace()],
        AgentRole::default(),
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
        vec![test_workspace()],
        AgentRole::default(),
        AgentRole::default().session_profile().unwrap(),
        test_agent_runtime(),
        None,
    );
    let engineer = write.alloc_agent_id();
    write.create_agent(
        UnixMs(2),
        engineer,
        None,
        vec![test_workspace()],
        AgentRole::default(),
        AgentRole::default().session_profile().unwrap(),
        test_agent_runtime(),
        Some(pm),
    );
    write.commit();

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
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        SessionBinding::ResponsesGpt55(InferenceProfile::default())
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
        vec![test_workspace()],
        AgentRole::default(),
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        None,
    );
    write.commit();

    let read = db.read();
    assert_eq!(
        read.get_agent(agent_id).config.workdirs,
        vec![test_workspace()]
    );
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

fn create(
    write: &mut rho_db::WriteTxn,
    spawn_name: Option<&str>,
    parent: Option<AgentId>,
) -> AgentId {
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        UnixMs(1),
        agent_id,
        spawn_name.map(str::to_owned),
        vec![test_workspace()],
        AgentRole::default(),
        SessionBinding::ResponsesGpt55(InferenceProfile::default()),
        test_agent_runtime(),
        parent,
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
    let tail = read
        .agent_presentation_source_tail(agent_id, usize::MAX)
        .into_iter()
        .map(|(pos, _)| pos.pos)
        .collect::<Vec<_>>();
    assert_eq!(tail, [1, 4]);
}

#[tokio::test]
async fn a_presentation_update_is_rejected_when_its_source_was_rewound_away() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut write = db.write().await;
    write.init_agent_tables();
    let agent_id = create(&mut write, None, None);
    let first = write.append_agent_event(agent_id, &user_event("first"));
    let second = write.append_agent_event(agent_id, &user_event("later"));
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
    write.append_agent_event(agent_id, &user_event("even later"));

    write.rewind_agent(UnixMs(3), agent_id, second);

    // A completion based on the discarded input cannot write after the
    // rewind, even if it reaches the serialized loop late.
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
    assert_eq!(record.activity.as_deref(), Some("reading first request"));
}

#[tokio::test]
async fn the_head_folds_title_activity_turns_and_user_contact() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    write.init_agent_tables();
    let root = create(&mut write, None, None);
    let child = create(&mut write, Some("named"), Some(root));
    write.tell_turn(UnixMs(2), child, TurnEdge::Started);
    write.append_agent_event(
        child,
        &AgentEvent::Presented {
            title: PresentationField::Set("story-log".to_owned()),
            activity: PresentationField::Set("writing the fold".to_owned()),
            at: UnixMs(3),
        },
    );
    write.commit();

    let head = db.read().get_agent(child);
    // The spawner's name beats the sidecar's title.
    assert_eq!(head.title(), Some("named"));
    assert_eq!(head.generated_title.as_deref(), Some("story-log"));
    assert_eq!(head.activity.as_deref(), Some("writing the fold"));
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
async fn the_journal_names_every_row_in_write_order() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));
    let mut feed = crate::mirror::feed(&db);

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
    while let Ok(crate::mirror::Feed::Appended(appended)) = feed.try_recv() {
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
