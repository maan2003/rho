//! Daemon-owned OpenAI realtime signaling and sideband.
//!
//! The GUI owns only WebRTC media. Provider control events and commands stay
//! on the daemon's authenticated sideband connection. No agent stands behind
//! the voice session yet: a delegation is answered with that fact so the
//! model never promises work.

use std::sync::Arc;

use anyhow::Context as _;
use rho_inference::ResolvedOAuth;
use rho_openai_realtime::{
    ContextChannel, ProviderEvent, Sideband, SidebandConfig, call_id_from_location,
};
use rho_ui_proto::realtime::{RealtimeClientFrame, RealtimeServerFrame};
use rho_ui_proto::{ServerMessage, read_frame, write_frame};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::Services;

const NO_BACKEND_REPLY: &str =
    "No agent is attached to the voice session in this build, so that cannot be done by voice yet.";
const MAX_SDP_BYTES: usize = 256 * 1024;
const SIGNALING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CALL_URL: &str =
    "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";

pub(crate) async fn serve<R, W>(
    services: Arc<Services>,
    mut reader: R,
    mut writer: W,
    offer_sdp: String,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let _lease = match services.voice_lease.clone().try_lock_owned() {
        Ok(lease) => lease,
        Err(_) => {
            write_frame(
                &mut writer,
                &ServerMessage::RealtimeRefused {
                    reason: "another GUI already owns the voice session".to_owned(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let opened = async {
        let auth = services.inference.auth().await?;
        let credential = tokio::task::spawn_blocking(move || auth.resolve_oauth())
            .await
            .context("join realtime OAuth resolver")??;
        validate_sdp(&offer_sdp, "offer")?;
        create_call(credential, offer_sdp).await
    }
    .await;
    let opened = match opened {
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

    write_frame(
        &mut writer,
        &ServerMessage::RealtimeOpened {
            answer_sdp: opened.answer_sdp,
        },
    )
    .await?;
    let sideband_connect = Sideband::connect(&opened.sideband);
    tokio::pin!(sideband_connect);
    let mut sideband = tokio::select! {
            result = &mut sideband_connect => match result {
                Ok(sideband) => sideband,
                Err(error) => {
                    write_frame(
                        &mut writer,
                        &RealtimeServerFrame::Error(format!("{error:#}")),
                    ).await?;
                    return Ok(());
                }
            },
            frame = read_frame::<_, RealtimeClientFrame>(&mut reader) => match frame {
                Ok(RealtimeClientFrame::Close) | Err(_) => {
                    let _ = write_frame(&mut writer, &RealtimeServerFrame::Closed).await;
                    return Ok(());
                }
            }
    };

    write_frame(&mut writer, &RealtimeServerFrame::SidebandReady).await?;

    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
            frame = read_frame::<_, RealtimeClientFrame>(&mut reader) => {
                match frame {
                    Ok(RealtimeClientFrame::Close) | Err(_) => break,
                }
            }
            event = sideband.next_event() => {
                match event {
                    Ok(Some(ProviderEvent::DelegationCreated { id, .. })) => {
                        sideband
                            .append_delegation(&id, ContextChannel::Speakable, NO_BACKEND_REPLY)
                            .await?;
                    }
                    Ok(Some(ProviderEvent::TranscriptDelta { .. } | ProviderEvent::TranscriptDone { .. })) => {}
                    Ok(Some(ProviderEvent::Error(error))) => {
                        write_frame(&mut writer, &RealtimeServerFrame::Error(error)).await?;
                        break;
                    }
                    Ok(Some(ProviderEvent::Other)) => {}
                    Ok(None) => {
                        write_frame(
                            &mut writer,
                            &RealtimeServerFrame::Error(
                                "OpenAI realtime sideband closed unexpectedly".to_owned(),
                            ),
                        ).await?;
                        break;
                    }
                    Err(error) => {
                        write_frame(
                            &mut writer,
                            &RealtimeServerFrame::Error(format!("{error:#}")),
                        ).await?;
                        break;
                    }
                }
            }
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        let _ = write_frame(
            &mut writer,
            &RealtimeServerFrame::Error(format!("{error:#}")),
        )
        .await;
    }
    let _ = write_frame(&mut writer, &RealtimeServerFrame::Closed).await;
    result
}

struct OpenedCall {
    answer_sdp: String,
    sideband: SidebandConfig,
}

async fn create_call(credential: ResolvedOAuth, offer_sdp: String) -> anyhow::Result<OpenedCall> {
    let account_id = credential
        .account_id
        .context("realtime requires a ChatGPT account id")?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let thread_id = uuid::Uuid::new_v4().to_string();
    let installation_id = uuid::Uuid::new_v4().to_string();
    let originator = "rho_gui".to_owned();
    let user_agent = "rho-gui".to_owned();
    let body = CreateCallRequest {
        sdp: offer_sdp,
        session: CreateCallSession {
            model: RealtimeModel::GptLive1Codex,
            instructions: "You are Rho's voice assistant. Be concise, natural, warm, and \
                 interruption-friendly. No agent is attached to this session, so when a \
                 request needs work done in the user's repositories or services, say plainly \
                 that voice cannot do that yet."
                .to_owned(),
            audio: SessionAudio {
                output: AudioOutput { voice: Voice::Cove },
            },
            delegation: SessionDelegation {
                delegation_type: SessionDelegationType::Client,
            },
        },
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(SIGNALING_TIMEOUT)
        .build()
        .context("build realtime signaling client")?;
    let mut response = client
        .post(CALL_URL)
        .bearer_auth(&credential.bearer_token)
        .header("chatgpt-account-id", &account_id)
        .header("openai-alpha", "quicksilver=v2")
        .header("x-session-id", &session_id)
        .header("session-id", &session_id)
        .header("thread-id", &thread_id)
        .header("x-codex-installation-id", &installation_id)
        .header("originator", &originator)
        .header("user-agent", &user_agent)
        .json(&body)
        .send()
        .await
        .context("create realtime WebRTC call")?;
    let status = response.status();
    let call_id = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(call_id_from_location);
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read realtime call response")?
    {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_SDP_BYTES,
            "realtime call response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let detail = serde_json::from_slice::<ApiErrorEnvelope>(&bytes)
            .ok()
            .map(|response| response.error.message)
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(500).collect());
        anyhow::bail!("realtime call creation failed with {status}: {detail}");
    }
    let answer_sdp = String::from_utf8(bytes).context("decode realtime SDP answer")?;
    validate_sdp(&answer_sdp, "answer").context("provider returned an invalid SDP answer")?;
    let call_id = call_id.context("realtime call response omitted a valid call id")?;
    Ok(OpenedCall {
        answer_sdp,
        sideband: SidebandConfig {
            call_id,
            bearer_token: credential.bearer_token,
            account_id,
            session_id,
            thread_id,
            installation_id,
            originator,
            user_agent,
        },
    })
}

fn validate_sdp(value: &str, kind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(value.len() <= MAX_SDP_BYTES, "SDP {kind} is too large");
    anyhow::ensure!(value.starts_with("v=0"), "invalid SDP {kind}");
    Ok(())
}

#[derive(Serialize)]
struct CreateCallRequest {
    sdp: String,
    session: CreateCallSession,
}

#[derive(Serialize)]
struct CreateCallSession {
    model: RealtimeModel,
    instructions: String,
    audio: SessionAudio,
    delegation: SessionDelegation,
}

#[derive(Serialize)]
enum RealtimeModel {
    #[serde(rename = "gpt-live-1-codex")]
    GptLive1Codex,
}

#[derive(Serialize)]
struct SessionAudio {
    output: AudioOutput,
}

#[derive(Serialize)]
struct AudioOutput {
    voice: Voice,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Voice {
    Cove,
}

#[derive(Serialize)]
struct SessionDelegation {
    #[serde(rename = "type")]
    delegation_type: SessionDelegationType,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionDelegationType {
    Client,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: ApiError,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_codex_live_model() {
        assert_eq!(
            serde_json::to_value(RealtimeModel::GptLive1Codex).unwrap(),
            "gpt-live-1-codex"
        );
    }
}
