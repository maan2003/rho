//! A native VP9 viewer connection. Only decoded images are coalesced.
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rho_desktop_media::codec::Decoder;
use rho_desktop_proto::Input;
use tokio::sync::{mpsc, watch};

pub struct Image {
    pub width: usize,
    pub height: usize,
    pub render: gpui::VideoFrame,
    pub planes: Arc<rho_desktop_media::codec::RetainedFrame>,
}
impl Image {
    /// Convert only when the user exports a screenshot.
    pub fn export_bgra(&self) -> Result<Vec<u8>> {
        Ok(rho_desktop_media::codec::export_bgra(&self.planes)?.bgra)
    }
}
struct VideoData(Arc<rho_desktop_media::codec::RetainedFrame>);
impl gpui::Yuv444Data for VideoData {
    fn size(&self) -> (u32, u32) {
        (self.0.width() as u32, self.0.height() as u32)
    }
    fn plane(&self, index: usize) -> &[u8] {
        self.0.plane(index)
    }
    fn stride(&self, index: usize) -> u32 {
        self.0.stride(index) as u32
    }
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

pub(crate) async fn open(
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
    let feedback = input.clone();
    let _ = input.try_send(Input::Quality {
        bitrate: 2_000_000,
        keyframe: true,
    });
    let (motion, mut movement) = watch::channel(None);
    let (packets, mut decode) = mpsc::channel::<bytes::Bytes>(2);
    let decoded = images.clone();
    // Decode off GPUI and Tokio IO workers. GPUI samples the retained YUV planes.
    let decode_task = tokio::task::spawn_blocking(move || -> Result<()> {
        tracing::info!(
            desktop_id,
            elapsed_ms = started.elapsed().as_millis(),
            "desktop decoder task started"
        );
        let mut decoder = Decoder::new()?;
        let mut first = true;
        while let Some(packet) = decode.blocking_recv() {
            if let Some(frame) = decoder.decode_planes(&packet)? {
                if first {
                    tracing::info!(
                        desktop_id,
                        elapsed_ms = started.elapsed().as_millis(),
                        "desktop first frame decoded"
                    );
                    first = false;
                }
                let planes = Arc::new(frame);
                let render = gpui::VideoFrame::new(Arc::new(VideoData(planes.clone())))?;
                decoded.send_replace(Some(Arc::new(Image {
                    width: planes.width(),
                    height: planes.height(),
                    render,
                    planes,
                })));
            }
        }

        Ok(())
    });
    let task = tokio::spawn(async move {
        struct Close(moq_net::Session);
        impl Drop for Close {
            fn drop(&mut self) {
                self.0.abort(moq_net::Error::Cancel);
            }
        }
        let _close = Close(session.clone());
        let receive = async {
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

            while let Some(mut group) = subscription.next_group().await? {
                // Every group begins with a keyframe, which resets references
                // without destroying the decoder or its reusable frame pool.
                loop {
                    let frame = async {
                        let Some(mut frame) = group.next_frame().await? else {
                            return Ok(None);
                        };
                        if frame.size > rho_desktop_media::MAX_PACKET as u64 {
                            return Err(moq_net::Error::Cancel);
                        }
                        let timestamp = frame.timestamp;
                        let payload = frame.read_all().await?;
                        Ok::<_, moq_net::Error>(Some(moq_net::frame::Frame { timestamp, payload }))
                    }
                    .await;
                    let lag = match frame {
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
                            let offset = clock.elapsed().as_micros() as i128
                                - frame.timestamp.as_micros() as i128;
                            let base = baseline.get_or_insert(offset);
                            *base = (*base).min(offset);
                            let lag = offset - *base;
                            packets
                                .send(frame.payload)
                                .await
                                .map_err(|_| anyhow::anyhow!("decoder stopped"))?;
                            lag
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
                            let _ = feedback.try_send(Input::Quality {
                                bitrate: rate,
                                keyframe: true,
                            });
                            break;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    if sample.elapsed() >= Duration::from_secs(1) {
                        let congested = lag > 150_000;
                        rate = if congested {
                            rate * 3 / 4
                        } else {
                            rate + rate / 20
                        };
                        rate = rate.clamp(128_000, 4_000_000);
                        let _ = feedback.try_send(Input::Quality {
                            bitrate: rate,
                            keyframe: congested,
                        });
                        sample = Instant::now();
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        let control = async {
            loop {
                let input = tokio::select! {
                    biased;
                    command=commands.recv()=>match command { Some(input)=>input,None=>break },
                    changed=movement.changed()=> {
                        changed?;
                        let Some((x,y))=*movement.borrow_and_update() else {continue};
                        Input::Move{x,y}
                    }
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
