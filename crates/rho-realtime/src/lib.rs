//! Native realtime voice sessions.
//!
//! This crate owns WebRTC, audio devices, and the provider data-channel
//! protocol. Consumers supply signaling and handle typed session events.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use interceptor::registry::Registry;
use media::Sample;
use opus_rs::{Application, OpusDecoder, OpusEncoder};
use rho_inference::ResolvedOAuth;
use rodio::microphone::MicrophoneBuilder;
use rodio::{ChannelCount, DeviceSinkBuilder, MixerDeviceSink, SampleRate, Source as _};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

const SAMPLE_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 100;
const MAX_PROVIDER_EVENT_BYTES: usize = 1024 * 1024;
const CONTEXT_APPEND_MAX_BYTES: usize = 500;
const MAX_SDP_BYTES: usize = 256 * 1024;
const CALL_URL: &str =
    "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";

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

/// Exchange a native WebRTC offer using a daemon-resolved OAuth credential.
/// Keeping credential resolution outside this crate lets GUI clients signal
/// through a local or remote daemon without receiving the bearer token.
pub async fn create_call(
    credential: ResolvedOAuth,
    offer_sdp: SdpOffer,
) -> anyhow::Result<SdpAnswer> {
    let account_id = credential
        .account_id
        .context("realtime requires a ChatGPT account id")?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let body = CreateCallRequest {
        sdp: offer_sdp.into_string(),
        session: CreateCallSession {
            model: RealtimeModel::GptLive1BoulderAlpha,
            instructions: "You are Rho's realtime voice interface. Be concise. Delegate work that \
                           needs tools or durable agent state to the client."
                .to_owned(),
            audio: SessionAudio {
                output: AudioOutput { voice: Voice::Cove },
            },
            delegation: SessionDelegation {
                delegation_type: SessionDelegationType::Client,
            },
        },
    };
    let mut response = reqwest::Client::new()
        .post(CALL_URL)
        .bearer_auth(credential.bearer_token)
        .header("chatgpt-account-id", account_id)
        .header("openai-alpha", "quicksilver=v2")
        .header("x-session-id", &session_id)
        .header("session-id", &session_id)
        .header("thread-id", uuid::Uuid::new_v4().to_string())
        .header("x-codex-installation-id", uuid::Uuid::new_v4().to_string())
        .header("originator", "rho_gui")
        .header("user-agent", "rho-gui")
        .json(&body)
        .send()
        .await
        .context("create realtime WebRTC call")?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read realtime call response")?
    {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_SDP_BYTES,
            "realtime call response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let detail = serde_json::from_slice::<ApiErrorEnvelope>(&bytes)
            .ok()
            .map(|response| response.error.message)
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(500).collect());
        anyhow::bail!("realtime call creation failed with {status}: {detail}");
    }
    let answer = String::from_utf8(bytes).context("decode realtime SDP answer")?;
    SdpAnswer::try_from(answer).context("provider returned an invalid SDP answer")
}

#[derive(Serialize)]
struct CreateCallRequest {
    sdp: String,
    session: CreateCallSession,
}

#[derive(Serialize)]
struct CreateCallSession {
    model: RealtimeModel,
    instructions: String,
    audio: SessionAudio,
    delegation: SessionDelegation,
}

#[derive(Serialize)]
enum RealtimeModel {
    #[serde(rename = "gpt-live-1-boulder-alpha")]
    GptLive1BoulderAlpha,
}

#[derive(Serialize)]
struct SessionAudio {
    output: AudioOutput,
}

