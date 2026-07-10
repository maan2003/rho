//! Runnable terminal UI for the opinionated rho agent harness.
//!
//! This crate deliberately assembles concrete rho building blocks instead of
//! defining a reusable CLI framework: `rho-agent` owns the harness loop,
//! `rho-inference` owns inference transport, and
//! `rho-tool-shell` owns the built-in shell/apply_patch tools. Fork this crate
//! when the desired user experience diverges.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use rho_cli_term_raw::{
    BlockId, Color, CursorShape, Event, Span, Style, StyledBlock, StyledText, Term, TermHandle,
};
use rho_core::ToolOutputStatus;
use rho_daemon::debug::DebugArgs;
use rho_daemon::{DaemonArgs, default_socket_path};
use rho_inference::{AuthArgs, run_auth_cli};
use rho_ui_proto::client::{AgentClient, Client as UiClient};
use rho_ui_proto::remote::{UiAgentState, UiAgentStatus, UiBlock, UiTool, UiToolStatus};
use rho_ui_proto::{AgentId, IoCounters, MessageDelivery};
use rho_voice::{VoiceArgs, run_voice_cli};
use tokio::task::JoinHandle;

mod completion;
mod land;
mod markdown;
mod mcp_agent_tools;
mod slack;
mod tool_render;

#[cfg(test)]
mod tests;

use markdown::markdown_block;
use tool_render::{ToolRenderStatus, tool_call_block};

const UI_IO_BUCKET_SECS: u64 = 1;
const UI_IO_WINDOW_SECS: u64 = 30;
const UI_IO_BUCKETS: usize = (UI_IO_WINDOW_SECS / UI_IO_BUCKET_SECS) as usize;

pub fn main() -> Result<()> {
    let args = Args::parse_or_exit(std::env::args().skip(1));
    if matches!(args.command, Command::Daemon(_)) {
        // SAFETY: top of main, before the runtime — no threads exist yet and
        // nothing has captured pre-namespace state.
        unsafe { rho_daemon::init_daemon_namespace() }.expect("set up daemon namespace");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(args.command))
}

async fn run(command: Command) -> Result<()> {
    match command {
        Command::Chat(args) => {
            if args.prompt_stdin {
                run_prompt_stdin(args).await
            } else {
                run_interactive(args).await
            }
        }
        Command::Auth(auth) => {
            run_auth_cli(auth)?;
            Ok(())
        }
        Command::Voice(voice) => {
            run_voice_cli(voice)?;
            Ok(())
        }
        Command::Daemon(args) => rho_daemon::run(args).await,
        Command::Debug(args) => {
            rho_daemon::debug::run(args).await?;
            Ok(())
        }
        Command::Land(args) => land::run(args).await,
        Command::McpAgentTools(args) => mcp_agent_tools::run(args).await,
        Command::Slack(args) => slack::run(args).await,
        Command::ProtocolLog(args) => {
            let mut stdout = io::stdout().lock();
            rho_ui_proto::print_protocol_log(&args.path, &mut stdout)?;
            Ok(())
        }
    }
}

async fn run_interactive(args: ChatArgs) -> Result<()> {
    let mut term = ChatTerm::new()?;
    let agent = build_agent(&args, Some(term.renderer())).await?;
    term.set_completion_agent(agent.clone());
    term.start_io_status(agent.io_counters());
    term.print_history(&agent.blocks());
    let mut app = ChatApp {
        agent,
        running_turn: None,
        term,
    };
    app.run().await
}

async fn run_prompt_stdin(args: ChatArgs) -> Result<()> {
    let mut prompt = String::new();
    io::stdin().read_to_string(&mut prompt)?;
    if prompt.trim().is_empty() {
        return Ok(());
    }

    let renderer = PlainRenderer::default();
    let output = renderer.output();
    let agent = build_agent(&args, None).await?;
    send_prompt(&agent, prompt);
    watch_agent(&agent, Some(output)).await;

    let text = renderer.finish();
    if !text.is_empty() {
        let mut stdout = io::stdout().lock();
        stdout.write_all(text.as_bytes())?;
        if !text.ends_with('\n') {
            stdout.write_all(b"\n")?;
        }
        stdout.flush()?;
    }
    Ok(())
}

async fn build_agent(args: &ChatArgs, renderer: Option<UpdateRenderer>) -> Result<AgentClient> {
    let socket_path = default_socket_path()?;
    let client =
        AgentClient::connect_client(connect_or_start_daemon(&socket_path, &args.auth).await?)
            .await?;
    if renderer.is_some() {
        let changes = client.subscribe();
        tokio::spawn(async move {
            futures::pin_mut!(changes);
            while let Some(states) = changes.next().await {
                let Some(state) = primary_state(&states) else {
                    continue;
                };
                if let Some(renderer) = &renderer
                    && let Ok(mut renderer) = renderer.lock()
                {
                    renderer.handle_state(state);
                }
            }
        });
    }
    Ok(client)
}

