//! The voice session's media: a `rho-rtc` session whose offer and
//! answer, and whose sideband, go over a realtime stream to the host.

use std::future::Future;

use futures::StreamExt as _;
use rho_rpc::protocol::{read_frame, write_open};
use rho_rtc::protocol::{RealtimeClientFrame, RealtimeServerFrame};
use rho_rtc::{RtcEvent, RtcSession, SdpAnswer};

struct RealtimeChannel {
    answer_sdp: String,
    requests: futures::channel::mpsc::Sender<RealtimeClientFrame>,
    replies: futures::channel::mpsc::Receiver<anyhow::Result<RealtimeServerFrame>>,
    _transport: rho_rpc::ChannelTask,
}

/// Runs voice against the host until `stop` fires or the session fails.
pub(crate) fn start(
    link: &rho_agent_hosts::Link,
    stop: tokio::sync::oneshot::Receiver<()>,
    input_muted: tokio::sync::watch::Receiver<bool>,
) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
    link.run(|dialer| run(move |offer_sdp| dial(dialer, offer_sdp), stop, input_muted))
}

async fn dial(
    dialer: rho_agent_hosts::Dialer,
    offer_sdp: String,
) -> anyhow::Result<RealtimeChannel> {
    // Interactive streams outrank the sessions (priority 1 and below).
    let mut stream = dialer.open(Some(50)).await?;
    write_open(&mut stream, &rho_rtc::protocol::Open { offer_sdp }).await?;
    let answer_sdp = match read_frame(&mut stream).await? {
        rho_rtc::protocol::Opened::Answer { answer_sdp } => answer_sdp,
        rho_rtc::protocol::Opened::Refused { reason } => anyhow::bail!("{reason}"),
    };
    let channel = stream.into_channel(rho_rpc::ChannelConfig {
        tx_limit: rho_rpc::protocol::MAX_FRAME_LEN,
        rx_limit: rho_rpc::protocol::MAX_FRAME_LEN,
        tx_capacity: 32,
        rx_capacity: 32,
    });
    let (requests, replies, transport) = channel.into_parts();
    Ok(RealtimeChannel {
        answer_sdp,
        requests,
        replies,
        _transport: transport,
    })
}

async fn run<D, F>(
    dial: D,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    mut input_muted: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()>
where
    D: FnOnce(String) -> F,
    F: Future<Output = anyhow::Result<RealtimeChannel>>,
{
    tracing::info!("starting realtime voice session");
    let (channel_tx, channel_rx) = tokio::sync::oneshot::channel();
    let connecting = RtcSession::connect(move |offer_sdp| async move {
        let channel = dial(offer_sdp.into_string()).await?;
        let answer_sdp = channel.answer_sdp.clone();
        channel_tx
            .send(channel)
            .map_err(|_| anyhow::anyhow!("realtime session stopped during signaling"))?;
        SdpAnswer::try_from(answer_sdp)
    });
    tokio::pin!(connecting);
    let mut session = tokio::select! {
        biased;
        _ = &mut stop => return Ok(()),
        result = &mut connecting => result?,
    };
    let mut channel = channel_rx.await?;
    tokio::select! {
            biased;
            _ = &mut stop => {
                drop(session);
                let _ = channel.requests.try_send(RealtimeClientFrame::Close);
                return Ok(());
            }
            event = session.next_event() => match event {
                Some(RtcEvent::Error(error)) => anyhow::bail!("realtime media failed: {error}"),
                Some(RtcEvent::Closed) | None => anyhow::bail!("realtime peer closed before sideband became ready"),
            },
            reply = channel.replies.next() => match reply.transpose()? {
                Some(RealtimeServerFrame::SidebandReady) => {}
                Some(RealtimeServerFrame::Error(error)) => anyhow::bail!("realtime sideband failed: {error}"),
                Some(RealtimeServerFrame::Closed) | None => anyhow::bail!("realtime sideband closed before becoming ready"),
            }
    }
    session.set_input_muted(*input_muted.borrow())?;
    session.start_audio()?;
    tracing::info!("realtime client session established");

    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                biased;
                _ = &mut stop => break,
                changed = input_muted.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    session.set_input_muted(*input_muted.borrow())?;
                }
                event = session.next_event() => match event {
                    Some(RtcEvent::Error(error)) => anyhow::bail!("realtime media failed: {error}"),
                    Some(RtcEvent::Closed) | None => anyhow::bail!("realtime peer closed unexpectedly"),
                },
                reply = channel.replies.next() => match reply.transpose()? {
                    Some(RealtimeServerFrame::SidebandReady) => {}
                    Some(RealtimeServerFrame::Error(error)) => {
                        anyhow::bail!("realtime sideband failed: {error}")
                    }
                    Some(RealtimeServerFrame::Closed) | None => break,
                }
            }
        }
        Ok(())
    }
    .await;
    drop(session);
    let _ = channel.requests.try_send(RealtimeClientFrame::Close);
    result?;
    tracing::info!("realtime client session ended");
    Ok(())
}
