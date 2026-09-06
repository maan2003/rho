//! The one-off copy of every existing Claude agent's session file into
//! its log, run in the background at daemon start (`LIVE-TAIL-PLAN.md`,
//! "The Claude runtime reads its file"). Before 6 Sep the loop wrote
//! `Replied`, `Sent` and `ClaudePresentationSource` rows from the stream;
//! the file's `Transcript` rows replace them behind one `Rewound`. One
//! agent per transaction, newest first. An agent with a cursor is done,
//! so a restart resumes where it stopped and a load that gets there
//! first wins. Delete once every store has started under this build; the
//! loop does not know this ran.

use camino::{Utf8Path, Utf8PathBuf};
use rho_claude::{TailRead, TranscriptTail};
use rho_db::RhoDb;
use uuid::Uuid;

use super::{append_rows, project_rows, told_lines};
use crate::AgentEvent;
use crate::db::{
    AgentEventPos, AgentId, AgentReadTxnExt as _, AgentRuntime, AgentUsageModel,
    AgentWriteTxnExt as _, ClaudeTranscriptCursor, UnixMillis,
};

/// Copies the file of every Claude agent without a cursor, and says on
/// stderr what it did.
pub async fn backfill_claude_transcripts(db: RhoDb) {
    let candidates = candidates(&db);
    if candidates.is_empty() {
        return;
    }
    let (mut copied, mut absent, mut failed) = (0, 0, 0);
    for candidate in candidates {
        match backfill_agent(&db, &candidate).await {
            Ok(Backfilled::Copied(_)) => copied += 1,
            Ok(Backfilled::NoFile) => absent += 1,
            Ok(Backfilled::AlreadyDone) => {}
            Err(error) => {
                failed += 1;
                eprintln!(
                    "rho-agent: Claude transcript of {} not copied: {error:#}",
                    candidate.agent_id.encoded()
                );
            }
        }
        tokio::task::yield_now().await;
    }
    eprintln!(
        "rho-agent: Claude session files copied for {copied} agents \
         ({absent} without a file, {failed} failed)"
    );
}

struct Candidate {
    agent_id: AgentId,
    session_id: Uuid,
    primary_repo: Utf8PathBuf,
    usage_model: AgentUsageModel,
    touched: UnixMillis,
}

/// Every Claude agent without a cursor, most recently touched first. The
/// runtime is read from the creation row, so only Claude agents are
/// folded.
fn candidates(db: &RhoDb) -> Vec<Candidate> {
    let read = db.read();
    let mut candidates = Vec::new();
    for agent_id in read.list_agent_ids() {
        if read.claude_transcript_cursor(agent_id).is_some() {
            continue;
        }
        let Some(AgentEvent::Created {
            runtime: AgentRuntime::Claude { .. },
            ..
        }) = read.agent_event(agent_id, AgentEventPos::ZERO)
        else {
            continue;
        };
        let Some(head) = read.try_get_agent(agent_id) else {
            continue;
        };
        let AgentRuntime::Claude { session_id } = &head.config.runtime else {
            continue;
        };
        candidates.push(Candidate {
            agent_id,
            session_id: *session_id,
            primary_repo: head.primary_workdir().repo().to_owned(),
            usage_model: crate::db::usage_model_of(&head.config.runtime, head.config.binding),
            touched: head.last_turn_ended.unwrap_or(head.config.created_at),
        });
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.touched));
    candidates
}

#[derive(Debug, PartialEq)]
enum Backfilled {
    /// This many rows appended.
    Copied(usize),
    /// No file: the rows stay, and the cursor says not to ask again.
    NoFile,
    /// A cursor was there by the time this looked.
    AlreadyDone,
}

async fn backfill_agent(db: &RhoDb, candidate: &Candidate) -> anyhow::Result<Backfilled> {
    let (path, _) =
        rho_claude::session_transcript_path(candidate.session_id, &candidate.primary_repo).await?;
    backfill_file(
        db,
        candidate.agent_id,
        candidate.session_id,
        &path,
        candidate.usage_model,
    )
    .await
}

