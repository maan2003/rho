use super::*;

#[test]
fn builds_responses_request_with_tools_and_item_timeline() {
    let session = InferenceService::new("gpt-test").with_prompt_cache_key("cache-key");
    let request = InferenceRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::System, "be concise")),
                }],
            },
            ItemBlock::InferenceResponse {
                provider_response_id: Some("resp_prev".to_owned()),
                items: Vec::new(),
            },
            ItemBlock::Local {
                items: vec![
                    Item {
                        id: ItemId("item-1".to_owned()),
                        kind: ItemKind::Message(Message::text(Role::User, "hello")),
                    },
                    Item {
                        id: ItemId("item-2".to_owned()),
                        kind: ItemKind::ToolCall(ToolCall {
                            id: ToolCallId("call-1".to_owned()),
                            name: "shell.run".to_owned(),
                            tool_type: ToolType::Function,
                            arguments: json!({"command": "pwd"}),
                        }),
                    },
                    Item {
                        id: ItemId("item-3".to_owned()),
                        kind: ItemKind::ToolResult(ToolResult::success(
                            ToolCallId("call-1".to_owned()),
                            "done",
                        )),
                    },
                ],
            },
        ],
        tools: vec![ToolSpec {
            name: "shell.run".to_owned(),
            tool_type: ToolType::Function,
            description: "run shell".to_owned(),
            input_schema: json!({"type": "object"}),
            format: None,
        }],
    };

    let body = ResponsesRequest::from_inference_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["model"], "gpt-test");
    assert_eq!(json["instructions"], "be concise");
    assert!(json.get("temperature").is_none());
    assert!(json.get("max_output_tokens").is_none());
    assert_eq!(json["input"][0]["role"], "user");
    assert_eq!(json["input"][1]["type"], "function_call");
    assert_eq!(json["input"][1]["name"], "shell_run");
    assert_eq!(json["input"][2]["type"], "function_call_output");
    assert_eq!(json["tools"][0]["name"], "shell_run");
    assert_eq!(json["tool_choice"], "auto");
    assert_eq!(json["store"], false);
    assert!(json.get("reasoning").is_none());
    assert_eq!(json["text"]["verbosity"], "medium");
    assert!(json.get("service_tier").is_none());
    assert_eq!(json["prompt_cache_key"], "cache-key");
    assert_eq!(json["previous_response_id"], "resp_prev");
    assert_eq!(json["include"][0], "reasoning.encrypted_content");
}

#[test]
fn omits_tool_choice_without_declared_tools() {
    let session = InferenceService::new("gpt-test");
    let request = InferenceRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_inference_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("tool_choice").is_none());
}

#[test]
fn stamps_phase_on_assistant_messages_when_supported() {
    let request = InferenceRequest {
        input: vec![ItemBlock::Local {
            items: vec![
                Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(
                        Message::text(Role::Assistant, "commentary")
                            .with_phase(MessagePhase::Commentary),
                    ),
                },
                Item {
                    id: ItemId("item-1".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::Assistant, "legacy answer")),
                },
            ],
        }],
        tools: Vec::new(),
    };

    let body =
        ResponsesRequest::from_inference_request(&InferenceService::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["phase"], "commentary");
    assert_eq!(json["input"][1]["phase"], "final_answer");
}

#[test]
fn omits_empty_reasoning_request_when_no_effort_is_set() {
    let request = InferenceRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body =
        ResponsesRequest::from_inference_request(&InferenceService::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("reasoning").is_none());
}

#[test]
fn serializes_prompt_cache_key() {
    let session = InferenceService::new("gpt-test").with_prompt_cache_key("cache-key");
    let request = InferenceRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_inference_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["prompt_cache_key"], "cache-key");
}

#[test]
fn previous_response_hint_slices_input_in_provider() {
    let request = InferenceRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::System, "system rules")),
                }],
            },
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-1".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "first")),
                }],
            },
            ItemBlock::InferenceResponse {
                provider_response_id: Some("resp_1".to_owned()),
                items: vec![Item {
                    id: ItemId("item-2".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::Assistant, "done")),
                }],
            },
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-3".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "second")),
                }],
            },
        ],
        tools: Vec::new(),
    };

    let body =
        ResponsesRequest::from_inference_request(&InferenceService::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["instructions"], "system rules");
    assert_eq!(json["previous_response_id"], "resp_1");
    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "second");
}

