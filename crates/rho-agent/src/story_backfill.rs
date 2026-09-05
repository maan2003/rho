//! The story of the agents that predate it. Built from the raw log for a
//! Rho agent, from the Claude session file for a Claude agent, and
//! `HistoryUnavailableBefore` when that file is gone.
//!
//! It runs in the background so a restart is not held for it: agents are
//! taken most-recently-touched first, one per transaction, and the head's
//! `story_built` makes it resumable. A load jumps the queue
//! ([`ensure_story`]) because a runtime must never append live events to
//! a story its history has not been written into yet.
//!
//! Deleted, like every migration, once the user has restarted on it.

use rho_core::UnixMs;
use rho_db::RhoDb;

use crate::AgentEvent;
use crate::db::{
    AgentEventPos, AgentId, AgentReadTxnExt as _, AgentRuntime, AgentWriteTxnExt as _, UnixMillis,
};
use crate::story::{self, StoryEvent};

/// Every agent still awaiting its story, most-recently-touched first: the
/// ones a person is most likely to open are ready soonest.
pub fn agents_awaiting_story(db: &RhoDb) -> Vec<AgentId> {
    let read = db.read();
    let mut waiting = read
        .list_agents()
        .into_iter()
        .filter(|(_, head)| !head.story_built)
        .map(|(agent_id, _)| (agent_id, read.agent_attention(agent_id).updated_at))
        .collect::<Vec<_>>();
    waiting.sort_by_key(|(_, touched)| std::cmp::Reverse(*touched));
    waiting.into_iter().map(|(agent_id, _)| agent_id).collect()
}

/// Builds one agent's story if it has none yet, in a single transaction.
/// Returns how many events were written.
pub async fn ensure_story(db: &RhoDb, agent_id: AgentId) -> usize {
    let head = db.read().get_agent(agent_id);
    if head.story_built {
        return 0;
    }
    // Reading the Claude session file is I/O; do it before the write
    // transaction so no other agent waits behind it.
    let told = match &head.config.runtime {
        AgentRuntime::Rho { .. } => from_raw_log(db, agent_id),
        AgentRuntime::Claude { session_id } => {
            let cwd = head.primary_workdir().repo().to_owned();
            let messages = rho_claude::read_session_messages_by_id(
                *session_id,
                &cwd,
                rho_claude::SessionMessagesOptions::default(),
            )
            .await
            .unwrap_or_default();
            from_claude_transcript(db, agent_id, &messages)
        }
    };
    let mut write = db.write().await;
    // The background job and a load can prepare the same agent at once;
    // the transaction is where that is decided, so the loser writes
    // nothing rather than telling the history twice.
    if write.agent_story_built(agent_id) {
        return 0;
    }
    for (event, through) in &told {
        match through {
            Some(through) => write.append_agent_story_from(agent_id, event, *through),
            None => write.append_agent_story(agent_id, event),
        };
    }
    write.mark_agent_story_built(agent_id);
    write.commit();
    told.len()
}

/// A Rho agent's story: its raw log, told, each event remembering the raw
/// position it came from. The log carries no wall clock, so times come
/// from the tool results in it — the only stamps it has — carried forward
/// and never allowed to go backwards.
fn from_raw_log(db: &RhoDb, agent_id: AgentId) -> Vec<(StoryEvent, Option<AgentEventPos>)> {
    let read = db.read();
    let head = read.get_agent(agent_id);
    let (_, records) = read.agent_event_records(agent_id);
    let mut at = head.config.created_at;
    let mut told = Vec::new();
    for (position, event) in &records {
        if let AgentEvent::ToolResult { result } = event
            && result.finished_at > at
        {
            at = result.finished_at;
        }
        told.extend(
            story::from_raw_event(event, at)
                .into_iter()
                .map(|event| (event, Some(*position))),
        );
    }
    told
}

