use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use js_sys::{Array, ArrayBuffer, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    Blob, Event, HtmlAudioElement, MediaStream, MediaStreamConstraints, MediaStreamTrack,
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelState, RtcIceGatheringState,
    RtcIceServer, RtcPeerConnection, RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit,
    RtcTrackEvent,
};

use crate::*;

const SEND_LOW_WATER_BYTES: u32 = 64 * 1024;
const SEND_HIGH_WATER_BYTES: u32 = 256 * 1024;

type EventSender = Rc<RefCell<mpsc::Sender<RealtimeEvent>>>;

pub struct RealtimeSession {
    peer: RtcPeerConnection,
    data_channel: RtcDataChannel,
    events: mpsc::Receiver<RealtimeEvent>,
    transcript_events: mpsc::Receiver<RealtimeEvent>,
    transcript: Rc<RefCell<TranscriptState>>,
    _microphone: Microphone,
    remote_audio: Rc<RefCell<Option<HtmlAudioElement>>>,
    _callbacks: Callbacks,
}

struct Callbacks {
    _ice: Closure<dyn FnMut(Event)>,
    _open: Closure<dyn FnMut(Event)>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(Event)>,
    _connection: Closure<dyn FnMut(Event)>,
    _track: Closure<dyn FnMut(RtcTrackEvent)>,
}

struct Microphone(MediaStream);

impl Drop for Microphone {
    fn drop(&mut self) {
        for track in self.0.get_tracks() {
            if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
                track.stop();
            }
        }
    }
}

impl RealtimeSession {
    /// Establish a browser voice session using the caller's signaling function.
    pub async fn connect<S, F>(signal: S) -> anyhow::Result<Self>
    where
        S: FnOnce(SdpOffer) -> F,
        F: Future<Output = anyhow::Result<SdpAnswer>>,
    {
        let window = web_sys::window().context("browser window is unavailable")?;
        let constraints = MediaStreamConstraints::new();
        constraints.set_audio_bool(true);
        constraints.set_video_bool(false);
        let microphone = Microphone(
            JsFuture::from(
                window
                    .navigator()
                    .media_devices()
                    .map_err(js_error)?
                    .get_user_media_with_constraints(&constraints)
                    .map_err(js_error)?,
            )
            .await
            .map_err(js_error)?
            .dyn_into()
            .map_err(js_error)?,
        );

        let server = RtcIceServer::new();
        server.set_urls_str("stun:stun.l.google.com:19302");
        let servers = Array::new();
        servers.push(&server);
        let configuration = RtcConfiguration::new();
        configuration.set_ice_servers(&servers);
        let peer = RtcPeerConnection::new_with_configuration(&configuration).map_err(js_error)?;

        for track in microphone.0.get_audio_tracks() {
            let track: MediaStreamTrack = track.dyn_into().map_err(js_error)?;
            peer.add_track_0(&track, &microphone.0);
        }

        let data_channel = peer.create_data_channel("oai-events");
        data_channel.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);
        data_channel.set_buffered_amount_low_threshold(SEND_LOW_WATER_BYTES);

        let (event_tx, event_rx) = mpsc::channel(64);
        let (transcript_tx, transcript_rx) = mpsc::channel(64);
        let event_tx = Rc::new(RefCell::new(event_tx));
        let transcript_tx = Rc::new(RefCell::new(transcript_tx));
        let transcript = Rc::new(RefCell::new(TranscriptState::default()));

        let (ice_tx, ice_rx) = oneshot::channel();
        let ice_tx = Rc::new(RefCell::new(Some(ice_tx)));
        let ice_peer = peer.clone();
        let ice = Closure::wrap(Box::new(move |_event: Event| {
            if ice_peer.ice_gathering_state() == RtcIceGatheringState::Complete {
                if let Some(tx) = ice_tx.borrow_mut().take() {
                    let _ = tx.send(());
                }
            }
        }) as Box<dyn FnMut(_)>);
        peer.set_onicegatheringstatechange(Some(ice.as_ref().unchecked_ref()));

        let remote_audio = Rc::new(RefCell::new(None));
        let track_audio = Rc::clone(&remote_audio);
        let track_events = Rc::clone(&event_tx);
        let track = Closure::wrap(Box::new(move |event: RtcTrackEvent| {
            let result = (|| -> anyhow::Result<()> {
                let stream = event
                    .streams()
                    .get(0)
                    .dyn_into::<MediaStream>()
                    .or_else(|_| {
                        let stream = MediaStream::new()?;
                        stream.add_track(&event.track());
                        Ok(stream)
                    })
                    .map_err(js_error)?;
                let audio = HtmlAudioElement::new().map_err(js_error)?;
                audio.set_autoplay(true);
                audio.set_src_object(Some(&stream));
                let playback = audio.play().map_err(js_error)?;
                let playback_events = Rc::clone(&track_events);
                spawn_local(async move {
                    if let Err(error) = JsFuture::from(playback).await {
                        send_event(
                            &playback_events,
                            RealtimeEvent::Error(js_error(error).to_string()),
                        );
                    }
                });
                *track_audio.borrow_mut() = Some(audio);
                Ok(())
            })();
            if let Err(error) = result {
                send_event(&track_events, RealtimeEvent::Error(error.to_string()));
            }
        }) as Box<dyn FnMut(_)>);
        peer.set_ontrack(Some(track.as_ref().unchecked_ref()));

