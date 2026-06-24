use super::*;

#[test]
fn websocket_pool_key_requires_chatgpt_pool_and_prompt_cache_key() {
    let auth = ResolvedAuth {
        bearer_token: "token".to_owned(),
        account_id: Some("acct".to_owned()),
    };
    let (_temp, inference_auth) = test_oauth_file("token", None);
    let session =
        InferenceService::new("gpt-test", inference_auth).with_prompt_cache_key("thread-1");
    let mut body = ResponsesRequest::from_inference_request(
        &session,
        InferenceRequest {
            input: vec![ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "hello")),
                }],
            }],
            tools: Vec::new(),
        },
    );

    let key = WebSocketPoolKey::from_request(&session, &body, &auth).unwrap();

    assert_eq!(key.base_url, DEFAULT_CHATGPT_BASE_URL);
    assert_eq!(key.account_id.as_deref(), Some("acct"));
    assert_eq!(key.thread_id.as_str(), "thread-1");

    body.prompt_cache_key = None;
    assert!(WebSocketPoolKey::from_request(&session, &body, &auth).is_none());
}

#[tokio::test]
async fn websocket_wait_sends_keepalive_ping_before_event_timeout() {
    let mut socket = PendingSocket::default();
    let mut last_event_at = tokio::time::Instant::now();
    let mut ping_interval = Some(tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_millis(5),
        Duration::from_millis(5),
    ));

    let error = next_ws_message(
        &mut socket,
        Duration::from_millis(25),
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
    let mut session = InferenceService::new("gpt-test", auth);
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
    let session = InferenceService::new("gpt-test", auth);

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
    })
    .unwrap();
    let mut session = InferenceService::new("gpt-test", InferenceAuth::oauth_file(file.path()));
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
        include: Vec::new(),
        prompt_cache_key: Some("thread-1".to_owned()),
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
