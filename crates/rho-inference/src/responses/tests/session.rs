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
    let crate::config::InferenceConfig::Gpt5(config) = session.config.config();
    assert_eq!(config.auto_compaction, None);
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
