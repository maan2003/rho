use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use tokio::sync::{mpsc, oneshot};

use crate::*;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 1;
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 100;

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
            let event = provider_transcript
                .lock()
                .ok()
                .and_then(|mut transcript| process_provider_message(message.data, &mut transcript));
            if let Some((lane, event)) = event {
                let sender = match lane {
                    EventLane::General => &provider_events,
                    EventLane::Transcript => &transcript_tx,
                };
                let _ = sender.try_send(event);
            }
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

    /// Append output that is not associated with a provider delegation to the
    /// active realtime session.
    pub async fn append_context(
        &self,
        channel: DelegateResponseChannel,
        text: &str,
    ) -> anyhow::Result<()> {
        for chunk in utf8_chunks(text, CONTEXT_APPEND_MAX_BYTES) {
            let command = ProviderCommand::SessionContextAppend {
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

impl Drop for RealtimeSession {
    fn drop(&mut self) {
        self.microphone_task.abort();
        self.data_channel.close();
        self.peer.close();
    }
}
pub(crate) fn add_ice_candidates(
    sdp: &str,
    candidates: &[(i32, String)],
) -> anyhow::Result<String> {
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

pub(crate) struct RealtimePlayback {
    receiver: std::sync::mpsc::Receiver<Vec<f32>>,
    buffered: VecDeque<f32>,
}

impl RealtimePlayback {
    pub(crate) fn new(receiver: std::sync::mpsc::Receiver<Vec<f32>>) -> Self {
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