pub(crate) async fn connect_or_start_daemon(
    socket_path: &std::path::Path,
    auth: &str,
) -> Result<UiClient> {
    if let Ok(client) = UiClient::connect(socket_path).await {
        return Ok(client);
    }

    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("--auth")
        .arg(auth)
        .arg("--socket-path")
        .arg(socket_path)
        .arg("--die-on-detached")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match UiClient::connect(socket_path).await {
            Ok(client) => return Ok(client),
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

struct ChatApp {
    agent: AgentClient,
    running_turn: Option<JoinHandle<()>>,
    term: ChatTerm,
}

impl ChatApp {
    async fn run(&mut self) -> Result<()> {
        loop {
            self.reap_finished_turn().await;
            match self.term.get_next_event()? {
                Event::Line(line) => {
                    let line = line.trim().to_owned();
                    if line.is_empty() {
                        continue;
                    }
                    if matches!(line.as_str(), ":quit" | ":exit") {
                        self.stop_running_turn(false).await;
                        break;
                    }
                    if self.handle_command(&line).await? {
                        continue;
                    }
                    if self.turn_is_running() {
                        self.term
                            .print_system("agent is running; press Ctrl-C twice to cancel");
                        continue;
                    }
                    self.term.print_user(&line);
                    self.term.set_status("running");
                    send_prompt(&self.agent, line);
                    self.running_turn = Some(spawn_turn_watcher(
                        self.agent.clone(),
                        self.term.renderer(),
                        self.term.handle.clone(),
                    ));
                }
                Event::Eof => {
                    self.stop_running_turn(false).await;
                    break;
                }
                Event::CancelPrompt => self.cancel_running_turn().await,
                Event::Resize { .. } | Event::BufferChanged | Event::CompletionAccept => {}
                Event::FocusChanged { .. } => {}
                Event::BackTab | Event::Escape | Event::ExternalEditor | Event::Binding(_) => {}
                Event::Notice(message) => self.term.print_system(&message),
            }
        }
        Ok(())
    }

    async fn handle_command(&mut self, line: &str) -> Result<bool> {
        let Some(parsed) = rho_commands::parse(line) else {
            return Ok(false);
        };
        let command = match parsed {
            rho_commands::Parsed::Command(command) => command,
            rho_commands::Parsed::Invalid(usage) => {
                self.term.print_system(&format!("usage: {usage}"));
                return Ok(true);
            }
            rho_commands::Parsed::Unknown(command) => {
                self.term
                    .print_system(&format!("unknown command `{command}`; try :help"));
                return Ok(true);
            }
        };
        match command {
            rho_commands::Command::Quit => unreachable!("handled before command dispatch"),
            rho_commands::Command::VoiceToggle => {
                // Voice needs an audio device next to the user; only the GUI
                // client carries one.
                self.term
                    .print_system(":voice is only available in rho-gui");
            }
            rho_commands::Command::AgentCancel => {
                self.cancel_running_turn().await;
            }
            rho_commands::Command::Continue => {
                if self.turn_is_running() {
                    self.term
                        .print_system("agent is running; press Ctrl-C twice to cancel");
                    return Ok(true);
                }
                let Some(agent_id) = primary_agent_id(&self.agent) else {
                    self.term.print_system(":continue: no agent yet");
                    return Ok(true);
                };
                self.term.set_status("running");
                self.agent.continue_turn(agent_id);
                self.running_turn = Some(spawn_turn_watcher(
                    self.agent.clone(),
                    self.term.renderer(),
                    self.term.handle.clone(),
                ));
            }
            rho_commands::Command::Compact => {
                let Some(agent_id) = primary_agent_id(&self.agent) else {
                    self.term.print_system(":compact: no agent yet");
                    return Ok(true);
                };
                self.term.print_system("compacting context");
                self.agent
                    .compact_agent(agent_id, MessageDelivery::NextTurn);
            }
            rho_commands::Command::AgentNew { working_directory } => {
                let topic_id = self.agent.default_topic_id();
                let Some(working_directory) =
                    resolve_working_directory(working_directory, &workdir_table(&self.agent))
                else {
                    self.term
                        .print_system("cannot determine a working directory");
                    return Ok(true);
                };
                self.term
                    .print_system(&format!("new agent in {working_directory}"));
                self.agent.new_agent_in_topic(topic_id, working_directory);
            }
            rho_commands::Command::AgentRename { name } => {
                let Some(agent_id) = primary_agent_id(&self.agent) else {
                    self.term.print_system(":agent rename: no agent yet");
                    return Ok(true);
                };
                self.term
                    .print_system(&format!("renamed {agent_id:?} to `{name}`"));
                self.agent.rename_agent(agent_id, name);
            }
            rho_commands::Command::AgentPin => {
                self.toggle_agent_status(rho_ui_proto::Status::Pinned);
            }
            rho_commands::Command::AgentFast { .. } | rho_commands::Command::AgentEffort { .. } => {
                self.term
                    .print_system("runtime mode changes are only available in rho-gui");
            }
            rho_commands::Command::AgentDone { .. } | rho_commands::Command::AgentSnooze { .. } => {
                // Attention triage lives in the GUI rail; the CLI fronts a
                // single agent and has nothing to clear.
                self.term
                    .print_system(":done and :snooze are only available in rho-gui");
            }
            rho_commands::Command::Rewind { turns } => {
                if self.turn_is_running() {
                    self.term
                        .print_system(":rewind is only available while idle; use :cancel first");
                    return Ok(true);
                }
                let Some(agent_id) = primary_agent_id(&self.agent) else {
                    self.term.print_system(":rewind: no agent yet");
                    return Ok(true);
                };
                self.term
                    .print_system(&format!("rewinding {turns} turn(s)"));
                self.agent.rewind(agent_id, turns);
            }
            rho_commands::Command::TopicNew { name } => {
                self.term.print_system(&format!("creating topic `{name}`"));
                self.agent.new_topic(name);
            }
            rho_commands::Command::TopicRename { name } => {
                let topic_id = primary_agent_id(&self.agent)
                    .and_then(|agent_id| topic_of(&self.agent, agent_id))
                    .unwrap_or_else(|| self.agent.default_topic_id());
                self.term
                    .print_system(&format!("renamed topic to `{name}`"));
                self.agent.rename_topic(topic_id, name);
            }
            rho_commands::Command::TopicPin { name } => {
                self.toggle_topic_status(name, rho_ui_proto::Status::Pinned);
            }
            rho_commands::Command::TopicMove { name } => {
                let Some(agent_id) = primary_agent_id(&self.agent) else {
                    self.term.print_system(":topic move: no agent yet");
                    return Ok(true);
                };
                let topics = topic_labels(&self.agent);
                let target = match rho_commands::resolve_topic(&name, &topics) {
                    Some(topic_id) => rho_ui_proto::TopicTarget::Existing(topic_id),
                    None => rho_ui_proto::TopicTarget::Named(name.clone()),
                };
                self.term
                    .print_system(&format!("moving {agent_id:?} to topic `{name}`"));
                self.agent.move_agent(agent_id, target);
            }
            rho_commands::Command::WorkdirAdd { path, name } => {
                let Some(path) = resolve_working_directory(path, &[]) else {
                    self.term
                        .print_system("cannot determine a working directory");
                    return Ok(true);
                };
                self.term.print_system(&format!("registered {path}"));
                self.agent.set_workdir(path, name);
            }
            rho_commands::Command::WorkdirRemove { path } => {
                let workdirs = workdir_table(&self.agent);
                match rho_commands::resolve_workdir(&path, &workdirs) {
                    Some(resolved) => {
                        self.term.print_system(&format!("removed {resolved}"));
                        self.agent.remove_workdir(resolved.into());
                    }
                    None => self
                        .term
                        .print_system(&format!("no registered workdir `{path}`")),
                }
            }
            rho_commands::Command::Clear => {
                self.term.clear_output();
                if let Ok(mut renderer) = self.term.renderer.lock() {
                    renderer.clear();
                }
                self.term.print_system("cleared");
            }
            rho_commands::Command::Help => self.term.print_help(),
            rho_commands::Command::Version => self.term.print_system(env!("CARGO_PKG_VERSION")),
        }
        Ok(true)
    }

    fn turn_is_running(&self) -> bool {
        self.running_turn
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// Pin toggles for the chat's agent.
    fn toggle_agent_status(&mut self, target: rho_ui_proto::Status) {
        let Some(agent_id) = primary_agent_id(&self.agent) else {
            self.term.print_system("no agent yet");
            return;
        };
        let current = self
            .agent
            .topics()
            .iter()
            .flat_map(|topic| &topic.agents)
            .find(|summary| summary.agent_id == agent_id)
            .map(|summary| summary.status)
            .unwrap_or(rho_ui_proto::Status::Normal);
        let status = rho_commands::toggle_status(current, target);
        self.term
            .print_system(&format!("{agent_id:?} is now {status:?}"));
        self.agent.set_agent_status(agent_id, status);
    }

    /// Pin toggles for a topic named by argument, defaulting to the
    /// chat agent's own topic.
    fn toggle_topic_status(&mut self, name: Option<String>, target: rho_ui_proto::Status) {
        let topics = self.agent.topics();
        let topic_id = match &name {
            Some(name) => {
                let Some(topic_id) = rho_commands::resolve_topic(name, &topic_labels(&self.agent))
                else {
                    self.term.print_system(&format!("no topic named `{name}`"));
                    return;
                };
                topic_id
            }
            None => primary_agent_id(&self.agent)
                .and_then(|agent_id| {
                    topics
                        .iter()
                        .find(|topic| topic.agents.iter().any(|a| a.agent_id == agent_id))
                        .map(|topic| topic.topic_id)
                })
                .unwrap_or_else(|| self.agent.default_topic_id()),
        };
        let current = topics
            .iter()
            .find(|topic| topic.topic_id == topic_id)
            .map(|topic| topic.status)
            .unwrap_or(rho_ui_proto::Status::Normal);
        let status = rho_commands::toggle_status(current, target);
        self.term
            .print_system(&format!("{topic_id:?} is now {status:?}"));
        self.agent.set_topic_status(topic_id, status);
    }

    async fn reap_finished_turn(&mut self) {
        let Some(handle) = self.running_turn.take() else {
            return;
        };
        if handle.is_finished() {
            let _ = handle.await;
        } else {
            self.running_turn = Some(handle);
        }
    }

    async fn cancel_running_turn(&mut self) {
        self.stop_running_turn(true).await;
    }

    async fn stop_running_turn(&mut self, print_cancelled: bool) {
        let Some(handle) = self.running_turn.take() else {
            if print_cancelled {
                self.term.print_system("nothing running");
            }
            return;
        };
        if handle.is_finished() {
            let _ = handle.await;
            if print_cancelled {
                self.term.print_system("nothing running");
            }
            self.term.set_status("/quit");
            return;
        }
        // Ask the agent loop to interrupt; the spawned `send` task records the
        // cancellation, flushes its rendering, and then completes.
        if let Some(agent_id) = primary_agent_id(&self.agent) {
            self.agent.cancel(agent_id);
        }
        let _ = handle.await;
        if print_cancelled {
            self.term.print_system("cancelled");
        }
        self.term.set_status("/quit");
    }
}

/// The chat targets one agent: the lowest agent id, matching
/// [`AgentClient::state`].
fn primary_agent_id(agent: &AgentClient) -> Option<AgentId> {
    agent.known_agent_ids().into_iter().next()
}

fn topic_of(agent: &AgentClient, agent_id: AgentId) -> Option<rho_ui_proto::TopicId> {
    agent
        .topics()
        .into_iter()
        .find(|topic| {
            topic
                .agents
                .iter()
                .any(|summary| summary.agent_id == agent_id)
        })
        .map(|topic| topic.topic_id)
}

fn short_agent_label(agent_id: AgentId) -> String {
    format!("ag-{}", &agent_id.encoded()[..4])
}

fn primary_state(states: &HashMap<AgentId, UiAgentState>) -> Option<&UiAgentState> {
    states
        .iter()
        .min_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, state)| state)
}

fn send_prompt(agent: &AgentClient, text: String) {
    if let Some(agent_id) = primary_agent_id(agent) {
        agent.send_user_message(agent_id, text, MessageDelivery::NextRequest);
    } else {
        // New agents work where the CLI was launched; the daemon's own cwd
        // is meaningless by design.
        let Ok(working_directory) = std::env::current_dir()
            .map_err(anyhow::Error::from)
            .and_then(|cwd| Ok(camino::Utf8PathBuf::try_from(cwd)?))
        else {
            return;
        };
        agent.new_agent_with_user_message_in_topic(
            agent.default_topic_id(),
            working_directory,
            text,
        );
    }
}

/// Registered workdirs as the `(name, path)` table shared completion and
/// resolution expect.
fn workdir_table(agent: &AgentClient) -> Vec<(String, String)> {
    agent
        .workdirs()
        .into_iter()
        .map(|workdir| (workdir.name, workdir.path.into_string()))
        .collect()
}

/// Topics as the `(name, id)` pairs shared completion and resolution expect.
fn topic_labels(agent: &AgentClient) -> Vec<(String, rho_ui_proto::TopicId)> {
    agent
        .topics()
        .into_iter()
        .map(|topic| (topic.name, topic.topic_id))
        .collect()
}

/// Resolves a command's working-directory argument: a registered workdir
/// name, `~`-expanded, or relative to where the CLI was launched. `None`
/// falls back to the CLI's own cwd.
fn resolve_working_directory(
    argument: Option<camino::Utf8PathBuf>,
    workdirs: &[(String, String)],
) -> Option<camino::Utf8PathBuf> {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|cwd| camino::Utf8PathBuf::try_from(cwd).ok());
    let Some(argument) = argument else {
        return cwd;
    };
    if let Some(path) = rho_commands::resolve_workdir(argument.as_str(), workdirs) {
        return Some(path.into());
    }
    let home = || camino::Utf8PathBuf::try_from(dirs::home_dir()?).ok();
    if argument == "~" {
        return home();
    }
    if let Ok(rest) = argument.strip_prefix("~/") {
        return Some(home()?.join(rest));
    }
    Some(cwd?.join(argument))
}

