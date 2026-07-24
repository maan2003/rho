//! Signaling and transport bridge for GUI-owned realtime WebRTC peers.
//!
//! Provider event semantics remain in `rho-realtime` and the GUI. This module
//! resolves OAuth, relays signaling, and executes typed generic agent work on
//! the dedicated UI stream.

use std::sync::Arc;

use anyhow::Context as _;
use rho_ui_proto::realtime::{RealtimeClientFrame, RealtimeServerFrame};
use rho_ui_proto::{ServerMessage, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::AgentRegistry;

pub(crate) async fn serve<R, W>(
    agents: Arc<AgentRegistry>,
    mut reader: R,
    mut writer: W,
    offer_sdp: String,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let opened = async {
        let auth = agents.auth.clone();
        let credential = tokio::task::spawn_blocking(move || auth.resolve_oauth())
            .await
            .context("join realtime OAuth resolver")??;
        let offer = rho_realtime::SdpOffer::try_from(offer_sdp)?;
        rho_realtime::create_call(credential, offer)
            .await
            .map(rho_realtime::SdpAnswer::into_string)
    }
    .await;
    let answer_sdp = match opened {
        Ok(opened) => opened,
        Err(error) => {
            write_frame(
                &mut writer,
                &ServerMessage::RealtimeRefused {
                    reason: format!("{error:#}"),
                },
            )
            .await?;
            return Ok(());
        }
    };

    write_frame(&mut writer, &ServerMessage::RealtimeOpened { answer_sdp }).await?;
    let (result_tx, mut result_rx) = mpsc::channel(1);
    let mut delegate_active = false;

    loop {
        tokio::select! {
            frame = read_frame::<_, RealtimeClientFrame>(&mut reader) => {
                match frame {
                    Ok(RealtimeClientFrame::Delegate { request_id, agent_id, text }) => {
                        if delegate_active {
                            write_frame(&mut writer, &RealtimeServerFrame::Error(
                                "a realtime delegation is already active".to_owned(),
                            )).await?;
                            continue;
                        }
                        let mut backend = agents.pool.delegation_backend(agent_id).await?;
                        backend.submit(text);
                        let result_tx = result_tx.clone();
                        delegate_active = true;
                        tokio::spawn(async move {
                            let result = backend.next_final().await.map_err(|error| format!("{error:#}"));
                            let _ = result_tx.send((request_id, result)).await;
                        });
                    }
                    Ok(RealtimeClientFrame::Close) | Err(_) => break,
                }
            }
            result = result_rx.recv() => {
                let Some((request_id, result)) = result else { break; };
                delegate_active = false;
                match result {
                    Ok(text) => write_frame(
                        &mut writer,
                        &RealtimeServerFrame::Delegated { request_id, text },
                    ).await?,
                    Err(error) => write_frame(
                        &mut writer,
                        &RealtimeServerFrame::Error(error),
                    ).await?,
                }
            }
        }
    }
    let _ = write_frame(&mut writer, &RealtimeServerFrame::Closed).await;
    Ok(())
}
