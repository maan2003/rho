//! Raw read-only Drive and Meet HTTP on a host-owned Unix socket.
//! The caller chooses a Google resource, never a token endpoint or an origin.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, ensure};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

pub const GOOGLE_AUTHORIZED_USER: &str = "GOOGLE_AUTHORIZED_USER";

#[derive(Deserialize, Serialize)]
enum CredentialType {
    #[serde(rename = "authorized_user")]
    AuthorizedUser,
}

#[derive(Deserialize, Serialize)]
struct Grant {
    #[serde(rename = "type")]
    kind: CredentialType,
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

impl Grant {
    fn parse(input: &str) -> Result<Self> {
        // Serde errors can quote input. Do not expose any of it to callers.
        let grant: Self = serde_json::from_str(input)
            .map_err(|_| anyhow!("expected Google authorized_user credentials with a client ID, client secret and refresh token"))?;
        ensure!(
            [&grant.client_id, &grant.client_secret, &grant.refresh_token]
                .iter()
                .all(|s| !s.trim().is_empty()),
            "Google authorized_user credential fields must not be empty"
        );
        Ok(grant)
    }
}

/// Validate at import and discard all other fields, including access tokens and
/// token URIs.
pub fn authorized_user_json(input: &str) -> Result<String> {
    Ok(serde_json::to_string(&Grant::parse(input)?)?)
}

struct Endpoints {
    token: Url,
    drive: Url,
    meet: Url,
}

struct CachedToken {
    grant: String,
    bearer: String,
    expires: Instant,
}

struct Proxy {
    endpoints: Endpoints,
    client: Client,
    read: Box<dyn Fn() -> Result<String> + Send + Sync>,
    cached: Mutex<Option<CachedToken>>,
}

type Failure = (StatusCode, &'static str);
const NO_GRANT: Failure = (
    StatusCode::SERVICE_UNAVAILABLE,
    "Google is not configured. Run `rho google init` on the agent host.",
);
const AUTH_FAILED: Failure = (
    StatusCode::BAD_GATEWAY,
    "Google OAuth refresh failed. Check the grant installed with `rho google init`.",
);
const BAD_TARGET: Failure = (
    StatusCode::BAD_REQUEST,
    "Expected a Drive v3 or Meet conference record resource.",
);

impl Proxy {
    async fn bearer(&self) -> Result<String, Failure> {
        // One refresh at a time. A new operator grant takes effect without a
        // restart.
        let mut cache = self.cached.lock().await;
        let raw = (self.read)().map_err(|_| NO_GRANT)?;
        let now = Instant::now();
        if let Some(c) = cache.as_ref()
            && c.grant == raw
            && now < c.expires
        {
            return Ok(c.bearer.clone());
        }
        *cache = None;
        let grant = Grant::parse(&raw).map_err(|_| NO_GRANT)?;
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            expires_in: u64,
        }
        let response = self
            .client
            .post(self.endpoints.token.clone())
            .timeout(Duration::from_secs(30))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &grant.client_id),
                ("client_secret", &grant.client_secret),
                ("refresh_token", &grant.refresh_token),
            ])
            .send()
            .await
            .map_err(|_| AUTH_FAILED)?;
        if !response.status().is_success() {
            return Err(AUTH_FAILED);
        }
        let token: Token = response.json().await.map_err(|_| AUTH_FAILED)?;
        if token.access_token.is_empty() {
            return Err(AUTH_FAILED);
        }
        let expires = now
            .checked_add(Duration::from_secs(token.expires_in.saturating_sub(30)))
            .ok_or(AUTH_FAILED)?;
        *cache = Some(CachedToken {
            grant: raw,
            expires,
            bearer: token.access_token.clone(),
        });
        Ok(token.access_token)
    }

    fn target(&self, uri: &Uri) -> Result<Url, Failure> {
        let path = uri.path();
        let base = if path.starts_with("/drive/v3/") {
            &self.endpoints.drive
        } else if path == "/v2/conferenceRecords" || path.starts_with("/v2/conferenceRecords/") {
            &self.endpoints.meet
        } else {
            return Err(BAD_TARGET);
        };
        // Google resource names on these routes are ASCII path segments.
        // Reject ambiguous spellings rather than let another URL parser
        // normalize them.
        if uri.authority().is_some()
            || path[1..].split('/').any(|s| {
                s.is_empty()
                    || s == "."
                    || s == ".."
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.~=".contains(&b))
            })
        {
            return Err(BAD_TARGET);
        }
        let mut url = base.clone();
        url.set_path(path);
        url.set_query(uri.query());
        // Accept native API fields. Do not let system parameters supply
        // credentials, tunnel HTTP headers, or select another protocol.
        if url.query_pairs().any(|(k, _)| {
            !k.bytes().all(|b| b.is_ascii_alphanumeric())
                || matches!(
                    k.as_ref(),
                    "key" | "bearerToken" | "oauthToken" | "accessToken"
                )
        }) {
            return Err((
                StatusCode::BAD_REQUEST,
                "Unsupported Google query parameter.",
            ));
        }
        Ok(url)
    }
}

pub fn router(read: impl Fn() -> Result<String> + Send + Sync + 'static) -> Router {
    router_at(
        Endpoints {
            token: Url::parse("https://oauth2.googleapis.com/token").unwrap(),
            drive: Url::parse("https://www.googleapis.com").unwrap(),
            meet: Url::parse("https://meet.googleapis.com").unwrap(),
        },
        read,
    )
}

fn router_at(
    endpoints: Endpoints,
    read: impl Fn() -> Result<String> + Send + Sync + 'static,
) -> Router {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("static Google HTTP client configuration");
    Router::new().fallback(proxy).with_state(Arc::new(Proxy {
        endpoints,
        client,
        read: Box::new(read),
        cached: Mutex::default(),
    }))
}

async fn proxy(
    State(proxy): State<Arc<Proxy>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    if method != Method::GET {
        return Err((StatusCode::METHOD_NOT_ALLOWED, "Only GET is supported."));
    }
    let url = proxy.target(&uri)?;
    let bearer = proxy.bearer().await?;
    let mut request = proxy.client.get(url).bearer_auth(&bearer);
    for key in [
        header::RANGE,
        header::IF_NONE_MATCH,
        header::HeaderName::from_static("x-goog-drive-resource-keys"),
    ] {
        if let Some(value) = headers.get(&key) {
            request = request.header(key, value);
        }
    }
    let upstream = request
        .send()
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "Google request failed."))?;
    let status = upstream.status();
    if status == StatusCode::UNAUTHORIZED {
        let mut cache = proxy.cached.lock().await;
        if cache.as_ref().is_some_and(|c| c.bearer == bearer) {
            *cache = None;
        }
    }
    if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
        return Err((StatusCode::BAD_GATEWAY, "Google redirect not followed."));
    }
    let mut response_headers = HeaderMap::new();
    for key in [
        header::CONTENT_TYPE,
        header::CONTENT_RANGE,
        header::ETAG,
        header::RETRY_AFTER,
    ] {
        if let Some(value) = upstream.headers().get(&key) {
            response_headers.insert(key, value.clone());
        }
    }
    Ok((
        status,
        response_headers,
        Body::from_stream(upstream.bytes_stream()),
    )
        .into_response())
}

#[cfg(test)]
mod tests;
