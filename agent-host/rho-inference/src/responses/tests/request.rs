use std::sync::Arc;

use super::*;
use crate::inference::Inference;
use crate::types::ToolOutput;

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

#[test]
fn title_session_uses_luna_fast_profile() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session =
        InferenceSession::new_title(Inference::for_test(auth), PromptCacheKey::generate());

    assert_eq!(
        session.config.responses_config.model,
        ResponsesModel::Gpt6Luna
    );
    assert_eq!(
        session.config.responses_config.reasoning_context,
        ReasoningContext::AllTurns
    );
    assert_eq!(
        session.config.responses_config.effort,
        ResponsesEffort::Medium
    );
    assert_eq!(
        session.config.responses_config.text_verbosity,
        TextVerbosity::Low
    );
    assert_eq!(
        session.config.responses_config.service_tier,
        ServiceTier::Priority
    );
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
    let request = inference_request(vec![
        user_block("hello"),
        inference_response(
            None,
            vec![InferenceResponseItem::ToolCall {
                provider_specific: provider_specific(
                    "function_call",
                    json!({
                        "type": "function_call",
                        "id": "fc_call-1",
                        "call_id": "call-1",
                        "name": "shell_run",
                        "arguments": r#"{"command":"pwd"}"#,
                    }),
                ),
                id: tool_call_id("call-1"),
                name: tool_name("shell_run"),
                tool_type: ToolType::Function,
                arguments: r#"{"command":"pwd"}"#.to_owned(),
            }],
        ),
        Arc::new(ContextBlock::ToolResults {
            results: vec![{
                let mut result = tool_result_success(tool_call_id("call-1"), "done");
                result.body.full_output = Some(Arc::new("the complete host record".to_owned()));
                result
            }],
        }),
        Arc::new(ContextBlock::CompactionTrigger),
    ]);

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();
    assert!(json.get("generate").is_none());

    assert_eq!(json["model"], "gpt-test");
    assert!(json.get("temperature").is_none());
    assert!(json.get("max_output_tokens").is_none());
    assert_eq!(json["input"][0]["role"], "user");
    assert_eq!(json["input"][1]["type"], "function_call");
    assert_eq!(json["input"][1]["name"], "shell_run");
    assert_eq!(json["input"][1]["arguments"], r#"{"command":"pwd"}"#);
    assert_eq!(json["input"][2]["type"], "function_call_output");
    assert_eq!(json["input"][2]["call_id"], "call-1");
    assert_eq!(json["input"][2]["output"], "done");
    assert_eq!(json["input"][3]["type"], "compaction_trigger");
    assert_eq!(json["tools"][0]["name"], "exec");
    assert_eq!(json["tool_choice"], "auto");
    assert!(json.get("parallel_tool_calls").is_none());
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
fn text_completion_declares_no_tools() {
    let mut session = test_inference_service("gpt-test");
    session.config.mode = super::super::session::InferenceSessionMode::Title;
    let request = inference_request(vec![user_block("hello")]);

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("tool_choice").is_none());
}

#[test]
fn renders_text_and_image_user_content() {
    let request = inference_request(vec![Arc::new(ContextBlock::UserMessage {
        sender: crate::types::MessageSender::User,
        content: vec![
            ContentPart::Text {
                text: "inspect".to_owned(),
            },
            ContentPart::Image {
                media_type: "image/png".to_owned(),
                data: vec![1, 2, 3],
            },
        ],
    })]);
    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();
    assert_eq!(
        json["input"][0]["content"][0],
        json!({"type":"input_text", "text":"inspect"})
    );
    assert_eq!(
        json["input"][0]["content"][1],
        json!({
            "type":"input_image", "image_url":"data:image/png;base64,AQID"
        })
    );
}

#[test]
fn renders_agent_mail_with_supplied_short_label() {
    let session = test_inference_service("gpt-test");
    let agent_id =
        rho_agent_types::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
    let mut agent_id_labels = std::collections::BTreeMap::new();
    agent_id_labels.insert(agent_id, Arc::from("eng-h6u7"));
    let request = InferenceRequest {
        instructions: Arc::from("You are rho."),
        input: vec![Arc::new(ContextBlock::UserMessage {
            sender: crate::types::MessageSender::Agent { id: agent_id },
            content: vec![ContentPart::Text {
                text: "done".to_owned(),
            }],
        })],
        agent_id_labels,
    };

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(
        json["input"][0]["content"][0]["text"],
        "Message Type: MESSAGE\nSender: eng-h6u7\nPayload:\ndone"
    );
}

