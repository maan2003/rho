//! Protocol smoke test against the live xAI realtime voice API.
//!
//! Exists to empirically capture the real event stream before daemon
//! integration: every event is printed (unknowns as raw JSON), assistant
//! audio is written to a WAV file for by-ear verification, and `--tool`
//! offers the full control-surface tool set served by a fake in-memory
//! agent registry, so the whole conversation loop is exercisable without a
//! daemon.
//!
//! Uses the Grok CLI OAuth login (`grok login`) and copies it into rho's voice
//! auth file on first use. Sessions are billed per minute; runs are bounded
//! by a per-event timeout.

use std::collections::HashMap;
use std::fmt;
use std::io::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use rho_voice::session::{VoiceConfig, VoiceSession};
use rho_voice::tools::{VoiceToolCall, parse_tool_call, tool_definitions};
use rho_voice::wire::{
    AudioConfig, ClientEvent, ConversationItem, RealtimeItem, ServerEvent, SessionConfig,
};

const SAMPLE_RATE: u32 = 24000;
const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(
    name = "rho-voice-smoke",
    about = "Smoke-test the Grok realtime voice protocol"
)]
struct Args {
    /// Text prompt sent as a user message (skipped when --input-pcm is given).
    #[arg(long, default_value = "Say hello to rho in one short sentence.")]
    text: String,
    /// Raw PCM16 mono 24kHz file streamed as microphone input instead of text.
    #[arg(long)]
    input_pcm: Option<std::path::PathBuf>,
    /// Where assistant audio is written (PCM16 mono 24kHz WAV).
    #[arg(long, default_value = "voice-smoke.wav")]
    wav: std::path::PathBuf,
    /// Offer the control-surface tools, served by a fake agent registry.
    #[arg(long)]
    tool: bool,
    #[arg(long, default_value = "grok-voice-think-fast-1.0")]
    model: String,
    #[arg(long, default_value = "eve")]
    voice: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let started = Instant::now();
    let args = Args::parse();
    let mut config = VoiceConfig::grok_cli()
        .context("initialize Grok OAuth credentials; run `grok login` first")?;
    config.model = args.model.clone();

    log_t(
        started,
        format_args!("connecting to {} ({})", config.base_url, config.model),
    );
    let mut session = VoiceSession::connect(&config).await?;
    log_t(started, format_args!("websocket connected"));

    let tools = if args.tool {
        tool_definitions()
    } else {
        Vec::new()
    };
    let mut registry = FakeRegistry::new();
    session
        .send(&ClientEvent::SessionUpdate {
            session: SessionConfig {
                voice: Some(args.voice.clone()),
                instructions: Some(
                    "You are rho's voice interface to the user's coding agents. \
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
                     or long output verbatim; summarize only what is needed."
                        .to_owned(),
                ),
                // Manual turns: the smoke test drives the conversation itself.
                turn_detection: Some(None),
                audio: Some(AudioConfig::pcm(SAMPLE_RATE)),
                tools,
            },
        })
        .await?;
    log_t(started, format_args!("session.update sent"));

    match &args.input_pcm {
        None => {
            session
                .send(&ClientEvent::ConversationItemCreate {
                    item: ConversationItem::user_text(&args.text),
                })
                .await?;
            log_t(started, format_args!("text input sent"));
        }
        Some(path) => {
            let pcm = std::fs::read(path).context("read --input-pcm file")?;
            log_t(
                started,
                format_args!("streaming {} bytes of PCM input", pcm.len()),
            );
            for chunk in pcm.chunks(SAMPLE_RATE as usize / 50 * 2) {
                session.send_audio(chunk).await?;
            }
            session.send(&ClientEvent::InputAudioBufferCommit).await?;
            log_t(started, format_args!("input_audio_buffer.commit sent"));
        }
    }
    session
        .send(&ClientEvent::ResponseCreate { response: None })
        .await?;
    log_t(started, format_args!("response.create sent"));