async fn watch_agent(agent: &AgentClient, renderer: Option<UpdateRenderer>) {
    let changes = agent.subscribe();
    futures::pin_mut!(changes);
    let started_blocks = agent.blocks().len();
    let mut saw_running = agent.state().is_some_and(|state| {
        !matches!(
            state.status,
            UiAgentStatus::Idle | UiAgentStatus::UnfinishedTurn { .. }
        )
    });
    loop {
        let Some(states) = changes.next().await else {
            return;
        };
        let Some(state) = primary_state(&states) else {
            continue;
        };
        if let Some(renderer) = &renderer
            && let Ok(mut renderer) = renderer.lock()
        {
            renderer.handle_state(state);
        }
        if matches!(
            state.status,
            UiAgentStatus::Idle | UiAgentStatus::UnfinishedTurn { .. } | UiAgentStatus::Error
        ) {
            if saw_running || state.blocks.len() > started_blocks {
                return;
            }
        } else {
            saw_running = true;
        }
    }
}

async fn wait_idle(agent: &AgentClient) {
    watch_agent(agent, None).await;
}

/// Watch the already-started turn: when the agent goes idle, flush the
/// renderer and reset the status. The turn's outcome (including any error)
/// lands in the conversation, so there is nothing to return.
fn spawn_turn_watcher(
    agent: AgentClient,
    renderer: UpdateRenderer,
    handle: TermHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        wait_idle(&agent).await;
        if let Ok(mut renderer) = renderer.lock() {
            renderer.finish_turn();
        }
        set_status_on_handle(&handle, "/quit");
    })
}

