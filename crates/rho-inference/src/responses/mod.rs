//! OpenAI Responses API provider building blocks.
//!
//! The request-body shape and tool-name encoding are adapted from Tau's
//! Responses backend. Tau's protocol messages, event bus, VCR, WebSocket pool,
//! and HTTP loop are intentionally not copied into this crate; `rho-agent` or a
//! fork should own those runtime policies.

use std::collections::BTreeMap;

use anyhow::Result;
use futures::future::BoxFuture;
use rho_core::{IInferenceSession, InferenceRequest, InferenceUpdate, ToolSpec};

pub(crate) mod oauth;
mod session;
#[cfg(test)]
mod tests;
mod wire;
mod ws;

pub use oauth::InferenceAuth;
pub use session::InferenceSession;

pub(crate) const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub(crate) const DEFAULT_MODEL: &str = "gpt-5.5";
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 258_400;
pub(crate) const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";

/// How the Responses API should compact long threads. `Default` lets the
/// provider pick the threshold; `Threshold` pins an explicit token count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Compaction {
    Default,
    Threshold(u64),
}

fn responses_url(base_url: &str) -> String {
    format!("{}/codex/responses", base_url.trim_end_matches('/'))
}

impl IInferenceSession for InferenceSession {
    fn request(&mut self, request: InferenceRequest) {
        InferenceSession::request(self, request);
    }

    fn run(&mut self) -> BoxFuture<'_, Result<InferenceUpdate>> {
        Box::pin(InferenceSession::run(self))
    }

    fn abort(&mut self) {
        InferenceSession::abort(self);
    }
}

fn tool_name_map(tools: &[ToolSpec]) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for tool in tools {
        let wire_name = encode_tool_name(&tool.name);
        match names.get(&wire_name) {
            Some(existing) if existing != &tool.name => {
                names.insert(wire_name.clone(), wire_name);
            }
            Some(_) => {}
            None => {
                names.insert(wire_name, tool.name.clone());
            }
        }
    }
    names
}

fn encode_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn is_stale_previous_response_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("previous_response")
        || message.contains("previous response")
        || message.contains("response not found")
}