#[test]
fn stamps_phase_on_assistant_messages_when_supported() {
    let request = inference_request(vec![inference_response(
        None,
        vec![
            assistant_message_with_phase("commentary", MessagePhase::Commentary),
            assistant_message("legacy answer"),
        ],
    )]);

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["phase"], "commentary");
    assert_eq!(json["input"][1]["phase"], "final_answer");
}

#[test]
fn serializes_configured_reasoning_effort() {
    let request = inference_request(vec![user_block("hello")]);

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
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
        Inference::for_test(auth),
        InferenceProfile {
            effort: ReasoningEffort::High,
            fast_mode: false,
        },
        InferenceModel::Gpt6Sol,
        PromptCacheKey::from_bytes(*b"testkey0"),
    );
    session.config.responses_config.model = ResponsesModel::Test("gpt-test".to_owned());
    session.config.responses_config.reasoning_context = ReasoningContext::CurrentTurn;
    session.config.responses_config.text_verbosity = TextVerbosity::Medium;
    let request = inference_request(vec![user_block("hello")]);

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["reasoning"]["context"], "current_turn");
    assert_eq!(json["reasoning"]["effort"], "high");
}

#[test]
fn serializes_required_instructions() {
    let request = InferenceRequest {
        instructions: Arc::from("You are rho."),
        input: vec![user_block("hello")],
        agent_id_labels: std::collections::BTreeMap::new(),
    };

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
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
    let request = inference_request(vec![user_block("hello")]);

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(
        json["prompt_cache_key"],
        "b6df7bf9-ec1a-8f8e-bff2-23d552ce5bcf"
    );
}

#[test]
fn previous_response_hint_slices_input_in_provider() {
    let request = inference_request(vec![
        user_block("first"),
        inference_response(Some("resp_1"), vec![assistant_message("done")]),
        user_block("second"),
    ]);

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        Some("resp_1"),
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["previous_response_id"], "resp_1");
    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "second");
}

#[test]
fn previous_response_hint_requires_connection_cached_match() {
    let request = inference_request(vec![
        user_block("first"),
        inference_response(Some("resp_1"), vec![assistant_message("done")]),
        user_block("second"),
    ]);

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        Some("other_resp"),
    );
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("previous_response_id").is_none());
    assert_eq!(json["input"].as_array().unwrap().len(), 3);
}

#[test]
fn previous_response_without_valid_boundary_replays_full_history() {
    let request = inference_request(vec![
        user_block("first"),
        inference_response(None, vec![assistant_message("done")]),
        user_block("second"),
    ]);

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("previous_response_id").is_none());
    assert_eq!(json["input"].as_array().unwrap().len(), 3);
}

#[test]
fn stale_previous_response_error_builds_full_replay_request() {
    let request = inference_request(vec![
        user_block("first"),
        inference_response(Some("resp_1"), vec![assistant_message("done")]),
        user_block("second"),
    ]);
    let sliced = serde_json::to_value(ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
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
        &test_inference_service("gpt-test").config,
        &request,
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
    let request = inference_request(vec![user_block("hello")]);

    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        None,
    );
    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
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
        Some(42_000),
    );
    let request = inference_request(vec![user_block("hello")]);

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
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
        Some(232_560),
    );
    let request = inference_request(vec![user_block("hello")]);

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["content"][0]["text"], "hello");
    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["context_management"][0]["compact_threshold"], 232_560);
}

#[test]
fn compaction_trigger_is_the_last_provider_input_item() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"testkey1"),
        Some(42_000),
    );
    let request = inference_request(vec![
        Arc::new(ContextBlock::CompactionTrigger),
        user_block("queued after compaction"),
        Arc::new(ContextBlock::CompactionTrigger),
    ]);

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["context_management"][0]["type"], "compaction");
    assert_eq!(json["context_management"][0]["compact_threshold"], 42_000);
    let input = json["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["content"][0]["text"], "queued after compaction");
    assert_eq!(input[1]["type"], "compaction_trigger");
}

