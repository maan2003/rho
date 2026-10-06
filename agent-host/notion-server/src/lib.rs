//! The agent host's Notion server.
//!
//! Agents call the tools of Notion's hosted MCP server over this Unix
//! socket, through the `rho_notion` module the notebook ships
//! (`agent-host/rho-notebook/src/rho_notion`). The server signs in as the
//! user with the grant `rho notion init` installed, so no agent holds a
//! token, keeps one MCP session for all agents, and forwards only the tools
//! in a fixed list. Notion validates the arguments.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::UnixListener;

pub mod oauth;

pub use oauth::Endpoints;

/// The platform secrets `rho notion init` installs.
pub const CLIENT_ID: &str = "NOTION_MCP_CLIENT_ID";
pub const REFRESH_TOKEN: &str = "NOTION_MCP_REFRESH_TOKEN";

pub type SecretReader = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;
pub type SecretWriter = Arc<dyn Fn(Vec<(String, String)>) -> Result<()> + Send + Sync>;

const PROTOCOL_VERSION: &str = "2025-06-18";
const SESSION_HEADER: &str = "mcp-session-id";

/// The MCP tools agents may call.
fn tools() -> &'static HashSet<String> {
    static TOOLS: OnceLock<HashSet<String>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let list: Value =
            serde_json::from_str(include_str!("../tools.json")).expect("bundled tool list is JSON");
        serde_json::from_value(list["tools"].clone()).expect("bundled tool list is a list")
    })
}

struct AppState {
    client: Client,
    endpoints: Endpoints,
    read_secret: SecretReader,
    write_secrets: SecretWriter,
    session: tokio::sync::Mutex<Session>,
    next_id: AtomicU64,
}

/// The access token and MCP session all calls share; either is fetched
/// again when Notion stops accepting it.
#[derive(Clone, Default, PartialEq)]
struct Session {
    access_token: Option<String>,
    id: Option<String>,
}

enum Failure {
    /// The token or session expired: start over once.
    Stale,
    Refused(StatusCode, String),
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Failure::Refused(StatusCode::BAD_GATEWAY, format!("{error:#}"))
    }
}

pub fn router(
    read_secret: SecretReader,
    write_secrets: SecretWriter,
    endpoints: Endpoints,
) -> Router {
    let state = Arc::new(AppState {
        client: oauth::http_client(),
        endpoints,
        read_secret,
        write_secrets,
        session: Default::default(),
        next_id: AtomicU64::new(1),
    });
    Router::new()
        .route("/tools", get(list))
        .route("/tools/{name}", post(call))
        .with_state(state)
}

pub async fn serve(listener: UnixListener, router: Router) -> Result<()> {
    axum::serve(listener, router).await?;
    Ok(())
}

/// The listed tools, with their descriptions and argument schemas.
async fn list(State(state): State<Arc<AppState>>) -> Response {
    let mut listed = Vec::new();
    let mut cursor = None;
    loop {
        let params = match &cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let page = match rpc(&state, "tools/list", params).await {
            Ok(page) => page,
            Err(failure) => return refused(failure),
        };
        listed.extend(
            page["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|tool| {
                    tool["name"]
                        .as_str()
                        .is_some_and(|name| tools().contains(name))
                })
                .cloned(),
        );
        match page["nextCursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => return axum::Json(json!({ "tools": listed })).into_response(),
        }
    }
}

