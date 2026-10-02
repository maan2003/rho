//! Selected ghapi REST operations. Agents never send a GitHub token.
use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/repos/{owner}/{repo}/pulls",
            get(list_pulls).post(create_pull),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}",
            get(get_pull).patch(update_pull),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/review-decision",
            get(get_review_decision),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/files",
            get(list_files),
        )
        .route("/repos/{owner}/{repo}/check-runs/{id}", get(get_check))
        .route(
            "/repos/{owner}/{repo}/check-runs/{id}/annotations",
            get(list_annotations),
        )
        .route("/repos/{owner}/{repo}/actions/runs", get(list_runs))
        .route("/repos/{owner}/{repo}/actions/runs/{id}", get(get_run))
        .route(
            "/repos/{owner}/{repo}/actions/runs/{id}/jobs",
            get(list_jobs),
        )
        .route("/repos/{owner}/{repo}/actions/jobs/{id}", get(get_job))
        .route(
            "/repos/{owner}/{repo}/actions/jobs/{id}/logs",
            get(job_logs),
        )
        .route(
            "/repos/{owner}/{repo}/actions/runs/{id}/logs",
            get(run_logs),
        )
        .route(
            "/repos/{owner}/{repo}/actions/jobs/{id}/rerun",
            axum::routing::post(rerun_job),
        )
        .route(
            "/repos/{owner}/{repo}/actions/runs/{id}/rerun",
            axum::routing::post(rerun_run),
        )
        .route(
            "/repos/{owner}/{repo}/actions/runs/{id}/rerun-failed-jobs",
            axum::routing::post(rerun_failed),
        )
        .route("/repos/{owner}/{repo}/issues", get(list_issues))
        .route("/repos/{owner}/{repo}/issues/{number}", get(get_issue))
        .route(
            "/repos/{owner}/{repo}/issues/{number}/comments",
            get(list_issue_comments).post(create_issue_comment),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/reviews",
            get(list_reviews),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/comments",
            get(list_review_comments),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/comments/{comment_id}/replies",
            axum::routing::post(reply_to_review_comment),
        )
        .route(
            "/repos/{owner}/{repo}/commits/{*reference}",
            get(get_commit),
        )
        .layer(DefaultBodyLimit::disable())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuesQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    milestone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee: Option<String>,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mentioned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue_field_values: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    check_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_id: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude_pull_requests: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude_pull_requests: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    check_suite_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_sha: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RerunOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_debug_logging: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobRerunOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_debug_logging: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_debugger: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueCommentsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewCommentsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditPull {
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maintainer_can_modify: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentBody {
    body: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePullRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    head: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_repo: Option<String>,
    base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maintainer_can_modify: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct User {
    login: String,
    id: Option<u64>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Branch {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

#[derive(Serialize, Deserialize)]
struct Pull {
    number: u64,
    html_url: String,
    title: String,
    body: Option<String>,
    state: String,
    draft: bool,
    head: Branch,
    base: Branch,
    user: Option<User>,
    created_at: String,
    updated_at: String,
    merged_at: Option<String>,
    merged: Option<bool>,
    mergeable: Option<bool>,
    mergeable_state: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Label {
    name: String,
}

#[derive(Serialize, Deserialize)]
struct IssuePull {
    html_url: String,
}

#[derive(Serialize, Deserialize)]
struct Issue {
    number: u64,
    html_url: String,
    title: String,
    body: Option<String>,
    state: String,
    user: Option<User>,
    labels: Vec<Label>,
    pull_request: Option<IssuePull>,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize, Deserialize)]
struct IssueComment {
    id: u64,
    html_url: String,
    body: Option<String>,
    user: Option<User>,
    author_association: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize, Deserialize)]
struct Review {
    id: u64,
    html_url: String,
    body: Option<String>,
    state: String,
    user: Option<User>,
    author_association: Option<String>,
    submitted_at: Option<String>,
    commit_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ReviewComment {
    id: u64,
    html_url: String,
    body: String,
    user: Option<User>,
    author_association: Option<String>,
    path: String,
    diff_hunk: String,
    line: Option<u64>,
    original_line: Option<u64>,
    side: Option<String>,
    commit_id: String,
    in_reply_to_id: Option<u64>,
    pull_request_review_id: Option<u64>,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize, Deserialize)]
struct CommitStatus {
    context: String,
    state: String,
    description: Option<String>,
    target_url: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CombinedStatus {
    state: String,
    statuses: Vec<CommitStatus>,
}

#[derive(Serialize, Deserialize)]
struct CheckRun {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    html_url: Option<String>,
    details_url: Option<String>,
    output: Option<CheckOutput>,
}

#[derive(Serialize, Deserialize)]
struct CheckOutput {
    title: Option<String>,
    summary: Option<String>,
    text: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CheckRuns {
    total_count: u64,
    check_runs: Vec<CheckRun>,
}

#[derive(Serialize, Deserialize)]
struct PullFile {
    filename: String,
    status: String,
    additions: u64,
    deletions: u64,
    changes: u64,
    patch: Option<String>,
    previous_filename: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Annotation {
    path: String,
    start_line: u64,
    end_line: u64,
    annotation_level: String,
    message: String,
    title: Option<String>,
    raw_details: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct WorkflowRun {
    id: u64,
    name: Option<String>,
    head_sha: String,
    head_branch: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    html_url: String,
    run_number: u64,
    run_attempt: u64,
    workflow_id: u64,
    event: String,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize, Deserialize)]
struct WorkflowRuns {
    total_count: u64,
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Serialize, Deserialize)]
struct JobStep {
    name: String,
    status: String,
    conclusion: Option<String>,
    number: u64,
    started_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct WorkflowJob {
    id: u64,
    run_id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    steps: Option<Vec<JobStep>>,
}

#[derive(Serialize, Deserialize)]
struct WorkflowJobs {
    total_count: u64,
    jobs: Vec<WorkflowJob>,
}

#[derive(Deserialize)]
struct GitHubError {
    message: String,
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"message":"GitHub operation unavailable through Octo"})),
    )
        .into_response()
}

fn sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// Only route handlers select the path, query, and typed body.
async fn github_get<T: DeserializeOwned + Serialize, Q: Serialize>(
    state: Arc<AppState>,
    path: &[&str],
    query: Option<Q>,
) -> Response {
    github::<T, (), Q>(state, Method::GET, path, query, None).await
}

async fn github_post<T: DeserializeOwned + Serialize, B: Serialize>(
    state: Arc<AppState>,
    path: &[&str],
    body: B,
) -> Response {
    github::<T, B, ()>(state, Method::POST, path, None, Some(body)).await
}

async fn github_patch<T: DeserializeOwned + Serialize, B: Serialize>(
    state: Arc<AppState>,
    path: &[&str],
    body: B,
) -> Response {
    github::<T, B, ()>(state, Method::PATCH, path, None, Some(body)).await
}

async fn github_request<B: Serialize, Q: Serialize>(
    state: Arc<AppState>,
    method: Method,
    path: &[&str],
    query: Option<Q>,
    body: Option<B>,
) -> Result<reqwest::Response, Box<Response>> {
    // Url encodes each item as one segment, including slashes and percent
    // signs. Reject empty/dot components rather than letting them change
    // the route.
    if !path
        .iter()
        .all(|part| part.split('/').all(|part| !matches!(part, "" | "." | "..")))
    {
        return Err(Box::new(forbidden()));
    }
    let token = match state.get_token().await {
        Ok(token) => token,
        Err(_) => return Err(Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response())),
    };
    let mut url = state.github_api_url.clone();
    url.path_segments_mut()
        .expect("GitHub API base is hierarchical")
        .clear()
        .extend(path);
    let mut request = state
        .client
        .request(method, url)
        .bearer_auth(token)
        .header("User-Agent", "octo-gh")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Accept", "application/vnd.github+json");
    if let Some(query) = query {
        request = request.query(&query);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let upstream = match request.send().await {
        Ok(response) => response,
        Err(_) => return Err(Box::new(StatusCode::BAD_GATEWAY.into_response())),
    };
    Ok(upstream)
}

// Only the host's token and fields represented by T cross the boundary.
async fn github<T: DeserializeOwned + Serialize, B: Serialize, Q: Serialize>(
    state: Arc<AppState>,
    method: Method,
    path: &[&str],
    query: Option<Q>,
    body: Option<B>,
) -> Response {
    let upstream = match github_request(state, method, path, query, body).await {
        Ok(response) => response,
        Err(error) => return *error,
    };
    // Following redirects could disclose the host's token to another origin.
    if upstream.status().is_redirection() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for name in [
        "link",
        "etag",
        "x-ratelimit-remaining",
        "x-ratelimit-limit",
        "x-ratelimit-reset",
    ] {
        if let Some(value) = upstream.headers().get(name) {
            headers.insert(name, value.clone());
        }
    }
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    if !status.is_success() {
        return github_error(status, &bytes);
    }
    match serde_json::from_slice::<T>(&bytes) {
        Ok(value) => (status, headers, Json(value)).into_response(),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

fn github_error(status: StatusCode, bytes: &[u8]) -> Response {
    let message = serde_json::from_slice::<GitHubError>(bytes)
        .map(|error| error.message)
        .unwrap_or_else(|_| "GitHub request failed".into());
    (status, Json(json!({"message":message}))).into_response()
}

fn positive(id: &str) -> bool {
    id.parse::<u64>().is_ok_and(|id| id > 0)
}

async fn github_empty_post<B: Serialize>(
    state: Arc<AppState>,
    path: &[&str],
    body: Option<B>,
) -> Response {
    let upstream = match github_request::<B, ()>(state, Method::POST, path, None, body).await {
        Ok(response) => response,
        Err(error) => return *error,
    };
    let status = upstream.status();
    if status != StatusCode::CREATED {
        if status.is_redirection() || status.is_success() {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        let bytes = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        };
        return github_error(status, &bytes);
    }
    StatusCode::CREATED.into_response()
}

// GitHub redirects to a short-lived download URL; never pass its URL or token
// to the agent.
async fn github_logs(state: Arc<AppState>, path: &[&str], content_type: &'static str) -> Response {
    let upstream =
        match github_request::<(), ()>(state.clone(), Method::GET, path, None, None).await {
            Ok(response) => response,
            Err(error) => return *error,
        };
    if upstream.status() != StatusCode::FOUND {
        let status = upstream.status();
        if status.is_redirection() || status.is_success() {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        let bytes = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        };
        return github_error(status, &bytes);
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
    // The production API uses HTTPS. Tests use an HTTP loopback API.
    if url.scheme() != state.github_api_url.scheme()
        || url.host_str().is_none()
        || !matches!(url.scheme(), "https" | "http")
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let downloaded = match state.client.get(url).send().await {
        Ok(response) => response,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    if !downloaded.status().is_success() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let bytes = match downloaded.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
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
        .bearer_auth(token)
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
        return github_error(status, &bytes);
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

async fn list_files(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if !positive(&number) {
        return forbidden();
    }
    github_get::<Vec<PullFile>, _>(
        state,
        &["repos", &owner, &repo, "pulls", &number, "files"],
        Some(query),
    )
    .await
}

async fn get_check(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    if uri.query().is_some() || !positive(&id) {
        return forbidden();
    }
    github_get::<CheckRun, ()>(state, &["repos", &owner, &repo, "check-runs", &id], None).await
}

async fn list_annotations(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if !positive(&id) {
        return forbidden();
    }
    github_get::<Vec<Annotation>, _>(
        state,
        &["repos", &owner, &repo, "check-runs", &id, "annotations"],
        Some(query),
    )
    .await
}

async fn list_runs(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    query: Result<Query<RunsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if query.head_sha.as_deref().is_some_and(|sha| !self::sha(sha)) {
        return forbidden();
    }
    github_get::<WorkflowRuns, _>(
        state,
        &["repos", &owner, &repo, "actions", "runs"],
        Some(query),
    )
    .await
}

async fn get_run(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    query: Result<Query<RunQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if !positive(&id) {
        return forbidden();
    }
    github_get::<WorkflowRun, _>(
        state,
        &["repos", &owner, &repo, "actions", "runs", &id],
        Some(query),
    )
    .await
}

async fn list_jobs(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    query: Result<Query<JobsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if !positive(&id) {
        return forbidden();
    }
    github_get::<WorkflowJobs, _>(
        state,
        &["repos", &owner, &repo, "actions", "runs", &id, "jobs"],
        Some(query),
    )
    .await
}

async fn get_job(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    if uri.query().is_some() || !positive(&id) {
        return forbidden();
    }
    github_get::<WorkflowJob, ()>(
        state,
        &["repos", &owner, &repo, "actions", "jobs", &id],
        None,
    )
    .await
}

async fn job_logs(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    if uri.query().is_some() || !positive(&id) {
        return forbidden();
    }
    github_logs(
        state,
        &["repos", &owner, &repo, "actions", "jobs", &id, "logs"],
        "text/plain; charset=utf-8",
    )
    .await
}

async fn run_logs(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    if uri.query().is_some() || !positive(&id) {
        return forbidden();
    }
    github_logs(
        state,
        &["repos", &owner, &repo, "actions", "runs", &id, "logs"],
        "application/zip",
    )
    .await
}

async fn rerun_job(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    uri: Uri,
    body: Result<Option<Json<JobRerunOptions>>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(body) = body else { return forbidden() };
    if uri.query().is_some() || !positive(&id) {
        return forbidden();
    }
    github_empty_post(
        state,
        &["repos", &owner, &repo, "actions", "jobs", &id, "rerun"],
        body.map(|Json(v)| v),
    )
    .await
}

async fn rerun_run(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    uri: Uri,
    body: Result<Option<Json<RerunOptions>>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(body) = body else { return forbidden() };
    if uri.query().is_some() || !positive(&id) {
        return forbidden();
    }
    github_empty_post(
        state,
        &["repos", &owner, &repo, "actions", "runs", &id, "rerun"],
        body.map(|Json(v)| v),
    )
    .await
}

async fn rerun_failed(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, id)): Path<(String, String, String)>,
    uri: Uri,
    body: Result<Option<Json<RerunOptions>>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(body) = body else { return forbidden() };
    if uri.query().is_some() || !positive(&id) {
        return forbidden();
    }
    github_empty_post(
        state,
        &[
            "repos",
            &owner,
            &repo,
            "actions",
            "runs",
            &id,
            "rerun-failed-jobs",
        ],
        body.map(|Json(v)| v),
    )
    .await
}

async fn list_pulls(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    query: Result<Query<PullsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    github_get::<Vec<Pull>, _>(state, &["repos", &owner, &repo, "pulls"], Some(query)).await
}

async fn get_pull(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    if uri.query().is_some() || !number.parse::<u64>().is_ok_and(|number| number > 0) {
        return forbidden();
    }
    github_get::<Pull, ()>(state, &["repos", &owner, &repo, "pulls", &number], None).await
}

async fn create_pull(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    uri: Uri,
    body: Result<Json<CreatePullRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(pr)) = body else {
        return forbidden();
    };
    if uri.query().is_some() || (pr.title.is_none() && pr.issue.is_none()) {
        return forbidden();
    }
    github_post::<Pull, _>(state, &["repos", &owner, &repo, "pulls"], pr).await
}

async fn update_pull(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    uri: Uri,
    body: Result<Json<EditPull>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(edit)) = body else {
        return forbidden();
    };
    if uri.query().is_some() || !number.parse::<u64>().is_ok_and(|number| number > 0) {
        return forbidden();
    }
    github_patch::<Pull, _>(state, &["repos", &owner, &repo, "pulls", &number], edit).await
}

async fn list_issues(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    query: Result<Query<IssuesQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    github_get::<Vec<Issue>, _>(state, &["repos", &owner, &repo, "issues"], Some(query)).await
}

async fn get_issue(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    if uri.query().is_some() || !number.parse::<u64>().is_ok_and(|number| number > 0) {
        return forbidden();
    }
    github_get::<Issue, ()>(state, &["repos", &owner, &repo, "issues", &number], None).await
}

async fn list_issue_comments(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    query: Result<Query<IssueCommentsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if !number.parse::<u64>().is_ok_and(|number| number > 0) {
        return forbidden();
    }
    github_get::<Vec<IssueComment>, _>(
        state,
        &["repos", &owner, &repo, "issues", &number, "comments"],
        Some(query),
    )
    .await
}

async fn create_issue_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    uri: Uri,
    body: Result<Json<CommentBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(comment)) = body else {
        return forbidden();
    };
    if uri.query().is_some() || !number.parse::<u64>().is_ok_and(|number| number > 0) {
        return forbidden();
    }
    github_post::<IssueComment, _>(
        state,
        &["repos", &owner, &repo, "issues", &number, "comments"],
        comment,
    )
    .await
}

async fn list_reviews(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if !number.parse::<u64>().is_ok_and(|number| number > 0) {
        return forbidden();
    }
    github_get::<Vec<Review>, _>(
        state,
        &["repos", &owner, &repo, "pulls", &number, "reviews"],
        Some(query),
    )
    .await
}

async fn list_review_comments(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, String)>,
    query: Result<Query<ReviewCommentsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if !number.parse::<u64>().is_ok_and(|number| number > 0) {
        return forbidden();
    }
    github_get::<Vec<ReviewComment>, _>(
        state,
        &["repos", &owner, &repo, "pulls", &number, "comments"],
        Some(query),
    )
    .await
}

