use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use gloo_timers::future::TimeoutFuture;
use js_sys::Array;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    Event, HtmlAudioElement, MediaStream, MediaStreamConstraints, MediaStreamTrack,
    RtcConfiguration, RtcIceGatheringState, RtcIceServer, RtcPeerConnection,
    RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit, RtcTrackEvent,
};

use crate::*;

type EventSender = Rc<RefCell<mpsc::Sender<RtcEvent>>>;

pub struct RtcSession {
    peer: RtcPeerConnection,
    events: mpsc::Receiver<RtcEvent>,
    _microphone: Microphone,
    input_muted: bool,
    remote_audio: Rc<RefCell<Option<HtmlAudioElement>>>,
    _callbacks: Callbacks,
}

struct Callbacks {
    _ice: Closure<dyn FnMut(Event)>,
    _connection: Closure<dyn FnMut(Event)>,
    _track: Closure<dyn FnMut(RtcTrackEvent)>,
}

struct Microphone(MediaStream);

struct PendingPeer {
    peer: RtcPeerConnection,
    remote_audio: Rc<RefCell<Option<HtmlAudioElement>>>,
    armed: bool,
}

impl Drop for PendingPeer {
    fn drop(&mut self) {
        if self.armed {
            cleanup_peer(&self.peer, &self.remote_audio);
        }
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        for track in self.0.get_tracks() {
            if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
                track.stop();
            }
        }
    }
}

impl RtcSession {
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
            track.set_enabled(false);
            peer.add_track_0(&track, &microphone.0);
        }

        let (event_tx, event_rx) = mpsc::channel(4);
        let event_tx = Rc::new(RefCell::new(event_tx));

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
                            RtcEvent::Error(js_error(error).to_string()),
                        );
                    }
                });
                *track_audio.borrow_mut() = Some(audio);
                Ok(())
            })();
            if let Err(error) = result {
                send_event(&track_events, RtcEvent::Error(error.to_string()));
            }
        }) as Box<dyn FnMut(_)>);
        peer.set_ontrack(Some(track.as_ref().unchecked_ref()));

        let connection_peer = peer.clone();
        let connection_events = Rc::clone(&event_tx);
        let connection = Closure::wrap(Box::new(move |_event: Event| {
            if matches!(
                connection_peer.connection_state(),
                RtcPeerConnectionState::Failed | RtcPeerConnectionState::Closed
            ) {
                send_event(&connection_events, RtcEvent::Closed);
            }
        }) as Box<dyn FnMut(_)>);
        peer.set_onconnectionstatechange(Some(connection.as_ref().unchecked_ref()));

        // Keep the JS peer from retaining callbacks whose Rust closures are
        // dropped if any of the setup awaits below fail or are cancelled.
        let mut pending_peer = PendingPeer {
            peer: peer.clone(),
            remote_audio: Rc::clone(&remote_audio),
            armed: true,
        };

        let offer = JsFuture::from(peer.create_offer())
            .await
            .map_err(js_error)?
            .dyn_into::<RtcSessionDescriptionInit>()
            .map_err(js_error)?;
        JsFuture::from(peer.set_local_description(&offer))
            .await
            .map_err(js_error)?;
        if peer.ice_gathering_state() != RtcIceGatheringState::Complete {
            let ice = ice_rx.fuse();
            let timeout = TimeoutFuture::new(10_000).fuse();
            futures::pin_mut!(ice, timeout);
            futures::select_biased! {
                result = ice => result.map_err(|_| anyhow::anyhow!("ICE gathering ended before completion"))?,
                _ = timeout => anyhow::bail!("timed out gathering realtime ICE candidates"),
            }
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

        pending_peer.armed = false;
        Ok(Self {
            peer,
            events: event_rx,
            _microphone: microphone,
            input_muted: true,
            remote_audio,
            _callbacks: Callbacks {
                _ice: ice,
                _connection: connection,
                _track: track,
            },
        })
    }

    pub async fn next_event(&mut self) -> Option<RtcEvent> {
        self.events.next().await
    }

    pub fn start_audio(&mut self) -> anyhow::Result<()> {
        for track in self._microphone.0.get_audio_tracks() {
            let track: MediaStreamTrack = track.dyn_into().map_err(js_error)?;
            track.set_enabled(!self.input_muted);
        }
        Ok(())
    }

    pub fn set_input_muted(&mut self, muted: bool) -> anyhow::Result<()> {
        self.input_muted = muted;
        for track in self._microphone.0.get_audio_tracks() {
            let track: MediaStreamTrack = track.dyn_into().map_err(js_error)?;
            track.set_enabled(!muted);
        }
        Ok(())
    }
}

impl Drop for RtcSession {
    fn drop(&mut self) {
        cleanup_peer(&self.peer, &self.remote_audio);
    }
}

fn send_event(sender: &EventSender, event: RtcEvent) {
    let _ = sender.borrow_mut().try_send(event);
}

fn cleanup_peer(peer: &RtcPeerConnection, remote_audio: &Rc<RefCell<Option<HtmlAudioElement>>>) {
    peer.set_onicegatheringstatechange(None);
    peer.set_onconnectionstatechange(None);
    peer.set_ontrack(None);
    if let Some(audio) = remote_audio.borrow_mut().take() {
        audio.pause().ok();
        audio.set_src_object(None);
    }
    peer.close();
}

fn js_error(value: JsValue) -> anyhow::Error {
    anyhow::anyhow!("browser WebRTC error: {value:?}")
}
