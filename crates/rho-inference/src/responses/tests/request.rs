use std::sync::Arc;

use rho_core::ToolOutput;

use super::*;

fn inference_response(
    provider_response_id: Option<&str>,
    items: Vec<InferenceResponseItem>,
) -> Arc<ContextBlock> {
    Arc::new(ContextBlock::InferenceResponse {
        provider_response_id: provider_response_id
            .map(|id| ProviderResponseId::try_from(id).unwrap()),
        items,
    })
}

fn opaque(tag: &str, payload: Value) -> OpaqueProviderData {
    OpaqueProviderData {
        tag: tag.into(),
        data: payload.to_string().into(),
    }
}

#[test]
fn builds_responses_request_with_tools_and_item_timeline() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"cachekey"),
        None,
    );
    let request = inference_request(
        vec![
            user_block("hello"),
            inference_response(
                None,
                vec![InferenceResponseItem::ToolCall {
                    id: tool_call_id("call-1"),
                    name: tool_name("shell_run"),
                    tool_type: ToolType::Function,
                    arguments: r#"{"command":"pwd"}"#.to_owned(),
                }],
            ),
            Arc::new(ContextBlock::ToolResults {
                results: vec![tool_result_success(tool_call_id("call-1"), "done")],
            }),
        ],
        vec![ToolSpec {
            name: tool_name("shell_run"),
            tool_type: ToolType::Function,
            description: "run shell".to_owned(),
            input_schema: json!({"type": "object"}),
            format: None,
        }],
    );

    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["model"], "gpt-test");
    assert!(json.get("temperature").is_none());
    assert!(json.get("max_output_tokens").is_none());
    assert_eq!(json["input"][0]["role"], "user");
    assert_eq!(json["input"][1]["type"], "function_call");
    assert_eq!(json["input"][1]["name"], "shell_run");
    assert_eq!(json["input"][1]["arguments"], r#"{"command":"pwd"}"#);
    assert_eq!(json["input"][2]["type"], "function_call_output");
    assert_eq!(json["tools"][0]["name"], "shell_run");
    assert_eq!(json["tool_choice"], "auto");
    assert_eq!(json["store"], false);
    assert_eq!(json["reasoning"]["effort"], "medium");
    assert_eq!(json["text"]["verbosity"], "medium");
    assert_eq!(json["service_tier"], "default");
    assert_eq!(
        json["prompt_cache_key"],
        "b6df7bf9-ec1a-8f8e-bff2-23d552ce5bcf"
    );
    assert_eq!(json["include"][0], "reasoning.encrypted_content");
}

#[test]
fn omits_tool_choice_without_declared_tools() {
    let session = test_inference_service("gpt-test");
    let request = inference_request(vec![user_block("hello")], Vec::new());

    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("tool_choice").is_none());
}

#[test]
fn renders_agent_mail_with_supplied_short_label() {
    let session = test_inference_service("gpt-test");
    let agent_id = rho_core::AgentId::from_counter(1, &rho_core::AgentIdDomain(0)).unwrap();
    let mut agent_id_labels = std::collections::BTreeMap::new();
    agent_id_labels.insert(agent_id, Arc::from("ag-h6u7"));
    let request = InferenceRequest {
        instructions: Arc::from("You are rho."),
        input: vec![Arc::new(ContextBlock::UserMessage {
            sender: rho_core::MessageSender::Agent { id: agent_id },
            content: vec![ContentPart::Text {
                text: "done".to_owned(),
            }],
        })],
        agent_id_labels,
        tools: Arc::from([]),
    };

    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(
        json["input"][0]["content"][0]["text"],
        "Message Type: MESSAGE\nSender: ag-h6u7\nPayload:\ndone"
    );
}

#[test]
fn stamps_phase_on_assistant_messages_when_supported() {
    let request = inference_request(
        vec![inference_response(
            None,
            vec![
                assistant_message_with_phase("commentary", MessagePhase::Commentary),
                assistant_message("legacy answer"),
            ],
        )],
        Vec::new(),
    );

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["phase"], "commentary");
    assert_eq!(json["input"][1]["phase"], "final_answer");
}

#[test]
fn serializes_configured_reasoning_effort() {
    let request = inference_request(vec![user_block("hello")], Vec::new());

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["reasoning"]["effort"], "medium");
    assert_eq!(json["reasoning"]["context"], "all_turns");
    assert_eq!(json["reasoning"]["summary"], "auto");
}

