//! Durable, watch-scoped Luna presentation sidecar.
//!
//! Sources are observed only after their event commits. Native agents write
//! them directly; Claude mirrors confirmed external transcript text. Every
//! provider attempt rebuilds from that durable source of truth, so a rewind
//! cannot leave a session summarizing an abandoned branch.

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rho_core::{
    ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    MessageSender, PendingInferenceResponse, ToolOutput, ToolOutputStatus, ToolResult, ToolSpec,
    ToolType, UnixMs,
};
use rho_db::RhoDb;
use rho_inference::{Inference, InferenceSession, PromptCacheKey};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::db::{AgentId, AgentPresentationUpdate, AgentReadTxnExt as _, PresentationField};
use crate::{
    PRESENTATION_SOURCE_TAIL_BYTES, PresentationSource, PresentationSpeaker, presentation_sources,
};

const MAX_MESSAGE_BYTES: usize = 1024;
const MAX_TRANSCRIPT_BYTES: usize = 10 * 1024;
const MAX_TITLE_CHARS: usize = 30;
const MAX_STATUS_BYTES: usize = 50;
const MAX_TURNS: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_REQUESTS: usize = 2;
pub(crate) const MIN_INTERVAL: Duration = Duration::from_secs(15);
const SET_TITLE: &str = "set_title";
const SET_STATUS: &str = "set_status";

const INSTRUCTIONS: &str = "You maintain an agent's compact presentation from its recent XML \
transcript. Set title only when it is missing or the conversation's subject materially changed. \
A title is lowercase kebab-case, at most 30 characters, and names the subject rather than a task. \
Set activity to a concise three or four word current-progress label, at most 50 bytes. Use \
lowercase words unless a type, function, or other code identifier needs its conventional casing. \
When the agent is no longer actively working, do not set activity. Use the tools; do not write prose.";

pub struct Watch {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

impl Watch {
    pub(crate) fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

fn request_semaphore() -> Arc<Semaphore> {
    static REQUESTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(REQUESTS.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS))))
}

/// Generate one presentation proposal from the committed lineage. Persistence
/// remains in the owning runtime loop so completion cannot race source events.
pub(crate) async fn acquire_request() -> anyhow::Result<OwnedSemaphorePermit> {
    Ok(request_semaphore().acquire_owned().await?)
}

pub(crate) fn has_input(db: &RhoDb, agent_id: AgentId) -> bool {
    presentation_input(db, agent_id).is_some()
}

/// Canonical durable representation of one external Claude transcript item.
/// Keeping the cap at the mirror boundary prevents an unbounded JSONL record
/// from becoming a second unbounded local persistence path.
pub(crate) fn canonical_source_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut capped = String::new();
    for character in text.chars() {
        if capped.len() + character.len_utf8() > MAX_MESSAGE_BYTES {
            break;
        }
        capped.push(character);
    }
    Some(capped)
}

pub(crate) async fn generate(
    db: RhoDb,
    inference: Inference,
    agent_id: AgentId,
    _permit: OwnedSemaphorePermit,
) -> anyhow::Result<Option<AgentPresentationUpdate>> {
    if presentation_input(&db, agent_id).is_none() {
        return Ok(None);
    }
    let Some((seed, sources)) = presentation_input(&db, agent_id) else {
        return Ok(None);
    };
    let mut session = Session::new(inference, seed);
    for source in sources {
        session.push(source);
    }
    let Some(update) = tokio::time::timeout(REQUEST_TIMEOUT, session.update()).await?? else {
        return Ok(None);
    };
    Ok(Some(update))
}

fn presentation_input(db: &RhoDb, agent_id: AgentId) -> Option<(Seed, Vec<PresentationSource>)> {
    let (seed, sources) = {
        let read = db.read();
        let record = read.get_agent(agent_id);
        let records = read.agent_presentation_source_tail(agent_id, PRESENTATION_SOURCE_TAIL_BYTES);
        (
            Seed {
                title: record
                    .display_name
                    .map(|title| (title, true))
                    .or_else(|| record.generated_title.map(|title| (title, false))),
                activity: record.activity,
            },
            presentation_sources(agent_id, &records),
        )
    };
    (!sources.is_empty()).then_some((seed, sources))
}

