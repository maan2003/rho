//! `rho`, ad hoc: an agent working here, in this terminal.
//!
//! A bare `rho` opens a draft for a new agent in the current directory.
//! Until the first prompt is sent nothing exists, and the role can still
//! change; the first prompt creates the agent and the screen follows it.
//! `rho attach <agent>` opens one that exists. The chat view shows what
//! was said, the activity view what was done; Tab switches.
//!
//! The agent host on this machine is started if it is not running. On a
//! clean exit (Ctrl-D) a host this CLI started is stopped again, unless a
//! client attached meanwhile or an agent still works; the host decides.
//! Losing the terminal leaves everything running, to attach again.

mod agent;
mod host;
mod projection;
mod screen;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use rho_agent_types::{AgentPos, AgentRole, ContentPart};
use rho_agents_client::create::{DEFAULT_ROLE, cycle_agent_role_text, parse_agent_role};
use rho_agents_client::protocol::transcript::RuntimeState;
use rho_agents_client::protocol::{
    AgentCommand, AgentLog, ClientFrame, NewAgent, ServerFrame, StartMode, UserMessage,
};
use rho_agents_client::state::{UiAgentStatus, UiBlock, UiToolStatus};
use rho_term_ui::{Color, Event, Key, Span, Style, StyledBlock, StyledText, Term, TermHandle};

use crate::agent::{Directory, Followed, resolve, short_handle};
use crate::host::Host;
use crate::projection::View;
use crate::screen::Transcript;

#[derive(Parser)]
#[command(
    name = "rho",
    about = "Start an agent here, or attach to one, and chat with it"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The new agent's role; Shift-Tab cycles it until the first prompt.
    #[arg(long, default_value = DEFAULT_ROLE, global = true)]
    role: String,
    /// The agent host's socket.
    #[arg(long, global = true)]
    socket_path: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Open an agent that exists: its handle or the start of its id.
    Attach { agent: String },
}

fn main() -> Result<()> {
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(args))
}

/// Where the screen stands: a new agent not yet asked for, or one being
/// followed.
enum Phase {
    Draft { role: String },
    Following(Box<Followed>),
}