/// Calls one tool; the body is its arguments. Replies with the MCP result.
async fn call(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    axum::Json(arguments): axum::Json<Value>,
) -> Response {
    if !tools().contains(&name) {
        return error(StatusCode::FORBIDDEN, "rho_tool_unavailable");
    }
    if !arguments.is_object() {
        return error(
            StatusCode::BAD_REQUEST,
            "rho_invalid_arguments: the arguments are not an object",
        );
    }
    match rpc(
        &state,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await
    {
        Ok(result) => axum::Json(result).into_response(),
        Err(failure) => refused(failure),
    }
}

async fn rpc(state: &AppState, method: &str, params: Value) -> Result<Value, Failure> {
    let session = ready(state).await?;
    match send(state, &session, method, &params).await {
        Err(Failure::Stale) => {
            forget(state, &session).await;
            let session = ready(state).await?;
            match send(state, &session, method, &params).await {
                Err(Failure::Stale) => Err(Failure::Refused(
                    StatusCode::BAD_GATEWAY,
                    "rho_notion_unauthorized: Notion refused a fresh session".to_owned(),
                )),
                other => other.map(|(_, result)| result),
            }
        }
        other => other.map(|(_, result)| result),
    }
}

/// A session with an access token and an MCP session id, made if missing.
async fn ready(state: &AppState) -> Result<Session, Failure> {
    let mut session = state.session.lock().await;
    if session.access_token.is_none() {
        let read = |name| {
            (state.read_secret)(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let (Some(client_id), Some(refresh_token)) = (read(CLIENT_ID), read(REFRESH_TOKEN)) else {
            return Err(Failure::Refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "rho_no_notion_grant: run `rho notion init` on the agent host".to_owned(),
            ));
        };
        let tokens = oauth::refresh(&state.client, &state.endpoints, &client_id, &refresh_token)
            .await
            .map_err(|error| {
                Failure::Refused(
                    StatusCode::BAD_GATEWAY,
                    format!("rho_notion_unauthorized: {error:#}; run `rho notion init` again"),
                )
            })?;
        if let Some(rotated) = tokens.refresh_token {
            (state.write_secrets)(vec![(REFRESH_TOKEN.to_owned(), rotated)])?;
        }
        *session = Session {
            access_token: Some(tokens.access_token),
            id: None,
        };
    }
    if session.id.is_none() {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "rho", "version": env!("CARGO_PKG_VERSION") },
        });
        let (headers, _) = match send(state, &session, "initialize", &params).await {
            Err(Failure::Stale) => {
                *session = Session::default();
                return Err(Failure::Refused(
                    StatusCode::BAD_GATEWAY,
                    "rho_notion_unauthorized: Notion refused a fresh token".to_owned(),
                ));
            }
            other => other?,
        };
        session.id = headers
            .get(SESSION_HEADER)
            .and_then(|id| id.to_str().ok())
            .map(str::to_owned);
        notify_initialized(state, &session).await?;
    }
    Ok(session.clone())
}

/// Drops what Notion stopped accepting, unless another call already has.
async fn forget(state: &AppState, stale: &Session) {
    let mut session = state.session.lock().await;
    if *session == *stale {
        *session = Session::default();
    }
}

fn request(state: &AppState, session: &Session, body: &Value) -> reqwest::RequestBuilder {
    let mut request = state
        .client
        .post(state.endpoints.mcp.clone())
        .bearer_auth(session.access_token.as_deref().unwrap_or_default())
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL_VERSION)
        .json(body);
    if let Some(id) = &session.id {
        request = request.header(SESSION_HEADER, id);
    }
    request
}

async fn send(
    state: &AppState,
    session: &Session,
    method: &str,
    params: &Value,
) -> Result<(reqwest::header::HeaderMap, Value), Failure> {
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let response = request(state, session, &body)
        .send()
        .await
        .map_err(|error| unreachable(&error))?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED
        || (status == StatusCode::NOT_FOUND && session.id.is_some())
    {
        return Err(Failure::Stale);
    }
    let headers = response.headers().clone();
    let event_stream = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let text = response.text().await.map_err(|error| unreachable(&error))?;
    if !status.is_success() {
        return Err(Failure::Refused(
            StatusCode::BAD_GATEWAY,
            format!("rho_notion_error: Notion MCP returned {status}: {text}"),
        ));
    }
    let messages: Vec<Value> = if event_stream {
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .collect()
    } else {
        serde_json::from_str(&text).into_iter().collect()
    };
    let Some(reply) = messages.into_iter().find(|message| message["id"] == id) else {
        return Err(Failure::Refused(
            StatusCode::BAD_GATEWAY,
            format!("rho_notion_error: no reply to {method}"),
        ));
    };
    if let Some(error) = reply.get("error") {
        return Err(Failure::Refused(
            StatusCode::BAD_GATEWAY,
            format!(
                "rho_notion_error: {}",
                error["message"].as_str().unwrap_or("unknown")
            ),
        ));
    }
    Ok((headers, reply["result"].clone()))
}

