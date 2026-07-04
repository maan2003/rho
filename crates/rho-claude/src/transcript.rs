use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
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
    #[serde(default)]
    message: Value,
    timestamp: Option<String>,
    #[serde(alias = "parent_tool_use_id")]
    parent_tool_use_id: Option<String>,
    is_meta: Option<bool>,
    is_sidechain: Option<bool>,
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
    preserved_messages: Option<PreservedMessages>,
    preserved_segment: Option<PreservedSegment>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreservedMessages {
    anchor_uuid: Uuid,
    uuids: Vec<Uuid>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreservedSegment {
    anchor_uuid: Uuid,
    head_uuid: Uuid,
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
            && !self.is_sidechain.unwrap_or(false)
            && self.team_name.is_none()
    }
}

pub async fn read_session_messages(
    transcript_path: &Utf8Path,
    options: SessionMessagesOptions,
) -> Result<Vec<SessionMessage>> {
    let file = tokio::fs::File::open(transcript_path)
        .await
        .with_context(|| format!("open Claude transcript {transcript_path}"))?;
    let mut lines = BufReader::new(file).lines();
    let mut entries = Vec::new();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).with_context(|| {
            format!("parse Claude transcript line in {transcript_path}: {line}")
        })?;
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(
            kind,
            "user" | "assistant" | "system" | "progress" | "attachment"
        ) {
            continue;
        }
        let entry: TranscriptEntry = serde_json::from_value(value).with_context(|| {
            format!("parse Claude transcript message in {transcript_path}: {line}")
        })?;
        if entry.is_message_like() {
            entries.push(entry);
        }
    }
    Ok(session_messages(entries, options))
}

pub async fn read_session_messages_by_id(
    session_id: Uuid,
    cwd: &Utf8Path,
    options: SessionMessagesOptions,
) -> Result<Vec<SessionMessage>> {
    let Some(transcript_path) = find_session_transcript(session_id, cwd).await? else {
        return Ok(Vec::new());
    };
    read_session_messages(&transcript_path, options).await
}

pub async fn find_session_transcript(
    session_id: Uuid,
    cwd: &Utf8Path,
) -> Result<Option<Utf8PathBuf>> {
    let Some(projects_dir) = claude_projects_dir() else {
        return Ok(None);
    };
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

const MAX_PROJECT_KEY_LEN: usize = 200;

fn claude_projects_dir() -> Option<Utf8PathBuf> {
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(Utf8PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| Utf8PathBuf::from(home).join(".claude"))
        })?;
    Some(config_dir.join("projects"))
}

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
    mut entries: Vec<TranscriptEntry>,
    options: SessionMessagesOptions,
) -> Vec<SessionMessage> {
    apply_compact_boundaries(&mut entries);
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

fn apply_compact_boundaries(entries: &mut [TranscriptEntry]) {
    let mut by_uuid = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.uuid().map(|uuid| (uuid, index)))
        .collect::<HashMap<_, _>>();

    for index in 0..entries.len() {
        let entry = entries[index].clone();
        if entry.kind != TranscriptEntryKind::System
            || entry.subtype.as_deref() != Some("compact_boundary")
        {
            continue;
        }
        let Some(metadata) = entry.compact_metadata else {
            continue;
        };
        if let Some(preserved) = metadata.preserved_messages {
            if preserved.uuids.is_empty()
                || preserved
                    .uuids
                    .iter()
                    .any(|uuid| !by_uuid.contains_key(uuid))
            {
                continue;
            }
            let mut parent = preserved.anchor_uuid;
            for uuid in &preserved.uuids {
                let entry_index = by_uuid[uuid];
                entries[entry_index].parent_uuid = Some(parent);
                parent = *uuid;
            }
            let first = preserved.uuids[0];
            let last = *preserved.uuids.last().expect("not empty");
            for entry in entries.iter_mut() {
                if entry.parent_uuid == Some(preserved.anchor_uuid) && entry.uuid != Some(first) {
                    entry.parent_uuid = Some(last);
                }
            }
        } else if let Some(preserved) = metadata.preserved_segment
            && let Some(head_index) = by_uuid.get(&preserved.head_uuid).copied()
        {
            entries[head_index].parent_uuid = Some(preserved.anchor_uuid);
            for entry in entries.iter_mut() {
                if entry.parent_uuid == Some(preserved.anchor_uuid)
                    && entry.uuid != Some(preserved.head_uuid)
                {
                    entry.parent_uuid = Some(preserved.tail_uuid);
                }
            }
        }
        by_uuid = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.uuid().map(|uuid| (uuid, index)))
            .collect();
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
        let Some(parent_uuid) = current.parent_uuid else {
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

    fn entry(kind: TranscriptEntryKind, uuid: Uuid, parent_uuid: Option<Uuid>) -> TranscriptEntry {
        TranscriptEntry {
            kind,
            uuid: Some(uuid),
            session_id: Some(uuid::uuid!("00000000-0000-4000-8000-000000000001")),
            parent_uuid,
            message: json!({"role": "user", "content": "hello"}),
            timestamp: None,
            parent_tool_use_id: None,
            is_meta: None,
            is_sidechain: None,
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
