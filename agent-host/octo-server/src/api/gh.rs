//! GitHub REST operations from the same pinned metadata used by ghapi.
//! Reads and writes share a method/path allowlist. Credentials and signed
//! redirects stay on the host; branch-changing APIs are not exposed.
use std::sync::{Arc, OnceLock};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::AppState;

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
}

fn operations() -> &'static [Operation] {
    static OPS: OnceLock<Vec<Operation>> = OnceLock::new();
    OPS.get_or_init(|| {
        let mut ops = serde_json::from_str::<Spec>(include_str!(
            "../../../rho-notebook/src/ghapi/gh_spec.json"
        ))
        .expect("bundled ghapi metadata is valid")
        .ops;
        // Attachment bytes go to GitHub's upload origin, not its REST API.
        // Keep this fixed operation beside the shared method/path allowlist.
        ops.push(Operation {
            group: "attachments".into(),
            name: "upload".into(),
            path: "/user-attachments/assets".into(),
            verb: "POST".into(),
        });
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
    let token = match state.get_token().await {
        Ok(token) => token,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut url = if op.group == "attachments" {
        state.github_upload_url()
    } else {
        state.github_api_url.clone()
    };
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
    relay(state, upstream, &token, op).await
}

// Repo metadata and nested search/PR responses can carry ephemeral credentials.
// Remove capabilities without restricting the rest of the response schema.
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
        if let Some(value) = upstream.headers().get(name)
            && value.to_str().is_ok_and(|text| !text.contains(token))
        {
            headers.insert(name, value.clone());
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
        // A known GitHub download operation may return a signed URL. Fetch
        // once, without credentials, and never expose Location or
        // follow another redirect.
        let downloaded = match state.client.get(url).send().await {
            Ok(response) if response.status().is_success() => response,
            _ => return StatusCode::BAD_GATEWAY.into_response(),
        };
        let content_type = if op.name == "download_job_logs_for_workflow_run" {
            "text/plain; charset=utf-8"
        } else {
            "application/zip"
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
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    if let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
        redact(&mut value, token);
        (status, headers, Json(value)).into_response()
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
) -> Result<T, Box<Response>> {
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
        .map_err(|_| Box::new(StatusCode::BAD_GATEWAY.into_response()))?;
    let status = upstream.status();
    if status.is_redirection() {
        return Err(Box::new(StatusCode::BAD_GATEWAY.into_response()));
    }
    let bytes = upstream
        .bytes()
        .await
        .map_err(|_| Box::new(StatusCode::BAD_GATEWAY.into_response()))?;
    if !status.is_success() {
        let clean = String::from_utf8_lossy(&bytes).replace(token, "[redacted]");
        return Err(Box::new(github_error(status, clean.as_bytes())));
    }
    let result: GraphQlEnvelope<T> = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(StatusCode::BAD_GATEWAY.into_response()))?;
    if result.errors.is_some_and(|errors| !errors.is_empty()) {
        return Err(Box::new(StatusCode::BAD_GATEWAY.into_response()));
    }
    result
        .data
        .ok_or_else(|| Box::new(StatusCode::BAD_GATEWAY.into_response()))
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
        Err(error) => return *error,
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
        Err(error) => return *error,
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
    #[test]
    fn rest_allowlist_contains_only_the_selected_methods_and_paths() {
        let mut reads = 0;
        let mut writes = 0;
        for op in operations() {
            if op.group == "attachments"
                || matches!(op.name.as_str(), "review_decision" | "set_draft")
            {
                continue;
            }
            if op.verb == "GET" {
                reads += 1;
            } else {
                writes += 1;
            }
        }
        assert_eq!((reads, writes), (91, 40));
        let uploads: Vec<_> = operations()
            .iter()
            .filter(|op| op.group == "attachments")
            .collect();
        assert_eq!(uploads.len(), 1);
        assert_eq!(
            (uploads[0].verb.as_str(), uploads[0].path.as_str()),
            ("POST", "/user-attachments/assets")
        );
        for name in ["merge", "merge_async", "update_branch", "dismiss_review"] {
            assert!(
                !operations()
                    .iter()
                    .any(|op| op.group == "pulls" && op.name == name)
            );
        }
        for (group, name) in [
            ("repos", "update"),
            ("git", "create_ref"),
            ("apps", "create_installation_access_token"),
        ] {
            assert!(
                !operations()
                    .iter()
                    .any(|op| op.group == group && op.name == name)
            );
        }
    }

    #[test]
    fn templates_preserve_nontrivial_values_and_reject_traversal() {
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
    }

    #[tokio::test]
    async fn pr_issue_and_search_requests_relay_payloads_and_fields() {
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
                    let status = if matches!(op.name.as_str(), "create" | "request_reviewers") {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    };
                    let value = json!({
                        "future_field": {"unexpected": [null, 73]},
                        "body": "echo host-token-test end",
                        "closed_at": "2026-01-02T03:04:05Z",
                        "total_count": 2,
                        "items": [{"new_result_field": true}]
                    });
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
                json!({"body":"only body","labels":["bug","UI"],"assignees":["alice","bob"],"milestone":41}),
                StatusCode::OK,
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
            (
                Method::PATCH,
                "repos/acme/widget/issues/7",
                json!({
                    "title":null,"assignees":[{"suggest":true,"confidence":"medium"}],
                    "labels":[{"name":"UI","color":null,"confidence":"high"}],
                    "type":{"value":null,"rationale":"clear suggested type"},
                    "state":"closed","state_reason":"not_planned",
                    "issue_field_values":[{"field_id":5,"value":["High","UI"],"suggest":true}]
                }),
                StatusCode::OK,
            ),
            (
                Method::PATCH,
                "repos/acme/widget/issues/7",
                json!({"issue_field_values":[{"field_id":5,"value":9007199254740993u64}]}),
                StatusCode::OK,
            ),
            (
                Method::PUT,
                "repos/acme/widget/pulls/7/reviews/23",
                json!({"body":"Updated pending review"}),
                StatusCode::OK,
            ),
            (
                Method::POST,
                "repos/acme/widget/pulls/7/reviews/23/events",
                json!({"event":"REQUEST_CHANGES","body":"Please fix"}),
                StatusCode::OK,
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
            assert_eq!(value["future_field"], json!({"unexpected": [null, 73]}));
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
    async fn reads_pass_queries_and_response_shapes_but_strip_credentials() {
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured = seen.clone();
        let (upstream, upstream_task) = serve(Router::new().fallback(any(
            move |uri: Uri, headers: HeaderMap, body: Bytes| {
                let captured = captured.clone();
                async move {
                    assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer host-token-test");
                    assert!(!headers.contains_key(header::COOKIE));
                    captured.lock().await.push((uri.to_string(), body.to_vec()));
                    let response = match uri.path() {
                        "/search/issues" => Json(json!({
                            "total_count":"upstream decides its types",
                            "unknown": {"nullable":null},
                            "items":[{"repository":{"full_name":"acme/widget","temp_clone_token":"ephemeral-secret","future_field":17}}],
                            "echo":"host-token-test"
                        })).into_response(),
                        "/repos/acme/widget/issues/0" =>
                            (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"message":"upstream rejects zero"}))).into_response(),
                        "/repos/acme/widget/issues/not-a-number" =>
                            ([(header::CONTENT_TYPE,"application/json")], "{not valid json").into_response(),
                        _ => ([(header::CONTENT_TYPE,"application/octet-stream")], b"\x00\xfe\xffraw".to_vec()).into_response(),
                    };
                    let (mut parts, body) = response.into_parts();
                    parts.headers.insert(header::SET_COOKIE, "secret=cookie".parse().unwrap());
                    parts.headers.insert(header::ETAG, "\"v3\"".parse().unwrap());
                    Response::from_parts(parts, body)
                }
            }
        ))).await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        let path = "/search/issues?page=next&page=3&future_flag=yes";
        let response = client
            .get(format!("{base}{path}"))
            .header(header::AUTHORIZATION, "Bearer agent-token")
            .header(header::COOKIE, "agent=cookie")
            .body("read body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"v3\"");
        let value: Value = response.json().await.unwrap();
        assert_eq!(
            value,
            json!({
                "total_count":"upstream decides its types",
                "unknown":{"nullable":null},
                "items":[{"repository":{"full_name":"acme/widget","future_field":17}}],
                "echo":"[redacted]"
            })
        );
        assert_eq!(
            seen.lock().await[0],
            (path.to_owned(), b"read body".to_vec())
        );
        for (path, status, expected) in [
            (
                "/repos/acme/widget/issues/0",
                StatusCode::UNPROCESSABLE_ENTITY,
                b"{\"message\":\"upstream rejects zero\"}".as_slice(),
            ),
            (
                "/repos/acme/widget/issues/not-a-number",
                StatusCode::OK,
                b"{not valid json".as_slice(),
            ),
            (
                "/repos/acme/widget/pulls/7",
                StatusCode::OK,
                b"\x00\xfe\xffraw".as_slice(),
            ),
        ] {
            let response = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.bytes().await.unwrap().as_ref(), expected);
        }
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn writes_preserve_omission_nullable_nulls_and_falsy_values() {
        let calls = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured = calls.clone();
        let (upstream, upstream_task) = serve(Router::new().fallback(any(
            move |method: Method, uri: Uri, body: Bytes| {
                let captured = captured.clone();
                async move {
                    captured.lock().await.push((
                        method.clone(),
                        uri.path().to_owned(),
                        serde_json::from_slice::<Value>(&body).unwrap(),
                    ));
                    if uri.path().contains("/actions/") {
                        StatusCode::CREATED.into_response()
                    } else {
                        let status = if method == Method::POST {
                            StatusCode::CREATED
                        } else {
                            StatusCode::OK
                        };
                        (status, Json(json!({"number":7}))).into_response()
                    }
                }
            },
        )))
        .await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        for (method, path, payload, expected, status) in [
            (
                Method::POST,
                "pulls",
                json!({"head":"topic","base":"main","issue":7,"draft":false,"maintainer_can_modify":false}),
                json!({"head":"topic","base":"main","issue":7,"draft":false,"maintainer_can_modify":false}),
                StatusCode::CREATED,
            ),
            (
                Method::PATCH,
                "pulls/7",
                json!({"body":"","base":"release/next","maintainer_can_modify":false}),
                json!({"body":"","base":"release/next","maintainer_can_modify":false}),
                StatusCode::OK,
            ),
            (
                Method::POST,
                "actions/jobs/19/rerun",
                json!({"enable_debug_logging":false,"enable_debugger":true}),
                json!({"enable_debug_logging":false,"enable_debugger":true}),
                StatusCode::CREATED,
            ),
            (
                Method::POST,
                "actions/runs/23/rerun",
                json!({}),
                json!({}),
                StatusCode::CREATED,
            ),
            (
                Method::POST,
                "actions/runs/23/rerun-failed-jobs",
                json!({"enable_debug_logging":false}),
                json!({"enable_debug_logging":false}),
                StatusCode::CREATED,
            ),
            (
                Method::PATCH,
                "issues/7",
                json!({"milestone":null,"body":null,"type":null,"labels":[]}),
                json!({"milestone":null,"body":null,"type":null,"labels":[]}),
                StatusCode::OK,
            ),
        ] {
            let response = client
                .request(method.clone(), format!("{base}/repos/acme/widget/{path}"))
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{method} {path}");
            let seen = calls.lock().await;
            let last = seen.last().unwrap();
            assert_eq!(last.0, method);
            assert_eq!(last.1, format!("/repos/acme/widget/{path}"));
            assert_eq!(last.2, expected, "{path}");
        }
        assert_eq!(calls.lock().await.len(), 6);
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn writes_relay_raw_bodies_queries_and_upstream_validation_errors() {
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
                    assert_eq!(
                        headers.get(header::CONTENT_TYPE).unwrap(),
                        "application/json"
                    );
                    seen.lock()
                        .await
                        .push((method, uri.to_string(), body.to_vec()));
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({
                            "message":"Validation Failed",
                            "errors":[{"field":"body","code":"invalid"}]
                        })),
                    )
                }
            },
        )))
        .await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        for (method, path, body) in [
            (
                Method::PATCH,
                "/repos/acme/widget/pulls/not-a-number?future_flag=yes&future_flag=no",
                b"{ \"body\": null, \"future_field\": 9007199254740993, \"draft\": false }"
                    .as_slice(),
            ),
            (
                Method::POST,
                "/repos/acme/widget/issues?future_flag=yes",
                b"not JSON".as_slice(),
            ),
            (
                Method::POST,
                "/repos/acme/widget/actions/runs/0/rerun",
                b"{\"enable_debug_logging\":null}".as_slice(),
            ),
        ] {
            let response = client
                .request(method.clone(), format!("{base}{path}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, "Bearer agent-token")
                .header(header::COOKIE, "agent=cookie")
                .header("x-forwarded-host", "evil.example")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({
                    "message":"Validation Failed",
                    "errors":[{"field":"body","code":"invalid"}]
                })
            );
            assert_eq!(
                captured.lock().await.last().unwrap(),
                &(method, path.to_owned(), body.to_vec())
            );
        }
        assert_eq!(captured.lock().await.len(), 3);
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn expanded_operations_relay_to_github_without_new_handlers() {
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
                    seen.lock()
                        .await
                        .push((method, uri.to_string(), body.to_vec()));
                    (StatusCode::ACCEPTED, "upstream result")
                }
            },
        )))
        .await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        // Concrete fixtures selected independently from pinned upstream
        // metadata.
        for (method, path) in [
            (Method::GET, "/repos/acme/widget/actions/artifacts/7"),
            (Method::GET, "/repos/acme/widget/actions/workflows/7"),
            (Method::GET, "/repos/acme/widget/actions/runs/7/attempts/7"),
            (Method::GET, "/repos/acme/widget/actions/artifacts"),
            (
                Method::GET,
                "/repos/acme/widget/actions/runs/7/attempts/7/jobs",
            ),
            (Method::GET, "/repos/acme/widget/actions/workflows"),
            (Method::GET, "/repos/acme/widget/actions/runs/7/artifacts"),
            (Method::GET, "/repos/acme/widget/actions/workflows/7/runs"),
            (Method::GET, "/repos/acme/widget/git/blobs/7"),
            (Method::GET, "/repos/acme/widget/git/commits/7"),
            (Method::GET, "/repos/acme/widget/git/ref/heads%2Ftopic"),
            (Method::GET, "/repos/acme/widget/git/tags/7"),
            (Method::GET, "/repos/acme/widget/git/trees/7"),
            (
                Method::GET,
                "/repos/acme/widget/git/matching-refs/heads%2Ftopic",
            ),
            (Method::POST, "/repos/acme/widget/issues/7/assignees"),
            (
                Method::POST,
                "/repos/acme/widget/issues/7/dependencies/blocked_by",
            ),
            (
                Method::POST,
                "/repos/acme/widget/issues/7/issue-field-values",
            ),
            (Method::POST, "/repos/acme/widget/issues/7/labels"),
            (Method::POST, "/repos/acme/widget/issues/7/sub_issues"),
            (Method::DELETE, "/repos/acme/widget/issues/comments/7"),
            (
                Method::DELETE,
                "/repos/acme/widget/issues/7/issue-field-values/7",
            ),
            (Method::DELETE, "/repos/acme/widget/issues/7/labels"),
            (Method::DELETE, "/repos/acme/widget/issues/7/assignees"),
            (
                Method::DELETE,
                "/repos/acme/widget/issues/7/dependencies/blocked_by/7",
            ),
            (Method::DELETE, "/repos/acme/widget/issues/7/labels/7"),
            (Method::DELETE, "/repos/acme/widget/issues/7/sub_issue"),
            (
                Method::PATCH,
                "/repos/acme/widget/issues/7/sub_issues/priority",
            ),
            (
                Method::PUT,
                "/repos/acme/widget/issues/7/issue-field-values",
            ),
            (Method::PUT, "/repos/acme/widget/issues/7/labels"),
            (Method::POST, "/repos/acme/widget/issues/7/reactions"),
            (
                Method::POST,
                "/repos/acme/widget/issues/comments/7/reactions",
            ),
            (
                Method::POST,
                "/repos/acme/widget/pulls/comments/7/reactions",
            ),
            (Method::DELETE, "/repos/acme/widget/issues/7/reactions/7"),
            (
                Method::DELETE,
                "/repos/acme/widget/issues/comments/7/reactions/7",
            ),
            (
                Method::DELETE,
                "/repos/acme/widget/pulls/comments/7/reactions/7",
            ),
            (Method::GET, "/repos/acme/widget/issues/7/reactions"),
            (
                Method::GET,
                "/repos/acme/widget/issues/comments/7/reactions",
            ),
            (Method::GET, "/repos/acme/widget/pulls/comments/7/reactions"),
            (Method::GET, "/repos/acme/widget/compare/main...topic"),
            (Method::GET, "/repos/acme/widget"),
            (Method::GET, "/repos/acme/widget/branches/release%2Fnext"),
            (Method::GET, "/repos/acme/widget/commits/heads%2Ftopic"),
            (Method::GET, "/repos/acme/widget/contents/src%2Fmain.rs"),
            (Method::GET, "/repos/acme/widget/releases/latest"),
            (Method::GET, "/repos/acme/widget/readme"),
            (Method::GET, "/repos/acme/widget/releases/7"),
            (Method::GET, "/repos/acme/widget/releases/tags/v1.2"),
            (Method::GET, "/repos/acme/widget/branches"),
            (Method::GET, "/repos/acme/widget/commits"),
            (Method::GET, "/user/repos"),
            (Method::GET, "/orgs/team/repos"),
            (Method::GET, "/users/alice/repos"),
            (Method::GET, "/repos/acme/widget/releases/7/assets"),
            (Method::GET, "/repos/acme/widget/releases"),
            (Method::GET, "/repos/acme/widget/tags"),
        ] {
            let uri = format!("{path}?page=3&future_flag=one&future_flag=two");
            let body = b"{ \"future_field\": null, \"enabled\": false }";
            let response = client
                .request(method.clone(), format!("{base}{}", uri.replace("%2F", "/")))
                .body(body.as_slice())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED, "{method} {path}");
            assert_eq!(response.text().await.unwrap(), "upstream result");
            assert_eq!(
                captured.lock().await.last().unwrap(),
                &(method, uri, body.to_vec())
            );
        }
        assert_eq!(captured.lock().await.len(), 55);
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn attachments_relay_binary_uploads_without_credentials_or_redirects_to_agents() {
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let seen = captured.clone();
        let (upstream, upstream_task) = serve(Router::new().route(
            "/user-attachments/assets",
            axum::routing::post(move |uri: Uri, headers: HeaderMap, body: Bytes| {
                let seen = seen.clone();
                async move {
                    assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer host-token-test");
                    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/octet-stream");
                    assert!(!headers.contains_key(header::COOKIE));
                    assert!(!headers.contains_key("x-forwarded-host"));
                    assert_eq!(headers.get(header::USER_AGENT).unwrap(), "octo-gh");
                    seen.lock().await.push((uri.to_string(), body.to_vec()));
                    match uri.query().unwrap() {
                        "name=screen%20%26%20shot.png&content_type=image%2Fpng&repository_id=913" => (
                            StatusCode::CREATED,
                            Json(json!({"url":"https://github.com/user-attachments/assets/image",
                                        "message":"host-token-test",
                                        "repository":{"full_name":"acme/widget","temp_clone_token":"ephemeral"}}))
                        ).into_response(),
                        "name=clip.webm&content_type=video%2Fwebm&repository_id=913" => (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            Json(json!({"message":"Video exceeds plan limit","errors":[{"code":"too_large"}]}))
                        ).into_response(),
                        _ => (
                            StatusCode::TEMPORARY_REDIRECT,
                            [(header::LOCATION, "https://untrusted.invalid/host-token-test")],
                        ).into_response(),
                    }
                }
            }),
        )).await;
        let (base, task) = serve(crate::router(token_provider(), upstream.parse().unwrap())).await;
        let client = reqwest::Client::new();
        let body = b"\x89PNG\r\n\x1a\n\x00\xffbinary\x00";
        for (query, status, expected) in [
            (
                "name=screen%20%26%20shot.png&content_type=image%2Fpng&repository_id=913",
                StatusCode::CREATED,
                Some(
                    json!({"url":"https://github.com/user-attachments/assets/image",
                         "message":"[redacted]","repository":{"full_name":"acme/widget"}}),
                ),
            ),
            (
                "name=clip.webm&content_type=video%2Fwebm&repository_id=913",
                StatusCode::UNPROCESSABLE_ENTITY,
                Some(json!({"message":"Video exceeds plan limit","errors":[{"code":"too_large"}]})),
            ),
            (
                "name=redirect.png&content_type=image%2Fpng&repository_id=913",
                StatusCode::BAD_GATEWAY,
                None,
            ),
        ] {
            let path = format!("/user-attachments/assets?{query}");
            let response = client
                .post(format!("{base}{path}"))
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::AUTHORIZATION, "Bearer agent-token")
                .header(header::COOKIE, "session=agent")
                .header("x-forwarded-host", "untrusted.invalid")
                .body(body.as_slice())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert!(!response.headers().contains_key(header::LOCATION));
            if let Some(expected) = expected {
                assert_eq!(response.json::<Value>().await.unwrap(), expected);
            } else {
                assert!(response.bytes().await.unwrap().is_empty());
            }
            assert_eq!(
                captured.lock().await.last().unwrap(),
                &(path, body.to_vec())
            );
        }
        assert_eq!(captured.lock().await.len(), 3);
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn unavailable_methods_and_paths_never_obtain_credentials() {
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
            (Method::GET, "user-attachments/assets", Value::Null),
            (Method::DELETE, "user-attachments/assets", Value::Null),
            (Method::POST, "user-attachments/assets/7", json!({})),
            (Method::POST, "user-attachments/assets/../other", json!({})),
            (Method::POST, "user-attachments/%2e%2e/assets", json!({})),
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
                Method::GET,
                "repos/acme/widget/pulls/7/update-branch",
                Value::Null,
            ),
            (Method::GET, "repos/acme/widget/git/refs", Value::Null),
            (Method::HEAD, "repos/acme/widget/issues/7", Value::Null),
            (
                Method::PUT,
                "repos/acme/widget/issues/comments/7/pin",
                Value::Null,
            ),
            (
                Method::DELETE,
                "repos/acme/widget/issues/comments/7/pin",
                Value::Null,
            ),
            (
                Method::PUT,
                "repos/acme/widget/issues/7/lock",
                json!({"lock_reason":"spam"}),
            ),
            (
                Method::DELETE,
                "repos/acme/widget/issues/7/lock",
                Value::Null,
            ),
            (
                Method::POST,
                "repos/acme/widget/issues/7/suggestions/7/approve",
                Value::Null,
            ),
            (
                Method::POST,
                "repos/acme/widget/issues/7/suggestions/7/dismiss",
                Value::Null,
            ),
            (
                Method::POST,
                "repos/acme/widget/labels",
                json!({"name":"bug","color":"abcdef"}),
            ),
            (
                Method::PATCH,
                "repos/acme/widget/labels/7",
                json!({"new_name":"defect"}),
            ),
            (Method::DELETE, "repos/acme/widget/labels/7", Value::Null),
            (
                Method::POST,
                "repos/acme/widget/milestones",
                json!({"title":"Next"}),
            ),
            (
                Method::PATCH,
                "repos/acme/widget/milestones/7",
                json!({"title":"Next"}),
            ),
            (
                Method::DELETE,
                "repos/acme/widget/milestones/7",
                Value::Null,
            ),
            (
                Method::PUT,
                "repos/acme/widget/pulls/7/draft?extra=true",
                json!({"draft":false}),
            ),
            (
                Method::PUT,
                "repos/acme/widget/pulls/7/draft",
                json!({"draft":"false"}),
            ),
            (Method::GET, "repos/acme/widget/hooks/7/config", Value::Null),
            (
                Method::GET,
                "repos/acme/widget/actions/secrets",
                Value::Null,
            ),
            (Method::GET, "repos/acme/widget/keys/7", Value::Null),
            (
                Method::POST,
                "repos/acme/widget/actions/runners/registration-token",
                json!({}),
            ),
            (
                Method::PUT,
                "repos/acme/widget/contents/file.txt",
                json!({"message":"write","content":"YQ=="}),
            ),
            (
                Method::POST,
                "repos/acme/widget/actions/workflows/7/dispatches",
                json!({"ref":"main"}),
            ),
            (
                Method::POST,
                "repos/acme/widget/deployments",
                json!({"ref":"main"}),
            ),
            (Method::PATCH, "repos/acme/widget", json!({"private":false})),
            (Method::DELETE, "repos/acme/widget", Value::Null),
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
    async fn text_empty_errors_and_untyped_responses_keep_their_semantics() {
        let (upstream,upstream_task)=serve(Router::new().fallback(any(
            |method:Method,uri:Uri,headers:HeaderMap| async move {
                assert_eq!(headers.get(header::AUTHORIZATION).unwrap(),"Bearer host-token-test");
                match (method.as_str(),uri.path()) {
                    ("GET","/repos/acme/widget/pulls/7")=>{
                        assert_eq!(headers.get(header::ACCEPT).unwrap(),"application/vnd.github.diff");
                        ([(header::CONTENT_TYPE,"text/plain")],"diff --git a/foo b/foo\n+fixed").into_response()
                    }
                    ("DELETE","/repos/acme/widget/pulls/comments/7")=>StatusCode::NO_CONTENT.into_response(),
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
                "repos/acme/widget/pulls/comments/7",
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
            .get(format!("{base}/repos/acme/widget/issues/999"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"number":"not an integer"})
        );
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
            let expected_type = if path.contains("/jobs/") {
                "text/plain; charset=utf-8"
            } else {
                "application/zip"
            };
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                expected_type
            );
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
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            assert!(!response.headers().contains_key(header::LOCATION));
        }
        assert_eq!(downloads.load(Ordering::SeqCst), 4);
        task.abort();
        upstream_task.abort();
        download_task.abort();
    }
}
