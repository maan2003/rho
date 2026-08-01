//! OpenAI realtime sideband transport and typed wire protocol.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::time::Duration;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderMap;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

const CONNECT_ATTEMPTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
const CONTEXT_APPEND_MAX_BYTES: usize = 500;
const MAX_PROVIDER_EVENT_BYTES: usize = 1024 * 1024;
const MAX_DELEGATION_ID_BYTES: usize = 256;
const MAX_TRANSCRIPT_CONTEXT_BYTES: usize = 16 * 1024;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone)]
pub struct SidebandConfig {
    pub call_id: String,
    pub bearer_token: String,
    pub account_id: String,
    pub session_id: String,
    pub thread_id: String,
    pub installation_id: String,
    pub originator: String,
    pub user_agent: String,
}

pub struct Sideband {
    socket: Socket,
}

impl Sideband {
    pub async fn connect(config: &SidebandConfig) -> anyhow::Result<Self> {
        let request = build_request(config)?;
        let mut failure = None;
        for attempt in 0..CONNECT_ATTEMPTS {
            let websocket_config =
                tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                    .max_message_size(Some(MAX_PROVIDER_EVENT_BYTES))
                    .max_frame_size(Some(MAX_PROVIDER_EVENT_BYTES));
            match tokio::time::timeout(
                CONNECT_TIMEOUT,
                connect_async_with_config(request.clone(), Some(websocket_config), false),
            )
            .await
            {
                Ok(Ok((socket, _))) => return Ok(Self { socket }),
                Ok(Err(error)) => failure = Some(anyhow::Error::from(error)),
                Err(error) => failure = Some(anyhow::Error::from(error)),
            }
            if attempt + 1 < CONNECT_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
            }
        }
        Err(failure.unwrap_or_else(|| anyhow::anyhow!("realtime sideband connection failed")))
            .context("connect OpenAI realtime sideband")
    }

    pub async fn next_event(&mut self) -> anyhow::Result<Option<ProviderEvent>> {
        loop {
            let Some(message) = self.socket.next().await.transpose()? else {
                return Ok(None);
            };
            match message {
                Message::Text(text) => return ProviderEvent::from_json(text.as_bytes()).map(Some),
                Message::Ping(payload) => self.socket.send(Message::Pong(payload)).await?,
                Message::Pong(_) => {}
                Message::Close(_) => return Ok(None),
                Message::Binary(_) => anyhow::bail!("realtime sideband returned a binary frame"),
                Message::Frame(_) => {}
            }
        }
    }

    pub async fn append_delegation(
        &mut self,
        delegation_item_id: &str,
        channel: ContextChannel,
        text: &str,
    ) -> anyhow::Result<()> {
        for chunk in utf8_chunks(text, CONTEXT_APPEND_MAX_BYTES) {
            self.send(&ProviderCommand::DelegationContextAppend {
                delegation_item_id,
                channel,
                content: [ProviderCommandContent::InputText { text: chunk }],
            })
            .await?;
        }
        Ok(())
    }

    pub async fn append_session(
        &mut self,
        channel: ContextChannel,
        text: &str,
    ) -> anyhow::Result<()> {
        for chunk in utf8_chunks(text, CONTEXT_APPEND_MAX_BYTES) {
            self.send(&ProviderCommand::SessionContextAppend {
                channel,
                content: [ProviderCommandContent::InputText { text: chunk }],
            })
            .await?;
        }
        Ok(())
    }

    async fn send(&mut self, command: &ProviderCommand<'_>) -> anyhow::Result<()> {
        let text = serde_json::to_string(command).context("encode realtime sideband command")?;
        tokio::time::timeout(SEND_TIMEOUT, self.socket.send(Message::Text(text.into())))
            .await
            .context("timed out sending realtime sideband command")??;
        Ok(())
    }
}

fn build_request(
    config: &SidebandConfig,
) -> anyhow::Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    anyhow::ensure!(valid_call_id(&config.call_id), "invalid realtime call id");
    let url = format!("wss://api.openai.com/v1/live/{}", config.call_id);
    let mut request = url.into_client_request()?;
    let headers = request.headers_mut();
    set_header(
        headers,
        "authorization",
        &format!("Bearer {}", config.bearer_token),
    )?;
    set_header(headers, "chatgpt-account-id", &config.account_id)?;
    set_header(headers, "openai-alpha", "quicksilver=v2")?;
    set_header(headers, "x-session-id", &config.session_id)?;
    set_header(headers, "session-id", &config.session_id)?;
    set_header(headers, "thread-id", &config.thread_id)?;
    set_header(headers, "x-codex-installation-id", &config.installation_id)?;
    set_header(headers, "originator", &config.originator)?;
    set_header(headers, "user-agent", &config.user_agent)?;
    Ok(request)
}

fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> anyhow::Result<()> {
    headers.insert(name, value.parse()?);
    Ok(())
}

pub fn call_id_from_location(location: &str) -> Option<String> {
    location
        .split_once('?')
        .map_or(location, |(path, _)| path)
        .split('/')
        .find(|segment| valid_call_id(segment))
        .map(str::to_owned)
}

fn valid_call_id(value: &str) -> bool {
    value.starts_with("rtc_")
        && value.len() > 4
        && value[4..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextChannel {
    Commentary,
    Speakable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    DelegationCreated {
        id: String,
        text: String,
    },
    TranscriptDelta {
        role: TranscriptRole,
        delta: String,
        item_id: Option<String>,
    },
    TranscriptDone {
        role: TranscriptRole,
        text: String,
        item_id: Option<String>,
    },
    Error(String),
    Other,
}

impl ProviderEvent {
    fn from_json(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_PROVIDER_EVENT_BYTES,
            "realtime provider event exceeds {MAX_PROVIDER_EVENT_BYTES} bytes"
        );
        let event: WireEvent =
            serde_json::from_slice(bytes).context("decode realtime provider event")?;
        Ok(match event {
            WireEvent::DelegationCreated { item } => {
                let text = item
                    .content
                    .into_iter()
                    .filter_map(|part| match part {
                        DelegationContent::InputText { text } => Some(text),
                        DelegationContent::Unsupported => None,
                    })
                    .collect::<String>();
                anyhow::ensure!(!item.id.is_empty(), "realtime delegation id is empty");
                anyhow::ensure!(
                    item.id.len() <= MAX_DELEGATION_ID_BYTES,
                    "realtime delegation id exceeds {MAX_DELEGATION_ID_BYTES} bytes"
                );
                anyhow::ensure!(!text.is_empty(), "realtime delegation text is empty");
                Self::DelegationCreated { id: item.id, text }
            }
            WireEvent::InputTranscriptDelta { delta, item_id }
            | WireEvent::InputAudioTranscriptDelta { delta, item_id }
            | WireEvent::InputTranscriptAdded {
                item:
                    TranscriptItem {
                        id: item_id,
                        text: delta,
                    },
            } => Self::TranscriptDelta {
                role: TranscriptRole::User,
                delta,
                item_id,
            },
            WireEvent::InputTranscriptMarked {
                transcript: text,
                item_id,
            }
            | WireEvent::InputAudioTranscriptDone {
                transcript: text,
                item_id,
            } => Self::TranscriptDone {
                role: TranscriptRole::User,
                text,
                item_id,
            },
            WireEvent::OutputTranscriptDelta { delta, item_id }
            | WireEvent::OutputTextDelta { delta, item_id }
            | WireEvent::OutputAudioTranscriptDelta { delta, item_id }
            | WireEvent::OutputTranscriptAdded {
                item:
                    TranscriptItem {
                        id: item_id,
                        text: delta,
                    },
            } => Self::TranscriptDelta {
                role: TranscriptRole::Assistant,
                delta,
                item_id,
            },
            WireEvent::OutputTextDone { text, item_id }
            | WireEvent::OutputAudioTranscriptDone {
                transcript: text,
                item_id,
            } => Self::TranscriptDone {
                role: TranscriptRole::Assistant,
                text,
                item_id,
            },
            WireEvent::TurnDone { turn } => Self::TranscriptDone {
                role: match turn.role {
                    TranscriptRoleWire::User => TranscriptRole::User,
                    TranscriptRoleWire::Assistant => TranscriptRole::Assistant,
                },
                text: turn.transcript,
                item_id: turn.id,
            },
            WireEvent::Error { message, error } => {
                let (message, code) = match error {
                    Some(error) => (error.message, error.code),
                    None => (
                        message.unwrap_or_else(|| "realtime provider reported an error".to_owned()),
                        None,
                    ),
                };
                Self::Error(match code {
                    Some(code) => format!("{message} ({code})"),
                    None => message,
                })
            }
            WireEvent::Other => Self::Other,
        })
    }
}

#[derive(Default)]
pub struct TranscriptState {
    entries: Vec<TranscriptEntry>,
    seen_item_ids: VecDeque<u64>,
    consumed_unidentified: VecDeque<(TranscriptRole, String)>,
}

struct TranscriptEntry {
    role: TranscriptRole,
    text: String,
    item_id: Option<u64>,
    complete: bool,
}

impl TranscriptState {
    pub fn apply(&mut self, event: &ProviderEvent) {
        match event {
            ProviderEvent::TranscriptDelta {
                role,
                delta,
                item_id,
            } if !delta.is_empty() => self.delta(*role, delta, item_id.as_deref()),
            ProviderEvent::TranscriptDone {
                role,
                text,
                item_id,
            } if !text.is_empty() => self.done(*role, text, item_id.as_deref()),
            _ => {}
        }
    }

    pub fn take_snapshot(&mut self) -> String {
        let item_ids = self
            .entries
            .iter()
            .filter_map(|entry| entry.item_id)
            .collect::<Vec<_>>();
        for item_id in item_ids {
            self.remember_item(item_id);
        }
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.item_id.is_none() && !entry.complete)
        {
            self.consumed_unidentified
                .push_back((entry.role, entry.text.clone()));
            if self.consumed_unidentified.len() > 8 {
                self.consumed_unidentified.pop_front();
            }
        }
        std::mem::take(&mut self.entries)
            .iter()
            .map(|entry| {
                format!(
                    "{}: {}",
                    match entry.role {
                        TranscriptRole::User => "user",
                        TranscriptRole::Assistant => "assistant",
                    },
                    entry.text,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn take_tail(&mut self) -> Option<String> {
        if !self
            .entries
            .iter()
            .any(|entry| entry.role == TranscriptRole::User && !entry.text.trim().is_empty())
        {
            self.take_snapshot();
            return None;
        }
        let tail = self.take_snapshot();
        (!tail.trim().is_empty()).then_some(tail)
    }

    fn delta(&mut self, role: TranscriptRole, delta: &str, item_id: Option<&str>) {
        let item_id = item_id.map(item_fingerprint);
        if item_id.is_some_and(|id| self.seen_item_ids.contains(&id)) {
            return;
        }
        if item_id.is_none() {
            self.consumed_unidentified
                .retain(|(consumed_role, _)| *consumed_role != role);
        }
        let entry = match item_id {
            Some(item_id) => self
                .entries
                .iter_mut()
                .find(|entry| entry.item_id == Some(item_id)),
            None => self
                .entries
                .iter_mut()
                .rev()
                .find(|entry| entry.item_id.is_none() && entry.role == role && !entry.complete),
        };
        if let Some(entry) = entry {
            entry.text.push_str(delta);
        } else {
            self.entries.push(TranscriptEntry {
                role,
                text: delta.to_owned(),
                item_id,
                complete: false,
            });
        }
        self.bound();
    }

    fn done(&mut self, role: TranscriptRole, text: &str, item_id: Option<&str>) {
        let item_id = item_id.map(item_fingerprint);
        if item_id.is_some_and(|id| self.seen_item_ids.contains(&id)) {
            return;
        }
        if item_id.is_none()
            && let Some(index) = self
                .consumed_unidentified
                .iter()
                .position(|done| done.0 == role && done.1 == text)
        {
            self.consumed_unidentified.remove(index);
            return;
        }
        let entry = match item_id {
            Some(item_id) => self
                .entries
                .iter_mut()
                .find(|entry| entry.item_id == Some(item_id)),
            None => self
                .entries
                .iter_mut()
                .rev()
                .find(|entry| entry.item_id.is_none() && entry.role == role && !entry.complete),
        };
        if let Some(entry) = entry {
            entry.text = text.to_owned();
            entry.complete = true;
        } else {
            self.entries.push(TranscriptEntry {
                role,
                text: text.to_owned(),
                item_id,
                complete: true,
            });
        }
        if let Some(item_id) = item_id {
            self.remember_item(item_id);
        }
        self.bound();
    }

    fn remember_item(&mut self, item_id: u64) {
        if !self.seen_item_ids.contains(&item_id) {
            self.seen_item_ids.push_back(item_id);
            if self.seen_item_ids.len() > 128 {
                self.seen_item_ids.pop_front();
            }
        }
    }

    fn bound(&mut self) {
        while transcript_encoded_len(&self.entries) > MAX_TRANSCRIPT_CONTEXT_BYTES
            && self.entries.len() > 1
        {
            let removed = self.entries.remove(0);
            if let Some(item_id) = removed.item_id {
                self.remember_item(item_id);
            } else if !removed.complete {
                self.consumed_unidentified
                    .push_back((removed.role, removed.text));
                if self.consumed_unidentified.len() > 8 {
                    self.consumed_unidentified.pop_front();
                }
            }
        }
        if transcript_encoded_len(&self.entries) > MAX_TRANSCRIPT_CONTEXT_BYTES
            && let Some(entry) = self.entries.first_mut()
        {
            let prefix_bytes = role_prefix(entry.role).len();
            let keep = MAX_TRANSCRIPT_CONTEXT_BYTES.saturating_sub(prefix_bytes + 5);
            let mut start = entry.text.len().saturating_sub(keep);
            while !entry.text.is_char_boundary(start) {
                start += 1;
            }
            entry.text = format!("…{}", &entry.text[start..]);
        }
    }
}

fn item_fingerprint(item_id: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    item_id.hash(&mut hasher);
    hasher.finish()
}

fn role_prefix(role: TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User => "user",
        TranscriptRole::Assistant => "assistant",
    }
}

fn transcript_encoded_len(entries: &[TranscriptEntry]) -> usize {
    entries
        .iter()
        .map(|entry| role_prefix(entry.role).len() + 2 + entry.text.len())
        .sum::<usize>()
        .saturating_add(entries.len().saturating_sub(1))
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum WireEvent {
    #[serde(rename = "delegation.created")]
    DelegationCreated { item: DelegationItem },
    #[serde(rename = "conversation.input_transcript.delta")]
    InputTranscriptDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "conversation.item.input_audio_transcription.delta")]
    InputAudioTranscriptDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "conversation.input_transcript.turn_marked")]
    InputTranscriptMarked {
        transcript: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "conversation.item.input_audio_transcription.completed")]
    InputAudioTranscriptDone {
        transcript: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "input_transcript.added")]
    InputTranscriptAdded { item: TranscriptItem },
    #[serde(rename = "conversation.output_transcript.delta")]
    OutputTranscriptDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.output_audio_transcript.delta")]
    OutputAudioTranscriptDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        text: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.output_audio_transcript.done")]
    OutputAudioTranscriptDone {
        transcript: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "output_transcript.added")]
    OutputTranscriptAdded { item: TranscriptItem },
    #[serde(rename = "turn.done")]
    TurnDone { turn: TranscriptTurn },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        error: Option<ProviderError>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct TranscriptItem {
    #[serde(default)]
    id: Option<String>,
    text: String,
}
#[derive(Deserialize)]
struct TranscriptTurn {
    #[serde(default)]
    id: Option<String>,
    role: TranscriptRoleWire,
    transcript: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TranscriptRoleWire {
    User,
    Assistant,
}
#[derive(Deserialize)]
struct ProviderError {
    message: String,
    #[serde(default)]
    code: Option<String>,
}
#[derive(Deserialize)]
struct DelegationItem {
    #[serde(rename = "type")]
    _item_type: DelegationItemType,
    #[serde(rename = "target")]
    _target: DelegationTarget,
    id: String,
    content: Vec<DelegationContent>,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DelegationItemType {
    Delegation,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DelegationTarget {
    Client,
}
#[derive(Deserialize)]
#[serde(tag = "type")]
enum DelegationContent {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(other)]
    Unsupported,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ProviderCommand<'a> {
    #[serde(rename = "delegation.context.append")]
    DelegationContextAppend {
        delegation_item_id: &'a str,
        channel: ContextChannel,
        content: [ProviderCommandContent<'a>; 1],
    },
    #[serde(rename = "session.context.append")]
    SessionContextAppend {
        channel: ContextChannel,
        content: [ProviderCommandContent<'a>; 1],
    },
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ProviderCommandContent<'a> {
    #[serde(rename = "input_text")]
    InputText { text: &'a str },
}

fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_valid_call_ids() {
        assert_eq!(
            call_id_from_location("https://api.openai.com/v1/live/rtc_a-B_1?x=1").as_deref(),
            Some("rtc_a-B_1")
        );
        assert_eq!(call_id_from_location("https://example.com/not_rtc"), None);
    }

    #[test]
    fn decodes_delegation_and_transcript_snapshot() {
        let event = ProviderEvent::from_json(br#"{"type":"delegation.created","item":{"type":"delegation","target":"client","id":"d1","content":[{"type":"input_text","text":"do it"}]}}"#).unwrap();
        assert_eq!(
            event,
            ProviderEvent::DelegationCreated {
                id: "d1".to_owned(),
                text: "do it".to_owned()
            }
        );
        let mut transcript = TranscriptState::default();
        transcript.apply(&ProviderEvent::TranscriptDelta {
            role: TranscriptRole::User,
            delta: "hello ".to_owned(),
            item_id: Some("u1".to_owned()),
        });
        transcript.apply(&ProviderEvent::TranscriptDone {
            role: TranscriptRole::User,
            text: "hello world".to_owned(),
            item_id: Some("u1".to_owned()),
        });
        assert_eq!(transcript.take_snapshot(), "user: hello world");

        transcript.apply(&ProviderEvent::TranscriptDone {
            role: TranscriptRole::User,
            text: "hello world".to_owned(),
            item_id: Some("u1".to_owned()),
        });
        assert_eq!(transcript.take_snapshot(), "");

        transcript.apply(&ProviderEvent::TranscriptDone {
            role: TranscriptRole::Assistant,
            text: "all done".to_owned(),
            item_id: Some("a1".to_owned()),
        });
        assert_eq!(transcript.take_tail(), None);

        for (role, id) in [
            (TranscriptRole::User, "u2"),
            (TranscriptRole::Assistant, "a2"),
            (TranscriptRole::User, "u3"),
        ] {
            transcript.apply(&ProviderEvent::TranscriptDone {
                role,
                text: "yes".to_owned(),
                item_id: Some(id.to_owned()),
            });
        }
        assert_eq!(
            transcript.take_snapshot(),
            "user: yes\nassistant: yes\nuser: yes"
        );

        transcript.apply(&ProviderEvent::TranscriptDelta {
            role: TranscriptRole::User,
            delta: "par".to_owned(),
            item_id: Some("u4".to_owned()),
        });
        transcript.apply(&ProviderEvent::TranscriptDelta {
            role: TranscriptRole::Assistant,
            delta: "reply".to_owned(),
            item_id: Some("a4".to_owned()),
        });
        transcript.apply(&ProviderEvent::TranscriptDone {
            role: TranscriptRole::User,
            text: "partial".to_owned(),
            item_id: Some("u4".to_owned()),
        });
        assert_eq!(
            transcript.take_snapshot(),
            "user: partial\nassistant: reply"
        );

        for role in [
            TranscriptRole::User,
            TranscriptRole::Assistant,
            TranscriptRole::User,
        ] {
            transcript.apply(&ProviderEvent::TranscriptDone {
                role,
                text: "same".to_owned(),
                item_id: None,
            });
        }
        assert_eq!(
            transcript.take_snapshot(),
            "user: same\nassistant: same\nuser: same"
        );

        transcript.apply(&ProviderEvent::TranscriptDelta {
            role: TranscriptRole::User,
            delta: "late".to_owned(),
            item_id: None,
        });
        assert_eq!(transcript.take_snapshot(), "user: late");
        transcript.apply(&ProviderEvent::TranscriptDone {
            role: TranscriptRole::User,
            text: "late".to_owned(),
            item_id: None,
        });
        assert_eq!(transcript.take_snapshot(), "");

        for index in 0..10_000 {
            transcript.apply(&ProviderEvent::TranscriptDone {
                role: if index % 2 == 0 {
                    TranscriptRole::User
                } else {
                    TranscriptRole::Assistant
                },
                text: format!("x{index}"),
                item_id: Some(format!("item-{index}")),
            });
        }
        assert!(transcript.take_snapshot().len() <= MAX_TRANSCRIPT_CONTEXT_BYTES);
    }

    #[test]
    fn command_chunks_preserve_utf8() {
        let text = "a".repeat(499) + "é";
        assert_eq!(
            utf8_chunks(&text, 500)
                .iter()
                .map(|part| part.len())
                .collect::<Vec<_>>(),
            [499, 2]
        );
    }
}