#[derive(Serialize)]
struct AudioOutput {
    voice: Voice,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Voice {
    Cove,
}

#[derive(Serialize)]
struct SessionDelegation {
    #[serde(rename = "type")]
    delegation_type: SessionDelegationType,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionDelegationType {
    Client,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: ApiError,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegateRequest {
    pub id: DelegateRequestId,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegateRequestId(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RealtimeEvent {
    DelegateRequest(DelegateRequest),
    Error(String),
    Closed,
}

pub struct RealtimeSession {
    peer: Arc<RTCPeerConnection>,
    data_channel: Arc<RTCDataChannel>,
    events: mpsc::Receiver<RealtimeEvent>,
    microphone_task: tokio::task::JoinHandle<()>,
    _output: MixerDeviceSink,
}

impl RealtimeSession {
    /// Establish a native voice session using the caller's OAuth signaling
    /// function to exchange an SDP offer for an SDP answer.
    pub async fn connect<S, F>(signal: S) -> anyhow::Result<Self>
    where
        S: FnOnce(SdpOffer) -> F,
        F: Future<Output = anyhow::Result<SdpAnswer>>,
    {
        tracing::info!("creating realtime WebRTC peer");
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        let peer = Arc::new(
            api.new_peer_connection(RTCConfiguration {
                ice_servers: vec![RTCIceServer {
                    urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            })
            .await?,
        );
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: SAMPLE_RATE,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                ..Default::default()
            },
            "rho-microphone".to_owned(),
            "rho-realtime".to_owned(),
        ));
        peer.add_track(audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let data_channel = peer.create_data_channel("oai-events", None).await?;
        tracing::info!(label = "oai-events", "created realtime data channel");

        peer.on_ice_gathering_state_change(Box::new(|state| {
            Box::pin(async move {
                tracing::info!(?state, "realtime ICE gathering state changed");
            })
        }));
        peer.on_ice_connection_state_change(Box::new(|state| {
            Box::pin(async move {
                tracing::info!(?state, "realtime ICE connection state changed");
            })
        }));

        let (playback_tx, playback_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        peer.on_track(Box::new(move |track, _, _| {
            let playback_tx = playback_tx.clone();
            Box::pin(async move {
                let codec = track.codec();
                tracing::info!(
                    track_id = %track.id(),
                    stream_id = %track.stream_id(),
                    kind = ?track.kind(),
                    mime_type = %codec.capability.mime_type,
                    clock_rate = codec.capability.clock_rate,
                    channels = codec.capability.channels,
                    "received realtime remote track"
                );
                let mut mono = OpusDecoder::new(SAMPLE_RATE as i32, 1).ok();
                let mut stereo = OpusDecoder::new(SAMPLE_RATE as i32, 2).ok();
                let mut packet_count = 0_u64;
                let mut decode_error_count = 0_u64;
                loop {
                    let packet = match track.read_rtp().await {
                        Ok((packet, _)) => packet,
                        Err(error) => {
                            tracing::warn!(%error, packet_count, "realtime remote RTP track ended");
                            break;
                        }
                    };
                    packet_count += 1;
                    if packet_count == 1 || packet_count.is_multiple_of(500) {
                        tracing::debug!(
                            packet_count,
                            payload_bytes = packet.payload.len(),
                            "received realtime audio RTP"
                        );
                    }
                    let mut decoded = vec![0.0_f32; 5760 * 2];
                    let samples = mono
                        .as_mut()
                        .and_then(|decoder| {
                            decoder.decode(&packet.payload, 5760, &mut decoded).ok()
                        })
                        .map(|samples| (samples, 1))
                        .or_else(|| {
                            stereo.as_mut().and_then(|decoder| {
                                decoder
                                    .decode(&packet.payload, 5760, &mut decoded)
                                    .ok()
                                    .map(|samples| (samples, 2))
                            })
                        });
                    let Some((samples, channels)) = samples else {
                        decode_error_count += 1;
                        if decode_error_count == 1 || decode_error_count.is_multiple_of(100) {
                            tracing::warn!(
                                decode_error_count,
                                packet_count,
                                "failed to decode realtime Opus packet"
                            );
                        }
                        continue;
                    };
                    let playback = if channels == 1 {
                        decoded.truncate(samples);
                        decoded
                    } else {
                        decoded[..samples * 2]
                            .chunks_exact(2)
                            .map(|pair| (pair[0] + pair[1]) * 0.5)
                            .collect()
                    };
                    match playback_tx.try_send(playback) {
                        Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            })
        }));

        let offer = peer.create_offer(None).await?;
        let mut gathering_complete = peer.gathering_complete_promise().await;
        peer.set_local_description(offer).await?;
        tracing::info!("waiting for realtime ICE candidate gathering");
        tokio::time::timeout(Duration::from_secs(10), gathering_complete.recv())
            .await
            .context("timed out gathering realtime ICE candidates")?;
        let offer_sdp = SdpOffer::try_from(
            peer.local_description()
                .await
                .context("WebRTC peer has no local description")?
                .sdp,
        )?;
        tracing::info!(
            offer_bytes = offer_sdp.0.len(),
            "realtime ICE gathering complete; signaling offer"
        );
        let answer_sdp = signal(offer_sdp).await?;
        tracing::info!(
            answer_bytes = answer_sdp.0.len(),
            "received realtime signaling answer"
        );
        let answer =
            RTCSessionDescription::answer(answer_sdp.0).context("parse realtime SDP answer")?;

        let (event_tx, event_rx) = mpsc::channel(64);
        let provider_events = event_tx.clone();
        data_channel.on_message(Box::new(move |message: DataChannelMessage| {
            let provider_events = provider_events.clone();
            Box::pin(async move {
                tracing::debug!(
                    bytes = message.data.len(),
                    is_string = message.is_string,
                    "received realtime data-channel message"
                );
                if !message.is_string {
                    return;
                }
                let event = match ProviderEvent::from_json(&message.data) {
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
                            })
                        }
                    }
                    Ok(ProviderEvent::Other) => return,
                    Err(error) => RealtimeEvent::Error(error.to_string()),
                };
                let _ = provider_events.try_send(event);
            })
        }));
        let (open_tx, open_rx) = oneshot::channel();
        let open_tx = Arc::new(std::sync::Mutex::new(Some(open_tx)));
        data_channel.on_open(Box::new(move || {
            let open_tx = open_tx.clone();
            Box::pin(async move {
                tracing::info!(label = "oai-events", "realtime data channel opened");
                if let Some(sender) = open_tx.lock().unwrap().take() {
                    let _ = sender.send(());
                }
            })
        }));
        data_channel.on_close(Box::new(|| {
            Box::pin(async move {
                tracing::info!(label = "oai-events", "realtime data channel closed");
            })
        }));
        data_channel.on_error(Box::new(|error| {
            Box::pin(async move {
                tracing::warn!(%error, label = "oai-events", "realtime data channel error");
            })
        }));
        peer.on_peer_connection_state_change(Box::new(move |state| {
            let event_tx = event_tx.clone();
            Box::pin(async move {
                tracing::info!(?state, "realtime peer connection state changed");
                if matches!(
                    state,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) {
                    let _ = event_tx.try_send(RealtimeEvent::Closed);
                }
            })
        }));
        tracing::info!("setting realtime remote description");
        peer.set_remote_description(answer).await?;
        tracing::info!("set realtime remote description");

        tracing::info!("opening realtime audio output");
        let mut output =
            DeviceSinkBuilder::open_default_sink().context("open realtime audio output")?;
        tracing::info!("realtime audio output opened; installing playback source");
        output.log_on_drop(false);
        output.mixer().add(RealtimePlayback::new(playback_rx));
        tracing::info!("realtime playback source installed; waiting for data channel");
        match tokio::time::timeout(Duration::from_secs(15), open_rx).await {
            Ok(result) => result.context("realtime data channel closed before opening")?,
            Err(error) => {
                tracing::warn!(%error, "timed out opening realtime data channel");
                return Err(error).context("timed out opening realtime data channel");
            }
        }
        tracing::info!("starting realtime microphone");
        let microphone_task = start_microphone(audio_track)?;
        tracing::info!("realtime session connected and microphone started");

        Ok(Self {
            peer,
            data_channel,
            events: event_rx,
            microphone_task,
            _output: output,
        })
    }

    pub async fn next_event(&mut self) -> Option<RealtimeEvent> {
        self.events.recv().await
    }

    pub async fn resolve_delegate(
        &self,
        request_id: DelegateRequestId,
        text: &str,
    ) -> anyhow::Result<()> {
        for chunk in utf8_chunks(text, CONTEXT_APPEND_MAX_BYTES) {
            let command = ProviderCommand::DelegationContextAppend {
                delegation_item_id: request_id.0.clone(),
                channel: DelegationChannel::Speakable,
                content: vec![ProviderCommandContent::InputText {
                    text: chunk.to_owned(),
                }],
            };
            self.data_channel
                .send(&Bytes::from(command.to_json()?))
                .await?;
        }
        Ok(())
    }
}

impl Drop for RealtimeSession {
    fn drop(&mut self) {
        self.microphone_task.abort();
        let peer = self.peer.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = peer.close().await;
            });
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ProviderEvent {
    #[serde(rename = "delegation.created")]
    DelegationCreated { item: DelegationItem },
    #[serde(other)]
    Other,
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
}

impl ProviderCommand {
    fn to_json(&self) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec(self).context("encode realtime provider command")
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum DelegationChannel {
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

fn start_microphone(
    track: Arc<TrackLocalStaticSample>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let microphone = MicrophoneBuilder::new()
        .default_device()?
        .default_config()?
        .prefer_sample_rates([rodio::nz!(48_000)])
        .prefer_channel_counts([rodio::nz!(1)])
        .open_stream()?;
    let sample_rate = microphone.sample_rate().get();
    let channels = microphone.channels().get() as usize;
    tracing::info!(sample_rate, channels, "opened realtime microphone");
    anyhow::ensure!(
        sample_rate == SAMPLE_RATE,
        "microphone does not support 48 kHz audio"
    );
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
    std::thread::Builder::new()
        .name("rho-realtime-microphone".to_owned())
        .spawn(move || {
            let mut microphone = microphone;
            let mut encoder = match OpusEncoder::new(SAMPLE_RATE as i32, 1, Application::Voip) {
                Ok(encoder) => encoder,
                Err(error) => {
                    tracing::warn!(?error, "failed to create realtime Opus encoder");
                    return;
                }
            };
            encoder.bitrate_bps = 24_000;
            encoder.use_inband_fec = true;
            let mut packet_count = 0_u64;
            loop {
                let mut frame = Vec::with_capacity(FRAME_SAMPLES);
                for _ in 0..FRAME_SAMPLES {
                    let mut mixed = 0.0_f32;
                    for _ in 0..channels {
                        let Some(sample) = microphone.next() else {
                            tracing::warn!(packet_count, "realtime microphone stream ended");
                            return;
                        };
                        mixed += sample;
                    }
                    let sample = (mixed / channels as f32).clamp(-1.0, 1.0);
                    frame.push(sample);
                }
                let mut packet = vec![0_u8; 4_000];
                let packet_len = match encoder.encode(&frame, FRAME_SAMPLES, &mut packet) {
                    Ok(packet_len) => packet_len,
                    Err(error) => {
                        tracing::warn!(
                            ?error,
                            packet_count,
                            "failed to encode realtime microphone audio"
                        );
                        return;
                    }
                };
                packet.truncate(packet_len);
                packet_count += 1;
                if packet_count == 1 || packet_count.is_multiple_of(500) {
                    tracing::debug!(
                        packet_count,
                        packet_bytes = packet_len,
                        "encoded realtime microphone audio"
                    );
                }
                if tx.blocking_send(packet).is_err() {
                    tracing::debug!(packet_count, "realtime microphone sender closed");
                    return;
                }
            }
        })?;
    Ok(tokio::spawn(async move {
        while let Some(packet) = rx.recv().await {
            if let Err(error) = track
                .write_sample(&Sample {
                    data: Bytes::from(packet),
                    duration: Duration::from_millis(10),
                    ..Default::default()
                })
                .await
            {
                tracing::warn!(%error, "failed to write realtime microphone sample");
                break;
            }
        }
    }))
}

struct RealtimePlayback {
    receiver: std::sync::mpsc::Receiver<Vec<f32>>,
    buffered: VecDeque<f32>,
}

impl RealtimePlayback {
    fn new(receiver: std::sync::mpsc::Receiver<Vec<f32>>) -> Self {
        Self {
            receiver,
            buffered: VecDeque::new(),
        }
    }
}

impl Iterator for RealtimePlayback {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(sample) = self.buffered.pop_front() {
                return Some(sample);
            }
            match self.receiver.try_recv() {
                Ok(samples) => self.buffered.extend(samples),
                Err(std::sync::mpsc::TryRecvError::Empty) => return Some(0.0),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            }
        }
    }
}

impl rodio::Source for RealtimePlayback {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        rodio::nz!(1)
    }
    fn sample_rate(&self) -> SampleRate {
        rodio::nz!(48_000)
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_delegate_request() {
        let event = ProviderEvent::from_json(br#"{"type":"delegation.created","item":{"type":"delegation","target":"client","id":"d1","content":[{"type":"input_text","text":"do it"}]}}"#).unwrap();
        assert!(matches!(event, ProviderEvent::DelegationCreated { .. }));
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
