//! A live view of one of the host's desktops: VP9 over the host's media
//! transport, input back on a stream of its own. Only decoded images are
//! coalesced.
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rho_desktop_media::codec::{Decoder, RetainedFrame};
use rho_desktop_proto::{Feedback, FrameId, Input};
use rho_rpc::protocol::{Opened, read_frame, write_open};
use tokio::sync::{mpsc, watch};

/// One decoded image: the YUV planes the decoder retained, which the
/// renderer samples as they are.
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub planes: Arc<RetainedFrame>,
    pub id: FrameId,
    received_at: Instant,
    lag_us: u64,
    progress: Arc<Progress>,
}
impl Image {
    /// Called once, from the GUI frame callback following this image's paint.
    pub fn presented(&self) {
        self.progress.presented(
            self.id,
            self.lag_us + self.received_at.elapsed().as_micros() as u64,
        );
    }
    /// Convert only when the user exports a screenshot.
    pub fn export_bgra(&self) -> Result<Vec<u8>> {
        Ok(rho_desktop_media::codec::export_bgra(&self.planes)?.bgra)
    }
}
// The floor invalidates queued and in-progress decoding together. It advances
// only by whole groups: a decoder never resumes halfway through a dependency
// chain.
#[derive(Default)]
struct Progress {
    floor: AtomicU64,
    feedback: Mutex<Feedback>,
}
impl Progress {
    fn accepts(&self, id: FrameId) -> bool {
        id.group >= self.floor.load(Ordering::Acquire)
    }
    fn recover(&self, floor: u64) {
        let mut feedback = self.feedback.lock().unwrap();
        self.floor.fetch_max(floor, Ordering::Release);
        feedback.recover = true;
    }
    fn presented(&self, id: FrameId, lag_us: u64) {
        let mut feedback = self.feedback.lock().unwrap();
        if self.accepts(id) && feedback.presented.is_none_or(|previous| id > previous) {
            feedback.presented = Some(id);
            feedback.lag_us = lag_us;
        }
    }
}

struct Encoded {
    id: FrameId,
    payload: bytes::Bytes,
    received_at: Instant,
    lag_us: u64,
}

fn recover(
    subscription: &mut moq_net::track::Ordered,
    progress: &Progress,
    group: u64,
) -> Result<()> {
    let floor = group
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("video group overflow"))?
        .max(subscription.latest().unwrap_or(0));
    progress.recover(floor);
    subscription.set_groups(floor..);
    subscription.update(
        subscription
            .subscription()
            .with_start(moq_net::track::Position::group(floor)),
    )?;
    Ok(())
}

fn decode_packets(
    mut decode: mpsc::Receiver<Encoded>,
    images: watch::Sender<Option<Arc<Image>>>,
    progress: Arc<Progress>,
    started: Instant,
    desktop_id: u64,
) -> Result<()> {
    tracing::info!(
        desktop_id,
        elapsed_ms = started.elapsed().as_millis(),
        "desktop decoder task started"
    );
    let mut decoder = Decoder::new()?;
    let mut first = true;
    while let Some(packet) = decode.blocking_recv() {
        if !progress.accepts(packet.id) {
            continue;
        }
        let decode_started = Instant::now();
        if let Some(frame) = decoder.decode_planes(&packet.payload)? {
            let mut feedback = progress.feedback.lock().unwrap();
            // Recovery may have superseded this decode while VP9 was running.
            if !progress.accepts(packet.id) {
                continue;
            }
            feedback.decoded = Some(packet.id);
            feedback.decode_us = decode_started.elapsed().as_micros() as u64;
            if first {
                tracing::info!(
                    desktop_id,
                    elapsed_ms = started.elapsed().as_millis(),
                    "desktop first frame decoded"
                );
                first = false;
            }
            let planes = Arc::new(frame);
            images.send_replace(Some(Arc::new(Image {
                width: planes.width(),
                height: planes.height(),
                planes,
                id: packet.id,
                received_at: packet.received_at,
                lag_us: packet.lag_us,
                progress: progress.clone(),
            })));
        }
    }

    Ok(())
}

