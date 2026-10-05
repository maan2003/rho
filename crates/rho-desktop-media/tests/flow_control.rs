//! Admission is accounted before fixed MoQ/QMux can buffer encoded bytes.
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use rho_desktop_media::sender::Sender;
use rho_desktop_media::{FrameKind, Header, media};
use rho_desktop_proto::{Feedback, FrameId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn feedback(received: Option<FrameId>) -> Feedback {
    Feedback {
        received,
        decoded: received,
        presented: received,
        delivery_bps: 2_000_000,
        rtt_us: 200_000,
        ..Default::default()
    }
}

#[tokio::test]
async fn oversize_refinement_and_recovery_across_a_bounded_link() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        let source = media::origin();
        let viewer = media::origin();
        let broadcast = source.create_broadcast("app")?;
        broadcast.announce(Default::default())?;
        let mut video = media::Video::new(&broadcast)?;

        // Fixed QMux runs over a bounded byte link. Each permit passes at most
        // 4096 bytes toward the viewer; reverse control traffic is never gated.
        let (server, bridge_in) = tokio::io::duplex(4096);
        let (bridge_out, client) = tokio::io::duplex(4096);
        let (mut read, mut reverse_write) = tokio::io::split(bridge_in);
        let (mut reverse_read, mut write) = tokio::io::split(bridge_out);
        let permits = Arc::new(tokio::sync::Semaphore::new(16));
        let gate = permits.clone();
        let bridge = tokio::spawn(async move {
            let forward = async {
                let mut bytes = [0; 4096];
                loop {
                    gate.acquire().await?.forget();
                    let size = read.read(&mut bytes).await?;
                    if size == 0 {
                        return Ok::<(), anyhow::Error>(());
                    }
                    write.write_all(&bytes[..size]).await?;
                }
            };
            let reverse = async {
                tokio::io::copy(&mut reverse_read, &mut reverse_write).await?;
                Ok::<(), anyhow::Error>(())
            };
            tokio::try_join!(forward, reverse)?;
            Ok::<(), anyhow::Error>(())
        });
        let (_sending, _receiving) = tokio::try_join!(
            media::local_server(server, &source),
            media::local_client(client, viewer.clone()),
        )?;
        video.track.used().await?;
        let remote = viewer.consume().request_broadcast("app").await?;
        let mut track = remote.track("video")?.subscribe(None).await?.ordered();

        let mut sender = Sender::default();
        sender.feedback(1, feedback(None), Instant::now());
        assert_eq!(sender.budget(Instant::now()).window_bytes, 75_000);
        assert!(sender.budget(Instant::now()).ready);
        let payload = Bytes::from((0..400_000).map(|n| (n % 251) as u8).collect::<Vec<_>>());
        sender.encoded(
            17_000,
            payload.len(),
            FrameKind::Checkpoint,
            false,
            Instant::now(),
        );
        video.write(FrameKind::Key, 17_000, payload.clone())?;
        let mut group = track
            .next_group()
            .await?
            .context("missing keyframe group")?;
        let mut frame = Box::pin(group.read_frame());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut frame)
                .await
                .is_err(),
            "the bounded link cannot deliver this frame on its startup permits"
        );

        // Repeated feedback without a complete receipt must not admit either
        // dependents or idle refinement while the large frame is serializing.
        for _ in 0..20 {
            sender.feedback(1, feedback(None), Instant::now());
            let budget = sender.budget(Instant::now());
            assert_eq!(budget.outstanding_bytes, 400_000);
            assert!(!budget.ready);
            assert!(!budget.refine);
        }
        let delivered = async {
            for _ in 0..110 {
                permits.add_permits(1);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let (received, ()) = tokio::join!(&mut frame, delivered);
        let received = received?.context("oversized frame truncated")?;
        assert_eq!(received.timestamp, moq_net::Timestamp::from_micros(17_000)?);
        assert_eq!(Header::unpack(received.payload)?.1, payload);
        drop(frame);
        let receipt = FrameId {
            epoch: 17_000,
            timestamp_us: 17_000,
        };
        sender.feedback(1, feedback(Some(receipt)), Instant::now());
        assert_eq!(sender.budget(Instant::now()).outstanding_bytes, 0);
        assert!(sender.budget(Instant::now()).refine);
        let last_feedback = Instant::now();
        sender.feedback(1, feedback(Some(receipt)), last_feedback);
        // No transport debt is not permission to ignore a stalled control path.
        assert!(
            sender
                .budget(last_feedback + Duration::from_millis(1000))
                .ready
        );
        let stale = sender.budget(last_feedback + Duration::from_millis(1001));
        assert!(!stale.ready);
        assert!(!stale.refine);
        sender.feedback(1, feedback(Some(receipt)), Instant::now());
        assert!(sender.budget(Instant::now()).refine);

        let refinement = Bytes::from_static(b"final asymmetric refinement");
        sender.encoded(
            18_250,
            refinement.len(),
            FrameKind::Checkpoint,
            true,
            Instant::now(),
        );
        video.write(FrameKind::Checkpoint, 18_250, refinement.clone())?;
        assert!(!sender.budget(Instant::now()).refine);
        permits.add_permits(16);
        let final_frame = group.read_frame().await?.context("missing refinement")?;
        assert_eq!(
            final_frame.timestamp,
            moq_net::Timestamp::from_micros(18_250)?
        );
        assert_eq!(Header::unpack(final_frame.payload)?.1, refinement);
        sender.feedback(
            1,
            feedback(Some(FrameId {
                timestamp_us: 18_250,
                ..receipt
            })),
            Instant::now(),
        );
        assert!(sender.budget(Instant::now()).refine);

        // Stall a dependent in the old group. One recovery generation grants
        // the compositor a keyframe bypass, without forgetting transport debt.
        permits.forget_permits(permits.available_permits());
        sender.encoded(
            19_000,
            400_000,
            FrameKind::Checkpoint,
            false,
            Instant::now(),
        );
        video.write(
            FrameKind::Checkpoint,
            19_000,
            Bytes::from(vec![93; 400_000]),
        )?;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), group.read_frame())
                .await
                .is_err()
        );
        assert!(!sender.budget(Instant::now()).ready);
        let mut recovery = feedback(Some(FrameId {
            timestamp_us: 18_250,
            ..receipt
        }));
        recovery.recover = true;
        recovery.recovery_id = 7;
        assert!(sender.feedback(1, recovery, Instant::now()));
        assert!(
            !sender.feedback(1, recovery, Instant::now()),
            "a repeated generation cannot grant a second keyframe bypass"
        );
        // The recover flag can remain set when a replacement request arrives.
        recovery.recovery_id = 8;
        assert!(sender.feedback(1, recovery, Instant::now()));
        assert!(!sender.feedback(1, recovery, Instant::now()));
        assert_eq!(sender.budget(Instant::now()).outstanding_bytes, 400_000);

        track.control().update(
            track
                .control()
                .subscription()
                .with_start(moq_net::track::Position::group(group.sequence + 1)),
        )?;
        let fresh_payload = Bytes::from_static(b"fresh recovery key");
        sender.encoded(
            20_000,
            fresh_payload.len(),
            FrameKind::Checkpoint,
            false,
            Instant::now(),
        );
        video.write(FrameKind::Key, 20_000, fresh_payload.clone())?;
        assert_eq!(
            sender.budget(Instant::now()).outstanding_bytes,
            400_000 + fresh_payload.len() as u64
        );
        assert!(!sender.budget(Instant::now()).ready);
        permits.add_permits(110);
        let mut fresh = track
            .next_group()
            .await?
            .context("missing recovery group")?;
        assert_eq!(fresh.sequence, group.sequence + 1);
        let key = fresh
            .read_frame()
            .await?
            .context("missing recovery keyframe")?;
        assert_eq!(key.timestamp, moq_net::Timestamp::from_micros(20_000)?);
        assert_eq!(Header::unpack(key.payload)?.1, fresh_payload);
        sender.feedback(
            1,
            feedback(Some(FrameId {
                epoch: 20_000,
                timestamp_us: 20_000,
            })),
            Instant::now(),
        );
        assert_eq!(sender.budget(Instant::now()).outstanding_bytes, 0);
        assert!(sender.budget(Instant::now()).refine);
        bridge.abort();
        Ok::<(), anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn actively_consumed_group_outlives_ten_seconds_of_cache_age() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(18), async {
        let source = media::origin();
        let viewer = media::origin();
        let broadcast = source.create_broadcast("app")?;
        broadcast.announce(Default::default())?;
        let mut video = media::Video::new(&broadcast)?;
        let (server, client) = tokio::io::duplex(4096);
        let (_sending, _receiving) = tokio::try_join!(
            media::local_server(server, &source),
            media::local_client(client, viewer.clone()),
        )?;
        video.track.used().await?;
        let remote = viewer.consume().request_broadcast("app").await?;
        let mut track = remote.track("video")?.subscribe(None).await?.ordered();
        video.write(FrameKind::Key, 1, Bytes::from_static(b"key"))?;
        let mut group = track.next_group().await?.context("missing initial group")?;
        let sequence = group.sequence;
        assert_eq!(
            group
                .read_frame()
                .await?
                .context("missing keyframe")?
                .payload
                .slice(Header::SIZE..),
            Bytes::from_static(b"key")
        );

        // MoQ's production driver advances a std::time::Instant clock; paused
        // Tokio time alone does not test its cache age. Keep reading the same
        // open group in real time, well inside its 750ms active-consumer
        // budget.
        let started = Instant::now();
        for index in 1u64..=44 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let payload = Bytes::copy_from_slice(&index.to_be_bytes());
            video.write(FrameKind::Checkpoint, index * 250_000, payload.clone())?;
            let frame = group.read_frame().await?.context("active group expired")?;
            assert_eq!(group.sequence, sequence);
            assert_eq!(
                frame.timestamp,
                moq_net::Timestamp::from_micros(index * 250_000)?
            );
            assert_eq!(Header::unpack(frame.payload)?.1, payload);
        }
        assert!(started.elapsed() > Duration::from_secs(10));
        // Rotation, not wall-clock group age, introduces the next keyframe.
        video.write(FrameKind::Key, 12_000_000, Bytes::from_static(b"next key"))?;
        assert!(
            group.read_frame().await.is_err(),
            "new root cancels old chain"
        );
        let mut next = track
            .next_group()
            .await?
            .context("missing replacement group")?;
        assert_eq!(next.sequence, sequence + 1);
        assert_eq!(
            next.read_frame()
                .await?
                .context("missing replacement keyframe")?
                .payload
                .slice(Header::SIZE..),
            Bytes::from_static(b"next key")
        );
        Ok::<(), anyhow::Error>(())
    })
    .await?
}
