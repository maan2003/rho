use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Sink, Stream};
use rho::{Item, ItemBlock, ItemId, ToolFormat, ToolGrammarSyntax, ToolResult, ToolType};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::build_request::ResponsesRequest;
use super::oauth::{OAuthFile, ResolvedAuth, ResponsesAuth, ResponsesOAuthCredentials};
use super::ws::{WebSocketPoolKey, WsResponseCreate, build_ws_request, next_ws_message};
use super::*;

fn first_message(response: &ProviderResponse) -> &Message {
    response
        .items
        .iter()
        .find_map(|item| match item {
            ItemKind::Message(message) => Some(message),
            _ => None,
        })
        .expect("message item")
}

fn first_tool_call(response: &ProviderResponse) -> &ToolCall {
    response
        .items
        .iter()
        .find_map(|item| match item {
            ItemKind::ToolCall(call) => Some(call),
            _ => None,
        })
        .expect("tool call item")
}

fn test_oauth_file(
    access_token: &str,
    account_id: Option<&str>,
) -> (tempfile::TempDir, ResponsesAuth) {
    let temp = tempfile::tempdir().unwrap();
    let file = OAuthFile::open_in(temp.path(), "chatgpt").unwrap();
    file.save(&ResponsesOAuthCredentials {
        access_token: access_token.to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: account_id.map(str::to_owned),
    })
    .unwrap();
    let auth = ResponsesAuth::oauth_file(file.path());
    (temp, auth)
}

#[derive(Default)]
struct PendingSocket {
    sent: Vec<WsMessage>,
}

impl Stream for PendingSocket {
    type Item = std::result::Result<WsMessage, tungstenite::Error>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Sink<WsMessage> for PendingSocket {
    type Error = tungstenite::Error;

    fn poll_ready(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: WsMessage) -> std::result::Result<(), Self::Error> {
        self.get_mut().sent.push(item);
        Ok(())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[test]
fn builds_responses_request_with_tools_and_item_timeline() {
    let session = ProviderSession::new("gpt-test").with_prompt_cache_key("cache-key");
    let request = ProviderRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::System, "be concise")),
                }],
            },
            ItemBlock::ProviderResponse {
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

    let body = ResponsesRequest::from_provider_request(&session, request);
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
    let session = ProviderSession::new("gpt-test");
    let request = ProviderRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_provider_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("tool_choice").is_none());
}

#[test]
fn websocket_pool_key_requires_chatgpt_pool_and_prompt_cache_key() {
    let auth = ResolvedAuth {
        bearer_token: "token".to_owned(),
        account_id: Some("acct".to_owned()),
    };
    let session = ProviderSession::new("gpt-test").with_prompt_cache_key("thread-1");
    let mut body = ResponsesRequest::from_provider_request(
        &session,
        ProviderRequest {
            input: vec![ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "hello")),
                }],
            }],
            tools: Vec::new(),
        },
    );

    let key = WebSocketPoolKey::from_request(&session, &body, Some(&auth)).unwrap();

    assert_eq!(key.base_url, DEFAULT_CHATGPT_BASE_URL);
    assert_eq!(key.account_id.as_deref(), Some("acct"));
    assert_eq!(key.thread_id, "thread-1");

    body.prompt_cache_key = None;
    assert!(WebSocketPoolKey::from_request(&session, &body, Some(&auth)).is_none());
}

#[test]
fn stamps_phase_on_assistant_messages_when_supported() {
    let request = ProviderRequest {
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

    let body = ResponsesRequest::from_provider_request(&ProviderSession::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["phase"], "commentary");
    assert_eq!(json["input"][1]["phase"], "final_answer");
}

#[test]
fn omits_empty_reasoning_request_when_no_effort_is_set() {
    let request = ProviderRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_provider_request(&ProviderSession::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("reasoning").is_none());
}

#[test]
fn serializes_prompt_cache_key() {
    let session = ProviderSession::new("gpt-test").with_prompt_cache_key("cache-key");
    let request = ProviderRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_provider_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["prompt_cache_key"], "cache-key");
}

