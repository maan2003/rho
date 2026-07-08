use super::*;

#[test]
fn parses_text_delta_stream() {
    let parsed = parse_response_events([
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","delta":"hel","output_index":0}"#,
        r#"{"type":"response.output_text.delta","delta":"lo","output_index":0}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
    ])
    .unwrap();

    assert_eq!(assistant_message_text(&parsed.items), "hello");
    assert!(
        !parsed
            .items
            .iter()
            .any(|item| matches!(item, InferenceResponseItem::ToolCall { .. }))
    );
}

#[test]
fn parses_text_deltas_by_output_index() {
    let parsed = parse_response_events([
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","delta":"first","output_index":0}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","delta":"second","output_index":1}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message"}}"#,
        r#"{"type":"response.completed"}"#,
    ])
    .unwrap();

    let messages = parsed
        .items
        .iter()
        .filter_map(|item| match item {
            InferenceResponseItem::AssistantMessage { content, .. } => Some(text_content(content)),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(messages, ["first", "second"]);
}

#[test]
fn preserves_output_item_order() {
    let parsed = parse_response_events([
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning"}}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"ciphertext","summary":[]}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","delta":"after reasoning","output_index":1}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message"}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    assert!(matches!(
        &parsed.items[0],
        InferenceResponseItem::EncryptedReasoning { .. }
    ));
    assert!(matches!(
        &parsed.items[1],
        InferenceResponseItem::AssistantMessage { content, .. } if text_content(content) == "after reasoning"
    ));
}

#[test]
fn streaming_parser_emits_context_item_events() {
    let parsed = parse_response_events([
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","delta":"hel","output_index":0}"#,
        r#"{"type":"response.output_text.delta","delta":"lo","output_index":0}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":2,"output_tokens":3}}}"#,
    ])
    .unwrap();

    assert_eq!(assistant_message_text(&parsed.items), "hello");
    assert!(matches!(
        &parsed.events[0],
        (
            0,
            ContextItemEvent::Update(StreamingContextItem::AssistantMessage { .. })
        )
    ));
    assert!(parsed.events.iter().any(|(index, event)| {
        *index == 0
            && matches!(
                event,
                ContextItemEvent::Update(StreamingContextItem::AssistantMessage { content, .. })
                    if content.iter().map(ToString::to_string).collect::<String>() == "hel"
            )
    }));
    assert_eq!(parsed.provider_response_id.as_deref(), Some("resp_1"));
    assert_eq!(
        parsed.usage,
        Some(TokenUsage {
            input_tokens: 2,
            cached_input_tokens: 0,
            output_tokens: 3,
        })
    );
    assert!(parsed.saw_finished);
}

#[test]
fn parses_function_call_stream() {
    let parsed = parse_response_events([
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"shell"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"command\":\"p"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"wd\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"shell"}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    let call = first_tool_call(&parsed.items);
    assert_eq!(call.id, tool_call_id("call_1"));
    assert_eq!(call.name, tool_name("shell"));
    assert_eq!(call.tool_type, ToolType::Function);
    assert_eq!(call.arguments, r#"{"command":"pwd"}"#);
}

#[test]
fn parses_custom_tool_call_stream() {
    let parsed = parse_response_events([
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","call_id":"call_1","name":"patch"}}"#,
            r#"{"type":"response.custom_tool_call_input.delta","output_index":0,"delta":"*** Begin Patch\n*** End Patch"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","call_id":"call_1","name":"patch"}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    let call = first_tool_call(&parsed.items);
    assert_eq!(call.tool_type, ToolType::Custom);
    assert_eq!(call.name, tool_name("patch"));
    assert_eq!(call.arguments, "*** Begin Patch\n*** End Patch");
}

#[test]
fn parses_message_phase_from_added_item() {
    let parsed = parse_response_events([
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","phase":"commentary"}}"#,
            r#"{"type":"response.output_text.delta","delta":"draft","output_index":0}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","phase":"commentary"}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    let (_, phase) = first_assistant_message(&parsed.items);
    assert_eq!(phase, Some(MessagePhase::Commentary));
}

#[test]
fn parses_message_phase_from_done_item() {
    let parsed = parse_response_events([
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","delta":"draft","output_index":0}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","phase":"final_answer"}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    let (_, phase) = first_assistant_message(&parsed.items);
    assert_eq!(phase, Some(MessagePhase::FinalAnswer));
}

#[test]
fn errors_when_stream_ends_without_terminal_event() {
    let error = parse_response_events([
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","delta":"partial","output_index":0}"#,
    ])
    .unwrap_err();

    assert!(error.to_string().contains("before response.completed"));
}

#[test]
fn captures_response_id_and_usage() {
    let parsed = parse_response_events([r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":7}}}"#])
            .unwrap();

    assert_eq!(parsed.provider_response_id.as_deref(), Some("resp_1"));
    assert_eq!(
        parsed.usage,
        Some(TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 4,
            output_tokens: 7,
        })
    );
}

#[test]
fn unifies_reasoning_summary_and_encrypted_content_into_one_item() {
    let parsed = parse_response_events([
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"thinking"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"ciphertext","summary":["thinking"]}}"#,
            r#"{"type":"response.completed"}"#,
        ])
        .unwrap();

    match &parsed.items[0] {
        InferenceResponseItem::EncryptedReasoning {
            payload: opaque,
            summary,
        } => {
            assert_eq!(summary, &["thinking".to_owned()]);
            assert!(opaque.data.contains("ciphertext"));
        }
        other => panic!("expected encrypted reasoning, got {other:?}"),
    }
}

#[test]
fn preserves_unknown_completed_provider_items() {
    let parsed = parse_response_events([
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"computer_call","id":"cc_1","action":{"type":"screenshot"}}}"#,
        r#"{"type":"response.completed"}"#,
    ])
    .unwrap();

    match &parsed.items[0] {
        InferenceResponseItem::Unknown(opaque) => {
            assert_eq!(&*opaque.tag, "computer_call");
            assert!(opaque.data.contains("cc_1"));
        }
        other => panic!("expected unknown provider item, got {other:?}"),
    }
    assert!(parsed.events.iter().any(|(index, event)| {
        *index == 0
            && matches!(
                event,
                ContextItemEvent::Update(StreamingContextItem::Unknown(_))
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

#[test]
fn classifies_transient_stream_errors_for_retry() {
    let error = parse_response_events([
        r#"{"type":"error","error":{"message":"The server is overloaded","code":"server_is_overloaded"}}"#,
    ])
    .unwrap_err();

    assert!(super::is_transient_turn_error(&error));

    let error = parse_response_events([
        r#"{"type":"error","error":{"message":"Slow down","code":"slow_down"}}"#,
    ])
    .unwrap_err();

    assert!(super::is_transient_turn_error(&error));
}

#[test]
fn does_not_classify_user_actionable_stream_errors_for_retry() {
    let error = parse_response_events([
        r#"{"type":"error","error":{"message":"Invalid model","code":"invalid_request_error"}}"#,
    ])
    .unwrap_err();

    assert!(!super::is_transient_turn_error(&error));
}

#[test]
fn transient_retry_backoff_matches_codex_jittered_exponential() {
    let first = super::transient_backoff(1);
    let second = super::transient_backoff(2);
    let third = super::transient_backoff(3);

    assert!((Duration::from_millis(180)..Duration::from_millis(220)).contains(&first));
    assert!((Duration::from_millis(360)..Duration::from_millis(440)).contains(&second));
    assert!((Duration::from_millis(720)..Duration::from_millis(880)).contains(&third));
}