async fn reply_to_review_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number, comment_id)): Path<(String, String, String, String)>,
    uri: Uri,
    body: Result<Json<CommentBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(reply)) = body else {
        return forbidden();
    };
    if uri.query().is_some()
        || !number.parse::<u64>().is_ok_and(|number| number > 0)
        || !comment_id.parse::<u64>().is_ok_and(|id| id > 0)
    {
        return forbidden();
    }
    github_post::<ReviewComment, _>(
        state,
        &[
            "repos",
            &owner,
            &repo,
            "pulls",
            &number,
            "comments",
            &comment_id,
            "replies",
        ],
        reply,
    )
    .await
}

async fn get_commit(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, tail)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    // The client preserves slashes in refs, so capture the ref and endpoint
    // together.
    let Some((reference, endpoint)) = tail.rsplit_once('/') else {
        return forbidden();
    };
    let path = ["repos", &owner, &repo, "commits", reference, endpoint];
    match endpoint {
        "status" => {
            let Ok(Query(query)) = Query::<PageQuery>::try_from_uri(&uri) else {
                return forbidden();
            };
            github_get::<CombinedStatus, _>(state, &path, Some(query)).await
        }
        "check-runs" => {
            let Ok(Query(query)) = Query::<CheckQuery>::try_from_uri(&uri) else {
                return forbidden();
            };
            github_get::<CheckRuns, _>(state, &path, Some(query)).await
        }
        _ => forbidden(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::any;
    use serde_json::Value;

    use super::*;

    fn pull() -> serde_json::Value {
        json!({
            "number":42, "html_url":"https://github.com/acme/widgets/pull/42",
            "title":"Fix", "body":null, "state":"open", "draft":true,
            "head":{"ref":"rho/fix","sha":"a".repeat(40)},
            "base":{"ref":"main","sha":"b".repeat(40)},
            "user":{"login":"alice"}, "created_at":"2025-01-01T00:00:00Z",
            "updated_at":"2025-01-02T00:00:00Z", "merged_at":null,
            "ignored":"not relayed"
        })
    }

    fn issue() -> serde_json::Value {
        json!({
            "number":12, "html_url":"https://github.com/acme/widgets/issues/12",
            "title":"Bug", "body":"details", "state":"open",
            "user":{"login":"alice"}, "labels":[{"name":"bug"}],
            "created_at":"2025-01-01T00:00:00Z", "updated_at":"2025-01-02T00:00:00Z",
            "ignored":"not relayed"
        })
    }

    #[tokio::test]
    async fn base_edit_and_review_decision_project_only_selected_fields() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let upstream = axum::Router::new().fallback(any(move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                assert_eq!(headers.get("authorization").unwrap(), "Bearer host-secret");
                let input: Value = serde_json::from_str(&body).unwrap();
                match (method, uri.path()) {
                    (Method::PATCH, "/repos/acme/widgets/pulls/42") => {
                        if input["state"] == "closed" {
                            assert_eq!(input, json!({"base":"release/next","state":"closed","maintainer_can_modify":false}));
                        } else {
                            assert_eq!(input, json!({"state":"open","maintainer_can_modify":true}));
                        }
                        let mut result = pull();
                        result["base"]["ref"] = json!("release/next");
                        result["merged"] = json!(false);
                        (StatusCode::OK, Json(result)).into_response()
                    }
                    (Method::POST, "/graphql") => {
                        if input["variables"]["number"] == 45 {
                            assert_eq!(input["variables"], json!({"owner":"acme\"","repo":"widgets/odd","number":45}));
                        } else {
                            assert_eq!(input["variables"]["owner"], "acme");
                            assert_eq!(input["variables"]["repo"], "widgets");
                        }
                        assert_eq!(input["query"], "query($owner: String!, $repo: String!, $number: Int!) { repository(owner: $owner, name: $repo) { pullRequest(number: $number) { reviewDecision } } }");
                        let result = match input["variables"]["number"].as_u64().unwrap() {
                            42 | 45 => json!({"data":{"repository":{"pullRequest":{
                                "reviewDecision":"REVIEW_REQUIRED","ignored":"hidden"}}},"ignored":"hidden"}),
                            43 => json!({"data":{"repository":{"pullRequest":{"reviewDecision":null}}}}),
                            44 => json!({"errors":[{"message":"Permission denied"}]}),
                            _ => panic!("unexpected number"),
                        };
                        (StatusCode::OK, Json(result)).into_response()
                    }
                    _ => panic!("unexpected request {uri}"),
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), upstream_url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!(
            "http://{}/repos/acme/widgets/pulls/42",
            listener.local_addr().unwrap()
        );
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        assert_eq!(
            client
                .patch(&base)
                .json(&json!({"base":"release/next","unexpected":true}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let edit = client
            .patch(&base)
            .json(&json!({"base":"release/next","state":"closed","maintainer_can_modify":false}))
            .send()
            .await
            .unwrap();
        assert_eq!(edit.status(), StatusCode::OK);
        let edit: Value = edit.json().await.unwrap();
        assert_eq!(edit["base"]["ref"], "release/next");
        assert_eq!(edit["merged"], false);
        assert!(edit.get("ignored").is_none());
        assert_eq!(
            client
                .patch(&base)
                .json(&json!({"state":"open","maintainer_can_modify":true}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let decision = client
            .get(format!("{base}/review-decision"))
            .send()
            .await
            .unwrap();
        assert_eq!(decision.status(), StatusCode::OK);
        assert_eq!(
            decision.json::<Value>().await.unwrap(),
            json!({"review_decision":"REVIEW_REQUIRED"})
        );
        let unusual_names = client
            .get(format!(
                "{}/repos/acme%22/widgets%2Fodd/pulls/45/review-decision",
                base.split("/repos/").next().unwrap()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(unusual_names.status(), StatusCode::OK);
        assert_eq!(
            unusual_names.json::<Value>().await.unwrap(),
            json!({"review_decision":"REVIEW_REQUIRED"})
        );
        assert_eq!(
            client
                .get(format!("{base}/review-decision?query=x"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn inspection_and_reruns_use_selected_routes_and_projected_responses() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let upstream = axum::Router::new().fallback(any(move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                assert_eq!(headers.get("authorization").unwrap(), "Bearer host-secret");
                assert!(!body.contains("unapproved"));
                let path = uri.path();
                let base = "/repos/acme/widgets/";
                assert!(path.starts_with(base));
                let suffix = &path[base.len()..];
                let (status, response) = match (method, suffix) {
                    (Method::GET, "pulls/42/files") => (StatusCode::OK, json!([{
                        "filename":"src/a.rs","status":"modified","additions":3,"deletions":1,"changes":4,
                        "patch":"@@ -1 +1 @@","ignored":"private"
                    }])),
                    (Method::GET, "check-runs/7") => (StatusCode::OK, json!({
                        "id":7,"name":"build","status":"completed","conclusion":"failure",
                        "output":{"title":"Failed","summary":"compiler error","text":"details","ignored":"private"},
                        "ignored":"private"
                    })),
                    (Method::GET, "check-runs/7/annotations") => (StatusCode::OK, json!([{
                        "path":"src/a.rs","start_line":8,"end_line":8,"annotation_level":"failure",
                        "message":"wrong type","ignored":"private"
                    }])),
                    (Method::GET, "actions/runs") => {
                        assert_eq!(uri.query(), Some("actor=alice&branch=release%2Fnext&event=pull_request&status=failure&page=2&per_page=11&created=2026-09-01..2026-09-29&exclude_pull_requests=false&check_suite_id=17&head_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
                        (StatusCode::OK, json!({
                        "total_count":1,"workflow_runs":[{
                            "id":11,"name":"CI","head_sha":"a".repeat(40),"status":"completed",
                            "conclusion":"failure","html_url":"https://github.com/acme/widgets/actions/runs/11",
                            "run_number":2,"run_attempt":1,"workflow_id":3,"event":"push",
                            "created_at":"2025-01-01","updated_at":"2025-01-02","ignored":"private"
                        }]
                    }))},
                    (Method::GET, "actions/runs/11") => {
                        assert_eq!(uri.query(), Some("exclude_pull_requests=true"));
                        (StatusCode::OK, json!({
                        "id":11,"name":"CI","head_sha":"a".repeat(40),"status":"completed",
                        "conclusion":"failure","html_url":"https://github.com/acme/widgets/actions/runs/11",
                        "run_number":2,"run_attempt":1,"workflow_id":3,"event":"push",
                        "created_at":"2025-01-01","updated_at":"2025-01-02"
                    }))},
                    (Method::GET, "actions/runs/11/jobs") => (StatusCode::OK, json!({
                        "total_count":1,"jobs":[{"id":12,"run_id":11,"name":"test",
                        "status":"completed","conclusion":"failure","steps":[{
                        "name":"cargo test","status":"completed","conclusion":"failure","number":2
                        }],"ignored":"private"}]
                    })),
                    (Method::GET, "actions/jobs/12") => (StatusCode::OK, json!({
                        "id":12,"run_id":11,"name":"test","status":"completed","conclusion":"failure"
                    })),
                    (Method::POST, "actions/runs/11/rerun" | "actions/runs/11/rerun-failed-jobs" | "actions/jobs/12/rerun") => {
                        if !body.is_empty() {
                            assert_eq!(serde_json::from_str::<Value>(&body).unwrap(),
                                if suffix == "actions/jobs/12/rerun" {
                                    json!({"enable_debug_logging":true,"enable_debugger":true})
                                } else { json!({"enable_debug_logging":true}) });
                        }
                        (StatusCode::CREATED, json!({}))
                    }
                    _ => panic!("unexpected {suffix}"),
                };
                (status, Json(response))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), upstream_url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!(
            "http://{}/repos/acme/widgets",
            listener.local_addr().unwrap()
        );
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        for (suffix, check) in [
            ("pulls/42/files?page=2", "filename"),
            ("check-runs/7", "output"),
            ("check-runs/7/annotations?per_page=3", "message"),
            (
                "actions/runs?actor=alice&branch=release%2Fnext&event=pull_request&status=failure&page=2&per_page=11&created=2026-09-01..2026-09-29&exclude_pull_requests=false&check_suite_id=17&head_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "workflow_runs",
            ),
            ("actions/runs/11?exclude_pull_requests=true", "head_sha"),
            ("actions/runs/11/jobs?filter=latest&page=2", "jobs"),
            ("actions/jobs/12", "name"),
        ] {
            let response = client.get(format!("{base}/{suffix}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{suffix}");
            let response: Value = response.json().await.unwrap();
            assert!(
                response.get(check).is_some() || response[0].get(check).is_some(),
                "{suffix}"
            );
            assert!(response.get("ignored").is_none(), "{suffix}");
        }
        for suffix in [
            "actions/runs/11/rerun",
            "actions/runs/11/rerun-failed-jobs",
            "actions/jobs/12/rerun",
        ] {
            let body = if suffix.contains("jobs/12/") {
                json!({"enable_debug_logging":true,"enable_debugger":true})
            } else {
                json!({"enable_debug_logging":true})
            };
            assert_eq!(
                client
                    .post(format!("{base}/{suffix}"))
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CREATED
            );
            assert_eq!(
                client
                    .post(format!("{base}/{suffix}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CREATED
            );
            assert_eq!(
                client
                    .post(format!("{base}/{suffix}?unexpected=1"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                client
                    .post(format!("{base}/{suffix}"))
                    .json(&json!({"unapproved":true}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        for suffix in [
            "actions/runs/0",
            "actions/jobs/0",
            "check-runs/0",
            "pulls/0/files",
            "actions/runs?head_sha=main",
            "actions/runs?unexpected=anyone",
            "actions/runs?exclude_pull_requests=maybe",
            "actions/runs/11?exclude_pull_requests=maybe",
            "actions/runs/11?actor=alice",
        ] {
            assert_eq!(
                client
                    .get(format!("{base}/{suffix}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            client
                .delete(format!("{base}/actions/runs/11/logs"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(calls.load(Ordering::Relaxed), 13);
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn log_download_follows_signed_url_without_token_or_redirecting_again() {
        // Use an independent signed-download listener: the API's Location
        // cannot depend on its request URI.
        let download =
            axum::Router::new().fallback(any(|headers: HeaderMap, uri: Uri| async move {
                assert!(headers.get("authorization").is_none());
                match uri.path() {
                    "/job" => (StatusCode::OK, "job log".as_bytes().to_vec()),
                    "/run" => (StatusCode::OK, b"PK\x03\x04archive".to_vec()),
                    _ => (StatusCode::NOT_FOUND, vec![]),
                }
            }));
        let download_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let download_base = format!("http://{}", download_listener.local_addr().unwrap());
        let download_task =
            tokio::spawn(async move { axum::serve(download_listener, download).await.unwrap() });
        let upstream = axum::Router::new().fallback(any(move |uri: Uri, headers: HeaderMap| {
            let download_base = download_base.clone();
            async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer host-secret");
                let target = if uri.path().ends_with("jobs/12/logs") {
                    "job"
                } else {
                    "run"
                };
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, format!("{download_base}/{target}"))],
                    "",
                )
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), upstream_url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!(
            "http://{}/repos/acme/widgets/actions",
            listener.local_addr().unwrap()
        );
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        for (suffix, content_type, bytes) in [
            (
                "jobs/12/logs",
                "text/plain; charset=utf-8",
                b"job log".as_slice(),
            ),
            (
                "runs/11/logs",
                "application/zip",
                b"PK\x03\x04archive".as_slice(),
            ),
        ] {
            let response = client.get(format!("{base}/{suffix}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
            assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
        }
        proxy_task.abort();
        upstream_task.abort();
        download_task.abort();
    }

    #[tokio::test]
    async fn log_download_rejects_unsafe_redirect_targets() {
        let upstream = axum::Router::new().fallback(any(|headers: HeaderMap| async move {
            assert_eq!(headers.get("authorization").unwrap(), "Bearer host-secret");
            (
                StatusCode::FOUND,
                [(header::LOCATION, "file:///etc/passwd")],
            )
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), upstream_url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let response = reqwest::Client::new()
            .get(format!("{base}/repos/acme/widgets/actions/jobs/12/logs"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(!response.text().await.unwrap().contains("passwd"));
        proxy_task.abort();
        upstream_task.abort();
    }

    #[test]
    fn workflow_head_sha_requires_a_commit_sha() {
        assert!(!sha("main"));
        assert!(sha(&"a".repeat(40)));
    }

    #[tokio::test]
    async fn pull_creation_uses_host_token_and_does_not_forward_other_writes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let upstream = axum::Router::new().fallback(any(
            move |method: Method, headers: HeaderMap, uri: Uri, body: String| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(method, Method::POST);
                    assert_eq!(uri.path(), "/repos/acme/widgets/pulls");
                    assert_eq!(headers.get("authorization").unwrap(), "Bearer host-secret");
                    assert!(headers.get("x-client-secret").is_none());
                    let request: Value = serde_json::from_str(&body).unwrap();
                    if let Some(issue) = request.get("issue") {
                        assert_eq!(issue, 19);
                        assert!(request.get("title").is_none());
                    } else {
                        assert_eq!(request["title"], "Fix");
                    }
                    if let Some(head_repo) = request.get("head_repo") {
                        assert_eq!(head_repo, "other-widgets");
                    }
                    assert_eq!(request["head"], "rho/fix");
                    assert_eq!(request["base"], "main");
                    let mut result = pull();
                    result["draft"] = request.get("draft").cloned().unwrap_or(Value::Bool(false));
                    (StatusCode::CREATED, Json(result))
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), upstream_url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let path = format!("{base}/repos/acme/widgets/pulls");
        for request in [
            json!({"title":"Fix","head":"rho/fix","base":"main","draft":true,"state":"open"}),
            json!({"title":"Fix","head":"rho/fix","base":"main","draft":"yes"}),
            json!({"head":"rho/fix","base":"main"}),
            json!({"issue":"nineteen","head":"rho/fix","base":"main"}),
        ] {
            let response = client.post(&path).json(&request).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        assert_eq!(
            client
                .post(format!("{base}/graphql"))
                .json(&json!({"query":"{}"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client
                .patch(format!("{path}/42"))
                .json(&json!({"unexpected":true}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let response = client
            .post(&path)
            .bearer_auth("untrusted")
            .header("x-client-secret", "untrusted")
            .json(&json!({"title":"Fix","head":"rho/fix","base":"main","draft":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let result = response.json::<Value>().await.unwrap();
        assert_eq!(result["number"], 42);
        assert!(result.get("ignored").is_none());
        for (request, expected_draft) in [
            (
                json!({"title":"Fix","head":"rho/fix","base":"main","draft":false}),
                false,
            ),
            (json!({"title":"Fix","head":"rho/fix","base":"main"}), false),
            (
                json!({"title":"Fix","head":"rho/fix","head_repo":"other-widgets","base":"main"}),
                false,
            ),
            (json!({"issue":19,"head":"rho/fix","base":"main"}), false),
        ] {
            let response = client.post(&path).json(&request).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(
                response.json::<Value>().await.unwrap()["draft"],
                expected_draft
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 5);
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn read_handlers_relay_only_understood_fields() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let upstream = axum::Router::new().fallback(any(move |uri: Uri| {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::Relaxed);
                let result = match uri.path() {
                    "/repos/acme/widgets/pulls" => {
                        assert_eq!(uri.query(), Some("state=all&head=alice%3Arho%2Ffix&base=release%2Fnext&sort=updated&direction=asc&page=2&per_page=7"));
                        json!([pull()])
                    }
                    "/repos/acme/widgets/pulls/42" => pull(),
                    "/repos/acme/widgets/issues" => {
                        assert_eq!(uri.query(), Some("milestone=none&state=closed&assignee=alice&type=Bug&creator=bob&mentioned=carol&issue_field_values=priority%3AUrgent&labels=bug%2Cui&sort=updated&direction=asc&since=2026-09-01T12%3A34%3A56Z&page=3&per_page=9"));
                        json!([issue()])
                    },
                    "/repos/acme/widgets/issues/12" => issue(),
                    "/repos/acme/widgets/issues/999" => json!({"number":999}),
                    other => panic!("unexpected GitHub path {other}"),
                };
                Json(result)
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), upstream_url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let path = format!("{base}/repos/acme/widgets");
        for (suffix, field, value) in [
            (
                "pulls?state=all&head=alice%3Arho%2Ffix&base=release%2Fnext&sort=updated&direction=asc&page=2&per_page=7",
                "title",
                "Fix",
            ),
            ("pulls/42", "title", "Fix"),
            (
                "issues?milestone=none&state=closed&assignee=alice&type=Bug&creator=bob&mentioned=carol&issue_field_values=priority%3AUrgent&labels=bug%2Cui&sort=updated&direction=asc&since=2026-09-01T12%3A34%3A56Z&page=3&per_page=9",
                "body",
                "details",
            ),
            ("issues/12", "body", "details"),
        ] {
            let response = client.get(format!("{path}/{suffix}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{suffix}");
            let body = response.json::<Value>().await.unwrap();
            let item = if body.is_array() { &body[0] } else { &body };
            assert_eq!(item[field], value, "{suffix}");
            assert!(item.get("ignored").is_none(), "{suffix}");
        }
        for suffix in [
            "pulls?unexpected=x",
            "pulls?labels=bug",
            "issues?head=alice",
            "pulls?page=2&page=3",
            "issues/0",
            "pulls/00",
            "pulls/../../users",
        ] {
            let status = client
                .get(format!("{path}/{suffix}"))
                .send()
                .await
                .unwrap()
                .status();
            assert_ne!(status, StatusCode::OK, "{suffix}");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        let response = client
            .get(format!("{path}/issues/999"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn edits_and_feedback_use_only_selected_methods_fields_and_threads() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let upstream = axum::Router::new().fallback(any(
            move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(headers.get("authorization").unwrap(), "Bearer host-secret");
                    assert!(headers.get("x-client-secret").is_none());
                    let request = (!body.is_empty()).then(|| serde_json::from_str::<Value>(&body).unwrap());
                    let (status, result) = match (method, uri.path()) {
                        (Method::PATCH, "/repos/acme/widgets/pulls/42") => {
                            assert_eq!(request.unwrap(), json!({"title":"New","body":"Updated"}));
                            (StatusCode::OK, pull())
                        }
                        (Method::GET, "/repos/acme/widgets/issues/42/comments") => {
                            assert_eq!(uri.query(), Some("page=2&per_page=5"));
                            (StatusCode::OK, json!([{
                                "id":11,"html_url":"https://github.com/acme/widgets/pull/42#issuecomment-11",
                                "body":"Please fix","user":{"login":"reviewer"},
                                "created_at":"2025-01-01T00:00:00Z",
                                "updated_at":"2025-01-01T00:00:00Z","ignored":"not relayed"
                            }]))
                        }
                        (Method::GET, "/repos/acme/widgets/pulls/42/reviews") => {
                            assert_eq!(uri.query(), Some("page=3"));
                            (StatusCode::OK, json!([{
                                "id":22,"html_url":"https://github.com/acme/widgets/pull/42#pullrequestreview-22",
                                "body":"Changes requested","state":"CHANGES_REQUESTED",
                                "user":{"login":"reviewer","id":9,"type":"Bot"},"author_association":"MEMBER","submitted_at":"2025-01-01T00:00:00Z",
                                "commit_id":"a".repeat(40),"ignored":"not relayed"
                            }]))
                        }
                        (Method::GET, "/repos/acme/widgets/pulls/42/comments") => {
                            assert_eq!(uri.query(), Some("sort=created&direction=asc&page=4"));
                            (StatusCode::OK, json!([{
                                "id":33,"html_url":"https://github.com/acme/widgets/pull/42#discussion_r33",
                                "body":"Fix this line","user":{"login":"reviewer"},
                                "path":"src/lib.rs","diff_hunk":"@@ -1 +1 @@",
                                "line":7,"original_line":6,"side":"RIGHT",
                                "commit_id":"a".repeat(40),"in_reply_to_id":null,"pull_request_review_id":22,"author_association":"COLLABORATOR",
                                "created_at":"2025-01-01T00:00:00Z",
                                "updated_at":"2025-01-01T00:00:00Z","ignored":"not relayed"
                            }, {
                                "id":34,"html_url":"https://github.com/acme/widgets/pull/42#discussion_r34",
                                "body":"Follow-up","user":{"login":"other","id":10,"type":"User"},
                                "path":"src/lib.rs","diff_hunk":"@@ -1 +1 @@",
                                "line":7,"original_line":6,"side":"RIGHT",
                                "commit_id":"a".repeat(40),"in_reply_to_id":33,"pull_request_review_id":22,
                                "created_at":"2025-01-02T00:00:00Z","updated_at":"2025-01-02T00:00:00Z"
                            }]))
                        }
                        (Method::POST, "/repos/acme/widgets/issues/42/comments") => {
                            assert_eq!(request.unwrap(), json!({"body":"Thanks"}));
                            (StatusCode::CREATED, json!({
                                "id":44,"html_url":"https://github.com/acme/widgets/pull/42#issuecomment-44",
                                "body":"Thanks","user":{"login":"agent"},
                                "created_at":"2025-01-01T00:00:00Z",
                                "updated_at":"2025-01-01T00:00:00Z","ignored":"not relayed"
                            }))
                        }
                        (Method::POST, "/repos/acme/widgets/pulls/42/comments/33/replies") => {
                            assert_eq!(request.unwrap(), json!({"body":"Fixed"}));
                            (StatusCode::CREATED, json!({
                                "id":55,"html_url":"https://github.com/acme/widgets/pull/42#discussion_r55",
                                "body":"Fixed","user":{"login":"agent"},
                                "path":"src/lib.rs","diff_hunk":"@@ -1 +1 @@",
                                "line":7,"original_line":6,"side":"RIGHT",
                                "commit_id":"a".repeat(40),"in_reply_to_id":33,
                                "created_at":"2025-01-01T00:00:00Z",
                                "updated_at":"2025-01-01T00:00:00Z","ignored":"not relayed"
                            }))
                        }
                        (method, path) => panic!("unexpected upstream request {method} {path}"),
                    };
                    (status, Json(result))
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), upstream_url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let pr = format!("{base}/repos/acme/widgets/pulls/42");
        let issue = format!("{base}/repos/acme/widgets/issues/42/comments");

        let edit = client
            .patch(&pr)
            .bearer_auth("untrusted")
            .header("x-client-secret", "untrusted")
            .json(&json!({"title":"New","body":"Updated"}))
            .send()
            .await
            .unwrap();
        assert_eq!(edit.status(), StatusCode::OK);
        assert!(edit.json::<Value>().await.unwrap().get("ignored").is_none());
        for (url, field, value) in [
            (format!("{issue}?page=2&per_page=5"), "body", "Please fix"),
            (format!("{pr}/reviews?page=3"), "state", "CHANGES_REQUESTED"),
            (
                format!("{pr}/comments?sort=created&direction=asc&page=4"),
                "path",
                "src/lib.rs",
            ),
        ] {
            let response = client.get(&url).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.json::<Value>().await.unwrap();
            let item = &body[0];
            assert_eq!(item[field], value);
            if url.contains("/reviews") {
                assert_eq!(item["user"]["type"], "Bot");
                assert_eq!(item["user"]["id"], 9);
                assert_eq!(item["author_association"], "MEMBER");
            }
            if url.contains("/pulls/42/comments?") {
                assert_eq!(item["pull_request_review_id"], 22);
                assert_eq!(item["author_association"], "COLLABORATOR");
                assert_eq!(body[1]["in_reply_to_id"], 33);
                assert_eq!(body[1]["pull_request_review_id"], 22);
                assert_eq!(body[1]["user"]["id"], 10);
            }
            assert!(item.get("ignored").is_none());
        }
        for (url, body, expected_id) in [
            (issue.clone(), "Thanks", 44),
            (format!("{pr}/comments/33/replies"), "Fixed", 55),
        ] {
            let response = client
                .post(url)
                .json(&json!({"body":body}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            let item = response.json::<Value>().await.unwrap();
            assert_eq!(item["id"], expected_id);
            if expected_id == 55 {
                assert_eq!(item["in_reply_to_id"], 33);
            }
            assert!(item.get("ignored").is_none());
        }
        for response in [
            client
                .patch(&pr)
                .json(&json!({"unexpected":true}))
                .send()
                .await
                .unwrap(),
            client
                .post(format!("{pr}/reviews"))
                .json(&json!({"event":"APPROVE"}))
                .send()
                .await
                .unwrap(),
            client
                .post(format!("{pr}/comments"))
                .json(&json!({"body":"new review"}))
                .send()
                .await
                .unwrap(),
            client
                .post(&issue)
                .json(&json!({"body":"ok","state":"closed"}))
                .send()
                .await
                .unwrap(),
        ] {
            assert!(matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::METHOD_NOT_ALLOWED
            ));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 6);
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn commit_status_reads_use_host_token_and_reject_mutations() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let upstream = axum::Router::new().fallback(any(
            move |method: Method, headers: HeaderMap, uri: Uri, body: String| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(method, Method::GET);
                    assert_eq!(headers.get("authorization").unwrap(), "Bearer host-secret");
                    assert!(headers.get("x-client-secret").is_none());
                    let tail = uri.path().strip_prefix("/repos/acme/widgets/commits/").unwrap();
                    let (reference, endpoint) = tail.rsplit_once('/').unwrap();
                    assert!(matches!(endpoint, "status" | "check-runs"));
                    if reference == sha {
                        assert_eq!(uri.query(), Some(if endpoint == "status" {
                            "page=3&per_page=7"
                        } else {
                            "check_name=unit+tests&status=completed&filter=all&page=2&per_page=99&app_id=23"
                        }));
                    } else {
                        // Slash-containing refs must remain one encoded upstream segment.
                        assert!(matches!(reference, "main" | "heads%2Frho%2Ffix"
                            | "tags%2Frelease%2Fv1.2+rc" | "rho%2F%E4%BF%AE%E6%AD%A3"
                            | "rho%2F50%25complete" | "rho%2Ffix%252Fother"), "{reference}");
                        assert!(uri.query().is_none());
                    }
                    assert!(body.is_empty());
                    let result = if uri.path().ends_with("/status") {
                        json!({"state":"success","statuses":[{"context":"build","state":"success"}],
                               "ignored":"not relayed"})
                    } else {
                        json!({"total_count":1, "check_runs":[{"id":17,"name":"build",
                            "status":"completed","conclusion":"success",
                            "started_at":null,"completed_at":null}],"ignored":"not relayed"})
                    };
                    let mut response = (StatusCode::OK, Json(result)).into_response();
                    response.headers_mut().insert(
                        "x-ratelimit-remaining",
                        axum::http::HeaderValue::from_static("4900"),
                    );
                    response.headers_mut().insert(
                        "x-ratelimit-limit",
                        axum::http::HeaderValue::from_static("5000"),
                    );
                    response
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), upstream_url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let path = format!("{base}/repos/acme/widgets/commits/{sha}");
        for suffix in [
            "status?page=3&per_page=7",
            "check-runs?check_name=unit+tests&status=completed&filter=all&page=2&per_page=99&app_id=23",
        ] {
            let response = client
                .get(format!("{path}/{suffix}"))
                .bearer_auth("untrusted")
                .header("x-client-secret", "do-not-forward")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get("x-ratelimit-remaining").unwrap(),
                "4900"
            );
            assert_eq!(response.headers().get("x-ratelimit-limit").unwrap(), "5000");
            let body = response.json::<Value>().await.unwrap();
            assert!(body.get("ignored").is_none());
            if suffix.starts_with("status") {
                assert_eq!(body["statuses"][0]["context"], "build");
            } else {
                assert_eq!(body["check_runs"][0]["id"], 17);
            }
        }
        for reference in [
            "main",
            "heads/rho/fix",
            "heads%2Frho%2Ffix",
            "tags/release/v1.2%2Brc",
            "rho/%E4%BF%AE%E6%AD%A3",
            "rho/50%25complete",
            "rho/fix%252Fother",
        ] {
            for endpoint in ["status", "check-runs"] {
                let response = client
                    .get(format!(
                        "{base}/repos/acme/widgets/commits/{reference}/{endpoint}"
                    ))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{reference}/{endpoint}");
                let result = response.json::<Value>().await.unwrap();
                if endpoint == "status" {
                    assert_eq!(result["statuses"][0]["context"], "build");
                } else {
                    assert_eq!(result["check_runs"][0]["id"], 17);
                }
            }
        }
        for suffix in [
            "rho%2F..%2Fsecret/status",
            "heads%2F.%2Ffix/check-runs",
            "rho%2F%2Ffix/status",
            "main/files",
            "main/status?filter=all",
        ] {
            assert_eq!(
                client
                    .get(format!("{base}/repos/acme/widgets/commits/{suffix}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN,
                "{suffix}"
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 16);
        for suffix in ["status?unexpected=2"] {
            assert_eq!(
                client
                    .get(format!("{path}/{suffix}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            client
                .post(format!("{path}/status"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            client
                .get(format!("{base}/repos/acme/widgets/commits/%2e%2e/status"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(calls.load(Ordering::Relaxed), 16);
        proxy_task.abort();
        upstream_task.abort();
    }
}
#[cfg(test)]
mod redirect_tests {
    use axum::routing::get;

    use super::*;

    #[tokio::test]
    async fn redirects_are_not_followed_with_host_token() {
        let upstream = axum::Router::new().route(
            "/repos/acme/widgets/pulls/12",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer host-secret");
                (
                    StatusCode::FOUND,
                    [("location", "https://untrusted.example/archive")],
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let router = crate::router(Arc::new(|| Ok("host-secret".to_owned())), url);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let response = reqwest::get(format!("http://{addr}/repos/acme/widgets/pulls/12"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        proxy_task.abort();
        upstream_task.abort();
    }
}
