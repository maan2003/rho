//! GitHub REST operations from the same pinned metadata used by ghapi.
//! Every REST operation has a generated typed handler. Credentials and signed
//! redirects stay on the host; branch-changing APIs are not exposed.
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use crate::state::AppState;

#[path = "gh_generated.rs"]
mod generated;

type HandlerFuture = Pin<Box<dyn Future<Output = Response> + Send>>;
type Handler = fn(Request) -> HandlerFuture;
type ResponseCodec = fn(StatusCode, Value) -> Result<Value, serde_json::Error>;

struct Request {
    state: Arc<AppState>,
    op: &'static Operation,
    parts: Vec<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

// Distinguish omission from null in PATCH requests and optional response
// fields.
enum Optional<T> {
    Missing,
    Present(T),
}

impl<T> Default for Optional<T> {
    fn default() -> Self {
        Self::Missing
    }
}

impl<T> Optional<T> {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Optional<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Present)
    }
}

impl<T: Serialize> Serialize for Optional<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => serializer.serialize_none(),
            Self::Present(value) => value.serialize(serializer),
        }
    }
}

// URL parameters are strings; schemas decide whether each is text, bool,
// numeric, or an array. Try the declared type before any coercion so "123"
// remains text for String fields.
fn query_value<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    deserializer: D,
) -> Result<T, D::Error> {
    let value = Value::deserialize(deserializer)?;
    fn scalar(value: &Value) -> Value {
        if let Value::String(text) = value {
            if let Ok(value) = serde_json::from_str(text) {
                return value;
            }
            if let Ok(value) = text.parse::<i64>() {
                return json!(value);
            }
            if let Ok(value) = text.parse::<u64>() {
                return json!(value);
            }
        }
        value.clone()
    }
    if let Ok(result) = serde_json::from_value(value.clone()) {
        return Ok(result);
    }
    let mut choices = vec![scalar(&value)];
    match &value {
        Value::Array(values) => choices.push(Value::Array(values.iter().map(scalar).collect())),
        Value::String(text) => {
            let values: Vec<_> = text
                .split(',')
                .map(|part| Value::String(part.to_owned()))
                .collect();
            choices.push(Value::Array(values.clone()));
            choices.push(Value::Array(values.iter().map(scalar).collect()));
        }
        _ => {}
    }
    for value in choices {
        if let Ok(result) = serde_json::from_value(value) {
            return Ok(result);
        }
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

fn query_object(uri: &Uri) -> Value {
    let mut fields = serde_json::Map::new();
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
        let bracketed = key.ends_with("[]");
        let key = key.strip_suffix("[]").unwrap_or(&key).to_owned();
        let value = Value::String(value.into_owned());
        match fields.entry(key) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(if bracketed {
                    Value::Array(vec![value])
                } else {
                    value
                });
            }
            serde_json::map::Entry::Occupied(mut entry) => {
                let previous = entry.get_mut();
                match previous {
                    Value::Array(values) => values.push(value),
                    _ => *previous = Value::Array(vec![previous.take(), value]),
                }
            }
        }
    }
    Value::Object(fields)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/review-decision",
            get(get_review_decision),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/draft",
            axum::routing::put(set_draft),
        )
        .route("/{*path}", any(rest))
        .layer(DefaultBodyLimit::disable())
}

#[derive(Deserialize)]
struct Spec {
    ops: Vec<Operation>,
}

#[derive(Deserialize)]
struct Operation {
    group: String,
    name: String,
    path: String,
    verb: String,
    #[serde(default)]
    query_params: Vec<String>,
    #[serde(default)]
    param_types: std::collections::BTreeMap<String, String>,
}

fn operations() -> &'static [Operation] {
    static OPS: OnceLock<Vec<Operation>> = OnceLock::new();
    OPS.get_or_init(|| {
        let mut ops = serde_json::from_str::<Spec>(include_str!(
            "../../../rho-notebook/src/ghapi/gh_spec.json"
        ))
        .expect("bundled ghapi metadata is valid")
        .ops;
        // Prefer literal routes (e.g. releases/latest) over parameter routes.
        ops.sort_by_key(|op| {
            std::cmp::Reverse(
                op.path
                    .split('/')
                    .filter(|part| !part.starts_with('{'))
                    .map(str::len)
                    .sum::<usize>(),
            )
        });
        ops
    })
}

fn denied(reason: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"message": reason}))).into_response()
}

fn forbidden() -> Response {
    denied("GitHub operation unavailable through Octo")
}

fn positive(id: &str) -> bool {
    id.parse::<u64>().is_ok_and(|id| id > 0)
}

