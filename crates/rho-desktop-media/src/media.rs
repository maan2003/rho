//! Fixed MoQ video groups over the existing authenticated Iroh connection.
//! Control/input uses a separate bidirectional stream, never the video queue.
use std::time::Duration;

use anyhow::{Result, ensure};
use bytes::Bytes;
use moq_net::{group, origin, track};

use crate::{FrameKind, Header};

pub fn origin() -> origin::Producer {
    let mut config = origin::Config::default();
    config.pool = moq_net::cache::Pool::new(
        moq_net::cache::Config::default()
            .with_capacity(32 * 1024 * 1024)
            .with_expiry(Duration::from_secs(2)),
    );
    config.cache_duration = Duration::from_millis(750);
    let (origin, driver) = origin::Producer::new(config);
    tokio::spawn(moq_net::time::run(driver));
    origin
}

/// A fixed video stream: desktop-open supplies the subscription out of band.
pub struct Session {
    task: tokio::task::JoinHandle<()>,
    done: tokio::sync::watch::Receiver<Option<String>>,
}
impl Session {
    pub fn abort(&self, _: moq_net::Error) {
        self.task.abort();
    }
    pub async fn closed(&self) -> String {
        let mut done = self.done.clone();
        loop {
            if let Some(error) = done.borrow().clone() {
                return error;
            }
            if done.changed().await.is_err() {
                return "desktop media closed".into();
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(feature = "iroh")]
pub async fn publish(
    transport: moq_tokio::shared_iroh::Session,
    origin: &origin::Producer,
) -> Result<Session> {
    fixed_publish(transport, origin)
}

fn fixed_publish<S: moq_net::web_transport_trait::Session + Send + Sync + Unpin + 'static>(
    transport: S,
    origin: &origin::Producer,
) -> Result<Session> {
    let origin = origin.clone();
    Ok(fixed_session(async move {
        use moq_net::web_transport_trait::{RecvStream as _, SendStream as _};
        let _close = CloseTransport(transport.clone());
        // The reverse stream carries asynchronous group floors, never a startup
        // gate.
        let (mut send, mut recv) = transport.open_bi().await?;
        let video = async {
            let broadcast = origin.consume().request_broadcast("app").await?;
            let checkpoints = broadcast.track("video")?.subscribe(None).await?;
            let states = broadcast.track("states")?.subscribe(None).await?;
            let controls = [checkpoints.control(), states.control()];
            let floors = async {
                loop {
                    let mut floor = [0; 9];
                    let mut read = 0;
                    while read < floor.len() {
                        match recv.read(&mut floor[read..]).await? {
                            Some(0) | None => return Ok::<(), anyhow::Error>(()),
                            Some(n) => read += n,
                        }
                    }
                    let control = controls
                        .get(floor[0] as usize)
                        .ok_or_else(|| anyhow::anyhow!("invalid video track"))?;
                    control.update(control.subscription().with_start(track::Position::group(
                        u64::from_be_bytes(floor[1..].try_into().unwrap()),
                    )))?;
                }
            };
            tokio::select! {
                result = moq_net::publish_fixed(moq_tokio::transport::Session::new(transport.clone()), checkpoints, 0) => Ok::<(), anyhow::Error>(result?),
                result = moq_net::publish_fixed(moq_tokio::transport::Session::new(transport.clone()), states, 1) => Ok::<(), anyhow::Error>(result?),
                result = floors => result,
            }
        };
        // QMux opens bidirectional streams lazily: write a one-byte preface to
        // make the reverse floor stream visible without waiting for its
        // receiver.
        let lifetime = async {
            send.write_chunk(Bytes::from_static(&[0])).await?;
            let _ = send.closed().await;
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! {
            result = video => result,
            result = lifetime => result,
            _ = transport.closed() => Ok(()),
        }
    }))
}

#[cfg(feature = "iroh")]
pub async fn subscribe(
    transport: moq_tokio::shared_iroh::Session,
    origin: origin::Producer,
) -> Result<Session> {
    fixed_subscribe(transport, origin)
}

fn fixed_subscribe<S: moq_net::web_transport_trait::Session + Send + Sync + Unpin + 'static>(
    transport: S,
    origin: origin::Producer,
) -> Result<Session> {
    let broadcast = origin.create_broadcast("app")?;
    let video = Video::new(&broadcast)?;
    broadcast.announce(Default::default())?;
    Ok(fixed_session(async move {
        use moq_net::transport::poll::Session as _;
        use moq_net::web_transport_trait::{RecvStream as _, SendStream as _};
        let _close = CloseTransport(transport.clone());
        let lifetime = async {
            let (mut send, mut recv) = transport.accept_bi().await?;
            let mut preface = [0];
            if recv.read(&mut preface).await?.is_none() {
                return Ok::<(), anyhow::Error>(());
            }
            let mut checkpoints = video.track.clone();
            let mut states = video.states.clone();
            let floors = async {
                loop {
                    let (id, subscription) = tokio::select! {
                        subscription = checkpoints.subscription_changed() => (0u8, subscription?),
                        subscription = states.subscription_changed() => (1u8, subscription?),
                    };
                    let floor = subscription.and_then(|s| s.start).map_or(0, |p| p.group);
                    let mut message = [0; 9];
                    message[0] = id;
                    message[1..].copy_from_slice(&floor.to_be_bytes());
                    send.write_chunk(Bytes::copy_from_slice(&message)).await?;
                }
            };
            let mut byte = [0];
            tokio::select! {
                result = floors => result,
                _ = recv.read(&mut byte) => Ok(()),
            }
        };
        let receive = async {
            let mut receiver = moq_tokio::transport::Session::new(transport.clone());
            let mut groups = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    stream = receiver.accept_uni(), if groups.len() < 32 => {
                        let stream = stream?;
                        let tracks = [video.track.clone(), video.states.clone()];
                        groups.spawn(async move {
                            moq_net::receive_fixed_group(stream, &tracks, moq_net::Timescale::MICRO).await
                        });
                    }
                    _ = groups.join_next(), if !groups.is_empty() => {}
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        let result = tokio::select! {
            result = lifetime => result,
            result = receive => result,
            _ = transport.closed() => Ok(()),
        };
        drop((video, broadcast, origin));
        result
    }))
}

// Pending adapter reads may own transport clones. Close explicitly rather than
// waiting for their last clone to drop when the viewer task is cancelled.
struct CloseTransport<S: moq_net::web_transport_trait::Session>(S);
impl<S: moq_net::web_transport_trait::Session> Drop for CloseTransport<S> {
    fn drop(&mut self) {
        self.0.close(0, "desktop viewer closed");
    }
}

fn fixed_session(future: impl Future<Output = Result<()>> + Send + 'static) -> Session {
    let (done, receive) = tokio::sync::watch::channel(None);
    let task = tokio::spawn(async move {
        let result = future.await;
        done.send_replace(Some(match result {
            Ok(()) => "desktop media closed".into(),
            Err(error) => format!("{error:#}"),
        }));
    });
    Session {
        task,
        done: receive,
    }
}

/// Checkpoints stay ordered in a keyframe-led group. Each ordinary state has
/// its own stream and can be superseded without changing the reference chain.
pub struct Video {
    pub track: track::Producer,
    pub states: track::Producer,
    group: Option<group::Producer>,
    state: Option<group::Producer>,
    epoch: u64,
    base: u64,
}
impl Video {
    pub fn new(broadcast: &moq_net::broadcast::Producer) -> Result<Self> {
        // Reliable checkpoints must not expire while a slow state is replaced.
        let info = track::Info::default().with_timescale(moq_net::Timescale::MICRO);
        Ok(Self {
            track: broadcast.create_track("video", Some(info.clone()))?,
            states: broadcast.create_track("states", Some(info))?,
            group: None,
            state: None,
            epoch: 0,
            base: 0,
        })
    }
    pub fn write(&mut self, kind: FrameKind, timestamp: u64, data: Bytes) -> Result<()> {
        ensure!(data.len() <= crate::MAX_PACKET, "video packet too large");
        if kind == FrameKind::Key {
            if let Some(group) = self.group.take() {
                group.abort(moq_net::Error::Old)?;
            }
            self.group = Some(self.track.append_group()?);
            self.epoch = timestamp;
            self.base = 0;
        }
        ensure!(self.epoch > 0, "video must start with a keyframe");
        let header = Header {
            kind,
            epoch: self.epoch,
            base: self.base,
        };
        let payload = header.pack(data);
        let time = moq_net::Timestamp::from_micros(timestamp)?;
        if let Some(state) = self.state.take() {
            // Abort, even after finish: obsolete bytes need no retransmission.
            state.abort(moq_net::Error::Old)?;
        }
        if kind == FrameKind::State {
            let mut state = self.states.append_group()?;
            state.write_frame(time, payload)?;
            state.finish()?;
            self.state = Some(state);
        } else {
            self.group.as_mut().unwrap().write_frame(time, payload)?;
            self.base = timestamp;
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "iroh"))]
mod tests {
    use anyhow::Context;
    use moq_tokio::shared_iroh::Mux;

    use super::*;

    #[tokio::test]
    async fn local_fixed_stream_releases_capture_on_drop() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let producer = origin();
            let consumer = origin();
            let broadcast = producer.create_broadcast("app")?;
            broadcast.announce(Default::default())?;
            let mut video = Video::new(&broadcast)?;
            let (server, client) = tokio::io::duplex(4096);
            let (sending, receiving) = tokio::join!(
                local_server(server, &producer),
                local_client(client, consumer.clone()),
            );
            let sending = sending?;
            let receiving = receiving?;
            video.track.used().await?;
            let remote = consumer.consume().request_broadcast("app").await?;
            let mut track = remote.track("video")?.subscribe(None).await?.ordered();
            video.write(FrameKind::Key, 17_000, Bytes::from_static(b"keyframe"))?;
            video.write(
                FrameKind::Checkpoint,
                18_250,
                Bytes::from_static(b"dependent"),
            )?;
            let mut group = track.next_group().await?.unwrap();
            let first = group.read_frame().await?.unwrap();
            assert_eq!(first.timestamp, moq_net::Timestamp::from_micros(17_000)?);
            assert_eq!(&first.payload[Header::SIZE..], b"keyframe");
            let second = group.read_frame().await?.unwrap();
            assert_eq!(second.timestamp, moq_net::Timestamp::from_micros(18_250)?);
            assert_eq!(&second.payload[Header::SIZE..], b"dependent");
            drop(receiving);
            // Keep the local model readers alive: the transport lifetime, not
            // their accidental destruction, must release compositor demand.
            video.track.unused().await?;
            sending.closed().await;
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn latest_state_cancels_old_stream_without_losing_checkpoint_chain() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let source = origin();
            let relay = origin();
            let viewer = origin();
            let broadcast = source.create_broadcast("app")?;
            broadcast.announce(Default::default())?;
            let mut video = Video::new(&broadcast)?;
            let (a, b) = tokio::io::duplex(4096);
            let (c, d) = tokio::io::duplex(4096);
            let sessions = tokio::try_join!(
                local_server(a, &source),
                local_client(b, relay.clone()),
                local_server(c, &relay),
                local_client(d, viewer.clone()),
            )?;
            let remote = viewer.consume().request_broadcast("app").await?;
            let mut checkpoints = remote.track("video")?.subscribe(None).await?.ordered();
            let mut states = remote.track("states")?.subscribe(None).await?.ordered();
            video.track.used().await?;
            video.write(FrameKind::Key, 100, Bytes::from_static(b"A"))?;
            let mut chain = checkpoints.next_group().await?.unwrap();
            let (header, payload) = Header::unpack(chain.read_frame().await?.unwrap().payload)?;
            assert_eq!(
                header,
                Header {
                    kind: FrameKind::Key,
                    epoch: 100,
                    base: 0
                }
            );
            assert_eq!(payload.as_ref(), b"A");

            video.write(FrameKind::State, 200, Bytes::from(vec![17; 4_000_000]))?;
            let mut stale = states.next_group().await?.unwrap();
            // Obtaining the partial frame proves old work reached both relays.
            let mut stale_frame = stale.next_frame().await?.unwrap();
            video.write(FrameKind::State, 300, Bytes::from_static(b"latest A"))?;
            let mut latest = states.next_group().await?.unwrap();
            let (header, payload) = Header::unpack(latest.read_frame().await?.unwrap().payload)?;
            assert_eq!(
                header,
                Header {
                    kind: FrameKind::State,
                    epoch: 100,
                    base: 100
                }
            );
            assert_eq!(payload.as_ref(), b"latest A");
            assert!(
                stale_frame.read_all().await.is_err(),
                "obsolete transport must be aborted"
            );

            video.write(FrameKind::Checkpoint, 400, Bytes::from_static(b"B"))?;
            video.write(FrameKind::State, 500, Bytes::from_static(b"latest B"))?;
            let (header, payload) = Header::unpack(chain.read_frame().await?.unwrap().payload)?;
            assert_eq!(
                header,
                Header {
                    kind: FrameKind::Checkpoint,
                    epoch: 100,
                    base: 100
                }
            );
            assert_eq!(payload.as_ref(), b"B");
            let mut newest = states.next_group().await?.unwrap();
            let (header, payload) = Header::unpack(newest.read_frame().await?.unwrap().payload)?;
            assert_eq!(
                header,
                Header {
                    kind: FrameKind::State,
                    epoch: 100,
                    base: 400
                }
            );
            assert_eq!(payload.as_ref(), b"latest B");
            drop(sessions);
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn detach_while_waiting_for_broadcast_closes_publisher() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let source = origin();
            let (server, client) = tokio::io::duplex(4096);
            let (sending, receiving) = tokio::try_join!(
                local_server(server, &source),
                local_client(client, origin()),
            )?;
            drop(receiving);
            sending.closed().await;
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn floor_crosses_relay_and_cancels_unfinished_group() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(8), async {
            let source = origin();
            let relay = origin();
            let viewer = origin();
            let broadcast = source.create_broadcast("app")?;
            broadcast.announce(Default::default())?;
            let mut video = Video::new(&broadcast)?;

            let (up_a, up_b) = tokio::io::duplex(4096);
            let (down_a, down_b) = tokio::io::duplex(4096);
            let (upstream, relay_input, relay_output, downstream) = tokio::try_join!(
                local_server(up_a, &source),
                local_client(up_b, relay.clone()),
                local_server(down_a, &relay),
                local_client(down_b, viewer.clone()),
            )?;
            let mut upstream_prefs = video.track.clone();
            // Consume the original viewer demand before looking for the changed
            // floor.
            let _ = upstream_prefs.subscription_changed().await?;
            let remote = viewer.consume().request_broadcast("app").await?;
            let mut subscribed = remote.track("video")?.subscribe(None).await?.ordered();
            video.write(FrameKind::Key, 17_000, Bytes::from_static(b"old"))?;
            let mut old = subscribed.next_group().await?.unwrap();
            assert_eq!(
                &old.read_frame().await?.unwrap().payload[Header::SIZE..],
                b"old"
            );

            subscribed.control().update(
                subscribed
                    .control()
                    .subscription()
                    .with_start(track::Position::group(1)),
            )?;
            // A spliced cursor forwards changed preferences to its active
            // segment when polled, independently of the previously handed
            // group.
            let mut next = Box::pin(subscribed.next_group());
            loop {
                tokio::select! {
                    changed = upstream_prefs.subscription_changed() => {
                        if changed?.unwrap().start == Some(track::Position::group(1)) { break; }
                    }
                    _ = &mut next => anyhow::bail!("unexpected next group"),
                }
            }
            drop(next);
            // A less restrictive second viewer holds the aggregate floor down.
            let mut second = remote.track("video")?.subscribe(None).await?.ordered();
            let second_old = second.next_group().await?.unwrap();
            drop(second_old);
            loop {
                if upstream_prefs.subscription_changed().await?.unwrap().start
                    == Some(track::Position::group(0))
                {
                    break;
                }
            }
            drop(second);
            loop {
                if upstream_prefs.subscription_changed().await?.unwrap().start
                    == Some(track::Position::group(1))
                {
                    break;
                }
            }
            // The old group is still open at the source; the reader must not
            // wait for its next frame or its source-side finish.
            // Expiry at the consumed end reports EOF, even though the source
            // has not finished this group; there is no truncated unread frame.
            assert!(old.read_frame().await?.is_none());
            video.write(
                FrameKind::Checkpoint,
                17_500,
                Bytes::from_static(b"superseded"),
            )?;
            assert!(old.read_frame().await?.is_none());
            video.write(FrameKind::Key, 18_000, Bytes::from_static(b"new"))?;
            let mut fresh = subscribed.next_group().await?.unwrap();
            assert_eq!(
                &fresh.read_frame().await?.unwrap().payload[Header::SIZE..],
                b"new"
            );
            drop((downstream, relay_output, relay_input, upstream));
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    async fn rpc_echo(connection: &iroh::endpoint::Connection, text: &str) -> Result<()> {
        let (send, recv) = connection.open_bi().await?;
        let mut stream = rho_rpc::Stream::new(recv, send);
        rho_rpc::write_frame(&mut stream, &text.to_owned(), 1024).await?;
        let (response, _) = rho_rpc::read_frame::<_, String>(&mut stream, 1024).await?;
        assert_eq!(response, format!("echo:{text}"));
        Ok(())
    }

    #[tokio::test]
    async fn media_shares_one_connection_and_detach_preserves_rpc() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let server = iroh::Endpoint::builder(iroh::endpoint::presets::N0DisableRelay)
                .alpns(vec![b"rho-test".to_vec()])
                .bind()
                .await?;
            let client = iroh::Endpoint::builder(iroh::endpoint::presets::N0DisableRelay)
                .bind()
                .await?;
            let (outgoing, incoming) =
                tokio::join!(client.connect(server.addr(), b"rho-test"), async {
                    server.accept().await.unwrap().await
                });
            let outgoing = outgoing?;
            let incoming = incoming?;
            let send_mux = Mux::new(incoming.clone());
            let recv_mux = Mux::new(outgoing.clone());
            let send_transport = send_mux.session(37)?;
            let recv_transport = recv_mux.session(37)?;
            let tasks = [
                tokio::spawn({
                    let m = send_mux.clone();
                    let conn = incoming.clone();
                    async move {
                        loop {
                            let (send, recv) =
                                conn.accept_bi().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                            let m = m.clone();
                            tokio::spawn(async move {
                                if let Some((mut reader, send)) =
                                    rho_rpc::accept_iroh_stream(&m, send, recv).await?
                                {
                                    let mut writer = rho_rpc::Writer::new(send);
                                    let (request, _) =
                                        rho_rpc::read_frame::<_, String>(&mut reader, 1024).await?;
                                    rho_rpc::write_frame(
                                        &mut writer,
                                        &format!("echo:{request}"),
                                        1024,
                                    )
                                    .await?;
                                }
                                Ok::<(), anyhow::Error>(())
                            });
                        }
                        #[allow(unreachable_code)]
                        Ok::<(), anyhow::Error>(())
                    }
                }),
                tokio::spawn({
                    let m = send_mux.clone();
                    async move { Ok::<(), anyhow::Error>(m.receive_uni().await?) }
                }),
                tokio::spawn({
                    let m = recv_mux.clone();
                    async move { Ok::<(), anyhow::Error>(m.receive_bi().await?) }
                }),
                tokio::spawn({
                    let m = recv_mux.clone();
                    async move { Ok::<(), anyhow::Error>(m.receive_uni().await?) }
                }),
            ];
            let producer = origin();
            let consumer = origin();
            let broadcast = producer.create_broadcast("app")?;
            broadcast.announce(Default::default())?;
            let mut video = Video::new(&broadcast)?;
            let sending = publish(send_transport, &producer).await?;
            // The receiver has sent nothing and has not even started. Opening
            // desktop video must demand encoding without a negotiation round
            // trip.
            tokio::time::timeout(Duration::from_secs(2), video.track.used()).await??;
            let receiving = subscribe(recv_transport, consumer.clone()).await?;
            let mut announced = consumer.consume().announced();
            let update = announced
                .next()
                .await
                .context("missing video announcement")?;
            assert_eq!(update.prefix.as_str(), "app");
            let remote = consumer.consume().request_broadcast("app").await?;
            let track = remote.track("video")?;
            let mut subscribed = track.subscribe(None).await?.ordered();
            let (drained, drain) = tokio::sync::oneshot::channel();
            let publish_frames = async {
                video.track.used().await?;
                rpc_echo(&outgoing, "while video is subscribed").await?;
                video.write(FrameKind::Key, 17_000, Bytes::from_static(b"keyframe"))?;
                video.write(
                    FrameKind::Checkpoint,
                    18_000,
                    Bytes::from_static(b"dependent"),
                )?;
                drain.await?;
                video.group.take().unwrap().abort(moq_net::Error::Old)?;
                video.write(FrameKind::Key, 19_000, Bytes::from_static(b"fresh"))?;
                Ok::<(), anyhow::Error>(())
            };
            let read_frames = async {
                let mut group = subscribed.next_group().await?.unwrap();
                assert_eq!(
                    &group.read_frame().await?.unwrap().payload[Header::SIZE..],
                    b"keyframe"
                );
                assert_eq!(
                    &group.read_frame().await?.unwrap().payload[Header::SIZE..],
                    b"dependent"
                );
                drained.send(()).unwrap();
                let stale = group.read_frame().await;
                assert!(
                    matches!(
                        stale,
                        Err(moq_net::Error::Stream(moq_net::StreamError::Old))
                    ),
                    "stale group: {stale:?}"
                );
                let mut group = subscribed.next_group().await?.unwrap();
                assert_eq!(
                    &group.read_frame().await?.unwrap().payload[Header::SIZE..],
                    b"fresh"
                );
                Ok::<(), anyhow::Error>(())
            };
            let (sent, read) = tokio::join!(publish_frames, read_frames);
            sent?;
            read?;
            drop(subscribed);
            receiving.abort(moq_net::Error::Cancel);
            video.track.unused().await?;
            sending.abort(moq_net::Error::Cancel);
            rpc_echo(&outgoing, "after media close").await?;
            for task in tasks {
                task.abort();
                let _ = task.await;
            }
            client.close().await;
            server.close().await;
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }
}

/// QMux only covers the local desktop-to-agent host byte transport. The remote
/// GUI still uses independent QUIC streams on its existing authenticated Iroh
/// connection.
pub async fn local_client<S>(stream: S, origin: origin::Producer) -> Result<Session>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fixed_subscribe(qmux_transport(stream, false).await?, origin)
}
pub async fn local_server<S>(stream: S, origin: &origin::Producer) -> Result<Session>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fixed_publish(qmux_transport(stream, true).await?, origin)
}
async fn qmux_transport<S>(stream: S, server: bool) -> Result<qmux::Session>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut config = qmux::Config::new(qmux::Version::QMux01);
    config.protocol = qmux::Protocol::Negotiated("rho-desktop-video-1".into());
    let stream = qmux::transport::Stream::new(stream, config.version, config.max_record_size);
    let session = if server {
        qmux::Session::accept(stream, config).await?
    } else {
        qmux::Session::connect(stream, config).await?
    };
    Ok(session)
}

/// Dropping a connection aborts only that MoQ session, not an enclosing Iroh
/// connection.
pub struct SessionGuard(pub Session);
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.abort(moq_net::Error::Cancel);
    }
}
