//! Native realtime voice sessions.
//!
//! This crate owns WebRTC, audio devices, and the provider data-channel
//! protocol. Consumers supply signaling and handle typed session events.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use futures::StreamExt as _;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::AudioSourceOptions;
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::data_channel::{DataChannel, DataChannelInit, DataChannelState};
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection::{
    IceGatheringState, OfferOptions, PeerConnection, PeerConnectionState,
};
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt as _;
use libwebrtc::peer_connection_factory::{
    ContinualGatheringPolicy, IceServer, PeerConnectionFactory, RtcConfiguration,
};
use libwebrtc::session_description::{SdpType, SessionDescription};
use rho_inference::ResolvedOAuth;
use rodio::microphone::MicrophoneBuilder;
use rodio::source::UniformSourceIterator;
use rodio::{ChannelCount, DeviceSinkBuilder, MixerDeviceSink, SampleRate, Source as _};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 1;
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
    peer: PeerConnection,
    data_channel: DataChannel,
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
        tracing::info!("creating libwebrtc realtime peer");
        let factory = PeerConnectionFactory::default();
        let peer = factory.create_peer_connection(RtcConfiguration {
            ice_servers: vec![IceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                username: String::new(),
                password: String::new(),
            }],
            continual_gathering_policy: ContinualGatheringPolicy::GatherOnce,
            ..Default::default()
        })?;
        let audio_source = NativeAudioSource::new(
            AudioSourceOptions {
                echo_cancellation: true,
                noise_suppression: true,
                auto_gain_control: true,
            },
            SAMPLE_RATE,
            CHANNELS,
            100,
        );
        let audio_track = factory.create_audio_track("rho-microphone", audio_source.clone());
        peer.add_track(audio_track.into(), &["rho-realtime"])?;
        let data_channel = peer.create_data_channel("oai-events", DataChannelInit::default())?;
        tracing::info!(label = "oai-events", "created realtime data channel");

        let (playback_tx, playback_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let runtime = tokio::runtime::Handle::current();
        peer.on_track(Some(Box::new(move |event| {
            let MediaStreamTrack::Audio(track) = event.track else {
                return;
            };
            let playback_tx = playback_tx.clone();
            runtime.spawn(async move {
                tracing::info!("received realtime remote audio track");
                let mut stream = NativeAudioStream::new(track, SAMPLE_RATE as i32, CHANNELS as i32);
                let mut frame_count = 0_u64;
                while let Some(frame) = stream.next().await {
                    frame_count += 1;
                    if frame_count == 1 || frame_count.is_multiple_of(500) {
                        tracing::debug!(
                            frame_count,
                            samples = frame.data.len(),
                            "received decoded realtime audio"
                        );
                    }
                    let playback = frame
                        .data
                        .iter()
                        .map(|sample| *sample as f32 / i16::MAX as f32)
                        .collect();
                    match playback_tx.try_send(playback) {
                        Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
                tracing::info!(frame_count, "realtime remote audio track ended");
            });
        })));

        let (ice_tx, ice_rx) = oneshot::channel();
        let ice_tx = Arc::new(Mutex::new(Some(ice_tx)));
        peer.on_ice_gathering_state_change(Some(Box::new(move |state| {
            tracing::info!(?state, "realtime ICE gathering state changed");
            if state == IceGatheringState::Complete
                && let Ok(mut sender) = ice_tx.lock()
                && let Some(sender) = sender.take()
            {
                let _ = sender.send(());
            }
        })));
        peer.on_ice_connection_state_change(Some(Box::new(|state| {
            tracing::info!(?state, "realtime ICE connection state changed");
        })));

        let offer = peer
            .create_offer(OfferOptions {
                offer_to_receive_audio: true,
                ..Default::default()
            })
            .await?;
        peer.set_local_description(offer).await?;
        tracing::info!("waiting for realtime ICE candidate gathering");
        tokio::time::timeout(Duration::from_secs(10), ice_rx)
            .await
            .context("timed out gathering realtime ICE candidates")??;
        let offer_sdp = SdpOffer::try_from(
            peer.current_local_description()
                .context("WebRTC peer has no local description")?
                .to_string(),
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
        let answer = SessionDescription::parse(&answer_sdp.0, SdpType::Answer)
            .context("parse realtime SDP answer")?;

        let (event_tx, event_rx) = mpsc::channel(64);
        let provider_events = event_tx.clone();
        data_channel.on_message(Some(Box::new(move |message| {
            tracing::debug!(
                bytes = message.data.len(),
                binary = message.binary,
                "received realtime data-channel message"
            );
            if message.binary {
                return;
            }
            let event = match ProviderEvent::from_json(message.data) {
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
        })));
        let (open_tx, open_rx) = oneshot::channel();
        let open_tx = Arc::new(Mutex::new(Some(open_tx)));
        let channel_events = event_tx.clone();
        data_channel.on_state_change(Some(Box::new(move |state| {
            tracing::info!(
                ?state,
                label = "oai-events",
                "realtime data channel state changed"
            );
            if state == DataChannelState::Open
                && let Ok(mut sender) = open_tx.lock()
                && let Some(sender) = sender.take()
            {
                let _ = sender.send(());
            } else if state == DataChannelState::Closed {
                let _ = channel_events.try_send(RealtimeEvent::Closed);
            }
        })));
        peer.on_connection_state_change(Some(Box::new(move |state| {
            tracing::info!(?state, "realtime peer connection state changed");
            if matches!(
                state,
                PeerConnectionState::Failed | PeerConnectionState::Closed
            ) {
                let _ = event_tx.try_send(RealtimeEvent::Closed);
            }
        })));
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
        let microphone_task = start_microphone(audio_source)?;
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
            self.data_channel.send(&command.to_json()?, false)?;
        }
        Ok(())
    }
}

impl Drop for RealtimeSession {
    fn drop(&mut self) {
        self.microphone_task.abort();
        self.data_channel.close();
        self.peer.close();
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

fn start_microphone(source: NativeAudioSource) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let microphone = MicrophoneBuilder::new()
        .default_device()?
        .default_config()?
        .prefer_sample_rates([
            rodio::nz!(48_000),
            rodio::nz!(96_000),
            rodio::nz!(44_100),
            rodio::nz!(16_000),
        ])
        .prefer_channel_counts([rodio::nz!(1), rodio::nz!(2)])
        .prefer_buffer_sizes(512..)
        .open_stream()?;
    let sample_rate = microphone.sample_rate().get();
    let channels = microphone.channels().get();
    tracing::info!(sample_rate, channels, "opened realtime microphone");
    let (tx, mut rx) = mpsc::channel::<Vec<i16>>(16);
    std::thread::Builder::new()
        .name("rho-realtime-microphone".to_owned())
        .spawn(move || {
            let mut microphone =
                UniformSourceIterator::new(microphone, rodio::nz!(1), rodio::nz!(48_000));
            let mut frame_count = 0_u64;
            loop {
                let mut frame = Vec::with_capacity(FRAME_SAMPLES);
                for _ in 0..FRAME_SAMPLES {
                    let Some(sample) = microphone.next() else {
                        tracing::warn!(frame_count, "realtime microphone stream ended");
                        return;
                    };
                    frame.push((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                }
                frame_count += 1;
                if frame_count == 1 || frame_count.is_multiple_of(500) {
                    tracing::debug!(frame_count, "captured realtime microphone audio");
                }
                if tx.blocking_send(frame).is_err() {
                    tracing::debug!(frame_count, "realtime microphone sender closed");
                    return;
                }
            }
        })?;
    Ok(tokio::spawn(async move {
        while let Some(samples) = rx.recv().await {
            let frame = AudioFrame {
                data: samples.into(),
                sample_rate: SAMPLE_RATE,
                num_channels: CHANNELS,
                samples_per_channel: FRAME_SAMPLES as u32,
            };
            if let Err(error) = source.capture_frame(&frame).await {
                tracing::warn!(?error, "failed to capture realtime microphone frame");
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
