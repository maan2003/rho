//! The agent host's Google server.
//!
//! Agents call Google's REST APIs raw over this Unix socket:
//! `/<host>/<path>?<query>` goes to `https://<host>/<path>?<query>` for a
//! host under `googleapis.com`, with the method, body and headers as sent,
//! and the grant's access token in place of any credentials the agent sent.
//! `rho google init` installs the grant: an OAuth client and the refresh
//! token of the Google account agents act as. The grant's scopes
//! ([`oauth::SCOPES`]) and what that account can see bound what agents
//! reach; Google checks the rest.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use reqwest::Client;
use serde_json::json;
use tokio::net::UnixListener;

pub mod oauth;

pub use oauth::Endpoints;

/// The platform secrets `rho google init` installs.
pub const CLIENT_ID: &str = "GOOGLE_CLIENT_ID";
pub const CLIENT_SECRET: &str = "GOOGLE_CLIENT_SECRET";
pub const REFRESH_TOKEN: &str = "GOOGLE_REFRESH_TOKEN";

pub type SecretReader = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

/// How long before its expiry an access token is refreshed, so a call
/// does not start with a token that expires on the way.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

struct AppState {
    client: Client,
    endpoints: Endpoints,
    read_secret: SecretReader,
    token: tokio::sync::Mutex<Option<AccessToken>>,
}

#[derive(Clone)]
struct AccessToken {
    value: String,
    refresh_at: Instant,
    /// The refresh token it came from: a new `rho google init` replaces it,
    /// and the new grant's scopes apply from the next call.
    grant: String,
}

pub fn router(read_secret: SecretReader, endpoints: Endpoints) -> Router {
    let state = Arc::new(AppState {
        client: oauth::http_client(),
        endpoints,
        read_secret,
        token: Default::default(),
    });
    Router::new().fallback(any(forward)).with_state(state)
}

pub async fn serve(listener: UnixListener, router: Router) -> Result<()> {
    axum::serve(listener, router).await?;
    Ok(())
}

/// Request headers that stay here: credentials, which the server sets, and
/// those of this hop.
const REQUEST_HEADERS_KEPT: &[HeaderName] = &[
    header::AUTHORIZATION,
    header::PROXY_AUTHORIZATION,
    header::COOKIE,
    header::HOST,
    header::CONNECTION,
    header::CONTENT_LENGTH,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Response headers of Google's hop, or that would reach past it.
const RESPONSE_HEADERS_KEPT: &[HeaderName] = &[
    header::CONNECTION,
    header::CONTENT_LENGTH,
    header::TRANSFER_ENCODING,
    header::SET_COOKIE,
    header::ALT_SVC,
];

/// Forwards one call, and once more with a fresh token if Google answers
/// 401 to a token the server had kept.
async fn forward(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((host, path)) = split(uri.path()) else {
        return refused(
            StatusCode::NOT_FOUND,
            "rho_invalid_url: call /<host>/<path>, such as /www.googleapis.com/drive/v3/files",
        );
    };
    if !is_google(host) {
        return refused(
            StatusCode::FORBIDDEN,
            &format!("rho_host_unavailable: {host} is not a host under googleapis.com"),
        );
    }
    let mut url = match state.endpoints.api_base(host) {
        Ok(url) => url,
        Err(error) => return refused(StatusCode::BAD_REQUEST, &format!("{error:#}")),
    };
    url.set_path(path);
    url.set_query(uri.query());
    let mut forwarded = headers;
    for name in REQUEST_HEADERS_KEPT {
        forwarded.remove(name);
    }
    let mut stale = None;
    loop {
        let token = match access_token(&state, stale.as_deref()).await {
            Ok(token) => token,
            Err(refusal) => return refused(refusal.status, &refusal.message),
        };
        let response = state
            .client
            .request(method.clone(), url.clone())
            .headers(forwarded.clone())
            .bearer_auth(&token)
            .body(body.clone())
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return refused(
                    StatusCode::BAD_GATEWAY,
                    &format!("rho_google_unreachable: {error}"),
                );
            }
        };
        if response.status() == StatusCode::UNAUTHORIZED && stale.is_none() {
            stale = Some(token);
            continue;
        }
        return relay(response);
    }
}

/// `/<host>/<path>` as the host and `/<path>`, still percent-encoded.
fn split(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix('/')?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    (!host.is_empty()).then_some((host, if path.is_empty() { "/" } else { path }))
}

/// A host name under `googleapis.com`, in lower case, without a port or
/// user info: the token goes only to Google.
fn is_google(host: &str) -> bool {
    let Some(name) = host.strip_suffix(".googleapis.com") else {
        return false;
    };
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

fn relay(response: reqwest::Response) -> Response {
    let status = response.status();
    let mut headers = response.headers().clone();
    for name in RESPONSE_HEADERS_KEPT {
        headers.remove(name);
    }
    let mut relayed = Response::new(Body::from_stream(response.bytes_stream()));
    *relayed.status_mut() = status;
    *relayed.headers_mut() = headers;
    relayed
}

/// The kept access token, unless it is `stale`, about to expire, or from
/// a replaced grant; otherwise a fresh one from the grant.
async fn access_token(state: &AppState, stale: Option<&str>) -> Result<String, Refusal> {
    let mut kept = state.token.lock().await;
    let read = |name| {
        (state.read_secret)(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let (Some(id), Some(secret), Some(refresh_token)) =
        (read(CLIENT_ID), read(CLIENT_SECRET), read(REFRESH_TOKEN))
    else {
        return Err(Refusal {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "rho_no_google_grant: run `rho google init` on the agent host".to_owned(),
        });
    };
    if let Some(token) = &*kept
        && token.grant == refresh_token
        && Some(token.value.as_str()) != stale
        && Instant::now() < token.refresh_at
    {
        return Ok(token.value.clone());
    }
    let client = oauth::OAuthClient { id, secret };
    let tokens = oauth::refresh(&state.client, &state.endpoints, &client, &refresh_token)
        .await
        .map_err(|error| {
            tracing::warn!(error = %format!("{error:#}"), "refreshing the Google access token failed");
            Refusal {
                status: StatusCode::BAD_GATEWAY,
                message: format!("rho_google_unauthorized: {error:#}; run `rho google init` again"),
            }
        })?;
    let token = AccessToken {
        refresh_at: Instant::now() + tokens.expires_in.saturating_sub(EXPIRY_MARGIN),
        value: tokens.access_token,
        grant: refresh_token,
    };
    *kept = Some(token.clone());
    Ok(token.value)
}

/// Why the server cannot get an access token.
struct Refusal {
    status: StatusCode,
    message: String,
}

/// A refusal of the server's own, shaped like Google's errors.
fn refused(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({ "error": { "code": status.as_u16(), "message": message } })),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