async fn receive_packets(
    origin: moq_net::origin::Producer,
    packets: mpsc::Sender<Encoded>,
    progress: Arc<Progress>,
    quality: watch::Sender<Option<Input>>,
    started: Instant,
    desktop_id: u64,
) -> Result<()> {
    let mut announced = origin.consume().announced();
    loop {
        let update = announced
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("video unavailable"))?;
        if update.prefix.as_str() == "app" && update.kind.is_active() {
            break;
        }
    }
    tracing::info!(
        desktop_id,
        elapsed_ms = started.elapsed().as_millis(),
        "desktop video announced"
    );
    let broadcast = origin.consume().request_broadcast("app").await?;
    let track = broadcast.track("video")?;
    let mut subscription = track.subscribe(None).await?.ordered();
    tracing::info!(
        desktop_id,
        elapsed_ms = started.elapsed().as_millis(),
        "desktop video subscribed"
    );
    let mut rate = 2_000_000u32;
    let mut sample = Instant::now();
    let mut baseline: Option<i128> = None;
    let clock = Instant::now();
    let mut first_packet = true;

    let mut next_group = subscription.next_group().await?;
    let mut ended = false;
    while let Some(mut group) = next_group.take() {
        let sequence = group.sequence;
        let mut first_in_group = true;
        progress.floor.fetch_max(sequence, Ordering::Release);
        subscription.update(
            subscription
                .subscription()
                .with_start(moq_net::track::Position::group(sequence)),
        )?;
        // Every group begins with a keyframe, which resets references
        // without destroying the decoder or its reusable frame pool.
        loop {
            let keyframe = first_in_group;
            let read = async {
                let Some(mut frame) = group.next_frame().await? else {
                    return Ok(None);
                };
                if frame.size > rho_desktop_media::MAX_PACKET as u64 {
                    return Err(moq_net::Error::Cancel);
                }
                let timestamp = frame.timestamp;
                // Idle groups have nothing to catch up on. Only a started
                // dependent frame has a delivery deadline; a large keyframe
                // must be allowed to arrive even over a slow path.
                let payload = if keyframe {
                    frame.read_all().await?
                } else {
                    tokio::time::timeout(Duration::from_millis(150), frame.read_all())
                        .await
                        .map_err(|_| {
                            moq_net::Error::Stream(moq_net::StreamError::DeliveryTimeout)
                        })??
                };
                Ok::<_, moq_net::Error>(Some(moq_net::frame::Frame { timestamp, payload }))
            };
            tokio::pin!(read);
            let frame = tokio::select! {
                biased;
                newer = subscription.next_group(), if !ended => {
                    next_group = newer?;
                    if next_group.is_some() {
                        // A new keyframe-led stream supersedes even an
                        // unfinished frame in the old group.
                        break;
                    }
                    ended = true;
                    read.await
                }
                frame = &mut read => frame,
            };
            match frame {
                Ok(Some(frame)) => {
                    if first_packet {
                        tracing::info!(
                            desktop_id,
                            elapsed_ms = started.elapsed().as_millis(),
                            bytes = frame.payload.len(),
                            "desktop first packet received"
                        );
                        first_packet = false;
                    }
                    let offset =
                        clock.elapsed().as_micros() as i128 - frame.timestamp.as_micros() as i128;
                    if first_in_group && progress.feedback.lock().unwrap().recover {
                        // Rebase after a discontinuity; otherwise a changed
                        // path/CPU baseline would request keyframes forever.
                        baseline = Some(offset);
                    }
                    let base = baseline.get_or_insert(offset);
                    *base = (*base).min(offset);
                    let lag = offset - *base;
                    if sample.elapsed() >= Duration::from_secs(1) {
                        let congested = lag > 150_000;
                        rate = if congested {
                            rate * 3 / 4
                        } else {
                            rate + rate / 20
                        };
                        rate = rate.clamp(128_000, 4_000_000);
                        quality.send_replace(Some(Input::Quality {
                            bitrate: rate,
                            keyframe: false,
                        }));
                        sample = Instant::now();
                    }
                    let id = FrameId {
                        group: sequence,
                        timestamp_us: frame.timestamp.as_micros() as u64,
                    };
                    {
                        let mut feedback = progress.feedback.lock().unwrap();
                        feedback.received = Some(id);
                        if first_in_group {
                            feedback.recover = false;
                        }
                        feedback.lag_us = lag as u64;
                    }
                    // Do not reject the recovery keyframe for the very
                    // backlog it was requested to fix.
                    if (!first_in_group && lag > 150_000)
                        || packets
                            .try_send(Encoded {
                                id,
                                payload: frame.payload,
                                received_at: Instant::now(),
                                lag_us: lag as u64,
                            })
                            .is_err()
                    {
                        recover(&mut subscription, &progress, sequence)?;
                        break;
                    }
                    first_in_group = false;
                }
                Ok(None) => break,
                Err(error)
                    if matches!(
                        error,
                        moq_net::Error::Old
                            | moq_net::Error::Evicted
                            | moq_net::Error::Lagged
                            | moq_net::Error::Stream(
                                moq_net::StreamError::Old
                                    | moq_net::StreamError::Evicted
                                    | moq_net::StreamError::TooFarBehind
                                    | moq_net::StreamError::DeliveryTimeout
                            )
                    ) =>
                {
                    recover(&mut subscription, &progress, sequence)?;
                    break;
                }
                Err(error) => return Err(error.into()),
            };
        }
        if next_group.is_none() && !ended {
            next_group = subscription.next_group().await?;
        }
    }
    Ok::<(), anyhow::Error>(())
}

