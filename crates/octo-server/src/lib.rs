//! Embedded Octo GitHub helper server.
//!
//! Rho runs this in-process on a daemon-owned Unix socket. GitHub tokens are
//! supplied by the daemon from its sealed RAM-only platform secret store; Octo
//! never receives them via argv/env or persists them to disk.

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use reqwest::{Client, Url};
use tokio::net::UnixListener;

mod api;
mod error;
mod state;
mod types;

use state::AppState;
pub use state::TokenProvider;

pub fn router(token_provider: TokenProvider, github_api_url: Url) -> Router {
    let state = Arc::new(AppState {
        client: Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("static Octo HTTP client configuration is valid"),
        token_provider,
        github_api_url,
    });

    Router::new()
        .merge(api::ci::router())
        .merge(api::git::router())
        .merge(api::pr::router())
        .with_state(state)
}

pub async fn serve(
    listener: UnixListener,
    token_provider: TokenProvider,
    github_api_url: Url,
) -> Result<()> {
    axum::serve(listener, router(token_provider, github_api_url)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn token_provider_is_used_without_persistence() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let state = AppState {
            client: Client::new(),
            token_provider: Arc::new(move || {
                seen.fetch_add(1, Ordering::Relaxed);
                Ok("ghp-test".to_owned())
            }),
            github_api_url: Url::parse("https://api.github.com").unwrap(),
        };

        assert_eq!(state.get_token().await.unwrap(), "ghp-test");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
