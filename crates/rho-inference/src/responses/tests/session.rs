use super::*;
use crate::inference::Inference;
use crate::responses::session::{DebugRun, debug_file_name, provider_debug_dir, redact_image_data};

#[test]
fn chatgpt_codex_config_sets_endpoint_defaults() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        None,
    );

    assert_eq!(session.config.base_url, DEFAULT_CHATGPT_BASE_URL);
    assert_eq!(session.config.responses_config.auto_compaction, None);
    assert_eq!(session.context_window(), Some(272_000));
    assert_eq!(session.auto_compact_token_limit(), Some(232_560));
}

#[test]
fn gpt56_models_use_explicit_context_and_compaction_limits() {
    for model in [
        InferenceModel::Gpt56Sol,
        InferenceModel::Gpt56Luna,
        InferenceModel::Gpt56Terra,
    ] {
        let (_temp, auth) = test_oauth_file("token", None);
        let session = InferenceSession::new_deep(
            Inference::for_test(auth),
            InferenceProfile::default(),
            model,
            PromptCacheKey::from_bytes(*b"testkey2"),
        );

        assert_eq!(session.context_window(), Some(372_000));
        assert_eq!(session.auto_compact_token_limit(), Some(280_000));
        assert_eq!(session.config.responses_config.auto_compaction, None);
    }
}

#[test]
fn astra_uses_the_normal_context_window() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = InferenceSession::new_deep(
        Inference::for_test(auth),
        InferenceProfile::default(),
        InferenceModel::Gpt6Astra,
        PromptCacheKey::from_bytes(*b"testkey2"),
    );

    assert_eq!(
        session.config.responses_config.model.as_str(),
        "gpt-6-astra"
    );
    assert!(session.config.responses_config.model.use_responses_lite());
    assert_eq!(session.context_window(), Some(272_000));
    assert_eq!(session.auto_compact_token_limit(), Some(232_560));
    assert_eq!(session.config.responses_config.auto_compaction, None);
}

#[test]
fn provider_debug_file_name_uses_prompt_cache_key_run_and_sequence() {
    assert_eq!(
        debug_file_name(
            PromptCacheKey::from_bytes(*b"testkey1"),
            DebugRun::from_parts(0x6aac_2e43, 0x1f9c),
            7,
            "request"
        ),
        "746573746b657931-6aac2e431f9c-0007-request.json"
    );
}

/// The bug this run token exists for: the prompt cache key outlives a session
/// task, but the sequence counter restarts with it, so two runs of one agent
/// used to write the same path and the older run was lost.
#[test]
fn provider_debug_file_name_separates_runs_of_one_prompt_cache_key() {
    let key = PromptCacheKey::from_bytes(*b"testkey1");
    assert_ne!(
        debug_file_name(key, DebugRun::from_parts(100, 0), 1, "request"),
        debug_file_name(key, DebugRun::from_parts(200, 0), 1, "request")
    );
}

#[test]
fn provider_debug_dir_is_rho_namespaced() {
    let Some(dir) = provider_debug_dir() else {
        return;
    };

    assert!(dir.ends_with("rho/debug/provider-requests"));
}

#[test]
fn provider_debug_request_redacts_image_data() {
    let mut value = serde_json::json!({"input": [{"content": [{
        "type": "input_image",
        "image_url": "data:image/png;base64,SECRET"
    }]}]});
    redact_image_data(&mut value);
    assert_eq!(
        value["input"][0]["content"][0]["image_url"],
        "[image data redacted]"
    );
    assert!(!value.to_string().contains("SECRET"));
}

#[test]
fn notes_policy_disables_server_compaction_and_can_restore_it() {
    let (_temp, auth) = test_oauth_file("token", None);
    let mut session = InferenceSession::new_deep(
        Inference::for_test(auth),
        InferenceProfile::default(),
        InferenceModel::Gpt55,
        PromptCacheKey::from_bytes(*b"noteskey"),
    );
    assert_eq!(
        session.config.responses_config.auto_compaction,
        Some(232_560)
    );
    session.set_context_rotation(true);
    assert_eq!(session.config.responses_config.auto_compaction, None);
    session.set_context_rotation(false);
    assert_eq!(
        session.config.responses_config.auto_compaction,
        Some(232_560)
    );
    session.set_deep_config(InferenceProfile::default(), InferenceModel::Gpt6Astra);
    session.set_context_rotation(false);
    assert_eq!(
        session.config.responses_config.auto_compaction, None,
        "Lite uses explicit triggers"
    );
}
