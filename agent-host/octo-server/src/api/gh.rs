//! Selected ghapi REST operations. The Unix socket and its input are untrusted.
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::AppState;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/repos/{owner}/{repo}/pulls", get(proxy).post(proxy))
        .route("/repos/{owner}/{repo}/pulls/{number}", get(proxy))
        .route("/repos/{owner}/{repo}/issues", get(proxy))
        .route("/repos/{owner}/{repo}/issues/{number}", get(proxy))
        .route("/repos/{owner}/{repo}/commits/{ref}/status", get(proxy))
        .route("/repos/{owner}/{repo}/commits/{ref}/check-runs", get(proxy))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftPr {
    title: String,
    head: String,
    base: String,
    draft: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maintainer_can_modify: Option<bool>,
}

fn allowed_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

fn parse_path(path: &str) -> Option<Vec<String>> {
    let mut result = Vec::new();
    for part in path.strip_prefix('/')?.split('/') {
        // Reject escapes, including encoded separators, rather than relying on
        // the URL library and the policy parser to normalize them identically.
        if !allowed_segment(part) {
            return None;
        }
        result.push(part.to_owned());
    }
    Some(result)
}

fn text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.contains('\0')
}

fn title(s: &str) -> bool {
    text(s, 256) && !s.trim().is_empty()
}

fn reference(s: &str) -> bool {
    text(s, 255) && !s.is_empty() && !s.chars().any(char::is_control)
}

fn number(s: &str) -> bool {
    s.parse::<u64>().is_ok_and(|v| v > 0)
}

fn commit_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_query_params(uri: &Uri, keys: &[&str]) -> Option<BTreeMap<String, String>> {
    let mut params = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
        if !keys.contains(&key.as_ref()) || params.contains_key(key.as_ref()) {
            return None;
        }
        let value = value.into_owned();
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return None;
        }
        let value = match key.as_ref() {
            "state" if matches!(value.as_str(), "open" | "closed" | "all") => value,
            "page" => value.parse::<u32>().ok().filter(|n| *n > 0)?.to_string(),
            "per_page" => value
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=100).contains(n))?
                .to_string(),
            _ => return None,
        };
        params.insert(key.into_owned(), value);
    }
    Some(params)
}

fn rest_operation(
    method: &Method,
    parts: &[&str],
    bytes: &[u8],
    uri: &Uri,
) -> Option<(BTreeMap<String, String>, Option<Value>)> {
    let (keys, body) = match (method, parts) {
        (&Method::GET, ["repos", _, _, "pulls" | "issues"]) => {
            (&["state", "page", "per_page"][..], None)
        }
        (&Method::GET, ["repos", _, _, "pulls" | "issues", n]) if number(n) => (&[][..], None),
        (&Method::GET, ["repos", _, _, "commits", sha, "status"]) if commit_sha(sha) => {
            (&[][..], None)
        }
        (&Method::GET, ["repos", _, _, "commits", sha, "check-runs"]) if commit_sha(sha) => {
            (&["page", "per_page"][..], None)
        }
        (&Method::POST, ["repos", _, _, "pulls"]) => {
            let pr: DraftPr = serde_json::from_slice(bytes).ok()?;
            if !pr.draft
                || !title(&pr.title)
                || !reference(&pr.head)
                || !reference(&pr.base)
                || pr.body.as_deref().is_some_and(|body| !text(body, 65536))
            {
                return None;
            }
            (&[][..], Some(serde_json::to_value(pr).ok()?))
        }
        _ => return None,
    };
    if body.is_none() && !bytes.is_empty() {
        return None;
    }
    Some((parse_query_params(uri, keys)?, body))
}

fn forbidden(message: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"message":message}))).into_response()
}

