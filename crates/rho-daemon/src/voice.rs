//! Daemon-side voice controller: one realtime voice session driving the
//! agent registry through the `rho-voice` tool vocabulary.
//!
//! The session is a peer of a UI connection, not of the agents: audio and
//! transcripts relay to the owning client over ui-proto, while tool calls
//! execute directly against [`AgentRegistry`] — agent text reaches the
//! provider as tool output without ever crossing the UI wire.
//!
//! Bounds: one session per daemon (`voice_active`), auto-stop after
//! [`IDLE_STOP`] without user speech (sessions are billed per minute), and
//! the whole task ends when the owning connection drops its handle.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rho_agent::MessageDelivery;
use rho_agent::db::{AgentDisposition, AgentId, AgentMode, DeepConfig, DeepEffort};
use rho_core::{InferenceResponseItem, MessagePhase, text_content};
use rho_ui_proto::{
    ClientMessage, ServerMessage, StartMode, TopicTarget, UiAgentSummary, VOICE_SAMPLE_RATE,
    VoiceRole, VoiceState, VoiceUiAction,
};
use rho_voice::session::{VoiceConfig, VoiceSession};
use rho_voice::tools::{VoiceToolCall, parse_tool_call, tool_definitions};
use rho_voice::wire::{
    AudioConfig, ClientEvent, ConversationItem, RealtimeItem, ServerEvent, SessionConfig,
    TurnDetection,
};
use tokio::sync::mpsc;

use crate::{AgentRegistry, subscribe_agent};

/// Stop the session when the user hasn't spoken for this long.
const IDLE_STOP: std::time::Duration = std::time::Duration::from_secs(5 * 60);

const INSTRUCTIONS: &str = "You are rho's voice interface to the user's coding agents. \
     The user is an expert user of rho and already understands what you can do. \
     Keep spoken responses brief, direct, and efficient with the user's time. \
     Avoid fluff, capability explanations, and unsolicited 'if you want, I can...' \
     follow-up suggestions. After completing a request, report the concrete result \
     briefly and stop. Your job is speech-to-action, not independent technical \
     judgment. Do not solve coding, product, or design questions from your own \
     opinion. When the user asks for work, judgment, investigation, or \
     implementation, pass the request and relevant conversation context to the \
     appropriate coding agent and rely on that agent's response. Treat the coding \
     agents as the thinkers; you are the interface. Use the prior conversation to \
     resolve pronouns and short commands. Do not reinterpret the user's request \
     based on your own speculation. If the target or action is genuinely ambiguous \
     and a wrong action would matter, ask one brief clarification. Use tools for \
     anything about agents, agent state, messages, navigation, or UI actions. Do \
     not invent agent state. When the user says 'the agent' or gives no name, omit \
     the agent argument to target the focused agent. Never read code, paths, logs, \
     or long output verbatim; summarize only what is needed.";

pub(crate) struct VoiceHandle {
    inbound: mpsc::UnboundedSender<VoiceInbound>,
}

impl VoiceHandle {
    pub(crate) fn audio(&self, pcm: Vec<u8>) {
        let _ = self.inbound.send(VoiceInbound::Audio(pcm));
    }

    pub(crate) fn focus(&self, agent_id: Option<AgentId>) {
        let _ = self.inbound.send(VoiceInbound::Focus(agent_id));
    }

    pub(crate) fn stop(&self) {
        let _ = self.inbound.send(VoiceInbound::Stop);
    }
}

enum VoiceInbound {
    Audio(Vec<u8>),
    Focus(Option<AgentId>),
    Stop,
}

/// Clears the daemon-wide active flag when the session task ends, however
/// it ends.
struct ActiveGuard(Arc<AtomicBool>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub(crate) fn start(
    registry: Arc<AgentRegistry>,
    outgoing: mpsc::UnboundedSender<ServerMessage>,
) -> anyhow::Result<VoiceHandle> {
    if registry.voice_active.swap(true, Ordering::SeqCst) {
        anyhow::bail!("a voice session is already active");
    }
    let guard = ActiveGuard(Arc::clone(&registry.voice_active));
    let config = match VoiceConfig::grok_cli() {
        Ok(config) => config,
        Err(error) => {
            drop(guard);
            anyhow::bail!("voice needs Grok OAuth credentials from `grok login`: {error}");
        }
    };
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _guard = guard;
        let reason = run_session(config, registry, &outgoing, inbound_rx).await;
        let _ = outgoing.send(ServerMessage::VoiceState {
            state: VoiceState::Stopped { reason },
        });
    });
    Ok(VoiceHandle {
        inbound: inbound_tx,
    })
}