#[test]
fn previous_response_without_valid_boundary_replays_full_history() {
    let request = InferenceRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "first")),
                }],
            },
            ItemBlock::InferenceResponse {
                provider_response_id: None,
                items: vec![Item {
                    id: ItemId("item-1".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::Assistant, "done")),
                }],
            },
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-2".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "second")),
                }],
            },
        ],
        tools: Vec::new(),
    };

    let body =
        ResponsesRequest::from_inference_request(&InferenceService::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("previous_response_id").is_none());
    assert_eq!(json["input"].as_array().unwrap().len(), 3);
}

#[test]
fn stale_previous_response_error_builds_full_replay_request() {
    let request = InferenceRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "first")),
                }],
            },
            ItemBlock::InferenceResponse {
                provider_response_id: Some("resp_1".to_owned()),
                items: vec![Item {
                    id: ItemId("item-1".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::Assistant, "done")),
                }],
            },
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-2".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "second")),
                }],
            },
        ],
        tools: Vec::new(),
    };
    let sliced = serde_json::to_value(ResponsesRequest::from_inference_request(
        &InferenceService::new("gpt-test"),
        request.clone(),
    ))
    .unwrap();
    assert_eq!(sliced["previous_response_id"], "resp_1");
    assert_eq!(sliced["input"].as_array().unwrap().len(), 1);

    let replay = stale_previous_response_replay_request(
        &InferenceService::new("gpt-test"),
        &request,
        &anyhow::anyhow!("stream error: previous_response_id expired"),
    )
    .expect("stale error should build replay");
    let replay = serde_json::to_value(replay).unwrap();

    assert!(replay.get("previous_response_id").is_none());
    assert_eq!(replay["input"].as_array().unwrap().len(), 3);
}

#[test]
fn non_stale_previous_response_error_does_not_build_replay_request() {
    let request = InferenceRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let replay = stale_previous_response_replay_request(
        &InferenceService::new("gpt-test"),
        &request,
        &anyhow::anyhow!("stream error: rate limit"),
    );

    assert!(replay.is_none());
}

#[test]
fn chatgpt_codex_request_omits_compaction_request_by_default() {
    let (_temp, auth) = test_oauth_file("token", None);
    let request = InferenceRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_inference_request(
        &InferenceService::chatgpt_codex_with_auth("gpt-test", auth),
        request,
    );
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("context_management").is_none());
    assert_eq!(json["input"][0]["content"][0]["text"], "hello");
    assert_eq!(json["store"], false);
}

#[test]
fn configured_compaction_threshold_overrides_provider_default() {
    let session = InferenceService::new("gpt-test").with_compaction_threshold(42_000);
    let request = InferenceRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_inference_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["type"], "compaction_trigger");
    assert_eq!(json["input"][1]["content"][0]["text"], "hello");
    assert_eq!(json["context_management"][0]["type"], "compaction");
    assert_eq!(json["context_management"][0]["compact_threshold"], 42_000);
}

#[test]
fn chatgpt_codex_with_compaction_requests_provider_default_threshold() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = InferenceService::chatgpt_codex_with_auth("gpt-test", auth).with_compaction();
    let request = InferenceRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_inference_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["type"], "compaction_trigger");
    assert_eq!(
        json["context_management"][0]["compact_threshold"],
        DEFAULT_CONTEXT_WINDOW * 9 / 10
    );
}

#[test]
fn compaction_replay_trims_before_latest_compaction_item() {
    let request = InferenceRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "before")),
                }],
            },
            ItemBlock::InferenceResponse {
                provider_response_id: Some("resp_compaction".to_owned()),
                items: vec![Item {
                    id: ItemId("item-1".to_owned()),
                    kind: ItemKind::ProviderItem(ProviderItem {
                        kind: ProviderItemKind::Compaction,
                        payload: json!({"type": "compaction", "id": "cmp_1"}),
                    }),
                }],
            },
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-2".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "after")),
                }],
            },
        ],
        tools: Vec::new(),
    };

    let body =
        ResponsesRequest::from_inference_request(&InferenceService::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"].as_array().unwrap().len(), 2);
    assert_eq!(json["input"][0]["type"], "compaction");
    assert_eq!(json["input"][1]["content"][0]["text"], "after");
    assert!(json.get("context_management").is_none());
}