    let mut audio = Vec::new();
    let mut answered_tool_call = false;
    let mut tool_names = HashMap::<String, String>::new();
    let mut saw_first_event = false;
    let mut saw_audio_this_response = false;
    let mut saw_transcript_this_response = false;
    let mut saw_tool_delta_this_response = false;
    loop {
        let Some(event) = session.next_event(Some(EVENT_TIMEOUT)).await? else {
            log_t(started, format_args!("server closed the socket"));
            break;
        };
        if !saw_first_event {
            saw_first_event = true;
            log_t(started, format_args!("first server event received"));
        }
        match event {
            ServerEvent::Ping(event) => log_t(
                started,
                format_args!("ping: timestamp={:?}", event.timestamp),
            ),
            ServerEvent::SessionCreated(event) => log_t(
                started,
                format_args!(
                    "session.created: id={:?} model={:?} voice={:?}",
                    event.session.id, event.session.model, event.session.voice
                ),
            ),
            ServerEvent::SessionUpdated(event) => log_t(
                started,
                format_args!(
                    "session.updated: id={:?} model={:?} voice={:?}",
                    event.session.id, event.session.model, event.session.voice
                ),
            ),
            ServerEvent::ConversationCreated(event) => {
                log_t(
                    started,
                    format_args!("conversation.created: id={:?}", event.conversation_id()),
                );
            }
            ServerEvent::ConversationItemAdded(event) => {
                remember_tool_name(&mut tool_names, &event.item);
                log_t(
                    started,
                    format_args!("conversation.item.added: {}", item_summary(&event.item)),
                );
            }
            ServerEvent::ResponseCreated(event) => {
                saw_audio_this_response = false;
                saw_transcript_this_response = false;
                saw_tool_delta_this_response = false;
                log_t(
                    started,
                    format_args!(
                        "response.created: id={:?} status={:?}",
                        event.response_id(),
                        event.response.status
                    ),
                );
            }
            ServerEvent::ResponseOutputItemAdded(event) => {
                remember_tool_name(&mut tool_names, &event.item);
                log_t(
                    started,
                    format_args!("response.output_item.added: {}", item_summary(&event.item)),
                );
            }
            ServerEvent::ResponseOutputItemDone(event) => {
                remember_tool_name(&mut tool_names, &event.item);
                log_t(
                    started,
                    format_args!("response.output_item.done: {}", item_summary(&event.item)),
                );
            }
            ServerEvent::ResponseContentPartAdded(event) => log_t(
                started,
                format_args!(
                    "response.content_part.added: item={:?} content_index={:?} type={:?}",
                    event.item_id, event.content_index, event.part.part_type
                ),
            ),
            ServerEvent::ResponseContentPartDone(event) => log_t(
                started,
                format_args!(
                    "response.content_part.done: item={:?} content_index={:?}",
                    event.item_id, event.content_index
                ),
            ),
            ServerEvent::OutputAudioDelta(event) => {
                if !saw_audio_this_response {
                    saw_audio_this_response = true;
                    log_t(
                        started,
                        format_args!(
                            "first response.output_audio.delta for response: item={:?} content_index={:?}",
                            event.item_id, event.content_index
                        ),
                    );
                }
                audio.extend_from_slice(&event.audio);
                print!("♪");
                std::io::stdout().flush().ok();
            }
            ServerEvent::OutputAudioDone(event) => log_t(
                started,
                format_args!(
                    "response.output_audio.done: item={:?} content_index={:?}",
                    event.item_id, event.content_index
                ),
            ),
            ServerEvent::OutputAudioTranscriptDelta(event) => {
                if !saw_transcript_this_response {
                    saw_transcript_this_response = true;
                    log_t(
                        started,
                        format_args!(
                            "first response.output_audio_transcript.delta for response: item={:?} content_index={:?}",
                            event.item_id, event.content_index
                        ),
                    );
                }
                print!("{}", event.delta);
                std::io::stdout().flush().ok();
            }
            ServerEvent::OutputAudioTranscriptDone(event) => {
                log_t(
                    started,
                    format_args!(
                        "response.output_audio_transcript.done: {}",
                        event.transcript
                    ),
                );
            }
            ServerEvent::TextDelta(event) => {
                if !saw_transcript_this_response {
                    saw_transcript_this_response = true;
                    log_t(
                        started,
                        format_args!(
                            "first response.output_text.delta for response: item={:?} content_index={:?}",
                            event.item_id, event.content_index
                        ),
                    );
                }
                print!("{}", event.delta);
                std::io::stdout().flush().ok();
            }
            ServerEvent::TextDone(event) => {
                log_t(
                    started,
                    format_args!("response.output_text.done: {}", event.text),
                );
            }
            ServerEvent::FunctionCallArgumentsDelta(event) => {
                if !saw_tool_delta_this_response {
                    saw_tool_delta_this_response = true;
                    log_t(
                        started,
                        format_args!(
                            "first response.function_call_arguments.delta for response: call_id={:?}",
                            event.call_id
                        ),
                    );
                }
                log_t(
                    started,
                    format_args!(
                        "response.function_call_arguments.delta: call_id={:?} bytes={}",
                        event.call_id,
                        event.delta.len()
                    ),
                );
            }
            ServerEvent::FunctionCallArgumentsDone(event) => {
                let call_id = event.call_id;
                let name = event.name.or_else(|| tool_names.get(&call_id).cloned());
                let arguments = event.arguments;
                log_t(
                    started,
                    format_args!(
                        "response.function_call_arguments.done: name={name:?} call_id={call_id} args={arguments}"
                    ),
                );
                // A parse failure goes back to the model as the tool output so
                // it can correct itself — same policy the daemon will use.
                let output = match &name {
                    None => "error: this endpoint sent no tool name; report this".to_owned(),
                    Some(name) => match parse_tool_call(name, &arguments) {
                        Ok(call) => registry.execute(call),
                        Err(error) => format!("error: {error:#}"),
                    },
                };
                log_t(started, format_args!("tool output ready: {output}"));
                session
                    .send(&ClientEvent::ConversationItemCreate {
                        item: ConversationItem::FunctionCallOutput { call_id, output },
                    })
                    .await?;
                log_t(started, format_args!("function_call_output sent"));
                session
                    .send(&ClientEvent::ResponseCreate { response: None })
                    .await?;
                log_t(started, format_args!("follow-up response.create sent"));
                answered_tool_call = true;
            }
            ServerEvent::ResponseDone(event) => {
                let total_tokens = event
                    .usage
                    .as_ref()
                    .or_else(|| {
                        event
                            .response
                            .as_ref()
                            .and_then(|response| response.usage.as_ref())
                    })
                    .and_then(|usage| usage.total_tokens);
                log_t(
                    started,
                    format_args!(
                        "response.done: id={:?} status={:?} total_tokens={:?}",
                        event.response_id(),
                        event.status(),
                        total_tokens
                    ),
                );
                if answered_tool_call {
                    // One more response follows the tool answer.
                    answered_tool_call = false;
                } else {
                    break;
                }
            }
            ServerEvent::InputAudioSpeechStarted(event) => {
                log_t(
                    started,
                    format_args!(
                        "input_audio_buffer.speech_started: item={:?}",
                        event.item_id
                    ),
                );
            }
            ServerEvent::InputAudioSpeechStopped(event) => {
                log_t(
                    started,
                    format_args!(
                        "input_audio_buffer.speech_stopped: item={:?}",
                        event.item_id
                    ),
                );
            }
            ServerEvent::InputAudioBufferCommitted(event) => {
                log_t(
                    started,
                    format_args!("input_audio_buffer.committed: item={:?}", event.item_id),
                );
            }
            ServerEvent::InputAudioBufferCleared(_) => {
                log_t(started, format_args!("input_audio_buffer.cleared"));
            }
            ServerEvent::Error(event) => {
                bail!("server error: {}\n{event:#?}", event.message());
            }
            ServerEvent::Unknown { event_type, raw } => {
                log_t(started, format_args!("unknown event {event_type}: {raw}"));
            }
        }
    }