// Slash-bearing refs/paths may appear before a literal suffix such as
// /check-runs. Other parameters (owner, repo, IDs, etc.) must remain a single
// path component.
fn match_path(template: &str, path: &str) -> Option<Vec<String>> {
    fn walk(template: &[&str], parts: &[&str]) -> Option<Vec<String>> {
        let Some((first, rest)) = template.split_first() else {
            return parts.is_empty().then(Vec::new);
        };
        if first.starts_with('{') && first.ends_with('}') {
            let name = first
                .trim_start_matches('{')
                .trim_start_matches('+')
                .trim_end_matches('}');
            let slash = matches!(
                name,
                "ref" | "branch" | "path" | "basehead" | "head" | "base" | "tag"
            );
            for count in (1..=if slash {
                parts.len()
            } else {
                parts.len().min(1)
            })
                .rev()
            {
                if let Some(mut result) = walk(rest, &parts[count..]) {
                    result.insert(0, parts[..count].join("/"));
                    return Some(result);
                }
            }
            None
        } else {
            let (part, tail) = parts.split_first()?;
            if part != first {
                return None;
            }
            let mut result = walk(rest, tail)?;
            result.insert(0, (*part).to_owned());
            Some(result)
        }
    }
    // Reject normalization tricks before matching or acquiring credentials.
    if path.split('/').any(|part| matches!(part, "." | "..")) {
        return None;
    }
    let decoded: Vec<_> = path
        .split('/')
        .map(|part| percent_encoding::percent_decode_str(part).decode_utf8())
        .collect::<Result<_, _>>()
        .ok()?;
    if decoded.iter().any(|part| {
        part.split('/')
            .any(|component| matches!(component, "." | ".."))
    }) {
        return None;
    }
    let parts: Vec<_> = decoded.iter().map(|part| part.as_ref()).collect();
    if parts
        .iter()
        .enumerate()
        .any(|(i, part)| part.is_empty() && i + 1 != parts.len())
    {
        return None;
    }
    walk(
        &template
            .trim_start_matches('/')
            .split('/')
            .collect::<Vec<_>>(),
        &parts,
    )
}

async fn rest(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((op, parts)) = operations()
        .iter()
        .filter(|op| op.verb == method.as_str())
        .find_map(|op| {
            match_path(&op.path, uri.path().trim_start_matches('/')).map(|parts| (op, parts))
        })
    else {
        return forbidden();
    };
    let Some(handler) = generated::handler(&op.group, &op.name) else {
        return forbidden();
    };
    handler(Request {
        state,
        op,
        parts,
        uri,
        headers,
        body,
    })
    .await
}

async fn typed<P, Q, B>(
    request: Request,
    body_required: bool,
    json_body: bool,
    response_codec: ResponseCodec,
) -> Response
where
    P: DeserializeOwned + Serialize,
    Q: DeserializeOwned + Serialize,
    B: DeserializeOwned + Serialize,
{
    let Request {
        state,
        op,
        parts,
        uri,
        headers,
        body,
    } = request;
    // Typed schema validation happens before acquiring host credentials.

    let (has_path, path) = {
        let mut path_fields = url::form_urlencoded::Serializer::new(String::new());
        let mut has_path = false;
        for (template, value) in op.path.trim_start_matches('/').split('/').zip(&parts) {
            if template.starts_with('{') {
                has_path = true;
                let name = template
                    .trim_start_matches('{')
                    .trim_start_matches('+')
                    .trim_end_matches('}');
                if op.param_types.get(name).is_some_and(|kind| kind == "int") && !positive(value) {
                    return forbidden();
                }
                path_fields.append_pair(
                    template
                        .trim_start_matches('{')
                        .trim_start_matches('+')
                        .trim_end_matches('}'),
                    value,
                );
            }
        }
        (has_path, path_fields.finish())
    };
    let valid_path = if has_path {
        serde_urlencoded::from_str::<P>(&path).is_ok()
    } else {
        serde_json::from_value::<P>(Value::Null).is_ok()
    };
    if !valid_path {
        return forbidden();
    }
    let query = query_object(&uri);
    let query = if query.as_object().is_some_and(|fields| fields.is_empty())
        && op.query_params.is_empty()
    {
        Value::Null
    } else {
        query
    };
    if serde_json::from_value::<Q>(query).is_err() {
        return forbidden();
    }
    let body = if json_body {
        if body.is_empty() {
            if body_required {
                return forbidden();
            }
            body
        } else {
            let Ok(value) = serde_json::from_slice::<B>(&body) else {
                return forbidden();
            };
            Bytes::from(serde_json::to_vec(&value).expect("typed body serializes"))
        }
    } else {
        if !body.is_empty() {
            return forbidden();
        }
        body
    };
    let token = match state.get_token().await {
        Ok(token) => token,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut url = state.github_api_url.clone();
    url.path_segments_mut()
        .expect("GitHub base is hierarchical")
        .clear()
        .extend(&parts);
    url.set_query(uri.query());
    let mut outbound = HeaderMap::new();
    outbound.insert(header::USER_AGENT, "octo-gh".parse().unwrap());
    outbound.insert("x-github-api-version", "2022-11-28".parse().unwrap());
    outbound.insert(
        header::ACCEPT,
        "application/vnd.github+json".parse().unwrap(),
    );
    // No Authorization, Host, cookies or arbitrary routing headers from agents.
    for name in [
        "accept",
        "content-type",
        "if-match",
        "if-none-match",
        "if-modified-since",
        "if-unmodified-since",
        "range",
        "x-github-api-version",
    ] {
        if let Some(value) = headers.get(name) {
            outbound.insert(name, value.clone());
        }
    }
    let request = state
        .client
        .request(op.verb.parse::<Method>().expect("schema method"), url)
        .bearer_auth(&token)
        .headers(outbound);
    let upstream = match request.body(body).send().await {
        Ok(response) => response,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let text_media = op.group == "pulls"
        && op.name == "get"
        && headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                matches!(
                    value,
                    "application/vnd.github.diff" | "application/vnd.github.patch"
                )
            });
    relay(state, upstream, &token, op, response_codec, text_media).await
}

