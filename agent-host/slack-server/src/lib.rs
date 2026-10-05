//! The agent host's Slack server.
//!
//! Agents call the Slack Web API over this Unix socket, through the
//! `slack_sdk` the notebook ships (`agent-host/rho-notebook/src/slack_sdk`).
//! The server adds the bot token, so no agent holds it. The scopes the user
//! grants the Slack app decide what the bot may do; the server refuses only
//! the methods that would revoke, uninstall or reconfigure the app itself.

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use reqwest::{Client, Url};
use tokio::net::UnixListener;

pub type TokenProvider = Arc<dyn Fn() -> Result<String> + Send + Sync>;

struct AppState {
    client: Client,
    token_provider: TokenProvider,
    slack_api_url: Url,
}

/// Methods an agent may not call, by exact name or by `prefix.`.
const REFUSED: &[&str] = &[
    "auth.revoke",
    "apps.uninstall",
    "apps.manifest.",
    "oauth.",
    "openid.",
    "tooling.",
];

pub fn router(token_provider: TokenProvider, slack_api_url: Url) -> Router {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let state = Arc::new(AppState {
        client: Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("static Slack HTTP client configuration is valid"),
        token_provider,
        slack_api_url,
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
    if method.is_empty()
        || !method
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.')
    {
        return slack_error("rho_invalid_method");
    }
    if REFUSED.iter().any(|refused| {
        method == *refused || (refused.ends_with('.') && method.starts_with(refused))
    }) {
        return slack_error("rho_method_refused");
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
    use serde_json::{Value, json};

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
        let app = router(Arc::new(move || Ok(token.to_owned())), slack);
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
    async fn refuses_methods_that_reconfigure_the_app_and_malformed_names() {
        let (slack, seen) = fake_slack().await;
        let server = server("xoxb-host", slack).await;

        for (method, error) in [
            ("auth.revoke", "rho_method_refused"),
            ("apps.uninstall", "rho_method_refused"),
            ("apps.manifest.update", "rho_method_refused"),
            ("oauth.v2.access", "rho_method_refused"),
            ("chat.post_message", "rho_invalid_method"),
            ("..%2Fauth.revoke", "rho_invalid_method"),
        ] {
            let body = reqwest::get(format!("{server}/api/{method}"))
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            assert_eq!(body, json!({"ok": false, "error": error}), "{method}");
        }
        assert!(seen.lock().unwrap().is_empty());

        // A name that only starts like a refused one is not refused.
        reqwest::get(format!("{server}/api/auth.revokeX"))
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn without_a_token_says_how_to_install_one() {
        let (slack, seen) = fake_slack().await;
        let server = server("  ", slack).await;

        let body = reqwest::get(format!("{server}/api/auth.test"))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();

        assert_eq!(body["ok"], false);
        assert!(body["error"].as_str().unwrap().contains("rho slack init"));
        assert!(seen.lock().unwrap().is_empty());
    }
}