#[test]
fn tool_eviction_mode_allows_explicit_compaction_fallback() {
    let (_temp, auth) = test_oauth_file("token", None);
    let mut session = test_inference_service_with(
        auth,
        "gpt-test",
        PromptCacheKey::from_bytes(*b"noteskey"),
        Some(42_000),
    );
    // Automatic server compaction is disabled, but explicit fallback remains
    // available.
    let request = inference_request(vec![
        user_block("discarded"),
        user_block("retained"),
        Arc::new(ContextBlock::ContextRotation { retain_from: 1 }),
        Arc::new(ContextBlock::CompactionTrigger),
        user_block("prepare notes"),
    ]);
    session.set_context_rotation(true);
    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();
    assert!(json.get("context_management").is_none());
    assert!(json["input"].to_string().contains("compaction_trigger"));
    assert!(!json["input"].to_string().contains("discarded"));
    assert!(json["input"].to_string().contains("retained"));

    session.set_context_rotation(false);
    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();
    assert_eq!(
        json["input"].as_array().unwrap().last().unwrap()["type"],
        "compaction_trigger"
    );
    assert!(!json["input"].to_string().contains("discarded"));
}

#[test]
fn compaction_replay_trims_before_latest_compaction_item() {
    let request = inference_request(vec![
        user_block("before"),
        inference_response(
            Some("resp_compaction"),
            vec![InferenceResponseItem::Compaction {
                provider_specific: provider_specific(
                    "compaction",
                    json!({
                        "type": "compaction",
                        "id": "cmp_1",
                        "encrypted_content": "compacted",
                    }),
                ),
            }],
        ),
        user_block("after"),
    ]);

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"].as_array().unwrap().len(), 2);
    assert_eq!(json["input"][0]["type"], "compaction");
    assert_eq!(json["input"][0]["encrypted_content"], "compacted");
    assert_eq!(json["input"][1]["content"][0]["text"], "after");
    assert!(json.get("context_management").is_none());
}

#[test]
fn skips_compaction_without_encrypted_content() {
    let request = inference_request(vec![
        inference_response(
            Some("resp_compaction"),
            vec![InferenceResponseItem::Compaction {
                provider_specific: Box::new(OpenAiResponsesProviderData::Compaction {
                    item_id: crate::types::ProviderResponseItemId::try_from("cmp_1").unwrap(),
                    encrypted_content: String::new(),
                }),
            }],
        ),
        user_block("after"),
    ]);

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "after");
}

#[test]
fn replays_reasoning_provider_item() {
    let reasoning = InferenceResponseItem::EncryptedReasoning {
        provider_specific: provider_specific(
            "reasoning",
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "sealed"}),
        ),
        summary: vec!["kept".to_owned()],
    };
    let request = inference_request(vec![
        inference_response(None, vec![reasoning]),
        user_block("after"),
    ]);

    let body = serde_json::to_value(ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        None,
    ))
    .unwrap();
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
    assert_eq!(body["input"][0]["encrypted_content"], "sealed");
    assert_eq!(
        body["input"][0]["summary"],
        json!([{"type": "summary_text", "text": "kept"}])
    );
}