async fn run(args: Args) -> Result<()> {
    let cwd = std::env::current_dir().context("current directory")?;
    parse_agent_role(&args.role).map_err(|error| anyhow::anyhow!(error))?;
    let host = Host::connect_or_start(args.socket_path).await?;
    let (to_host, mut from_host, session) = host.session().await?;
    let (journal_head, machine_seed, agent_counter) =
        match from_host.recv().await.context("the agent host closed")?? {
            ServerFrame::JournalHead {
                journal_head,
                machine_seed,
                agent_counter,
            } => (journal_head, machine_seed, agent_counter),
            _ => anyhow::bail!("the agent host did not open with its journal head"),
        };
    // Every row after the head arrives on the session; one agent's rows
    // before it are asked for on their own.
    to_host.send(ClientFrame::Follow {
        since: journal_head,
    })?;
    let mut directory = Directory::default();
    let mut phase = match &args.command {
        None => Phase::Draft { role: args.role },
        Some(Command::Attach { agent }) => {
            let agent_id = resolve(agent, machine_seed, agent_counter)
                .map_err(|error| anyhow::anyhow!(error))?;
            let mut followed = Followed::new(agent_id);
            // Rows the session carries meanwhile wait in its channel and are
            // folded after these; the fold drops the ones read twice.
            let mut from = AgentPos(0);
            loop {
                let page = host.call(AgentLog { agent_id, from }).await?;
                for (_, event) in &page.rows {
                    directory.tell_one(agent_id, event);
                }
                followed.tell_rows(&page.rows);
                match page.next {
                    Some(next) => from = next,
                    None => break,
                }
            }
            to_host.send(ClientFrame::Focus {
                agent_ids: vec![agent_id],
            })?;
            Phase::Following(Box::new(followed))
        }
    };

    let (term, handle) = Term::new(Span::new("› ", Style::default().fg(Color::Cyan).bold()))?;
    handle.set_prompt_gap(true);
    let status = handle.new_block(StyledBlock::new(""));
    handle.push_below(status);
    let mut transcript = Transcript::new(handle.clone(), View::Chat);
    handle.set_input_placeholder(Span::new(
        "What should the agent do?",
        Style::default().fg(Color::DarkGrey),
    ));
    let (events, input) = spawn_input(term);

    let ui = Ui {
        handle: handle.clone(),
        status,
        cwd: cwd.display().to_string(),
    };
    let mut runtime: Option<RuntimeState> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    if let Phase::Following(followed) = &phase
        && let Some(state) = followed.state()
    {
        transcript.sync(state, rho_agents_client::now_ms(), &|id| {
            directory.label(id)
        });
    }
    ui.status(&phase, transcript.view(), &directory, runtime.as_ref());

    let mut events = events;
    let clean = loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break false };
                match event {
                    Ok(Event::Eof) => break true,
                    Err(_) => break false,
                    Ok(Event::Line(text)) => {
                        let text = text.trim().to_owned();
                        if text.is_empty() {
                            continue;
                        }
                        match &mut phase {
                            Phase::Draft { role } => {
                                let role: AgentRole = parse_agent_role(role)
                                    .map_err(|error| anyhow::anyhow!(error))?;
                                let new = NewAgent {
                                    role,
                                    start: StartMode::Here {
                                        cwd: camino::Utf8PathBuf::try_from(cwd.clone())
                                            .context("the current directory is not UTF-8")?,
                                    },
                                    content: Some(vec![ContentPart::Text { text }]),
                                };
                                match host.call(new).await {
                                    Ok(agent_id) => {
                                        to_host.send(ClientFrame::Focus {
                                            agent_ids: vec![agent_id],
                                        })?;
                                        phase = Phase::Following(Box::new(Followed::new(agent_id)));
                                    }
                                    Err(error) => ui.notice(&format!("could not start the agent: {error:#}")),
                                }
                            }
                            Phase::Following(followed) => {
                                let command = AgentCommand::Send {
                                    agent_id: followed.agent_id,
                                    messages: vec![UserMessage {
                                        id: rand::random(),
                                        content: vec![ContentPart::Text { text }],
                                    }],
                                };
                                if let Err(error) = host.call(command).await {
                                    ui.notice(&format!("not sent: {error:#}"));
                                }
                            }
                        }
                    }
                    Ok(Event::Interrupt) => match &phase {
                        Phase::Following(followed)
                            if runtime.as_ref().is_some_and(RuntimeState::is_working) =>
                        {
                            let command = AgentCommand::Cancel { agent_id: followed.agent_id };
                            if let Err(error) = host.call(command).await {
                                ui.notice(&format!("could not stop the agent: {error:#}"));
                            }
                        }
                        _ => ui.notice("Ctrl-D leaves"),
                    },
                    Ok(Event::Key(Key::BackTab)) => {
                        if let Phase::Draft { role } = &mut phase {
                            *role = cycle_agent_role_text(role).to_owned();
                        }
                    }
                    Ok(Event::Key(Key::Tab)) => {
                        if let Phase::Following(followed) = &phase {
                            transcript.set_view(transcript.view().toggled());
                            // Clearing took the status line with it.
                            handle.push_below(status);
                            if let Some(state) = followed.state() {
                                transcript.sync(state, rho_agents_client::now_ms(), &|id| directory.label(id));
                            }
                        }
                    }
                    Ok(_) => continue,
                }
            }
            frame = from_host.recv() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(error)) => {
                        ui.notice(&format!("lost the agent host: {error:#}"));
                        break false;
                    }
                    None => break false,
                };
                let Phase::Following(followed) = &mut phase else {
                    if let ServerFrame::Log { entries } = &frame {
                        directory.tell(entries);
                    }
                    continue;
                };
                let moved = match frame {
                    ServerFrame::Log { entries } => {
                        directory.tell(&entries);
                        followed.tell(&entries)
                    }
                    ServerFrame::Live { agent_id, live } if agent_id == followed.agent_id => {
                        if let rho_agents_client::protocol::transcript::Live::Snapshot { state, .. } = &live {
                            runtime = Some(state.clone());
                        }
                        followed.live(live);
                        true
                    }
                    _ => false,
                };
                if !moved {
                    continue;
                }
                if let Some(state) = followed.state() {
                    transcript.sync(state, rho_agents_client::now_ms(), &|id| directory.label(id));
                }
            }
            _ = tick.tick() => {
                // Running calls count their seconds.
                if let Phase::Following(followed) = &phase
                    && let Some(state) = followed.state()
                    && state.blocks.iter().any(|block| matches!(block.as_ref(), UiBlock::Tool(tool) if tool.status == UiToolStatus::Running))
                {
                    transcript.sync(state, rho_agents_client::now_ms(), &|id| directory.label(id));
                }
            }
        }
        ui.status(&phase, transcript.view(), &directory, runtime.as_ref());
    };

    // The prompt and status line stay on screen after exit; they are not
    // the transcript.
    handle.set_buffer(String::new(), 0);
    handle.set_input_placeholder(StyledText::new());
    handle.remove_below(status);
    // The input thread still holds the terminal, so this draws.
    handle.redraw_sync();
    // With nobody to take its events, the input thread stops at its next
    // one, which the shutdown provides; it drops the terminal on the way
    // out, and then the terminal is the user's again and safe to print to.
    drop(events);
    handle.request_input_shutdown();
    let _ = tokio::task::spawn_blocking(move || input.join()).await;
    drop(to_host);
    drop(from_host);
    let _ = session.await;

    if let Phase::Following(followed) = &phase {
        let handle = directory.get(followed.agent_id).map_or_else(
            || followed.agent_id.encoded(),
            |identity| short_handle(identity.role.handle_prefix(), followed.agent_id),
        );
        eprintln!("agent {handle}: `rho attach {handle}` opens it again");
    }
    if clean && let Some(reason) = host.leave().await {
        eprintln!("the agent host keeps running: {reason}");
    }
    Ok(())
}

