//! Runnable terminal UI for the opinionated rho agent harness.
//!
//! This crate deliberately assembles concrete rho building blocks instead of
//! defining a reusable CLI framework: `rho-agent` owns the harness loop,
//! `rho-inference` owns inference transport, and
//! `rho-tool-shell` owns the built-in shell/apply_patch tools. Fork this crate
//! when the desired user experience diverges.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use rho_cli_term_raw::{
    BlockId, Color, CursorShape, Event, Span, Style, StyledBlock, StyledText, Term, TermHandle,
};
use rho_core::ToolOutputStatus;
use rho_daemon::{DaemonArgs, default_socket_path};
use rho_inference::{AuthArgs, run_auth_cli};
use rho_ui_proto::IoCounters;
use rho_ui_proto::client::{AgentClient, Client as UiClient};
use rho_ui_proto::remote::{
    UiAgentState, UiAgentStatus, UiBlock, UiStreamingItem, UiToolResult, UiToolStatus,
};
use tokio::task::JoinHandle;

mod completion;
mod markdown;
mod slash_commands;
mod tool_render;

#[cfg(test)]
mod tests;

use completion::completion_candidates;
use markdown::markdown_block;
use slash_commands::SlashCommand;
use tool_render::{ToolRenderStatus, tool_call_block};

const UI_IO_BUCKET_SECS: u64 = 1;
const UI_IO_WINDOW_SECS: u64 = 30;
const UI_IO_BUCKETS: usize = (UI_IO_WINDOW_SECS / UI_IO_BUCKET_SECS) as usize;

pub fn main() -> Result<()> {
    let args = Args::parse_or_exit(std::env::args().skip(1));
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
        Command::Daemon(args) => rho_daemon::run(args).await,
        Command::ProtocolLog(args) => {
            let mut stdout = io::stdout().lock();
            rho_ui_proto::print_protocol_log(&args.path, &mut stdout)?;
            Ok(())
        }
    }
}

async fn run_interactive(args: ChatArgs) -> Result<()> {
    let term = ChatTerm::new()?;
    let agent = build_agent(&args, Some(term.renderer())).await?;
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
    agent.send_user_message(prompt);
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
        AgentClient::connect_client(connect_or_start_daemon(&socket_path, args).await?).await?;
    if renderer.is_some() {
        let changes = client.subscribe();
        tokio::spawn(async move {
            futures::pin_mut!(changes);
            while let Some(state) = changes.next().await {
                if let Some(renderer) = &renderer
                    && let Ok(mut renderer) = renderer.lock()
                {
                    renderer.handle_state(&state);
                }
            }
        });
    }
    Ok(client)
}

async fn connect_or_start_daemon(
    socket_path: &std::path::Path,
    args: &ChatArgs,
) -> Result<UiClient> {
    if let Ok(client) = UiClient::connect(socket_path).await {
        return Ok(client);
    }

    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("--auth")
        .arg(&args.auth)
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
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error.into()),
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
                    if matches!(line.as_str(), "/quit" | "/exit") {
                        self.stop_running_turn(false).await;
                        break;
                    }
                    if line == "/detach" {
                        self.stop_running_turn(false).await;
                        self.term
                            .print_system("detach is not available in rho; exiting chat");
                        break;
                    }
                    if self.handle_slash_command(&line).await? {
                        continue;
                    }
                    if self.turn_is_running() {
                        self.term
                            .print_system("agent is running; press Ctrl-C twice to cancel");
                        continue;
                    }
                    self.term.print_user(&line);
                    self.term.set_status("running");
                    self.agent.send_user_message(line);
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

    async fn handle_slash_command(&mut self, line: &str) -> Result<bool> {
        let Some(command) = SlashCommand::parse(line) else {
            return Ok(false);
        };
        match command {
            SlashCommand::Quit => unreachable!("handled before slash dispatch"),
            SlashCommand::Cancel => {
                self.cancel_running_turn().await;
            }
            SlashCommand::Clear => {
                self.term.clear_output();
                if let Ok(mut renderer) = self.term.renderer.lock() {
                    renderer.clear();
                }
                self.term.print_system("cleared");
            }
            SlashCommand::Help => self.term.print_help(),
            SlashCommand::Version => self.term.print_system(env!("CARGO_PKG_VERSION")),
            SlashCommand::Unsupported(command) => self.term.print_system(&format!(
                "`{command}` is part of Tau's CLI surface but is not available in rho yet"
            )),
            SlashCommand::Unknown(command) => self
                .term
                .print_system(&format!("unknown command `{command}`; try /help")),
        }
        Ok(true)
    }

    fn turn_is_running(&self) -> bool {
        self.running_turn
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
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
        self.agent.cancel();
        let _ = handle.await;
        if print_cancelled {
            self.term.print_system("cancelled");
        }
        self.term.set_status("/quit");
    }
}