#[test]
fn serializes_custom_tool_calls_and_results() {
    let result = ToolResult {
        call_id: tool_call_id("call-1"),
        tool_type: ToolType::Custom,
        body: ToolOutput {
            full_output: None,
            images: Arc::new(vec![crate::types::ImageContent {
                media_type: "image/png".to_owned(),
                data: vec![1, 2, 3],
                detail: crate::types::ImageDetail::Original,
            }]),
            output: Arc::from("custom output".to_owned()),
            status: rho_agent_types::ToolOutputStatus::Success,
        },
        started_at: rho_agent_types::UnixMs(1),
        finished_at: rho_agent_types::UnixMs(2),
        metadata: None,
    };
    let request = inference_request(vec![
        inference_response(
            None,
            vec![InferenceResponseItem::ToolCall {
                provider_specific: provider_specific(
                    "custom_tool_call",
                    json!({
                        "type": "custom_tool_call",
                        "id": "ctc_call-1",
                        "call_id": "call-1",
                        "name": "patch",
                        "input": "*** Begin Patch\n*** End Patch",
                    }),
                ),
                id: tool_call_id("call-1"),
                name: tool_name("patch"),
                tool_type: ToolType::Custom,
                arguments: "*** Begin Patch\n*** End Patch".to_owned(),
            }],
        ),
        Arc::new(ContextBlock::ToolResults {
            results: vec![result],
        }),
    ]);

    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["tools"][0]["type"], "custom");
    assert_eq!(json["tools"][0]["format"]["type"], "text");
    assert_eq!(json["tools"][0]["name"], "exec");
    assert_eq!(json["input"][0]["type"], "custom_tool_call");
    assert_eq!(json["input"][0]["id"], "ctc_call-1");
    assert_eq!(json["input"][0]["input"], "*** Begin Patch\n*** End Patch");
    assert_eq!(json["input"][1]["type"], "custom_tool_call_output");
    assert_eq!(
        json["input"][1]["output"],
        json!([
            {"type": "input_text", "text": "custom output"},
            {"type": "input_image", "image_url": "data:image/png;base64,AQID", "detail": "original"}
        ])
    );
}

#[test]
fn exec_updates_are_named_and_unpaired_across_compaction_and_incremental_replay() {
    let call = inference_response(
        Some("resp-call"),
        vec![InferenceResponseItem::ToolCall {
            provider_specific: provider_specific(
                "custom_tool_call",
                json!({
                    "type": "custom_tool_call", "id": "ctc_exec", "call_id": "call-exec",
                    "name": "exec", "input": "command('sleep 5')",
                }),
            ),
            id: tool_call_id("call-exec"),
            name: tool_name("exec"),
            tool_type: ToolType::Custom,
            arguments: "command('sleep 5')".to_owned(),
        }],
    );
    let result = Arc::new(ContextBlock::ToolResults {
        results: vec![ToolResult {
            call_id: tool_call_id("call-exec"),
            tool_type: ToolType::Custom,
            body: ToolOutput {
                full_output: None,
                images: Arc::new(Vec::new()),
                output: Arc::new("Running".to_owned()),
                status: rho_agent_types::ToolOutputStatus::Success,
            },
            started_at: rho_agent_types::UnixMs(1),
            finished_at: rho_agent_types::UnixMs(2),
            metadata: None,
        }],
    });
    let update = Arc::new(ContextBlock::ToolUpdate(crate::types::ToolUpdate {
        status: None,
        images: Arc::new(vec![crate::types::ImageContent {
            media_type: "image/png".into(),
            data: vec![1, 2, 3],
            detail: crate::types::ImageDetail::Original,
        }]),
        call_id: tool_call_id("call-exec"),
        tool_type: ToolType::Custom,
        output: Arc::new("Command completed".to_owned()),
        full_output: None,
        at: rho_agent_types::UnixMs(3),
    }));
    let compact = inference_response(
        Some("resp-compact"),
        vec![InferenceResponseItem::Compaction {
            provider_specific: provider_specific(
                "compaction",
                json!({
                    "type": "compaction", "id": "cmp_exec", "encrypted_content": "sealed",
                }),
            ),
        }],
    );
    for model in [
        ResponsesModel::Test("gpt-test".to_owned()),
        ResponsesModel::Gpt6Astra,
    ] {
        let mut session = test_inference_service("gpt-test");
        session.config.responses_config.model = model;
        for compacted in [false, true] {
            for cached in [None, Some("resp-call")] {
                let mut blocks = vec![call.clone(), result.clone()];
                if compacted {
                    blocks.push(compact.clone());
                }
                blocks.push(update.clone());
                let body = serde_json::to_value(ResponsesRequest::from_inference_request(
                    &session.config,
                    &inference_request(blocks),
                    cached,
                ))
                .unwrap();
                let input = body["input"].as_array().unwrap();
                assert_eq!(
                    input.last().unwrap(),
                    &json!({
                        "type": "function_call_output", "namespace": "functions",
                        "name": "exec", "output": [
                            {"type":"input_text","text":"Command completed"},
                            {"type":"input_image","image_url":"data:image/png;base64,AQID","detail":"original"}
                        ],
                    })
                );
                if compacted {
                    assert!(input.iter().all(|item| item.get("call_id").is_none()));
                } else {
                    let result = input
                        .iter()
                        .find(|item| item["type"] == "custom_tool_call_output")
                        .unwrap();
                    assert_eq!(result["name"], "exec");
                    assert_eq!(result["call_id"], "call-exec");
                }
            }
        }
    }
}

