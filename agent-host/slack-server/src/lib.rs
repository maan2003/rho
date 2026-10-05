//! The agent host's Slack server.
//!
//! Agents call the Slack Web API over this Unix socket, through the
//! `slack_sdk` the notebook ships (`agent-host/rho-notebook/src/slack_sdk`).
//! The server adds the bot token, so no agent holds it, and forwards only
//! the methods in a fixed list: reads and the writes agents need to work
//! with people. Slack validates the arguments.
//!
//! One method is the server's own: `rho.events` long-polls the events the
//! app's Socket Mode connection receives (see [`events`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use reqwest::{Client, Url};
use serde_json::Value;
use tokio::net::UnixListener;

mod events;

pub use events::{Events, run_socket_mode};

pub type TokenProvider = Arc<dyn Fn() -> Result<String> + Send + Sync>;

struct AppState {
    client: Client,
    token_provider: TokenProvider,
    app_token: TokenProvider,
    slack_api_url: Url,
    events: Arc<Events>,
}

/// How long `rho.events` waits when the agent does not say, and at most.
const DEFAULT_WAIT: Duration = Duration::from_secs(20);
const MAX_WAIT: Duration = Duration::from_secs(300);

/// The Web API methods agents may call, each with the argument names it
/// takes. The names are upstream `slack_sdk`'s method signatures.
fn methods() -> &'static HashMap<String, HashSet<String>> {
    static METHODS: OnceLock<HashMap<String, HashSet<String>>> = OnceLock::new();
    METHODS.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct List {
            methods: HashMap<String, HashSet<String>>,
        }
        serde_json::from_str::<List>(include_str!("../methods.json"))
            .expect("bundled Slack method list is valid")
            .methods
    })
}

/// The argument names a call sends, from its query and its form or JSON
/// body. Slack ignores names it does not know, so a misspelt `thread_ts`
/// would post outside the thread instead of failing.
fn argument_names(query: &str, headers: &HeaderMap, body: &[u8]) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(name, _)| name.into_owned())
        .collect();
    if body.is_empty() {
        return Ok(names);
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    match content_type.split(';').next().unwrap_or_default().trim() {
        "application/x-www-form-urlencoded" => {
            names.extend(url::form_urlencoded::parse(body).map(|(name, _)| name.into_owned()))
        }
        "application/json" => match serde_json::from_slice::<Value>(body) {
            Ok(Value::Object(object)) => names.extend(object.into_iter().map(|(name, _)| name)),
            _ => return Err("the JSON body is not an object".to_owned()),
        },
        other => return Err(format!("unsupported content type {other:?}")),
    }
    Ok(names)
}

pub fn router(
    token_provider: TokenProvider,
    app_token: TokenProvider,
    slack_api_url: Url,
    events: Arc<Events>,
) -> Router {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let state = Arc::new(AppState {
        client: Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("static Slack HTTP client configuration is valid"),
        token_provider,
        app_token,
        slack_api_url,
        events,
    });
    Router::new()
        .route("/api/{method}", any(call))
        .with_state(state)
}

pub async fn serve(listener: UnixListener, router: Router) -> Result<()> {
    axum::serve(listener, router).await?;
    Ok(())
}