pub struct Viewer {
    pub started_at: Instant,
    pub desktop_id: u64,
    pub images: watch::Receiver<Option<Arc<Image>>>,
    pub errors: watch::Receiver<Option<String>>,
    pub input: mpsc::Sender<Input>,
    pub motion: watch::Sender<Option<(u32, u32)>>,
    task: tokio::task::JoinHandle<()>,
}
impl Viewer {
    pub fn disconnect(&self) {
        self.task.abort();
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Opens the desktop `session` of `agent` on the host `link` reaches.
/// Only an iroh host carries media.
pub fn open(
    link: &rho_agent_hosts::Link,
    agent: String,
    session: String,
) -> impl Future<Output = Result<Viewer>> + Send + 'static {
    let started = Instant::now();
    link.run(move |dialer| async move {
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis(),
            "desktop IO task started"
        );
        let rho_agent_hosts::Dialer::Iroh { connection, media } = dialer else {
            anyhow::bail!("the live Wayland viewer requires an Iroh host");
        };
        open_stream(connection, media, agent, session, started).await
    })
}

async fn open_stream(
    connection: iroh::endpoint::Connection,
    media: rho_rpc::media::Mux,
    agent: String,
    session: String,
    started: Instant,
) -> Result<Viewer> {
    static NEXT_MEDIA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_MEDIA.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(desktop_id = id, %agent, %session, "desktop open requested");
    for path in connection.paths().iter().filter(|path| path.is_selected()) {
        tracing::info!(
            desktop_id = id,
            direct = path.is_ip(),
            rtt_ms = path.rtt().as_millis(),
            lost_packets = connection.stats().lost_packets,
            "desktop selected network path"
        );
    }
    let transport = media.session(id)?;
    let (send, recv) = connection.open_bi().await?;
    send.set_priority(100)?;
    let mut stream = rho_rpc::Stream::new(recv, send);
    write_open(
        &mut stream,
        &crate::protocol::Open::Wayland {
            media_id: id,
            agent,
            session,
        },
    )
    .await?;
    if let Opened::Refused { reason } = read_frame(&mut stream).await? {
        anyhow::bail!("Wayland open refused: {reason}");
    }
    tracing::info!(
        desktop_id = id,
        elapsed_ms = started.elapsed().as_millis(),
        "desktop open acknowledged"
    );
    subscribe(transport, stream, started, id).await
}

