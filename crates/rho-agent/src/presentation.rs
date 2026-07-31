//! Durable, watch-scoped Luna presentation sidecar.
//!
//! Sources are observed only after their event commits. Native agents write
//! them directly; Claude mirrors confirmed external transcript text. Each
//! runtime loop owns its agent's persistent session, shared by activity
//! updates and turn reports so one prompt prefix stays warm across both.
//! Appending is position-checked against the durable tail: a rewind forks
//! the lineage, the positions stop matching, and the session rebuilds from
//! scratch — so it can never keep summarizing an abandoned branch.

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
use tokio::sync::{Mutex as TokioMutex, OwnedSemaphorePermit, Semaphore};

use crate::db::{
    AgentDisposition, AgentEventPos, AgentId, AgentPresentationUpdate, AgentReadTxnExt as _,
    AgentRole, AgentWriteTxnExt as _, PresentationField, TurnReport,
};
use crate::{
    PRESENTATION_SOURCE_TAIL_BYTES, PresentationSource, PresentationSpeaker, presentation_sources,
};

const MAX_MESSAGE_BYTES: usize = 1024;
const MAX_TRANSCRIPT_BYTES: usize = 10 * 1024;
const MAX_TITLE_CHARS: usize = 30;
const MAX_STATUS_BYTES: usize = 50;
const MAX_TURNS: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_REQUESTS: usize = 4;
pub(crate) const MIN_INTERVAL: Duration = Duration::from_secs(15);
const SET_TITLE: &str = "set_title";
const SET_STATUS: &str = "set_status";
const REPORT_FYI: &str = "report_fyi";
const REPORT_NEEDS_YOU: &str = "report_needs_you";
const MAX_SUMMARY_BYTES: usize = MAX_STATUS_BYTES;
const MAX_FINAL_MESSAGE_BYTES: usize = 4 * 1024;

const INSTRUCTIONS: &str = "You maintain a coding agent's compact presentation from its \
incrementally appended XML transcript, so a dashboard row can direct the user's attention. \
Set title when it is missing — always name a titleless conversation — and change it only when \
the subject materially changed or a clearer understanding of the work suggests a truer name. A \
title is lowercase kebab-case, at most 30 characters, and names the subject rather than a task. Set \
activity to a concise three or four word current-progress label, at most 50 bytes; when the \
agent is no longer actively working, do not set activity. When the newest input is a \
<turn_ended> block, classify its final message instead: call report_needs_you when it asks the \
user a question, answers a question the user asked, requests review or a decision, reports \
being blocked, failed, or unfinished, or otherwise leaves the next step with the user — an \
answer in a discussion expects the user's reply, so it is needs_you even when complete; call \
report_fyi only when delegated work finished cleanly and nothing is asked of the user — a \
trailing offer of optional follow-up work is still fyi. \
summary is a concise few-word label of the outcome — for report_needs_you, of what is being \
asked — at most 50 bytes, the same shape as activity. Use lowercase except for types, \
functions, and other code identifiers; no trailing period. Use the tools; do not write prose.";

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
    session: Arc<TokioMutex<Session>>,
    agent_id: AgentId,
    _permit: OwnedSemaphorePermit,
) -> anyhow::Result<Option<AgentPresentationUpdate>> {
    let Some((seed, sources)) = presentation_input(&db, agent_id) else {
        return Ok(None);
    };
    let mut session = session.lock().await;
    session.sync(seed, sources);
    session.update().await
}

/// Classify one finished turn on the loop-owned session — same instructions,
/// same tool list, same source timeline as activity updates — with a
/// `<turn_ended>` block appended, so both request kinds share one prompt
/// prefix instead of forking the cache. Detached from the calling loop: a
/// slow provider call never delays the runtime and the report still lands if
/// the agent unloads meanwhile. Not gated on any client watching: the report
/// decides row ordering, so it must exist before a client looks.
pub(crate) fn spawn_turn_report(
    db: RhoDb,
    pool: std::sync::Weak<crate::pool::AgentPool>,
    session: Arc<TokioMutex<Session>>,
    agent_id: AgentId,
    final_answer: &str,
) {
    let final_message = final_answer.trim().to_owned();
    if final_message.is_empty() {
        return;
    }
    // Sub-agent turns are the parent's court unless the user has personally
    // messaged the agent, and Iris is not a rail row; neither gets a report.
    let record = db.read().get_agent(agent_id);
    if (record.parent_agent.is_some() && !record.user_interacted)
        || record.role == AgentRole::Iris
        || record
            .labels
            .iter()
            .any(|label| label == crate::iris_tools::LABEL)
    {
        return;
    }
    tokio::spawn(async move {
        let report = async {
            let _permit = acquire_request().await?;
            let (seed, sources) = presentation_context(&db, agent_id);
            let mut session = session.lock().await;
            session.sync(seed, sources);
            session.report(&final_message).await
        }
        .await;
        let report = match report {
            Ok(Some(report)) => report,
            Ok(None) => return,
            Err(error) => {
                eprintln!("rho-agent: turn report failed: {error:#}");
                return;
            }
        };
        // Pending still wants the report; so does snoozed — the turn
        // finished inside the quiet window and the expiry broadcast will
        // resurface this row, summary and all. A Done row keeps showing its
        // settled summary, so a raced ack persists too; only Hidden means
        // the user does not want the row at all.
        match db.read().get_agent(agent_id).disposition {
            AgentDisposition::Pending
            | AgentDisposition::Snoozed { .. }
            | AgentDisposition::Done => {}
            AgentDisposition::Hidden => return,
        }
        {
            let mut write = db.write().await;
            write.record_agent_turn_report(agent_id, &report);
            write.commit();
        }
        if let Some(pool) = pool.upgrade() {
            pool.publish_turn_report(agent_id, report);
        }
    });
}