async fn watch_agent(agent: &AgentClient, renderer: Option<UpdateRenderer>) {
    let changes = agent.subscribe();
    futures::pin_mut!(changes);
    let started_blocks = agent.blocks().len();
    let mut saw_running = !matches!(
        agent.state().status,
        UiAgentStatus::Idle | UiAgentStatus::UnfinishedTurn { .. }
    );
    loop {
        let Some(state) = changes.next().await else {
            return;
        };
        if let Some(renderer) = &renderer
            && let Ok(mut renderer) = renderer.lock()
        {
            renderer.handle_state(&state);
        }
        if matches!(
            state.status,
            UiAgentStatus::Idle
                | UiAgentStatus::UnfinishedTurn { .. }
                | UiAgentStatus::Error { .. }
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
        term.set_completion_source(Some(Box::new(completion_candidates)));
        handle.set_input_placeholder(dim_text("send a message"));
        handle.set_right_prompt(dim_text("/quit"));
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

    fn print_user(&self, text: &str) {
        self.handle.print_output("user", user_message_block(text));
    }

    fn print_system(&self, text: &str) {
        print_system_on_handle(&self.handle, text);
    }

    fn print_help(&self) {
        self.handle.print_output(
            "help",
            StyledBlock::new(StyledText::from(vec![
                Span::new("commands\n", Style::default().fg(Color::DarkGrey).bold()),
                Span::plain("/quit          exit chat\n"),
                Span::plain("/cancel        cancel the current in-flight prompt\n"),
                Span::plain("/detach        exit chat (rho has no daemon detach yet)\n"),
                Span::plain("/version       show rho version\n"),
                Span::plain("/help          show this help\n"),
                Span::plain("/clear         clear rendered output\n"),
                Span::plain(
                    "\nTau-compatible commands complete but may report unavailable in rho.\n",
                ),
            ])),
        );
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
                UiBlock::AssistantMessage { text } => {
                    self.handle
                        .print_output("history-message", assistant_message_block(text));
                }
                UiBlock::Reasoning { .. } => {}
                UiBlock::ToolCall {
                    name,
                    arguments,
                    status,
                    ..
                } => {
                    self.handle.print_output(
                        "history-tool-call",
                        tool_call_block(
                            name,
                            arguments,
                            ToolRenderStatus::Done(tool_output_status(*status)),
                        ),
                    );
                }
                UiBlock::Notice { text } => {
                    self.print_system(text);
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

fn tool_output_status(status: UiToolStatus) -> ToolOutputStatus {
    match status {
        UiToolStatus::Running | UiToolStatus::Success => ToolOutputStatus::Success,
        UiToolStatus::Error => ToolOutputStatus::Error,
        UiToolStatus::Cancelled => ToolOutputStatus::Cancelled,
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
    tool_calls: BTreeMap<String, (String, String)>,
    tool_call_indices: BTreeMap<String, usize>,
    assistant_text: String,
    stream_base_index: usize,
}

impl StreamingRenderer {
    fn new(handle: TermHandle) -> Self {
        Self {
            handle: Some(handle),
            active_blocks: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
            tool_call_indices: BTreeMap::new(),
            assistant_text: String::new(),
            stream_base_index: 0,
        }
    }

    fn plain() -> Self {
        Self {
            handle: None,
            active_blocks: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
            tool_call_indices: BTreeMap::new(),
            assistant_text: String::new(),
            stream_base_index: 0,
        }
    }

    fn handle_state(&mut self, state: &UiAgentState) {
        match &state.status {
            UiAgentStatus::Streaming => {
                for (index, item) in state.pending_response.iter().enumerate() {
                    self.render_streaming_item(self.stream_base_index + index, item);
                }
                self.order_active_blocks();
            }
            UiAgentStatus::ToolCalling { results } => {
                self.stream_base_index = self
                    .active_blocks
                    .keys()
                    .next_back()
                    .map_or(0, |index| index + 1);
                for result in results {
                    self.render_tool_finished(result);
                }
                self.order_active_blocks();
            }
            UiAgentStatus::Error { message } => {
                self.render_notice(&format!("agent error: {message}"))
            }
            UiAgentStatus::Idle | UiAgentStatus::UnfinishedTurn { .. } => {
                if self.handle.is_some() {
                    self.finish_turn();
                }
            }
        }
    }

    fn render_streaming_item(&mut self, index: usize, item: &UiStreamingItem) {
        match item {
            UiStreamingItem::AssistantMessage { text } => {
                self.assistant_text = text.clone();
                let block = assistant_message_block(&self.assistant_text);
                self.set_index_block(index, "assistant", block);
            }
            UiStreamingItem::Reasoning { text } => {
                let block = StyledBlock::new(StyledText::from(vec![
                    Span::new("thinking\n", Style::default().fg(Color::DarkYellow).bold()),
                    Span::new(text.clone(), Style::default().fg(Color::DarkGrey)),
                ]));
                self.set_index_block(index, "thinking", block);
            }
            UiStreamingItem::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.tool_calls
                    .insert(id.clone(), (name.clone(), arguments.clone()));
                self.tool_call_indices.insert(id.clone(), index);
                let block = tool_call_block(name, arguments, ToolRenderStatus::Running);
                self.set_index_block(index, "tool", block);
            }
            UiStreamingItem::Notice { text } => {
                self.render_notice(text);
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
        self.tool_calls.clear();
        self.tool_call_indices.clear();
        handle.redraw();
        self.reset_turn();
    }

    fn reset_turn(&mut self) {
        self.assistant_text.clear();
        self.stream_base_index = 0;
    }

    fn clear(&mut self) {
        self.active_blocks.clear();
        self.tool_calls.clear();
        self.tool_call_indices.clear();
        self.reset_turn();
    }

    fn render_tool_finished(&mut self, result: &UiToolResult) {
        let Some(index) = self.tool_call_indices.get(&result.call_id).copied() else {
            return;
        };
        let (name, arguments) = self
            .tool_calls
            .get(&result.call_id)
            .cloned()
            .unwrap_or_else(|| ("tool".to_owned(), String::new()));
        let block = tool_call_block(
            &name,
            &arguments,
            ToolRenderStatus::Done(tool_output_status(result.status)),
        );
        self.set_index_block(index, "tool", block);
    }

    fn render_notice(&mut self, text: &str) {
        let Some(handle) = &self.handle else {
            return;
        };
        handle.print_output(
            "notice",
            StyledBlock::new(StyledText::from(Span::new(
                text.to_owned(),
                Style::default().fg(Color::DarkGrey),
            ))),
        );
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
    Daemon(DaemonArgs),
    ProtocolLog(ProtocolLogArgs),
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
    Daemon(DaemonArgs),
    ProtocolLog(ProtocolLogArgs),
}

#[derive(Clone, clap::Args)]
struct ChatArgs {
    #[arg(long = "auth", default_value = "default")]
    auth: String,
    #[arg(long = "prompt-stdin")]
    prompt_stdin: bool,
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
            Some(CliCommand::Daemon(args)) => Command::Daemon(args),
            Some(CliCommand::ProtocolLog(args)) => Command::ProtocolLog(args),
            None => Command::Chat(cli.chat),
        };
        Ok(Self { command })
    }
}

fn prompt_text() -> StyledText {
    StyledText::from("> ")
}

fn user_message_block(text: &str) -> StyledBlock {
    StyledBlock::new(StyledText::from(vec![
        Span::new("▌ ", Style::default().bold()),
        Span::plain(text.to_owned()),
    ]))
}

fn assistant_message_block(text: &str) -> StyledBlock {
    markdown_block(text)
}

fn dim_text(text: &str) -> StyledText {
    StyledText::from(Span::new(
        text.to_owned(),
        Style::default().fg(Color::DarkGrey),
    ))
}