#[test]
fn previous_response_hint_slices_input_in_provider() {
    let request = ProviderRequest {
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
            ItemBlock::ProviderResponse {
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

    let body = ResponsesRequest::from_provider_request(&ProviderSession::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["instructions"], "system rules");
    assert_eq!(json["previous_response_id"], "resp_1");
    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "second");
}

#[test]
fn previous_response_without_valid_boundary_replays_full_history() {
    let request = ProviderRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "first")),
                }],
            },
            ItemBlock::ProviderResponse {
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

    let body = ResponsesRequest::from_provider_request(&ProviderSession::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("previous_response_id").is_none());
    assert_eq!(json["input"].as_array().unwrap().len(), 3);
}

#[test]
fn stale_previous_response_error_builds_full_replay_request() {
    let request = ProviderRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "first")),
                }],
            },
            ItemBlock::ProviderResponse {
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
    let sliced = serde_json::to_value(ResponsesRequest::from_provider_request(
        &ProviderSession::new("gpt-test"),
        request.clone(),
    ))
    .unwrap();
    assert_eq!(sliced["previous_response_id"], "resp_1");
    assert_eq!(sliced["input"].as_array().unwrap().len(), 1);

    let replay = stale_previous_response_replay_request(
        &ProviderSession::new("gpt-test"),
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
    let request = ProviderRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let replay = stale_previous_response_replay_request(
        &ProviderSession::new("gpt-test"),
        &request,
        &anyhow::anyhow!("stream error: rate limit"),
    );

    assert!(replay.is_none());
}

#[test]
fn chatgpt_codex_request_omits_compaction_request_by_default() {
    let (_temp, auth) = test_oauth_file("token", None);
    let request = ProviderRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_provider_request(
        &ProviderSession::chatgpt_codex_with_auth("gpt-test", auth),
        request,
    );
    let json = serde_json::to_value(body).unwrap();

    assert!(json.get("context_management").is_none());
    assert_eq!(json["input"][0]["content"][0]["text"], "hello");
    assert_eq!(json["store"], false);
}

#[test]
fn configured_compaction_threshold_overrides_provider_default() {
    let session = ProviderSession::new("gpt-test").with_compaction_threshold(42_000);
    let request = ProviderRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_provider_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["type"], "compaction_trigger");
    assert_eq!(json["input"][1]["content"][0]["text"], "hello");
    assert_eq!(json["context_management"][0]["type"], "compaction");
    assert_eq!(json["context_management"][0]["compact_threshold"], 42_000);
}

#[test]
fn chatgpt_codex_with_compaction_requests_provider_default_threshold() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = ProviderSession::chatgpt_codex_with_auth("gpt-test", auth).with_compaction();
    let request = ProviderRequest {
        input: vec![ItemBlock::Local {
            items: vec![Item {
                id: ItemId("item-0".to_owned()),
                kind: ItemKind::Message(Message::text(Role::User, "hello")),
            }],
        }],
        tools: Vec::new(),
    };

    let body = ResponsesRequest::from_provider_request(&session, request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"][0]["type"], "compaction_trigger");
    assert_eq!(
        json["context_management"][0]["compact_threshold"],
        DEFAULT_CONTEXT_WINDOW * 9 / 10
    );
}