async fn subscribe(
    transport: rho_rpc::media::Session,
    stream: rho_rpc::Stream,
    started: Instant,
    desktop_id: u64,
) -> Result<Viewer> {
    let (mut reader, mut writer) = stream.into_split();
    let origin = rho_desktop_media::media::origin();
    let session = rho_desktop_media::media::subscribe(transport, origin.clone()).await?;
    let (images, receive) = watch::channel(None);
    let (errors, error_receive) = watch::channel(None);
    let (input, mut commands) = mpsc::channel(256);
    let progress = Arc::new(Progress::default());
    let (quality, mut quality_updates) = watch::channel(None);
    let (motion, mut movement) = watch::channel(None);
    let (packets, decode) = mpsc::channel::<Encoded>(2);
    let decoded = images.clone();
    let decoding = progress.clone();
    // Decode off the UI and Tokio IO workers; the renderer samples the
    // retained YUV planes.
    let decode_task = tokio::task::spawn_blocking(move || -> Result<()> {
        decode_packets(decode, decoded, decoding, started, desktop_id)
    });
    let task = tokio::spawn(async move {
        let receive = receive_packets(
            origin,
            packets,
            progress.clone(),
            quality,
            started,
            desktop_id,
        );
        let control = async {
            let mut ticks = tokio::time::interval(Duration::from_millis(100));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let input = tokio::select! {
                    biased;
                    command=commands.recv()=>match command { Some(input)=>input,None=>break },
                    changed=movement.changed()=> {
                        changed?;
                        let Some((x,y))=*movement.borrow_and_update() else {continue};
                        Input::Move{x,y}
                    },
                    changed=quality_updates.changed()=> {
                        changed?;
                        let Some(input)=quality_updates.borrow_and_update().clone() else {continue};
                        input
                    },
                    _=ticks.tick()=> Input::Feedback(*progress.feedback.lock().unwrap()),
                };
                rho_rpc::write_frame(&mut writer, &input, 64 * 1024).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let remote_error = async {
            let (frame, _) =
                rho_rpc::read_frame::<_, rho_desktop_proto::Packet>(&mut reader, 64 * 1024).await?;
            let rho_desktop_proto::Packet::Error(error) = frame;
            Err::<(), anyhow::Error>(anyhow::anyhow!("{error}"))
        };
        let result = tokio::select! {
            result=receive => result,
            result=decode_task => result.map_err(anyhow::Error::from).and_then(|result|result),
            result=control => result,
            result=remote_error => result,
            error=session.closed() => Err(anyhow::anyhow!("{error}")),
        };
        if let Err(error) = result {
            errors.send_replace(Some(format!("{error:#}")));
        }
    });
    Ok(Viewer {
        started_at: started,
        desktop_id,
        images: receive,
        errors: error_receive,
        input,
        motion,
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(group: u64, timestamp_us: u64) -> FrameId {
        FrameId {
            group,
            timestamp_us,
        }
    }

    #[test]
    fn recovery_invalidates_decode_and_late_presentation_by_group() {
        let progress = Progress::default();
        progress.presented(id(3, 100), 15);
        progress.recover(5);
        // A stale callback and an older recovery request cannot undo the floor.
        progress.recover(4);
        progress.presented(id(4, 300), 900);
        assert!(!progress.accepts(id(4, 999)));
        assert!(progress.accepts(id(5, 301)));
        assert_eq!(
            progress.feedback.lock().unwrap().presented,
            Some(id(3, 100))
        );
        progress.presented(id(5, 301), 25);
        progress.presented(id(5, 299), 800);
        let feedback = *progress.feedback.lock().unwrap();
        assert_eq!(feedback.presented, Some(id(5, 301)));
        assert_eq!(feedback.lag_us, 25);
        assert!(feedback.recover);
    }

    #[tokio::test]
    async fn newer_group_preempts_unfinished_old_frame() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let origin = rho_desktop_media::media::origin();
            let broadcast = origin.create_broadcast("app")?;
            broadcast.announce(Default::default())?;
            let track = broadcast.create_track(
                "video",
                Some(moq_net::track::Info::default().with_timescale(moq_net::Timescale::MICRO)),
            )?;
            let (packets, mut received) = mpsc::channel(2);
            let (quality, _) = watch::channel(None);
            let progress = Arc::new(Progress::default());
            let receiving = tokio::spawn(receive_packets(
                origin,
                packets,
                progress.clone(),
                quality,
                Instant::now(),
                0,
            ));
            track.used().await?;
            let mut old = track.append_group()?;
            old.write_frame(
                moq_net::Timestamp::from_micros(10)?,
                bytes::Bytes::from_static(b"old key"),
            )?;
            assert_eq!(received.recv().await.unwrap().id, id(old.sequence, 10));
            let old_sequence = old.sequence;
            let mut partial = old.create_frame(moq_net::frame::Info {
                timestamp: moq_net::Timestamp::from_micros(20)?,
                size: 100,
            })?;
            partial.write(bytes::Bytes::from_static(b"unfinished"))?;
            let mut new = track.append_group()?;
            new.write_frame(
                moq_net::Timestamp::from_micros(30)?,
                bytes::Bytes::from_static(b"new key"),
            )?;
            let packet = received.recv().await.unwrap();
            assert_eq!(packet.id, id(new.sequence, 30));
            assert_eq!(packet.payload.as_ref(), b"new key");
            assert!(!progress.accepts(id(old_sequence, 10)));
            assert_eq!(
                track.subscription().unwrap().start.unwrap().group,
                new.sequence
            );
            // An idle group is not a stalled frame.
            tokio::time::sleep(Duration::from_millis(180)).await;
            assert!(!progress.feedback.lock().unwrap().recover);
            let _stalled = new.create_frame(moq_net::frame::Info {
                timestamp: moq_net::Timestamp::from_micros(40)?,
                size: 100,
            })?;
            while !progress.feedback.lock().unwrap().recover {
                tokio::task::yield_now().await;
            }
            assert_eq!(progress.floor.load(Ordering::Acquire), packet.id.group + 1);
            receiving.abort();
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn full_decoder_queue_abandons_dependencies_until_next_keyframe() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let origin = rho_desktop_media::media::origin();
            let broadcast = origin.create_broadcast("app")?;
            broadcast.announce(Default::default())?;
            let track = broadcast.create_track(
                "video",
                Some(moq_net::track::Info::default().with_timescale(moq_net::Timescale::MICRO)),
            )?;
            let (packets, mut received) = mpsc::channel(1);
            let (quality, _) = watch::channel(None);
            let progress = Arc::new(Progress::default());
            let receiving = tokio::spawn(receive_packets(
                origin,
                packets,
                progress.clone(),
                quality,
                Instant::now(),
                0,
            ));
            track.used().await?;
            let mut old = track.append_group()?;
            for timestamp in [10, 20] {
                old.write_frame(
                    moq_net::Timestamp::from_micros(timestamp)?,
                    bytes::Bytes::from_static(b"old"),
                )?;
            }
            while !progress.feedback.lock().unwrap().recover {
                tokio::task::yield_now().await;
            }
            assert_eq!(progress.floor.load(Ordering::Acquire), old.sequence + 1);
            let queued = received.recv().await.unwrap();
            assert!(!progress.accepts(queued.id));
            old.write_frame(
                moq_net::Timestamp::from_micros(25)?,
                bytes::Bytes::from_static(b"undecodable delta"),
            )?;
            let mut new = track.append_group()?;
            new.write_frame(
                moq_net::Timestamp::from_micros(30)?,
                bytes::Bytes::from_static(b"recovery key"),
            )?;
            let packet = received.recv().await.unwrap();
            assert_eq!(packet.id, id(new.sequence, 30));
            assert_eq!(packet.payload.as_ref(), b"recovery key");
            assert!(!progress.feedback.lock().unwrap().recover);
            receiving.abort();
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn decoder_skips_obsolete_queued_packets_and_restarts_at_keyframe() -> Result<()> {
        let progress = Arc::new(Progress::default());
        progress.recover(7);
        let (packets, decode) = mpsc::channel(3);
        let (images, receive) = watch::channel(None);
        // Deliberately invalid VP9: decoding either obsolete packet must fail.
        for timestamp_us in [10, 20] {
            packets.try_send(Encoded {
                id: id(6, timestamp_us),
                payload: bytes::Bytes::from_static(b"obsolete invalid packet"),
                received_at: Instant::now(),
                lag_us: 0,
            })?;
        }
        let mut encoder = rho_desktop_media::codec::Encoder::new(48, 24, 500_000)?;
        let bgra = [37, 91, 203, 255].repeat(48 * 24);
        let keyframe = encoder.encode(&bgra, true)?.remove(0);
        assert!(keyframe.keyframe);
        packets.try_send(Encoded {
            id: id(7, 30),
            payload: keyframe.data.into(),
            received_at: Instant::now(),
            lag_us: 12,
        })?;
        drop(packets);
        let worker_progress = progress.clone();
        tokio::task::spawn_blocking(move || {
            decode_packets(decode, images, worker_progress, Instant::now(), 0)
        })
        .await??;
        let image = receive.borrow().clone().expect("new keyframe decoded");
        assert_eq!((image.width, image.height), (48, 24));
        assert_eq!(image.id, id(7, 30));
        assert_eq!(progress.feedback.lock().unwrap().decoded, Some(id(7, 30)));
        image.presented();
        assert_eq!(progress.feedback.lock().unwrap().presented, Some(id(7, 30)));
        Ok(())
    }
}