fn presentation_input(db: &RhoDb, agent_id: AgentId) -> Option<(Seed, Vec<PresentationSource>)> {
    let (seed, sources) = presentation_context(db, agent_id);
    (!sources.is_empty()).then_some((seed, sources))
}

fn presentation_context(db: &RhoDb, agent_id: AgentId) -> (Seed, Vec<PresentationSource>) {
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
}

struct Seed {
    title: Option<(String, bool)>,
    activity: Option<String>,
}

struct SourceMessage {
    source: PresentationSource,
    xml: String,
}

pub(crate) struct Session {
    inference: Inference,
    session: InferenceSession,
    seed: Seed,
    sources: VecDeque<SourceMessage>,
    timeline: Vec<Arc<ContextBlock>>,
    source_bytes: usize,
    turns: usize,
    last_through: Option<AgentEventPos>,
}

impl Session {
    pub(crate) fn new(inference: Inference) -> Self {
        let session = inference.status_session(PromptCacheKey::generate());
        Self {
            inference,
            session,
            seed: Seed {
                title: None,
                activity: None,
            },
            sources: VecDeque::new(),
            timeline: Vec::new(),
            source_bytes: 0,
            turns: 0,
            last_through: None,
        }
    }

    /// Bring the session up to date with the durable source tail. When the
    /// tail contains the last position this session consumed, only the
    /// sources after it are appended, preserving the warm prompt prefix.
    /// Otherwise — fresh session, rewound lineage, or a tail window that
    /// moved past us — the session rebuilds from scratch on a fresh cache
    /// key.
    fn sync(&mut self, seed: Seed, tail: Vec<PresentationSource>) {
        let resume_at = self.last_through.and_then(|pos| {
            tail.iter()
                .position(|source| source.through == pos)
                .map(|index| index + 1)
        });
        let Some(resume_at) = resume_at else {
            self.seed = seed;
            self.sources.clear();
            self.source_bytes = 0;
            self.last_through = None;
            self.reset();
            for source in tail {
                self.push(source);
            }
            return;
        };
        for source in tail.into_iter().skip(resume_at) {
            self.push(source);
        }
    }

    fn push(&mut self, source: PresentationSource) {
        self.last_through = Some(source.through);
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
        let items = self.complete().await?;
        let (title, activity) = fields_from_items(&items);
        Ok(
            (title != PresentationField::Unchanged || activity != PresentationField::Unchanged)
                .then_some(AgentPresentationUpdate {
                    generated_title: title,
                    activity,
                    through,
                }),
        )
    }

    async fn report(&mut self, final_message: &str) -> anyhow::Result<Option<TurnReport>> {
        self.timeline.push(turn_ended_block(final_message));
        let items = self.complete().await?;
        Ok(report_from_items(&items))
    }

    /// One provider round, bounded by [`REQUEST_TIMEOUT`]. A timeout or
    /// inference failure leaves the underlying session in an unknown state,
    /// so the session resets — fresh cache key, timeline rebuilt from
    /// sources — before the error propagates.
    async fn complete(&mut self) -> anyhow::Result<Vec<InferenceResponseItem>> {
        match tokio::time::timeout(REQUEST_TIMEOUT, self.exchange()).await {
            Ok(Ok(items)) => Ok(items),
            Ok(Err(error)) => {
                self.reset();
                Err(error)
            }
            Err(_) => {
                self.reset();
                anyhow::bail!("presentation request timed out")
            }
        }
    }