type UpdateRenderer = Arc<Mutex<StreamingRenderer>>;

struct ChatTerm {
    term: Term,
    handle: TermHandle,
    renderer: UpdateRenderer,
    completion_menu: Option<BlockId>,
}

impl ChatTerm {
    fn new() -> io::Result<Self> {
        let (mut term, handle) = Term::new(prompt_text(), CursorShape::Bar)?;
        term.set_completion_source(Some(Box::new(|buffer: &str, cursor: usize| {
            completion::completion_candidates(buffer, cursor, &[], &[])
        })));
        handle.set_input_placeholder(dim_text("send a message"));
        handle.set_right_prompt(dim_text(":quit"));
        handle.redraw();
        let renderer = Arc::new(Mutex::new(StreamingRenderer::new(handle.clone())));
        Ok(Self {
            term,
            handle,
            renderer,
            completion_menu: None,
        })
    }

    fn renderer(&self) -> UpdateRenderer {
        Arc::clone(&self.renderer)
    }

    /// Completion over live daemon data (workdirs, known agents) once
    /// connected.
    fn set_completion_agent(&mut self, agent: AgentClient) {
        self.term
            .set_completion_source(Some(Box::new(move |buffer: &str, cursor: usize| {
                let workdirs = workdir_table(&agent);
                let topics = topic_labels(&agent)
                    .into_iter()
                    .map(|(label, _)| label)
                    .collect::<Vec<_>>();
                completion::completion_candidates(buffer, cursor, &workdirs, &topics)
            })));
    }

