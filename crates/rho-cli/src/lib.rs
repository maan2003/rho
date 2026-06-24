//! Runnable terminal UI for the opinionated rho agent harness.
//!
//! This crate deliberately assembles concrete rho building blocks instead of
//! defining a reusable CLI framework: `rho-agent` owns the harness loop,
//! `rho-inference` owns inference transport, and
//! `rho-tool-shell` owns the built-in shell/apply_patch tools. Fork this crate
//! when the desired user experience diverges.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rho_agent::{Agent, AgentStatus, AgentStore, AgentTools, AgentUpdate};
use rho_cli_term_raw::{
    Color, CursorShape, Event, Span, Style, StyledBlock, StyledText, Term, TermHandle,
};
use rho_core::{
    InferenceUpdate, ItemBlock, ItemKind, ReasoningTextKind, Role, ToolCall, ToolResult,
    ToolResultStatus,
};
use rho_inference::{AuthArgs, InferenceAuth, InferenceSession, run_auth_cli};
use rho_store_cbor::CborLog;
use rho_tool_shell::ShellTools;
use tokio::task::JoinHandle;

#[cfg(test)]
mod tests;

const DEFAULT_SESSION_NAME: &str = "default";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_COMPACTION_THRESHOLD: u64 = 220_000;

pub fn main() -> Result<()> {
    let args = Args::parse_or_exit(std::env::args().skip(1));
    let runtime = tokio::runtime::Builder::new_multi_thread()
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
    }
}

