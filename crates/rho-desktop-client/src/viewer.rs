//! A live view of one of the host's desktops: VP9 over the host's media
//! transport, input back on a stream of its own. Checkpoints are ordered;
//! disposable states are coalesced before decoding.
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::FutureExt;
use rho_desktop_media::codec::{Decoder, RetainedFrame};
use rho_desktop_media::{FrameKind, Header};
use rho_desktop_proto::{Feedback, FrameId, Input};
use rho_rpc::protocol::{Opened, read_frame, write_open};
use tokio::sync::{Notify, mpsc, watch};

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
// only by root epochs: a decoder never resumes halfway through a checkpoint
// chain.
#[derive(Default)]
struct Progress {
    floor: AtomicU64,
    feedback: Mutex<Feedback>,
    recovery: Notify,
    receipt: Mutex<Receipt>,
}
#[derive(Default)]
struct Receipt {
    reference: Option<(u64, u64)>,
    state: Option<(FrameId, u64)>,
}
impl Progress {
    fn accepts(&self, id: FrameId) -> bool {
        id.epoch >= self.floor.load(Ordering::Acquire)
    }
    fn recover(&self, epoch: u64, floor: u64) {
        let mut feedback = self.feedback.lock().unwrap();
        if epoch < self.floor.load(Ordering::Acquire) {
            return;
        }
        self.floor.fetch_max(floor, Ordering::Release);
        feedback.recovery_id += 1;
        feedback.recover = true;
        self.recovery.notify_one();
    }
    async fn invalidated(&self, epoch: u64) {
        loop {
            let notified = self.recovery.notified();
            if epoch < self.floor.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    fn report(&self, rtt: Duration) -> Feedback {
        let mut feedback = *self.feedback.lock().unwrap();
        feedback.rtt_us = rtt.as_micros() as u64;
        feedback
    }
    fn received(&self, packet: &Encoded) {
        let mut receipt = self.receipt.lock().unwrap();
        let mut feedback = self.feedback.lock().unwrap();
        if !self.accepts(packet.id) {
            return;
        }
        match packet.header.kind {
            FrameKind::Key => {
                receipt.reference = Some((packet.header.epoch, packet.id.timestamp_us));
                feedback.recover = false;
            }
            FrameKind::Checkpoint => {
                receipt.reference = Some((packet.header.epoch, packet.id.timestamp_us));
            }
            FrameKind::State => {
                if receipt.state.is_none_or(|(id, _)| packet.id > id) {
                    receipt.state = Some((packet.id, packet.header.base));
                }
            }
        }
        // Other-stream states cannot acknowledge missing reliable checkpoints.
        let safe = packet.header.kind != FrameKind::State
            || receipt.reference == Some((packet.header.epoch, packet.header.base));
        let mut received = safe.then_some(packet.id);
        if let Some((state, base)) = receipt.state {
            if receipt.reference == Some((state.epoch, base)) {
                received = received.map(|id| id.max(state)).or(Some(state));
            }
        }
        if let Some(id) = received {
            if feedback.received.is_none_or(|previous| id > previous) {
                feedback.received = Some(id);
                feedback.lag_us = packet.lag_us;
            }
        }
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
    header: Header,
    id: FrameId,
    payload: bytes::Bytes,
    received_at: Instant,
    lag_us: u64,
}

fn recover(
    subscription: &mut moq_net::track::Ordered,
    progress: &Progress,
    sequence: u64,
    epoch: u64,
) -> Result<()> {
    progress.recover(
        epoch,
        epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("video epoch overflow"))?,
    );
    let floor = sequence
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("video group overflow"))?
        .max(subscription.latest().unwrap_or(0));
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
    mut states: watch::Receiver<Option<Arc<Encoded>>>,
    images: watch::Sender<Option<Arc<Image>>>,
    progress: Arc<Progress>,
    started: Instant,
    desktop_id: u64,
) -> Result<()> {
    let mut decoder = Decoder::new()?;
    let mut reference = None;
    let mut handled_state = None;
    let mut states_open = true;
    let mut first = true;
    loop {
        let packet = {
            // Keep future-base bytes only in the watch slot, not in a second
            // queue while checkpoints decode. Re-read its newest value each turn.
            let candidate = states.borrow_and_update().clone().filter(|packet| {
                handled_state != Some(packet.id)
                    && progress.accepts(packet.id)
                    && reference == Some((packet.header.epoch, packet.header.base))
            });
            match decode.try_recv() {
                Ok(packet) => Arc::new(packet),
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if let Some(packet) = candidate {
                        packet
                    } else {
                        break;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    if let Some(packet) = candidate {
                        packet
                    } else {
                        let next = tokio::runtime::Handle::current().block_on(async {
                            tokio::select! {
                                biased;
                                packet = decode.recv() => packet.map(Arc::new),
                                changed = states.changed(), if states_open => {
                                    if changed.is_err() { states_open = false; }
                                    None
                                }
                            }
                        });
                        if let Some(packet) = next {
                            packet
                        } else {
                            if decode.is_closed() && decode.is_empty() {
                                break;
                            }
                            continue;
                        }
                    }
                }
            }
        };
        if packet.header.kind == FrameKind::State {
            handled_state = Some(packet.id);
        }
        if !progress.accepts(packet.id) {
            continue;
        }
        let valid = match packet.header.kind {
            FrameKind::Key => {
                packet.header.base == 0 && packet.header.epoch == packet.id.timestamp_us
            }
            FrameKind::Checkpoint | FrameKind::State => {
                reference == Some((packet.header.epoch, packet.header.base))
                    && packet.id.timestamp_us > packet.header.base
            }
        };
        let decode_started = Instant::now();
        let frame = if valid {
            decoder.decode_planes(&packet.payload)
        } else {
            Err(anyhow::anyhow!("missing video checkpoint"))
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(desktop_id, ?packet.id, %error, "desktop decode recovery");
                progress.recover(
                    packet.id.epoch,
                    packet
                        .id
                        .epoch
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("video epoch overflow"))?,
                );
                decoder = Decoder::new()?;
                reference = None;
                continue;
            }
        };
        if packet.header.kind != FrameKind::State {
            reference = Some((packet.header.epoch, packet.id.timestamp_us));
        }
        if let Some(frame) = frame {
            let mut feedback = progress.feedback.lock().unwrap();
            if !progress.accepts(packet.id) {
                continue;
            }
            feedback.decoded = Some(feedback.decoded.map_or(packet.id, |id| id.max(packet.id)));
            feedback.decode_us = decode_started.elapsed().as_micros() as u64;
            if first {
                tracing::info!(
                    desktop_id,
                    elapsed_ms = started.elapsed().as_millis(),
                    "desktop first frame decoded"
                );
                first = false;
            }
            // Needed checkpoints can be older than a state already displayed.
            // Decode their reference update, but never regress the image.
            if images
                .borrow()
                .as_ref()
                .is_some_and(|image| image.id >= packet.id)
            {
                continue;
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

// Measure payload delivery, not idle time between captures. The initial rate
// matches the encoder's 2 Mbps target; complete keyframes calibrate it too.
struct Delivery {
    bytes_per_second: f64,
    measured: bool,
}
impl Default for Delivery {
    fn default() -> Self {
        Self {
            bytes_per_second: 250_000.0,
            measured: false,
        }
    }
}
impl Delivery {
    fn budget(&self, bytes: u64, rtt: Duration) -> Duration {
        Duration::from_secs_f64(bytes as f64 / self.bytes_per_second)
            + (rtt * 4).max(Duration::from_millis(150))
    }
    fn observe(&mut self, bytes: usize, elapsed: Duration, rtt: Duration) -> Option<u64> {
        // An already buffered frame says nothing about available bandwidth.
        // Tiny deltas are usually application/packet timing, not a bandwidth
        // sample; letting one loss-delayed delta set the rate would inflate
        // the next large frame's deadline arbitrarily.
        if elapsed < rtt.max(Duration::from_millis(100)) || bytes < 16 * 1024 {
            return None;
        }
        let rate = bytes as f64 / elapsed.as_secs_f64();
        // React immediately to a slower path; approach increases gradually.
        self.bytes_per_second = if self.measured {
            rate.min(self.bytes_per_second * 0.75 + rate * 0.25)
        } else {
            rate
        };
        self.measured = true;
        Some((self.bytes_per_second * 8.0) as u64)
    }
}

// Watch streaming progress with an independent cursor, then retain the complete
// contiguous payload without copying every chunk into a second frame
// allocation.
async fn read_payload(
    frame: &mut moq_net::frame::Consumer,
    reliable: bool,
    delivery: &mut Delivery,
    progress: &Progress,
    rtt: &impl Fn() -> Duration,
) -> Result<bytes::Bytes, moq_net::Error> {
    let mut observer = frame.clone();
    // Drain pre-existing bytes before starting the bandwidth clock. A buffered
    // frame and the idle interval before its header are not path measurements.
    while let Some(chunk) = observer.read_chunk().now_or_never() {
        if chunk?.is_none() {
            return frame.read_all().await;
        }
    }
    let mut sample_started = tokio::time::Instant::now();
    let mut sample_bytes = 0;
    loop {
        // Reliable checkpoints recover only on no progress. Disposable states
        // have no deadline: a final large state must finish unless superseded,
        // otherwise its unacknowledged bytes could strand source admission.
        let chunk = if reliable {
            let until = tokio::time::Instant::now()
                + delivery
                    .budget(frame.size, rtt())
                    .max(Duration::from_secs(3));
            tokio::time::timeout_at(until, observer.read_chunk())
                .await
                .map_err(|_| moq_net::Error::Stream(moq_net::StreamError::DeliveryTimeout))??
        } else {
            observer.read_chunk().await?
        };
        let Some(chunk) = chunk else {
            return frame.read_all().await;
        };
        sample_bytes += chunk.len();
        if let Some(bps) = delivery.observe(sample_bytes, sample_started.elapsed(), rtt()) {
            progress.feedback.lock().unwrap().delivery_bps = bps;
            sample_bytes = 0;
            sample_started = tokio::time::Instant::now();
        }
    }
}

async fn receive_packets(
    origin: moq_net::origin::Producer,
    packets: mpsc::Sender<Encoded>,
    states: watch::Sender<Option<Arc<Encoded>>>,
    progress: Arc<Progress>,
    started: Instant,
    desktop_id: u64,
    rtt: impl Fn() -> Duration,
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
    let broadcast = origin.consume().request_broadcast("app").await?;
    let mut reliable = broadcast.track("video")?.subscribe(None).await?.ordered();
    let mut disposable = broadcast.track("states")?.subscribe(None).await?.ordered();
    tracing::info!(
        desktop_id,
        elapsed_ms = started.elapsed().as_millis(),
        "desktop video subscribed"
    );
    let checkpoints = async {
        let mut delivery = Delivery::default();
        let mut baseline: Option<i128> = None;
        let clock = tokio::time::Instant::now();
        let mut next = reliable.next_group().await?;
        let mut ended = false;
        while let Some(mut group) = next.take() {
            let sequence = group.sequence;
            let epoch = AtomicU64::new(0);
            let mut base = 0;
            reliable.update(
                reliable
                    .subscription()
                    .with_start(moq_net::track::Position::group(sequence)),
            )?;
            loop {
                let previous_base = base;
                let read = async {
                    let Some(mut frame) = group.next_frame().await? else {
                        return Ok(None);
                    };
                    let timestamp = frame.timestamp.as_micros() as u64;
                    if previous_base == 0 {
                        epoch.store(timestamp, Ordering::Release);
                    }
                    if frame.size > (rho_desktop_media::MAX_PACKET + Header::SIZE) as u64 {
                        anyhow::bail!("video packet too large");
                    }
                    let payload =
                        read_payload(&mut frame, true, &mut delivery, &progress, &rtt).await?;
                    let (header, payload) = Header::unpack(payload)?;
                    anyhow::ensure!(
                        if previous_base == 0 {
                            header.kind == FrameKind::Key && header.epoch == timestamp
                        } else {
                            header.kind == FrameKind::Checkpoint
                                && header.epoch == epoch.load(Ordering::Acquire)
                                && header.base == previous_base
                                && timestamp > previous_base
                        },
                        "invalid checkpoint chain"
                    );
                    let offset = clock.elapsed().as_micros() as i128 - timestamp as i128;
                    if previous_base == 0 && progress.feedback.lock().unwrap().recover {
                        baseline = Some(offset);
                    }
                    let baseline = baseline.get_or_insert(offset);
                    *baseline = (*baseline).min(offset);
                    Ok::<_, anyhow::Error>(Some(Encoded {
                        header,
                        id: FrameId {
                            epoch: header.epoch,
                            timestamp_us: timestamp,
                        },
                        payload,
                        received_at: Instant::now(),
                        lag_us: (offset - *baseline) as u64,
                    }))
                };
                tokio::pin!(read);
                let packet = tokio::select! {
                    biased;
                    _ = progress.invalidated(epoch.load(Ordering::Acquire)), if base != 0 => {
                        recover(&mut reliable, &progress, sequence, epoch.load(Ordering::Acquire))?;
                        break;
                    }
                    newer = reliable.next_group(), if !ended => {
                        next = newer?;
                        if next.is_some() { break; }
                        ended = true;
                        read.await
                    }
                    packet = &mut read => packet,
                };
                match packet {
                    Ok(Some(packet)) => {
                        if base == 0 {
                            progress
                                .floor
                                .fetch_max(packet.header.epoch, Ordering::Release);
                        }
                        if !progress.accepts(packet.id) {
                            break;
                        }
                        base = packet.id.timestamp_us;
                        progress.received(&packet);
                        // Backpressure decoding rather than dropping needed
                        // checkpoints. A fresh root still preempts a full queue.
                        tokio::select! {
                            biased;
                            newer = reliable.next_group(), if !ended => {
                                next = newer?;
                                if next.is_some() { break; }
                                ended = true;
                                packets.reserve().await?.send(packet);
                            }
                            _ = progress.invalidated(packet.id.epoch) => {
                                recover(&mut reliable, &progress, sequence, packet.id.epoch)?;
                                break;
                            }
                            permit = packets.reserve() => { permit?.send(packet); }
                        }
                    }
                    Ok(None) => break,
                    Err(error)
                        if matches!(
                            error.downcast_ref::<moq_net::Error>(),
                            Some(
                                moq_net::Error::Old
                                    | moq_net::Error::Stream(moq_net::StreamError::Old)
                            )
                        ) =>
                    {
                        // Explicit supersession already has a new root on its
                        // way. Requesting another while it serializes is a storm.
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(desktop_id, sequence, %error, "desktop checkpoint recovery");
                        recover(
                            &mut reliable,
                            &progress,
                            sequence,
                            epoch.load(Ordering::Acquire),
                        )?;
                        break;
                    }
                }
            }
            if next.is_none() && !ended {
                next = reliable.next_group().await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let state_frames = async {
        let mut delivery = Delivery::default();
        let mut next = disposable.next_group().await?;
        let mut ended = false;
        while let Some(mut group) = next.take() {
            let read = async {
                let Some(mut frame) = group.next_frame().await? else {
                    return Ok(None);
                };
                if frame.size > (rho_desktop_media::MAX_PACKET + Header::SIZE) as u64 {
                    anyhow::bail!("state packet too large");
                }
                let timestamp_us = frame.timestamp.as_micros() as u64;
                let payload =
                    read_payload(&mut frame, false, &mut delivery, &progress, &rtt).await?;
                let (header, payload) = Header::unpack(payload)?;
                anyhow::ensure!(
                    header.kind == FrameKind::State && timestamp_us > header.base,
                    "invalid disposable state"
                );
                Ok::<_, anyhow::Error>(Some(Encoded {
                    header,
                    id: FrameId {
                        epoch: header.epoch,
                        timestamp_us,
                    },
                    payload,
                    received_at: Instant::now(),
                    lag_us: 0,
                }))
            };
            tokio::pin!(read);
            let packet = tokio::select! {
                biased;
                newer = disposable.next_group(), if !ended => {
                    next = newer?;
                    if next.is_some() { continue; }
                    ended = true;
                    read.await
                }
                packet = &mut read => packet,
            };
            if let Ok(Some(packet)) = packet {
                if progress.accepts(packet.id)
                    && states
                        .borrow()
                        .as_ref()
                        .is_none_or(|old| packet.id > old.id)
                {
                    progress.received(&packet);
                    states.send_replace(Some(Arc::new(packet)));
                }
            }
            // Cancellation/expiry of an optional state is normal. It never
            // invalidates the reliable reference or requests another root key.
            if next.is_none() && !ended {
                next = disposable.next_group().await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::try_join!(checkpoints, state_frames)?;
    Ok(())
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
    subscribe(transport, stream, started, id, connection).await
}

async fn subscribe(
    transport: rho_rpc::media::Session,
    stream: rho_rpc::Stream,
    started: Instant,
    desktop_id: u64,
    connection: iroh::endpoint::Connection,
) -> Result<Viewer> {
    let (mut reader, mut writer) = stream.into_split();
    let origin = rho_desktop_media::media::origin();
    let session = rho_desktop_media::media::subscribe(transport, origin.clone()).await?;
    let (images, receive) = watch::channel(None);
    let (errors, error_receive) = watch::channel(None);
    let (input, mut commands) = mpsc::channel(256);
    let progress = Arc::new(Progress::default());
    let (motion, mut movement) = watch::channel(None);
    let (packets, decode) = mpsc::channel::<Encoded>(2);
    let (states, state_decode) = watch::channel(None);
    let decoded = images.clone();
    let decoding = progress.clone();
    // Decode off the UI and Tokio IO workers; the renderer samples the
    // retained YUV planes.
    let decode_task = tokio::task::spawn_blocking(move || -> Result<()> {
        decode_packets(decode, state_decode, decoded, decoding, started, desktop_id)
    });
    let task = tokio::spawn(async move {
        let path_rtt = || {
            connection
                .paths()
                .iter()
                .filter(|path| path.is_selected())
                .map(|path| path.rtt())
                .max()
                .unwrap_or(Duration::from_millis(100))
        };
        let receive = receive_packets(
            origin,
            packets,
            states,
            progress.clone(),
            started,
            desktop_id,
            path_rtt,
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
                    _=ticks.tick()=> Input::Feedback(progress.report(path_rtt())),
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
    use bytes::Bytes;
    use rho_desktop_media::codec::Encoder;

    use super::*;

    fn id(epoch: u64, timestamp_us: u64) -> FrameId {
        FrameId {
            epoch,
            timestamp_us,
        }
    }
    fn packet(kind: FrameKind, epoch: u64, base: u64, timestamp: u64, payload: Bytes) -> Encoded {
        Encoded {
            header: Header { kind, epoch, base },
            id: id(epoch, timestamp),
            payload,
            received_at: Instant::now(),
            lag_us: 0,
        }
    }
    fn tracks(
        broadcast: &moq_net::broadcast::Producer,
    ) -> Result<(moq_net::track::Producer, moq_net::track::Producer)> {
        let info = moq_net::track::Info::default().with_timescale(moq_net::Timescale::MICRO);
        Ok((
            broadcast.create_track("video", Some(info.clone()))?,
            broadcast.create_track("states", Some(info))?,
        ))
    }
    fn write(group: &mut moq_net::group::Producer, packet: Encoded) -> Result<()> {
        group.write_frame(
            moq_net::Timestamp::from_micros(packet.id.timestamp_us)?,
            packet.header.pack(packet.payload),
        )?;
        Ok(())
    }
    fn receive(
        origin: moq_net::origin::Producer,
        progress: Arc<Progress>,
        capacity: usize,
    ) -> (
        tokio::task::JoinHandle<Result<()>>,
        mpsc::Receiver<Encoded>,
        watch::Receiver<Option<Arc<Encoded>>>,
    ) {
        let (packets, decode) = mpsc::channel(capacity);
        let (states, state_decode) = watch::channel(None);
        let task = tokio::spawn(receive_packets(
            origin,
            packets,
            states,
            progress,
            Instant::now(),
            0,
            || Duration::from_millis(200),
        ));
        (task, decode, state_decode)
    }
    fn vp9(encoder: &mut Encoder, kind: FrameKind, color: [u8; 4]) -> Result<Bytes> {
        let encoded = encoder.encode(&color.repeat(48 * 24), kind)?.remove(0);
        assert_eq!(encoded.kind, kind);
        Ok(encoded.data.into())
    }
    async fn image_at(
        images: &mut watch::Receiver<Option<Arc<Image>>>,
        timestamp: u64,
    ) -> Result<Arc<Image>> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(image) = images.borrow_and_update().clone() {
                    if image.id.timestamp_us == timestamp {
                        return Ok(image);
                    }
                }
                images.changed().await?;
            }
        })
        .await?
    }

    #[test]
    fn delivery_budget_uses_size_rtt_and_unbuffered_goodput() {
        let mut delivery = Delivery::default();
        assert_eq!(
            delivery.budget(400_000, Duration::from_millis(200)),
            Duration::from_millis(2400)
        );
        assert_eq!(
            delivery.budget(100_000, Duration::from_millis(400)),
            Duration::from_secs(2)
        );
        delivery.observe(250_000, Duration::from_secs(2), Duration::from_millis(200));
        assert_eq!(
            delivery.budget(400_000, Duration::from_millis(200)),
            Duration::from_secs(4)
        );
        delivery.observe(2_000_000, Duration::ZERO, Duration::from_millis(200));
        delivery.observe(100, Duration::from_secs(2), Duration::from_millis(200));
        assert_eq!(
            delivery.budget(400_000, Duration::from_millis(200)),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn future_state_receipt_cannot_acknowledge_missing_checkpoints() {
        let progress = Progress::default();
        progress.received(&packet(FrameKind::Key, 10, 0, 10, Bytes::new()));
        progress.received(&packet(FrameKind::State, 10, 30, 40, Bytes::new()));
        assert_eq!(progress.report(Duration::ZERO).received, Some(id(10, 10)));
        progress.received(&packet(FrameKind::Checkpoint, 10, 10, 20, Bytes::new()));
        assert_eq!(progress.report(Duration::ZERO).received, Some(id(10, 20)));
        progress.received(&packet(FrameKind::Checkpoint, 10, 20, 30, Bytes::new()));
        assert_eq!(progress.report(Duration::ZERO).received, Some(id(10, 40)));
        // Late old-base states cannot regress or falsely advance the frontier.
        progress.received(&packet(FrameKind::State, 10, 10, 50, Bytes::new()));
        assert_eq!(progress.report(Duration::ZERO).received, Some(id(10, 40)));
        progress.received(&packet(FrameKind::Key, 60, 0, 60, Bytes::new()));
        assert_eq!(progress.report(Duration::ZERO).received, Some(id(60, 60)));
    }

    #[test]
    fn recovery_reports_repeat_identity_until_a_new_root_arrives() {
        let progress = Progress::default();
        assert_eq!(progress.report(Duration::ZERO).recovery_id, 0);
        progress.recover(4, 5);
        for _ in 0..30 {
            let report = progress.report(Duration::from_millis(200));
            assert!(report.recover);
            assert_eq!(report.recovery_id, 1);
            assert_eq!(report.rtt_us, 200_000);
        }
        // Another observer of the same failed chain is not a new failure.
        progress.recover(4, 9);
        progress.recover(3, 4);
        assert_eq!(progress.report(Duration::ZERO).recovery_id, 1);
        // Reception, not group announcement or feedback reporting, clears it.
        progress.floor.fetch_max(6, Ordering::Release);
        assert!(progress.report(Duration::ZERO).recover);
        progress.feedback.lock().unwrap().recover = false;
        assert!(!progress.report(Duration::ZERO).recover);
        progress.recover(6, 7);
        let report = progress.report(Duration::from_millis(400));
        assert!(report.recover);
        assert_eq!(report.recovery_id, 2);
        assert_eq!(report.rtt_us, 400_000);
        // A failed replacement is distinct even while recovery is pending.
        progress.recover(7, 8);
        assert_eq!(progress.report(Duration::ZERO).recovery_id, 3);
    }

    #[test]
    fn recovery_invalidates_decode_and_late_presentation_by_epoch() {
        let progress = Progress::default();
        progress.presented(id(3, 100), 15);
        progress.recover(4, 5);
        // A stale callback and an older recovery request cannot undo the floor.
        progress.recover(3, 4);
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

    #[tokio::test(start_paused = true)]
    async fn arriving_checkpoints_outlive_initial_budget_and_report_in_flight_delivery()
    -> Result<()> {
        let origin = rho_desktop_media::media::origin();
        let broadcast = origin.create_broadcast("app")?;
        let (track, _states) = tracks(&broadcast)?;
        broadcast.announce(Default::default())?;
        let remote = rho_desktop_media::media::origin();
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (_publisher, _subscriber) = tokio::try_join!(
            rho_desktop_media::media::local_server(server, &origin),
            rho_desktop_media::media::local_client(client, remote.clone())
        )?;
        let progress = Arc::new(Progress::default());
        let (receiving, mut received, _states) = receive(remote, progress.clone(), 2);
        track.used().await?;
        let mut group = track.append_group()?;
        write(
            &mut group,
            packet(FrameKind::Key, 10, 0, 10, Bytes::from(vec![7; 100_000])),
        )?;
        assert_eq!(received.recv().await.unwrap().id, id(10, 10));
        assert_eq!(progress.report(Duration::from_millis(200)).delivery_bps, 0);
        tokio::time::sleep(Duration::from_secs(10)).await;
        let mut frame = group.create_frame(moq_net::frame::Info {
            timestamp: moq_net::Timestamp::from_micros(20)?,
            size: 400_000 + Header::SIZE as u64,
        })?;
        // The buffered prefix and ten seconds of source idle are not goodput.
        let header = Header {
            kind: FrameKind::Checkpoint,
            epoch: 10,
            base: 10,
        };
        frame.write(header.pack(Bytes::from(vec![9; 40_000])))?;
        tokio::task::yield_now().await;
        for chunk in 0..8 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            frame.write(Bytes::from(vec![chunk; 45_000]))?;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            let report = progress.report(Duration::from_millis(200));
            assert!(!report.recover);
            assert!(
                (350_000..=370_000).contains(&report.delivery_bps),
                "{report:?}"
            );
            if chunk < 7 {
                assert_eq!(report.received, Some(id(10, 10)));
            }
        }
        frame.finish()?;
        let checkpoint = received.recv().await.unwrap();
        assert_eq!(checkpoint.id, id(10, 20));
        let expected: Vec<_> = vec![9; 40_000]
            .into_iter()
            .chain((0..8).flat_map(|chunk| vec![chunk; 45_000]))
            .collect();
        assert_eq!(checkpoint.payload.as_ref(), expected);
        // Idle capture never times out, but a started checkpoint with no
        // delivery has the same no-progress watchdog as its root key.
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!progress.report(Duration::ZERO).recover);
        let _stalled = group.create_frame(moq_net::frame::Info {
            timestamp: moq_net::Timestamp::from_micros(30)?,
            size: 100,
        })?;
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(2900)).await;
        assert!(!progress.report(Duration::ZERO).recover);
        tokio::time::sleep(Duration::from_millis(200)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(progress.report(Duration::ZERO).recover);
        assert_eq!(progress.report(Duration::ZERO).recovery_id, 1);
        assert_eq!(progress.floor.load(Ordering::Acquire), 11);
        // A distinct stalled replacement creates a new request even while
        // the previous recover flag is still set.
        let mut replacement = track.append_group()?;
        let _also_stalled = replacement.create_frame(moq_net::frame::Info {
            timestamp: moq_net::Timestamp::from_micros(40)?,
            size: 800_000,
        })?;
        tokio::task::yield_now().await;
        // The replacement inherits the measured 45KB/s path, so its
        // 800KB serialization + 4 RTTs is about 18.58s, not the old 4s.
        tokio::time::sleep(Duration::from_secs(18)).await;
        assert_eq!(progress.report(Duration::ZERO).recovery_id, 1);
        tokio::time::sleep(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(progress.report(Duration::ZERO).recovery_id, 2);
        receiving.abort();
        Ok(())
    }

    #[tokio::test]
    async fn newer_root_preempts_unfinished_old_checkpoint() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let origin = rho_desktop_media::media::origin();
            let broadcast = origin.create_broadcast("app")?;
            let (track, _states) = tracks(&broadcast)?;
            broadcast.announce(Default::default())?;
            let progress = Arc::new(Progress::default());
            let (receiving, mut received, _) = receive(origin, progress.clone(), 2);
            track.used().await?;
            let mut old = track.append_group()?;
            write(
                &mut old,
                packet(FrameKind::Key, 10, 0, 10, Bytes::from_static(b"old")),
            )?;
            assert_eq!(received.recv().await.unwrap().id, id(10, 10));
            let mut partial = old.create_frame(moq_net::frame::Info {
                timestamp: moq_net::Timestamp::from_micros(20)?,
                size: 100,
            })?;
            partial.write(
                Header {
                    kind: FrameKind::Checkpoint,
                    epoch: 10,
                    base: 10,
                }
                .pack(Bytes::from_static(b"unfinished")),
            )?;
            let mut fresh = track.append_group()?;
            write(
                &mut fresh,
                packet(FrameKind::Key, 30, 0, 30, Bytes::from_static(b"fresh")),
            )?;
            assert_eq!(received.recv().await.unwrap().id, id(30, 30));
            assert!(!progress.accepts(id(10, 20)));
            assert_eq!(
                track.subscription().unwrap().start.unwrap().group,
                fresh.sequence
            );
            assert!(!progress.report(Duration::ZERO).recover);
            receiving.abort();
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn full_decoder_queue_backpressures_without_dropping_checkpoints() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let origin = rho_desktop_media::media::origin();
            let broadcast = origin.create_broadcast("app")?;
            let (track, _states) = tracks(&broadcast)?;
            broadcast.announce(Default::default())?;
            let progress = Arc::new(Progress::default());
            let (receiving, mut received, _) = receive(origin, progress.clone(), 1);
            track.used().await?;
            let mut group = track.append_group()?;
            write(
                &mut group,
                packet(FrameKind::Key, 10, 0, 10, Bytes::from_static(b"key")),
            )?;
            write(
                &mut group,
                packet(
                    FrameKind::Checkpoint,
                    10,
                    10,
                    20,
                    Bytes::from_static(b"first"),
                ),
            )?;
            write(
                &mut group,
                packet(
                    FrameKind::Checkpoint,
                    10,
                    20,
                    30,
                    Bytes::from_static(b"second"),
                ),
            )?;
            while progress.report(Duration::ZERO).received != Some(id(10, 20)) {
                tokio::task::yield_now().await;
            }
            assert!(!progress.report(Duration::ZERO).recover);
            for timestamp in [10, 20, 30] {
                assert_eq!(received.recv().await.unwrap().id, id(10, timestamp));
            }
            assert!(!progress.report(Duration::ZERO).recover);
            receiving.abort();
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_state_is_replaced_but_slow_final_state_finishes_without_recovery()
    -> Result<()> {
        let origin = rho_desktop_media::media::origin();
        let broadcast = origin.create_broadcast("app")?;
        let (track, states) = tracks(&broadcast)?;
        broadcast.announce(Default::default())?;
        let progress = Arc::new(Progress::default());
        let remote = rho_desktop_media::media::origin();
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (_publisher, _subscriber) = tokio::try_join!(
            rho_desktop_media::media::local_server(server, &origin),
            rho_desktop_media::media::local_client(client, remote.clone())
        )?;
        let (receiving, mut received, mut latest) = receive(remote, progress.clone(), 2);
        track.used().await?;
        let mut reliable = track.append_group()?;
        write(
            &mut reliable,
            packet(FrameKind::Key, 10, 0, 10, Bytes::from_static(b"key")),
        )?;
        received.recv().await.unwrap();
        let mut old = states.append_group()?;
        let mut partial = old.create_frame(moq_net::frame::Info {
            timestamp: moq_net::Timestamp::from_micros(20)?,
            size: 100,
        })?;
        partial.write(
            Header {
                kind: FrameKind::State,
                epoch: 10,
                base: 10,
            }
            .pack(Bytes::from_static(b"partial")),
        )?;
        tokio::task::yield_now().await;
        partial.abort(moq_net::Error::Old)?;
        let mut fresh = states.append_group()?;
        write(
            &mut fresh,
            packet(FrameKind::State, 10, 10, 30, Bytes::from_static(b"fresh")),
        )?;
        latest.changed().await?;
        assert_eq!(latest.borrow_and_update().as_ref().unwrap().id, id(10, 30));
        let mut final_group = states.append_group()?;
        let mut final_state = final_group.create_frame(moq_net::frame::Info {
            timestamp: moq_net::Timestamp::from_micros(50)?,
            size: 400_000 + Header::SIZE as u64,
        })?;
        final_state.write(
            Header {
                kind: FrameKind::State,
                epoch: 10,
                base: 10,
            }
            .pack(Bytes::from(vec![9; 40_000])),
        )?;
        tokio::task::yield_now().await;
        // Eight seconds is far beyond the initial 2.4s estimate. No newer
        // group exists: silently timing this out would strand sender debt.
        for chunk in 0..8 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            final_state.write(Bytes::from(vec![chunk; 45_000]))?;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert_eq!(progress.report(Duration::ZERO).recovery_id, 0);
            if chunk < 7 {
                assert_eq!(latest.borrow().as_ref().unwrap().id, id(10, 30));
                assert_eq!(progress.report(Duration::ZERO).received, Some(id(10, 30)));
            }
        }
        final_state.finish()?;
        latest.changed().await?;
        let newest = latest.borrow().clone().unwrap();
        assert_eq!(newest.id, id(10, 50));
        let expected: Vec<_> = vec![9; 40_000]
            .into_iter()
            .chain((0..8).flat_map(|chunk| vec![chunk; 45_000]))
            .collect();
        assert_eq!(newest.payload.as_ref(), expected);
        assert_eq!(progress.report(Duration::ZERO).received, Some(id(10, 50)));
        assert_eq!(progress.report(Duration::ZERO).recovery_id, 0);
        receiving.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn explicitly_superseded_root_waits_for_slow_replacement_without_recovery_storm()
    -> Result<()> {
        let origin = rho_desktop_media::media::origin();
        let broadcast = origin.create_broadcast("app")?;
        let (track, _states) = tracks(&broadcast)?;
        broadcast.announce(Default::default())?;
        let remote = rho_desktop_media::media::origin();
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (_publisher, _subscriber) = tokio::try_join!(
            rho_desktop_media::media::local_server(server, &origin),
            rho_desktop_media::media::local_client(client, remote.clone())
        )?;
        let progress = Arc::new(Progress::default());
        let (receiving, mut received, _) = receive(remote, progress.clone(), 2);
        track.used().await?;
        let mut old = track.append_group()?;
        write(
            &mut old,
            packet(FrameKind::Key, 10, 0, 10, Bytes::from_static(b"old root")),
        )?;
        assert_eq!(received.recv().await.unwrap().id, id(10, 10));
        old.abort(moq_net::Error::Old)?;
        // Let Old arrive before announcing the replacement, to exercise the
        // error path rather than only next_group's preemption branch.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(progress.report(Duration::ZERO).recovery_id, 0);
        let mut fresh = track.append_group()?;
        let mut frame = fresh.create_frame(moq_net::frame::Info {
            timestamp: moq_net::Timestamp::from_micros(30)?,
            size: 50_000 + Header::SIZE as u64,
        })?;
        frame.write(
            Header {
                kind: FrameKind::Key,
                epoch: 30,
                base: 0,
            }
            .pack(Bytes::new()),
        )?;
        for index in 0..5 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            frame.write(Bytes::from(vec![index + 1; 10_000]))?;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert_eq!(progress.report(Duration::ZERO).recovery_id, 0);
            assert!(!progress.report(Duration::ZERO).recover);
        }
        frame.finish()?;
        let root = received.recv().await.unwrap();
        assert_eq!(root.id, id(30, 30));
        assert_eq!(
            root.payload.as_ref(),
            (1..=5).flat_map(|n| vec![n; 10_000]).collect::<Vec<_>>()
        );
        receiving.abort();
        Ok(())
    }

    #[tokio::test]
    async fn decoder_retains_only_newest_future_state_and_decodes_every_checkpoint() -> Result<()> {
        let mut encoder = Encoder::new(48, 24, 500_000)?;
        let root = vp9(&mut encoder, FrameKind::Key, [37, 91, 203, 255])?;
        let checkpoint1 = vp9(&mut encoder, FrameKind::Checkpoint, [150, 19, 71, 255])?;
        let checkpoint2 = vp9(&mut encoder, FrameKind::Checkpoint, [7, 173, 44, 255])?;
        let state = vp9(&mut encoder, FrameKind::State, [89, 13, 241, 255])?;
        let mut expected = Decoder::new()?;
        expected.decode_planes(&root)?;
        expected.decode_planes(&checkpoint1)?;
        expected.decode_planes(&checkpoint2)?;
        let expected =
            rho_desktop_media::codec::export_bgra(&expected.decode_planes(&state)?.unwrap())?.bgra;
        let progress = Arc::new(Progress::default());
        let (packets, decode) = mpsc::channel(2);
        let (states, state_decode) = watch::channel(None);
        let (images, mut received) = watch::channel(None);
        let decoding = progress.clone();
        let worker = tokio::task::spawn_blocking(move || {
            decode_packets(decode, state_decode, images, decoding, Instant::now(), 0)
        });
        packets
            .send(packet(FrameKind::Key, 100, 0, 100, root))
            .await?;
        image_at(&mut received, 100).await?;
        // Invalid VP9 in a superseded future state must never reach libvpx.
        states.send_replace(Some(Arc::new(packet(
            FrameKind::State,
            100,
            300,
            310,
            Bytes::from_static(b"invalid superseded VP9"),
        ))));
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(received.borrow().as_ref().unwrap().id, id(100, 100));
        assert!(!progress.report(Duration::ZERO).recover);
        states.send_replace(Some(Arc::new(packet(
            FrameKind::State,
            100,
            300,
            320,
            state,
        ))));
        packets
            .send(packet(FrameKind::Checkpoint, 100, 100, 200, checkpoint1))
            .await?;
        packets
            .send(packet(FrameKind::Checkpoint, 100, 200, 300, checkpoint2))
            .await?;
        let newest = image_at(&mut received, 320).await?;
        assert_eq!(newest.export_bgra()?, expected);
        assert_eq!(progress.report(Duration::ZERO).decoded, Some(id(100, 320)));
        assert!(!progress.report(Duration::ZERO).recover);
        // An old-base state is discarded, not decoded and not recovery-worthy.
        states.send_replace(Some(Arc::new(packet(
            FrameKind::State,
            100,
            100,
            350,
            Bytes::from_static(b"invalid obsolete VP9"),
        ))));
        drop(states);
        drop(packets);
        worker.await??;
        assert!(!progress.report(Duration::ZERO).recover);
        assert_eq!(received.borrow().as_ref().unwrap().id, id(100, 320));
        Ok(())
    }

    #[tokio::test]
    async fn needed_checkpoint_decodes_without_regressing_newer_state_presentation() -> Result<()> {
        let mut encoder = Encoder::new(48, 24, 500_000)?;
        let root = vp9(&mut encoder, FrameKind::Key, [13, 77, 199, 255])?;
        let state1 = vp9(&mut encoder, FrameKind::State, [179, 15, 38, 255])?;
        let checkpoint = vp9(&mut encoder, FrameKind::Checkpoint, [67, 211, 92, 255])?;
        let state2 = vp9(&mut encoder, FrameKind::State, [99, 35, 230, 255])?;
        let mut expected = Decoder::new()?;
        expected.decode_planes(&root)?;
        expected.decode_planes(&checkpoint)?;
        let expected =
            rho_desktop_media::codec::export_bgra(&expected.decode_planes(&state2)?.unwrap())?.bgra;
        let progress = Arc::new(Progress::default());
        let (packets, decode) = mpsc::channel(2);
        let (states, state_decode) = watch::channel(None);
        let (images, mut received) = watch::channel(None);
        let decoding = progress.clone();
        let worker = tokio::task::spawn_blocking(move || {
            decode_packets(decode, state_decode, images, decoding, Instant::now(), 0)
        });
        packets
            .send(packet(FrameKind::Key, 100, 0, 100, root))
            .await?;
        image_at(&mut received, 100).await?;
        states.send_replace(Some(Arc::new(packet(
            FrameKind::State,
            100,
            100,
            300,
            state1,
        ))));
        let older = image_at(&mut received, 300).await?;
        older.presented();
        packets
            .send(packet(FrameKind::Checkpoint, 100, 100, 200, checkpoint))
            .await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), received.changed())
                .await
                .is_err()
        );
        assert_eq!(received.borrow().as_ref().unwrap().id, id(100, 300));
        states.send_replace(Some(Arc::new(packet(
            FrameKind::State,
            100,
            200,
            400,
            state2,
        ))));
        let newest = image_at(&mut received, 400).await?;
        assert_eq!(newest.export_bgra()?, expected);
        newest.presented();
        older.presented();
        assert_eq!(
            progress.report(Duration::ZERO).presented,
            Some(id(100, 400))
        );
        drop(packets);
        drop(states);
        worker.await??;
        Ok(())
    }

    #[tokio::test]
    async fn decoder_failure_cancels_idle_epoch_and_resumes_at_next_root() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let origin = rho_desktop_media::media::origin();
            let broadcast = origin.create_broadcast("app")?;
            let (track, _states) = tracks(&broadcast)?;
            broadcast.announce(Default::default())?;
            let progress = Arc::new(Progress::default());
            let (packets, decode) = mpsc::channel(2);
            let (states, state_decode) = watch::channel(None);
            let (images, mut received) = watch::channel(None);
            let decoding = progress.clone();
            let worker = tokio::task::spawn_blocking(move || {
                decode_packets(decode, state_decode, images, decoding, Instant::now(), 0)
            });
            let receiving = tokio::spawn(receive_packets(
                origin,
                packets,
                states,
                progress.clone(),
                Instant::now(),
                0,
                || Duration::from_millis(10),
            ));
            track.used().await?;
            let mut failed = track.append_group()?;
            write(
                &mut failed,
                packet(
                    FrameKind::Key,
                    10,
                    0,
                    10,
                    Bytes::from_static(b"invalid VP9"),
                ),
            )?;
            while track
                .subscription()
                .and_then(|subscription| subscription.start)
                .is_none_or(|position| position.group <= failed.sequence)
            {
                tokio::task::yield_now().await;
            }
            assert_eq!(progress.report(Duration::ZERO).recovery_id, 1);
            assert_eq!(progress.report(Duration::ZERO).received, Some(id(10, 10)));
            write(
                &mut failed,
                packet(
                    FrameKind::Checkpoint,
                    10,
                    10,
                    20,
                    Bytes::from_static(b"invalid abandoned VP9"),
                ),
            )?;
            let mut encoder = Encoder::new(48, 24, 500_000)?;
            let root = vp9(&mut encoder, FrameKind::Key, [37, 91, 203, 255])?;
            let mut replacement = track.append_group()?;
            write(&mut replacement, packet(FrameKind::Key, 30, 0, 30, root))?;
            let image = image_at(&mut received, 30).await?;
            assert_eq!(image.id, id(30, 30));
            assert_eq!((image.width, image.height), (48, 24));
            assert!(!progress.report(Duration::ZERO).recover);
            receiving.abort();
            let _ = receiving.await;
            worker.await??;
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn decoder_skips_obsolete_queued_packets_and_restarts_at_root() -> Result<()> {
        let progress = Arc::new(Progress::default());
        progress.recover(6, 7);
        let (packets, decode) = mpsc::channel(3);
        let (states, state_decode) = watch::channel(None);
        let (images, received) = watch::channel(None);
        for timestamp in [6, 20] {
            packets.try_send(packet(
                if timestamp == 6 {
                    FrameKind::Key
                } else {
                    FrameKind::Checkpoint
                },
                6,
                if timestamp == 6 { 0 } else { 6 },
                timestamp,
                Bytes::from_static(b"obsolete invalid VP9"),
            ))?;
        }
        let mut encoder = Encoder::new(48, 24, 500_000)?;
        let root = vp9(&mut encoder, FrameKind::Key, [37, 91, 203, 255])?;
        packets.try_send(packet(FrameKind::Key, 30, 0, 30, root))?;
        drop(packets);
        drop(states);
        let decoding = progress.clone();
        tokio::task::spawn_blocking(move || {
            decode_packets(decode, state_decode, images, decoding, Instant::now(), 0)
        })
        .await??;
        let image = received.borrow().clone().unwrap();
        assert_eq!(image.id, id(30, 30));
        assert_eq!(progress.report(Duration::ZERO).decoded, Some(id(30, 30)));
        image.presented();
        assert_eq!(progress.report(Duration::ZERO).presented, Some(id(30, 30)));
        Ok(())
    }
}