    fn print_user(&self, text: &str) {
        self.handle.print_output("user", user_message_block(text));
    }

    fn print_system(&self, text: &str) {
        print_system_on_handle(&self.handle, text);
    }

    fn print_help(&self) {
        let mut spans = vec![Span::new(
            "commands\n",
            Style::default().fg(Color::DarkGrey).bold(),
        )];
        let width = rho_commands::COMMANDS
            .iter()
            .map(|spec| spec.usage.len())
            .max()
            .unwrap_or(0);
        for spec in rho_commands::COMMANDS {
            spans.push(Span::plain(format!(
                "{:width$}  {}\n",
                spec.usage, spec.description
            )));
        }
        self.handle
            .print_output("help", StyledBlock::new(StyledText::from(spans)));
    }

    fn get_next_event(&mut self) -> io::Result<Event> {
        let event = self.term.get_next_event()?;
        self.sync_completion_menu();
        self.handle.redraw();
        Ok(event)
    }

    fn sync_completion_menu(&mut self) {
        match self.term.completion_state() {
            Some(view) => {
                let (width, height) = self.handle.size();
                let block = completion::render_menu_block(&view, width, height);
                let id = match self.completion_menu {
                    Some(id) => id,
                    None => {
                        let id = self.handle.new_block("completion-menu", "");
                        self.handle.push_suggestions(id);
                        self.completion_menu = Some(id);
                        id
                    }
                };
                self.handle.set_block(id, block);
            }
            None => {
                if let Some(id) = self.completion_menu.take() {
                    self.handle.remove_suggestions(id);
                    self.handle.remove_block(id);
                }
            }
        }
    }

    fn clear_output(&mut self) {
        self.completion_menu = None;
        self.handle.clear_output();
        self.handle.redraw();
    }

