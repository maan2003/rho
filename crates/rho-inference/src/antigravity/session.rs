use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rho_core::{
    AppendString, ContentPart, ContextItemEvent, InferenceEvent, InferenceRequest,
    InferenceResponseItem, StreamingContextItem,
};

use super::auth::AntigravityAuthFile;
use super::wire::{self, ParsedResponse};
use crate::config::InferenceProfile;
use crate::responses::PromptCacheKey;

const BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const RETRY_WINDOW: Duration = Duration::from_secs(8 * 60 * 60);

pub(crate) struct AntigravitySession {
    profile: InferenceProfile,
    prompt_cache_key: PromptCacheKey,
    events: tokio::sync::mpsc::UnboundedReceiver<(u64, InferenceEvent)>,
    events_tx: tokio::sync::mpsc::UnboundedSender<(u64, InferenceEvent)>,
    active: Option<tokio::task::JoinHandle<()>>,
    awaiting: Option<u64>,
    epochs: u64,
}

impl AntigravitySession {
    pub(crate) fn new(profile: InferenceProfile, prompt_cache_key: PromptCacheKey) -> Self {
        let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
        Self {
            profile,
            prompt_cache_key,
            events,
            events_tx,
            active: None,
            awaiting: None,
            epochs: 0,
        }
    }

    pub(crate) fn set_profile(&mut self, profile: InferenceProfile) {
        self.profile = profile;
    }

    pub(crate) fn prompt_cache_key(&self) -> PromptCacheKey {
        self.prompt_cache_key
    }

    pub(crate) fn set_prompt_cache_key(&mut self, key: PromptCacheKey) {
        self.prompt_cache_key = key;
    }

    pub(crate) fn has_active_request(&self) -> bool {
        self.awaiting.is_some()
    }

    pub(crate) fn context_window(&self) -> Option<u64> {
        Some(1_048_576)
    }

    pub(crate) fn auto_compact_token_limit(&self) -> Option<u64> {
        None
    }

    pub(crate) fn request(&mut self, request: InferenceRequest) {
        self.abort();
        self.epochs = self.epochs.saturating_add(1);
        let epoch = self.epochs;
        self.awaiting = Some(epoch);
        let events = self.events_tx.clone();
        self.active = Some(tokio::spawn(async move {
            if let Err(error) = run_turn(epoch, &events, request).await {
                let _ = events.send((
                    epoch,
                    InferenceEvent::Failed {
                        error: Arc::new(error),
                    },
                ));
            }
        }));
    }

    pub(crate) fn abort(&mut self) {
        self.awaiting = None;
        if let Some(active) = self.active.take() {
            active.abort();
        }
    }

    pub(crate) async fn run(&mut self) -> InferenceEvent {
        loop {
            let Some((epoch, event)) = self.events.recv().await else {
                std::future::pending::<()>().await;
                unreachable!()
            };
            if self.awaiting != Some(epoch) {
                continue;
            }
            if matches!(
                event,
                InferenceEvent::Finished { .. } | InferenceEvent::Failed { .. }
            ) {
                self.awaiting = None;
                self.active = None;
            }
            return event;
        }
    }
}

impl Drop for AntigravitySession {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            active.abort();
        }
    }
}

async fn run_turn(
    epoch: u64,
    events: &tokio::sync::mpsc::UnboundedSender<(u64, InferenceEvent)>,
    request: InferenceRequest,
) -> Result<()> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let started = Instant::now();
    let mut attempt = 0;
    loop {
        // A long transient retry window may cross the access-token expiry.
        // Resolve under the credential lock for each attempt so refresh stays
        // serialized and a retry never reuses a stale bearer.
        let auth = resolve_auth().await?;
        // Keep the bearer and project envelope from one credential snapshot;
        // `rho auth antigravity add` may replace the profile between attempts.
        let body = wire::build_request(&request_id, &auth.project_id, &request)?;
        let _ = events.send((epoch, InferenceEvent::RequestSent));
        match send(&auth.access_token, &body).await {
            Ok(parsed) => {
                let _ = events.send((epoch, InferenceEvent::StreamingStarted));
                emit_response(epoch, events, parsed)?;
                return Ok(());
            }
            Err(error) if error.transient && started.elapsed() < RETRY_WINDOW => {
                attempt += 1;
                let remaining = RETRY_WINDOW.saturating_sub(started.elapsed());
                let delay = crate::responses::transient_backoff(attempt).min(remaining);
                let retrying_at = Instant::now() + delay;
                let _ = events.send((
                    epoch,
                    InferenceEvent::TemporaryFailure {
                        error: Arc::new(anyhow::anyhow!(error.message)),
                        retrying_at,
                    },
                ));
                tokio::time::sleep(delay).await;
            }
            Err(error) => anyhow::bail!(error.message),
        }
    }
}