#[test]
fn astra_responses_lite_moves_tools_and_instructions_into_input() {
    let (_temp, auth) = test_oauth_file("token", None);
    let mut session = InferenceSession::new_deep(
        Inference::for_test(auth),
        InferenceProfile {
            effort: ReasoningEffort::Medium,
            fast_mode: false,
        },
        InferenceModel::Gpt6Astra,
        PromptCacheKey::from_bytes(*b"testkey0"),
    );
    session.config.responses_config.text_verbosity = TextVerbosity::Low;
    let mut request = inference_request(vec![user_block("hello")]);
    request.instructions = Arc::from("You are rho.");

    let body = ResponsesRequest::from_inference_request(&session.config, &request, None);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["model"], "gpt-6-astra");
    assert_eq!(json["instructions"], "");
    assert!(json.get("tools").is_none());
    assert!(json.get("tool_choice").is_none());
    assert_eq!(json["parallel_tool_calls"], false);
    assert!(json.get("context_management").is_none());
    assert_eq!(json["input"][0]["type"], "additional_tools");
    assert_eq!(json["input"][0]["role"], "developer");
    assert_eq!(json["input"][0]["tools"][0]["name"], "exec");
    assert_eq!(json["input"][1]["type"], "message");
    assert_eq!(json["input"][1]["role"], "developer");
    assert_eq!(json["input"][1]["content"][0]["text"], "You are rho.");
    assert_eq!(json["input"][2]["role"], "user");
    assert_eq!(
        json["client_metadata"]["ws_request_header_x_openai_internal_codex_responses_lite"],
        "true"
    );
}

#[test]
fn responses_lite_previous_response_skips_developer_prefix() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = InferenceSession::new_deep(
        Inference::for_test(auth),
        InferenceProfile {
            effort: ReasoningEffort::Medium,
            fast_mode: false,
        },
        InferenceModel::Gpt6Sol,
        PromptCacheKey::from_bytes(*b"testkey0"),
    );
    let mut request = inference_request(vec![
        user_block("first"),
        inference_response(Some("resp_1"), vec![assistant_message("done")]),
        user_block("second"),
    ]);
    request.instructions = Arc::from("You are rho.");

    let body = ResponsesRequest::from_inference_request(&session.config, &request, Some("resp_1"));
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["previous_response_id"], "resp_1");
    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "second");
    assert_eq!(
        json["client_metadata"]["ws_request_header_x_openai_internal_codex_responses_lite"],
        "true"
    );
}