// Repo metadata and nested search/PR responses can carry ephemeral credentials.
// Typed response models retain the schema fields; capabilities are removed
// here.
fn redact(value: &mut Value, token: &str) {
    match value {
        Value::Object(fields) => {
            if fields.get("full_name").is_some_and(Value::is_string) {
                fields.remove("temp_clone_token");
            }
            for child in fields.values_mut() {
                redact(child, token);
            }
        }
        Value::Array(values) => {
            for child in values {
                redact(child, token);
            }
        }
        Value::String(text) => *text = text.replace(token, "[redacted]"),
        _ => {}
    }
}

async fn relay(
    state: Arc<AppState>,
    upstream: reqwest::Response,
    token: &str,
    op: &Operation,
    response_codec: ResponseCodec,
    text_media: bool,
) -> Response {
    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for name in [
        "content-type",
        "link",
        "etag",
        "last-modified",
        "retry-after",
        "x-ratelimit-remaining",
        "x-ratelimit-limit",
        "x-ratelimit-reset",
        "x-ratelimit-used",
        "x-ratelimit-resource",
    ] {
        if let Some(value) = upstream.headers().get(name) {
            if value.to_str().is_ok_and(|text| !text.contains(token)) {
                headers.insert(name, value.clone());
            }
        }
    }
    if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
        if status != StatusCode::FOUND && status != StatusCode::TEMPORARY_REDIRECT {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        if !download_operation(op) {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        let Some(location) = upstream
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        let Ok(url) = reqwest::Url::parse(location) else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        if url.scheme() != state.github_api_url.scheme()
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.scheme(), "https" | "http")
        {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        // A known GitHub download operation may return a signed URL. Fetch once,
        // without credentials, and never expose Location or follow another redirect.
        let downloaded = match state.client.get(url).send().await {
            Ok(response) if response.status().is_success() => response,
            _ => return StatusCode::BAD_GATEWAY.into_response(),
        };
        let content_type = if op.name == "download_job_logs_for_workflow_run" {
            "text/plain; charset=utf-8"
        } else {
            "application/octet-stream"
        };
        let bytes = match downloaded.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        };
        if bytes
            .windows(token.len())
            .any(|part| part == token.as_bytes())
        {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        return ([(header::CONTENT_TYPE, content_type)], bytes).into_response();
    }
    let json_response = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("json"));
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    if let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
        if status.is_success() {
            value = match response_codec(status, value) {
                Ok(value) => value,
                Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
            };
        }
        redact(&mut value, token);
        (status, headers, Json(value)).into_response()
    } else if !bytes.is_empty() && (json_response || status.is_success() && !text_media) {
        StatusCode::BAD_GATEWAY.into_response()
    } else if bytes
        .windows(token.len())
        .any(|part| part == token.as_bytes())
    {
        StatusCode::BAD_GATEWAY.into_response()
    } else {
        (status, headers, bytes).into_response()
    }
}

fn download_operation(op: &Operation) -> bool {
    op.group == "actions"
        && matches!(
            op.name.as_str(),
            "download_job_logs_for_workflow_run" | "download_workflow_run_logs"
        )
}

#[derive(Deserialize)]
struct GitHubError {
    message: String,
}

fn github_error(status: StatusCode, bytes: &[u8]) -> Response {
    let message = serde_json::from_slice::<GitHubError>(bytes)
        .map(|error| error.message)
        .unwrap_or_else(|_| "GitHub request failed".into());
    (status, Json(json!({"message": message}))).into_response()
}
#[derive(Serialize)]
struct ReviewDecision {
    review_decision: String,
}

#[derive(Deserialize)]
struct GraphQlResult {
    data: Option<GraphQlData>,
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct GraphQlData {
    repository: Option<GraphQlRepository>,
}

#[derive(Deserialize)]
struct GraphQlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GraphQlPull>,
}

#[derive(Deserialize)]
struct GraphQlPull {
    #[serde(rename = "reviewDecision")]
    review_decision: Option<String>,
}

