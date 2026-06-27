use super::*;

#[tokio::test]
async fn websocket_wait_sends_keepalive_ping_before_event_timeout() {
    let mut socket = PendingSocket::default();
    let mut last_event_at = tokio::time::Instant::now();
    let mut ping_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_millis(5),
        Duration::from_millis(5),
    );

    let error = next_ws_message(
        &mut socket,
        Some(Duration::from_millis(25)),
        &mut last_event_at,
        &mut ping_interval,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("produced no events"));
    assert!(
        socket
            .sent
            .iter()
            .any(|message| matches!(message, WsMessage::Ping(_)))
    );
}

#[test]
fn websocket_request_uses_responses_url_and_prompt_cache_headers() {
    let (_temp, auth) = test_oauth_file("token", Some("acct_1"));
    let mut session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        None,
    );
    session.base_url = "https://chatgpt.com/backend-api".to_owned();

    let auth = session.auth.resolve().unwrap();
    let request = build_ws_request(&session, Some("thread-1"), &auth).unwrap();

    assert_eq!(
        request.uri(),
        "wss://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(request.headers()["OpenAI-Beta"], OPENAI_BETA_WS);
    assert_eq!(request.headers()["Authorization"], "Bearer token");
    assert_eq!(request.headers()["session-id"], "thread-1");
    assert_eq!(request.headers()["thread-id"], "thread-1");
    assert_eq!(request.headers()["chatgpt-account-id"], "acct_1");
}

#[test]
fn websocket_request_uses_oauth_bearer_without_account_header() {
    let (_temp, auth) = test_oauth_file("sk-test", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        None,
    );

    let auth = session.auth.resolve().unwrap();
    let request = build_ws_request(&session, None, &auth).unwrap();

    assert_eq!(request.headers()["Authorization"], "Bearer sk-test");
    assert!(!request.headers().contains_key("chatgpt-account-id"));
}

#[test]
fn websocket_request_uses_oauth_file_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let file = test_oauth_file_in(temp.path(), "chatgpt").unwrap();
    file.save(&ResponsesOAuthCredentials {
        access_token: "oauth-access".to_owned(),
        refresh_token: "oauth-refresh".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("acct_file".to_owned()),
        client_secret: *b"filesecrfilesecrfilesecrfilesecr",
    })
    .unwrap();
    let mut session = test_inference_service_with(
        InferenceAuth::oauth_file(file.path()),
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        None,
    );
    session.base_url = "https://chatgpt.com/backend-api".to_owned();

    let auth = session.auth.resolve().unwrap();
    let request = build_ws_request(&session, Some("thread-1"), &auth).unwrap();

    assert_eq!(request.headers()["Authorization"], "Bearer oauth-access");
    assert_eq!(request.headers()["chatgpt-account-id"], "acct_file");
}

#[test]
fn websocket_envelope_has_response_create_type() {
    let body = ResponsesRequest {
        model: "gpt-test".to_owned(),
        instructions: None,
        input: Vec::new(),
        store: Some(false),
        tools: Vec::new(),
        tool_choice: None,
        text: None,
        reasoning: None,
        service_tier: None,
        include: Vec::new(),
        prompt_cache_key: uuid::uuid!("b6df7bf9-ec1a-8f8e-bff2-23d552ce5bcf"),
        context_management: Vec::new(),
        previous_response_id: None,
    };

    let json = serde_json::to_value(WsResponseCreate {
        ty: "response.create",
        body,
    })
    .unwrap();

    assert_eq!(json["type"], "response.create");
    assert_eq!(json["model"], "gpt-test");
}