#[test]
fn current_exec_reply_precedes_background_updates_and_is_only_paired_once() {
    let call = |id: &str, response: &str, command: &str| {
        inference_response(
            Some(response),
            vec![InferenceResponseItem::ToolCall {
                provider_specific: provider_specific(
                    "custom_tool_call",
                    json!({
                        "type": "custom_tool_call", "id": format!("ctc_{id}"),
                        "call_id": id, "name": "exec", "input": command,
                    }),
                ),
                id: tool_call_id(id),
                name: tool_name("exec"),
                tool_type: ToolType::Custom,
                arguments: command.to_owned(),
            }],
        )
    };
    let reply = |id: &str, text: &str| {
        Arc::new(ContextBlock::ToolResults {
            results: vec![ToolResult {
                call_id: tool_call_id(id),
                tool_type: ToolType::Custom,
                body: ToolOutput {
                    full_output: None,
                    images: Arc::new(Vec::new()),
                    output: Arc::new(text.to_owned()),
                    status: rho_agent_types::ToolOutputStatus::Success,
                },
                started_at: rho_agent_types::UnixMs(1),
                finished_at: rho_agent_types::UnixMs(2),
                metadata: None,
            }],
        })
    };
    let update = |id: &str, text: &str| {
        Arc::new(ContextBlock::ToolUpdate(crate::types::ToolUpdate {
            status: None,
            images: Default::default(),
            call_id: tool_call_id(id),
            tool_type: ToolType::Custom,
            output: Arc::new(text.to_owned()),
            full_output: None,
            at: rho_agent_types::UnixMs(3),
        }))
    };
    let blocks = vec![
        call("call-old", "resp-old", "command('rho pr checks ...')"),
        reply("call-old", "Command 15 running: rho pr checks ..."),
        call("call-current", "resp-current", "command('cargo test')"),
        reply("call-current", "Command 16 running: cargo test"),
        update(
            "call-old",
            "Command 15 output (rho pr checks ...): CI pending",
        ),
        update(
            "call-current",
            "Command 16 completed (cargo test): exit_code=0",
        ),
    ];
    let session = test_inference_service("gpt-test");
    for cached in [None, Some("resp-current")] {
        let body = serde_json::to_value(ResponsesRequest::from_inference_request(
            &session.config,
            &inference_request(blocks.clone()),
            cached,
        ))
        .unwrap();
        let outputs = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("custom_tool_call_output" | "function_call_output")
                )
            })
            .collect::<Vec<_>>();
        let current = if cached.is_some() { 0 } else { 1 };
        assert_eq!(outputs.len(), current + 3);
        assert_eq!(
            outputs[current],
            &json!({
                "type": "custom_tool_call_output", "name": "exec",
                "call_id": "call-current", "output": "Command 16 running: cargo test",
            })
        );
        assert_eq!(
            outputs[current + 1],
            &json!({
                "type": "function_call_output", "namespace": "functions", "name": "exec",
                "output": "Command 15 output (rho pr checks ...): CI pending",
            })
        );
        assert_eq!(
            outputs[current + 2],
            &json!({
                "type": "function_call_output", "namespace": "functions", "name": "exec",
                "output": "Command 16 completed (cargo test): exit_code=0",
            })
        );
    }
}

#[test]
fn rotated_context_keeps_developer_notices_and_old_tool_output_without_orphan_calls() {
    let old_call = inference_response(
        Some("old-response"),
        vec![InferenceResponseItem::ToolCall {
            provider_specific: provider_specific(
                "custom_tool_call",
                json!({
                    "type": "custom_tool_call", "id": "old-item", "call_id": "old-call",
                    "name": "exec", "input": "old code",
                }),
            ),
            id: tool_call_id("old-call"),
            name: tool_name("exec"),
            tool_type: ToolType::Custom,
            arguments: "old code".into(),
        }],
    );
    let mut result = tool_result_success(tool_call_id("old-call"), "late output");
    result.tool_type = ToolType::Custom;
    result.body.images = Arc::new(vec![crate::types::ImageContent {
        media_type: "image/png".into(),
        data: vec![1, 2, 3],
        detail: crate::types::ImageDetail::High,
    }]);
    let mut request = inference_request(vec![
        user_block("discard this old request"),
        old_call,
        Arc::new(ContextBlock::DeveloperMessage {
            text: "retention boundary".into(),
        }),
        Arc::new(ContextBlock::ToolResults {
            results: vec![result],
        }),
        Arc::new(ContextBlock::ToolUpdate(crate::types::ToolUpdate {
            status: None,
            images: Default::default(),
            call_id: tool_call_id("old-call"),
            tool_type: ToolType::Custom,
            output: Arc::new("later update".into()),
            full_output: None,
            at: UnixMs(1),
        })),
    ]);
    request
        .input
        .push(Arc::new(ContextBlock::ContextRotation { retain_from: 2 }));
    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        Some("old-response"),
    );
    let json = serde_json::to_value(body).unwrap();
    assert!(json.get("previous_response_id").is_none());
    let input = json["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(input[0]["role"], "developer");
    assert_eq!(input[0]["content"][0]["text"], "retention boundary");
    assert_eq!(input[1]["type"], "function_call_output");
    assert_eq!(input[1]["name"], "exec");
    assert!(input[1].get("call_id").is_none());
    assert_eq!(input[1]["output"][0]["text"], "late output");
    assert_eq!(input[1]["output"][1]["type"], "input_image");
    assert_eq!(input[2]["name"], "exec");
    assert!(input[2].get("call_id").is_none());
}

