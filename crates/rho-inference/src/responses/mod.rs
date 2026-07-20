//! OpenAI Responses API provider building blocks.
//!
//! The request-body shape and tool-name encoding are adapted from Tau's
//! Responses backend. Tau's protocol messages, event bus, VCR, WebSocket pool,
//! and HTTP loop are intentionally not copied into this crate; `rho-agent` or a
//! fork should own those runtime policies.

pub(crate) mod oauth;
mod session;
#[cfg(test)]
mod tests;
mod wire;
mod ws;

pub use oauth::{InferenceAuth, ResolvedOAuth};
pub use session::{InferenceSession, PromptCacheKey};
pub use wire::OpenAiResponsesProviderData;

pub(crate) const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub(crate) const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";

fn responses_url(base_url: &str) -> String {
    format!("{}/codex/responses", base_url.trim_end_matches('/'))
}

fn is_stale_previous_response_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("previous_response")
        || message.contains("previous response")
        || message.contains("response not found")
}