#[test]
fn compaction_replay_trims_before_latest_compaction_item() {
    let request = ProviderRequest {
        input: vec![
            ItemBlock::Local {
                items: vec![Item {
                    id: ItemId("item-0".to_owned()),
                    kind: ItemKind::Message(Message::text(Role::User, "before")),
                }],
            },
            ItemBlock::ProviderResponse {
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

    let body = ResponsesRequest::from_provider_request(&ProviderSession::new("gpt-test"), request);
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
    let request = ProviderRequest {
        input: vec![
            ItemBlock::ProviderResponse {
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

    let body = serde_json::to_value(ResponsesRequest::from_provider_request(
        &ProviderSession::new("gpt-test"),
        request,
    ))
    .unwrap();
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
    assert_eq!(body["input"][0]["encrypted_content"], "sealed");
}

#[test]
fn does_not_replay_unknown_provider_items() {
    let request = ProviderRequest {
        input: vec![
            ItemBlock::ProviderResponse {
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

    let body = ResponsesRequest::from_provider_request(&ProviderSession::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["input"].as_array().unwrap().len(), 1);
    assert_eq!(json["input"][0]["content"][0]["text"], "after");
}

#[test]
fn serializes_custom_tool_calls_and_results() {
    let result = ToolResult {
        call_id: ToolCallId("call-1".to_owned()),
        tool_type: ToolType::Custom,
        status: rho::ToolResultStatus::Success,
        output: rho::ToolOutput {
            content: "custom output".to_owned(),
        },
    };
    let request = ProviderRequest {
        input: vec![
            ItemBlock::ProviderResponse {
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

    let body = ResponsesRequest::from_provider_request(&ProviderSession::new("gpt-test"), request);
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
    let request = ProviderRequest {
        input: vec![ItemBlock::ProviderResponse {
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

    let body = ResponsesRequest::from_provider_request(&ProviderSession::new("gpt-test"), request);
    let json = serde_json::to_value(body).unwrap();

    assert_eq!(json["tools"][0]["name"], "local_patch");
    assert_eq!(json["input"][0]["name"], "local_patch");

    let mut state = ResponseState::with_tool_names(tool_name_map(&[tool]));
    let (_done, updates) = apply_response_event(
        &mut state,
        &json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "custom_tool_call",
                "call_id": "call-2",
                "name": "local_patch",
                "input": "patch body"
            }
        }),
    )
    .unwrap();

    let response = state.finish();
    let call = first_tool_call(&response);
    assert_eq!(call.name, "local.patch");
    assert!(updates.iter().any(|update| {
        matches!(
            update,
            ResponsesUpdate::ToolCall { call, .. } if call.name == "local.patch"
        )
    }));
}

#[test]
fn parser_maps_wire_tool_name_back_to_declared_tool_name() {
    let tool_names = tool_name_map(&[ToolSpec {
        name: "shell.run".to_owned(),
        tool_type: ToolType::Function,
        description: "run shell".to_owned(),
        input_schema: json!({"type": "object"}),
        format: None,
    }]);
    let mut state = ResponseState::with_tool_names(tool_names);
    let (done, updates) = apply_response_event(
        &mut state,
        &json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call-1",
                "name": "shell_run",
                "arguments": "{\"command\":\"pwd\"}"
            }
        }),
    )
    .unwrap();

    assert!(!done);
    let response = state.finish();
    let call = first_tool_call(&response);
    assert_eq!(call.name, "shell.run");
    assert!(updates.iter().any(|update| {
        matches!(
            update,
            ResponsesUpdate::ToolCall { call, .. } if call.name == "shell.run"
        )
    }));
}

#[test]
fn tool_name_map_keeps_wire_name_for_ambiguous_collisions() {
    let tool_names = tool_name_map(&[
        ToolSpec {
            name: "shell.run".to_owned(),
            tool_type: ToolType::Function,
            description: String::new(),
            input_schema: Value::Null,
            format: None,
        },
        ToolSpec {
            name: "shell_run".to_owned(),
            tool_type: ToolType::Function,
            description: String::new(),
            input_schema: Value::Null,
            format: None,
        },
    ]);

    assert_eq!(
        tool_names.get("shell_run").map(String::as_str),
        Some("shell_run")
    );
}

#[test]
fn chatgpt_codex_config_sets_endpoint_defaults() {
    let (_temp, auth) = test_oauth_file("token", None);
    let session = ProviderSession::chatgpt_codex_with_auth("gpt-test", auth);

    assert_eq!(session.base_url, DEFAULT_CHATGPT_BASE_URL);
    assert_eq!(session.compaction, None);
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
    let mut session = ProviderSession::chatgpt_codex_with_auth("gpt-test", auth);
    session.base_url = "https://chatgpt.com/backend-api".to_owned();

    let auth = session.auth.resolve().unwrap();
    let request = build_ws_request(&session, Some("thread-1"), auth.as_ref()).unwrap();

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
    let session = ProviderSession::chatgpt_codex_with_auth("gpt-test", auth);

    let auth = session.auth.resolve().unwrap();
    let request = build_ws_request(&session, None, auth.as_ref()).unwrap();

    assert_eq!(request.headers()["Authorization"], "Bearer sk-test");
    assert!(!request.headers().contains_key("chatgpt-account-id"));
}

#[test]
fn websocket_request_uses_oauth_file_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let file = OAuthFile::open_in(temp.path(), "chatgpt").unwrap();
    file.save(&ResponsesOAuthCredentials {
        access_token: "oauth-access".to_owned(),
        refresh_token: "oauth-refresh".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("acct_file".to_owned()),
    })
    .unwrap();
    let mut session = ProviderSession::chatgpt_codex_with_auth(
        "gpt-test",
        ResponsesAuth::oauth_file(file.path()),
    );
    session.base_url = "https://chatgpt.com/backend-api".to_owned();

    let auth = session.auth.resolve().unwrap();
    let request = build_ws_request(&session, Some("thread-1"), auth.as_ref()).unwrap();

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

#[test]
fn parses_text_delta_stream() {
    let response = parse_response_events([
        r#"{"type":"response.output_text.delta","delta":"hel","output_index":0}"#,
        r#"{"type":"response.output_text.delta","delta":"lo","output_index":0}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
    ])
    .unwrap();

    assert_eq!(
        first_message(&response),
        &Message::text(Role::Assistant, "hello")
    );
    assert!(
        !response
            .items
            .iter()
            .any(|item| matches!(item, ItemKind::ToolCall(_)))
    );
}

#[test]
fn parses_text_deltas_by_output_index() {
    let response = parse_response_events([
        r#"{"type":"response.output_text.delta","delta":"first","output_index":0}"#,
        r#"{"type":"response.output_text.delta","delta":"second","output_index":2}"#,
        r#"{"type":"response.completed"}"#,
    ])
    .unwrap();

    let messages = response
        .items
        .iter()
        .filter_map(|item| match item {
            ItemKind::Message(message) => Some(message.text_content()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(messages, ["first", "second"]);
}

#[test]
fn preserves_nonzero_text_output_order() {
    let response = parse_response_events([
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"ciphertext","summary":[]}}"#,
            r#"{"type":"response.output_text.delta","delta":"after reasoning","output_index":2}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    assert!(matches!(
        &response.items[0],
        ItemKind::ProviderItem(provider_item)
            if provider_item.kind == ProviderItemKind::Reasoning
    ));
    assert!(matches!(
        &response.items[1],
        ItemKind::Message(message) if message.text_content() == "after reasoning"
    ));
}

#[test]
fn streaming_parser_emits_typed_updates() {
    let (response, updates) = collect_response_events_with_updates([
        r#"{"type":"response.output_text.delta","delta":"hel","output_index":0}"#,
        r#"{"type":"response.output_text.delta","delta":"lo","output_index":0}"#,
        r#"{"type":"response.reasoning_summary_text.delta","delta":"think","output_index":1}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":2,"output_tokens":3}}}"#,
    ])
    .unwrap();

    assert_eq!(first_message(&response).text_content(), "hello");
    assert!(matches!(
        &updates[0],
        ResponsesUpdate::TextDelta { output_index: 0, text } if text == "hel"
    ));
    assert!(updates.iter().any(|update| {
        matches!(
            update,
            ResponsesUpdate::ReasoningTextDelta {
                output_index: 1,
                kind: ReasoningTextKind::Summary,
                text,
            } if text == "think"
        )
    }));
    assert!(
        updates
            .iter()
            .any(|update| matches!(update, ResponsesUpdate::ResponseId(id) if id == "resp_1"))
    );
    assert!(matches!(updates.last(), Some(ResponsesUpdate::Finished(_))));
}

#[test]
fn parses_function_call_stream() {
    let response = parse_response_events([
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"shell"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"command\":\"p"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"wd\"}"}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    let call = first_tool_call(&response);
    assert_eq!(call.id, ToolCallId("call_1".to_owned()));
    assert_eq!(call.name, "shell");
    assert_eq!(call.tool_type, ToolType::Function);
    assert_eq!(call.arguments, json!({"command": "pwd"}));
}

#[test]
fn parses_custom_tool_call_stream() {
    let response = parse_response_events([
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","call_id":"call_1","name":"patch"}}"#,
            r#"{"type":"response.custom_tool_call_input.delta","output_index":0,"delta":"*** Begin"}"#,
            r#"{"type":"response.custom_tool_call_input.done","output_index":0,"input":"*** Begin Patch\n*** End Patch"}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    let call = first_tool_call(&response);
    assert_eq!(call.tool_type, ToolType::Custom);
    assert_eq!(call.name, "patch");
    assert_eq!(call.arguments, json!("*** Begin Patch\n*** End Patch"));
    assert!(matches!(
        &response.items[0],
        ItemKind::ToolCall(call) if call.tool_type == ToolType::Custom
    ));
}

#[test]
fn parses_final_message_item() {
    let response = parse_response_events([
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"final","annotations":[]}]}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    assert_eq!(first_message(&response).text_content(), "final");
}

#[test]
fn parses_message_phase_from_final_message_item() {
    let response = parse_response_events([
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","phase":"commentary","content":[{"type":"output_text","text":"draft","annotations":[]}]}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    assert_eq!(
        first_message(&response).phase,
        Some(MessagePhase::Commentary)
    );
}

#[test]
fn errors_when_stream_ends_without_terminal_event() {
    let error = parse_response_events([
        r#"{"type":"response.output_text.delta","delta":"partial","output_index":0}"#,
    ])
    .unwrap_err();

    assert!(error.to_string().contains("before response.completed"));
}

#[test]
fn captures_response_id_and_usage() {
    let response = parse_response_events([r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":7}}}"#])
            .unwrap();

    assert_eq!(response.provider_response_id.as_deref(), Some("resp_1"));
    assert_eq!(
        response.usage,
        Some(TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 4,
            output_tokens: 7,
        })
    );
}

#[test]
fn captures_reasoning_summary_and_encrypted_reasoning_item() {
    let response = parse_response_events([
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"thinking"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"ciphertext","summary":[]}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    assert!(response.items.iter().any(|item| {
        matches!(item, ItemKind::ReasoningText(reasoning) if reasoning.text == "thinking")
    }));
    assert!(response.items.iter().any(|item| {
        matches!(
            item,
            ItemKind::ProviderItem(provider_item)
                if provider_item.kind == ProviderItemKind::Reasoning
                    && provider_item.payload["encrypted_content"] == "ciphertext"
        )
    }));
}

#[test]
fn preserves_unknown_completed_provider_items() {
    let (response, updates) = collect_response_events_with_updates([
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"computer_call","id":"cc_1","action":{"type":"screenshot"}}}"#,
        r#"{"type":"response.completed"}"#,
    ])
    .unwrap();

    assert!(matches!(
        &response.items[0],
        ItemKind::ProviderItem(provider_item)
            if provider_item.kind == ProviderItemKind::Unknown
                && provider_item.payload["type"] == "computer_call"
    ));
    assert!(updates.iter().any(|update| {
        matches!(
            update,
            ResponsesUpdate::OutputItem {
                output_index: 0,
                item: ItemKind::ProviderItem(provider_item),
            } if provider_item.kind == ProviderItemKind::Unknown
                && provider_item.payload["id"] == "cc_1"
        )
    }));
}

#[test]
fn surfaces_stream_error() {
    let error = parse_response_events([
        r#"{"type":"error","error":{"message":"rate limit","code":"rate_limit_exceeded"}}"#,
    ])
    .unwrap_err();

    assert!(error.to_string().contains("rate limit"));
    assert!(error.to_string().contains("type=rate_limit_exceeded"));
}