#[test]
fn rotated_context_preserves_retained_call_result_pairs() {
    let mut request = inference_request(vec![
        user_block("discard"),
        Arc::new(ContextBlock::DeveloperMessage {
            text: "boundary".into(),
        }),
        inference_response(
            None,
            vec![InferenceResponseItem::ToolCall {
                provider_specific: provider_specific(
                    "function_call",
                    json!({
                        "type": "function_call", "id": "item", "call_id": "call",
                        "name": "exec", "arguments": "{}",
                    }),
                ),
                id: tool_call_id("call"),
                name: tool_name("exec"),
                tool_type: ToolType::Function,
                arguments: "{}".into(),
            }],
        ),
        Arc::new(ContextBlock::ToolResults {
            results: vec![tool_result_success(tool_call_id("call"), "done")],
        }),
    ]);
    request
        .input
        .push(Arc::new(ContextBlock::ContextRotation { retain_from: 1 }));
    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        None,
    );
    let json = serde_json::to_value(body).unwrap();
    assert_eq!(json["input"].as_array().unwrap().len(), 3);
    assert_eq!(json["input"][1]["call_id"], "call");
    assert_eq!(json["input"][2]["call_id"], "call");
}

#[test]
fn rotation_activation_invalidates_retained_old_continuations_but_not_new_ones() {
    let session = test_inference_service("gpt-test");
    let mut request = inference_request(vec![
        user_block("old context"),
        Arc::new(ContextBlock::DeveloperMessage {
            text: "early marker".into(),
        }),
        inference_response(Some("prepared-in-old-window"), Vec::new()),
    ]);
    // Merely announcing the boundary must not discard anything.
    let before = ResponsesRequest::from_inference_request(&session.config, &request, None);
    assert_eq!(before.input[0]["content"][0]["text"], "old context");

    request
        .input
        .push(Arc::new(ContextBlock::ContextRotation { retain_from: 1 }));
    request.input.push(user_block("resume"));
    let rotated = ResponsesRequest::from_inference_request(
        &session.config,
        &request,
        Some("prepared-in-old-window"),
    );
    assert!(rotated.previous_response_id.is_none());
    assert_eq!(rotated.input.len(), 2);
    assert_eq!(rotated.input[0]["content"][0]["text"], "early marker");
    assert_eq!(rotated.input[1]["content"][0]["text"], "resume");

    request
        .input
        .push(inference_response(Some("fresh-window"), Vec::new()));
    request.input.push(user_block("next"));
    let continued =
        ResponsesRequest::from_inference_request(&session.config, &request, Some("fresh-window"));
    assert_eq!(
        continued.previous_response_id.as_deref(),
        Some("fresh-window")
    );
    assert_eq!(continued.input.len(), 1);
    assert_eq!(continued.input[0]["content"][0]["text"], "next");
}

#[test]
fn incremental_results_resolve_both_old_and_recent_call_metadata() {
    let mut blocks = Vec::new();
    for index in 0..1000 {
        blocks.push(inference_response(
            Some(&format!("response-{index}")),
            vec![InferenceResponseItem::ToolCall {
                provider_specific: provider_specific(
                    "custom_tool_call",
                    json!({
                        "type": "custom_tool_call", "id": format!("item-{index}"),
                        "call_id": format!("call-{index}"), "name": format!("tool-{index}"),
                        "input": "pass",
                    }),
                ),
                id: tool_call_id(&format!("call-{index}")),
                name: tool_name(&format!("tool-{index}")),
                tool_type: ToolType::Custom,
                arguments: "pass".into(),
            }],
        ));
    }
    blocks.push(Arc::new(ContextBlock::ToolResults {
        results: [0, 999]
            .into_iter()
            .map(|index| {
                let mut result =
                    tool_result_success(tool_call_id(&format!("call-{index}")), "done");
                result.tool_type = ToolType::Custom;
                result
            })
            .collect(),
    }));
    let request = inference_request(blocks);
    let body = ResponsesRequest::from_inference_request(
        &test_inference_service("gpt-test").config,
        &request,
        Some("response-999"),
    );
    assert_eq!(body.previous_response_id.as_deref(), Some("response-999"));
    assert_eq!(body.input.len(), 2);
    for (result, index) in body.input.iter().zip([0, 999]) {
        assert_eq!(result["type"], "custom_tool_call_output");
        assert_eq!(result["call_id"], format!("call-{index}"));
        assert_eq!(result["name"], format!("tool-{index}"));
    }
}