async fn notify_initialized(state: &AppState, session: &Session) -> Result<(), Failure> {
    let body = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let response = request(state, session, &body)
        .send()
        .await
        .map_err(|error| unreachable(&error))?;
    if !response.status().is_success() {
        return Err(Failure::Refused(
            StatusCode::BAD_GATEWAY,
            format!(
                "rho_notion_error: Notion MCP returned {} to initialized",
                response.status()
            ),
        ));
    }
    Ok(())
}

fn unreachable(error: &reqwest::Error) -> Failure {
    Failure::Refused(
        StatusCode::BAD_GATEWAY,
        format!("rho_notion_unreachable: {error}"),
    )
}

fn refused(failure: Failure) -> Response {
    match failure {
        Failure::Stale => error(StatusCode::BAD_GATEWAY, "rho_notion_unauthorized"),
        Failure::Refused(status, message) => error(status, &message),
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    (status, headers, json!({ "error": message }).to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::extract::Request;
    use axum::routing::any;
    use reqwest::Url;

    use super::*;

    /// A fake Notion MCP: its token endpoint and its MCP endpoint.
    #[derive(Default)]
    struct Fake {
        /// What it saw, in order: `token <grant> <refresh token>` or
        /// `<method> <bearer> <session>`.
        seen: Vec<String>,
        tokens: u32,
        sessions: u32,
        /// Answer the next `tools/call` with this status instead.
        expire_with: Option<u16>,
    }

    async fn fake() -> (Url, Arc<Mutex<Fake>>) {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let shared = fake.clone();
        let app = Router::new().fallback(any(move |request: Request| {
            let fake = shared.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let mut fake = fake.lock().unwrap();
                if parts.uri.path() == "/token" {
                    let form: std::collections::HashMap<String, String> =
                        url::form_urlencoded::parse(&body).into_owned().collect();
                    fake.seen.push(format!("token {} {}", form["grant_type"], form["refresh_token"]));
                    fake.tokens += 1;
                    let n = fake.tokens;
                    return axum::Json(json!({
                        "access_token": format!("a{n}"),
                        "refresh_token": format!("r{}", n + 1),
                    }))
                    .into_response();
                }
                let message: Value = serde_json::from_slice(&body).unwrap();
                let header = |name| {
                    parts
                        .headers
                        .get(name)
                        .map(|value: &axum::http::HeaderValue| value.to_str().unwrap().to_owned())
                        .unwrap_or_default()
                };
                let method = message["method"].as_str().unwrap().to_owned();
                fake.seen.push(format!(
                    "{method} {} {}",
                    header("authorization"),
                    header(SESSION_HEADER)
                ));
                let id = message["id"].clone();
                match method.as_str() {
                    "initialize" => {
                        fake.sessions += 1;
                        let session = format!("s{}", fake.sessions);
                        (
                            [(SESSION_HEADER, session)],
                            axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
                        )
                            .into_response()
                    }
                    "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
                    "tools/call" => {
                        if let Some(status) = fake.expire_with.take() {
                            return StatusCode::from_u16(status).unwrap().into_response();
                        }
                        let result = json!({"content": [{"type": "text", "text": message["params"].to_string()}]});
                        let stream = format!(
                            "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                            json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}),
                            json!({"jsonrpc": "2.0", "id": id, "result": result}),
                        );
                        ([(header::CONTENT_TYPE, "text/event-stream")], stream).into_response()
                    }
                    "tools/list" => {
                        let page = match message["params"]["cursor"].as_str() {
                            None => json!({"tools": [{"name": "notion-fetch"}, {"name": "notion-move-pages"}], "nextCursor": "2"}),
                            Some(_) => json!({"tools": [{"name": "notion-spawn-session"}, {"name": "notion-get-comments"}]}),
                        };
                        axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": page})).into_response()
                    }
                    other => panic!("unexpected {other}"),
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, fake)
    }

    type Written = Arc<Mutex<Vec<(String, String)>>>;

    async fn server(grant: bool, notion: &Url) -> (String, Written) {
        let written: Written = Arc::default();
        let read = written.clone();
        let write = written.clone();
        let app = router(
            // Like the host's store: what the server wrote, else what
            // `rho notion init` installed.
            Arc::new(move |name| {
                let rotated = read
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone());
                match (grant, name, rotated) {
                    (true, _, Some(value)) => Ok(value),
                    (true, CLIENT_ID, None) => Ok("client".to_owned()),
                    (true, REFRESH_TOKEN, None) => Ok("r1".to_owned()),
                    _ => anyhow::bail!("no {name}"),
                }
            }),
            Arc::new(move |secrets| {
                write.lock().unwrap().extend(secrets);
                Ok(())
            }),
            Endpoints::at(notion),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, written)
    }

    async fn call(server: &str, tool: &str, arguments: Value) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(format!("{server}/tools/{tool}"))
            .json(&arguments)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    #[tokio::test]
    async fn signs_in_once_and_forwards_listed_tools_in_one_session() {
        let (notion, fake) = fake().await;
        let (server, written) = server(true, &notion).await;

        for page in ["p1", "p2"] {
            let (status, result) = call(&server, "notion-fetch", json!({"id": page})).await;
            assert_eq!(status, StatusCode::OK);
            let sent = json!({"name": "notion-fetch", "arguments": {"id": page}}).to_string();
            assert_eq!(result, json!({"content": [{"type": "text", "text": sent}]}));
        }

        assert_eq!(
            fake.lock().unwrap().seen,
            [
                "token refresh_token r1",
                "initialize Bearer a1 ",
                "notifications/initialized Bearer a1 s1",
                "tools/call Bearer a1 s1",
                "tools/call Bearer a1 s1",
            ]
        );
        // Notion rotated the refresh token; the host keeps the new one.
        assert_eq!(
            *written.lock().unwrap(),
            [(REFRESH_TOKEN.to_owned(), "r2".to_owned())]
        );
    }

    #[tokio::test]
    async fn starts_over_once_when_notion_drops_the_token_or_the_session() {
        let (notion, fake) = fake().await;
        let (server, _) = server(true, &notion).await;
        call(&server, "notion-fetch", json!({})).await;

        // Each refresh uses the refresh token the one before rotated in.
        for (status, expected) in [
            (
                401,
                [
                    "tools/call Bearer a1 s1",
                    "token refresh_token r2",
                    "initialize Bearer a2 ",
                    "notifications/initialized Bearer a2 s2",
                    "tools/call Bearer a2 s2",
                ],
            ),
            (
                404,
                [
                    "tools/call Bearer a2 s2",
                    "token refresh_token r3",
                    "initialize Bearer a3 ",
                    "notifications/initialized Bearer a3 s3",
                    "tools/call Bearer a3 s3",
                ],
            ),
        ] {
            fake.lock().unwrap().seen.clear();
            fake.lock().unwrap().expire_with = Some(status);
            let (reply, _) = call(&server, "notion-fetch", json!({})).await;
            assert_eq!(reply, StatusCode::OK);
            assert_eq!(fake.lock().unwrap().seen, expected, "{status}");
        }
    }

    #[tokio::test]
    async fn refuses_unlisted_tools_and_non_object_arguments() {
        let (notion, fake) = fake().await;
        let (server, _) = server(true, &notion).await;

        for tool in [
            "notion-move-pages",
            "notion-spawn-session",
            "notion-fetch2",
            "..%2Fnotion-fetch",
        ] {
            let (status, body) = call(&server, tool, json!({})).await;
            assert_eq!(
                (status, body),
                (
                    StatusCode::FORBIDDEN,
                    json!({"error": "rho_tool_unavailable"})
                ),
                "{tool}"
            );
        }
        let (status, body) = call(&server, "notion-fetch", json!(["p1"])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("rho_invalid_arguments")
        );
        assert!(fake.lock().unwrap().seen.is_empty());
    }

    #[tokio::test]
    async fn lists_only_listed_tools_across_pages() {
        let (notion, _) = fake().await;
        let (server, _) = server(true, &notion).await;

        let listed: Value = reqwest::get(format!("{server}/tools"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            listed,
            json!({"tools": [{"name": "notion-fetch"}, {"name": "notion-get-comments"}]})
        );
    }

    #[tokio::test]
    async fn without_a_grant_says_how_to_install_one() {
        let (notion, fake) = fake().await;
        let (server, _) = server(false, &notion).await;

        let (status, body) = call(&server, "notion-fetch", json!({})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].as_str().unwrap().contains("rho notion init"));
        assert!(fake.lock().unwrap().seen.is_empty());
    }
}
