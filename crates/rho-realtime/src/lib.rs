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

pub struct RealtimeSession {
    peer: PeerConnection,
    data_channel: DataChannel,
    events: mpsc::Receiver<RealtimeEvent>,
    transcript_events: mpsc::Receiver<RealtimeEvent>,
    microphone_task: tokio::task::JoinHandle<()>,
    transcript: Arc<Mutex<TranscriptState>>,
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
        peer.on_ice_candidate_error(Some(Box::new(|error| {
            tracing::warn!(
                url = %error.url,
                error_code = error.error_code,
                error_text = %error.error_text,
                "realtime ICE candidate error"
            );
        })));
        let ice_candidates = Arc::new(Mutex::new(Vec::new()));
        let gathered_candidates = ice_candidates.clone();
        peer.on_ice_candidate(Some(Box::new(move |candidate| {
            if let Ok(mut candidates) = gathered_candidates.lock() {
                candidates.push((candidate.sdp_mline_index(), candidate.candidate()));
            }
        })));

        let offer = peer
            .create_offer(OfferOptions {
                offer_to_receive_audio: true,
                ..Default::default()
            })
            .await?;
        let offer_sdp = offer.to_string();
        peer.set_local_description(offer).await?;
        tracing::info!("waiting for realtime ICE candidate gathering");
        tokio::time::timeout(Duration::from_secs(10), ice_rx)
            .await
            .context("timed out gathering realtime ICE candidates")??;
        let ice_candidates = ice_candidates
            .lock()
            .map_err(|_| anyhow::anyhow!("realtime ICE candidate collector was poisoned"))?
            .clone();
        let offer_sdp = SdpOffer::try_from(add_ice_candidates(&offer_sdp, &ice_candidates)?)?;
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
        let (transcript_tx, transcript_rx) = mpsc::channel(64);
        let transcript = Arc::new(Mutex::new(TranscriptState::default()));
        let provider_transcript = Arc::clone(&transcript);
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
                        let transcript_delta = provider_transcript
                            .lock()
                            .map(|mut transcript| transcript.take_snapshot())
                            .unwrap_or_default();
                        RealtimeEvent::DelegateRequest(DelegateRequest {
                            id: DelegateRequestId(item.id),
                            text,
                            transcript_delta,
                        })
                    }
                }
                Ok(ProviderEvent::InputTranscriptDelta { delta })
                | Ok(ProviderEvent::InputAudioTranscriptDelta { delta })
                | Ok(ProviderEvent::InputTranscriptAdded {
                    item: TranscriptItem { text: delta },
                }) => {
                    if let Ok(mut transcript) = provider_transcript.lock() {
                        transcript.delta(TranscriptRole::User, &delta);
                    }
                    let event = RealtimeEvent::TranscriptDelta {
                        role: TranscriptRole::User,
                        delta,
                    };
                    let _ = transcript_tx.try_send(event);
                    return;
                }
                Ok(ProviderEvent::InputTranscriptMarked { transcript: text })
                | Ok(ProviderEvent::InputAudioTranscriptDone { transcript: text }) => {
                    if let Ok(mut transcript) = provider_transcript.lock() {
                        transcript.done(TranscriptRole::User, &text);
                    }
                    let event = RealtimeEvent::TranscriptDone {
                        role: TranscriptRole::User,
                        text,
                    };
                    let _ = transcript_tx.try_send(event);
                    return;
                }
                Ok(ProviderEvent::OutputTranscriptDelta { delta })
                | Ok(ProviderEvent::OutputTextDelta { delta })
                | Ok(ProviderEvent::OutputAudioTranscriptDelta { delta })
                | Ok(ProviderEvent::OutputTranscriptAdded {
                    item: TranscriptItem { text: delta },
                }) => {
                    if let Ok(mut transcript) = provider_transcript.lock() {
                        transcript.delta(TranscriptRole::Assistant, &delta);
                    }
                    let event = RealtimeEvent::TranscriptDelta {
                        role: TranscriptRole::Assistant,
                        delta,
                    };
                    let _ = transcript_tx.try_send(event);
                    return;
                }
                Ok(ProviderEvent::OutputTextDone { text })
                | Ok(ProviderEvent::OutputAudioTranscriptDone { transcript: text }) => {
                    if let Ok(mut transcript) = provider_transcript.lock() {
                        transcript.done(TranscriptRole::Assistant, &text);
                    }
                    let event = RealtimeEvent::TranscriptDone {
                        role: TranscriptRole::Assistant,
                        text,
                    };
                    let _ = transcript_tx.try_send(event);
                    return;
                }
                Ok(ProviderEvent::TurnDone { turn }) => {
                    let role = match turn.role {
                        TranscriptRoleWire::User => TranscriptRole::User,
                        TranscriptRoleWire::Assistant => TranscriptRole::Assistant,
                    };
                    if let Ok(mut transcript) = provider_transcript.lock() {
                        transcript.done(role, &turn.transcript);
                    }
                    let _ = transcript_tx.try_send(RealtimeEvent::TranscriptDone {
                        role,
                        text: turn.transcript,
                    });
                    return;
                }
                Ok(ProviderEvent::Error { message, error }) => {
                    let (message, code) = match error {
                        Some(error) => (error.message, error.code),
                        None => (
                            message.unwrap_or_else(|| {
                                "realtime provider reported an error".to_owned()
                            }),
                            None,
                        ),
                    };
                    RealtimeEvent::Error(match code {
                        Some(code) => format!("{message} ({code})"),
                        None => message,
                    })
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
            transcript_events: transcript_rx,
            microphone_task,
            transcript,
            _output: output,
        })
    }

    pub async fn next_event(&mut self) -> Option<RealtimeEvent> {
        tokio::select! {
            biased;
            event = self.events.recv() => event,
            event = self.transcript_events.recv() => event,
        }
    }

    pub async fn resolve_delegate(
        &self,
        request_id: DelegateRequestId,
        text: &str,
    ) -> anyhow::Result<()> {
        self.resolve_delegate_chunk(request_id, DelegateResponseChannel::Speakable, text)
            .await
    }

    pub async fn resolve_delegate_chunk(
        &self,
        request_id: DelegateRequestId,
        channel: DelegateResponseChannel,
        text: &str,
    ) -> anyhow::Result<()> {
        for chunk in utf8_chunks(text, CONTEXT_APPEND_MAX_BYTES) {
            let command = ProviderCommand::DelegationContextAppend {
                delegation_item_id: request_id.0.clone(),
                channel: match channel {
                    DelegateResponseChannel::Commentary => DelegationChannel::Commentary,
                    DelegateResponseChannel::Speakable => DelegationChannel::Speakable,
                },
                content: vec![ProviderCommandContent::InputText {
                    text: chunk.to_owned(),
                }],
            };
            self.data_channel.send(&command.to_json()?, false)?;
        }
        Ok(())
    }

    pub fn take_transcript_tail(&self) -> Option<String> {
        self.transcript
            .lock()
            .ok()
            .and_then(|mut transcript| transcript.take_tail())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelegateResponseChannel {
    Commentary,
    Speakable,
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

fn add_ice_candidates(sdp: &str, candidates: &[(i32, String)]) -> anyhow::Result<String> {
    let media_count = sdp.lines().filter(|line| line.starts_with("m=")).count();
    for (index, candidate) in candidates {
        anyhow::ensure!(
            *index >= 0 && (*index as usize) < media_count,
            "ICE candidate refers to invalid media section {index}"
        );
        anyhow::ensure!(candidate.starts_with("candidate:"), "invalid ICE candidate");
    }

    let mut completed = String::with_capacity(sdp.len() + candidates.len() * 128);
    let mut media_index = None;
    let append_candidates = |completed: &mut String, media_index: usize| {
        for (_, candidate) in candidates
            .iter()
            .filter(|(index, _)| *index as usize == media_index)
        {
            completed.push_str("a=");
            completed.push_str(candidate.trim_end_matches(['\r', '\n']));
            completed.push_str("\r\n");
        }
        completed.push_str("a=end-of-candidates\r\n");
    };
    for line in sdp.lines() {
        if line.starts_with("m=") {
            if let Some(index) = media_index {
                append_candidates(&mut completed, index);
            }
            media_index = Some(media_index.map_or(0, |index| index + 1));
        }
        completed.push_str(line.trim_end_matches('\r'));
        completed.push_str("\r\n");
    }
    if let Some(index) = media_index {
        append_candidates(&mut completed, index);
    }
    Ok(completed)
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