#[test]
fn eviction_removes_only_selected_tool_exchanges_and_invalidates_old_continuations() {
    let session = test_inference_service("gpt-test");
    let call = |id: &str| InferenceResponseItem::ToolCall {
        provider_specific: provider_specific(
            "function_call",
            json!({
                "type": "function_call", "id": format!("fc_{id}"), "call_id": id,
                "name": "shell_run", "arguments": "pwd",
            }),
        ),
        id: tool_call_id(id),
        name: tool_name("shell_run"),
        tool_type: ToolType::Function,
        arguments: "pwd".into(),
    };
    let reasoning = InferenceResponseItem::EncryptedReasoning {
        provider_specific: provider_specific(
            "reasoning",
            json!({
                "type": "reasoning", "id": "reason", "encrypted_content": "opaque",
                "summary": [{"type":"summary_text","text":"keep this reasoning"}],
            }),
        ),
        summary: vec!["keep this reasoning".into()],
    };
    let mut request = inference_request(vec![
        user_block("keep original request"),
        inference_response(
            Some("before-eviction"),
            vec![
                reasoning,
                assistant_message("keep prose"),
                call("old"),
                call("recent"),
            ],
        ),
        Arc::new(ContextBlock::ToolResults {
            results: vec![
                tool_result_success(tool_call_id("old"), "old-result"),
                tool_result_success(tool_call_id("recent"), "recent-result"),
            ],
        }),
        Arc::new(ContextBlock::ToolUpdate(crate::types::ToolUpdate {
            call_id: tool_call_id("old"),
            tool_type: ToolType::Function,
            output: Arc::new("old-late-output".into()),
            full_output: None,
            status: Some(rho_agent_types::ToolOutputStatus::Success),
            at: rho_agent_types::UnixMs(1),
            images: Default::default(),
        })),
        Arc::new(ContextBlock::ToolHistoryEvicted {
            call_ids: vec![tool_call_id("old")],
        }),
        Arc::new(ContextBlock::DeveloperMessage {
            text: "history available".into(),
        }),
    ]);
    let output = ResponsesRequest::from_inference_request(
        &session.config,
        &request,
        Some("before-eviction"),
    );
    assert!(output.previous_response_id.is_none());
    let wire = serde_json::to_value(&output).unwrap()["input"].clone();
    let wire_text = wire.to_string();
    for retained in [
        "keep original request",
        "keep prose",
        "keep this reasoning",
        "opaque",
        "recent-result",
    ] {
        assert!(wire_text.contains(retained), "{retained}");
    }
    assert!(!wire_text.contains("old-result"));
    assert!(!wire_text.contains("old-late-output"));
    assert!(
        !wire
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["call_id"] == "old")
    );
    assert!(
        wire.as_array()
            .unwrap()
            .iter()
            .any(|item| item["call_id"] == "recent" && item["type"] == "function_call")
    );
    request.input.push(inference_response(
        Some("after-eviction"),
        vec![assistant_message("new")],
    ));
    request.input.push(user_block("continue"));
    let warm =
        ResponsesRequest::from_inference_request(&session.config, &request, Some("after-eviction"));
    assert_eq!(warm.previous_response_id.as_deref(), Some("after-eviction"));
    assert_eq!(warm.input.len(), 1);
    assert_eq!(warm.input[0]["content"][0]["text"], "continue");
    assert_eq!(
        request.input.len(),
        8,
        "projection must not mutate original history"
    );
}