/// A Claude agent's story: its session transcript, told with the
/// transcript's own timestamps. Its `Created` still comes from the raw
/// log, which slice A wrote for every agent.
fn from_claude_transcript(
    db: &RhoDb,
    agent_id: AgentId,
    messages: &[rho_claude::SessionMessage],
) -> Vec<(StoryEvent, Option<AgentEventPos>)> {
    let read = db.read();
    let head = read.get_agent(agent_id);
    let created = head.config.created_at;
    let (_, records) = read.agent_event_records(agent_id);
    let mut told = records
        .iter()
        .filter(|(_, event)| matches!(event, AgentEvent::Created { .. }))
        .flat_map(|(position, event)| {
            story::from_raw_event(event, created)
                .into_iter()
                .map(|event| (event, Some(*position)))
        })
        .collect::<Vec<_>>();
    if messages.is_empty() {
        // The session file is gone: say so rather than invent a history.
        told.push((StoryEvent::HistoryUnavailableBefore { at: created }, None));
        return told;
    }
    let mut at = created;
    for message in messages {
        if let Some(stamp) = message_at(message)
            && stamp > at
        {
            at = stamp;
        }
        if let Ok(Some((_, speaker, text))) =
            crate::claude::projection::presentation_source(message)
        {
            told.extend(
                story::from_claude_source(speaker, &text, at)
                    .into_iter()
                    .map(|event| (event, None)),
            );
        }
        told.extend(
            tool_calls(&message.message, at)
                .into_iter()
                .map(|event| (event, None)),
        );
    }
    told
}

/// The calls one transcript message made, in order.
fn tool_calls(message: &serde_json::Value, at: UnixMillis) -> Vec<StoryEvent> {
    let Some(content) = message
        .get("content")
        .and_then(|content| content.as_array())
    else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| block.get("type").and_then(|kind| kind.as_str()) == Some("tool_use"))
        .filter_map(|block| {
            let name = rho_core::ToolName::try_from(block.get("name")?.as_str()?).ok()?;
            let arguments = block
                .get("input")
                .map(ToString::to_string)
                .unwrap_or_default();
            Some(StoryEvent::ToolCall {
                name,
                what: story::tool_line(&arguments),
                at,
            })
        })
        .collect()
}

fn message_at(message: &rho_claude::SessionMessage) -> Option<UnixMs> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(message.timestamp.as_deref()?).ok()?;
    Some(UnixMs(timestamp.timestamp_millis().try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;
    use rho_db::RhoDb;

    use super::*;
    use crate::db::AgentProfileWriteTxnExt as _;

    #[tokio::test]
    async fn a_rho_agent_gets_its_history_before_it_is_loaded() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let (agent_id, next) = {
            let mut write = db.write().await;
            write.init_agent_tables();
            let agent_id = write.alloc_agent_id();
            let next = write.create_agent(
                UnixMs(1),
                agent_id,
                None,
                vec![crate::db::tests::test_workspace()],
                crate::db::AgentRole::PM,
                crate::db::SessionBinding::ResponsesGpt55(Default::default()),
                crate::db::tests::test_agent_runtime(),
                None,
            );
            write.append_agent_event(next, &crate::db::tests::user_event("do the thing"));
            // An agent from before the story existed: the head says so, and
            // creation's own story event is discarded with it.
            crate::db::clear_agent_story(&mut write, agent_id);
            write.commit();
            (agent_id, next)
        };
        let _ = next;

        assert_eq!(agents_awaiting_story(&db), vec![agent_id]);
        assert_eq!(ensure_story(&db, agent_id).await, 2);
        // Once built it is never rebuilt, however often an agent is loaded.
        assert_eq!(ensure_story(&db, agent_id).await, 0);
        assert!(agents_awaiting_story(&db).is_empty());

        let story = db
            .read()
            .agent_story(agent_id, crate::db::StoryPos::default());
        assert!(matches!(story[0].1, StoryEvent::Created { .. }));
        assert!(matches!(
            &story[1].1,
            StoryEvent::UserMessage { text, .. } if text == "do the thing"
        ));
    }
}