/// Drives one session to completion; the returned string is the stop reason
/// shown to the user.
async fn run_session(
    config: VoiceConfig,
    registry: Arc<AgentRegistry>,
    outgoing: &mpsc::UnboundedSender<ServerMessage>,
    mut inbound: mpsc::UnboundedReceiver<VoiceInbound>,
) -> String {
    let _ = outgoing.send(ServerMessage::VoiceState {
        state: VoiceState::Starting,
    });
    let mut session = match VoiceSession::connect(&config).await {
        Ok(session) => session,
        Err(error) => return format!("connect failed: {error:#}"),
    };
    let mut tool_names = HashMap::<String, String>::new();
    let configure = ClientEvent::SessionUpdate {
        session: SessionConfig {
            voice: Some("eve".to_owned()),
            instructions: Some(INSTRUCTIONS.to_owned()),
            turn_detection: Some(Some(TurnDetection::ServerVad {
                threshold: None,
                silence_duration_ms: None,
                prefix_padding_ms: None,
            })),
            audio: Some(AudioConfig::pcm(VOICE_SAMPLE_RATE)),
            tools: tool_definitions(),
        },
    };
    if let Err(error) = session.send(&configure).await {
        return format!("session configuration failed: {error:#}");
    }
    let _ = outgoing.send(ServerMessage::VoiceState {
        state: VoiceState::Active,
    });

    let mut focus: Option<AgentId> = None;
    let mut last_speech = tokio::time::Instant::now();
    let mut idle_check = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tokio::select! {
            inbound = inbound.recv() => match inbound {
                None => return "client disconnected".to_owned(),
                Some(VoiceInbound::Stop) => return "stopped".to_owned(),
                Some(VoiceInbound::Focus(agent_id)) => focus = agent_id,
                Some(VoiceInbound::Audio(pcm)) => {
                    if let Err(error) = session.send_audio(&pcm).await {
                        return format!("audio send failed: {error:#}");
                    }
                }
            },
            _ = idle_check.tick() => {
                if last_speech.elapsed() >= IDLE_STOP {
                    return "stopped after 5 minutes without speech".to_owned();
                }
            }
            event = session.next_event(None) => {
                let event = match event {
                    Err(error) => return format!("session error: {error:#}"),
                    Ok(None) => return "provider closed the connection".to_owned(),
                    Ok(Some(event)) => event,
                };
                match event {
                    ServerEvent::OutputAudioDelta(event) => {
                        let _ = outgoing.send(ServerMessage::VoiceAudio { pcm: event.audio });
                    }
                    ServerEvent::InputAudioSpeechStarted(_) => {
                        last_speech = tokio::time::Instant::now();
                        let _ = outgoing.send(ServerMessage::VoiceFlushPlayback);
                        let _ = outgoing.send(ServerMessage::VoiceTranscript {
                            role: VoiceRole::User,
                            text: String::new(),
                        });
                    }
                    ServerEvent::InputAudioSpeechStopped(_) => {
                        last_speech = tokio::time::Instant::now();
                    }
                    ServerEvent::OutputAudioTranscriptDelta(event) => {
                        let _ = outgoing.send(ServerMessage::VoiceTranscript {
                            role: VoiceRole::Assistant,
                            text: event.delta,
                        });
                    }
                    ServerEvent::TextDelta(event) => {
                        let _ = outgoing.send(ServerMessage::VoiceTranscript {
                            role: VoiceRole::Assistant,
                            text: event.delta,
                        });
                    }
                    ServerEvent::ResponseOutputItemAdded(event)
                    | ServerEvent::ResponseOutputItemDone(event) => {
                        remember_tool_name(&mut tool_names, &event.item);
                    }
                    ServerEvent::FunctionCallArgumentsDone(event) => {
                        last_speech = tokio::time::Instant::now();
                        let call_id = event.call_id;
                        let name = event.name.or_else(|| tool_names.get(&call_id).cloned());
                        let arguments = event.arguments;
                        let output = match name {
                            None => "error: tool call arrived without a tool name".to_owned(),
                            Some(name) => match parse_tool_call(&name, &arguments) {
                                Err(error) => format!("error: {error:#}"),
                                Ok(call) => {
                                    execute_tool(&registry, outgoing, &focus, call).await
                                }
                            },
                        };
                        let send = async {
                            session
                                .send(&ClientEvent::ConversationItemCreate {
                                    item: ConversationItem::FunctionCallOutput { call_id, output },
                                })
                                .await?;
                            session.send(&ClientEvent::ResponseCreate { response: None }).await
                        };
                        if let Err(error) = send.await {
                            return format!("tool reply failed: {error:#}");
                        }
                    }
                    ServerEvent::Error(event) => {
                        return format!("provider error: {}", event.message());
                    }
                    ServerEvent::Ping(_)
                    | ServerEvent::SessionCreated(_)
                    | ServerEvent::SessionUpdated(_)
                    | ServerEvent::ConversationCreated(_)
                    | ServerEvent::ConversationItemAdded(_)
                    | ServerEvent::InputAudioBufferCommitted(_)
                    | ServerEvent::InputAudioBufferCleared(_)
                    | ServerEvent::ResponseCreated(_)
                    | ServerEvent::ResponseContentPartAdded(_)
                    | ServerEvent::ResponseContentPartDone(_)
                    | ServerEvent::OutputAudioDone(_)
                    | ServerEvent::OutputAudioTranscriptDone(_)
                    | ServerEvent::TextDone(_)
                    | ServerEvent::FunctionCallArgumentsDelta(_)
                    | ServerEvent::ResponseDone(_)
                    | ServerEvent::Unknown { .. } => {}
                }
            }
        }
    }
}