#[test]
fn serializes_configured_reasoning_context() {
    let (_temp, auth) = test_oauth_file("token", None);
    let mut session = InferenceSession::new_deep(
        auth,
        DeepConfig {
            effort: DeepEffort::Medium,
            fast_mode: false,
        },
        PromptCacheKey::from_bytes(*b"testkey0"),
    );
    session.responses_config.model = ResponsesModel::Test("gpt-test".to_owned());
    session.responses_config.reasoning_context = ReasoningContext::CurrentTurn;
    session.responses_config.text_verbosity = TextVerbosity::Medium;
    let request = inference_request(vec![user_block("hello")], Vec::new());

    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["reasoning"]["context"], "current_turn");
}

#[test]
fn serializes_required_instructions() {
    let request = InferenceRequest {
        instructions: Arc::from("You are rho."),
        input: vec![user_block("hello")],
        agent_id_labels: std::collections::BTreeMap::new(),
        tools: Arc::from([]),
    };

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["instructions"], "You are rho.");
}

#[test]
fn serializes_prompt_cache_key() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"cachekey"),
        None,
    );
    let request = inference_request(vec![user_block("hello")], Vec::new());

    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(
        json["prompt_cache_key"],
        "b6df7bf9-ec1a-8f8e-bff2-23d552ce5bcf"
    );
}

#[test]
fn previous_response_hint_slices_input_in_provider() {
    let request = inference_request(
        vec![
            user_block("first"),
            inference_response(Some("resp_1"), vec![assistant_message("done")]),
            user_block("second"),
        ],
        Vec::new(),
    );

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        Some("resp_1"),
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["previous_response_id"], "resp_1");
    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "second");
}

#[test]
fn previous_response_hint_requires_connection_cached_match() {
    let request = inference_request(
        vec![
            user_block("first"),
            inference_response(Some("resp_1"), vec![assistant_message("done")]),
            user_block("second"),
        ],
        Vec::new(),
    );

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        Some("other_resp"),
    );
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("previous_response_id").is_none());
    assert_eq!(json["input"].as_array().unwrap().len(), 3);
}

#[test]
fn previous_response_without_valid_boundary_replays_full_history() {
    let request = inference_request(
        vec![
            user_block("first"),
            inference_response(None, vec![assistant_message("done")]),
            user_block("second"),
        ],
        Vec::new(),
    );

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("previous_response_id").is_none());
    assert_eq!(json["input"].as_array().unwrap().len(), 3);
}

#[test]
fn stale_previous_response_error_builds_full_replay_request() {
    let request = inference_request(
        vec![
            user_block("first"),
            inference_response(Some("resp_1"), vec![assistant_message("done")]),
            user_block("second"),
        ],
        Vec::new(),
    );
    let sliced = serde_json::to_value(ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request.clone(),
        Some("resp_1"),
    ))
    .unwrap();
    assert_eq!(sliced["previous_response_id"], "resp_1");
    assert_eq!(sliced["input"].as_array().unwrap().len(), 1);

    // A stale-`previous_response` error is recognized, and the full replay drops
    // `previous_response_id` and forwards the whole history.
    assert!(is_stale_previous_response_error(&anyhow::anyhow!(
        "stream error: previous_response_id expired"
    )));
    let replay = serde_json::to_value(ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    ))
    .unwrap();
    assert!(replay.get("previous_response_id").is_none());
    assert_eq!(replay["input"].as_array().unwrap().len(), 3);
}

#[test]
fn non_stale_previous_response_error_is_not_classified_stale() {
    assert!(!is_stale_previous_response_error(&anyhow::anyhow!(
        "stream error: rate limit"
    )));
    assert!(is_stale_previous_response_error(&anyhow::anyhow!(
        "response not found"
    )));
}

#[test]
fn chatgpt_codex_request_omits_compaction_request_by_default() {
    let (_temp, auth) = test_oauth_file("token", None);
    let request = inference_request(vec![user_block("hello")], Vec::new());

    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        None,
    );
    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("context_management").is_none());
    assert_eq!(json["input"][0]["content"][0]["text"], "hello");
    assert_eq!(json["store"], false);
}

#[test]
fn configured_compaction_threshold_overrides_provider_default() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        Some(AutoCompaction::Threshold(42_000)),
    );
    let request = inference_request(vec![user_block("hello")], Vec::new());

    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["content"][0]["text"], "hello");
    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["context_management"][0]["type"], "compaction");
    assert_eq!(json["context_management"][0]["compact_threshold"], 42_000);
}