async fn get_review_decision(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    if uri.query().is_some() || !positive(&number) {
        return forbidden();
    }
    let Ok(number) = number.parse::<i32>() else {
        return forbidden();
    };
    if number <= 0 {
        return forbidden();
    }
    let token = match state.get_token().await {
        Ok(token) => token,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut url = state.github_api_url.clone();
    url.set_path("/graphql");
    let upstream = match state.client.post(url)
        .bearer_auth(&token)
        .header("User-Agent", "octo-gh")
        .header("Accept", "application/vnd.github+json")
        .json(&json!({
            "query": "query($owner: String!, $repo: String!, $number: Int!) { repository(owner: $owner, name: $repo) { pullRequest(number: $number) { reviewDecision } } }",
            "variables": {"owner":owner, "repo":repo, "number":number}
        }))
        .send().await
    {
        Ok(response) => response,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let status = upstream.status();
    if status.is_redirection() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    if !status.is_success() {
        let clean = String::from_utf8_lossy(&bytes).replace(&token, "[redacted]");
        return github_error(status, clean.as_bytes());
    }
    let Ok(result) = serde_json::from_slice::<GraphQlResult>(&bytes) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if result.errors.is_some() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let Some(pull) = result
        .data
        .and_then(|data| data.repository)
        .and_then(|repo| repo.pull_request)
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    Json(ReviewDecision {
        review_decision: pull.review_decision.unwrap_or_else(|| "NONE".into()),
    })
    .into_response()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DraftState {
    draft: bool,
}

#[derive(Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    errors: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct DraftLookup {
    repository: Option<DraftRepository>,
}
#[derive(Deserialize)]
struct DraftRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<DraftPull>,
}
#[derive(Deserialize)]
struct DraftPull {
    id: String,
}

#[derive(Deserialize)]
struct DraftMutation {
    draft: Option<DraftMutationPayload>,
}
#[derive(Deserialize)]
struct DraftMutationPayload {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GraphQlDraftState>,
}
#[derive(Deserialize)]
struct GraphQlDraftState {
    #[serde(rename = "isDraft")]
    draft: bool,
}

async fn graphql<T: DeserializeOwned>(
    state: &AppState,
    token: &str,
    query: &'static str,
    variables: Value,
) -> Result<T, Response> {
    let mut url = state.github_api_url.clone();
    url.set_path("/graphql");
    let upstream = state
        .client
        .post(url)
        .bearer_auth(token)
        .header("User-Agent", "octo-gh")
        .json(&json!({"query":query,"variables":variables}))
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY.into_response())?;
    let status = upstream.status();
    if status.is_redirection() {
        return Err(StatusCode::BAD_GATEWAY.into_response());
    }
    let bytes = upstream
        .bytes()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY.into_response())?;
    if !status.is_success() {
        let clean = String::from_utf8_lossy(&bytes).replace(token, "[redacted]");
        return Err(github_error(status, clean.as_bytes()));
    }
    let result: GraphQlEnvelope<T> =
        serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_GATEWAY.into_response())?;
    if result.errors.is_some_and(|errors| !errors.is_empty()) {
        return Err(StatusCode::BAD_GATEWAY.into_response());
    }
    result
        .data
        .ok_or_else(|| StatusCode::BAD_GATEWAY.into_response())
}