fn remember_tool_name(tool_names: &mut HashMap<String, String>, item: &RealtimeItem) {
    if item.item_type.as_deref() != Some("function_call") {
        return;
    }
    let Some(name) = item.name.clone() else {
        return;
    };
    if let Some(call_id) = item.call_id.as_deref().or(item.id.as_deref()) {
        tool_names.insert(call_id.to_owned(), name);
    }
}

/// Executes one tool call against the live registry. The returned string is
/// spoken from by the model, so it is prose, not data; errors are prose too
/// so the model can relay or correct them.
async fn execute_tool(
    registry: &Arc<AgentRegistry>,
    outgoing: &mpsc::UnboundedSender<ServerMessage>,
    focus: &Option<AgentId>,
    call: VoiceToolCall,
) -> String {
    match call {
        VoiceToolCall::ListAgents => list_agents(registry, focus).await,
        VoiceToolCall::AgentStatus { agent } => match resolve(registry, focus, &agent) {
            Err(error) => error,
            Ok((agent_id, label)) => match registry.load(agent_id).await {
                Err(error) => format!("could not load {label}: {error:#}"),
                Ok((_, agent, _)) => format!("{label} is {}", describe_state(&agent.state())),
            },
        },
        VoiceToolCall::ReadLastResponse { agent } => match resolve(registry, focus, &agent) {
            Err(error) => error,
            Ok((agent_id, label)) => match registry.load(agent_id).await {
                Err(error) => format!("could not load {label}: {error:#}"),
                Ok((_, agent, _)) => last_response(&label, &agent.state()),
            },
        },
        VoiceToolCall::SendMessage { agent, message } => match resolve(registry, focus, &agent) {
            Err(error) => error,
            Ok((agent_id, label)) => match registry.load(agent_id).await {
                Err(error) => format!("could not load {label}: {error:#}"),
                Ok((_, agent, _)) => {
                    agent.send_user_message(message, MessageDelivery::NextRequest);
                    format!("sent to {label}; it works asynchronously")
                }
            },
        },
        VoiceToolCall::NewAgent {
            workdir,
            topic,
            message,
        } => new_agent(registry, outgoing, workdir, topic, message).await,
        VoiceToolCall::CancelTurn { agent } => match resolve(registry, focus, &agent) {
            Err(error) => error,
            Ok((agent_id, label)) => match registry.get(agent_id).await {
                None => format!("{label} is not loaded, so nothing is running"),
                Some(agent) => {
                    agent.cancel();
                    let _ = outgoing.send(ServerMessage::TurnCancelled { agent_id });
                    format!("cancelled {label}'s turn")
                }
            },
        },
        VoiceToolCall::RenameAgent { agent, name } => match resolve(registry, focus, &agent) {
            Err(error) => error,
            Ok((agent_id, label)) => match registry.rename_agent(agent_id, name.clone()).await {
                Err(error) => format!("rename failed: {error:#}"),
                Ok(()) => {
                    let _ = outgoing.send(registry.ready_message().await);
                    format!("renamed {label} to {name}")
                }
            },
        },
        VoiceToolCall::MoveToTopic { agent, topic } => match resolve(registry, focus, &agent) {
            Err(error) => error,
            Ok((agent_id, label)) => {
                match registry
                    .move_agent(agent_id, TopicTarget::Named(topic.clone()))
                    .await
                {
                    Err(error) => format!("move failed: {error:#}"),
                    Ok(()) => {
                        let _ = outgoing.send(registry.ready_message().await);
                        format!("moved {label} to topic {topic}")
                    }
                }
            }
        },
        VoiceToolCall::ArchiveAgent { agent } => match resolve(registry, focus, &agent) {
            Err(error) => error,
            Ok((agent_id, label)) => {
                registry
                    .set_disposition(agent_id, AgentDisposition::Hidden)
                    .await;
                let _ = outgoing.send(registry.ready_message().await);
                format!("filed {label} away; it is hidden, not deleted")
            }
        },
        VoiceToolCall::FocusAgent { agent } => match resolve(registry, focus, &agent) {
            Err(error) => error,
            Ok((agent_id, label)) => {
                let _ = outgoing.send(ServerMessage::VoiceUiAction(VoiceUiAction::FocusAgent {
                    agent_id,
                }));
                format!("focused {label}")
            }
        },
        VoiceToolCall::ShowAgents => {
            let _ = outgoing.send(ServerMessage::VoiceUiAction(VoiceUiAction::ShowAgents));
            "showing the agent list".to_owned()
        }
        VoiceToolCall::OpenNewAgentScreen => {
            let _ = outgoing.send(ServerMessage::VoiceUiAction(
                VoiceUiAction::EnterNewAgentScreen,
            ));
            "opened the new-agent screen".to_owned()
        }
    }
}

