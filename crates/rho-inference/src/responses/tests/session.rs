use super::*;
use crate::responses::session::{debug_file_name, provider_debug_dir};

#[test]
fn chatgpt_codex_config_sets_endpoint_defaults() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        None,
    );

    assert_eq!(session.base_url, DEFAULT_CHATGPT_BASE_URL);
    assert_eq!(session.responses_config.auto_compaction, None);
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
            auth,
            InferenceProfile::default(),
            model,
            PromptCacheKey::from_bytes(*b"testkey2"),
        );

        assert_eq!(session.context_window(), Some(372_000));
        assert_eq!(session.auto_compact_token_limit(), Some(280_000));
        assert_eq!(session.responses_config.auto_compaction, None);
    }
}

#[test]
fn provider_debug_file_name_uses_prompt_cache_key_and_sequence() {
    assert_eq!(
        debug_file_name(PromptCacheKey::from_bytes(*b"testkey1"), 7, "request"),
        "746573746b657931-0007-request.json"
    );
}

#[test]
fn provider_debug_dir_is_rho_namespaced() {
    let Some(dir) = provider_debug_dir() else {
        return;
    };

    assert!(dir.ends_with("rho/debug/provider-requests"));
}