        let (open_tx, open_rx) = oneshot::channel();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));
        let opened = Rc::clone(&open_tx);
        let open = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(tx) = opened.borrow_mut().take() {
                let _ = tx.send(());
            }
        }) as Box<dyn FnMut(_)>);
        data_channel.set_onopen(Some(open.as_ref().unchecked_ref()));

        let message_transcript = Rc::clone(&transcript);
        let message_events = Rc::clone(&event_tx);
        let message_transcript_events = Rc::clone(&transcript_tx);
        let message = Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();
            if let Some(text) = data.as_string() {
                dispatch_message(
                    text.into_bytes(),
                    &message_transcript,
                    &message_events,
                    &message_transcript_events,
                );
            } else if let Ok(buffer) = data.clone().dyn_into::<ArrayBuffer>() {
                if buffer.byte_length() as usize > MAX_PROVIDER_EVENT_BYTES {
                    send_event(
                        &message_events,
                        RealtimeEvent::Error(format!(
                            "realtime provider event exceeds {MAX_PROVIDER_EVENT_BYTES} bytes"
                        )),
                    );
                    return;
                }
                dispatch_message(
                    Uint8Array::new(&buffer).to_vec(),
                    &message_transcript,
                    &message_events,
                    &message_transcript_events,
                );
            } else if let Ok(blob) = data.dyn_into::<Blob>() {
                if blob.size() > MAX_PROVIDER_EVENT_BYTES as f64 {
                    send_event(
                        &message_events,
                        RealtimeEvent::Error(format!(
                            "realtime provider event exceeds {MAX_PROVIDER_EVENT_BYTES} bytes"
                        )),
                    );
                    return;
                }
                let transcript = Rc::clone(&message_transcript);
                let events = Rc::clone(&message_events);
                let transcript_events = Rc::clone(&message_transcript_events);
                spawn_local(async move {
                    match JsFuture::from(blob.array_buffer()).await {
                        Ok(buffer) => dispatch_message(
                            Uint8Array::new(&buffer).to_vec(),
                            &transcript,
                            &events,
                            &transcript_events,
                        ),
                        Err(error) => {
                            send_event(&events, RealtimeEvent::Error(js_error(error).to_string()))
                        }
                    }
                });
            } else {
                send_event(
                    &message_events,
                    RealtimeEvent::Error(
                        "unsupported realtime data-channel message type".to_owned(),
                    ),
                );
            }
        }) as Box<dyn FnMut(_)>);
        data_channel.set_onmessage(Some(message.as_ref().unchecked_ref()));

        let close_events = Rc::clone(&event_tx);
        let close = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(tx) = open_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
            send_event(&close_events, RealtimeEvent::Closed);
        }) as Box<dyn FnMut(_)>);
        data_channel.set_onclose(Some(close.as_ref().unchecked_ref()));

        let connection_peer = peer.clone();
        let connection_events = Rc::clone(&event_tx);
        let connection = Closure::wrap(Box::new(move |_event: Event| {
            if matches!(
                connection_peer.connection_state(),
                RtcPeerConnectionState::Failed | RtcPeerConnectionState::Closed
            ) {
                send_event(&connection_events, RealtimeEvent::Closed);
            }
        }) as Box<dyn FnMut(_)>);
        peer.set_onconnectionstatechange(Some(connection.as_ref().unchecked_ref()));

        let offer = JsFuture::from(peer.create_offer())
            .await
            .map_err(js_error)?
            .dyn_into::<RtcSessionDescriptionInit>()
            .map_err(js_error)?;
        JsFuture::from(peer.set_local_description(&offer))
            .await
            .map_err(js_error)?;
        if peer.ice_gathering_state() != RtcIceGatheringState::Complete {
            ice_rx
                .await
                .map_err(|_| anyhow::anyhow!("ICE gathering ended before completion"))?;
        }
        let offer = peer
            .local_description()
            .context("browser did not produce a local SDP offer")?
            .sdp();
        let answer = signal(SdpOffer::try_from(offer)?).await?;
        let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        remote.set_sdp(&answer.0);
        JsFuture::from(peer.set_remote_description(&remote))
            .await
            .map_err(js_error)?;

        if data_channel.ready_state() != RtcDataChannelState::Open {
            open_rx
                .await
                .map_err(|_| anyhow::anyhow!("realtime data channel closed before opening"))?;
            anyhow::ensure!(
                data_channel.ready_state() == RtcDataChannelState::Open,
                "realtime data channel closed before opening"
            );
        }

        Ok(Self {
            peer,
            data_channel,
            events: event_rx,
            transcript_events: transcript_rx,
            transcript,
            _microphone: microphone,
            remote_audio,
            _callbacks: Callbacks {
                _ice: ice,
                _open: open,
                _message: message,
                _close: close,
                _connection: connection,
                _track: track,
            },
        })
    }

    pub async fn next_event(&mut self) -> Option<RealtimeEvent> {
        futures::select_biased! {
            event = self.events.next().fuse() => event,
            event = self.transcript_events.next().fuse() => event,
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
                channel: channel.into(),
                content: vec![ProviderCommandContent::InputText {
                    text: chunk.to_owned(),
                }],
            };
            self.send(command.to_json()?).await?;
        }
        Ok(())
    }

    pub async fn append_context(
        &self,
        channel: DelegateResponseChannel,
        text: &str,
    ) -> anyhow::Result<()> {
        for chunk in utf8_chunks(text, CONTEXT_APPEND_MAX_BYTES) {
            let command = ProviderCommand::SessionContextAppend {
                channel: channel.into(),
                content: vec![ProviderCommandContent::InputText {
                    text: chunk.to_owned(),
                }],
            };
            self.send(command.to_json()?).await?;
        }
        Ok(())
    }

    pub fn take_transcript_tail(&self) -> Option<String> {
        self.transcript.borrow_mut().take_tail()
    }

    async fn send(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.data_channel.ready_state() == RtcDataChannelState::Open,
            "realtime data channel is not open"
        );
        while self.data_channel.buffered_amount() > SEND_HIGH_WATER_BYTES {
            wait_for_buffered_amount_low(&self.data_channel).await?;
            anyhow::ensure!(
                self.data_channel.ready_state() == RtcDataChannelState::Open,
                "realtime data channel closed while sending"
            );
        }
        let text = std::str::from_utf8(&bytes).context("provider command is not UTF-8")?;
        self.data_channel.send_with_str(text).map_err(js_error)
    }
}

