//! One-shot display-name generation for agents.
//!
//! Rho owns titling instead of relying on any runtime's transcript side
//! channel: Claude Code only writes `ai-title` entries for interactive
//! sessions, and the Rho runtime has no equivalent at all. Generating here
//! keeps one policy for both runtimes and stores the result in our db.

use std::sync::Arc;

use rho_core::{
    ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    PendingInferenceResponse,
};
use rho_inference::{InferenceAuth, InferenceSession, PromptCacheKey};

const INSTRUCTIONS: &str = "Write a kebab-case title (at most 25 characters) for a coding \
agent's conversation. Name the subject — the component, feature, or problem — not the kind of \
task, because the task may change (exploration becomes implementation) while the subject stays. \
Never use task words such as explore, investigate, implement, propose, alternatives, options. \
Exception: code review requests are titled review-<subject>, since a review stays a review.

Examples:
\"explore alternatives to elision breaking when claude emits a second final message\" -> claude-elision
\"implement retry with backoff for the upload endpoint\" -> upload-retry
\"why is the topic rail flickering on resize?\" -> topic-rail-flicker
\"review my changes to the daemon socket protocol\" -> review-daemon-socket

Respond with only the title: lowercase words joined by hyphens, no quotes.";

/// The prompt excerpt is capped so a pasted wall of text doesn't balloon the
/// title request; the opening of a message is what names it anyway.
const MAX_PROMPT_CHARS: usize = 2000;

const MAX_TITLE_CHARS: usize = 30;

pub async fn generate_title(auth: InferenceAuth, user_message: &str) -> anyhow::Result<String> {
    let mut session = InferenceSession::new_title(auth, PromptCacheKey::generate());
    session.request(InferenceRequest {
        instructions: Arc::from(INSTRUCTIONS),
        input: vec![Arc::new(ContextBlock::UserMessage {
            sender: rho_core::MessageSender::User,
            content: vec![ContentPart::Text {
                text: truncate_chars(user_message, MAX_PROMPT_CHARS).to_owned(),
            }],
        })],
        agent_id_labels: std::collections::BTreeMap::new(),
        tools: Arc::from([]),
    });
    let mut pending = PendingInferenceResponse::default();
    let items = loop {
        match session.run().await {
            InferenceEvent::ContextItem { index, event } => pending.apply(index, event),
            InferenceEvent::Finished { .. } => break pending.finish()?,
            InferenceEvent::Failed { error } => {
                anyhow::bail!("title inference failed: {error:#}")
            }
            InferenceEvent::TemporaryFailure { .. } => {
                pending = PendingInferenceResponse::default();
            }
            InferenceEvent::RequestSent | InferenceEvent::StreamingStarted => {}
        }
    };
    let text = items
        .iter()
        .filter_map(|item| match item {
            InferenceResponseItem::AssistantMessage { content, .. } => Some(
                content
                    .iter()
                    .map(|ContentPart::Text { text }| text.as_str())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    sanitize(&text).ok_or_else(|| anyhow::anyhow!("title inference returned no usable text"))
}

/// First non-empty line, forced into kebab-case, length-capped.
fn sanitize(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let kebab = line
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if kebab.is_empty() {
        return None;
    }
    let capped = truncate_chars(&kebab, MAX_TITLE_CHARS).trim_end_matches('-');
    Some(capped.to_owned())
}

fn truncate_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((offset, _)) => &text[..offset],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitize_keeps_kebab_case() {
        assert_eq!(
            sanitize("fix-login-redirect").as_deref(),
            Some("fix-login-redirect")
        );
    }

    #[test]
    fn sanitize_kebab_cases_prose() {
        assert_eq!(
            sanitize("\n  \"Fix Login redirect.\"\nsecond line").as_deref(),
            Some("fix-login-redirect")
        );
    }

    #[test]
    fn sanitize_rejects_empty() {
        assert_eq!(sanitize("  \n \"\" "), None);
    }

    #[test]
    fn sanitize_caps_length_without_trailing_hyphen() {
        let title = sanitize("very-long-agent-title-that-keeps-going").unwrap();
        assert!(title.chars().count() <= 30);
        assert!(!title.ends_with('-'));
        assert_eq!(title, "very-long-agent-title-that-kee");
    }
}
