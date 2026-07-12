use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use reqwest::{Client, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::AppError;

#[allow(dead_code)]
const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";

pub type TokenProvider = Arc<dyn Fn() -> Result<String> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub token_provider: TokenProvider,
    pub github_api_url: Url,
}

impl AppState {
    pub(crate) fn github_git_url(&self, owner: &str, repo: &str, endpoint: &str) -> Result<Url> {
        let mut url = self.github_api_url.clone();
        if url.host_str() == Some("api.github.com") {
            url.set_host(Some("github.com"))?;
            url.set_path("");
        }
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("GitHub URL cannot be a base"))?
            .extend([owner, &format!("{repo}.git"), endpoint]);
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

    #[allow(dead_code)]
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

        let resp = self
            .client
            .post(GITHUB_GRAPHQL_URL)
            .header("Authorization", format!("Bearer {}", token))
            .header("User-Agent", "octo")
            .json(&body)
            .send()
            .await?;

        let result: T = resp.json().await?;
        Ok(result)
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
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
        let mut headers = HeaderMap::new();
        if let Some(ct) = resp.headers().get("content-type") {
            headers.insert("content-type", ct.clone());
        }
        async move {
            let body = Body::from(resp.bytes().await?);
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
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub returned {}: {}", status, text);
        }

        let result = resp.json().await?;
        Ok(result)
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
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub returned {}: {}", status, text);
        }

        let result = resp.json().await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use reqwest::Url;

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
