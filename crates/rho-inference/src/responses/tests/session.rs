use super::*;

#[test]
fn chatgpt_codex_config_sets_endpoint_defaults() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(auth, "gpt-test", None, None);

    assert_eq!(session.base_url, DEFAULT_CHATGPT_BASE_URL);
    let crate::config::InferenceConfig::Gpt5(config) = session.config.config();
    assert_eq!(config.auto_compaction, None);
}
