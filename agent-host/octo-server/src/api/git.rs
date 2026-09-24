use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt as _, stream};
use serde::Deserialize;

use crate::error::AppError;
use crate::state::AppState;

const MAX_COMMAND_BYTES: usize = 64 * 1024;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/git/{owner}/{repo}/info/refs", get(advertise_refs))
        .route("/git/{owner}/{repo}/git-receive-pack", post(receive_pack))
        .route("/git/{owner}/{repo}/git-upload-pack", post(upload_pack))
}

#[derive(Deserialize)]
struct ServiceQuery {
    service: String,
}

async fn advertise_refs(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<ServiceQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !matches!(
        query.service.as_str(),
        "git-receive-pack" | "git-upload-pack"
    ) {
        return Ok((
            StatusCode::BAD_REQUEST,
            "only git-receive-pack and git-upload-pack are supported",
        )
            .into_response());
    }

    let repo = repo.trim_end_matches(".git");
    proxy(
        &state,
        reqwest::Method::GET,
        &owner,
        repo,
        "info/refs",
        Some(&format!("service={}", query.service)),
        None,
        headers.get("git-protocol"),
        None,
    )
    .await
}

async fn upload_pack(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    let repo = repo.trim_end_matches(".git");
    proxy(
        &state,
        reqwest::Method::POST,
        &owner,
        repo,
        "git-upload-pack",
        None,
        Some(reqwest::Body::wrap_stream(body.into_data_stream())),
        headers.get("git-protocol"),
        headers.get("content-encoding"),
    )
    .await
}

async fn receive_pack(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    let repo = repo.trim_end_matches(".git");
    let body = validate_commands(body).await?;
    proxy(
        &state,
        reqwest::Method::POST,
        &owner,
        repo,
        "git-receive-pack",
        None,
        Some(body),
        headers.get("git-protocol"),
        headers.get("content-encoding"),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn proxy(
    state: &AppState,
    method: reqwest::Method,
    owner: &str,
    repo: &str,
    endpoint: &str,
    query: Option<&str>,
    body: Option<reqwest::Body>,
    git_protocol: Option<&axum::http::HeaderValue>,
    content_encoding: Option<&axum::http::HeaderValue>,
) -> Result<Response, AppError> {
    let token = state.get_token().await?;
    let mut url = state.github_git_url(owner, repo, endpoint)?;
    url.set_query(query);
    let credentials =
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));

    let mut request = state
        .client
        .request(method, url)
        .header("Authorization", format!("Basic {credentials}"))
        .header("User-Agent", "octo");
    if let Some(git_protocol) = git_protocol {
        request = request.header("Git-Protocol", git_protocol);
    }
    if let Some(content_encoding) = content_encoding {
        request = request.header("Content-Encoding", content_encoding);
    }
    if let Some(body) = body {
        request = request
            .header("Content-Type", format!("application/x-{endpoint}-request"))
            .body(body);
    }
    let response = request.send().await?;
    let status = StatusCode::from_u16(response.status().as_u16())?;
    let mut headers = HeaderMap::new();
    if let Some(value) = response.headers().get("content-type") {
        headers.insert("content-type", value.clone());
    }
    Ok((status, headers, Body::from_stream(response.bytes_stream())).into_response())
}

async fn validate_commands(body: Body) -> Result<reqwest::Body, AppError> {
    let mut input = body.into_data_stream();
    let mut prefix = BytesMut::new();
    let command_end;

    loop {
        let Some(chunk) = input.next().await else {
            return Err(anyhow::anyhow!("truncated git receive-pack request").into());
        };
        prefix.extend_from_slice(&chunk?);
        if let Some(end) = parse_and_validate_commands(&prefix)? {
            if end > MAX_COMMAND_BYTES {
                return Err(anyhow::anyhow!("git receive-pack command list is too large").into());
            }
            command_end = end;
            break;
        }
        if prefix.len() > MAX_COMMAND_BYTES {
            return Err(anyhow::anyhow!("git receive-pack command list is too large").into());
        }
    }

    // Preserve bytes from the first pack frame that may share the final command
    // chunk.
    let prefix = prefix.freeze();
    debug_assert!(command_end <= prefix.len());
    let stream = stream::once(async move { Ok::<Bytes, axum::Error>(prefix) }).chain(input);
    Ok(reqwest::Body::wrap_stream(stream))
}