async fn run_interactive(args: ChatArgs) -> Result<()> {
    let term = ChatTerm::new()?;
    let agent = build_agent(&args, Some(term.renderer())).await?;
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
    let agent = build_agent(&args, Some(output)).await?;
    agent.send(prompt);
    wait_idle(&agent).await;

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

async fn build_agent(args: &ChatArgs, renderer: Option<UpdateRenderer>) -> Result<Agent> {
    let inference = Box::new(build_inference_session(args)?);
    let tools = vec![AgentTools::Shell(ShellTools::new(DEFAULT_TOOL_TIMEOUT))];
    let mut builder = Agent::builder(inference, tools);
    if !args.no_store {
        let store = AgentStore::CborLog(CborLog::new(args.session_path()?));
        builder = builder.with_store_loaded(store).await?;
    }
    let agent = builder.spawn();
    if let Some(renderer) = renderer {
        let (_, mut inference_updates) = agent.subscribe_inference_updates();
        let inference_renderer = Arc::clone(&renderer);
        tokio::spawn(async move {
            while let Ok(update) = inference_updates.recv().await {
                if let Ok(mut renderer) = inference_renderer.lock() {
                    renderer.handle_inference(update);
                }
            }
        });
        let (_, mut agent_updates) = agent.subscribe_agent_updates();
        tokio::spawn(async move {
            while let Ok(update) = agent_updates.recv().await {
                if let Ok(mut renderer) = renderer.lock() {
                    renderer.handle_agent(update);
                }
            }
        });
    }
    Ok(agent)
}

fn build_inference_session(args: &ChatArgs) -> Result<InferenceSession> {
    let auth = InferenceAuth::named(&args.auth)?;
    let mut session = InferenceSession::new(args.model.clone(), auth)
        .with_compaction_threshold(DEFAULT_COMPACTION_THRESHOLD);
    if !args.no_store {
        session = session.with_prompt_cache_key(args.session.clone());
    }
    Ok(session)
}

struct ChatApp {
    agent: Agent,
    running_turn: Option<JoinHandle<()>>,
    term: ChatTerm,
}

impl ChatApp {
    async fn run(&mut self) -> Result<()> {
        loop {
            self.reap_finished_turn().await;
            match self.term.term.get_next_event()? {
                Event::Line(line) => {
                    let line = line.trim().to_owned();
                    if line.is_empty() {
                        continue;
                    }
                    if matches!(line.as_str(), "/quit" | "/exit") {
                        self.stop_running_turn(false).await;
                        break;
                    }
                    if self.turn_is_running() {
                        self.term
                            .print_system("agent is running; press Ctrl-C twice to cancel");
                        continue;
                    }
                    self.term.print_user(&line);
                    self.term.set_status("running");
                    self.agent.send(line);
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

async fn wait_idle(agent: &Agent) {
    let (_, mut changes) = agent.subscribe_status();
    let mut saw_running = false;
    while let Ok(status) = changes.recv().await {
        match status {
            AgentStatus::Running => saw_running = true,
            AgentStatus::Idle if saw_running => return,
            AgentStatus::Idle => {}
        }
    }
}

/// Watch the already-started turn: when the agent goes idle, flush the
/// renderer and reset the status. The turn's outcome (including any error)
/// lands in the conversation, so there is nothing to return.
fn spawn_turn_watcher(
    agent: Agent,
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
}

impl ChatTerm {
    fn new() -> io::Result<Self> {
        let (term, handle) = Term::new(prompt_text(), CursorShape::Bar)?;
        handle.set_input_placeholder(dim_text("send a message"));
        handle.set_right_prompt(dim_text("/quit"));
        handle.redraw();
        let renderer = Arc::new(Mutex::new(StreamingRenderer::new(handle.clone())));
        Ok(Self {
            term,
            handle,
            renderer,
        })
    }

    fn renderer(&self) -> UpdateRenderer {
        Arc::clone(&self.renderer)
    }

    fn print_user(&self, text: &str) {
        self.handle.print_output(
            "user",
            StyledBlock::new(StyledText::from(vec![
                Span::new("you\n", Style::default().fg(Color::DarkCyan).bold()),
                Span::plain(text.to_owned()),
            ]))
            .margin_left(1),
        );
    }

    fn print_system(&self, text: &str) {
        print_system_on_handle(&self.handle, text);
    }

    fn print_history(&self, blocks: &[ItemBlock]) {
        if blocks.is_empty() {
            self.print_system("rho ready");
            return;
        }
        self.print_system("loaded previous session");
        for block in blocks {
            for item in match block {
                ItemBlock::Local { items } | ItemBlock::InferenceResponse { items, .. } => items,
            } {
                match &item.kind {
                    ItemKind::Message(message) => {
                        self.handle.print_output(
                            "history-message",
                            message_block(role_label(message.role), message.text_content()),
                        );
                    }
                    ItemKind::ToolCall(call) => {
                        self.handle.print_output(
                            "history-tool-call",
                            StyledBlock::new(StyledText::from(vec![
                                Span::new("tool ", Style::default().fg(Color::DarkMagenta).bold()),
                                Span::new(
                                    call.name.clone(),
                                    Style::default().fg(Color::DarkMagenta),
                                ),
                            ]))
                            .margin_left(1),
                        );
                    }
                    ItemKind::ToolResult(result) => {
                        self.handle.print_output(
                            "history-tool-result",
                            StyledBlock::new(StyledText::from(vec![
                                Span::new(
                                    "tool result ",
                                    Style::default().fg(Color::DarkMagenta).bold(),
                                ),
                                Span::new(
                                    tool_status_label(&result.status),
                                    Style::default().fg(Color::DarkGrey),
                                ),
                            ]))
                            .margin_left(1),
                        );
                    }
                    ItemKind::ReasoningText(_) | ItemKind::ProviderItem(_) => {}
                }
            }
        }
    }

    fn set_status(&self, text: &str) {
        set_status_on_handle(&self.handle, text);
    }
}

fn print_system_on_handle(handle: &TermHandle, text: &str) {
    handle.print_output(
        "system",
        StyledBlock::new(StyledText::from(Span::new(
            text.to_owned(),
            Style::default().fg(Color::DarkGrey),
        )))
        .margin_left(1),
    );
}

fn set_status_on_handle(handle: &TermHandle, text: &str) {
    handle.set_right_prompt(dim_text(text));
    handle.redraw();
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
    assistant_block: Option<rho_cli_term_raw::BlockId>,
    thinking_block: Option<rho_cli_term_raw::BlockId>,
    tool_blocks: BTreeMap<String, rho_cli_term_raw::BlockId>,
    assistant_text: String,
    thinking_text: String,
}

impl StreamingRenderer {
    fn new(handle: TermHandle) -> Self {
        Self {
            handle: Some(handle),
            assistant_block: None,
            thinking_block: None,
            tool_blocks: BTreeMap::new(),
            assistant_text: String::new(),
            thinking_text: String::new(),
        }
    }

    fn plain() -> Self {
        Self {
            handle: None,
            assistant_block: None,
            thinking_block: None,
            tool_blocks: BTreeMap::new(),
            assistant_text: String::new(),
            thinking_text: String::new(),
        }
    }

    fn handle_inference(&mut self, update: InferenceUpdate) {
        match update {
            InferenceUpdate::TextDelta { text, .. } => {
                self.assistant_text.push_str(&text);
                self.render_assistant();
            }
            InferenceUpdate::ReasoningTextDelta { kind, text, .. } => {
                if kind == ReasoningTextKind::Summary {
                    self.thinking_text.push_str(&text);
                    self.render_thinking();
                }
            }
            InferenceUpdate::ToolCall { call, .. } => self.render_tool_call(&call),
            InferenceUpdate::OutputItem { item, .. } => {
                if self.assistant_text.is_empty()
                    && let ItemKind::Message(message) = item
                {
                    self.assistant_text.push_str(&message.text_content());
                    self.render_assistant();
                }
            }
            InferenceUpdate::CompactionStarted { .. } => self.render_notice("compacting context"),
            InferenceUpdate::Usage(_) | InferenceUpdate::ResponseId(_) => {}
            InferenceUpdate::Finished(response) => {
                let requests_tool_calls = response
                    .items
                    .iter()
                    .any(|item| matches!(item, ItemKind::ToolCall(_)));
                if self.assistant_text.is_empty() {
                    for item in response.items {
                        if let ItemKind::Message(message) = item {
                            self.assistant_text.push_str(&message.text_content());
                        }
                    }
                    self.render_assistant();
                }
                if self.handle.is_some() && !requests_tool_calls {
                    self.finish_turn();
                }
            }
        }
    }

    fn handle_agent(&mut self, update: AgentUpdate) {
        match update {
            AgentUpdate::ToolCallStarted(call) => self.render_tool_started(&call),
            AgentUpdate::ToolCallFinished(result) => self.render_tool_finished(&result),
        }
    }

    fn finish_turn(&mut self) {
        let Some(handle) = &self.handle else {
            self.reset_turn();
            return;
        };
        if let Some(id) = self.assistant_block.take() {
            handle.remove_above_active(id);
            handle.push_history(id);
        }
        if let Some(id) = self.thinking_block.take() {
            handle.remove_above_active(id);
            handle.push_history(id);
        }
        for (_, id) in std::mem::take(&mut self.tool_blocks) {
            handle.remove_above_active(id);
            handle.push_history(id);
        }
        handle.redraw();
        self.reset_turn();
    }

    fn reset_turn(&mut self) {
        self.assistant_text.clear();
        self.thinking_text.clear();
    }

    fn render_assistant(&mut self) {
        let block = StyledBlock::new(StyledText::from(vec![
            Span::new("assistant\n", Style::default().fg(Color::Green).bold()),
            Span::plain(self.assistant_text.clone()),
        ]))
        .margin_left(1);
        self.set_active_block(ActiveBlock::Assistant, "assistant", block);
    }

    fn render_thinking(&mut self) {
        let block = StyledBlock::new(StyledText::from(vec![
            Span::new("thinking\n", Style::default().fg(Color::DarkYellow).bold()),
            Span::new(
                self.thinking_text.clone(),
                Style::default().fg(Color::DarkGrey),
            ),
        ]))
        .margin_left(1);
        self.set_active_block(ActiveBlock::Thinking, "thinking", block);
    }

    fn render_tool_call(&mut self, call: &ToolCall) {
        let block = StyledBlock::new(StyledText::from(vec![
            Span::new("tool ", Style::default().fg(Color::DarkMagenta).bold()),
            Span::new(call.name.clone(), Style::default().fg(Color::DarkMagenta)),
            Span::new(" requested", Style::default().fg(Color::DarkGrey)),
            Span::plain("\n"),
            Span::plain(call.arguments.to_string()),
        ]))
        .margin_left(1);
        self.set_tool_block(call.id.0.clone(), block);
    }

    fn render_tool_started(&mut self, call: &ToolCall) {
        let block = StyledBlock::new(StyledText::from(vec![
            Span::new("tool ", Style::default().fg(Color::DarkMagenta).bold()),
            Span::new(call.name.clone(), Style::default().fg(Color::DarkMagenta)),
            Span::new(" running", Style::default().fg(Color::DarkYellow)),
            Span::plain("\n"),
            Span::plain(call.arguments.to_string()),
        ]))
        .margin_left(1);
        self.set_tool_block(call.id.0.clone(), block);
    }

    fn render_tool_finished(&mut self, result: &ToolResult) {
        let status = tool_status_label(&result.status);
        let output = truncate_display(&result.rendered_output(), 4_000);
        let block = StyledBlock::new(StyledText::from(vec![
            Span::new(
                "tool result ",
                Style::default().fg(Color::DarkMagenta).bold(),
            ),
            Span::new(
                status,
                Style::default().fg(tool_status_color(&result.status)),
            ),
            Span::plain("\n"),
            Span::plain(output),
        ]))
        .margin_left(1);
        self.set_tool_block(result.call_id.0.clone(), block);
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
            )))
            .margin_left(1),
        );
    }

    fn set_active_block(&mut self, which: ActiveBlock, debug_id: &'static str, block: StyledBlock) {
        let Some(handle) = &self.handle else {
            return;
        };
        let slot = match which {
            ActiveBlock::Assistant => &mut self.assistant_block,
            ActiveBlock::Thinking => &mut self.thinking_block,
        };
        match *slot {
            Some(id) => handle.set_block(id, block),
            None => {
                let id = handle.new_block(debug_id, block);
                handle.push_above_active(id);
                *slot = Some(id);
            }
        }
        handle.redraw();
    }

    fn set_tool_block(&mut self, call_id: String, block: StyledBlock) {
        let Some(handle) = &self.handle else {
            return;
        };
        match self.tool_blocks.get(&call_id).copied() {
            Some(id) => handle.set_block(id, block),
            None => {
                let id = handle.new_block("tool", block);
                handle.push_above_active(id);
                self.tool_blocks.insert(call_id, id);
            }
        }
        handle.redraw();
    }
}

enum ActiveBlock {
    Assistant,
    Thinking,
}

#[derive(Clone)]
struct Args {
    command: Command,
}

#[derive(Clone)]
enum Command {
    Chat(ChatArgs),
    Auth(AuthArgs),
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
}

#[derive(Clone, clap::Args)]
struct ChatArgs {
    #[arg(long, default_value_t = InferenceSession::DEFAULT_MODEL.to_owned())]
    model: String,
    #[arg(long = "auth", default_value = "default")]
    auth: String,
    #[arg(long, default_value = DEFAULT_SESSION_NAME)]
    session: String,
    #[arg(long = "session-path")]
    session_path: Option<PathBuf>,
    #[arg(long = "prompt-stdin")]
    prompt_stdin: bool,
    #[arg(long = "no-store")]
    no_store: bool,
}

impl Args {
    fn parse_or_exit(args: impl Iterator<Item = String>) -> Self {
        Self::try_parse(args).unwrap_or_else(|error| error.exit())
    }

    fn try_parse(args: impl Iterator<Item = String>) -> std::result::Result<Self, clap::Error> {
        let cli = Cli::try_parse_from(std::iter::once("rho".to_owned()).chain(args))?;
        let command = match cli.command {
            Some(CliCommand::Auth { command }) => Command::Auth(command),
            None => Command::Chat(cli.chat),
        };
        let args = Self { command };
        args.validate()?;
        Ok(args)
    }

    fn validate(&self) -> std::result::Result<(), clap::Error> {
        if let Command::Chat(chat) = &self.command {
            if chat.no_store && chat.session_path.is_some() {
                return Err(clap::Error::raw(
                    clap::error::ErrorKind::ArgumentConflict,
                    "--no-store cannot be used with --session-path",
                ));
            }
        }
        Ok(())
    }
}

impl ChatArgs {
    fn session_path(&self) -> Result<PathBuf> {
        if let Some(path) = &self.session_path {
            return Ok(path.clone());
        }
        let state_dir = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .context("cannot determine state directory")?;
        Ok(state_dir
            .join("rho")
            .join("sessions")
            .join(format!("{}.cbor", self.session)))
    }
}

fn prompt_text() -> StyledText {
    StyledText::from(vec![
        Span::new("rho", Style::default().fg(Color::Green).bold()),
        Span::plain("> "),
    ])
}

fn message_block(label: &'static str, text: String) -> StyledBlock {
    StyledBlock::new(StyledText::from(vec![
        Span::new(
            format!("{label}\n"),
            Style::default().fg(role_color(label)).bold(),
        ),
        Span::plain(text),
    ]))
    .margin_left(1)
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "you",
        Role::Assistant => "assistant",
    }
}

fn role_color(label: &str) -> Color {
    match label {
        "you" => Color::DarkCyan,
        "assistant" => Color::Green,
        "system" | "developer" => Color::DarkGrey,
        _ => Color::White,
    }
}

fn tool_status_label(status: &ToolResultStatus) -> String {
    match status {
        ToolResultStatus::Success => "success".to_owned(),
        ToolResultStatus::Error { message } => format!("error: {message}"),
        ToolResultStatus::Cancelled { reason } => format!("cancelled: {reason}"),
    }
}

fn tool_status_color(status: &ToolResultStatus) -> Color {
    match status {
        ToolResultStatus::Success => Color::Green,
        ToolResultStatus::Error { .. } => Color::DarkRed,
        ToolResultStatus::Cancelled { .. } => Color::DarkYellow,
    }
}

fn truncate_display(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut output = text.chars().take(max_chars).collect::<String>();
    output.push_str("\n... truncated");
    output
}

fn dim_text(text: &str) -> StyledText {
    StyledText::from(Span::new(
        text.to_owned(),
        Style::default().fg(Color::DarkGrey),
    ))
}