/// One agent's file into its log, in one transaction with the `Rewound`
/// over the older rows and the cursor.
async fn backfill_file(
    db: &RhoDb,
    agent_id: AgentId,
    session_id: Uuid,
    path: &Utf8Path,
    usage_model: AgentUsageModel,
) -> anyhow::Result<Backfilled> {
    let mut tail = TranscriptTail::new(path.to_owned(), 0);
    let lines = match tail.read().await? {
        TailRead::Lines(lines) => lines,
        TailRead::Truncated => anyhow::bail!("file shorter than nothing"),
        // Gone for good (Claude deletes old sessions): the rows stay.
        TailRead::Missing => {
            let mut write = db.write().await;
            if write.claude_transcript_cursor(agent_id).is_some() {
                return Ok(Backfilled::AlreadyDone);
            }
            write.set_claude_transcript_cursor(
                agent_id,
                &ClaudeTranscriptCursor { session_id, end: 0 },
            );
            write.commit();
            return Ok(Backfilled::NoFile);
        }
    };
    let (told, rewind_to) = {
        let read = db.read();
        (
            told_lines(&read, agent_id),
            first_stream_row(&read, agent_id),
        )
    };
    let mut usage_told = None;
    let rows = project_rows(agent_id, &lines, Some(&told), usage_model, &mut usage_told);
    let count = rows.len();
    let mut write = db.write().await;
    if write.claude_transcript_cursor(agent_id).is_some() {
        return Ok(Backfilled::AlreadyDone);
    }
    if count > 0
        && let Some(to) = rewind_to
    {
        write.rewind_agent(UnixMillis::now(), agent_id, to);
    }
    append_rows(&mut write, agent_id, rows);
    write.set_claude_transcript_cursor(
        agent_id,
        &ClaudeTranscriptCursor {
            session_id,
            end: tail.end(),
        },
    );
    write.commit();
    Ok(Backfilled::Copied(count))
}