#[test]
fn chatgpt_codex_with_compaction_requests_configured_threshold() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        Some(AutoCompaction::Threshold(232_560)),
    );
    let request = inference_request(vec![user_block("hello")], Vec::new());

    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["content"][0]["text"], "hello");
    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["context_management"][0]["compact_threshold"], 232_560);
}

#[test]
fn compaction_trigger_is_sent_as_provider_input_item() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        Some(AutoCompaction::Threshold(42_000)),
    );
    let request = inference_request(vec![Arc::new(ContextBlock::CompactionTrigger)], Vec::new());

    let body = ResponsesRequest::from_inference_request(&session, request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["context_management"][0]["type"], "compaction");
    assert_eq!(json["context_management"][0]["compact_threshold"], 42_000);
    assert_eq!(json["input"][0]["type"], "compaction_trigger");
}

#[test]
fn compaction_replay_trims_before_latest_compaction_item() {
    let request = inference_request(
        vec![
            user_block("before"),
            inference_response(
                Some("resp_compaction"),
                vec![InferenceResponseItem::Compaction(opaque(
                    "compaction",
                    json!({"type": "compaction", "id": "cmp_1"}),
                ))],
            ),
            user_block("after"),
        ],
        Vec::new(),
    );

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"].as_array().unwrap().len(), 2);
    assert_eq!(json["input"][0]["type"], "compaction");
    assert_eq!(json["input"][1]["content"][0]["text"], "after");
    assert!(json.get("context_management").is_none());
}

#[test]
fn replays_reasoning_provider_item() {
    let reasoning = InferenceResponseItem::EncryptedReasoning {
        payload: opaque(
            "reasoning",
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "sealed"}),
        ),
        summary: Vec::new(),
    };
    let request = inference_request(
        vec![
            inference_response(None, vec![reasoning]),
            user_block("after"),
        ],
        Vec::new(),
    );

    let body = serde_json::to_value(ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    ))
    .unwrap();
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
    assert_eq!(body["input"][0]["encrypted_content"], "sealed");
}

#[test]
fn does_not_replay_unknown_provider_items() {
    let request = inference_request(
        vec![
            inference_response(
                None,
                vec![InferenceResponseItem::Unknown(opaque(
                    "computer_call",
                    json!({"type": "computer_call", "id": "cc_1"}),
                ))],
            ),
            user_block("after"),
        ],
        Vec::new(),
    );

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "after");
}

#[test]
fn serializes_custom_tool_calls_and_results() {
    let result = ToolResult {
        call_id: tool_call_id("call-1"),
        tool_type: ToolType::Custom,
        body: ToolOutput {
            output: Arc::from("custom output".to_owned()),
            status: rho_core::ToolOutputStatus::Success,
        },
        started_at: rho_core::UnixMs(1),
        finished_at: rho_core::UnixMs(2),
        metadata: None,
    };
    let request = inference_request(
        vec![
            inference_response(
                None,
                vec![InferenceResponseItem::ToolCall {
                    id: tool_call_id("call-1"),
                    name: tool_name("patch"),
                    tool_type: ToolType::Custom,
                    arguments: "*** Begin Patch\n*** End Patch".to_owned(),
                }],
            ),
            Arc::new(ContextBlock::ToolResults {
                results: vec![result],
            }),
        ],
        vec![ToolSpec {
            name: tool_name("patch"),
            tool_type: ToolType::Custom,
            description: "apply a patch".to_owned(),
            input_schema: Value::Null,
            format: Some(ToolFormat::Grammar {
                syntax: ToolGrammarSyntax::Lark,
                definition: "start: /.+/".to_owned(),
            }),
        }],
    );

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test"),
        request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["tools"][0]["type"], "custom");
    assert_eq!(json["tools"][0]["format"]["type"], "grammar");
    assert_eq!(json["tools"][0]["format"]["syntax"], "lark");
    assert_eq!(json["tools"][0]["format"]["definition"], "start: /.+/");
    assert_eq!(json["input"][0]["type"], "custom_tool_call");
    assert_eq!(json["input"][0]["id"], "ctc_call-1");
    assert_eq!(json["input"][0]["input"], "*** Begin Patch\n*** End Patch");
    assert_eq!(json["input"][1]["type"], "custom_tool_call_output");
    assert_eq!(json["input"][1]["output"], "custom output");
}
