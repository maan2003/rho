use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt as _;
use reqwest::{Client, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::AppError;

pub type TokenProvider = Arc<dyn Fn() -> Result<String> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub token_provider: TokenProvider,
    pub github_api_url: Url,
}

impl AppState {
    const MAX_JSON_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

    pub(crate) fn github_git_url(&self, owner: &str, repo: &str, endpoint: &str) -> Result<Url> {
        let mut url = self.github_api_url.clone();
        if url.host_str() == Some("api.github.com") {
            url.set_host(Some("github.com"))?;
            url.set_path("");
        }
        let repo = format!("{repo}.git");
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("GitHub URL cannot be a base"))?;
        segments.extend([owner, &repo]);
        segments.extend(endpoint.split('/'));
        drop(segments);
        Ok(url)
    }

    pub(crate) async fn get_token(&self) -> Result<String> {
        (self.token_provider)()
            .map(|token| token.trim().to_owned())
            .and_then(|token| {
                if token.is_empty() {
                    anyhow::bail!("no GITHUB_TOKEN configured");
                }
                Ok(token)
            })
    }

    pub async fn execute_graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let token = self.get_token().await?;

        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let mut url = self.github_api_url.clone();
        url.set_path("/graphql");
        url.set_query(None);
        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", token))
            .header("User-Agent", "octo")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let bytes = Self::bounded_response_bytes(resp).await?;
        if !status.is_success() {
            anyhow::bail!(
                "GitHub GraphQL returned {}: {}",
                status,
                String::from_utf8_lossy(&bytes)
            );
        }
        let result: T = serde_json::from_slice(&bytes)?;
        Ok(result)
    }

    async fn bounded_response_bytes(mut response: reqwest::Response) -> Result<bytes::Bytes> {
        let mut body = bytes::BytesMut::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                body.len().saturating_add(chunk.len()) <= Self::MAX_JSON_RESPONSE_BYTES,
                "GitHub JSON response exceeded {} bytes",
                Self::MAX_JSON_RESPONSE_BYTES
            );
            body.extend_from_slice(&chunk);
        }
        Ok(body.freeze())
    }

    fn build_url(&self, segments: &[&str], query: Option<&str>) -> Url {
        let mut url = self.github_api_url.clone();
        url.path_segments_mut()
            .expect("github_api_url cannot be base")
            .extend(segments);
        if let Some(q) = query {
            url.set_query(Some(q));
        }
        url
    }

    fn github_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            format!("Bearer {}", token).parse().unwrap(),
        );
        headers.insert("Accept", "application/vnd.github+json".parse().unwrap());
        headers.insert("User-Agent", "octo".parse().unwrap());
        headers.insert("X-GitHub-Api-Version", "2022-11-28".parse().unwrap());
        headers
    }

    fn reqwest_to_response(
        resp: reqwest::Response,
    ) -> impl std::future::Future<Output = Result<Response, AppError>> {
        const MAX_PROXY_BYTES: usize = 48 * 1024 * 1024;
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
        let mut headers = HeaderMap::new();
        if let Some(ct) = resp.headers().get("content-type") {
            headers.insert("content-type", ct.clone());
        }
        async move {
            let mut received = 0_usize;
            let stream = resp.bytes_stream().map(move |chunk| match chunk {
                Ok(chunk) => {
                    received = received.saturating_add(chunk.len());
                    if received > MAX_PROXY_BYTES {
                        Err(std::io::Error::other(
                            "GitHub response exceeded the 48 MiB proxy limit",
                        ))
                    } else {
                        Ok(chunk)
                    }
                }
                Err(error) => Err(std::io::Error::other(error)),
            });
            let body = Body::from_stream(stream);
            Ok((status, headers, body).into_response())
        }
    }

    pub async fn proxy_github_get(
        &self,
        segments: &[&str],
        query: Option<&str>,
    ) -> Result<Response, AppError> {
        let token = self.get_token().await?;
        let url = self.build_url(segments, query);

        let resp = self
            .client
            .get(url)
            .headers(Self::github_headers(&token))
            .send()
            .await?;

        Self::reqwest_to_response(resp).await
    }

    pub async fn proxy_github_post(&self, segments: &[&str]) -> Result<Response, AppError> {
        let token = self.get_token().await?;
        let url = self.build_url(segments, None);

        let resp = self
            .client
            .post(url)
            .headers(Self::github_headers(&token))
            .send()
            .await?;

        Self::reqwest_to_response(resp).await
    }

    pub async fn github_get_json(
        &self,
        segments: &[&str],
        query: Option<&str>,
    ) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let url = self.build_url(segments, query);

        let resp = self
            .client
            .get(url)
            .headers(Self::github_headers(&token))
            .send()
            .await?;

        let status = resp.status();
        let bytes = Self::bounded_response_bytes(resp).await?;
        if !status.is_success() {
            anyhow::bail!(
                "GitHub returned {}: {}",
                status,
                String::from_utf8_lossy(&bytes)
            );
        }
        let result = serde_json::from_slice(&bytes)?;
        Ok(result)
    }

    /// Fetch a bounded GitHub REST collection. The bound prevents a watched
    /// PR with pathological history from monopolizing the daemon.
    pub async fn github_get_json_pages(
        &self,
        segments: &[&str],
        max_pages: usize,
    ) -> Result<serde_json::Value> {
        let mut values = Vec::new();
        for page in 1..=max_pages {
            let value = self
                .github_get_json(segments, Some(&format!("per_page=100&page={page}")))
                .await?;
            let page_values = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("GitHub collection response was not an array"))?;
            let complete = page_values.len() < 100;
            values.extend(page_values.iter().cloned());
            if complete {
                break;
            }
            if page == max_pages {
                anyhow::bail!(
                    "GitHub collection exceeds the configured {max_pages}-page safety bound"
                );
            }
        }
        Ok(serde_json::Value::Array(values))
    }

    pub async fn github_post_json<B: Serialize>(
        &self,
        segments: &[&str],
        body: &B,
    ) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let url = self.build_url(segments, None);

        let resp = self
            .client
            .post(url)
            .headers(Self::github_headers(&token))
            .json(body)
            .send()
            .await?;

        let status = resp.status();
        let bytes = Self::bounded_response_bytes(resp).await?;
        if !status.is_success() {
            anyhow::bail!(
                "GitHub returned {}: {}",
                status,
                String::from_utf8_lossy(&bytes)
            );
        }
        let result = serde_json::from_slice(&bytes)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reqwest::Url;

    use super::AppState;

    fn build_url(base: &str, segments: &[&str]) -> String {
        let mut url = Url::parse(base).unwrap();
        url.path_segments_mut()
            .expect("cannot be base")
            .extend(segments);
        url.to_string()
    }

    #[test]
    fn test_simple_segments() {
        assert_eq!(
            build_url("https://api.github.com", &["repos", "owner", "repo"]),
            "https://api.github.com/repos/owner/repo"
        );
    }

    #[test]
    fn test_encodes_slashes() {
        assert_eq!(
            build_url("https://api.github.com", &["repos", "owner/evil", "repo"]),
            "https://api.github.com/repos/owner%2Fevil/repo"
        );
    }

    #[test]
    fn github_git_url_preserves_endpoint_path_segments() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let state = AppState {
            client: reqwest::Client::new(),
            token_provider: Arc::new(|| Ok("token".to_owned())),
            github_api_url: Url::parse("https://api.github.com").unwrap(),
        };

        assert_eq!(
            state
                .github_git_url("fedimint", "fedimint", "info/refs")
                .unwrap()
                .as_str(),
            "https://github.com/fedimint/fedimint.git/info/refs"
        );
    }

    #[test]
    fn test_encodes_special_chars() {
        assert_eq!(
            build_url("https://api.github.com", &["repos", "owner", "repo%name"]),
            "https://api.github.com/repos/owner/repo%25name"
        );
    }

    #[test]
    fn test_encodes_spaces() {
        assert_eq!(
            build_url("https://api.github.com", &["repos", "my owner", "repo"]),
            "https://api.github.com/repos/my%20owner/repo"
        );
    }

    #[test]
    fn test_preserves_base_path() {
        assert_eq!(
            build_url("https://api.github.com/v1", &["repos", "owner"]),
            "https://api.github.com/v1/repos/owner"
        );
    }
}
