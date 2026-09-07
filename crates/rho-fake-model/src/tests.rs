use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use super::{FakeModel, FakeModelConfig, TerminalOutcome};

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