    if audio.is_empty() {
        log_t(started, format_args!("no audio received"));
    } else {
        write_wav(&args.wav, SAMPLE_RATE, &audio)?;
        let seconds = audio.len() as f64 / (SAMPLE_RATE as f64 * 2.0);
        log_t(
            started,
            format_args!("wrote {} ({seconds:.1}s of audio)", args.wav.display()),
        );
    }
    Ok(())
}

fn log_t(started: Instant, message: fmt::Arguments<'_>) {
    println!("[+{:.3}s] {message}", started.elapsed().as_secs_f64());
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

fn item_summary(item: &RealtimeItem) -> String {
    format!(
        "id={:?} type={:?} role={:?} status={:?} call_id={:?} name={:?}",
        item.id, item.item_type, item.role, item.status, item.call_id, item.name
    )
}

/// In-memory stand-in for the daemon's agent registry: enough state for the
/// tools to behave consistently across a multi-turn spoken conversation.
/// Tool outputs are plain prose — they are written for the model to speak
/// from, the same contract the daemon implementation will honor.
struct FakeAgent {
    name: String,
    topic: String,
    activity: String,
    last_response: String,
    archived: bool,
}

struct FakeRegistry {
    agents: Vec<FakeAgent>,
    /// What the GUI would report as the focused agent.
    focused: String,
}

impl FakeRegistry {
    fn new() -> Self {
        let agent = |name: &str, topic: &str, activity: &str, last_response: &str| FakeAgent {
            name: name.to_owned(),
            topic: topic.to_owned(),
            activity: activity.to_owned(),
            last_response: last_response.to_owned(),
            archived: false,
        };
        Self {
            agents: vec![
                agent(
                    "gui-polish",
                    "rho-gui",
                    "idle for 10 minutes",
                    "I finished the topic rail hover states and all 14 tests pass.",
                ),
                agent(
                    "voice-integration",
                    "rho-voice",
                    "running tests, 3 minutes into the turn",
                    "Still working: wiring the websocket session into the daemon.",
                ),
                agent(
                    "flaky-ci",
                    "default",
                    "errored: inference request failed after 4 attempts",
                    "The retry logic needs a decision on backoff caps before I continue.",
                ),
            ],
            focused: "voice-integration".to_owned(),
        }
    }

    /// Resolves `agent` like the daemon will: `None` means the focused agent,
    /// otherwise a case-insensitive substring match over display names, with
    /// ambiguity reported instead of guessed.
    fn resolve(&mut self, agent: &Option<String>) -> Result<usize, String> {
        let spoken = match agent {
            None => &self.focused,
            Some(name) => name,
        };
        let needle = spoken.to_lowercase();
        let matches: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| agent.name.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect();
        match matches.as_slice() {
            [index] => Ok(*index),
            [] => Err(format!("no agent matches '{spoken}'; use list_agents")),
            many => Err(format!(
                "'{spoken}' is ambiguous between: {}",
                many.iter()
                    .map(|&index| self.agents[index].name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn execute(&mut self, call: VoiceToolCall) -> String {
        match call {
            VoiceToolCall::ListAgents => {
                let lines: Vec<String> = self
                    .agents
                    .iter()
                    .filter(|agent| !agent.archived)
                    .map(|agent| {
                        let focus = if agent.name == self.focused {
                            " (focused)"
                        } else {
                            ""
                        };
                        format!(
                            "- {}{focus} in topic '{}': {}",
                            agent.name, agent.topic, agent.activity
                        )
                    })
                    .collect();
                format!("{} agents:\n{}", lines.len(), lines.join("\n"))
            }
            VoiceToolCall::AgentStatus { agent } => match self.resolve(&agent) {
                Err(error) => error,
                Ok(index) => {
                    let agent = &self.agents[index];
                    format!("{} is {}", agent.name, agent.activity)
                }
            },
            VoiceToolCall::ReadLastResponse { agent } => match self.resolve(&agent) {
                Err(error) => error,
                Ok(index) => self.agents[index].last_response.clone(),
            },
            VoiceToolCall::SendMessage { agent, message } => match self.resolve(&agent) {
                Err(error) => error,
                Ok(index) => {
                    let agent = &mut self.agents[index];
                    agent.activity = "just started a turn on your message".to_owned();
                    format!(
                        "sent to {}: '{message}'. It works asynchronously; \
                         you'll hear when it finishes.",
                        agent.name
                    )
                }
            },
            VoiceToolCall::NewAgent {
                workdir,
                topic,
                message,
            } => {
                let name = format!("agent-{}", self.agents.len() + 1);
                let activity = match &message {
                    Some(_) => "just started on your instruction".to_owned(),
                    None => "idle, waiting for instructions".to_owned(),
                };
                self.agents.push(FakeAgent {
                    name: name.clone(),
                    topic: topic.unwrap_or_else(|| "default".to_owned()),
                    activity,
                    last_response: String::new(),
                    archived: false,
                });
                format!(
                    "created {name} in {}",
                    workdir.unwrap_or_else(|| "the default workdir".to_owned())
                )
            }
            VoiceToolCall::CancelTurn { agent } => match self.resolve(&agent) {
                Err(error) => error,
                Ok(index) => {
                    let agent = &mut self.agents[index];
                    agent.activity = "idle (turn cancelled)".to_owned();
                    format!("cancelled {}'s turn", agent.name)
                }
            },
            VoiceToolCall::RenameAgent { agent, name } => match self.resolve(&agent) {
                Err(error) => error,
                Ok(index) => {
                    let old = std::mem::replace(&mut self.agents[index].name, name.clone());
                    if self.focused == old {
                        self.focused = name.clone();
                    }
                    format!("renamed {old} to {name}")
                }
            },
            VoiceToolCall::MoveToTopic { agent, topic } => match self.resolve(&agent) {
                Err(error) => error,
                Ok(index) => {
                    let agent = &mut self.agents[index];
                    agent.topic = topic.clone();
                    format!("moved {} to topic '{topic}'", agent.name)
                }
            },
            VoiceToolCall::ArchiveAgent { agent } => match self.resolve(&agent) {
                Err(error) => error,
                Ok(index) => {
                    let agent = &mut self.agents[index];
                    agent.archived = true;
                    format!("archived {} (hidden, not deleted)", agent.name)
                }
            },
            VoiceToolCall::FocusAgent { agent } => match self.resolve(&agent) {
                Err(error) => error,
                Ok(index) => {
                    self.focused = self.agents[index].name.clone();
                    format!("focused {}", self.focused)
                }
            },
            VoiceToolCall::ShowAgents => "showing the agent list".to_owned(),
            VoiceToolCall::OpenNewAgentScreen => "opened the new-agent screen".to_owned(),
        }
    }
}

/// Minimal PCM16 mono WAV container around the raw samples.
fn write_wav(path: &std::path::Path, sample_rate: u32, pcm: &[u8]) -> Result<()> {
    let data_len: u32 = pcm.len().try_into().context("audio too large for wav")?;
    let byte_rate = sample_rate * 2;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // linear PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