async fn set_draft(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    uri: Uri,
    body: Result<Json<DraftState>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(requested)) = body else {
        return forbidden();
    };
    if uri.query().is_some() || !positive(&number) {
        return forbidden();
    }
    let Ok(number) = number.parse::<i32>() else {
        return forbidden();
    };
    let token = match state.get_token().await {
        Ok(token) => token,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let lookup = graphql::<DraftLookup>(&state, &token,
        "query($owner: String!, $repo: String!, $number: Int!) { repository(owner: $owner, name: $repo) { pullRequest(number: $number) { id } } }",
        json!({"owner":owner,"repo":repo,"number":number})).await;
    let data = match lookup {
        Ok(data) => data,
        Err(error) => return error,
    };
    let Some(pull) = data.repository.and_then(|repo| repo.pull_request) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mutation = if requested.draft {
        "mutation($id: ID!) { draft: convertPullRequestToDraft(input: {pullRequestId: $id}) { pullRequest { isDraft } } }"
    } else {
        "mutation($id: ID!) { draft: markPullRequestReadyForReview(input: {pullRequestId: $id}) { pullRequest { isDraft } } }"
    };
    let data = match graphql::<DraftMutation>(&state, &token, mutation, json!({"id":pull.id})).await
    {
        Ok(data) => data,
        Err(error) => return error,
    };
    let Some(actual) = data.draft.and_then(|payload| payload.pull_request) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if actual.draft != requested.draft {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    Json(DraftState {
        draft: actual.draft,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::task::JoinHandle;

    use super::*;

    async fn serve(router: Router) -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (url, task)
    }
    fn token_provider() -> crate::TokenProvider {
        Arc::new(|| Ok("host-token-test".into()))
    }
    fn example(operation: &str) -> (StatusCode, Value) {
        let fixtures: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/github-responses.json"))
                .unwrap();
        let fixture = &fixtures["responses"][operation];
        (
            StatusCode::from_u16(fixture["status"].as_u64().unwrap() as u16).unwrap(),
            fixture["value"].clone(),
        )
    }

    #[test]
    fn selected_operations_have_typed_handlers_and_exclude_authoritative_writes() {
        let mut count = 0;
        for op in operations() {
            if matches!(op.name.as_str(), "review_decision" | "set_draft") {
                continue;
            }
            assert!(
                generated::handler(&op.group, &op.name).is_some(),
                "{}.{}",
                op.group,
                op.name
            );
            count += 1;
        }
        assert_eq!(count, 103);
        for name in ["merge", "merge_async", "update_branch", "dismiss_review"] {
            assert!(generated::handler("pulls", name).is_none(), "{name}");
        }
        for (group, name) in [
            ("repos", "get"),
            ("git", "create_ref"),
            ("apps", "create_installation_access_token"),
        ] {
            assert!(generated::handler(group, name).is_none());
        }
    }

    #[test]
    fn requested_reviewers_responses_match_official_and_current_public_samples() {
        for sample in ["pulls/request-reviewers", "pulls/request-reviewers-current"] {
            let (_, value) = example(sample);
            serde_json::from_value::<generated::PullsRequestReviewersResponse201>(value).unwrap();
        }
    }

    #[test]
    fn output_projection_retains_union_fields_and_distinguishes_null_from_omission() {
        let original = json!({"body":"edited","performed_via_github_app":{"owner":{
            "id":73,"slug":"enterprise-owner","created_at":"2026-01-02T03:04:05Z",
            "website_url":"https://example.com"
        }}});
        let projected = serde_json::to_value(
            serde_json::from_value::<generated::IssuesUpdateCommentResponse200>(original.clone())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(projected, original);
        for (value, expected) in [
            (json!({}), json!({})),
            (json!({"milestone":null}), json!({"milestone":null})),
        ] {
            let projected = serde_json::to_value(
                serde_json::from_value::<generated::IssuesUpdateResponse200>(value).unwrap(),
            )
            .unwrap();
            assert_eq!(projected, expected);
        }
        assert!(
            serde_json::from_value::<generated::IssuesUpdateResponse200>(
                json!({"number":"wrong type"})
            )
            .is_err()
        );

        let events = json!([
            {"event":"renamed","rename":{"from":"old name","to":"new name"}},
            {"event":"committed","sha":"commit-73","message":"a change"},
            {"event":"cross-referenced","source":{"type":"issue","issue":{"number":41,"title":"linked issue"}}}
        ]);
        let projected = serde_json::to_value(
            serde_json::from_value::<generated::IssuesListEventsForTimelineResponse200>(
                events.clone(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(projected, events);
    }

    #[test]
    fn templates_and_query_coercion_preserve_nontrivial_values() {
        assert_eq!(
            match_path(
                "/repos/{owner}/{repo}/commits/{ref}/status",
                "repos/acme/widget/commits/heads/rho/fix%252Fpercent/status"
            ),
            Some(
                vec![
                    "repos",
                    "acme",
                    "widget",
                    "commits",
                    "heads/rho/fix%2Fpercent",
                    "status"
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            )
        );
        assert_eq!(
            match_path(
                "/repos/{owner}/{repo}/labels/{name}",
                "repos/acme/widget/labels/UI%2F%E4%BF%AE%E6%AD%A3"
            ),
            Some(
                vec!["repos", "acme", "widget", "labels", "UI/修正"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            )
        );
        for path in [
            "repos/acme/widget/commits/%2E%2E%2Fmain/status",
            "repos/acme/widget/commits/heads//fix/status",
            "repos/acme/widget/commits/heads/%FF/status",
        ] {
            assert!(
                match_path("/repos/{owner}/{repo}/commits/{ref}/status", path).is_none(),
                "{path}"
            );
        }
        #[derive(Deserialize)]
        struct QueryExample {
            #[serde(deserialize_with = "query_value")]
            q: String,
            #[serde(deserialize_with = "query_value")]
            page: u64,
            #[serde(deserialize_with = "query_value")]
            all: bool,
        }
        let uri: Uri = "/search?q=123&page=003&all=false".parse().unwrap();
        let decoded: QueryExample = serde_json::from_value(query_object(&uri)).unwrap();
        assert_eq!(decoded.q, "123");
        assert_eq!(decoded.page, 3);
        assert!(!decoded.all);
        let uri: Uri = "/search?q=text&page=2&page=3&all=false".parse().unwrap();
        assert!(serde_json::from_value::<QueryExample>(query_object(&uri)).is_err());
    }

    #[tokio::test]
    async fn pr_issue_and_search_handlers_forward_typed_payloads_and_fields() {
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let seen = captured.clone();
        let (upstream, upstream_task) = serve(Router::new().fallback(any(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let seen = seen.clone();
                async move {
                    assert_eq!(
                        headers.get(header::AUTHORIZATION).unwrap(),
                        "Bearer host-token-test"
                    );
                    assert!(!headers.contains_key(header::COOKIE));
                    assert!(!headers.contains_key("x-forwarded-host"));
                    seen.lock().await.push((
                        method.clone(),
                        uri.to_string(),
                        serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
                    ));
                    let op = operations()
                        .iter()
                        .filter(|op| op.verb == method.as_str())
                        .find(|op| {
                            match_path(&op.path, uri.path().trim_start_matches('/')).is_some()
                        })
                        .unwrap();
                    let id = format!(
                        "{}/{}",
                        op.group.replace('_', "-"),
                        op.name.replace('_', "-")
                    );
                    let (status, mut value) = example(&id);
                    if let Some(fields) = value.as_object_mut() {
                        fields.insert("future_field".into(), json!("not a typed response field"));
                        if fields.contains_key("body") {
                            fields.insert("body".into(), json!("echo host-token-test end"));
                        }
                        if fields.contains_key("closed_at") {
                            fields.insert("closed_at".into(), json!("2026-01-02T03:04:05Z"));
                        }
                    }
                    (
                        status,
                        [
                            (header::ETAG, "\"rev-2\""),
                            (
                                header::LINK,
                                "<https://api.github.com/next?page=3>; rel=\"next\"",
                            ),
                        ],
                        Json(value),
                    )
                }
            },
        )))
        .await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        for (method, path, payload, status) in [
            (
                Method::PATCH,
                "repos/acme/widget/issues/comments/19",
                json!({"body":"edited"}),
                StatusCode::OK,
            ),
            (
                Method::PATCH,
                "repos/acme/widget/pulls/comments/23",
                json!({"body":"inline edited"}),
                StatusCode::OK,
            ),
            (
                Method::POST,
                "repos/acme/widget/issues",
                json!({"title":73,"labels":["bug"],"issue_field_values":[{"field_id":5,"value":["High","UI"]}]}),
                StatusCode::CREATED,
            ),
            (
                Method::PATCH,
                "repos/acme/widget/issues/7",
                json!({"milestone":null,"type":null,"body":null,"labels":[]}),
                StatusCode::OK,
            ),
            (
                Method::PATCH,
                "repos/acme/widget/issues/7",
                json!({"body":"only body"}),
                StatusCode::OK,
            ),
            (
                Method::POST,
                "repos/acme/widget/issues/7/assignees",
                json!({"assignees":["alice","bob"]}),
                StatusCode::CREATED,
            ),
            (
                Method::POST,
                "repos/acme/widget/issues/7/labels",
                json!({"labels":["bug","UI"]}),
                StatusCode::OK,
            ),
            (
                Method::PUT,
                "repos/acme/widget/issues/7/labels",
                json!(["UI"]),
                StatusCode::OK,
            ),
            (
                Method::POST,
                "repos/acme/widget/labels",
                json!({"name":"urgent","color":"abcdef"}),
                StatusCode::CREATED,
            ),
            (
                Method::POST,
                "repos/acme/widget/milestones",
                json!({"title":"v2","due_on":"2026-10-10T12:00:00Z"}),
                StatusCode::CREATED,
            ),
            (
                Method::POST,
                "repos/acme/widget/pulls/7/reviews",
                json!({"event":"COMMENT","body":"review","comments":[{"path":"foo","line":5,"side":"RIGHT","body":"check"}]}),
                StatusCode::OK,
            ),
            (
                Method::POST,
                "repos/acme/widget/pulls/7/requested_reviewers",
                json!({"reviewers":["alice"],"team_reviewers":["maintainers"]}),
                StatusCode::CREATED,
            ),
        ] {
            let response = client
                .request(method.clone(), format!("{base}/{path}"))
                .header(header::AUTHORIZATION, "Bearer agent-token")
                .header(header::COOKIE, "secret=cookie")
                .header("x-forwarded-host", "evil.example")
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{method} {path}");
            assert_eq!(captured.lock().await.last().unwrap().2, payload, "{path}");
            assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-2\"");
            let value: Value = response.json().await.unwrap();
            assert!(value.get("future_field").is_none());
            if let Some(body) = value.get("body") {
                assert_eq!(body, "echo [redacted] end");
            }
            if let Some(closed) = value.get("closed_at") {
                assert_eq!(closed, "2026-01-02T03:04:05Z");
            }
        }
        for (path, expected) in [
            (
                "search/issues?q=repo%3Aacme%2Fwidget%20is%3Apr%20foo%2Bbar&page=3",
                "/search/issues?q=repo%3Aacme%2Fwidget%20is%3Apr%20foo%2Bbar&page=3",
            ),
            (
                "repos/acme/widget/commits/heads/rho/fix%252Fother/status",
                "/repos/acme/widget/commits/heads%2Frho%2Ffix%252Fother/status",
            ),
            (
                "repos/acme/widget/labels/UI%2F%E4%BF%AE%E6%AD%A3",
                "/repos/acme/widget/labels/UI%2F%E4%BF%AE%E6%AD%A3",
            ),
            (
                "repos/acme/widget/pulls/7/commits",
                "/repos/acme/widget/pulls/7/commits",
            ),
        ] {
            let response = client.get(format!("{base}/{path}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(captured.lock().await.last().unwrap().1, expected);
            let _: Value = response.json().await.unwrap();
        }
        for (path, query) in [
            ("code", "language:Rust foo"),
            ("commits", "repo:acme/widget fix"),
            ("repositories", "language:Rust stars:>20"),
            ("labels", "repo:acme/widget bug"),
            ("topics", "rust"),
            ("users", "alice"),
        ] {
            let mut query = vec![("q", query.to_owned()), ("per_page", "7".into())];
            if path == "labels" {
                query.push(("repository_id", "73".into()));
            }
            let response = client
                .get(format!("{base}/search/{path}"))
                .query(&query)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "search/{path}");
            let value: Value = response.json().await.unwrap();
            assert!(value["total_count"].is_number());
            assert!(value["items"].is_array());
        }
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn invalid_or_authoritative_requests_never_obtain_credentials() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let provider: crate::TokenProvider = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok("host-token-test".into())
        });
        let (base, task) = serve(crate::router(
            provider,
            "http://127.0.0.1:1".parse().unwrap(),
        ))
        .await;
        let client = reqwest::Client::new();
        for (method, path, body) in [
            (Method::PUT, "repos/acme/widget/pulls/7/merge", json!({})),
            (
                Method::PUT,
                "repos/acme/widget/pulls/7/merge-async",
                json!({}),
            ),
            (
                Method::PUT,
                "repos/acme/widget/pulls/7/update-branch",
                json!({}),
            ),
            (
                Method::PUT,
                "repos/acme/widget/pulls/7/reviews/9/dismissals",
                json!({"message":"dismiss"}),
            ),
            (
                Method::POST,
                "repos/acme/widget/git/refs",
                json!({"ref":"refs/heads/main","sha":"a"}),
            ),
            (Method::POST, "graphql", json!({"query":"mutation { ... }"})),
            (
                Method::PATCH,
                "repos/acme/widget/issues/comments/19",
                json!({}),
            ),
            (
                Method::PATCH,
                "repos/acme/widget/issues/comments/19",
                json!({"body":19}),
            ),
            (
                Method::PATCH,
                "repos/acme/widget/issues/comments/19",
                json!({"body":"edited","boddy":"typo"}),
            ),
            (
                Method::POST,
                "repos/acme/widget/issues",
                json!({"title":true}),
            ),
            (
                Method::POST,
                "repos/acme/widget/issues",
                json!({"title":"Issue","issue_field_values":[{"field_id":"five","value":"x"}]}),
            ),
            (
                Method::POST,
                "repos/acme/widget/pulls/7/reviews",
                json!({"event":"COMMENT","comments":[{"path":"foo","line":"five","body":"text"}]}),
            ),
            (Method::GET, "search/issues?page=3", Value::Null),
            (
                Method::GET,
                "repos/acme/widget/issues/7",
                json!({"unexpected":"body"}),
            ),
            (
                Method::GET,
                "repos/acme/widget/issues?sttae=all",
                Value::Null,
            ),
            (
                Method::GET,
                "repos/acme/widget/issues?page=next",
                Value::Null,
            ),
            (
                Method::GET,
                "repos/acme/widget/issues?page=2&page=3",
                Value::Null,
            ),
            (
                Method::GET,
                "repos/acme/widget/issues/not-a-number",
                Value::Null,
            ),
            (Method::GET, "repos/acme/widget/issues/0", Value::Null),
            (Method::GET, "repos/acme/widget/pulls/-1", Value::Null),
            (
                Method::PUT,
                "repos/acme/widget/pulls/7/draft",
                json!({"draft":"false"}),
            ),
            (
                Method::PUT,
                "repos/acme/widget/pulls/7/draft?extra=true",
                json!({"draft":false}),
            ),
        ] {
            let mut request = client.request(method.clone(), format!("{base}/{path}"));
            if !body.is_null() {
                request = request.json(&body);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        task.abort();
    }

    #[tokio::test]
    async fn draft_ready_transitions_use_fixed_typed_graphql() {
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured = seen.clone();
        let (upstream, upstream_task) = serve(Router::new().route(
            "/graphql",
            axum::routing::post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let captured = captured.clone();
                async move {
                    assert_eq!(
                        headers.get(header::AUTHORIZATION).unwrap(),
                        "Bearer host-token-test"
                    );
                    captured.lock().await.push(body.clone());
                    let query = body["query"].as_str().unwrap();
                    if query.starts_with("query") {
                        assert_eq!(
                            body["variables"],
                            json!({"owner":"other","repo":"project","number":41})
                        );
                        Json(json!({"data":{"repository":{"pullRequest":{"id":"PR_node_41"}}}}))
                    } else {
                        assert_eq!(body["variables"], json!({"id":"PR_node_41"}));
                        let draft = query.contains("convertPullRequestToDraft");
                        assert!(draft || query.contains("markPullRequestReadyForReview"));
                        Json(json!({"data":{"draft":{"pullRequest":{"isDraft":draft}}}}))
                    }
                }
            }),
        ))
        .await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        for draft in [false, true] {
            let response = client
                .put(format!("{base}/repos/other/project/pulls/41/draft"))
                .json(&json!({"draft":draft}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({"draft":draft})
            );
        }
        assert_eq!(seen.lock().await.len(), 4);
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn text_empty_errors_and_malformed_typed_responses_keep_their_semantics() {
        let (upstream,upstream_task)=serve(Router::new().fallback(any(
            |method:Method,uri:Uri,headers:HeaderMap| async move {
                assert_eq!(headers.get(header::AUTHORIZATION).unwrap(),"Bearer host-token-test");
                match (method.as_str(),uri.path()) {
                    ("GET","/repos/acme/widget/pulls/7")=>{
                        assert_eq!(headers.get(header::ACCEPT).unwrap(),"application/vnd.github.diff");
                        ([(header::CONTENT_TYPE,"text/plain")],"diff --git a/foo b/foo\n+fixed").into_response()
                    }
                    ("DELETE","/repos/acme/widget/issues/comments/7")=>StatusCode::NO_CONTENT.into_response(),
                    ("GET","/repos/acme/widget/issues/7")=>{
                        assert_eq!(headers.get(header::IF_NONE_MATCH).unwrap(),"\"etag\"");
                        StatusCode::NOT_MODIFIED.into_response()
                    }
                    ("GET","/repos/acme/widget/issues/404")=>(StatusCode::NOT_FOUND,Json(json!({"message":"Not found","errors":[{"field":"number","code":"missing"}]}))).into_response(),
                    ("GET","/repos/acme/widget/issues/500")=>(StatusCode::INTERNAL_SERVER_ERROR,"echo host-token-test").into_response(),
                    ("GET","/repos/acme/widget/issues/999")=>Json(json!({"number":"not an integer"})).into_response(),
                    _=>panic!("unexpected route {method} {uri}"),
                }
            }
        ))).await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{base}/repos/acme/widget/pulls/7"))
            .header(header::ACCEPT, "application/vnd.github.diff")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.text().await.unwrap(),
            "diff --git a/foo b/foo\n+fixed"
        );
        for (method, path, status) in [
            (
                Method::DELETE,
                "repos/acme/widget/issues/comments/7",
                StatusCode::NO_CONTENT,
            ),
            (
                Method::GET,
                "repos/acme/widget/issues/7",
                StatusCode::NOT_MODIFIED,
            ),
            (
                Method::GET,
                "repos/acme/widget/issues/500",
                StatusCode::BAD_GATEWAY,
            ),
            (
                Method::GET,
                "repos/acme/widget/issues/999",
                StatusCode::BAD_GATEWAY,
            ),
        ] {
            let response = client
                .request(method, format!("{base}/{path}"))
                .header(header::IF_NONE_MATCH, "\"etag\"")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{path}");
            assert!(response.bytes().await.unwrap().is_empty());
        }
        let response = client
            .get(format!("{base}/repos/acme/widget/issues/404"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.json::<Value>().await.unwrap()["errors"][0]["code"],
            "missing"
        );
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn log_downloads_follow_once_without_credentials_or_signed_locations() {
        let downloads = Arc::new(AtomicUsize::new(0));
        let seen = downloads.clone();
        let (download_url, download_task) = serve(Router::new().fallback(any(
            move |uri: Uri, headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    assert!(!headers.contains_key(header::AUTHORIZATION));
                    assert!(!headers.contains_key(header::COOKIE));
                    assert_eq!(uri.query(), Some("signature=signed-capability"));
                    seen.fetch_add(1, Ordering::SeqCst);
                    match uri.path() {
                        "/again" => (
                            StatusCode::FOUND,
                            [(header::LOCATION, "https://evil.example/secret")],
                        )
                            .into_response(),
                        "/secret" => "host-token-test".into_response(),
                        _ => b"\x00\xfe\xffdownload".to_vec().into_response(),
                    }
                }
            },
        )))
        .await;
        let (upstream, upstream_task) = serve(Router::new().fallback(any(
            move |uri: Uri, headers: HeaderMap| {
                let download_url = download_url.clone();
                async move {
                    assert_eq!(
                        headers.get(header::AUTHORIZATION).unwrap(),
                        "Bearer host-token-test"
                    );
                    let target = match uri.query() {
                        Some("again=true") => "again",
                        Some("secret=true") => "secret",
                        _ => "file",
                    };
                    (
                        StatusCode::FOUND,
                        [(
                            header::LOCATION,
                            format!("{download_url}/{target}?signature=signed-capability"),
                        )],
                    )
                }
            },
        )))
        .await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        for path in [
            "repos/acme/widget/actions/jobs/7/logs",
            "repos/acme/widget/actions/runs/7/logs",
        ] {
            let response = client.get(format!("{base}/{path}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key(header::LOCATION));
            assert_eq!(
                response.bytes().await.unwrap().as_ref(),
                b"\x00\xfe\xffdownload"
            );
        }
        assert_eq!(downloads.load(Ordering::SeqCst), 2);
        for path in [
            "repos/acme/widget/issues/7",
            "repos/acme/widget/actions/runs/7/logs?again=true",
            "repos/acme/widget/actions/runs/7/logs?secret=true",
        ] {
            let response = client.get(format!("{base}/{path}")).send().await.unwrap();
            // Typed schemas reject undeclared query flags before upstream.
            let status = if path.contains('?') {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_GATEWAY
            };
            assert_eq!(response.status(), status);
            assert!(!response.headers().contains_key(header::LOCATION));
        }
        assert_eq!(downloads.load(Ordering::SeqCst), 2);
        task.abort();
        upstream_task.abort();
        download_task.abort();
    }
}