async fn list_agents(registry: &Arc<AgentRegistry>, focus: &Option<AgentId>) -> String {
    let mut lines = Vec::new();
    for topic in registry.topics(&registry.agent_state_kinds().await) {
        for agent in &topic.agents {
            if agent.hidden {
                continue;
            }
            let activity = match registry.get(agent.agent_id).await {
                Some(running) => describe_state(&running.state()),
                None => "not loaded".to_owned(),
            };
            let focused = if *focus == Some(agent.agent_id) {
                " (focused)"
            } else {
                ""
            };
            lines.push(format!(
                "- {}{focused} in topic '{}': {activity}",
                label(agent),
                topic.name
            ));
        }
    }
    if lines.is_empty() {
        "no agents yet; new_agent starts one".to_owned()
    } else {
        format!("{} agents:\n{}", lines.len(), lines.join("\n"))
    }
}

async fn new_agent(
    registry: &Arc<AgentRegistry>,
    outgoing: &mpsc::UnboundedSender<ServerMessage>,
    workdir: Option<String>,
    topic: Option<String>,
    message: Option<String>,
) -> String {
    let workdirs = registry.workdirs();
    let path = match &workdir {
        Some(spoken) => {
            let needle = spoken.to_lowercase();
            let matches: Vec<_> = workdirs
                .iter()
                .filter(|workdir| {
                    workdir.name.to_lowercase().contains(&needle)
                        || workdir.path.as_str().to_lowercase().contains(&needle)
                })
                .collect();
            match matches.as_slice() {
                [workdir] => workdir.path.clone(),
                [] => return format!("no registered workdir matches '{spoken}'"),
                many => {
                    let names: Vec<_> = many.iter().map(|w| w.name.as_str()).collect();
                    return format!("'{spoken}' is ambiguous between: {}", names.join(", "));
                }
            }
        }
        None => match workdirs.as_slice() {
            [workdir] => workdir.path.clone(),
            [] => return "no workdirs are registered; register one first".to_owned(),
            many => {
                let names: Vec<_> = many.iter().map(|w| w.name.as_str()).collect();
                return format!("say which workdir: {}", names.join(", "));
            }
        },
    };
    let created = registry
        .create(
            registry.default_topic_id,
            AgentMode::Deep(DeepConfig {
                effort: DeepEffort::Medium,
                fast_mode: true,
            }),
            StartMode::NewOn {
                repo: path,
                // Same base the GUI's draft seeds: the parents of the user's
                // working copy.
                revset: "@-".to_owned(),
            },
        )
        .await;
    let (topic_id, agent_id, agent) = match created {
        Ok(created) => created,
        Err(error) => return format!("could not create the agent: {error:#}"),
    };
    subscribe_agent(agent_id, agent.clone(), outgoing.clone());
    let _ = outgoing.send(ServerMessage::AgentCreated { topic_id, agent_id });
    if let Some(topic) = &topic
        && let Err(error) = registry
            .move_agent(agent_id, TopicTarget::Named(topic.clone()))
            .await
    {
        return format!("agent created, but moving it to '{topic}' failed: {error:#}");
    }
    if let Some(message) = message {
        agent.send_user_message(message, MessageDelivery::NextRequest);
    }
    let _ = outgoing.send(registry.ready_message().await);
    match topic {
        Some(topic) => format!("created a new agent in topic '{topic}' and started it"),
        None => "created a new agent".to_owned(),
    }
}

