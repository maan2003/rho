//! GUI integration for the provider-independent `rho-rtc` media session.

use std::future::Future;

use futures::StreamExt as _;
use rho_rtc::{RtcEvent, RtcSession, SdpAnswer};
use rho_ui_proto::realtime::{RealtimeClientFrame, RealtimeServerFrame};

#[cfg(feature = "native")]
use crate::connection::{ChannelDialer, dial_realtime};

pub(crate) struct RealtimeChannel {
    pub(crate) answer_sdp: String,
    pub(crate) requests: futures::channel::mpsc::Sender<RealtimeClientFrame>,
    pub(crate) replies: futures::channel::mpsc::Receiver<anyhow::Result<RealtimeServerFrame>>,
    #[cfg(feature = "native")]
    pub(crate) _transport: rho_rpc::ChannelTask,
}

#[cfg(feature = "native")]
pub(crate) async fn run_native(
    dialer: ChannelDialer,
    stop: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    run(move |offer_sdp| dial_realtime(dialer, offer_sdp), stop).await
}

pub(crate) async fn run<D, F>(
    dial: D,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()>
where
    D: FnOnce(String) -> F,
    F: Future<Output = anyhow::Result<RealtimeChannel>>,
{
    tracing::info!("starting Iris realtime session");
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
    session.start_audio()?;
    tracing::info!("realtime client session established");

    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                biased;
                _ = &mut stop => break,
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