impl From<DelegateResponseChannel> for DelegationChannel {
    fn from(channel: DelegateResponseChannel) -> Self {
        match channel {
            DelegateResponseChannel::Commentary => Self::Commentary,
            DelegateResponseChannel::Speakable => Self::Speakable,
        }
    }
}

impl Drop for RealtimeSession {
    fn drop(&mut self) {
        self.data_channel.set_onopen(None);
        self.data_channel.set_onmessage(None);
        self.data_channel.set_onclose(None);
        self.peer.set_onicegatheringstatechange(None);
        self.peer.set_onconnectionstatechange(None);
        self.peer.set_ontrack(None);
        self.data_channel.close();
        if let Some(audio) = self.remote_audio.borrow_mut().take() {
            audio.pause().ok();
            audio.set_src_object(None);
        }
        self.peer.close();
    }
}

fn dispatch_message(
    bytes: Vec<u8>,
    transcript: &Rc<RefCell<TranscriptState>>,
    events: &EventSender,
    transcript_events: &EventSender,
) {
    let event = process_provider_message(&bytes, &mut transcript.borrow_mut());
    if let Some((lane, event)) = event {
        match lane {
            EventLane::General => send_event(events, event),
            EventLane::Transcript => send_event(transcript_events, event),
        }
    }
}

fn send_event(sender: &EventSender, event: RealtimeEvent) {
    let _ = sender.borrow_mut().try_send(event);
}

async fn wait_for_buffered_amount_low(channel: &RtcDataChannel) -> anyhow::Result<()> {
    if channel.buffered_amount() <= SEND_LOW_WATER_BYTES {
        return Ok(());
    }
    let (tx, rx) = oneshot::channel();
    let tx = Rc::new(RefCell::new(Some(tx)));
    let closure = Closure::wrap(Box::new(move |_event: Event| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(());
        }
    }) as Box<dyn FnMut(_)>);
    channel
        .add_event_listener_with_callback("bufferedamountlow", closure.as_ref().unchecked_ref())
        .map_err(js_error)?;
    channel
        .add_event_listener_with_callback("close", closure.as_ref().unchecked_ref())
        .map_err(js_error)?;
    if channel.buffered_amount() > SEND_LOW_WATER_BYTES
        && channel.ready_state() == RtcDataChannelState::Open
    {
        rx.await
            .map_err(|_| anyhow::anyhow!("buffered amount waiter was cancelled"))?;
    }
    channel
        .remove_event_listener_with_callback("bufferedamountlow", closure.as_ref().unchecked_ref())
        .map_err(js_error)?;
    channel
        .remove_event_listener_with_callback("close", closure.as_ref().unchecked_ref())
        .map_err(js_error)?;
    Ok(())
}

fn js_error(value: JsValue) -> anyhow::Error {
    anyhow::anyhow!("browser WebRTC error: {value:?}")
}