/// `None` targets the focused agent; a name fuzzy-matches display names with
/// ambiguity reported, never guessed. Errors are prose for the model.
fn resolve(
    registry: &Arc<AgentRegistry>,
    focus: &Option<AgentId>,
    agent: &Option<String>,
) -> Result<(AgentId, String), String> {
    let all: Vec<(AgentId, String)> = registry
        // Only ids and labels are read here; attention (the `working` set)
        // is irrelevant, so an empty set keeps this resolver synchronous.
        .topics(&Default::default())
        .iter()
        .flat_map(|topic| topic.agents.iter())
        .map(|agent| (agent.agent_id, label(agent)))
        .collect();
    match agent {
        None => {
            let agent_id =
                focus.ok_or("no agent is focused; say which agent you mean".to_owned())?;
            let label = all
                .iter()
                .find(|(id, _)| *id == agent_id)
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| "the focused agent".to_owned());
            Ok((agent_id, label))
        }
        Some(spoken) => {
            let needle = spoken.to_lowercase();
            let matches: Vec<_> = all
                .iter()
                .filter(|(_, label)| label.to_lowercase().contains(&needle))
                .collect();
            match matches.as_slice() {
                [(agent_id, label)] => Ok((*agent_id, label.clone())),
                [] => Err(format!("no agent matches '{spoken}'; use list_agents")),
                many => Err(format!(
                    "'{spoken}' is ambiguous between: {}",
                    many.iter()
                        .map(|(_, label)| label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
    }
}

fn label(agent: &UiAgentSummary) -> String {
    if let Some(name) = &agent.display_name {
        return name.clone();
    }
    agent
        .workspace
        .workspace_name()
        .unwrap_or_else(|| "user-checkout".to_owned())
}

fn describe_state(state: &rho_agent::AgentState) -> String {
    use rho_agent::AgentStateKind;
    match &state.kind {
        AgentStateKind::Idle => "idle, waiting for instructions".to_owned(),
        AgentStateKind::ApiStreaming { .. } => "thinking".to_owned(),
        AgentStateKind::ToolCalling {
            waiting: Some(_), ..
        } => "waiting for messages from other agents or the user".to_owned(),
        AgentStateKind::ToolCalling {
            previews, results, ..
        } => format!(
            "running {} tools ({} finished)",
            previews.len() + results.len(),
            results.len()
        ),
        AgentStateKind::UnfinishedTurn { .. } => {
            "restored with an unfinished turn from a previous run".to_owned()
        }
        AgentStateKind::Error(failure) => format!(
            "errored after {} attempts: {}",
            failure.attempt_count, failure.error
        ),
    }
}

/// The last completed assistant answer, preferring final-answer text over
/// commentary.
fn last_response(label: &str, state: &rho_agent::AgentState) -> String {
    for block in state.blocks.iter().rev() {
        let rho_core::ContextBlock::InferenceResponse { items, .. } = block.as_ref() else {
            continue;
        };
        let text_of = |wanted_final: bool| {
            items
                .iter()
                .filter_map(|item| match item {
                    InferenceResponseItem::AssistantMessage { content, phase, .. } => {
                        let is_final = *phase == Some(MessagePhase::FinalAnswer);
                        (is_final == wanted_final || !wanted_final).then(|| text_content(content))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = {
            let final_text = text_of(true);
            if final_text.trim().is_empty() {
                text_of(false)
            } else {
                final_text
            }
        };
        if !text.trim().is_empty() {
            return text;
        }
    }
    format!("{label} has not answered yet")
}

/// Routes one voice-related client message; non-voice messages return
/// `false` so the caller's normal dispatch handles them.
pub(crate) fn handle_client_message(
    registry: &Arc<AgentRegistry>,
    outgoing: &mpsc::UnboundedSender<ServerMessage>,
    voice: &mut Option<VoiceHandle>,
    message: &ClientMessage,
) -> anyhow::Result<bool> {
    match message {
        ClientMessage::VoiceStart => {
            if voice.is_some() {
                anyhow::bail!("this connection already has a voice session");
            }
            *voice = Some(start(Arc::clone(registry), outgoing.clone())?);
            Ok(true)
        }
        ClientMessage::VoiceStop => {
            if let Some(handle) = voice.take() {
                handle.stop();
            }
            Ok(true)
        }
        ClientMessage::VoiceAudio { pcm } => {
            if let Some(handle) = voice {
                handle.audio(pcm.clone());
            }
            Ok(true)
        }
        ClientMessage::VoiceFocus { agent_id } => {
            if let Some(handle) = voice {
                handle.focus(*agent_id);
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}