fn parse_and_validate_commands(input: &[u8]) -> Result<Option<usize>, AppError> {
    let mut offset = 0;
    let mut first = true;
    loop {
        if input.len() < offset + 4 {
            return Ok(None);
        }
        let length = usize::from_str_radix(
            std::str::from_utf8(&input[offset..offset + 4])
                .map_err(|_| anyhow::anyhow!("invalid receive-pack packet length"))?,
            16,
        )
        .map_err(|_| anyhow::anyhow!("invalid receive-pack packet length"))?;
        if length == 0 {
            return Ok(Some(offset + 4));
        }
        if length < 4 {
            return Err(anyhow::anyhow!("invalid receive-pack packet length").into());
        }
        if input.len() < offset + length {
            return Ok(None);
        }
        let mut command = &input[offset + 4..offset + length];
        if command.starts_with(b"shallow ") {
            offset += length;
            continue;
        }
        if command.starts_with(b"push-cert") {
            return Err(anyhow::anyhow!("Octo does not support signed pushes").into());
        }
        if first {
            command = command.split(|byte| *byte == 0).next().unwrap_or(command);
            first = false;
        }
        let command = std::str::from_utf8(command)
            .map_err(|_| anyhow::anyhow!("invalid receive-pack command"))?
            .trim_end_matches('\n');
        let mut fields = command.split_ascii_whitespace();
        let _old = fields.next();
        let _new = fields.next();
        let reference = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid receive-pack command"))?;
        if !valid_rho_ref(reference) {
            return Err(anyhow::anyhow!(
                "Octo may only push valid refs below refs/heads/rho/ (requested {reference})"
            )
            .into());
        }
        offset += length;
    }
}

fn valid_rho_ref(reference: &str) -> bool {
    let Some(suffix) = reference.strip_prefix("refs/heads/rho/") else {
        return false;
    };
    !suffix.is_empty()
        && !suffix.starts_with('.')
        && !suffix.ends_with(['/', '.'])
        && !suffix.contains("..")
        && !suffix.contains("@{")
        && !suffix.contains("//")
        && !suffix.split('/').any(|part| part.ends_with(".lock"))
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn advertises_all_upstream_refs() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let refs = Bytes::from_static(
            b"master refs/heads/master\nrho refs/heads/rho/change\ntag refs/tags/v1\n",
        );
        let upstream = Router::new().route(
            "/acme/library.git/info/refs",
            get({
                let refs = refs.clone();
                move || {
                    let refs = refs.clone();
                    async move { (StatusCode::OK, refs) }
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

        let octo = crate::router(
            Arc::new(|| Ok("test-token".to_owned())),
            reqwest::Url::parse(&format!("http://{upstream_addr}")).unwrap(),
        );
        let octo_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let octo_addr = octo_listener.local_addr().unwrap();
        let octo_task =
            tokio::spawn(async move { axum::serve(octo_listener, octo).await.unwrap() });

        let response = reqwest::get(format!(
            "http://{octo_addr}/git/acme/library.git/info/refs?service=git-upload-pack"
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap(), refs);

        octo_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn forwards_git_protocol_and_content_encoding() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let seen_tx = Arc::new(std::sync::Mutex::new(Some(seen_tx)));
        let upstream = Router::new().route(
            "/acme/library.git/git-upload-pack",
            post({
                let seen_tx = seen_tx.clone();
                move |headers: HeaderMap, body: Bytes| {
                    let seen_tx = seen_tx.clone();
                    async move {
                        seen_tx
                            .lock()
                            .unwrap()
                            .take()
                            .unwrap()
                            .send((
                                headers.get("git-protocol").cloned(),
                                headers.get("content-encoding").cloned(),
                                body,
                            ))
                            .unwrap();
                        StatusCode::OK
                    }
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_task =
            tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

        let octo = crate::router(
            Arc::new(|| Ok("test-token".to_owned())),
            reqwest::Url::parse(&format!("http://{upstream_addr}")).unwrap(),
        );
        let octo_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let octo_addr = octo_listener.local_addr().unwrap();
        let octo_task =
            tokio::spawn(async move { axum::serve(octo_listener, octo).await.unwrap() });

        let body = Bytes::from_static(b"compressed git request");
        let response = reqwest::Client::new()
            .post(format!(
                "http://{octo_addr}/git/acme/library/git-upload-pack"
            ))
            .header("Git-Protocol", "version=2")
            .header("Content-Encoding", "gzip")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let (protocol, encoding, forwarded_body) = seen_rx.await.unwrap();
        assert_eq!(protocol.unwrap(), "version=2");
        assert_eq!(encoding.unwrap(), "gzip");
        assert_eq!(forwarded_body, body);

        octo_task.abort();
        upstream_task.abort();
    }

    fn packet(command: &str) -> Vec<u8> {
        let payload = format!("{command}\0 report-status\n");
        format!("{:04x}{payload}0000", payload.len() + 4).into_bytes()
    }

    #[test]
    fn permits_rho_branches() {
        assert!(parse_and_validate_commands(&packet(
            "0000000000000000000000000000000000000000 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/rho/test"
        ))
        .unwrap()
        .is_some());
    }

    #[test]
    fn rejects_other_refs() {
        let error = parse_and_validate_commands(&packet(
            "0000000000000000000000000000000000000000 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main",
        ))
        .unwrap_err();
        assert!(error.0.to_string().contains("refs/heads/rho/"));
    }

    #[test]
    fn rejects_invalid_names_within_rho_namespace() {
        for reference in [
            "refs/heads/rho/",
            "refs/heads/rho/../main",
            "refs/heads/rho/a.lock",
            "refs/heads/rho/a//b",
        ] {
            assert!(!valid_rho_ref(reference), "accepted {reference}");
        }
    }
}