    fn print_history(&self, blocks: &[UiBlock]) {
        if blocks.is_empty() {
            self.print_system("rho ready");
            return;
        }
        self.print_system("loaded previous session");
        for block in blocks {
            match block {
                UiBlock::UserMessage { text } => {
                    self.handle
                        .print_output("history-message", user_message_block(text));
                }
                UiBlock::AssistantMessage { text, .. } => {
                    self.handle
                        .print_output("history-message", assistant_message_block(text));
                }
                UiBlock::Reasoning { .. } => {}
                UiBlock::Tool(tool) => {
                    self.handle.print_output(
                        "history-tool-call",
                        tool_call_block(&tool.name, &tool.arguments, tool_render_status(tool)),
                    );
                }
                UiBlock::Notice { text } => {
                    self.print_system(text);
                }
                UiBlock::QueuedMessage {
                    text,
                    delivery,
                    sender,
                } => {
                    let sender = sender.map(short_agent_label);
                    let text = queued_message_text(text, sender.as_deref());
                    match delivery_label(*delivery) {
                        Some(label) => self.print_system(&format!("{text} {label}")),
                        None => {
                            self.handle
                                .print_output("history-message", user_message_block(&text));
                        }
                    }
                }
                UiBlock::AgentMessage { sender, text } => {
                    let sender = short_agent_label(*sender);
                    self.handle.print_output(
                        "history-message",
                        user_message_block(&format!("[from {sender}] {text}")),
                    );
                }
            }
        }
    }

    fn set_status(&self, text: &str) {
        set_status_on_handle(&self.handle, text);
    }

    fn start_io_status(&self, counters: IoCounters) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            let mut tracker = UiIoTracker::new(counters.snapshot());
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(UI_IO_BUCKET_SECS)).await;
                let rates = tracker.sample(counters.snapshot());
                if rates.is_zero() {
                    continue;
                }
                set_status_on_handle(
                    &handle,
                    &format!(
                        "io ↑{} ↓{}",
                        format_io_rate(rates.sent_per_sec),
                        format_io_rate(rates.received_per_sec)
                    ),
                );
            }
        });
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct UiIoRates {
    sent_per_sec: u64,
    received_per_sec: u64,
}

impl UiIoRates {
    fn is_zero(self) -> bool {
        self.sent_per_sec == 0 && self.received_per_sec == 0
    }
}

struct UiIoTracker {
    last: rho_ui_proto::IoStats,
    buckets: VecDeque<UiIoRates>,
}

impl UiIoTracker {
    fn new(initial: rho_ui_proto::IoStats) -> Self {
        Self {
            last: initial,
            buckets: VecDeque::with_capacity(UI_IO_BUCKETS),
        }
    }

    fn sample(&mut self, current: rho_ui_proto::IoStats) -> UiIoRates {
        let rates = UiIoRates {
            sent_per_sec: current.sent.saturating_sub(self.last.sent) / UI_IO_BUCKET_SECS,
            received_per_sec: current.received.saturating_sub(self.last.received)
                / UI_IO_BUCKET_SECS,
        };
        self.last = current;
        if self.buckets.len() == UI_IO_BUCKETS {
            self.buckets.pop_front();
        }
        self.buckets.push_back(rates);
        self.rolling_max()
    }

    fn rolling_max(&self) -> UiIoRates {
        let mut max = UiIoRates::default();
        for rates in &self.buckets {
            max.sent_per_sec = max.sent_per_sec.max(rates.sent_per_sec);
            max.received_per_sec = max.received_per_sec.max(rates.received_per_sec);
        }
        max
    }
}

fn print_system_on_handle(handle: &TermHandle, text: &str) {
    handle.print_output(
        "system",
        StyledBlock::new(StyledText::from(Span::new(
            text.to_owned(),
            Style::default().fg(Color::DarkGrey),
        ))),
    );
}

fn set_status_on_handle(handle: &TermHandle, text: &str) {
    handle.set_right_prompt(dim_text(text));
    handle.redraw();
}

/// First block of the running turn: everything after the last user message.
fn turn_base_index(blocks: &[UiBlock]) -> usize {
    blocks
        .iter()
        .rposition(|block| matches!(block, UiBlock::UserMessage { .. }))
        .map_or(0, |index| index + 1)
}

fn tool_render_status(tool: &UiTool) -> ToolRenderStatus {
    match tool.status {
        UiToolStatus::Running => ToolRenderStatus::Running,
        UiToolStatus::Success => ToolRenderStatus::Done(ToolOutputStatus::Success),
        UiToolStatus::Error => ToolRenderStatus::Done(ToolOutputStatus::Error),
        UiToolStatus::Cancelled => ToolRenderStatus::Done(ToolOutputStatus::Cancelled),
    }
}

fn format_io_rate(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        return "0".to_owned();
    }
    if bytes_per_sec < 1024 {
        return format!("{bytes_per_sec}B/s");
    }
    if bytes_per_sec < 1024 * 1024 {
        return format_scaled_io_rate(bytes_per_sec, 1024, "K");
    }
    format_scaled_io_rate(bytes_per_sec, 1024 * 1024, "M")
}