struct Seed {
    title: Option<(String, bool)>,
    activity: Option<String>,
}

struct SourceMessage {
    source: PresentationSource,
    xml: String,
}

struct Session {
    inference: Inference,
    session: InferenceSession,
    seed: Seed,
    sources: VecDeque<SourceMessage>,
    timeline: Vec<Arc<ContextBlock>>,
    source_bytes: usize,
    turns: usize,
}

impl Session {
    fn new(inference: Inference, seed: Seed) -> Self {
        let session = inference.status_session(PromptCacheKey::generate());
        let mut this = Self {
            inference,
            session,
            seed,
            sources: VecDeque::new(),
            timeline: Vec::new(),
            source_bytes: 0,
            turns: 0,
        };
        this.timeline.push(presentation_block(&this.seed));
        this
    }

    fn push(&mut self, source: PresentationSource) {
        let text = source.text.trim();
        if text.is_empty() {
            return;
        }
        let start = format!("<message speaker=\"{}\">", speaker_name(source.speaker));
        let end = "</message>";
        let xml = format!(
            "{start}{}{end}",
            xml_escape_capped(
                text,
                MAX_MESSAGE_BYTES.saturating_sub(start.len() + end.len())
            )
        );
        self.source_bytes += xml.len();
        self.sources.push_back(SourceMessage { source, xml });
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
    }

    async fn update(&mut self) -> anyhow::Result<Option<AgentPresentationUpdate>> {
        let Some(through) = self.sources.back().map(|source| source.source.through) else {
            return Ok(None);
        };
        self.session.request(InferenceRequest {
            instructions: Arc::from(INSTRUCTIONS),
            input: self.timeline.clone(),
            agent_id_labels: std::collections::BTreeMap::new(),
            tools: Arc::from([set_title_spec(), set_status_spec()]),
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
                    anyhow::bail!("presentation inference failed: {error:#}")
                }
                InferenceEvent::TemporaryFailure { .. }
                | InferenceEvent::RequestSent
                | InferenceEvent::StreamingStarted
                | InferenceEvent::Quota { .. } => {}
            }
        };
        let (title, activity) = fields_from_items(&items);
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
        Ok(
            (title != PresentationField::Unchanged || activity != PresentationField::Unchanged)
                .then_some(AgentPresentationUpdate {
                    generated_title: title,
                    activity,
                    through,
                }),
        )
    }

    fn reset(&mut self) {
        self.session.abort();
        self.session = self.inference.status_session(PromptCacheKey::generate());
        self.timeline = vec![presentation_block(&self.seed)];
        self.timeline
            .extend(self.sources.iter().map(|source| source_block(&source.xml)));
        self.turns = 0;
    }
}

fn speaker_name(speaker: PresentationSpeaker) -> &'static str {
    match speaker {
        PresentationSpeaker::User => "user",
        PresentationSpeaker::Agent => "agent",
        PresentationSpeaker::Assistant => "assistant",
    }
}

fn presentation_block(seed: &Seed) -> Arc<ContextBlock> {
    let title = seed
        .title
        .as_ref()
        .map_or_else(String::new, |(title, manual)| {
            format!(
                "<title origin=\"{}\">{}</title>",
                if *manual { "manual" } else { "generated" },
                xml_escape_capped(title, MAX_TITLE_CHARS)
            )
        });
    let activity = seed.activity.as_ref().map_or_else(String::new, |activity| {
        format!(
            "<activity>{}</activity>",
            xml_escape_capped(activity, MAX_STATUS_BYTES)
        )
    });
    Arc::new(ContextBlock::UserMessage {
        sender: MessageSender::User,
        content: vec![ContentPart::Text {
            text: format!("<current_presentation>{title}{activity}</current_presentation>"),
        }],
    })
}