async fn resolve_auth() -> Result<super::auth::ResolvedAntigravityAuth> {
    tokio::task::spawn_blocking(|| AntigravityAuthFile::open_default()?.resolve())
        .await
        .context("joining Antigravity credential resolver")?
        .map_err(Into::into)
}

struct SendError {
    message: String,
    transient: bool,
}

async fn send(access_token: &str, body: &serde_json::Value) -> Result<ParsedResponse, SendError> {
    super::ensure_crypto_provider();
    let url = format!("{BASE_URL}/v1internal:generateContent");
    let mut response = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", "antigravity/cli/1.0.1 linux/amd64")
        .timeout(REQUEST_TIMEOUT)
        .json(body)
        .send()
        .await
        .map_err(|error| SendError {
            message: format!("Antigravity request failed: {error}"),
            transient: error.is_timeout() || error.is_connect() || error.is_request(),
        })?;
    let status = response.status();
    let mut bytes = Vec::new();
    loop {
        let chunk = response.chunk().await.map_err(|error| SendError {
            message: format!("reading Antigravity response: {error}"),
            transient: true,
        })?;
        let Some(chunk) = chunk else { break };
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES as usize {
            return Err(SendError {
                message: "Antigravity response exceeded 8 MiB".to_owned(),
                transient: false,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&bytes[..bytes.len().min(16 * 1024)]);
        return Err(SendError {
            message: format!("Antigravity HTTP {}: {}", status.as_u16(), detail.trim()),
            transient: status.as_u16() == 429 || status.is_server_error(),
        });
    }
    let value = serde_json::from_slice(&bytes).map_err(|error| SendError {
        message: format!("decoding Antigravity response: {error}"),
        transient: false,
    })?;
    wire::parse_response(value).map_err(|error| SendError {
        message: format!("invalid Antigravity response: {error:#}"),
        transient: false,
    })
}

fn emit_response(
    epoch: u64,
    events: &tokio::sync::mpsc::UnboundedSender<(u64, InferenceEvent)>,
    parsed: ParsedResponse,
) -> Result<()> {
    for (index, item) in parsed.items.into_iter().enumerate() {
        let item = streaming_item(item)?;
        let _ = events.send((
            epoch,
            InferenceEvent::ContextItem {
                index,
                event: ContextItemEvent::Update(item),
            },
        ));
        let _ = events.send((
            epoch,
            InferenceEvent::ContextItem {
                index,
                event: ContextItemEvent::Finish,
            },
        ));
    }
    let _ = events.send((
        epoch,
        InferenceEvent::Finished {
            usage: parsed.usage,
            provider_response_id: None,
        },
    ));
    Ok(())
}

fn streaming_item(item: InferenceResponseItem) -> Result<StreamingContextItem> {
    Ok(match item {
        InferenceResponseItem::AssistantMessage {
            provider_specific,
            content,
            phase,
        } => {
            let mut parts = Vec::new();
            for content in content {
                let ContentPart::Text { text } = content else {
                    anyhow::bail!("Antigravity returned an unsupported image")
                };
                let mut append = AppendString::new();
                append.push_str(&text);
                parts.push(append.snapshot());
            }
            StreamingContextItem::AssistantMessage {
                provider_specific,
                content: parts,
                phase,
            }
        }
        InferenceResponseItem::ToolCall {
            provider_specific,
            id,
            name,
            tool_type,
            arguments,
        } => {
            let mut append = AppendString::new();
            append.push_str(&arguments);
            StreamingContextItem::ToolCall {
                provider_specific,
                id,
                name,
                tool_type,
                arguments: append.snapshot(),
            }
        }
        InferenceResponseItem::EncryptedReasoning {
            provider_specific,
            summary,
        } => {
            let summary = summary
                .into_iter()
                .map(|text| {
                    let mut append = AppendString::new();
                    append.push_str(&text);
                    append.snapshot()
                })
                .collect();
            StreamingContextItem::EncryptedReasoning {
                provider_specific,
                summary,
            }
        }
        InferenceResponseItem::RawReasoning { .. }
        | InferenceResponseItem::Compaction { .. }
        | InferenceResponseItem::Unknown { .. } => {
            anyhow::bail!("Antigravity produced an unsupported response item")
        }
    })
}