fn format_scaled_io_rate(bytes_per_sec: u64, divisor: u64, suffix: &str) -> String {
    let whole = bytes_per_sec / divisor;
    let tenth = bytes_per_sec % divisor * 10 / divisor;
    if whole < 10 && tenth != 0 {
        format!("{whole}.{tenth}{suffix}/s")
    } else {
        format!("{whole}{suffix}/s")
    }
}

#[derive(Default)]
struct PlainRenderer {
    output: UpdateRenderer,
}

impl PlainRenderer {
    fn output(&self) -> UpdateRenderer {
        Arc::clone(&self.output)
    }

    fn finish(self) -> String {
        self.output
            .lock()
            .expect("renderer lock")
            .assistant_text
            .clone()
    }
}

impl Default for StreamingRenderer {
    fn default() -> Self {
        Self::plain()
    }
}

struct StreamingRenderer {
    handle: Option<TermHandle>,
    active_blocks: BTreeMap<usize, rho_cli_term_raw::BlockId>,
    assistant_text: String,
    /// Index of the first block belonging to the turn being rendered live;
    /// captured when the turn is first observed running.
    turn_base: Option<usize>,
}

impl StreamingRenderer {
    fn new(handle: TermHandle) -> Self {
        Self {
            handle: Some(handle),
            active_blocks: BTreeMap::new(),
            assistant_text: String::new(),
            turn_base: None,
        }
    }

    fn plain() -> Self {
        Self {
            handle: None,
            active_blocks: BTreeMap::new(),
            assistant_text: String::new(),
            turn_base: None,
        }
    }

    fn handle_state(&mut self, state: &UiAgentState) {
        match state.status {
            UiAgentStatus::Idle | UiAgentStatus::UnfinishedTurn { .. } => {
                if self.handle.is_some() {
                    self.finish_turn();
                }
            }
            UiAgentStatus::Streaming | UiAgentStatus::ToolCalling { .. } | UiAgentStatus::Error => {
                let turn_base = *self
                    .turn_base
                    .get_or_insert_with(|| turn_base_index(&state.blocks));
                for (index, block) in state.blocks.iter().enumerate().skip(turn_base) {
                    self.render_block(index, block);
                }
                self.order_active_blocks();
                if state.status == UiAgentStatus::Error {
                    self.finish_turn();
                }
            }
        }
    }

    fn render_block(&mut self, index: usize, block: &UiBlock) {
        match block {
            UiBlock::UserMessage { text } => {
                self.set_index_block(index, "user", user_message_block(text));
            }
            UiBlock::AssistantMessage { text, .. } => {
                self.assistant_text = text.clone();
                let block = assistant_message_block(&self.assistant_text);
                self.set_index_block(index, "assistant", block);
            }
            UiBlock::Reasoning { .. } => {}
            UiBlock::Tool(tool) => {
                let block = tool_call_block(&tool.name, &tool.arguments, tool_render_status(tool));
                self.set_index_block(index, "tool", block);
            }
            UiBlock::Notice { text } => {
                self.set_index_block(index, "notice", StyledBlock::new(dim_text(text)));
            }
            UiBlock::QueuedMessage {
                text,
                delivery,
                sender,
            } => {
                let sender = sender.map(short_agent_label);
                let text = queued_message_text(text, sender.as_deref());
                match delivery_label(*delivery) {
                    Some(label) => {
                        let queued = format!("{text} {label}");
                        self.set_index_block(index, "queued", StyledBlock::new(dim_text(&queued)));
                    }
                    None => {
                        self.set_index_block(index, "user", user_message_block(&text));
                    }
                }
            }
            UiBlock::AgentMessage { sender, text } => {
                let sender = short_agent_label(*sender);
                self.set_index_block(
                    index,
                    "user",
                    user_message_block(&format!("[from {sender}] {text}")),
                );
            }
        }
    }

    fn finish_turn(&mut self) {
        let Some(handle) = &self.handle else {
            self.reset_turn();
            return;
        };
        for (_, id) in std::mem::take(&mut self.active_blocks) {
            handle.remove_above_active(id);
            handle.push_history(id);
        }
        handle.redraw();
        self.reset_turn();
    }

    fn reset_turn(&mut self) {
        self.assistant_text.clear();
        self.turn_base = None;
    }

    fn clear(&mut self) {
        self.active_blocks.clear();
        self.reset_turn();
    }

    fn set_index_block(&mut self, index: usize, debug_id: &'static str, block: StyledBlock) {
        let Some(handle) = &self.handle else {
            return;
        };
        match self.active_blocks.get(&index).copied() {
            Some(id) => handle.set_block(id, block),
            None => {
                let id = handle.new_block(debug_id, block);
                self.active_blocks.insert(index, id);
            }
        }
        handle.redraw();
    }