pub async fn proxy(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    _headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(parts) = parse_path(uri.path()) else {
        return forbidden("GitHub operation unavailable through Octo");
    };
    let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
    let bytes = match to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let Some((params, body)) = rest_operation(&method, &parts, &bytes, &uri) else {
        return forbidden("GitHub operation unavailable through Octo");
    };
    let token = match state.get_token().await {
        Ok(token) => token,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut url = state.github_api_url.clone();
    url.path_segments_mut()
        .expect("GitHub API base is hierarchical")
        .clear()
        .extend(&parts);
    url.set_query(None);
    if !params.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    let mut request = state
        .client
        .request(method, url)
        .bearer_auth(token)
        .header("User-Agent", "octo-gh")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Accept", "application/vnd.github+json");
    if let Some(body) = body {
        request = request.json(&body);
    }
    let upstream = match request.send().await {
        Ok(response) => response,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    // The explicit operations here do not require redirects. Never follow an
    // unvalidated Location with the host's token.
    if upstream.status().is_redirection() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let status = upstream.status();
    let mut response_headers = HeaderMap::new();
    for name in [
        "content-type",
        "link",
        "etag",
        "x-ratelimit-remaining",
        "x-ratelimit-limit",
        "x-ratelimit-reset",
    ] {
        if let Some(value) = upstream.headers().get(name) {
            response_headers.insert(name, value.clone());
        }
    }
    let mut received = 0_usize;
    let stream = upstream.bytes_stream().map(move |part| {
        let part = part.map_err(std::io::Error::other)?;
        received = received.saturating_add(part.len());
        if received > 48 * 1024 * 1024 {
            return Err(std::io::Error::other(
                "GitHub response exceeded the 48 MiB proxy limit",
            ));
        }
        Ok(part)
    });
    (status, response_headers, Body::from_stream(stream)).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::any;

    use super::*;

    #[test]
    fn ghapi_paths_and_draft_creation_are_only_rest_operations() {
        assert!(parse_path("/repos/acme/widgets/%2e%2e").is_none());
        let pulls = &["repos", "acme", "widgets", "pulls"];
        let query: Uri = "/repos/acme/widgets/pulls?state=open&per_page=100&page=2"
            .parse()
            .unwrap();
        assert!(rest_operation(&Method::GET, pulls, b"", &query).is_some());
        for q in [
            "state=other",
            "per_page=101",
            "page=0",
            "page=1&page=2",
            "head=branch",
        ] {
            let uri: Uri = format!("/repos/acme/widgets/pulls?{q}").parse().unwrap();
            assert!(
                rest_operation(&Method::GET, pulls, b"", &uri).is_none(),
                "{q}"
            );
        }
        let uri: Uri = "/repos/acme/widgets/pulls".parse().unwrap();
        assert!(
            rest_operation(
                &Method::POST,
                pulls,
                br#"{"title":"Draft","head":"rho/change","base":"main","draft":true}"#,
                &uri
            )
            .is_some()
        );
        for body in [
            br#"{"title":"Draft","head":"rho/change","base":"main","draft":false}"#.as_slice(),
            br#"{"title":"Draft","head":"rho/change","base":"main"}"#,
            br#"{"title":"Draft","head":"rho/change","base":"main","draft":true,"state":"open"}"#,
        ] {
            assert!(rest_operation(&Method::POST, pulls, body, &uri).is_none());
        }
        assert!(
            rest_operation(
                &Method::PATCH,
                &["repos", "acme", "widgets", "pulls", "12"],
                br#"{"title":"Changed"}"#,
                &uri
            )
            .is_none()
        );
        assert!(
            rest_operation(
                &Method::GET,
                &["repos", "acme", "widgets", "pulls", "12", "files"],
                b"",
                &uri
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn draft_creation_uses_host_token_and_does_not_forward_other_writes() {
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
                    assert_eq!(request["draft"], true);
                    assert_eq!(request["title"], "Fix");
                    assert_eq!(request["head"], "rho/fix");
                    assert_eq!(request["base"], "main");
                    (StatusCode::CREATED, Json(json!({"number":42,"draft":true})))
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
            json!({"title":"Fix","head":"rho/fix","base":"main","draft":false}),
            json!({"title":"Fix","head":"rho/fix","base":"main"}),
            json!({"title":"Fix","head":"rho/fix","base":"main","draft":true,"state":"open"}),
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
        assert_eq!(response.json::<Value>().await.unwrap()["number"], 42);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        proxy_task.abort();
        upstream_task.abort();
    }

    #[test]
    fn commit_status_reads_require_sha_and_bounded_queries() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let status = ["repos", "acme", "widgets", "commits", sha, "status"];
        let checks = ["repos", "acme", "widgets", "commits", sha, "check-runs"];
        let uri: Uri = format!("/repos/acme/widgets/commits/{sha}/status")
            .parse()
            .unwrap();
        assert!(rest_operation(&Method::GET, &status, b"", &uri).is_some());
        assert!(rest_operation(&Method::GET, &status, b"body", &uri).is_none());
        assert!(rest_operation(&Method::HEAD, &status, b"", &uri).is_none());
        assert!(rest_operation(&Method::POST, &status, b"", &uri).is_none());
        let query: Uri = format!("{uri}?per_page=100&page=2").parse().unwrap();
        assert!(rest_operation(&Method::GET, &status, b"", &query).is_none());
        let (params, _) = rest_operation(&Method::GET, &checks, b"", &query).unwrap();
        assert_eq!(params["page"], "2");
        assert_eq!(params["per_page"], "100");
        for query in ["page=0", "per_page=101", "page=2&page=3", "filter=latest"] {
            let uri: Uri = format!("/repos/acme/widgets/commits/{sha}/check-runs?{query}")
                .parse()
                .unwrap();
            assert!(
                rest_operation(&Method::GET, &checks, b"", &uri).is_none(),
                "{query}"
            );
        }
        for ref_name in ["main", "..", "0123456789abcdef0123456789abcdef0123456g"] {
            let parts = ["repos", "acme", "widgets", "commits", ref_name, "status"];
            assert!(rest_operation(&Method::GET, &parts, b"", &uri).is_none());
        }
        assert!(parse_path("/repos/acme/widgets/commits/%2e%2e/status").is_none());
        assert!(parse_path("/repos/acme/widgets/commits/main%2Fsecret/status").is_none());
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
                    let mut response = (StatusCode::OK, "{\"state\":\"success\"}").into_response();
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
            assert_eq!(response.text().await.unwrap(), "{\"state\":\"success\"}");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        for suffix in [
            "status?page=2",
            "check-runs?per_page=101",
            "check-runs?page=0",
        ] {
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