/// The terminal's input, read on a thread of its own: `get_next_event`
/// blocks. The thread owns the terminal and restores it when it ends:
/// once nobody takes its events, or on a terminal error.
fn spawn_input(
    term: Term,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<std::io::Result<Event>>,
    std::thread::JoinHandle<()>,
) {
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let thread = std::thread::spawn(move || {
        loop {
            // Ctrl-D is an event like any other: the terminal stays up
            // until the session has drawn its last frame.
            let event = term.get_next_event();
            let failed = event.is_err();
            if events.send(event).is_err() || failed {
                break;
            }
        }
    });
    (receiver, thread)
}

/// The lines around the transcript: the status line under the prompt and
/// the notices printed above it.
struct Ui {
    handle: TermHandle,
    status: rho_term_ui::BlockId,
    cwd: String,
}

impl Ui {
    fn notice(&self, text: &str) {
        self.handle.print_output(StyledText::from(Span::new(
            text,
            Style::default().fg(Color::Yellow),
        )));
    }

    fn status(
        &self,
        phase: &Phase,
        view: View,
        directory: &Directory,
        runtime: Option<&RuntimeState>,
    ) {
        let dim = Style::default().fg(Color::DarkGrey);
        let mut spans = Vec::new();
        match phase {
            Phase::Draft { role } => {
                spans.push(Span::new("new agent", Style::default().bold()));
                spans.push(Span::new(format!(" in {} · ", self.cwd), dim));
                spans.push(Span::new(role.clone(), Style::default().fg(Color::Cyan)));
                spans.push(Span::new(
                    " (Shift-Tab) · Enter starts it · Ctrl-D leaves",
                    dim,
                ));
            }
            Phase::Following(followed) => {
                let handle = directory.label(followed.agent_id);
                spans.push(Span::new(handle, Style::default().bold()));
                let (state, color) = followed
                    .state()
                    .map_or(("connecting", Color::DarkGrey), |state| {
                        status_word(state.status)
                    });
                spans.push(Span::new(" · ", dim));
                spans.push(Span::new(state, Style::default().fg(color)));
                if let Some(tasks) = runtime.map(|runtime| runtime.running_tasks)
                    && tasks > 0
                {
                    spans.push(Span::new(format!(" · {tasks} running"), dim));
                }
                spans.push(Span::new(
                    format!(
                        " · {} (Tab: {}) · Ctrl-C stops · Ctrl-D leaves",
                        view.name(),
                        view.toggled().name()
                    ),
                    dim,
                ));
            }
        }
        self.handle
            .set_block(self.status, StyledBlock::new(StyledText::from(spans)));
        self.handle.redraw();
    }
}

fn status_word(status: UiAgentStatus) -> (&'static str, Color) {
    match status {
        UiAgentStatus::Idle => ("idle", Color::Green),
        UiAgentStatus::Streaming => ("responding", Color::Yellow),
        UiAgentStatus::ToolCalling { .. } => ("working", Color::Yellow),
        UiAgentStatus::Retrying { .. } => ("retrying", Color::DarkYellow),
        UiAgentStatus::UnfinishedTurn { .. } => ("unfinished", Color::DarkYellow),
        UiAgentStatus::Error => ("stopped on an error", Color::Red),
    }
}
