//! One-off, 7 Sep: every Claude log the file copier wrote is rebuilt
//! from Claude Code's session file.
//!
//! Until today the Claude runtime copied the file's lines into rows and
//! wrote its queue between them (`Accepted`, `QueueCleared`, `Cleared`),
//! though Claude Code never persists a queue and the rows are now what
//! the stream tells. A copied log is rewound to its first conversation
//! row and the file's active branch is appended again in the file's
//! order, through the projection the stream's events go through, so
//! every reader sees what the stream would have told: rows only, no
//! queue. A log without a file (a session never spoken to, or one whose
//! file is gone) keeps its rows; only a queue left open is closed.
//! Delete this module once every daemon has run it.

use rho_claude::SessionLine;
use rho_core::UnixMs;
use rho_db::RhoDb;
use uuid::Uuid;

use super::projection::{assistant_row, compacted_row, line_time, user_row};
use crate::db::{
    AgentEventPos, AgentId, AgentReadTxnExt as _, AgentRuntime, AgentUsageModel,
    AgentWriteTxnExt as _,
};
use crate::{AgentEvent, TranscriptLine};

/// What the run did, for the daemon's log line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rebuilt {
    /// Logs rewound and written again from their file.
    pub rebuilt: usize,
    /// Logs with no file to rebuild from whose open queue was closed.
    pub closed: usize,
}

/// What one agent's log got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Rebuilt,
    Closed,
    Nothing,
}