    /// One provider round with the session's single stable prompt shape:
    /// the same instructions and full tool list for every request, so
    /// activity updates and turn reports share one cacheable prefix.
    async fn exchange(&mut self) -> anyhow::Result<Vec<InferenceResponseItem>> {
        self.session.request(InferenceRequest {
            instructions: Arc::from(INSTRUCTIONS),
            input: self.timeline.clone(),
            agent_id_labels: std::collections::BTreeMap::new(),
            tools: Arc::from([
                set_title_spec(),
                set_status_spec(),
                report_fyi_spec(),
                report_needs_you_spec(),
            ]),
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
        let results = tool_results(&items);
        self.timeline
            .push(Arc::new(ContextBlock::InferenceResponse {
                items: items.clone(),
                provider_response_id,
            }));
        if !results.is_empty() {
            self.timeline
                .push(Arc::new(ContextBlock::ToolResults { results }));
        }
        self.turns += 1;
        Ok(items)
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

/// The turn-end marker carries the final message itself rather than trusting
/// the mirrored source tail to already contain it: settlement can race the
/// source commit, and redundancy here is harmless.
fn turn_ended_block(final_message: &str) -> Arc<ContextBlock> {
    Arc::new(ContextBlock::UserMessage {
        sender: MessageSender::User,
        content: vec![ContentPart::Text {
            text: format!(
                "<turn_ended><final_message>{}</final_message></turn_ended>",
                xml_escape_capped(final_message, MAX_FINAL_MESSAGE_BYTES)
            ),
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

fn report_fyi_spec() -> ToolSpec {
    ToolSpec {
        name: REPORT_FYI.try_into().expect("valid report tool name"),
        tool_type: ToolType::Function,
        description: "The turn finished cleanly; nothing is asked of the user.".to_owned(),
        input_schema: serde_json::json!({"type":"object","properties":{"summary":{"type":"string","maxLength":50}},"required":["summary"],"additionalProperties":false}),
        format: None,
    }
}

fn report_needs_you_spec() -> ToolSpec {
    ToolSpec {
        name: REPORT_NEEDS_YOU.try_into().expect("valid report tool name"),
        tool_type: ToolType::Function,
        description: "The turn leaves the next step with the user: a question, review request, \
blocker, or failure."
            .to_owned(),
        input_schema: serde_json::json!({"type":"object","properties":{"summary":{"type":"string","maxLength":50}},"required":["summary"],"additionalProperties":false}),
        format: None,
    }
}

/// Trimmed and byte-capped on a char boundary: an overlong line from the
/// model is still a usable row, unlike a rejected one.
fn bounded_summary(summary: &str) -> Option<String> {
    let summary = summary.trim();
    if summary.is_empty() {
        return None;
    }
    let mut capped = String::new();
    for character in summary.chars() {
        if capped.len() + character.len_utf8() > MAX_SUMMARY_BYTES {
            break;
        }
        capped.push(character);
    }
    Some(capped)
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

fn report_from_items(items: &[InferenceResponseItem]) -> Option<TurnReport> {
    items.iter().find_map(|item| {
        let InferenceResponseItem::ToolCall {
            name, arguments, ..
        } = item
        else {
            return None;
        };
        let needs_you = match name.as_str() {
            REPORT_FYI => false,
            REPORT_NEEDS_YOU => true,
            _ => return None,
        };
        let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
        let summary = bounded_summary(value.get("summary")?.as_str()?)?;
        Some(TurnReport { needs_you, summary })
    })
}

fn bounded_title(title: &str) -> Option<String> {
    // Luna occasionally calls `set_title` with lowercase words separated by
    // whitespace despite the tool contract. Canonicalize that harmless form
    // before enforcing the persisted kebab-case representation.
    let title = title.split_whitespace().collect::<Vec<_>>().join("-");
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
            } if matches!(
                name.as_str(),
                SET_TITLE | SET_STATUS | REPORT_FYI | REPORT_NEEDS_YOU
            ) =>
            {
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
        MAX_MESSAGE_BYTES, MAX_STATUS_BYTES, MAX_SUMMARY_BYTES, bounded_status, bounded_summary,
        bounded_title, canonical_source_text,
    };

    #[test]
    fn presentation_fields_are_bounded() {
        assert_eq!(
            bounded_title("daemon-socket"),
            Some("daemon-socket".to_owned())
        );
        assert_eq!(
            bounded_title("ci lockfile fix"),
            Some("ci-lockfile-fix".to_owned())
        );
        assert_eq!(bounded_title("Not kebab"), None);
        assert_eq!(
            bounded_status(&"a".repeat(MAX_STATUS_BYTES)),
            Some("a".repeat(MAX_STATUS_BYTES))
        );
        assert_eq!(bounded_status(&"a".repeat(MAX_STATUS_BYTES + 1)), None);
    }

    #[test]
    fn summary_is_trimmed_and_capped_not_rejected() {
        assert_eq!(bounded_summary("  \n "), None);
        assert_eq!(
            bounded_summary(" tests pass "),
            Some("tests pass".to_owned())
        );
        let capped = bounded_summary(&"é".repeat(MAX_SUMMARY_BYTES)).unwrap();
        assert!(capped.len() <= MAX_SUMMARY_BYTES);
        assert!(capped.is_char_boundary(capped.len()));
    }

    #[test]
    fn canonical_source_text_trims_and_caps_unicode() {
        assert_eq!(canonical_source_text("  \n "), None);
        let text = canonical_source_text(&"é".repeat(MAX_MESSAGE_BYTES)).unwrap();
        assert!(text.len() <= MAX_MESSAGE_BYTES);
        assert!(text.is_char_boundary(text.len()));
    }
}
