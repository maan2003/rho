use super::*;

#[test]
fn chatgpt_codex_config_sets_endpoint_defaults() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = InferenceService::chatgpt_codex_with_auth("gpt-test", auth);

    assert_eq!(session.base_url, DEFAULT_CHATGPT_BASE_URL);
    assert_eq!(session.compaction, None);
}