/// Rebuilds every copied Claude log; run before any agent loop starts,
/// since a loop appends to the log it would rewrite.
pub async fn rebuild_claude_logs(db: &RhoDb) -> Rebuilt {
    {
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
    }
    let claude_agents = db
        .read()
        .list_agents()
        .into_iter()
        .filter_map(|(agent_id, head)| match head.config.runtime {
            AgentRuntime::Claude { session_id } => Some((
                agent_id,
                session_id,
                head.primary_workdir().repo().to_owned(),
            )),
            AgentRuntime::Rho { .. } => None,
        })
        .collect::<Vec<_>>();
    let mut done = Rebuilt::default();
    for (agent_id, session_id, repo) in claude_agents {
        if rewind_target(&db.read().agent_event_records(agent_id).1).is_none() {
            continue;
        }
        let lines = match rho_claude::find_session_transcript(session_id, &repo).await {
            Ok(Some(path)) => match rho_claude::read_session_lines(&path).await {
                Ok(lines) => Some(lines),
                Err(error) => {
                    eprintln!(
                        "rho daemon: {agent_id:?}: session file {path} unreadable, rows kept: {error:#}"
                    );
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                eprintln!(
                    "rho daemon: {agent_id:?}: session {session_id} file lookup failed, rows kept: {error:#}"
                );
                None
            }
        };
        match rebuild_agent(db, agent_id, lines.as_deref()).await {
            Outcome::Rebuilt => done.rebuilt += 1,
            Outcome::Closed => done.closed += 1,
            Outcome::Nothing => {}
        }
    }
    done
}

/// One agent: with the file's lines, its log is rewound and written
/// again; without, a queue left open is closed.
async fn rebuild_agent(db: &RhoDb, agent_id: AgentId, lines: Option<&[SessionLine]>) -> Outcome {
    let records = db.read().agent_event_records(agent_id).1;
    let Some(to) = rewind_target(&records) else {
        return Outcome::Nothing;
    };
    let rows = lines.map(rebuilt_rows).unwrap_or_default();
    let now = UnixMs::now();
    let mut write = db.write().await;
    let outcome = if rows.is_empty() {
        if !queue_open(&records) {
            return Outcome::Nothing;
        }
        write.append_agent_event(agent_id, &AgentEvent::Cleared { at: now });
        Outcome::Closed
    } else {
        write.rewind_agent(now, agent_id, to);
        for (uuid, line, at) in rows {
            write.append_agent_event(agent_id, &AgentEvent::Transcript { uuid, line, at });
        }
        Outcome::Rebuilt
    };
    write.commit();
    outcome
}

/// Where the rewind goes: the first row of the Claude runtime's, when
/// the visible log has any row only the copier wrote (its queue, a
/// presentation source). `None` once the log is the stream's rows only,
/// which is what makes a second run do nothing. Rows of the Rho
/// runtime's before the agent was rebound to Claude are not the file's
/// and stay ahead of the rewind.
fn rewind_target(records: &[(AgentEventPos, AgentEvent<'static>)]) -> Option<AgentEventPos> {
    let old = records.iter().any(|(_, event)| {
        matches!(
            event,
            AgentEvent::Accepted(_)
                | AgentEvent::QueueCleared
                | AgentEvent::ClaudePresentationSource { .. }
        )
    });
    if !old {
        return None;
    }
    records
        .iter()
        .find(|(_, event)| {
            matches!(
                event,
                AgentEvent::Accepted(_)
                    | AgentEvent::QueueCleared
                    | AgentEvent::Cleared { .. }
                    | AgentEvent::ClaudePresentationSource { .. }
                    | AgentEvent::Transcript { .. }
            )
        })
        .map(|(pos, _)| *pos)
}

/// Whether the copier's queue is still open: an `Accepted` no echo,
/// `Sent` or clear ever closed.
fn queue_open(records: &[(AgentEventPos, AgentEvent<'static>)]) -> bool {
    records
        .iter()
        .fold(0usize, |queued, (_, event)| match event {
            AgentEvent::Accepted(_) => queued + 1,
            AgentEvent::Sent { .. } | AgentEvent::QueueCleared | AgentEvent::Cleared { .. } => 0,
            _ => queued,
        })
        > 0
}

/// The file's lines as the stream's rows, in the file's order. Usage is
/// left off: the usage tables and every reader's fold already counted
/// the rows being rewound, and a rewind does not uncount.
fn rebuilt_rows(lines: &[SessionLine]) -> Vec<(Uuid, TranscriptLine, UnixMs)> {
    let mut usage_told = None;
    let mut rows = Vec::new();
    for line in lines {
        let row = match line {
            SessionLine::Assistant(message) => {
                if message.parent_tool_use_id.is_some() {
                    continue;
                }
                assistant_row(message, AgentUsageModel::UNKNOWN, &mut usage_told)
            }
            SessionLine::User(message) => {
                if message.parent_tool_use_id.is_some()
                    || message.is_replay.unwrap_or(false)
                    || message.is_synthetic.unwrap_or(false)
                {
                    continue;
                }
                user_row(message)
            }
            SessionLine::Compacted {
                uuid,
                timestamp,
                metadata,
            } => {
                let (uuid, line, _) = compacted_row(Some(&uuid.to_string()), Some(metadata));
                Ok(Some((uuid, line, line_time(timestamp.as_deref()))))
            }
        };
        match row {
            Ok(Some((uuid, mut line, at))) => {
                if let TranscriptLine::Assistant { usage, .. } = &mut line {
                    *usage = None;
                }
                rows.push((uuid, line, at));
            }
            Ok(None) => {}
            Err(error) => eprintln!("rho daemon: Claude session line skipped: {error:#}"),
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use rho_core::{ContentPart, MessageDelivery, MessageSender};

    use super::*;
    use crate::db::{AgentProfileWriteTxnExt as _, AgentRole, ClaudeEffort, SessionBinding};
    use crate::{InputKind, QueuedInput};

    const FILE: &str = r#"
{"type":"user","uuid":"00000000-0000-4000-8000-000000000001","parentUuid":null,"sessionId":"00000000-0000-4000-8000-0000000000aa","timestamp":"2026-09-07T10:00:00.000Z","message":{"role":"user","content":"hi"}}
{"type":"assistant","uuid":"00000000-0000-4000-8000-000000000002","parentUuid":"00000000-0000-4000-8000-000000000001","sessionId":"00000000-0000-4000-8000-0000000000aa","timestamp":"2026-09-07T10:00:01.000Z","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"hello"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":1,"output_tokens":2}}}
{"type":"user","uuid":"00000000-0000-4000-8000-000000000003","parentUuid":"00000000-0000-4000-8000-000000000002","sessionId":"00000000-0000-4000-8000-0000000000aa","timestamp":"2026-09-07T10:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a\n"}]}}
{"type":"system","subtype":"compact_boundary","uuid":"00000000-0000-4000-8000-000000000004","parentUuid":"00000000-0000-4000-8000-000000000003","logicalParentUuid":"00000000-0000-4000-8000-000000000003","sessionId":"00000000-0000-4000-8000-0000000000aa","timestamp":"2026-09-07T10:00:03.000Z","compactMetadata":{"postTokens":50}}
{"type":"user","uuid":"00000000-0000-4000-8000-000000000005","parentUuid":"00000000-0000-4000-8000-000000000004","sessionId":"00000000-0000-4000-8000-0000000000aa","timestamp":"2026-09-07T10:00:04.000Z","message":{"role":"user","content":"again"}}
"#;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000 | n)
    }

    async fn claude_agent(db: &RhoDb, counter: u64) -> AgentId {
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent_id = write.alloc_agent_id();
        write.create_agent(
            UnixMs(1),
            agent_id,
            None,
            vec![rho_workspaces::WorkspaceInfo::Workspace {
                repo: "/home/user/src/rho".into(),
                id: rho_workspaces::WorkspaceId::from_counter(
                    counter,
                    &rho_workspaces::WorkspaceIdDomain(0),
                )
                .unwrap(),
            }],
            AgentRole::default(),
            SessionBinding::ClaudeOpus {
                effort: ClaudeEffort::High,
            },
            AgentRuntime::Claude {
                session_id: uuid::Uuid::new_v4(),
            },
            None,
        );
        write.commit();
        agent_id
    }

    fn accepted(text: &str) -> AgentEvent<'static> {
        AgentEvent::Accepted(QueuedInput {
            source: MessageSender::User,
            kind: InputKind::Message {
                content: vec![ContentPart::Text {
                    text: text.to_owned(),
                }],
            },
            delivery: MessageDelivery::NextRequest,
            at: UnixMs(2),
        })
    }

    fn visible(db: &RhoDb, agent_id: AgentId) -> Vec<AgentEvent<'static>> {
        db.read()
            .agent_event_records(agent_id)
            .1
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }

    #[test]
    fn the_files_lines_become_the_streams_rows_in_the_files_order() {
        let lines = rho_claude::parse_session_lines(FILE).unwrap();
        let rows = rebuilt_rows(&lines);
        let uuids = rows.iter().map(|(uuid, _, _)| *uuid).collect::<Vec<_>>();
        assert_eq!(uuids, [id(1), id(2), id(3), id(4), id(5)]);
        assert!(matches!(&rows[0].1, TranscriptLine::User { text } if text == "hi"));
        assert!(matches!(
            &rows[1].1,
            TranscriptLine::Assistant { text, calls, usage: None, context_used: Some(3) }
                if text == "hello" && calls.len() == 1 && calls[0].name == "Bash"
        ));
        assert!(
            matches!(&rows[2].1, TranscriptLine::ToolResults { results } if results.len() == 1)
        );
        assert!(matches!(
            &rows[3].1,
            TranscriptLine::Compacted {
                context_used: Some(50)
            }
        ));
        assert!(matches!(&rows[4].1, TranscriptLine::User { text } if text == "again"));
        assert!(rows.windows(2).all(|pair| pair[0].2 < pair[1].2));
    }

    #[tokio::test]
    async fn a_copied_log_is_rewound_and_written_again_from_its_file() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let agent_id = claude_agent(&db, 1).await;
        {
            let mut write = db.write().await;
            write.append_agent_event(agent_id, &accepted("hi"));
            write.append_agent_event(
                agent_id,
                &AgentEvent::Transcript {
                    uuid: id(1),
                    line: TranscriptLine::User {
                        text: "hi".to_owned(),
                    },
                    at: UnixMs(3),
                },
            );
            write.append_agent_event(agent_id, &AgentEvent::QueueCleared);
            write.append_agent_event(agent_id, &accepted("dangling"));
            write.commit();
        }
        let lines = rho_claude::parse_session_lines(FILE).unwrap();
        assert_eq!(
            rebuild_agent(&db, agent_id, Some(&lines)).await,
            Outcome::Rebuilt
        );
        let rows = visible(&db, agent_id);
        assert!(matches!(rows[0], AgentEvent::Created { .. }));
        assert!(matches!(
            rows[1],
            AgentEvent::Rewound { to, .. } if to == AgentEventPos::new(1)
        ));
        let uuids = rows[2..]
            .iter()
            .map(|event| match event {
                AgentEvent::Transcript { uuid, .. } => *uuid,
                other => panic!("not a transcript row: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(uuids, [id(1), id(2), id(3), id(4), id(5)]);

        // Rows only now: a second run has nothing to take back.
        assert_eq!(
            rebuild_agent(&db, agent_id, Some(&lines)).await,
            Outcome::Nothing
        );
        assert_eq!(visible(&db, agent_id).len(), rows.len());
    }

    #[tokio::test]
    async fn without_a_file_the_rows_stay_and_an_open_queue_is_closed() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let dangling = claude_agent(&db, 1).await;
        let closed = claude_agent(&db, 2).await;
        {
            let mut write = db.write().await;
            write.append_agent_event(dangling, &accepted("one"));
            write.append_agent_event(closed, &accepted("two"));
            write.append_agent_event(closed, &AgentEvent::QueueCleared);
            write.commit();
        }
        assert_eq!(rebuild_agent(&db, dangling, None).await, Outcome::Closed);
        assert!(matches!(
            visible(&db, dangling).last(),
            Some(AgentEvent::Cleared { .. })
        ));
        assert_eq!(rebuild_agent(&db, dangling, None).await, Outcome::Nothing);
        assert_eq!(rebuild_agent(&db, closed, None).await, Outcome::Nothing);
        assert!(matches!(
            visible(&db, closed).last(),
            Some(AgentEvent::QueueCleared)
        ));
    }

    /// Proof on a `cp` of a real database, never the live file:
    /// `RHO_REBUILD_DB_COPY=<copy> cargo test -p rho-agent -- --ignored
    /// rebuilds_a_copy`.
    #[tokio::test]
    #[ignore]
    async fn rebuilds_a_copy_of_a_real_db() {
        let Ok(path) = std::env::var("RHO_REBUILD_DB_COPY") else {
            return;
        };
        let db = RhoDb::open(std::path::PathBuf::from(path));
        let done = rebuild_claude_logs(&db).await;
        for (agent_id, head) in db.read().list_agents() {
            if !matches!(head.config.runtime, AgentRuntime::Claude { .. }) {
                continue;
            }
            let rows = visible(&db, agent_id);
            let mut kinds = std::collections::BTreeMap::<&str, usize>::new();
            for row in &rows {
                let kind = match row {
                    AgentEvent::Transcript { .. } => "transcript",
                    AgentEvent::Rewound { .. } => "rewound",
                    AgentEvent::Accepted(_) => "accepted",
                    AgentEvent::QueueCleared | AgentEvent::Cleared { .. } => "cleared",
                    AgentEvent::Sent { .. } => "sent",
                    AgentEvent::Replied { .. } => "replied",
                    AgentEvent::Failed { .. } => "failed",
                    AgentEvent::Turn { .. } => "turn",
                    AgentEvent::Presented { .. } => "presented",
                    AgentEvent::Wants { .. } => "wants",
                    AgentEvent::ClaudePresentationSource { .. } => "source",
                    AgentEvent::Created { .. } => "created",
                    AgentEvent::RoleChanged { .. } => "role",
                    AgentEvent::WorkdirAdded { .. } => "workdir",
                    AgentEvent::RuntimeRebound { .. } => "rebound",
                };
                *kinds.entry(kind).or_default() += 1;
            }
            eprintln!("{agent_id:?} visible={} {kinds:?}", rows.len());
        }
        eprintln!("{done:?}");
    }
}
