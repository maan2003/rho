//! Host-owned OpenAI realtime signaling and sideband.
//!
//! The GUI owns only WebRTC media. Provider control events and commands stay
//! on the agent host's authenticated sideband connection. The session belongs
//! to one agent: the live model starts with that agent's thread, hands each
//! request to it as a user message, and hears back what the agent sends the
//! user (see [`thread`]).

mod thread;

use std::sync::Arc;

use anyhow::Context as _;
use rho_agent::db::AgentReadTxnExt as _;
use rho_agent::log::AgentEventPos;
use rho_agent_types::{AgentId, ContentPart};
use rho_agents_client::protocol::{AgentCommand, UserMessage};
use rho_inference::ResolvedOAuth;
use rho_openai_realtime::{
    ContextChannel, ProviderEvent, Sideband, SidebandConfig, TranscriptState, call_id_from_location,
};
use rho_rpc::protocol::{read_frame, write_frame};
use rho_rtc::protocol::{Opened, RealtimeClientFrame, RealtimeServerFrame};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::Services;

/// The live model's instructions, adapted from the ChatGPT app's voice
/// prompt. The thread's startup context is appended.
const INSTRUCTIONS: &str = include_str!("realtime/voice.md");
/// How far back in the agent's log the startup context looks for turns.
const CONTEXT_SCAN_BACK: u64 = 2_000;
const MAX_SDP_BYTES: usize = 256 * 1024;
const SIGNALING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CALL_URL: &str =
    "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";

pub(crate) async fn serve<R, W>(
    services: Arc<Services>,
    mut reader: R,
    mut writer: W,
    offer_sdp: String,
    agent_id: AgentId,
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
                &Opened::Refused {
                    reason: "another GUI already owns the voice session".to_owned(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let opened = async {
        let instructions = instructions(&services, agent_id)?;
        let auth = services.inference.auth().await?;
        let credential = tokio::task::spawn_blocking(move || auth.resolve_oauth())
            .await
            .context("join realtime OAuth resolver")??;
        validate_sdp(&offer_sdp, "offer")?;
        create_call(credential, offer_sdp, instructions).await
    }
    .await;
    let opened = match opened {
        Ok(opened) => opened,
        Err(error) => {
            write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: format!("{error:#}"),
                },
            )
            .await?;
            return Ok(());
        }
    };

    write_frame(
        &mut writer,
        &Opened::Answer {
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

    // Replies are what the agent writes from here on.
    let mut feed = rho_agent::journal::feed(services.pool.db());
    let mut cursor = next_pos(&services, agent_id);
    // The request the agent's messages answer: the latest one, until the
    // agent completes it.
    let mut active: Option<String> = None;
    let mut transcript = TranscriptState::default();
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
            frame = read_frame::<_, RealtimeClientFrame>(&mut reader) => {
                match frame {
                    Ok(RealtimeClientFrame::Close) | Err(_) => break,
                }
            }
            appended = feed.recv() => {
                match appended {
                    Ok(appended) if appended.agent_id != agent_id => continue,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                let head = next_pos(&services, agent_id);
                let events = {
                    let read = services.pool.db().read();
                    (cursor..head)
                        .filter_map(|pos| read.agent_event(agent_id, AgentEventPos { pos }))
                        .collect::<Vec<_>>()
                };
                cursor = head;
                for event in &events {
                    let (Some(id), Some((channel, text, complete))) =
                        (active.as_deref(), thread::reply(event))
                    else {
                        continue;
                    };
                    sideband.append_delegation(id, channel, &text).await?;
                    if complete {
                        active = None;
                    }
                }
            }
            event = sideband.next_event() => {
                match event {
                    Ok(Some(ProviderEvent::DelegationCreated { id, text })) => {
                        let message = thread::delegation(&text, &transcript.take_snapshot());
                        let delivered = crate::agents::handle_agent_command(
                            &services,
                            AgentCommand::Send {
                                agent_id,
                                messages: vec![UserMessage {
                                    id: rho_agent::entry::MessageId::new().0,
                                    content: vec![ContentPart::Text { text: message }],
                                }],
                            },
                        )
                        .await;
                        match delivered {
                            Ok(()) => active = Some(id),
                            Err(error) => {
                                sideband
                                    .append_delegation(
                                        &id,
                                        ContextChannel::Speakable,
                                        &format!("[COMPLETE] The request did not reach the agent: {error:#}"),
                                    )
                                    .await?;
                            }
                        }
                    }
                    Ok(Some(event @ (ProviderEvent::TranscriptDelta { .. } | ProviderEvent::TranscriptDone { .. }))) => {
                        transcript.apply(&event);
                    }
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

/// The live model's instructions with the agent's thread appended.
fn instructions(services: &Services, agent_id: AgentId) -> anyhow::Result<String> {
    let read = services.pool.db().read();
    let head = read
        .try_get_agent(agent_id)
        .context("voice is attached to an agent this host does not have")?;
    let end = head.next.pos;
    let said = (end.saturating_sub(CONTEXT_SCAN_BACK)..end)
        .filter_map(|pos| read.agent_event(agent_id, AgentEventPos { pos }))
        .filter_map(|event| thread::Said::of(&event));
    Ok(match thread::startup_context(said) {
        Some(context) => format!("{INSTRUCTIONS}\n{context}"),
        None => INSTRUCTIONS.to_owned(),
    })
}

/// Where the agent's next log row will land.
fn next_pos(services: &Services, agent_id: AgentId) -> u64 {
    services
        .pool
        .db()
        .read()
        .try_get_agent(agent_id)
        .map_or(0, |head| head.next.pos)
}

struct OpenedCall {
    answer_sdp: String,
    sideband: SidebandConfig,
}

async fn create_call(
    credential: ResolvedOAuth,
    offer_sdp: String,
    instructions: String,
) -> anyhow::Result<OpenedCall> {
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
            model: RealtimeModel::GptLive11Codex,
            instructions,
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
    #[serde(rename = "gpt-live-1.1-codex")]
    GptLive11Codex,
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
            serde_json::to_value(RealtimeModel::GptLive11Codex).unwrap(),
            "gpt-live-1.1-codex"
        );
    }
}
