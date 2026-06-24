use anyhow::{Result, bail};

use super::*;

fn parse_response_events(
    events: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<InferenceResponse> {
    let mut state = ResponseState::default();
    let mut completed = false;
    for event in events {
        if apply_response_event_str(&mut state, event.as_ref())?.0 {
            completed = true;
            break;
        }
    }
    if !completed {
        bail!("response stream ended before response.completed");
    }
    Ok(state.finish())
}

fn collect_response_events_with_updates(
    events: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<(InferenceResponse, Vec<InferenceUpdate>)> {
    let mut state = ResponseState::default();
    let mut completed = false;
    let mut updates = Vec::new();
    for event in events {
        let (done, event_updates) = apply_response_event_str(&mut state, event.as_ref())?;
        updates.extend(event_updates);
        if done {
            completed = true;
            break;
        }
    }
    if !completed {
        bail!("response stream ended before response.completed");
    }
    let response = state.finish();
    updates.push(InferenceUpdate::Finished(response.clone()));
    Ok((response, updates))
}

fn apply_response_event_str(
    state: &mut ResponseState,
    data: &str,
) -> Result<(bool, Vec<InferenceUpdate>)> {
    let data = data.trim_end();
    if data == "[DONE]" {
        return Ok((true, Vec::new()));
    }
    let event: Value = match serde_json::from_str(data) {
        Ok(event) => event,
        Err(_) => return Ok((false, Vec::new())),
    };
    state.apply_event(&event)
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
    let (done, updates) = state
        .apply_event(&json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call-1",
                "name": "shell_run",
                "arguments": "{\"command\":\"pwd\"}"
            }
        }))
        .unwrap();

    assert!(!done);
    let response = state.finish();
    let call = first_tool_call(&response);
    assert_eq!(call.name, "shell.run");
    assert!(updates.iter().any(|update| {
        matches!(
            update,
            InferenceUpdate::ToolCall { call, .. } if call.name == "shell.run"
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
        InferenceUpdate::TextDelta { output_index: 0, text } if text == "hel"
    ));
    assert!(updates.iter().any(|update| {
        matches!(
            update,
            InferenceUpdate::ReasoningTextDelta {
                output_index: 1,
                kind: ReasoningTextKind::Summary,
                text,
            } if text == "think"
        )
    }));
    assert!(
        updates
            .iter()
            .any(|update| matches!(update, InferenceUpdate::ResponseId(id) if id == "resp_1"))
    );
    assert!(matches!(updates.last(), Some(InferenceUpdate::Finished(_))));
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
            InferenceUpdate::OutputItem {
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
