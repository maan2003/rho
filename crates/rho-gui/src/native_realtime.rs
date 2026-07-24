//! GUI integration for the provider-independent `rho-realtime` session.

use anyhow::bail;
use futures::StreamExt as _;
use rho_realtime::{RealtimeEvent, RealtimeSession, SdpAnswer};
use rho_ui_proto::AgentId;
use rho_ui_proto::realtime::{RealtimeClientFrame, RealtimeRequestId, RealtimeServerFrame};

use crate::connection::{ChannelDialer, dial_realtime};

pub(crate) async fn run(dialer: ChannelDialer, delegate_agent: AgentId) -> anyhow::Result<()> {
    tracing::info!(?delegate_agent, "starting native realtime client session");
    let (channel_tx, channel_rx) = tokio::sync::oneshot::channel();
    let mut session = RealtimeSession::connect(move |offer_sdp| async move {
        let channel = dial_realtime(dialer, offer_sdp.into_string()).await?;
        let answer_sdp = channel.answer_sdp.clone();
        channel_tx
            .send(channel)
            .map_err(|_| anyhow::anyhow!("realtime session stopped during signaling"))?;
        SdpAnswer::try_from(answer_sdp)
    })
    .await?;
    let mut channel = channel_rx.await?;
    tracing::info!("native realtime client session established");
    let mut next_request_id = 1_u64;

    while let Some(event) = session.next_event().await {
        match event {
            RealtimeEvent::DelegateRequest(request) => {
                tracing::info!("received realtime delegation request");
                let request_id = RealtimeRequestId(next_request_id);
                next_request_id = next_request_id.wrapping_add(1).max(1);
                channel
                    .requests
                    .unbounded_send(RealtimeClientFrame::Delegate {
                        request_id,
                        agent_id: delegate_agent,
                        text: request.text,
                    })?;
                match channel.replies.next().await {
                    Some(RealtimeServerFrame::Delegated {
                        request_id: completed_id,
                        text,
                    }) if completed_id == request_id => {
                        tracing::info!(?request_id, "realtime delegation completed");
                        session.resolve_delegate(request.id, &text).await?;
                    }
                    Some(RealtimeServerFrame::Delegated { .. }) => {
                        bail!("realtime daemon returned a mismatched delegation id")
                    }
                    Some(RealtimeServerFrame::Error(error)) => {
                        bail!("realtime delegation failed: {error}")
                    }
                    Some(RealtimeServerFrame::Closed) | None => break,
                }
            }
            RealtimeEvent::Error(error) => bail!("realtime provider error: {error}"),
            RealtimeEvent::Closed => {
                tracing::info!("native realtime peer closed");
                break;
            }
        }
    }
    tracing::info!("native realtime client session ended");
    Ok(())
}