/// Forwards one Web API call with the bot token in place of any the agent sent.
async fn call(
    State(state): State<Arc<AppState>>,
    Path(method): Path<String>,
    RawQuery(query): RawQuery,
    http_method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(arguments) = methods().get(&method) else {
        return slack_error("rho_method_unavailable");
    };
    match argument_names(query.as_deref().unwrap_or_default(), &headers, &body) {
        Ok(names) => {
            let unknown: BTreeSet<&str> = names
                .iter()
                .filter(|name| !arguments.contains(*name))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                let unknown = unknown.into_iter().collect::<Vec<_>>().join(", ");
                return slack_error(&format!("rho_invalid_arguments: {unknown}"));
            }
        }
        Err(error) => return slack_error(&format!("rho_invalid_arguments: {error}")),
    }
    if method == "rho.events" {
        return wait_for_events(&state, query.as_deref().unwrap_or_default()).await;
    }
    let token = match (state.token_provider)() {
        Ok(token) if !token.trim().is_empty() => token.trim().to_owned(),
        _ => return slack_error("rho_no_slack_token: run `rho slack init` on the agent host"),
    };
    let mut url = state.slack_api_url.clone();
    url.path_segments_mut()
        .expect("the Slack API URL can be a base")
        .pop_if_empty()
        .push(&method);
    url.set_query(query.as_deref());
    let mut request = state
        .client
        .request(http_method, url)
        .bearer_auth(token)
        .body(body);
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return slack_error(&format!("rho_slack_unreachable: {error}")),
    };
    let status = response.status();
    let mut forwarded = HeaderMap::new();
    for name in [header::CONTENT_TYPE, header::RETRY_AFTER] {
        if let Some(value) = response.headers().get(&name) {
            forwarded.insert(name, value.clone());
        }
    }
    match response.bytes().await {
        Ok(body) => (status, forwarded, body).into_response(),
        Err(error) => slack_error(&format!("rho_slack_unreachable: {error}")),
    }
}

/// `rho.events`: `cursor` and `timeout` (seconds) arrive as query parameters.
async fn wait_for_events(state: &AppState, query: &str) -> Response {
    if events::present(&state.app_token).is_none() {
        return slack_error("rho_no_slack_app_token: run `rho slack init` on the agent host");
    }
    let mut cursor = None;
    let mut timeout = DEFAULT_WAIT;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match &*key {
            "cursor" => cursor = Some(value.into_owned()),
            "timeout" => match value.parse::<f64>() {
                Ok(seconds) if seconds >= 0.0 => {
                    timeout = Duration::from_secs_f64(seconds).min(MAX_WAIT)
                }
                _ => return slack_error("invalid_arguments: timeout"),
            },
            _ => {}
        }
    }
    let reply = state.events.wait(cursor.as_deref(), timeout).await;
    axum::Json(reply).into_response()
}

