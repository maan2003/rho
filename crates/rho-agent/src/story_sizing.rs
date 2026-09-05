//! How big a whole-history story log would be for a real store. Sizing
//! only: it writes nothing and answers the one question slice B's
//! migration turns on. Point `RHO_PROOF_DB` at a copy. Deleted once the
//! story migration lands.

use rho_core::ContentPart;
use rho_db::RhoDb;

use crate::db::{AgentReadTxnExt, AgentRuntime};
use crate::{AgentEvent, InferenceResponseItem, QueuedItem, QueuedItemKind};

/// Per story event: a position key, a variant tag, an `at`, and framing.
const EVENT_OVERHEAD: usize = 48;
/// One typed tool line: the name plus the path, command or query, cut.
const TOOL_LINE: usize = 160;

#[derive(Default)]
struct Sizes {
    agents: usize,
    events: usize,
    bytes: usize,
}

impl Sizes {
    fn line(&self, what: &str) -> String {
        format!(
            "{what}: {} agents, {} story events, {:.1} MiB",
            self.agents,
            self.events,
            self.bytes as f64 / (1024.0 * 1024.0)
        )
    }
}

#[tokio::test]
#[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
async fn sizes_a_whole_history_story() {
    let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
    let db = RhoDb::open(&path);
    let heads = db.read().list_agents();

    let mut rho = Sizes::default();
    let mut claude = Sizes::default();
    let mut claude_bytes_on_disk = 0usize;
    let mut missing_transcripts = 0usize;
    let mut raw_events = 0usize;
    let mut recent = Sizes::default();
    let thirty_days_ago = rho_core::UnixMs::now()
        .0
        .saturating_sub(30 * 24 * 60 * 60 * 1000);

    for (agent_id, head) in &heads {
        match &head.config.runtime {
            AgentRuntime::Claude { session_id } => {
                claude.agents += 1;
                let cwd = head.primary_workdir().repo().to_owned();
                match rho_claude::find_session_transcript(*session_id, &cwd).await {
                    Ok(Some(transcript)) => {
                        let size = std::fs::metadata(&transcript)
                            .map(|meta| meta.len() as usize)
                            .unwrap_or(0);
                        claude_bytes_on_disk += size;
                        // A story keeps the text and one line per call, not
                        // the tool output the transcript carries; a third
                        // of the file is the working guess.
                        claude.bytes += size / 3;
                        claude.events += size / 4096;
                    }
                    Ok(None) | Err(_) => missing_transcripts += 1,
                }
            }
            AgentRuntime::Rho { .. } => {
                rho.agents += 1;
                let (_, events) = db.read().agent_events(*agent_id);
                let mut agent = Sizes::default();
                for event in &events {
                    raw_events += 1;
                    match event {
                        AgentEvent::Queued(QueuedItem {
                            kind: QueuedItemKind::UserMessage { content, .. },
                            ..
                        }) => {
                            agent.events += 1;
                            agent.bytes += EVENT_OVERHEAD + text_bytes(content);
                        }
                        AgentEvent::InferenceResponse { items, .. } => {
                            for item in items.iter() {
                                match item {
                                    InferenceResponseItem::AssistantMessage { content, .. } => {
                                        let bytes = text_bytes(content);
                                        if bytes > 0 {
                                            agent.events += 1;
                                            agent.bytes += EVENT_OVERHEAD + bytes;
                                        }
                                    }
                                    InferenceResponseItem::ToolCall { .. } => {
                                        agent.events += 1;
                                        agent.bytes += EVENT_OVERHEAD + TOOL_LINE;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        // A turn boundary, a cost line, and a title or
                        // activity: small, typed, and countable.
                        AgentEvent::Dequeued { .. } => {
                            agent.events += 3;
                            agent.bytes += 3 * EVENT_OVERHEAD + 64;
                        }
                        AgentEvent::PresentationUpdated { .. } => {
                            agent.events += 1;
                            agent.bytes += EVENT_OVERHEAD + 64;
                        }
                        _ => {}
                    }
                }
                rho.events += agent.events;
                rho.bytes += agent.bytes;
                let attention = db.read().agent_attention(*agent_id);
                let touched = attention
                    .last_turn_ended
                    .unwrap_or(attention.last_user_message)
                    .0;
                if touched >= thirty_days_ago {
                    recent.agents += 1;
                    recent.events += agent.events;
                    recent.bytes += agent.bytes;
                }
            }
        }
    }

    eprintln!("sizing: {} agents, {raw_events} raw events", heads.len());
    eprintln!("sizing: {}", rho.line("rho runtime"));
    eprintln!("sizing: {}", claude.line("claude runtime"));
    eprintln!(
        "sizing: claude transcripts {:.1} MiB on disk, {missing_transcripts} missing \
         (HistoryUnavailableBefore)",
        claude_bytes_on_disk as f64 / (1024.0 * 1024.0)
    );
    eprintln!("sizing: {}", recent.line("rho agents touched in 30 days"));
}

fn text_bytes(content: &[ContentPart]) -> usize {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.len(),
            ContentPart::Image { .. } => 0,
        })
        .sum()
}

/// What the backfill actually built, read back from the store it ran on.
#[tokio::test]
#[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
async fn reports_the_built_story() {
    use crate::db::StoryPos;
    use crate::story::StoryEvent;

    let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
    let db = RhoDb::open(&path);
    let heads = db.read().list_agents();

    let mut rho = Sizes::default();
    let mut claude = Sizes::default();
    let mut unbuilt = 0usize;
    let mut history_unavailable = 0usize;
    let mut replies = 0usize;
    let mut tool_calls = 0usize;
    let mut longest_reply = 0usize;
    let mut users = 0usize;

    for (agent_id, head) in &heads {
        if !head.story_built {
            unbuilt += 1;
        }
        let story = db.read().agent_story(*agent_id, StoryPos::default());
        let bytes = story
            .iter()
            .map(|(_, event)| senax_encoder::encode(event).map_or(0, |bytes| bytes.len()))
            .sum::<usize>();
        let sizes = match &head.config.runtime {
            AgentRuntime::Claude { .. } => &mut claude,
            AgentRuntime::Rho { .. } => &mut rho,
        };
        sizes.agents += 1;
        sizes.events += story.len();
        sizes.bytes += bytes + story.len() * EVENT_OVERHEAD;
        for (_, event) in &story {
            match event {
                StoryEvent::HistoryUnavailableBefore { .. } => history_unavailable += 1,
                StoryEvent::Reply { text, .. } => {
                    replies += 1;
                    longest_reply = longest_reply.max(text.len());
                }
                StoryEvent::UserMessage { .. } | StoryEvent::AgentMail { .. } => users += 1,
                StoryEvent::ToolCall { .. } => tool_calls += 1,
                _ => {}
            }
        }
    }

    println!("{}", rho.line("built rho"));
    println!("{}", claude.line("built claude"));
    println!(
        "built: {replies} replies (longest {longest_reply} bytes), {tool_calls} tool calls, \
         {users} messages in"
    );
    println!("built: {history_unavailable} agents with HistoryUnavailableBefore");
    println!("built: {unbuilt} agents still awaiting a story");
    println!(
        "store file: {:.1} GiB",
        std::fs::metadata(&path).unwrap().len() as f64 / (1024.0 * 1024.0 * 1024.0)
    );
}

/// Puts a copy back the way slice A leaves it, so the backfill can be
/// timed again from nothing. One transaction per agent, like the job.
#[tokio::test]
#[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
async fn clears_every_story() {
    let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
    let db = RhoDb::open(&path);
    let agents = db
        .read()
        .list_agents()
        .into_iter()
        .map(|(agent_id, _)| agent_id)
        .collect::<Vec<_>>();
    for agent_id in &agents {
        let mut write = db.write().await;
        crate::db::clear_agent_story(&mut write, *agent_id);
        write.commit();
    }
    println!("cleared: {} agents", agents.len());
}
