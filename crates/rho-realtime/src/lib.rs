//! Realtime voice sessions.
//!
//! This crate owns WebRTC, audio devices, and the provider data-channel
//! protocol. Consumers supply signaling and handle typed session events.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

const MAX_PROVIDER_EVENT_BYTES: usize = 1024 * 1024;
const CONTEXT_APPEND_MAX_BYTES: usize = 500;
const MAX_SDP_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpOffer(String);

impl TryFrom<String> for SdpOffer {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_sdp(&value, "offer")?;
        Ok(Self(value))
    }
}

impl SdpOffer {
    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpAnswer(String);

impl TryFrom<String> for SdpAnswer {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_sdp(&value, "answer")?;
        Ok(Self(value))
    }
}

impl SdpAnswer {
    pub fn into_string(self) -> String {
        self.0
    }
}

fn validate_sdp(value: &str, kind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(value.len() <= MAX_SDP_BYTES, "SDP {kind} is too large");
    anyhow::ensure!(value.starts_with("v=0"), "invalid SDP {kind}");
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegateRequest {
    pub id: DelegateRequestId,
    pub text: String,
    /// Role-bearing conversation snapshot captured when delegation occurred.
    pub transcript_delta: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegateRequestId(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RealtimeEvent {
    DelegateRequest(DelegateRequest),
    TranscriptDelta { role: TranscriptRole, delta: String },
    TranscriptDone { role: TranscriptRole, text: String },
    Error(String),
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptRole {
    User,
    Assistant,
}

#[derive(Default)]
struct TranscriptState {
    entries: Vec<(TranscriptRole, String)>,
    open_role: Option<TranscriptRole>,
}

const MAX_TRANSCRIPT_CONTEXT_BYTES: usize = 16 * 1024;

impl TranscriptState {
    fn delta(&mut self, role: TranscriptRole, delta: &str) {
        if self.open_role == Some(role)
            && let Some((_, text)) = self.entries.last_mut()
        {
            text.push_str(delta);
        } else {
            self.entries.push((role, delta.to_owned()));
            self.open_role = Some(role);
        }
        self.bound();
    }

    fn done(&mut self, role: TranscriptRole, text: &str) {
        if self.open_role == Some(role)
            && let Some((_, last_text)) = self.entries.last_mut()
        {
            *last_text = text.to_owned();
        } else {
            self.entries.push((role, text.to_owned()));
        }
        self.open_role = None;
        self.bound();
    }

    fn take_snapshot(&mut self) -> String {
        self.open_role = None;
        std::mem::take(&mut self.entries)
            .iter()
            .map(|(role, text)| {
                format!(
                    "{}: {}",
                    match role {
                        TranscriptRole::User => "user",
                        TranscriptRole::Assistant => "assistant",
                    },
                    text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn take_tail(&mut self) -> Option<String> {
        let tail = self.take_snapshot();
        (!tail.trim().is_empty()).then_some(tail)
    }

    fn bound(&mut self) {
        while self
            .entries
            .iter()
            .map(|(_, text)| text.len())
            .sum::<usize>()
            > MAX_TRANSCRIPT_CONTEXT_BYTES
            && self.entries.len() > 1
        {
            self.entries.remove(0);
        }
        if let Some((_, text)) = self.entries.first_mut()
            && text.len() > MAX_TRANSCRIPT_CONTEXT_BYTES
        {
            let mut start = text.len() - MAX_TRANSCRIPT_CONTEXT_BYTES;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            *text = format!("…{}", &text[start..]);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelegateResponseChannel {
    Commentary,
    Speakable,
}

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::RealtimeSession;

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
pub use browser::RealtimeSession;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ProviderEvent {
    #[serde(rename = "delegation.created")]
    DelegationCreated { item: DelegationItem },
    #[serde(rename = "conversation.input_transcript.delta")]
    InputTranscriptDelta { delta: String },
    #[serde(rename = "conversation.item.input_audio_transcription.delta")]
    InputAudioTranscriptDelta { delta: String },
    #[serde(rename = "conversation.input_transcript.turn_marked")]
    InputTranscriptMarked { transcript: String },
    #[serde(rename = "conversation.item.input_audio_transcription.completed")]
    InputAudioTranscriptDone { transcript: String },
    #[serde(rename = "input_transcript.added")]
    InputTranscriptAdded { item: TranscriptItem },
    #[serde(rename = "conversation.output_transcript.delta")]
    OutputTranscriptDelta { delta: String },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    #[serde(rename = "response.output_audio_transcript.delta")]
    OutputAudioTranscriptDelta { delta: String },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone { text: String },
    #[serde(rename = "response.output_audio_transcript.done")]
    OutputAudioTranscriptDone { transcript: String },
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
    text: String,
}

#[derive(Deserialize)]
struct TranscriptTurn {
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

impl ProviderEvent {
    fn from_json(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_PROVIDER_EVENT_BYTES,
            "realtime provider event exceeds {MAX_PROVIDER_EVENT_BYTES} bytes"
        );
        serde_json::from_slice(bytes).context("decode realtime provider event")
    }
}

#[derive(Clone, Copy)]
enum EventLane {
    General,
    Transcript,
}

fn process_provider_message(
    bytes: &[u8],
    transcript: &mut TranscriptState,
) -> Option<(EventLane, RealtimeEvent)> {
    let event = match ProviderEvent::from_json(bytes) {
        Ok(ProviderEvent::DelegationCreated { item }) => {
            let text = item
                .content
                .into_iter()
                .filter_map(|part| match part {
                    DelegationContent::InputText { text } => Some(text),
                    DelegationContent::Unsupported => None,
                })
                .collect::<String>();
            if item.id.is_empty() {
                RealtimeEvent::Error("realtime delegation id is empty".to_owned())
            } else if text.is_empty() {
                RealtimeEvent::Error("realtime delegation text is empty".to_owned())
            } else {
                RealtimeEvent::DelegateRequest(DelegateRequest {
                    id: DelegateRequestId(item.id),
                    text,
                    transcript_delta: transcript.take_snapshot(),
                })
            }
        }
        Ok(ProviderEvent::InputTranscriptDelta { delta })
        | Ok(ProviderEvent::InputAudioTranscriptDelta { delta })
        | Ok(ProviderEvent::InputTranscriptAdded {
            item: TranscriptItem { text: delta },
        }) => {
            transcript.delta(TranscriptRole::User, &delta);
            return Some((
                EventLane::Transcript,
                RealtimeEvent::TranscriptDelta {
                    role: TranscriptRole::User,
                    delta,
                },
            ));
        }
        Ok(ProviderEvent::InputTranscriptMarked { transcript: text })
        | Ok(ProviderEvent::InputAudioTranscriptDone { transcript: text }) => {
            transcript.done(TranscriptRole::User, &text);
            return Some((
                EventLane::Transcript,
                RealtimeEvent::TranscriptDone {
                    role: TranscriptRole::User,
                    text,
                },
            ));
        }
        Ok(ProviderEvent::OutputTranscriptDelta { delta })
        | Ok(ProviderEvent::OutputTextDelta { delta })
        | Ok(ProviderEvent::OutputAudioTranscriptDelta { delta })
        | Ok(ProviderEvent::OutputTranscriptAdded {
            item: TranscriptItem { text: delta },
        }) => {
            transcript.delta(TranscriptRole::Assistant, &delta);
            return Some((
                EventLane::Transcript,
                RealtimeEvent::TranscriptDelta {
                    role: TranscriptRole::Assistant,
                    delta,
                },
            ));
        }
        Ok(ProviderEvent::OutputTextDone { text })
        | Ok(ProviderEvent::OutputAudioTranscriptDone { transcript: text }) => {
            transcript.done(TranscriptRole::Assistant, &text);
            return Some((
                EventLane::Transcript,
                RealtimeEvent::TranscriptDone {
                    role: TranscriptRole::Assistant,
                    text,
                },
            ));
        }
        Ok(ProviderEvent::TurnDone { turn }) => {
            let role = match turn.role {
                TranscriptRoleWire::User => TranscriptRole::User,
                TranscriptRoleWire::Assistant => TranscriptRole::Assistant,
            };
            transcript.done(role, &turn.transcript);
            return Some((
                EventLane::Transcript,
                RealtimeEvent::TranscriptDone {
                    role,
                    text: turn.transcript,
                },
            ));
        }
        Ok(ProviderEvent::Error { message, error }) => {
            let (message, code) = match error {
                Some(error) => (error.message, error.code),
                None => (
                    message.unwrap_or_else(|| "realtime provider reported an error".to_owned()),
                    None,
                ),
            };
            RealtimeEvent::Error(match code {
                Some(code) => format!("{message} ({code})"),
                None => message,
            })
        }
        Ok(ProviderEvent::Other) => return None,
        Err(error) => RealtimeEvent::Error(error.to_string()),
    };
    Some((EventLane::General, event))
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
enum ProviderCommand {
    #[serde(rename = "delegation.context.append")]
    DelegationContextAppend {
        delegation_item_id: String,
        channel: DelegationChannel,
        content: Vec<ProviderCommandContent>,
    },
    #[serde(rename = "session.context.append")]
    SessionContextAppend {
        channel: DelegationChannel,
        content: Vec<ProviderCommandContent>,
    },
}

impl ProviderCommand {
    fn to_json(&self) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec(self).context("encode realtime provider command")
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum DelegationChannel {
    Commentary,
    Speakable,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ProviderCommandContent {
    #[serde(rename = "input_text")]
    InputText { text: String },
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
    #[cfg(not(target_arch = "wasm32"))]
    use super::native::{RealtimePlayback, add_ice_candidates};
    use super::*;

    #[test]
    fn decodes_delegate_request() {
        let event = ProviderEvent::from_json(br#"{"type":"delegation.created","item":{"type":"delegation","target":"client","id":"d1","content":[{"type":"input_text","text":"do it"}]}}"#).unwrap();
        assert!(matches!(event, ProviderEvent::DelegationCreated { .. }));
    }

    #[test]
    fn decodes_provider_error() {
        let event = ProviderEvent::from_json(
            br#"{"type":"error","error":{"code":"bad_audio","message":"Invalid audio"}}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            ProviderEvent::Error {
                error: Some(ProviderError { code: Some(code), message }),
                ..
            } if code == "bad_audio" && message == "Invalid audio"
        ));
    }

    #[test]
    fn rejects_oversized_provider_events() {
        let event = vec![b' '; MAX_PROVIDER_EVENT_BYTES + 1];
        assert!(ProviderEvent::from_json(&event).is_err());
    }

    #[test]
    fn frameless_transcript_chunks_and_turn_completion_decode() {
        let added = ProviderEvent::from_json(
            br#"{"type":"input_transcript.added","item":{"text":"hello "}}"#,
        )
        .unwrap();
        assert!(matches!(
            added,
            ProviderEvent::InputTranscriptAdded {
                item: TranscriptItem { text }
            } if text == "hello "
        ));
        let done = ProviderEvent::from_json(
            br#"{"type":"turn.done","turn":{"role":"user","transcript":"hello world"}}"#,
        )
        .unwrap();
        assert!(matches!(
            done,
            ProviderEvent::TurnDone {
                turn: TranscriptTurn {
                    role: TranscriptRoleWire::User,
                    transcript
                }
            } if transcript == "hello world"
        ));
    }

    #[test]
    fn transcript_snapshots_are_incremental_and_preserve_turn_boundaries() {
        let mut transcript = TranscriptState::default();
        transcript.delta(TranscriptRole::User, "hello ");
        transcript.delta(TranscriptRole::User, "world");
        transcript.done(TranscriptRole::User, "hello world");
        transcript.delta(TranscriptRole::User, "second turn");
        assert_eq!(
            transcript.take_snapshot(),
            "user: hello world\nuser: second turn"
        );
        assert_eq!(transcript.take_snapshot(), "");
        transcript.delta(TranscriptRole::Assistant, "done");
        assert_eq!(transcript.take_tail().as_deref(), Some("assistant: done"));
    }

    #[test]
    fn transcript_context_is_bounded() {
        let mut transcript = TranscriptState::default();
        transcript.delta(
            TranscriptRole::User,
            &"x".repeat(MAX_TRANSCRIPT_CONTEXT_BYTES * 2),
        );
        assert!(transcript.take_snapshot().len() <= MAX_TRANSCRIPT_CONTEXT_BYTES + 16);
    }

    #[test]
    fn encodes_typed_delegate_response() {
        let command = ProviderCommand::DelegationContextAppend {
            delegation_item_id: "d1".to_owned(),
            channel: DelegationChannel::Speakable,
            content: vec![ProviderCommandContent::InputText {
                text: "done".to_owned(),
            }],
        };
        assert_eq!(
            String::from_utf8(command.to_json().unwrap()).unwrap(),
            r#"{"type":"delegation.context.append","delegation_item_id":"d1","channel":"speakable","content":[{"type":"input_text","text":"done"}]}"#
        );
    }

    #[test]
    fn encodes_typed_session_context() {
        let command = ProviderCommand::SessionContextAppend {
            channel: DelegationChannel::Commentary,
            content: vec![ProviderCommandContent::InputText {
                text: "working".to_owned(),
            }],
        };
        assert_eq!(
            String::from_utf8(command.to_json().unwrap()).unwrap(),
            r#"{"type":"session.context.append","channel":"commentary","content":[{"type":"input_text","text":"working"}]}"#
        );
    }

    #[test]
    fn sdp_newtypes_validate_at_the_boundary() {
        assert!(SdpOffer::try_from("not sdp".to_owned()).is_err());
        assert!(SdpAnswer::try_from("v=0\r\n".to_owned()).is_ok());
    }

    #[test]
    fn chunks_on_utf8_boundaries() {
        let text = "a".repeat(499) + "é";
        assert_eq!(
            utf8_chunks(&text, 500)
                .iter()
                .map(|s| s.len())
                .collect::<Vec<_>>(),
            [499, 2]
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn adds_gathered_ice_candidates_to_their_media_sections() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\n";
        let completed = add_ice_candidates(
            sdp,
            &[
                (1, "candidate:data 1 udp 1 127.0.0.1 2 typ host".to_owned()),
                (0, "candidate:audio 1 udp 1 127.0.0.1 1 typ host".to_owned()),
            ],
        )
        .unwrap();
        assert_eq!(
            completed,
            "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=candidate:audio 1 udp 1 127.0.0.1 1 typ host\r\na=end-of-candidates\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\na=candidate:data 1 udp 1 127.0.0.1 2 typ host\r\na=end-of-candidates\r\n"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn playback_emits_silence_without_blocking() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut playback = RealtimePlayback::new(rx);
        assert_eq!(playback.next(), Some(0.0));
        tx.send(vec![0.25, -0.5]).unwrap();
        assert_eq!(playback.next(), Some(0.25));
        assert_eq!(playback.next(), Some(-0.5));
        assert_eq!(playback.next(), Some(0.0));
        drop(tx);
        assert_eq!(playback.next(), None);
    }
}
