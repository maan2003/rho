//! GUI integration for the provider-independent `rho-realtime` session.

use std::sync::{Arc, Mutex};

use anyhow::bail;
use futures::{SinkExt as _, StreamExt as _};
use rho_realtime::{
    DelegateResponseChannel, RealtimeEvent, RealtimeSession, SdpAnswer, TranscriptRole,
};
use rho_ui_proto::AgentId;
use rho_ui_proto::realtime::{
    RealtimeClientFrame, RealtimeRequestId, RealtimeResponsePhase, RealtimeServerFrame,
};

use crate::connection::{ChannelDialer, dial_realtime};

pub(crate) async fn run(
    dialer: ChannelDialer,
    context_agent: Arc<Mutex<Option<AgentId>>>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    tracing::info!("starting native Iris realtime session");
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
    let mut requests = Vec::new();
    const MAX_OUTSTANDING_DELEGATIONS: usize = 16;

    let result: anyhow::Result<()> = async {
    loop {
        tokio::select! {
            _ = &mut stop => break,
            event = session.next_event() => match event {
            Some(RealtimeEvent::DelegateRequest(request)) => {
                if requests.len() >= MAX_OUTSTANDING_DELEGATIONS {
                    bail!("too many outstanding realtime delegations");
                }
                tracing::info!("received realtime delegation request");
                let request_id = RealtimeRequestId(next_request_id);
                next_request_id = next_request_id.wrapping_add(1).max(1);
                let context_agent = *context_agent.lock().expect("Iris context mutex poisoned");
                let transcript_delta = request.transcript_delta;
                channel
                    .requests
                    .send(RealtimeClientFrame::Delegate {
                        request_id,
                        context_agent,
                        text: request.text,
                        transcript_delta,
                    }).await?;
                requests.push((request_id, request.id));
            }
            Some(RealtimeEvent::TranscriptDelta { role, delta }) => {
                tracing::debug!(role = transcript_role(role), bytes = delta.len(), "realtime transcript delta");
            }
            Some(RealtimeEvent::TranscriptDone { role, text }) => {
                tracing::debug!(role = transcript_role(role), bytes = text.len(), "realtime transcript complete");
            }
            Some(RealtimeEvent::Error(error)) => bail!("realtime provider error: {error}"),
            Some(RealtimeEvent::Closed) | None => {
                tracing::info!("native realtime peer closed");
                break;
            }
        },
            reply = channel.replies.next() => match reply.transpose()? {
                Some(RealtimeServerFrame::DelegatedItem { request_id, phase, text }) => {
                    let provider_id = requests
                        .iter()
                        .find(|(id, _)| *id == request_id)
                        .map(|(_, id)| id.clone())
                        .ok_or_else(|| anyhow::anyhow!("realtime daemon returned an unknown delegation id"))?;
                    let channel = match phase {
                        RealtimeResponsePhase::Commentary => DelegateResponseChannel::Commentary,
                        RealtimeResponsePhase::Speakable => DelegateResponseChannel::Speakable,
                    };
                    session.resolve_delegate_chunk(provider_id, channel, &text).await?;
                }
                Some(RealtimeServerFrame::Delegated { request_id, text }) => {
                    let index = requests
                        .iter()
                        .position(|(id, _)| *id == request_id)
                        .ok_or_else(|| anyhow::anyhow!("realtime daemon returned an unknown delegation id"))?;
                    let (_, provider_id) = requests.remove(index);
                    if !text.is_empty() {
                        session.resolve_delegate(provider_id, &text).await?;
                    }
                    tracing::info!(?request_id, "realtime delegation completed");
                }
                Some(RealtimeServerFrame::Steered { request_id }) => {
                    let index = requests
                        .iter()
                        .position(|(id, _)| *id == request_id)
                        .ok_or_else(|| anyhow::anyhow!("realtime daemon returned an unknown delegation id"))?;
                    let (_, provider_id) = requests.remove(index);
                    session.resolve_delegate(
                        provider_id,
                        "This was sent to steer the work already in progress.",
                    ).await?;
                }
                Some(RealtimeServerFrame::Error(error)) => {
                    bail!("realtime delegation failed: {error}")
                }
                Some(RealtimeServerFrame::Closed) | None => break,
            }
        }
    }
    Ok(())
    }.await;
    if let Some(text) = session.take_transcript_tail() {
        let context_agent = *context_agent.lock().expect("Iris context mutex poisoned");
        let _ = channel
            .requests
            .send(RealtimeClientFrame::TranscriptTail {
                context_agent,
                text,
            })
            .await;
    }
    let _ = channel.requests.send(RealtimeClientFrame::Close).await;
    result?;
    tracing::info!("native realtime client session ended");
    Ok(())
}

fn transcript_role(role: TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User => "user",
        TranscriptRole::Assistant => "assistant",
    }
}
