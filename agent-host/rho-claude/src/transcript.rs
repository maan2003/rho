use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionMessagesOptions {
    pub limit: Option<usize>,
    pub offset: usize,
    pub include_system_messages: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionMessage {
    pub kind: SessionMessageKind,
    pub uuid: Uuid,
    pub session_id: Uuid,
    pub message: Value,
    pub parent_tool_use_id: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionMessageKind {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptEntry {
    #[serde(rename = "type")]
    kind: TranscriptEntryKind,
    uuid: Option<Uuid>,
    #[serde(alias = "session_id")]
    session_id: Option<Uuid>,
    #[serde(alias = "parent_uuid")]
    parent_uuid: Option<Uuid>,
    #[serde(alias = "logical_parent_uuid")]
    logical_parent_uuid: Option<Uuid>,
    #[serde(default)]
    message: Value,
    timestamp: Option<String>,
    #[serde(alias = "parent_tool_use_id")]
    parent_tool_use_id: Option<String>,
    is_meta: Option<bool>,
    #[serde(alias = "isReplay")]
    is_replay: Option<bool>,
    #[serde(alias = "isSynthetic")]
    is_synthetic: Option<bool>,
    is_sidechain: Option<bool>,
    /// The summary Claude writes after compacting, in the user's seat.
    is_compact_summary: Option<bool>,
    is_visible_in_transcript_only: Option<bool>,
    team_name: Option<String>,
    subtype: Option<String>,
    compact_metadata: Option<CompactMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TranscriptEntryKind {
    User,
    Assistant,
    System,
    Progress,
    Attachment,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompactMetadata {
    preserved_segment: Option<PreservedSegment>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreservedSegment {
    tail_uuid: Uuid,
}

impl TranscriptEntry {
    fn uuid(&self) -> Option<Uuid> {
        self.uuid
    }

    fn session_id(&self) -> Option<Uuid> {
        self.session_id
    }

    fn is_message_like(&self) -> bool {
        matches!(
            self.kind,
            TranscriptEntryKind::User
                | TranscriptEntryKind::Assistant
                | TranscriptEntryKind::System
                | TranscriptEntryKind::Progress
                | TranscriptEntryKind::Attachment
        ) && self.uuid.is_some()
    }

    fn visible(&self, include_system_messages: bool) -> bool {
        match self.kind {
            TranscriptEntryKind::User | TranscriptEntryKind::Assistant => {}
            TranscriptEntryKind::System if include_system_messages => {}
            _ => return false,
        }
        !self.is_meta.unwrap_or(false)
            && !self.is_replay.unwrap_or(false)
            && !self.is_synthetic.unwrap_or(false)
            && !self.is_sidechain.unwrap_or(false)
            && !self.is_compact_summary.unwrap_or(false)
            && !self.is_visible_in_transcript_only.unwrap_or(false)
            && self.team_name.is_none()
    }
}

pub async fn read_session_messages(
    transcript_path: &Utf8Path,
    options: SessionMessagesOptions,
) -> Result<Vec<SessionMessage>> {
    let entries = read_transcript_entries(transcript_path).await?;
    Ok(session_messages(entries, options))
}

async fn read_transcript_entries(transcript_path: &Utf8Path) -> Result<Vec<TranscriptEntry>> {
    let text = tokio::fs::read_to_string(transcript_path)
        .await
        .with_context(|| format!("read Claude transcript {transcript_path}"))?;
    parse_transcript_entries(&text, transcript_path.as_str())
}

fn parse_transcript_entries(text: &str, source: &str) -> Result<Vec<TranscriptEntry>> {
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("parse Claude transcript line in {source}: {line}"))?;
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(
            kind,
            "user" | "assistant" | "system" | "progress" | "attachment"
        ) {
            continue;
        }
        let entry: TranscriptEntry = serde_json::from_value(value)
            .with_context(|| format!("parse Claude transcript message in {source}: {line}"))?;
        if entry.is_message_like() {
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub async fn read_session_messages_by_id(
    projects: &Utf8Path,
    session_id: Uuid,
    cwd: &Utf8Path,
    options: SessionMessagesOptions,
) -> Result<Vec<SessionMessage>> {
    let Some(transcript_path) = find_session_transcript(projects, session_id, cwd).await? else {
        return Ok(Vec::new());
    };
    read_session_messages(&transcript_path, options).await
}

/// Where a session's transcript is, under the `projects/` tree the caller
/// names. The tree is passed in rather than resolved here: this crate does
/// not decide which Claude configuration a process is running against.
pub async fn find_session_transcript(
    projects: &Utf8Path,
    session_id: Uuid,
    cwd: &Utf8Path,
) -> Result<Option<Utf8PathBuf>> {
    let projects_dir = projects;
    let cwd = canonical_utf8(cwd).await.unwrap_or_else(|| cwd.to_owned());
    let project_key = project_key(&cwd);
    let direct = projects_dir
        .join(&project_key)
        .join(format!("{session_id}.jsonl"));
    if non_empty_file(&direct).await? {
        return Ok(Some(direct));
    }
    if project_key.len() <= MAX_PROJECT_KEY_LEN {
        return Ok(None);
    }

    let prefix = format!("{}-", &project_key[..MAX_PROJECT_KEY_LEN]);
    let mut entries = match tokio::fs::read_dir(&projects_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read Claude projects directory"),
    };
    while let Some(entry) = entries.next_entry().await? {
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        if !file_name.starts_with(&prefix) {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .ok()
            .map(|path| path.join(format!("{session_id}.jsonl")));
        let Some(path) = path else {
            continue;
        };
        if non_empty_file(&path).await? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// A moved agent's transcript, brought to where its new place looks for
/// it. Claude keys a session's file by the directory it ran in, so an agent
/// whose place changed would otherwise start the session again under the
/// same id with nothing in it. The file is hard-linked into the project
/// directory for `cwd` (copied when a link is not possible); the old
/// directory keeps its entry. Returns the file at the new place, or `None`
/// when the session has no file anywhere: it was never spoken to.
pub async fn relocate_session_transcript(
    projects: &Utf8Path,
    session_id: Uuid,
    cwd: &Utf8Path,
) -> Result<Option<Utf8PathBuf>> {
    if let Some(found) = find_session_transcript(projects, session_id, cwd).await? {
        return Ok(Some(found));
    }
    let cwd = canonical_utf8(cwd).await.unwrap_or_else(|| cwd.to_owned());
    let project_key = project_key(&cwd);
    if project_key.len() > MAX_PROJECT_KEY_LEN {
        // Claude shortens a long key with a hash this crate does not compute.
        return Ok(None);
    }
    let Some(source) = find_session_transcript_anywhere(projects, session_id).await? else {
        return Ok(None);
    };
    let dir = projects.join(&project_key);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("create Claude project directory {dir}"))?;
    let target = dir.join(format!("{session_id}.jsonl"));
    match tokio::fs::hard_link(&source, &target).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => {
            tokio::fs::copy(&source, &target)
                .await
                .with_context(|| format!("copy Claude transcript {source} to {target}"))?;
        }
    }
    Ok(Some(target))
}

/// The session's file under any project directory.
async fn find_session_transcript_anywhere(
    projects: &Utf8Path,
    session_id: Uuid,
) -> Result<Option<Utf8PathBuf>> {
    let mut entries = match tokio::fs::read_dir(projects).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read Claude projects directory"),
    };
    let file_name = format!("{session_id}.jsonl");
    while let Some(entry) = entries.next_entry().await? {
        let Some(path) = Utf8PathBuf::from_path_buf(entry.path())
            .ok()
            .map(|path| path.join(&file_name))
        else {
            continue;
        };
        if non_empty_file(&path).await? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

const MAX_PROJECT_KEY_LEN: usize = 200;

async fn canonical_utf8(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let path = tokio::fs::canonicalize(path).await.ok()?;
    Utf8PathBuf::from_path_buf(path).ok()
}

fn project_key(path: &Utf8Path) -> String {
    path.as_str()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

async fn non_empty_file(path: &Utf8Path) -> Result<bool> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("stat Claude transcript {path}")),
    }
}

fn session_messages(
    entries: Vec<TranscriptEntry>,
    options: SessionMessagesOptions,
) -> Vec<SessionMessage> {
    let chain = latest_chain(&entries);
    let messages = chain
        .into_iter()
        .filter(|entry| entry.visible(options.include_system_messages))
        .filter_map(to_session_message)
        .collect::<Vec<_>>();
    let offset = options.offset;
    match options.limit {
        Some(limit) if limit > 0 => messages.into_iter().skip(offset).take(limit).collect(),
        _ => messages.into_iter().skip(offset).collect(),
    }
}

fn latest_chain(entries: &[TranscriptEntry]) -> Vec<&TranscriptEntry> {
    if entries.is_empty() {
        return Vec::new();
    }
    let by_uuid = entries
        .iter()
        .filter_map(|entry| entry.uuid().map(|uuid| (uuid, entry)))
        .collect::<HashMap<_, _>>();
    let Some(mut current) = entries.iter().rev().find(|entry| {
        matches!(
            entry.kind,
            TranscriptEntryKind::User | TranscriptEntryKind::Assistant
        )
    }) else {
        return Vec::new();
    };

    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    while let Some(uuid) = current.uuid() {
        if !seen.insert(uuid) {
            break;
        }
        chain.push(current);
        // Compaction starts a new physical parent chain at the summary, but
        // `logicalParentUuid` retains the previous visible history. Follow it
        // so transcript consumers see the full active branch rather than only
        // the tail since the latest compaction.
        let parent_uuid = if current.kind == TranscriptEntryKind::System
            && current.subtype.as_deref() == Some("compact_boundary")
        {
            current
                .logical_parent_uuid
                .or_else(|| {
                    current
                        .compact_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.preserved_segment.as_ref())
                        .map(|segment| segment.tail_uuid)
                })
                .or(current.parent_uuid)
        } else {
            current.parent_uuid
        };
        let Some(parent_uuid) = parent_uuid else {
            break;
        };
        let Some(parent) = by_uuid.get(&parent_uuid).copied() else {
            break;
        };
        current = parent;
    }
    chain.reverse();
    chain
}

/// Usage recorded with the most recent assistant message, if any. Transcript
/// entries log `input_tokens` as a streaming placeholder, but the cache
/// read/creation buckets — which dominate context occupancy — are recorded
/// accurately, so this slightly undercounts and self-corrects on the next
/// live turn.
pub fn last_assistant_usage(messages: &[SessionMessage]) -> Option<crate::protocol::TokenUsage> {
    messages
        .iter()
        .rev()
        .filter(|message| message.kind == SessionMessageKind::Assistant)
        .find_map(|message| serde_json::from_value(message.message.get("usage")?.clone()).ok())
}

/// Returns the visible transcript prefix before the selected user turn and
/// the assistant UUID Claude should resume from. `None` as the UUID means the
/// first user turn was selected and the replacement session should be fresh.
pub fn rewind_session_messages(
    messages: &[SessionMessage],
    turns: u32,
) -> Option<(Vec<SessionMessage>, Option<Uuid>)> {
    if turns == 0 {
        return None;
    }
    let user_positions = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| is_user_prompt(message).then_some(index))
        .collect::<Vec<_>>();
    if user_positions.is_empty() {
        return None;
    }
    let selected = user_positions[user_positions.len().saturating_sub(turns as usize)];
    let resume = messages[..selected]
        .iter()
        .rposition(|message| message.kind == SessionMessageKind::Assistant);
    match resume {
        Some(index) => Some((messages[..=index].to_vec(), Some(messages[index].uuid))),
        None => Some((Vec::new(), None)),
    }
}

pub fn session_messages_through_assistant(
    messages: &[SessionMessage],
    assistant_uuid: Uuid,
) -> Option<Vec<SessionMessage>> {
    let index = messages.iter().position(|message| {
        message.kind == SessionMessageKind::Assistant && message.uuid == assistant_uuid
    })?;
    Some(messages[..=index].to_vec())
}

fn is_user_prompt(message: &SessionMessage) -> bool {
    if message.kind != SessionMessageKind::User {
        return false;
    }
    match message.message.get("content") {
        Some(Value::String(text)) => !is_auxiliary_user_text(text),
        Some(Value::Array(content)) => content.iter().any(|part| {
            part.get("type").and_then(Value::as_str) == Some("text")
                && part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !is_auxiliary_user_text(text))
        }),
        _ => false,
    }
}

fn is_auxiliary_user_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<command-name>")
        || text.starts_with("<local-command-stdout>")
        || text.starts_with("<task-notification>")
}

fn to_session_message(entry: &TranscriptEntry) -> Option<SessionMessage> {
    Some(SessionMessage {
        kind: match entry.kind {
            TranscriptEntryKind::User => SessionMessageKind::User,
            TranscriptEntryKind::Assistant => SessionMessageKind::Assistant,
            TranscriptEntryKind::System => SessionMessageKind::System,
            TranscriptEntryKind::Progress | TranscriptEntryKind::Attachment => return None,
        },
        uuid: entry.uuid()?,
        session_id: entry.session_id()?,
        message: entry.message.clone(),
        parent_tool_use_id: entry.parent_tool_use_id.clone(),
        timestamp: entry.timestamp.clone(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn a_moved_agents_transcript_is_brought_to_its_new_place() {
        let dir = tempfile::tempdir().unwrap();
        let projects = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let session_id = uuid::uuid!("00000000-0000-4000-8000-000000000005");
        let old = projects.join("-home-someone-src-rho");
        std::fs::create_dir_all(&old).unwrap();
        let rows = "{\"type\":\"user\"}\n";
        std::fs::write(old.join(format!("{session_id}.jsonl")), rows).unwrap();
        let cwd = Utf8Path::new("/nowhere-yet/src");

        let brought = relocate_session_transcript(&projects, session_id, cwd)
            .await
            .unwrap()
            .expect("the file is found under the old place");
        assert_eq!(
            brought,
            projects
                .join("-nowhere-yet-src")
                .join(format!("{session_id}.jsonl"))
        );
        assert_eq!(std::fs::read_to_string(&brought).unwrap(), rows);
        // Already there: found, not made again.
        assert_eq!(
            relocate_session_transcript(&projects, session_id, cwd)
                .await
                .unwrap(),
            Some(brought)
        );
        // Never spoken to: nothing anywhere, nothing made.
        let unspoken = uuid::uuid!("00000000-0000-4000-8000-000000000006");
        assert_eq!(
            relocate_session_transcript(&projects, unspoken, cwd)
                .await
                .unwrap(),
            None
        );
    }

    fn entry(kind: TranscriptEntryKind, uuid: Uuid, parent_uuid: Option<Uuid>) -> TranscriptEntry {
        TranscriptEntry {
            kind,
            uuid: Some(uuid),
            session_id: Some(uuid::uuid!("00000000-0000-4000-8000-000000000001")),
            parent_uuid,
            logical_parent_uuid: None,
            message: json!({"role": "user", "content": "hello"}),
            timestamp: None,
            parent_tool_use_id: None,
            is_meta: None,
            is_replay: None,
            is_synthetic: None,
            is_sidechain: None,
            is_compact_summary: None,
            is_visible_in_transcript_only: None,
            team_name: None,
            subtype: None,
            compact_metadata: None,
        }
    }

    #[test]
    fn restores_usage_from_last_assistant_entry() {
        let a = uuid::uuid!("00000000-0000-4000-8000-00000000000a");
        let b = uuid::uuid!("00000000-0000-4000-8000-00000000000b");
        let c = uuid::uuid!("00000000-0000-4000-8000-00000000000c");
        let mut old = entry(TranscriptEntryKind::Assistant, b, Some(a));
        old.message =
            json!({"role": "assistant", "usage": {"input_tokens": 1, "output_tokens": 1}});
        let mut last = entry(TranscriptEntryKind::Assistant, c, Some(b));
        last.message = json!({
            "role": "assistant",
            "usage": {
                "input_tokens": 3,
                "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": 60_000,
                "output_tokens": 200,
                "service_tier": "standard"
            }
        });
        let messages = session_messages(
            vec![entry(TranscriptEntryKind::User, a, None), old, last],
            SessionMessagesOptions::default(),
        );

        let usage = last_assistant_usage(&messages).expect("usage present");
        assert_eq!(usage.context_total(), 60_303);
    }

    #[test]
    fn rewinds_user_turns_without_counting_tool_results() {
        let user_one = uuid::uuid!("00000000-0000-4000-8000-00000000000a");
        let assistant_one = uuid::uuid!("00000000-0000-4000-8000-00000000000b");
        let tool_result = uuid::uuid!("00000000-0000-4000-8000-00000000000c");
        let assistant_two = uuid::uuid!("00000000-0000-4000-8000-00000000000d");
        let user_two = uuid::uuid!("00000000-0000-4000-8000-00000000000e");
        let command = uuid::uuid!("00000000-0000-4000-8000-00000000000f");
        let notification = uuid::uuid!("00000000-0000-4000-8000-000000000010");

        let mut first = entry(TranscriptEntryKind::User, user_one, None);
        first.message = json!({"role": "user", "content": [{"type": "text", "text": "one"}]});
        let mut response = entry(
            TranscriptEntryKind::Assistant,
            assistant_one,
            Some(user_one),
        );
        response.message = json!({"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]});
        let mut result = entry(TranscriptEntryKind::User, tool_result, Some(assistant_one));
        result.message = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}]});
        let mut final_response = entry(
            TranscriptEntryKind::Assistant,
            assistant_two,
            Some(tool_result),
        );
        final_response.message =
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]});
        let mut command_entry = entry(TranscriptEntryKind::User, command, Some(assistant_two));
        command_entry.message =
            json!({"role": "user", "content": "<command-name>/compact</command-name>"});
        let mut notification_entry = entry(TranscriptEntryKind::User, notification, Some(command));
        notification_entry.message = json!({"role": "user", "content": "<task-notification><status>completed</status></task-notification>"});
        let mut second = entry(TranscriptEntryKind::User, user_two, Some(notification));
        second.message = json!({"role": "user", "content": [{"type": "text", "text": "two"}]});
        let messages = session_messages(
            vec![
                first,
                response,
                result,
                final_response,
                command_entry,
                notification_entry,
                second,
            ],
            SessionMessagesOptions::default(),
        );

        let (one_turn, resume_at) = rewind_session_messages(&messages, 1).unwrap();
        assert_eq!(resume_at, Some(assistant_two));
        assert_eq!(
            one_turn.last().map(|message| message.uuid),
            Some(assistant_two)
        );

        let (all_turns, resume_at) = rewind_session_messages(&messages, 2).unwrap();
        assert!(all_turns.is_empty());
        assert_eq!(resume_at, None);
    }

    #[test]
    fn returns_latest_parent_chain() {
        let a = uuid::uuid!("00000000-0000-4000-8000-00000000000a");
        let b = uuid::uuid!("00000000-0000-4000-8000-00000000000b");
        let c = uuid::uuid!("00000000-0000-4000-8000-00000000000c");
        let fork = uuid::uuid!("00000000-0000-4000-8000-00000000000d");
        let messages = session_messages(
            vec![
                entry(TranscriptEntryKind::User, a, None),
                entry(TranscriptEntryKind::Assistant, b, Some(a)),
                entry(TranscriptEntryKind::User, fork, Some(a)),
                entry(TranscriptEntryKind::Assistant, c, Some(b)),
            ],
            SessionMessagesOptions::default(),
        );

        assert_eq!(
            messages
                .iter()
                .map(|message| message.uuid)
                .collect::<Vec<_>>(),
            [a, b, c]
        );
    }

    #[test]
    fn follows_history_across_compaction_boundary() {
        let user = uuid::uuid!("00000000-0000-4000-8000-00000000000a");
        let assistant = uuid::uuid!("00000000-0000-4000-8000-00000000000b");
        let compact = uuid::uuid!("00000000-0000-4000-8000-00000000000c");
        let summary = uuid::uuid!("00000000-0000-4000-8000-00000000000d");
        let latest = uuid::uuid!("00000000-0000-4000-8000-00000000000e");

        let mut boundary = entry(TranscriptEntryKind::System, compact, None);
        boundary.subtype = Some("compact_boundary".to_owned());
        boundary.logical_parent_uuid = Some(assistant);
        let messages = session_messages(
            vec![
                entry(TranscriptEntryKind::User, user, None),
                entry(TranscriptEntryKind::Assistant, assistant, Some(user)),
                boundary,
                entry(TranscriptEntryKind::User, summary, Some(compact)),
                entry(TranscriptEntryKind::Assistant, latest, Some(summary)),
            ],
            SessionMessagesOptions::default(),
        );

        assert_eq!(
            messages
                .iter()
                .map(|message| message.uuid)
                .collect::<Vec<_>>(),
            [user, assistant, summary, latest]
        );
    }

    #[test]
    fn filters_system_messages_by_default() {
        let a = uuid::uuid!("00000000-0000-4000-8000-00000000000a");
        let b = uuid::uuid!("00000000-0000-4000-8000-00000000000b");
        let mut system = entry(TranscriptEntryKind::System, b, Some(a));
        system.message = json!({"content": "notice"});

        let messages = session_messages(
            vec![entry(TranscriptEntryKind::User, a, None), system],
            SessionMessagesOptions::default(),
        );

        assert_eq!(
            messages
                .iter()
                .map(|message| message.uuid)
                .collect::<Vec<_>>(),
            [a]
        );
    }

    #[test]
    fn filters_synthetic_messages() {
        let a = uuid::uuid!("00000000-0000-4000-8000-00000000000a");
        let b = uuid::uuid!("00000000-0000-4000-8000-00000000000b");
        let mut synthetic = entry(TranscriptEntryKind::User, b, Some(a));
        synthetic.is_synthetic = Some(true);
        synthetic.message = json!({"role": "user", "content": "continued summary"});

        let messages = session_messages(
            vec![entry(TranscriptEntryKind::User, a, None), synthetic],
            SessionMessagesOptions::default(),
        );

        assert_eq!(
            messages
                .iter()
                .map(|message| message.uuid)
                .collect::<Vec<_>>(),
            [a]
        );
    }

    #[test]
    fn filters_replay_messages() {
        let a = uuid::uuid!("00000000-0000-4000-8000-00000000000a");
        let b = uuid::uuid!("00000000-0000-4000-8000-00000000000b");
        let mut replay = entry(TranscriptEntryKind::User, b, Some(a));
        replay.is_replay = Some(true);
        replay.message = json!({"role": "user", "content": "<task-notification />"});

        let messages = session_messages(
            vec![entry(TranscriptEntryKind::User, a, None), replay],
            SessionMessagesOptions::default(),
        );

        assert_eq!(
            messages
                .iter()
                .map(|message| message.uuid)
                .collect::<Vec<_>>(),
            [a]
        );
    }

    #[test]
    fn applies_offset_and_limit() {
        let a = uuid::uuid!("00000000-0000-4000-8000-00000000000a");
        let b = uuid::uuid!("00000000-0000-4000-8000-00000000000b");
        let c = uuid::uuid!("00000000-0000-4000-8000-00000000000c");
        let messages = session_messages(
            vec![
                entry(TranscriptEntryKind::User, a, None),
                entry(TranscriptEntryKind::Assistant, b, Some(a)),
                entry(TranscriptEntryKind::User, c, Some(b)),
            ],
            SessionMessagesOptions {
                offset: 1,
                limit: Some(1),
                include_system_messages: false,
            },
        );

        assert_eq!(
            messages
                .iter()
                .map(|message| message.uuid)
                .collect::<Vec<_>>(),
            [b]
        );
    }
}