#[test]
fn replays_reasoning_provider_item() {
    let reasoning = ItemKind::ProviderItem(ProviderItem {
        kind: ProviderItemKind::Reasoning,
        payload: json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "sealed"}),
    });
    let request = InferenceRequest {
        input: vec![
            ItemBlock::InferenceResponse {
                provider_response_id: None,
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: reasoning.clone(),
                }],
            },
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-1".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "after")),
                }],
            },
        ],
        tools: Vec::new(),
    };

    let body = serde_json::to_value(ResponsesRequest::from_inference_request(
        &InferenceService::new("gpt-test"),
        request,
    ))
    .unwrap();
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
    assert_eq!(body["input"][0]["encrypted_content"], "sealed");
}

#[test]
fn does_not_replay_unknown_provider_items() {
    let request = InferenceRequest {
        input: vec![
            ItemBlock::InferenceResponse {
                provider_response_id: Some("resp_1".to_owned()),
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::ProviderItem(ProviderItem {
                        kind: ProviderItemKind::Unknown,
                        payload: json!({"type": "computer_call", "id": "cc_1"}),
                    }),
                }],
            },
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-1".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "after")),
                }],
            },
        ],
        tools: Vec::new(),
    };

    let body =
        ResponsesRequest::from_inference_request(&InferenceService::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "after");
}

#[test]
fn serializes_custom_tool_calls_and_results() {
    let result = ToolResult {
        call_id: ToolCallId("call-1".to_owned()),
        tool_type: ToolType::Custom,
        status: rho_core::ToolResultStatus::Success,
        output: rho_core::ToolOutput {
            content: "custom output".to_owned(),
        },
    };
    let request = InferenceRequest {
        input: vec![
            ItemBlock::InferenceResponse {
                provider_response_id: None,
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::ToolCall(ToolCall {
                        id: ToolCallId("call-1".to_owned()),
                        name: "patch".to_owned(),
                        tool_type: ToolType::Custom,
                        arguments: json!("*** Begin Patch\n*** End Patch"),
                    }),
                }],
            },
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-1".to_owned()),
                    kind: ItemKind::ToolResult(result),
                }],
            },
        ],
        tools: vec![ToolSpec {
            name: "patch".to_owned(),
            tool_type: ToolType::Custom,
            description: "apply a patch".to_owned(),
            input_schema: Value::Null,
            format: Some(ToolFormat::Grammar {
                syntax: ToolGrammarSyntax::Lark,
                definition: "start: /.+/".to_owned(),
            }),
        }],
    };

    let body =
        ResponsesRequest::from_inference_request(&InferenceService::new("gpt-test"), request);
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

#[test]
fn encoded_tool_name_stays_mapped_to_declared_tool_name() {
    let tool = ToolSpec {
        name: "local.patch".to_owned(),
        tool_type: ToolType::Custom,
        description: String::new(),
        input_schema: Value::Null,
        format: Some(ToolFormat::Text),
    };
    let request = InferenceRequest {
        input: vec![ItemBlock::InferenceResponse {
            provider_response_id: None,
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::ToolCall(ToolCall {
                    id: ToolCallId("call-1".to_owned()),
                    name: "local.patch".to_owned(),
                    tool_type: ToolType::Custom,
                    arguments: json!("patch body"),
                }),
            }],
        }],
        tools: vec![tool.clone()],
    };

    let body =
        ResponsesRequest::from_inference_request(&InferenceService::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["tools"][0]["name"], "local_patch");
    assert_eq!(json["input"][0]["name"], "local_patch");

    let mut state = ResponseState::with_tool_names(tool_name_map(&[tool]));
    let (_done, updates) = state
        .apply_event(&json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "custom_tool_call",
                "call_id": "call-2",
                "name": "local_patch",
                "input": "patch body"
            }
        }))
        .unwrap();

    let response = state.finish();
    let call = first_tool_call(&response);
    assert_eq!(call.name, "local.patch");
    assert!(updates.iter().any(|update| {
        matches!(
            update,
            InferenceUpdate::ToolCall { call, .. } if call.name == "local.patch"
        )
    }));
}