fn source_block(xml: &str) -> Arc<ContextBlock> {
    Arc::new(ContextBlock::UserMessage {
        sender: MessageSender::User,
        content: vec![ContentPart::Text {
            text: format!("<recent_transcript>{xml}</recent_transcript>"),
        }],
    })
}

fn set_title_spec() -> ToolSpec {
    ToolSpec {
        name: SET_TITLE.try_into().expect("valid title tool name"),
        tool_type: ToolType::Function,
        description: "Set a lowercase kebab-case subject title, at most 30 characters.".to_owned(),
        input_schema: serde_json::json!({"type":"object","properties":{"title":{"type":"string","maxLength":30}},"required":["title"],"additionalProperties":false}),
        format: None,
    }
}

fn set_status_spec() -> ToolSpec {
    ToolSpec {
        name: SET_STATUS.try_into().expect("valid status tool name"),
        tool_type: ToolType::Function,
        description: "Set a concise current activity label, at most 50 bytes; use lowercase except for code identifiers.".to_owned(),
        input_schema: serde_json::json!({"type":"object","properties":{"status":{"type":"string","maxLength":50}},"required":["status"],"additionalProperties":false}),
        format: None,
    }
}

fn fields_from_items(items: &[InferenceResponseItem]) -> (PresentationField, PresentationField) {
    let mut title = PresentationField::Unchanged;
    let mut activity = PresentationField::Unchanged;
    for item in items {
        let InferenceResponseItem::ToolCall {
            name, arguments, ..
        } = item
        else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
            continue;
        };
        if name.as_str() == SET_TITLE {
            if let Some(value) = value
                .get("title")
                .and_then(serde_json::Value::as_str)
                .and_then(bounded_title)
            {
                title = PresentationField::Set(value);
            }
        } else if name.as_str() == SET_STATUS
            && let Some(value) = value
                .get("status")
                .and_then(serde_json::Value::as_str)
                .and_then(bounded_status)
        {
            activity = PresentationField::Set(value);
        }
    }
    (title, activity)
}

fn bounded_title(title: &str) -> Option<String> {
    let title = title.trim();
    (!title.is_empty()
        && title.chars().count() <= MAX_TITLE_CHARS
        && title
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
    .then(|| title.to_owned())
}
fn bounded_status(status: &str) -> Option<String> {
    (!status.trim().is_empty() && status.len() <= MAX_STATUS_BYTES).then(|| status.to_owned())
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
            } if name.as_str() == SET_TITLE || name.as_str() == SET_STATUS => {
                let now = UnixMs::now();
                Some(ToolResult {
                    call_id: id.clone(),
                    tool_type: *tool_type,
                    body: ToolOutput {
                        output: Arc::new("presentation recorded".to_owned()),
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
    use super::{
        MAX_MESSAGE_BYTES, MAX_STATUS_BYTES, bounded_status, bounded_title, canonical_source_text,
    };

    #[test]
    fn presentation_fields_are_bounded() {
        assert_eq!(
            bounded_title("daemon-socket"),
            Some("daemon-socket".to_owned())
        );
        assert_eq!(bounded_title("Not kebab"), None);
        assert_eq!(
            bounded_status(&"a".repeat(MAX_STATUS_BYTES)),
            Some("a".repeat(MAX_STATUS_BYTES))
        );
        assert_eq!(bounded_status(&"a".repeat(MAX_STATUS_BYTES + 1)), None);
    }

    #[test]
    fn canonical_source_text_trims_and_caps_unicode() {
        assert_eq!(canonical_source_text("  \n "), None);
        let text = canonical_source_text(&"é".repeat(MAX_MESSAGE_BYTES)).unwrap();
        assert!(text.len() <= MAX_MESSAGE_BYTES);
        assert!(text.is_char_boundary(text.len()));
    }
}