    fn order_active_blocks(&self) {
        let Some(handle) = &self.handle else {
            return;
        };
        for id in self.active_blocks.values().copied() {
            handle.remove_above_active(id);
        }
        for id in self.active_blocks.values().copied() {
            handle.push_above_active(id);
        }
        handle.redraw();
    }
}

#[derive(Clone)]
struct Args {
    command: Command,
}

#[derive(Clone)]
enum Command {
    Chat(ChatArgs),
    Auth(AuthArgs),
    Voice(VoiceArgs),
    Daemon(DaemonArgs),
    Debug(DebugArgs),
    Land(LandArgs),
    McpAgentTools(McpAgentToolsArgs),
    ProtocolLog(ProtocolLogArgs),
    Slack(SlackArgs),
}

#[derive(Parser)]
#[command(name = "rho")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,

    #[command(flatten)]
    chat: ChatArgs,
}

#[derive(Subcommand)]
enum CliCommand {
    Auth {
        #[command(subcommand)]
        command: AuthArgs,
    },
    Voice {
        #[command(subcommand)]
        command: VoiceArgs,
    },
    Daemon(DaemonArgs),
    Debug(DebugArgs),
    Land(LandArgs),
    McpAgentTools(McpAgentToolsArgs),
    ProtocolLog(ProtocolLogArgs),
    Slack(SlackArgs),
}

#[derive(Clone, clap::Args)]
pub(crate) struct SlackArgs {
    #[arg(long = "auth", default_value = "default")]
    auth: String,
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: SlackCommand,
}

#[derive(Clone, Subcommand)]
pub(crate) enum SlackCommand {
    /// Install Slack tokens (read from stdin) and connect to Slack.
    Init,
}

#[derive(Clone, clap::Args)]
pub(crate) struct McpAgentToolsArgs {
    #[arg(long = "agent-id")]
    agent_id: Option<String>,
    #[arg(long = "auth", default_value = "default")]
    auth: String,
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
}

#[derive(Clone, clap::Args)]
struct ChatArgs {
    #[arg(long = "auth", default_value = "default")]
    auth: String,
    #[arg(long = "prompt-stdin")]
    prompt_stdin: bool,
}

#[derive(Clone, clap::Args)]
pub(crate) struct LandArgs {
    #[arg(long = "auth", default_value = "default")]
    auth: String,
    /// Checkout path to land from (defaults to the current directory).
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
}

#[derive(Clone, clap::Args)]
struct ProtocolLogArgs {
    path: std::path::PathBuf,
}

impl Args {
    fn parse_or_exit(args: impl Iterator<Item = String>) -> Self {
        Self::try_parse(args).unwrap_or_else(|error| error.exit())
    }

    fn try_parse(args: impl Iterator<Item = String>) -> std::result::Result<Self, clap::Error> {
        let cli = Cli::try_parse_from(std::iter::once("rho".to_owned()).chain(args))?;
        let command = match cli.command {
            Some(CliCommand::Auth { command }) => Command::Auth(command),
            Some(CliCommand::Voice { command }) => Command::Voice(command),
            Some(CliCommand::Daemon(args)) => Command::Daemon(args),
            Some(CliCommand::Debug(args)) => Command::Debug(args),
            Some(CliCommand::Land(args)) => Command::Land(args),
            Some(CliCommand::McpAgentTools(args)) => Command::McpAgentTools(args),
            Some(CliCommand::ProtocolLog(args)) => Command::ProtocolLog(args),
            Some(CliCommand::Slack(args)) => Command::Slack(args),
            None => Command::Chat(cli.chat),
        };
        Ok(Self { command })
    }
}

fn prompt_text() -> StyledText {
    StyledText::from("> ")
}

fn user_message_block(text: &str) -> StyledBlock {
    let style = Style::default().fg(Color::Green);
    StyledBlock::new(StyledText::from(vec![
        Span::new("▌ ", style.bold()),
        Span::new(text.to_owned(), style),
    ]))
}

fn assistant_message_block(text: &str) -> StyledBlock {
    markdown_block(text)
}

fn delivery_label(delivery: MessageDelivery) -> Option<&'static str> {
    match delivery {
        MessageDelivery::Immediate => None,
        MessageDelivery::NextRequest => Some("(steering)"),
        MessageDelivery::NextTurn => Some("(queued)"),
    }
}

/// Queued mail carries its sender inline so the queue rendering shows who
/// is waiting to be heard.
fn queued_message_text(text: &str, sender: Option<&str>) -> String {
    match sender {
        Some(sender) => format!("[from {sender}] {text}"),
        None => text.to_owned(),
    }
}

fn dim_text(text: &str) -> StyledText {
    StyledText::from(Span::new(
        text.to_owned(),
        Style::default().fg(Color::DarkGrey),
    ))
}
