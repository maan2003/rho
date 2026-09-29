//! Selected ghapi REST operations. Agents never send a GitHub token.
use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
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
        .route("/repos/{owner}/{repo}/pulls/{number}", get(get_pull))
        .route("/repos/{owner}/{repo}/issues", get(list_issues))
        .route("/repos/{owner}/{repo}/issues/{number}", get(get_issue))
        .route(
            "/repos/{owner}/{repo}/commits/{ref}/status",
            get(get_status),
        )
        .route(
            "/repos/{owner}/{repo}/commits/{ref}/check-runs",
            get(list_checks),
        )
        .layer(DefaultBodyLimit::disable())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePullRequest {
    title: String,
    head: String,
    base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maintainer_can_modify: Option<bool>,
}

#[derive(Serialize, Deserialize)]
struct User {
    login: String,
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
}

#[derive(Serialize, Deserialize)]
struct CheckRuns {
    total_count: u64,
    check_runs: Vec<CheckRun>,
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

fn allowed_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
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

// Only the host's token and fields represented by T cross the boundary.
async fn github<T: DeserializeOwned + Serialize, B: Serialize, Q: Serialize>(
    state: Arc<AppState>,
    method: Method,
    path: &[&str],
    query: Option<Q>,
    body: Option<B>,
) -> Response {
    if !path.iter().all(|part| allowed_segment(part)) {
        return forbidden();
    }
    let token = match state.get_token().await {
        Ok(token) => token,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
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
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
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
        let message = serde_json::from_slice::<GitHubError>(&bytes)
            .map(|error| error.message)
            .unwrap_or_else(|_| "GitHub request failed".into());
        return (status, Json(json!({"message":message}))).into_response();
    }
    match serde_json::from_slice::<T>(&bytes) {
        Ok(value) => (status, headers, Json(value)).into_response(),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

async fn list_pulls(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    query: Result<Query<ListQuery>, QueryRejection>,
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
    if uri.query().is_some() {
        return forbidden();
    }
    github_post::<Pull, _>(state, &["repos", &owner, &repo, "pulls"], pr).await
}

async fn list_issues(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    query: Result<Query<ListQuery>, QueryRejection>,
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

async fn get_status(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    if uri.query().is_some() || !self::sha(&sha) {
        return forbidden();
    }
    github_get::<CombinedStatus, ()>(
        state,
        &["repos", &owner, &repo, "commits", &sha, "status"],
        None,
    )
    .await
}

async fn list_checks(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    query: Result<Query<CheckQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return forbidden();
    };
    if !self::sha(&sha) {
        return forbidden();
    }
    github_get::<CheckRuns, _>(
        state,
        &["repos", &owner, &repo, "commits", &sha, "check-runs"],
        Some(query),
    )
    .await
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

    #[test]
    fn path_segments_cannot_escape_the_selected_route() {
        for segment in ["..", ".", "main/secret", "main%2Fsecret"] {
            assert!(!allowed_segment(segment));
        }
        assert!(allowed_segment("rho.fix"));
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
                    assert_eq!(request["title"], "Fix");
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
                .json(&json!({"title":"Changed"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
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
        ] {
            let response = client.post(&path).json(&request).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(
                response.json::<Value>().await.unwrap()["draft"],
                expected_draft
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 3);
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
                        assert_eq!(uri.query(), Some("state=all&page=2"));
                        json!([pull()])
                    }
                    "/repos/acme/widgets/pulls/42" => pull(),
                    "/repos/acme/widgets/issues" => json!([issue()]),
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
            ("pulls?state=all&page=2", "title", "Fix"),
            ("pulls/42", "title", "Fix"),
            ("issues", "body", "details"),
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
                    assert!(
                        uri.path() == format!("/repos/acme/widgets/commits/{sha}/status")
                            && uri.query().is_none()
                            || uri.path()
                                == format!("/repos/acme/widgets/commits/{sha}/check-runs")
                                && uri.query() == Some("page=2&per_page=99")
                    );
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
        for suffix in ["status", "check-runs?page=2&per_page=99"] {
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
            if suffix == "status" {
                assert_eq!(body["statuses"][0]["context"], "build");
            } else {
                assert_eq!(body["check_runs"][0]["id"], 17);
            }
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        for suffix in ["status?page=2"] {
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
        assert_eq!(calls.load(Ordering::Relaxed), 2);
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