/// A failure in Slack's own shape, so `slack_sdk` raises `SlackApiError` with
/// it.
fn slack_error(error: &str) -> Response {
    let body = serde_json::json!({ "ok": false, "error": error }).to_string();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::extract::Request;
    use serde_json::json;

    use super::*;

    /// What the fake Slack saw: method, path and query, auth, content type,
    /// body.
    type Seen = Arc<Mutex<Vec<(String, String, String, String, String)>>>;

    async fn fake_slack() -> (Url, Seen) {
        let seen: Seen = Arc::default();
        let record = seen.clone();
        let app = Router::new().fallback(any(move |request: Request| {
            let record = record.clone();
            async move {
                let (parts, body) = request.into_parts();
                let header = |name| {
                    parts
                        .headers
                        .get(name)
                        .map(|v: &HeaderValue| v.to_str().unwrap().to_owned())
                        .unwrap_or_default()
                };
                let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                record.lock().unwrap().push((
                    parts.method.to_string(),
                    parts.uri.to_string(),
                    header(header::AUTHORIZATION),
                    header(header::CONTENT_TYPE),
                    String::from_utf8(body.to_vec()).unwrap(),
                ));
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "7"), (header::SET_COOKIE, "b=1")],
                    axum::Json(json!({"ok": false, "error": "ratelimited"})),
                )
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/api/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    async fn server(token: &'static str, slack: Url) -> String {
        let app = router(
            Arc::new(move || Ok(token.to_owned())),
            Arc::new(move || Ok(token.to_owned())),
            slack,
            Arc::default(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn forwards_calls_with_the_host_token_in_place_of_the_agents() {
        let (slack, seen) = fake_slack().await;
        let server = server("xoxb-host", slack).await;

        let response = reqwest::Client::new()
            .post(format!("{server}/api/chat.postMessage?thread_ts=1.2"))
            .bearer_auth("xoxb-agent")
            .header(header::CONTENT_TYPE, "application/json;charset=utf-8")
            .body(r#"{"channel":"D1","text":"hi"}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"ok": false, "error": "ratelimited"})
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [(
                "POST".to_owned(),
                "/api/chat.postMessage?thread_ts=1.2".to_owned(),
                "Bearer xoxb-host".to_owned(),
                "application/json;charset=utf-8".to_owned(),
                r#"{"channel":"D1","text":"hi"}"#.to_owned(),
            )]
        );
    }

    #[tokio::test]
    async fn forwards_only_listed_methods() {
        let (slack, seen) = fake_slack().await;
        let server = server("xoxb-host", slack).await;

        for method in [
            "auth.revoke",
            "conversations.kick",
            "chat.postMessageX",
            "chat.postmessage",
            "..%2Fchat.postMessage",
        ] {
            let body = reqwest::get(format!("{server}/api/{method}"))
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            assert_eq!(
                body,
                json!({"ok": false, "error": "rho_method_unavailable"}),
                "{method}"
            );
        }
        assert!(seen.lock().unwrap().is_empty());

        reqwest::get(format!("{server}/api/conversations.replies"))
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rho_events_long_polls_with_query_parameters() {
        let (slack, seen) = fake_slack().await;
        let events = Arc::new(Events::default());
        let app = router(
            Arc::new(|| Ok("xoxb-host".to_owned())),
            Arc::new(|| Ok("xapp-host".to_owned())),
            slack,
            events.clone(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let poll = |query: String| {
            let server = server.clone();
            async move {
                reqwest::get(format!("{server}/api/rho.events?{query}"))
                    .await
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap()
            }
        };

        let start = poll("timeout=0".into()).await;
        assert_eq!(start["events"], json!([]));
        let cursor = start["cursor"].as_str().unwrap().to_owned();
        let reply = message_after(&events, poll(format!("cursor={cursor}&timeout=5")));
        assert_eq!(
            reply.await["events"],
            json!([{"payload": {"event": {"channel": "D1", "ts": "2.0"}}}])
        );
        assert_eq!(
            poll("timeout=-1".into()).await,
            json!({"ok": false, "error": "invalid_arguments: timeout"})
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    /// Runs `poll`, and pushes an event while it waits.
    async fn message_after(events: &Arc<Events>, poll: impl Future<Output = Value>) -> Value {
        let events = events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            events.push(json!({"payload": {"event": {"channel": "D1", "ts": "2.0"}}}));
        });
        poll.await
    }

    #[tokio::test]
    async fn refuses_argument_names_the_method_does_not_take() {
        let (slack, seen) = fake_slack().await;
        let server = server("xoxb-host", slack).await;
        let client = reqwest::Client::new();
        let call = |query: &str, content_type: &str, body: &str| {
            client
                .post(format!("{server}/api/chat.postMessage{query}"))
                .header(header::CONTENT_TYPE, content_type)
                .body(body.to_owned())
                .send()
        };
        let json = "application/json;charset=utf-8";
        let form = "application/x-www-form-urlencoded";

        for (request, error) in [
            (
                call(
                    "",
                    json,
                    r#"{"channel":"D1","thread_tss":"1.0","text":"hi"}"#,
                ),
                "thread_tss",
            ),
            (
                call("?thread_tss=1.0&zz=1", json, r#"{"channel":"D1"}"#),
                "thread_tss, zz",
            ),
            (call("", form, "channel=D1&token=xoxp-1"), "token"),
            (
                call("", json, r#"[{"channel":"D1"}]"#),
                "the JSON body is not an object",
            ),
            (
                call("", "multipart/form-data; boundary=x", "--x--"),
                r#"unsupported content type "multipart/form-data""#,
            ),
        ] {
            let body = request.await.unwrap().json::<Value>().await.unwrap();
            assert_eq!(
                body,
                json!({"ok": false, "error": format!("rho_invalid_arguments: {error}")})
            );
        }
        assert!(seen.lock().unwrap().is_empty());

        // Declared names in the query and the form body go through.
        call("?thread_ts=1.0", form, "channel=D1&text=hi")
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn without_a_token_says_how_to_install_one() {
        let (slack, seen) = fake_slack().await;
        let server = server("  ", slack).await;

        for method in ["auth.test", "rho.events"] {
            let body = reqwest::get(format!("{server}/api/{method}"))
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();

            assert_eq!(body["ok"], false);
            assert!(body["error"].as_str().unwrap().contains("rho slack init"));
        }
        assert!(seen.lock().unwrap().is_empty());
    }
}