/// The first row the loop before 6 Sep wrote from the stream, which the
/// `Rewound` takes back. `Accepted` and `Failed` are the new loop's rows
/// too, so never this.
fn first_stream_row(read: &rho_db::ReadTxn, agent_id: AgentId) -> Option<AgentEventPos> {
    read.agent_event_records(agent_id)
        .1
        .into_iter()
        .find_map(|(pos, event)| {
            matches!(
                event,
                AgentEvent::ClaudePresentationSource { .. }
                    | AgentEvent::Replied { .. }
                    | AgentEvent::Sent { .. }
            )
            .then_some(pos)
        })
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::super::tests::{
        A1, U1, assistant_json, claude_test_agent, told, transcript_rows, user_json,
    };
    use super::super::{Copied, TranscriptCopy};
    use super::*;

    const SESSION: Uuid = uuid::uuid!("00000000-0000-4000-8000-000000000002");
    const USAGE: AgentUsageModel = AgentUsageModel::OPUS;

    async fn older_loops_rows(db: &RhoDb, agent_id: AgentId) {
        let mut write = db.write().await;
        write.append_agent_event(
            agent_id,
            &AgentEvent::ClaudePresentationSource {
                source_id: Uuid::parse_str(U1).unwrap(),
                speaker: crate::PresentationSpeaker::User,
                text: Cow::Borrowed("hello"),
                at: rho_core::UnixMs(2),
            },
        );
        write.append_agent_event(
            agent_id,
            &AgentEvent::Replied {
                blocks: Cow::Owned(Vec::new()),
                context_used: None,
                usage: None,
                at: rho_core::UnixMs(3),
            },
        );
        write.commit();
    }

    fn kinds(db: &RhoDb, agent_id: AgentId) -> Vec<&'static str> {
        db.read()
            .agent_event_records(agent_id)
            .1
            .iter()
            .map(|(_, event)| match event {
                AgentEvent::Created { .. } => "created",
                AgentEvent::Rewound { .. } => "rewound",
                AgentEvent::Transcript { .. } => "transcript",
                AgentEvent::ClaudePresentationSource { .. } => "source",
                AgentEvent::Replied { .. } => "replied",
                _ => "other",
            })
            .collect()
    }

    fn two_lines() -> String {
        format!("{}\n{}\n", user_json(U1, "hello"), assistant_json(A1, "hi"))
    }

    #[tokio::test]
    async fn takes_back_the_older_loops_rows_behind_the_files() {
        let (temp, db, agent_id) = claude_test_agent(SESSION).await;
        let path = Utf8PathBuf::try_from(temp.path().join("session.jsonl")).unwrap();
        older_loops_rows(&db, agent_id).await;
        std::fs::write(&path, two_lines()).unwrap();

        let done = backfill_file(&db, agent_id, SESSION, &path, USAGE)
            .await
            .unwrap();

        assert_eq!(done, Backfilled::Copied(2));
        assert_eq!(
            kinds(&db, agent_id),
            ["created", "rewound", "transcript", "transcript"],
            "the older rows are behind the rewind"
        );
        assert_eq!(
            told(&transcript_rows(&db, agent_id)),
            ["user: hello", "assistant: hi"]
        );
        assert_eq!(
            db.read().claude_transcript_cursor(agent_id),
            Some(ClaudeTranscriptCursor {
                session_id: SESSION,
                end: two_lines().len() as u64,
            })
        );
    }

    #[tokio::test]
    async fn keeps_the_rows_of_an_agent_whose_file_is_gone() {
        let (temp, db, agent_id) = claude_test_agent(SESSION).await;
        let path = Utf8PathBuf::try_from(temp.path().join("session.jsonl")).unwrap();
        older_loops_rows(&db, agent_id).await;

        let done = backfill_file(&db, agent_id, SESSION, &path, USAGE)
            .await
            .unwrap();

        assert_eq!(done, Backfilled::NoFile);
        assert_eq!(kinds(&db, agent_id), ["created", "source", "replied"]);
        assert_eq!(
            db.read().claude_transcript_cursor(agent_id),
            Some(ClaudeTranscriptCursor {
                session_id: SESSION,
                end: 0
            })
        );
    }

    #[tokio::test]
    async fn leaves_an_agent_with_a_cursor_alone() {
        let (temp, db, agent_id) = claude_test_agent(SESSION).await;
        let path = Utf8PathBuf::try_from(temp.path().join("session.jsonl")).unwrap();
        std::fs::write(&path, two_lines()).unwrap();
        {
            let mut write = db.write().await;
            write.set_claude_transcript_cursor(
                agent_id,
                &ClaudeTranscriptCursor {
                    session_id: SESSION,
                    end: 0,
                },
            );
            write.commit();
        }

        let done = backfill_file(&db, agent_id, SESSION, &path, USAGE)
            .await
            .unwrap();

        assert_eq!(done, Backfilled::AlreadyDone);
        assert_eq!(kinds(&db, agent_id), ["created"]);
    }

    #[tokio::test]
    async fn a_loop_that_read_before_the_copy_does_not_tell_a_line_twice() {
        let (temp, db, agent_id) = claude_test_agent(SESSION).await;
        let path = Utf8PathBuf::try_from(temp.path().join("session.jsonl")).unwrap();
        std::fs::write(&path, two_lines()).unwrap();
        // The loop opened its copier (no cursor yet) before the copy ran.
        let mut copy = TranscriptCopy::open(&db, agent_id, SESSION, path.clone(), true);

        let done = backfill_file(&db, agent_id, SESSION, &path, USAGE)
            .await
            .unwrap();
        assert_eq!(done, Backfilled::Copied(2));
        let copied = copy.copy(&db, USAGE).await.unwrap();

        assert_eq!(copied, Copied::Nothing);
        assert_eq!(
            kinds(&db, agent_id),
            ["created", "transcript", "transcript"]
        );
        assert_eq!(
            db.read()
                .claude_transcript_cursor(agent_id)
                .map(|cursor| cursor.end),
            Some(two_lines().len() as u64)
        );
    }
}
