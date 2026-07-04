//! Wire vocabulary for the Grok realtime voice protocol.
//!
//! Client events are serialized exactly; server events are parsed
//! defensively from semi-trusted provider JSON. The normal events observed
//! from xAI are modeled explicitly; future/unrecognized event types still
//! land in [`ServerEvent::Unknown`] with their payload intact.

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Message sent by the client over the realtime socket.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum ClientEvent {
    #[serde(rename = "session.update")]
    SessionUpdate { session: SessionConfig },
    /// Streams base64 PCM16 microphone audio into the input buffer.
    #[serde(rename = "input_audio_buffer.append")]
    InputAudioBufferAppend { audio: String },
    /// Ends the user turn when turn detection is manual (`turn_detection:
    /// null`).
    #[serde(rename = "input_audio_buffer.commit")]
    InputAudioBufferCommit,
    #[serde(rename = "input_audio_buffer.clear")]
    InputAudioBufferClear,
    #[serde(rename = "conversation.item.create")]
    ConversationItemCreate { item: ConversationItem },
    #[serde(rename = "response.create")]
    ResponseCreate {
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<ResponseOptions>,
    },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// `Some(None)` serializes as `null`: manual turns, no server VAD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_detection: Option<Option<TurnDetection>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum TurnDetection {
    #[serde(rename = "server_vad")]
    ServerVad {
        #[serde(skip_serializing_if = "Option::is_none")]
        threshold: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        silence_duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prefix_padding_ms: Option<u64>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioConfig {
    pub input: AudioSideConfig,
    pub output: AudioSideConfig,
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioSideConfig {
    pub format: AudioFormat,
}

/// Linear PCM16 little-endian at `rate` Hz; the only format rho uses.
#[derive(Clone, Debug, Serialize)]
pub struct AudioFormat {
    #[serde(rename = "type")]
    pub format_type: &'static str,
    pub rate: u32,
}

impl AudioFormat {
    pub fn pcm(rate: u32) -> Self {
        Self {
            format_type: "audio/pcm",
            rate,
        }
    }
}

impl AudioConfig {
    pub fn pcm(rate: u32) -> Self {
        Self {
            input: AudioSideConfig {
                format: AudioFormat::pcm(rate),
            },
            output: AudioSideConfig {
                format: AudioFormat::pcm(rate),
            },
        }
    }
}

/// Client-executed function tool offered to the voice model.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum ToolDefinition {
    #[serde(rename = "function")]
    Function {
        name: String,
        description: String,
        parameters: Value,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum ConversationItem {
    #[serde(rename = "message")]
    Message {
        role: &'static str,
        content: Vec<ContentItem>,
    },
    /// Result of a function tool call, answered by a follow-up
    /// [`ClientEvent::ResponseCreate`].
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
}

impl ConversationItem {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::Message {
            role: "user",
            content: vec![ContentItem::InputText { text: text.into() }],
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum ContentItem {
    #[serde(rename = "input_text")]
    InputText { text: String },
}

/// Per-turn overrides on `response.create`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ResponseOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

pub fn encode_audio_append(pcm: &[u8]) -> ClientEvent {
    ClientEvent::InputAudioBufferAppend {
        audio: BASE64.encode(pcm),
    }
}

/// Message received from the server.
#[derive(Debug)]
pub enum ServerEvent {
    Ping(PingEvent),
    SessionCreated(SessionCreatedEvent),
    SessionUpdated(SessionUpdatedEvent),
    ConversationCreated(ConversationCreatedEvent),
    ConversationItemAdded(ConversationItemAddedEvent),
    /// Server VAD detected the user speaking: barge-in, flush playback.
    InputAudioSpeechStarted(InputAudioSpeechStartedEvent),
    InputAudioSpeechStopped(InputAudioSpeechStoppedEvent),
    InputAudioBufferCommitted(InputAudioBufferCommittedEvent),
    InputAudioBufferCleared(InputAudioBufferClearedEvent),
    ResponseCreated(ResponseCreatedEvent),
    ResponseOutputItemAdded(ResponseOutputItemEvent),
    ResponseOutputItemDone(ResponseOutputItemEvent),
    ResponseContentPartAdded(ResponseContentPartAddedEvent),
    ResponseContentPartDone(ResponseContentPartDoneEvent),
    /// Decoded PCM16 assistant audio.
    OutputAudioDelta(OutputAudioDeltaEvent),
    OutputAudioDone(OutputAudioDoneEvent),
    OutputAudioTranscriptDelta(TranscriptDeltaEvent),
    OutputAudioTranscriptDone(TranscriptDoneEvent),
    TextDelta(TextDeltaEvent),
    TextDone(TextDoneEvent),
    FunctionCallArgumentsDelta(FunctionCallArgumentsDeltaEvent),
    FunctionCallArgumentsDone(FunctionCallArgumentsDoneEvent),
    ResponseDone(ResponseDoneEvent),
    Error(ErrorEvent),
    Unknown {
        event_type: String,
        raw: Value,
    },
}

#[derive(Debug, Deserialize)]
pub struct PingEvent {
    pub event_id: Option<String>,
    pub previous_item_id: Option<String>,
    pub timestamp: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct SessionCreatedEvent {
    pub event_id: Option<String>,
    pub session: SessionInfo,
}

#[derive(Debug, Deserialize)]
pub struct SessionUpdatedEvent {
    pub event_id: Option<String>,
    pub previous_item_id: Option<String>,
    pub session: SessionInfo,
}

#[derive(Debug, Deserialize)]
pub struct SessionInfo {
    pub id: Option<String>,
    pub instructions: Option<String>,
    pub model: Option<String>,
    pub voice: Option<String>,
    #[serde(default)]
    pub modalities: Vec<String>,
    #[serde(default)]
    pub tools: Vec<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ConversationCreatedEvent {
    pub event_id: Option<String>,
    pub conversation: Option<ConversationRef>,
    pub conversation_id: Option<String>,
}

impl ConversationCreatedEvent {
    pub fn conversation_id(&self) -> Option<&str> {
        self.conversation
            .as_ref()
            .and_then(|conversation| conversation.id.as_deref())
            .or(self.conversation_id.as_deref())
    }
}

#[derive(Debug, Deserialize)]
pub struct ConversationRef {
    pub id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ConversationItemAddedEvent {
    pub event_id: Option<String>,
    pub previous_item_id: Option<String>,
    pub item: RealtimeItem,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RealtimeItem {
    pub id: Option<String>,
    pub object: Option<String>,
    #[serde(rename = "type")]
    pub item_type: Option<String>,
    pub role: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub content: Vec<RealtimeContentPart>,
    pub call_id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
    pub output: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RealtimeContentPart {
    #[serde(rename = "type")]
    pub part_type: Option<String>,
    pub text: Option<String>,
    pub transcript: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InputAudioSpeechStartedEvent {
    pub event_id: Option<String>,
    pub item_id: Option<String>,
    pub audio_start_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct InputAudioSpeechStoppedEvent {
    pub event_id: Option<String>,
    pub item_id: Option<String>,
    pub audio_end_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct InputAudioBufferCommittedEvent {
    pub event_id: Option<String>,
    pub previous_item_id: Option<String>,
    pub item_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InputAudioBufferClearedEvent {
    pub event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseCreatedEvent {
    pub event_id: Option<String>,
    pub previous_item_id: Option<String>,
    pub response: ResponseInfo,
}

impl ResponseCreatedEvent {
    pub fn response_id(&self) -> Option<&str> {
        self.response.id.as_deref()
    }
}

#[derive(Debug, Deserialize)]
pub struct ResponseDoneEvent {
    pub event_id: Option<String>,
    pub previous_item_id: Option<String>,
    pub response_id: Option<String>,
    pub response: Option<ResponseInfo>,
    pub usage: Option<ResponseUsage>,
}

impl ResponseDoneEvent {
    pub fn response_id(&self) -> Option<&str> {
        self.response
            .as_ref()
            .and_then(|response| response.id.as_deref())
            .or(self.response_id.as_deref())
    }

    pub fn status(&self) -> Option<&str> {
        self.response
            .as_ref()
            .and_then(|response| response.status.as_deref())
    }
}

#[derive(Debug, Deserialize)]
pub struct ResponseInfo {
    pub id: Option<String>,
    pub object: Option<String>,
    #[serde(default)]
    pub output: Vec<RealtimeItem>,
    pub status: Option<String>,
    pub status_details: Option<Value>,
    pub usage: Option<ResponseUsage>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ResponseUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub output_audio_seconds: Option<f64>,
    pub input_token_details: Option<Value>,
    pub output_token_details: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseOutputItemEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub output_index: Option<u64>,
    pub previous_item_id: Option<String>,
    pub item: RealtimeItem,
}

#[derive(Debug, Deserialize)]
pub struct ResponseContentPartAddedEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub content_index: Option<u64>,
    pub previous_item_id: Option<String>,
    pub part: RealtimeContentPart,
}

#[derive(Debug, Deserialize)]
pub struct ResponseContentPartDoneEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub content_index: Option<u64>,
    pub previous_item_id: Option<String>,
}

#[derive(Debug)]
pub struct OutputAudioDeltaEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub content_index: Option<u64>,
    pub audio: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct OutputAudioDeltaWireEvent {
    event_id: Option<String>,
    response_id: Option<String>,
    item_id: Option<String>,
    output_index: Option<u64>,
    content_index: Option<u64>,
    delta: String,
}

impl OutputAudioDeltaWireEvent {
    fn decode(self) -> Result<OutputAudioDeltaEvent> {
        Ok(OutputAudioDeltaEvent {
            event_id: self.event_id,
            response_id: self.response_id,
            item_id: self.item_id,
            output_index: self.output_index,
            content_index: self.content_index,
            audio: BASE64
                .decode(self.delta.as_bytes())
                .context("decode audio delta")?,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct OutputAudioDoneEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub content_index: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptDeltaEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub content_index: Option<u64>,
    #[serde(default)]
    pub delta: String,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptDoneEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub content_index: Option<u64>,
    #[serde(default)]
    pub transcript: String,
}

#[derive(Debug, Deserialize)]
pub struct TextDeltaEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub content_index: Option<u64>,
    #[serde(default)]
    pub delta: String,
}

#[derive(Debug, Deserialize)]
pub struct TextDoneEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub content_index: Option<u64>,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct FunctionCallArgumentsDeltaEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub call_id: Option<String>,
    #[serde(default)]
    pub delta: String,
}

#[derive(Debug, Deserialize)]
pub struct FunctionCallArgumentsDoneEvent {
    pub event_id: Option<String>,
    pub response_id: Option<String>,
    pub item_id: Option<String>,
    pub output_index: Option<u64>,
    pub call_id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct ErrorEvent {
    pub event_id: Option<String>,
    pub message: Option<String>,
    pub error: Option<ErrorBody>,
}

impl ErrorEvent {
    pub fn message(&self) -> String {
        self.error
            .as_ref()
            .and_then(|error| error.message.as_deref())
            .or(self.message.as_deref())
            .unwrap_or("unknown error")
            .to_owned()
    }
}

#[derive(Debug, Deserialize)]
pub struct ErrorBody {
    pub message: Option<String>,
    pub code: Option<String>,
    #[serde(rename = "type")]
    pub error_type: Option<String>,
}

/// Parses one server JSON frame. Only malformed JSON or a frame without a
/// string `type` is an error; unknown event types succeed as `Unknown`.
pub fn parse_server_event(text: &str) -> Result<ServerEvent> {
    let raw: Value = serde_json::from_str(text).context("parse realtime server event")?;
    let event_type = raw
        .get("type")
        .and_then(Value::as_str)
        .context("realtime server event has no type")?
        .to_owned();

    Ok(match event_type.as_str() {
        "ping" => ServerEvent::Ping(parse_event(raw, "ping")?),
        "session.created" => ServerEvent::SessionCreated(parse_event(raw, "session.created")?),
        "session.updated" => ServerEvent::SessionUpdated(parse_event(raw, "session.updated")?),
        "conversation.created" => {
            ServerEvent::ConversationCreated(parse_event(raw, "conversation.created")?)
        }
        "conversation.item.added" | "conversation.item.created" => {
            ServerEvent::ConversationItemAdded(parse_event(raw, "conversation.item.added")?)
        }
        "input_audio_buffer.speech_started" => ServerEvent::InputAudioSpeechStarted(parse_event(
            raw,
            "input_audio_buffer.speech_started",
        )?),
        "input_audio_buffer.speech_stopped" => ServerEvent::InputAudioSpeechStopped(parse_event(
            raw,
            "input_audio_buffer.speech_stopped",
        )?),
        "input_audio_buffer.committed" => ServerEvent::InputAudioBufferCommitted(parse_event(
            raw,
            "input_audio_buffer.committed",
        )?),
        "input_audio_buffer.cleared" => {
            ServerEvent::InputAudioBufferCleared(parse_event(raw, "input_audio_buffer.cleared")?)
        }
        "response.created" => ServerEvent::ResponseCreated(parse_event(raw, "response.created")?),
        "response.output_item.added" => {
            ServerEvent::ResponseOutputItemAdded(parse_event(raw, "response.output_item.added")?)
        }
        "response.output_item.done" => {
            ServerEvent::ResponseOutputItemDone(parse_event(raw, "response.output_item.done")?)
        }
        "response.content_part.added" => {
            ServerEvent::ResponseContentPartAdded(parse_event(raw, "response.content_part.added")?)
        }
        "response.content_part.done" => {
            ServerEvent::ResponseContentPartDone(parse_event(raw, "response.content_part.done")?)
        }
        "response.output_audio.delta" | "response.audio.delta" => {
            let wire: OutputAudioDeltaWireEvent = parse_event(raw, "response.output_audio.delta")?;
            ServerEvent::OutputAudioDelta(wire.decode()?)
        }
        "response.output_audio.done" | "response.audio.done" => {
            ServerEvent::OutputAudioDone(parse_event(raw, "response.output_audio.done")?)
        }
        "response.output_audio_transcript.delta" | "response.audio_transcript.delta" => {
            ServerEvent::OutputAudioTranscriptDelta(parse_event(
                raw,
                "response.output_audio_transcript.delta",
            )?)
        }
        "response.output_audio_transcript.done" | "response.audio_transcript.done" => {
            ServerEvent::OutputAudioTranscriptDone(parse_event(
                raw,
                "response.output_audio_transcript.done",
            )?)
        }
        "response.text.delta" | "response.output_text.delta" => {
            ServerEvent::TextDelta(parse_event(raw, "response.output_text.delta")?)
        }
        "response.text.done" | "response.output_text.done" => {
            ServerEvent::TextDone(parse_event(raw, "response.output_text.done")?)
        }
        "response.function_call_arguments.delta" => ServerEvent::FunctionCallArgumentsDelta(
            parse_event(raw, "response.function_call_arguments.delta")?,
        ),
        "response.function_call_arguments.done" => ServerEvent::FunctionCallArgumentsDone(
            parse_event(raw, "response.function_call_arguments.done")?,
        ),
        "response.done" => ServerEvent::ResponseDone(parse_event(raw, "response.done")?),
        "error" => ServerEvent::Error(parse_event(raw, "error")?),
        _ => ServerEvent::Unknown { event_type, raw },
    })
}

fn parse_event<T>(raw: Value, event_name: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(raw).with_context(|| format!("parse {event_name} event"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_update_serializes_null_turn_detection() {
        let event = ClientEvent::SessionUpdate {
            session: SessionConfig {
                voice: Some("eve".to_owned()),
                turn_detection: Some(None),
                audio: Some(AudioConfig::pcm(24000)),
                ..SessionConfig::default()
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "session.update");
        assert_eq!(json["session"]["turn_detection"], Value::Null);
        assert_eq!(json["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert!(json["session"].get("instructions").is_none());
    }

    #[test]
    fn user_text_item_matches_wire_shape() {
        let event = ClientEvent::ConversationItemCreate {
            item: ConversationItem::user_text("hello"),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["item"]["type"], "message");
        assert_eq!(json["item"]["role"], "user");
        assert_eq!(json["item"]["content"][0]["type"], "input_text");
        assert_eq!(json["item"]["content"][0]["text"], "hello");
    }

    #[test]
    fn parses_audio_delta_from_base64() {
        let pcm: &[u8] = &[0x01, 0x02, 0x03, 0x04];
        let text = serde_json::json!({
            "type": "response.output_audio.delta",
            "delta": BASE64.encode(pcm),
        })
        .to_string();
        match parse_server_event(&text).unwrap() {
            ServerEvent::OutputAudioDelta(event) => assert_eq!(event.audio, pcm),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn unknown_event_type_is_preserved_not_rejected() {
        let text = r#"{"type":"rate_limits.updated","limits":[]}"#;
        match parse_server_event(text).unwrap() {
            ServerEvent::Unknown { event_type, raw } => {
                assert_eq!(event_type, "rate_limits.updated");
                assert!(raw.get("limits").is_some());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn function_call_done_tolerates_missing_name() {
        let text =
            r#"{"type":"response.function_call_arguments.done","call_id":"c1","arguments":"{}"}"#;
        match parse_server_event(text).unwrap() {
            ServerEvent::FunctionCallArgumentsDone(event) => {
                assert_eq!(event.call_id, "c1");
                assert_eq!(event.name, None);
                assert_eq!(event.arguments, "{}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_observed_grok_lifecycle_events_as_strong_types() {
        assert!(matches!(
            parse_server_event(r#"{"type":"ping","event_id":"p1","timestamp":123}"#).unwrap(),
            ServerEvent::Ping(PingEvent {
                timestamp: Some(123),
                ..
            })
        ));

        match parse_server_event(
            r#"{"type":"session.created","event_id":"e0","session":{"id":"s1","model":"grok-voice-think-fast-1.0","voice":"ara","modalities":["audio"]}}"#,
        )
        .unwrap()
        {
            ServerEvent::SessionCreated(event) => {
                assert_eq!(event.session.id.as_deref(), Some("s1"));
                assert_eq!(event.session.voice.as_deref(), Some("ara"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"session.updated","event_id":"e1","session":{"id":"s1","model":"grok-voice-think-fast-1.0","voice":"human_Eve","modalities":["audio"]}}"#,
        )
        .unwrap()
        {
            ServerEvent::SessionUpdated(event) => {
                assert_eq!(event.session.id.as_deref(), Some("s1"));
                assert_eq!(event.session.model.as_deref(), Some("grok-voice-think-fast-1.0"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"conversation.created","event_id":"c0","conversation":{"id":"conv1"}}"#,
        )
        .unwrap()
        {
            ServerEvent::ConversationCreated(event) => {
                assert_eq!(event.conversation_id(), Some("conv1"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"conversation.item.added","item":{"id":"i1","type":"message","role":"user","status":"completed","content":[{"type":"input_text","text":"hello"}]}}"#,
        )
        .unwrap()
        {
            ServerEvent::ConversationItemAdded(event) => {
                assert_eq!(event.item.id.as_deref(), Some("i1"));
                assert_eq!(event.item.item_type.as_deref(), Some("message"));
                assert_eq!(event.item.content[0].text.as_deref(), Some("hello"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"conversation.item.added","item":{"id":"out1","type":"function_call_output","role":"tool","status":"completed","call_id":"c1","output":"3 agents"}}"#,
        )
        .unwrap()
        {
            ServerEvent::ConversationItemAdded(event) => {
                assert_eq!(event.item.item_type.as_deref(), Some("function_call_output"));
                assert_eq!(event.item.call_id.as_deref(), Some("c1"));
                assert_eq!(event.item.output.as_deref(), Some("3 agents"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#,
        )
        .unwrap()
        {
            ServerEvent::ResponseCreated(event) => {
                assert_eq!(event.response_id(), Some("r1"));
                assert_eq!(event.response.status.as_deref(), Some("in_progress"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"response.output_item.added","response_id":"r1","output_index":0,"item":{"id":"fc1","type":"function_call","status":"in_progress","call_id":"c1","name":"list_agents"}}"#,
        )
        .unwrap()
        {
            ServerEvent::ResponseOutputItemAdded(event) => {
                assert_eq!(event.response_id.as_deref(), Some("r1"));
                assert_eq!(event.item.call_id.as_deref(), Some("c1"));
                assert_eq!(event.item.name.as_deref(), Some("list_agents"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"response.function_call_arguments.delta","response_id":"r1","item_id":"fc1","call_id":"c1","delta":"{}"}"#,
        )
        .unwrap()
        {
            ServerEvent::FunctionCallArgumentsDelta(event) => {
                assert_eq!(event.call_id.as_deref(), Some("c1"));
                assert_eq!(event.delta, "{}");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"response.content_part.added","response_id":"r1","item_id":"i2","content_index":0,"part":{"type":"audio","transcript":""}}"#,
        )
        .unwrap()
        {
            ServerEvent::ResponseContentPartAdded(event) => {
                assert_eq!(event.item_id.as_deref(), Some("i2"));
                assert_eq!(event.part.part_type.as_deref(), Some("audio"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match parse_server_event(
            r#"{"type":"response.output_audio_transcript.done","response_id":"r1","item_id":"i2","content_index":0,"transcript":"hello"}"#,
        )
        .unwrap()
        {
            ServerEvent::OutputAudioTranscriptDone(event) => {
                assert_eq!(event.transcript, "hello");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert!(matches!(
            parse_server_event(
                r#"{"type":"response.content_part.done","response_id":"r1","item_id":"i2","content_index":0}"#
            )
            .unwrap(),
            ServerEvent::ResponseContentPartDone(_)
        ));
        assert!(matches!(
            parse_server_event(
                r#"{"type":"response.output_audio.done","response_id":"r1","item_id":"i2","content_index":0}"#
            )
            .unwrap(),
            ServerEvent::OutputAudioDone(_)
        ));
        assert!(matches!(
            parse_server_event(
                r#"{"type":"response.output_item.done","response_id":"r1","item":{"id":"i2","type":"message","status":"completed"}}"#
            )
            .unwrap(),
            ServerEvent::ResponseOutputItemDone(_)
        ));

        match parse_server_event(
            r#"{"type":"response.done","response_id":"r1","response":{"id":"r1","status":"completed","output":[]},"usage":{"total_tokens":42}}"#,
        )
        .unwrap()
        {
            ServerEvent::ResponseDone(event) => {
                assert_eq!(event.response_id(), Some("r1"));
                assert_eq!(event.status(), Some("completed"));
                assert_eq!(event.usage.unwrap().total_tokens, Some(42));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn malformed_audio_delta_is_error_not_panic() {
        let text = r#"{"type":"response.output_audio.delta","delta":"not base64!!"}"#;
        assert!(parse_server_event(text).is_err());
    }
}
