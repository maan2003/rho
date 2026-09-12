use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use super::{FakeModel, FakeModelConfig, Scenario, TerminalOutcome, synthetic_result_bytes};

#[tokio::test]
async fn openai_websocket_is_deterministic_and_continues_after_tool_result() {
    let server = FakeModel::start(FakeModelConfig::seeded(7)).await.unwrap();
    let url = server.openai_base_url().replace("http://", "ws://") + "/codex/responses";
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
        .send(Message::Text(
            json!({
                "type":"response.create","model":"gpt-test","input":[],
                "tools":[{"type":"custom","name":"exec"}]
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let first = read_turn(&mut socket).await;
    assert!(
        first
            .iter()
            .any(|event| event["type"] == "response.custom_tool_call_input.delta")
    );
    socket.send(Message::Text(json!({
        "type":"response.create","model":"gpt-test",
        "input":[{"type":"custom_tool_call_output","call_id":"call_fake_0","output":"done"}],
        "tools":[{"type":"custom","name":"exec"}]
    }).to_string().into())).await.unwrap();
    let second = read_turn(&mut socket).await;
    assert!(second.iter().any(|event| matches!(
        event["type"].as_str(),
        Some("response.output_text.delta" | "response.custom_tool_call_input.delta")
    )));
    assert_eq!(server.metrics().completed_turns, 2);
    server.shutdown().await.unwrap();
}

async fn read_turn(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let event: Value = serde_json::from_str(&message).unwrap();
        let done = event["type"] == "response.completed" || event["type"] == "response.failed";
        events.push(event);
        if done {
            return events;
        }
    }
}

#[tokio::test]
async fn anthropic_stream_has_native_sse_events_and_real_error_body() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let server = FakeModel::start(FakeModelConfig::seeded(11)).await.unwrap();
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/v1/messages", server.anthropic_base_url()))
        .json(&json!({
            "model":"claude-test","messages":[{"role":"user","content":"hello"}],"stream":true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: message_start"));
    assert!(body.contains("event: message_stop"));

    let mut config = FakeModelConfig::seeded(11);
    config.distribution.forced_outcome = Some(TerminalOutcome::RateLimit);
    let limited = FakeModel::start(config).await.unwrap();
    let response = client
        .post(format!("{}/v1/messages", limited.anthropic_base_url()))
        .json(&json!({
            "model":"claude-test","messages":[],"stream":true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "rate_limit_error"
    );
    server.shutdown().await.unwrap();
    limited.shutdown().await.unwrap();
}

#[tokio::test]
async fn rate_limit_scenario_uses_provider_http_errors_and_retry_after() {
    let client = reqwest::Client::new();
    let mut config = FakeModelConfig::seeded(4);
    config.scenario = Scenario::RateLimit;
    let server = FakeModel::start(config).await.unwrap();

    let openai = client
        .post(format!("{}/codex/responses", server.openai_base_url()))
        .json(&json!({"model":"gpt-test","input":[],"tools":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(openai.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(openai.headers()["retry-after"], "1");
    assert_eq!(
        openai.json::<Value>().await.unwrap()["error"]["type"],
        "rate_limit_error"
    );

    let overloaded = client
        .post(format!("{}/v1/messages", server.anthropic_base_url()))
        .json(&json!({"model":"claude-test","messages":[],"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(overloaded.status().as_u16(), 529);
    assert_eq!(overloaded.headers()["retry-after"], "1");
    assert_eq!(
        overloaded.json::<Value>().await.unwrap()["error"]["type"],
        "overloaded_error"
    );

    let retry = openai_events(&client, &server, json!({"model":"gpt-test"})).await;
    assert!(
        retry
            .iter()
            .any(|event| event["type"] == "response.completed")
    );
    server.shutdown().await.unwrap();

    let mut config = FakeModelConfig::seeded(4);
    config.scenario = Scenario::RateLimit;
    let anthropic_server = FakeModel::start(config).await.unwrap();
    let limited = client
        .post(format!(
            "{}/v1/messages",
            anthropic_server.anthropic_base_url()
        ))
        .json(&json!({"model":"claude-test","messages":[],"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers()["retry-after"], "1");
    assert_eq!(
        limited.json::<Value>().await.unwrap()["error"]["type"],
        "rate_limit_error"
    );
    anthropic_server.shutdown().await.unwrap();
}

#[tokio::test]
async fn stream_cut_ends_during_text_once_and_retry_completes() {
    let client = reqwest::Client::new();
    let mut config = FakeModelConfig::seeded(8);
    config.scenario = Scenario::StreamCut;
    let server = FakeModel::start(config).await.unwrap();
    let request = json!({
        "model":"gpt-test",
        "input":[],
        "tools":[{"type":"custom","name":"exec"}]
    });

    let first = client
        .post(format!("{}/codex/responses", server.openai_base_url()))
        .json(&request)
        .send()
        .await;
    if let Ok(response) = first {
        assert!(response.text().await.is_err());
    }
    let observations = server.drain_observations();
    assert!(
        observations
            .iter()
            .any(|event| event.event_type == "response.output_text.delta")
    );
    assert!(
        !observations
            .iter()
            .any(|event| event.event_type == "response.completed")
    );
    let second = openai_events(&client, &server, request).await;
    assert!(
        second
            .iter()
            .any(|event| event["type"] == "response.completed")
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn forty_calls_compaction_and_clarifying_scenarios_are_protocol_items() {
    let client = reqwest::Client::new();

    let mut config = FakeModelConfig::seeded(1);
    config.scenario = Scenario::FortyToolCalls;
    let forty = FakeModel::start(config).await.unwrap();
    let events = openai_events(
        &client,
        &forty,
        json!({"model":"gpt-test","input":[{
            "type":"additional_tools","role":"developer",
            "tools":[{"type":"custom","name":"exec"}]
        }]}),
    )
    .await;
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event["type"] == "response.output_item.added"
                    && event["item"]["type"] == "custom_tool_call"
            })
            .count(),
        40
    );
    forty.shutdown().await.unwrap();

    let mut config = FakeModelConfig::seeded(2);
    config.scenario = Scenario::ReasoningCompaction;
    let compact = FakeModel::start(config).await.unwrap();
    let events = openai_events(&client, &compact, json!({"model":"gpt-test"})).await;
    assert!(events.iter().any(|event| {
        event["type"] == "response.output_item.done"
            && event["item"]["type"] == "reasoning"
            && event["item"]["encrypted_content"].is_string()
    }));
    assert!(events.iter().any(|event| {
        event["type"] == "response.output_item.done"
            && event["item"]["type"] == "compaction"
            && event["item"]["encrypted_content"].is_string()
    }));
    compact.shutdown().await.unwrap();

    let mut config = FakeModelConfig::seeded(3);
    config.scenario = Scenario::ClarifyingQuestion;
    let clarifying = FakeModel::start(config).await.unwrap();
    let events = openai_events(
        &client,
        &clarifying,
        json!({"model":"gpt-test","tools":[{"type":"custom","name":"exec"}]}),
    )
    .await;
    assert!(!events.iter().any(|event| {
        matches!(
            event["item"]["type"].as_str(),
            Some("custom_tool_call" | "function_call")
        )
    }));
    assert!(events.iter().any(|event| {
        event["delta"]
            .as_str()
            .is_some_and(|text| text.contains("clarify"))
    }));
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.completed")
    );
    clarifying.shutdown().await.unwrap();
}

#[test]
fn huge_tool_output_population_has_documented_exact_landmarks() {
    let mut values = (0..100)
        .map(|attempt| synthetic_result_bytes(Scenario::HugeToolOutput, 37, attempt))
        .collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values.iter().filter(|&&value| value == 227).count(), 89);
    assert_eq!(values[49], 227);
    assert_eq!(values[89], 13_097);
    assert_eq!(values[99], 170_448);
    assert_eq!(values.iter().sum::<usize>() / values.len(), 4_047);
}

#[tokio::test]
async fn slow_trickle_uses_word_chunks_at_three_hundred_milliseconds() {
    let mut config = FakeModelConfig::seeded(9);
    config.scenario = Scenario::SlowTrickle;
    config.timing.mode = super::TimingMode::Timed;
    assert_eq!(
        super::chunks("one two three", &config, 0),
        ["one ", "two ", "three"]
    );

    let event = json!({"type":"response.output_text.delta","delta":"one "});
    let started = tokio::time::Instant::now();
    super::delay(&config, 0, &event).await;
    assert!(started.elapsed() >= std::time::Duration::from_millis(280));
}

async fn openai_events(client: &reqwest::Client, server: &FakeModel, request: Value) -> Vec<Value> {
    let body = client
        .post(format!("{}/codex/responses", server.openai_base_url()))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}
