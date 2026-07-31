//! Runtime-only Luna activity summaries.
//!
//! This is intentionally not an agent transcript. The caller supplies only
//! text messages, receives an optional display label, and drops the whole
//! session with the runtime.

use std::collections::VecDeque;
use std::sync::Arc;

use rho_core::{
    ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    MessageSender, PendingInferenceResponse, ToolOutput, ToolOutputStatus, ToolResult, ToolSpec,
    ToolType, UnixMs,
};
use rho_inference::{Inference, InferenceSession, PromptCacheKey};

const MAX_MESSAGE_BYTES: usize = 1024;
const MAX_TRANSCRIPT_BYTES: usize = 10 * 1024;
const MAX_STATUS_BYTES: usize = 50;
const MAX_TURNS: usize = 8;
const SET_STATUS: &str = "set_status";

const INSTRUCTIONS: &str = "You maintain a tiny activity label for an agent. Read the recent XML \
transcript to infer what the agent is currently doing. When that activity materially changes, call \
set_status with a concise three or four word label. Do not write a normal response.";

#[derive(Clone, Copy)]
pub enum ActivitySpeaker {
    User,
    Agent,
    Assistant,
}

impl ActivitySpeaker {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Assistant => "assistant",
        }
    }
}

struct SourceMessage {
    xml: String,
}

pub struct ActivitySession {
    inference: Inference,
    session: InferenceSession,
    sources: VecDeque<SourceMessage>,
    timeline: Vec<Arc<ContextBlock>>,
    source_bytes: usize,
    turns: usize,
}

impl ActivitySession {
    pub fn new(inference: Inference) -> Self {
        let session = inference.status_session(PromptCacheKey::generate());
        Self {
            inference,
            session,
            sources: VecDeque::new(),
            timeline: Vec::new(),
            source_bytes: 0,
            turns: 0,
        }
    }

    /// Adds one text-only source message. Returns false for empty text.
    pub fn push(&mut self, speaker: ActivitySpeaker, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let start = format!("<message speaker=\"{}\">", speaker.as_str());
        let end = "</message>";
        let xml = format!(
            "{start}{}{end}",
            xml_escape_capped(
                text,
                MAX_MESSAGE_BYTES.saturating_sub(start.len() + end.len())
            )
        );
        self.source_bytes += xml.len();
        self.sources.push_back(SourceMessage { xml });
        let mut reset = false;
        while self.source_bytes > MAX_TRANSCRIPT_BYTES {
            let Some(removed) = self.sources.pop_front() else {
                break;
            };
            self.source_bytes = self.source_bytes.saturating_sub(removed.xml.len());
            reset = true;
        }
        if reset || self.turns >= MAX_TURNS {
            self.reset();
        } else if let Some(source) = self.sources.back() {
            self.timeline.push(source_block(&source.xml));
        }
        true
    }

    /// Runs one sidecar turn. The status is emitted exclusively through the
    /// `set_status` tool, never through normal assistant prose.
    pub async fn update(&mut self) -> anyhow::Result<Option<String>> {
        if self.timeline.is_empty() {
            return Ok(None);
        }
        self.session.request(InferenceRequest {
            instructions: Arc::from(INSTRUCTIONS),
            input: self.timeline.clone(),
            agent_id_labels: std::collections::BTreeMap::new(),
            tools: Arc::from([set_status_spec()]),
        });
        let mut pending = PendingInferenceResponse::default();
        let (items, provider_response_id) = loop {
            match self.session.run().await {
                InferenceEvent::ContextItem { index, event } => pending.apply(index, event),
                InferenceEvent::Finished {
                    provider_response_id,
                    ..
                } => break (pending.finish()?, provider_response_id),
                InferenceEvent::Failed { error } => {
                    anyhow::bail!("activity inference failed: {error:#}")
                }
                InferenceEvent::TemporaryFailure { .. }
                | InferenceEvent::RequestSent
                | InferenceEvent::StreamingStarted
                | InferenceEvent::Quota { .. } => {}
            }
        };
        let status = status_from_items(&items);
        let results = tool_results(&items);
        self.timeline
            .push(Arc::new(ContextBlock::InferenceResponse {
                items,
                provider_response_id,
            }));
        if !results.is_empty() {
            self.timeline
                .push(Arc::new(ContextBlock::ToolResults { results }));
        }
        self.turns += 1;
        Ok(status)
    }

    fn reset(&mut self) {
        self.session.abort();
        self.session = self.inference.status_session(PromptCacheKey::generate());
        self.timeline = self
            .sources
            .iter()
            .map(|source| source_block(&source.xml))
            .collect();
        self.turns = 0;
    }
}

fn source_block(xml: &str) -> Arc<ContextBlock> {
    Arc::new(ContextBlock::UserMessage {
        sender: MessageSender::User,
        content: vec![ContentPart::Text {
            text: format!("<recent_transcript>{xml}</recent_transcript>"),
        }],
    })
}

fn set_status_spec() -> ToolSpec {
    ToolSpec {
        name: SET_STATUS.try_into().expect("valid status tool name"),
        tool_type: ToolType::Function,
        description: "Set the current activity label. Use a concise three or four word label, at most 50 bytes."
            .to_owned(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "status": { "type": "string", "maxLength": 50 } },
            "required": ["status"],
            "additionalProperties": false
        }),
        format: None,
    }
}

fn status_from_items(items: &[InferenceResponseItem]) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        InferenceResponseItem::ToolCall {
            name, arguments, ..
        } if name.as_str() == SET_STATUS => serde_json::from_str::<serde_json::Value>(arguments)
            .ok()?
            .get("status")?
            .as_str()
            .and_then(bounded_status),
        _ => None,
    })
}

fn bounded_status(status: &str) -> Option<String> {
    (status.len() <= MAX_STATUS_BYTES).then(|| status.to_owned())
}

fn tool_results(items: &[InferenceResponseItem]) -> Vec<ToolResult> {
    items
        .iter()
        .filter_map(|item| match item {
            InferenceResponseItem::ToolCall {
                id,
                name,
                tool_type,
                ..
            } if name.as_str() == SET_STATUS => {
                let now = UnixMs::now();
                Some(ToolResult {
                    call_id: id.clone(),
                    tool_type: *tool_type,
                    body: ToolOutput {
                        output: Arc::new("status recorded".to_owned()),
                        status: ToolOutputStatus::Success,
                    },
                    started_at: now,
                    finished_at: now,
                    metadata: None,
                })
            }
            _ => None,
        })
        .collect()
}

fn xml_escape_capped(text: &str, max_bytes: usize) -> String {
    let mut escaped = String::new();
    for character in text.chars() {
        let encoded = match character {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&apos;".to_owned(),
            _ => character.to_string(),
        };
        if escaped.len() + encoded.len() > max_bytes {
            break;
        }
        escaped.push_str(&encoded);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{MAX_MESSAGE_BYTES, MAX_STATUS_BYTES, bounded_status, xml_escape_capped};

    #[test]
    fn status_is_limited_to_fifty_bytes() {
        let at_limit = "a".repeat(MAX_STATUS_BYTES);
        assert_eq!(
            bounded_status(&at_limit).as_deref(),
            Some(at_limit.as_str())
        );
        assert_eq!(bounded_status(&"a".repeat(MAX_STATUS_BYTES + 1)), None);
    }

    #[test]
    fn escaped_source_stays_bounded() {
        let text = "<&>\"'".repeat(MAX_MESSAGE_BYTES);
        assert!(xml_escape_capped(&text, MAX_MESSAGE_BYTES).len() <= MAX_MESSAGE_BYTES);
    }
}
