//! Gemini, through an Antigravity account's Cloud Code API.
//!
//! One `generateContent` call per step. Cloud Code sends a function call's
//! arguments whole even when streaming, so a step's code arrives at once
//! and nothing streams. The account's login stays on the agent host
//! ([`auth`]); a worker asks it for a token before each step.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use rho_agent::inference as agent;
use serde_json::Value;

mod auth;
mod wire;

pub(crate) use auth::{GeminiAuth, Login};

use crate::config::{InferenceModel, InferenceProfile};

const ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
/// Cloud Code serves newer models only to clients at least this new; with
/// an older version, Gemini 4 is missing and generation is refused. The
/// version is Antigravity's (its updater manifest names the latest).
const USER_AGENT: &str = "antigravity/hub/2.33.0 (aidev_client; os_type=darwin; arch=arm64)";
/// Long enough for the slowest step at the highest thinking level.
const STEP_TIMEOUT: Duration = Duration::from_secs(10 * 60);

fn http() -> reqwest::Client {
    crate::ensure_crypto_provider();
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .expect("HTTP client")
}

/// Cloud Code's error, by its status and Google's message.
fn refusal(status: reqwest::StatusCode, body: &[u8]) -> anyhow::Error {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|body| body.pointer("/error/message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&body[..body.len().min(2048)]).into_owned());
    let error = format!("Gemini refused the request: {status}: {message}");
    if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
        agent::Retryable(error).into()
    } else {
        anyhow::anyhow!(error)
    }
}

/// A transport failure: the request may not have reached Gemini, so trying
/// again is safe.
fn unreached(error: reqwest::Error) -> anyhow::Error {
    agent::Retryable(format!("reaching Gemini: {error}")).into()
}

async fn post(
    http: &reqwest::Client,
    method: &str,
    token: &str,
    body: &Value,
) -> anyhow::Result<bytes::Bytes> {
    let response = http
        .post(format!("{ENDPOINT}:{method}"))
        .bearer_auth(token)
        .json(body)
        .timeout(STEP_TIMEOUT)
        .send()
        .await
        .map_err(unreached)?;
    let status = response.status();
    let body = response.bytes().await.map_err(unreached)?;
    if !status.is_success() {
        return Err(refusal(status, &body));
    }
    Ok(body)
}

/// The project Cloud Code bills this account's requests to. Antigravity
/// asks for it at sign-in; any other project refuses generation.
async fn project(http: &reqwest::Client, token: &str) -> anyhow::Result<String> {
    let body = serde_json::json!({"metadata": {
        "ideType": "ANTIGRAVITY",
        "platform": "PLATFORM_UNSPECIFIED",
        "pluginType": "GEMINI",
    }});
    let answer: Value = serde_json::from_slice(&post(http, "loadCodeAssist", token, &body).await?)?;
    answer
        .get("cloudaicompanionProject")
        .and_then(|project| project.as_str().or_else(|| project.get("id")?.as_str()))
        .map(str::to_owned)
        .context("Cloud Code named no project for this Antigravity account")
}

pub(crate) struct Session {
    inference: crate::Inference,
    http: reqwest::Client,
    model: InferenceModel,
    profile: InferenceProfile,
}

impl Session {
    pub(crate) fn new(
        inference: crate::Inference,
        profile: InferenceProfile,
        model: InferenceModel,
    ) -> Arc<Self> {
        Arc::new(Self {
            inference,
            http: http(),
            model,
            profile,
        })
    }

    async fn step(&self, request: agent::Request) -> anyhow::Result<agent::Step> {
        let auth = self.inference.gemini_auth().await?;
        let body = wire::body(
            self.model,
            self.profile.effort,
            &auth.project,
            &request.instructions,
            &request.items,
            request.cache_key,
        );
        let answer = post(&self.http, "generateContent", &auth.access_token, &body).await?;
        wire::step(&answer, wire::compacting(&request.items))
    }
}

impl agent::Session for Session {
    fn start(&self, request: agent::Request) -> agent::Response {
        let (events, response) = tokio::sync::mpsc::unbounded_channel();
        let session = Self {
            inference: self.inference.clone(),
            http: self.http.clone(),
            model: self.model,
            profile: self.profile,
        };
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = events.closed() => {}
                result = session.step(request) => {
                    let _ = events.send(match result {
                        Ok(step) => agent::Event::Completed(step),
                        Err(error) => agent::Event::Failed(error),
                    });
                }
            }
        });
        response
    }
}

#[cfg(test)]
mod tests;
