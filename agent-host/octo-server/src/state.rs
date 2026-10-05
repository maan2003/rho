use std::sync::Arc;

use anyhow::Result;
use reqwest::{Client, Url};

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
        let repo = format!("{repo}.git");
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("GitHub URL cannot be a base"))?;
        segments.extend([owner, &repo]);
        segments.extend(endpoint.split('/'));
        drop(segments);
        Ok(url)
    }

    pub(crate) fn github_upload_url(&self) -> Url {
        let mut url = self.github_api_url.clone();
        if url.host_str() == Some("api.github.com") {
            url.set_host(Some("uploads.github.com"))
                .expect("fixed GitHub upload host is valid");
        }
        url
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reqwest::Url;

    use super::AppState;

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
    fn github_upload_url_uses_the_fixed_upload_origin() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let state = AppState {
            client: reqwest::Client::new(),
            token_provider: Arc::new(|| Ok("token".to_owned())),
            github_api_url: Url::parse("https://api.github.com").unwrap(),
        };
        assert_eq!(
            state.github_upload_url().as_str(),
            "https://uploads.github.com/"
        );
        assert_eq!(state.github_api_url.as_str(), "https://api.github.com/");
    }
}
