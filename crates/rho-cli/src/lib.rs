//! Runnable terminal UI for the opinionated rho agent harness.
//!
//! This crate deliberately assembles concrete rho building blocks instead of
//! defining a reusable CLI framework: `rho-agent` owns the harness loop,
//! `rho-provider-responses` owns ChatGPT/Codex Responses transport, and
//! `rho-tool-shell` owns the built-in shell/apply_patch tools. Fork this crate
//! when the desired user experience diverges.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rho::{ItemBlock, ItemKind, ReasoningTextKind, Role, ToolCall, ToolResult, ToolResultStatus};
use rho_agent::{Agent, AgentProvider, AgentStore, AgentTools, AgentUpdate};
use rho_cli_term_raw::{
    Color, CursorShape, Event, Span, Style, StyledBlock, StyledText, Term, TermHandle,
};
use rho_provider_responses::oauth::{
    oauth_token_should_refresh, openai_codex_auth_url, openai_codex_exchange, parse_redirect_url,
};
use rho_provider_responses::{
    DEFAULT_MODEL, OAuthFile, ProviderSession, ResponsesAuth, ResponsesOAuthCredentials,
    ResponsesUpdate,
};
use rho_store_cbor::CborLog;
use rho_tool_shell::ShellTools;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

#[cfg(test)]
mod tests;

const DEFAULT_SESSION_NAME: &str = "default";
const DEFAULT_AUTH_NAME: &str = "default";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_AGENT_STEPS_PER_PROMPT: usize = 128;
const DEFAULT_COMPACTION_THRESHOLD: u64 = 220_000;

pub fn main() -> Result<()> {
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
    {
        print!("{HELP}");
        return Ok(());
    }
    let args = Args::parse(std::env::args().skip(1))?;
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
        Command::Auth(auth) => run_auth(auth).await,
        Command::Provider(provider) => run_provider(provider).await,
    }
}

async fn run_provider(command: ProviderCommand) -> Result<()> {
    match command {
        ProviderCommand::Add => {
            let name = prompt_with_default("Provider namespace", DEFAULT_AUTH_NAME)?;
            let credentials = login_openai_codex()?;
            let file = OAuthFile::open_default(name.trim())?;
            file.save(&credentials)?;
            println!("{}", auth_status_line(&file.path(), Some(&credentials)));
            Ok(())
        }
        ProviderCommand::List => list_providers(),
        ProviderCommand::Remove { name } => {
            let file = OAuthFile::open_default(name.trim())?;
            if file.delete()? {
                println!("removed {}", file.path().display());
            } else {
                println!("missing {}", file.path().display());
            }
            Ok(())
        }
    }
}

async fn run_auth(command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Path { name } => {
            let file = OAuthFile::open_default(name)?;
            println!("{}", file.path().display());
            Ok(())
        }
        AuthCommand::Status { name } => {
            let file = OAuthFile::open_default(name)?;
            println!("{}", auth_status_line(&file.path(), file.load()?.as_ref()));
            Ok(())
        }
        AuthCommand::Import { name, path } => {
            let credentials = read_oauth_credentials(path)?;
            let file = OAuthFile::open_default(name)?;
            file.save(&credentials)?;
            println!("{}", auth_status_line(&file.path(), Some(&credentials)));
            Ok(())
        }
    }
}

fn login_openai_codex() -> Result<ResponsesOAuthCredentials> {
    let (auth_url, expected_state, verifier) = openai_codex_auth_url();

    eprintln!();
    eprintln!("Open this URL in your browser:");
    eprintln!();
    eprintln!("{auth_url}");
    eprintln!("\x1b]8;;{auth_url}\x1b\\Or click here.\x1b]8;;\x1b\\");
    eprintln!();
    eprintln!("After logging in, copy the full redirect URL from the browser address bar.");
    eprint!("Redirect URL: ");
    io::stderr().flush()?;

    let mut redirect_input = String::new();
    io::stdin().read_line(&mut redirect_input)?;
    let (code, state) =
        parse_redirect_url(&redirect_input).map_err(|error| anyhow::anyhow!(error))?;
    if state != expected_state {
        bail!("state mismatch; restart login and use the newest URL");
    }

    eprintln!("Exchanging code for tokens...");
    openai_codex_exchange(&code, &verifier).context("exchanging OAuth code")
}

fn prompt_with_default(prompt: &str, default: &str) -> Result<String> {
    eprint!("{prompt} [{default}]: ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

fn read_oauth_credentials(path: Option<PathBuf>) -> Result<ResponsesOAuthCredentials> {
    let text = match path {
        Some(path) => std::fs::read_to_string(&path)
            .with_context(|| format!("reading OAuth credentials from {}", path.display()))?,
        None => {
            let mut text = String::new();
            io::stdin().read_to_string(&mut text)?;
            text
        }
    };
    serde_json::from_str(&text).context("parsing OAuth credentials JSON")
}

fn list_providers() -> Result<()> {
    let auth_dir = OAuthFile::default_auth_dir()?;
    let entries = match std::fs::read_dir(&auth_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("No provider credentials configured.");
            return Ok(());
        }
        Err(error) => return Err(error).context("reading provider credentials directory"),
    };

    let mut names = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        names.push(name.to_owned());
    }
    names.sort();

    if names.is_empty() {
        println!("No provider credentials configured.");
        return Ok(());
    }

    for name in names {
        let file = OAuthFile::open_at(&auth_dir, &name)?;
        println!(
            "{name}\tchatgpt\t{}",
            auth_status_label(file.load()?.as_ref())
        );
    }
    Ok(())
}

fn auth_status_label(credentials: Option<&ResponsesOAuthCredentials>) -> &'static str {
    let Some(credentials) = credentials else {
        return "missing";
    };
    if credentials.access_token.trim().is_empty() {
        "invalid"
    } else if oauth_token_should_refresh(&credentials.access_token, credentials.expires_at_ms) {
        "refresh-due"
    } else {
        "logged-in"
    }
}

fn auth_status_line(
    path: &std::path::Path,
    credentials: Option<&ResponsesOAuthCredentials>,
) -> String {
    let Some(credentials) = credentials else {
        return format!("missing path={}", path.display());
    };
    let status = if credentials.access_token.trim().is_empty() {
        "invalid"
    } else if oauth_token_should_refresh(&credentials.access_token, credentials.expires_at_ms) {
        "refresh_due"
    } else {
        "fresh"
    };
    let account = credentials.account_id.as_deref().unwrap_or("unknown");
    let refresh = if credentials.refresh_token.trim().is_empty() {
        "no"
    } else {
        "yes"
    };
    format!(
        "present path={} status={} account={} refresh_token={} expires_at_ms={}",
        path.display(),
        status,
        account,
        refresh,
        credentials.expires_at_ms
    )
}

async fn run_interactive(args: ChatArgs) -> Result<()> {
    let term = ChatTerm::new()?;
    let agent = build_agent(&args, Some(term.renderer())).await?;
    term.print_history(agent.blocks());
    let mut app = ChatApp {
        agent: Arc::new(AsyncMutex::new(agent)),
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
    let mut agent = build_agent(&args, Some(output)).await?;
    agent.push_user_message(prompt);
    agent.run_until_idle(MAX_AGENT_STEPS_PER_PROMPT).await?;

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
    let provider = AgentProvider::Responses(Box::new(build_provider_session(args)));
    let tools = AgentTools::Shell(ShellTools::new(DEFAULT_TOOL_TIMEOUT));
    let mut agent = if args.no_store {
        Agent::new(provider)
    } else {
        let store = AgentStore::CborLog(CborLog::new(args.session_path()?));
        Agent::from_store(provider, store).await?
    }
    .with_tool(tools)
    .with_max_provider_retries(1);
    if let Some(renderer) = renderer {
        let provider_renderer = Arc::clone(&renderer);
        agent = agent.with_provider_updates(move |update| {
            if let Ok(mut renderer) = provider_renderer.lock() {
                renderer.handle_provider(update);
            }
        });
        agent = agent.with_agent_updates(move |update| {
            if let Ok(mut renderer) = renderer.lock() {
                renderer.handle_agent(update);
            }
        });
    }
    Ok(agent)
}

fn build_provider_session(args: &ChatArgs) -> ProviderSession {
    let mut session = ProviderSession::chatgpt_codex(args.model.clone(), args.auth.clone())
        .with_compaction_threshold(DEFAULT_COMPACTION_THRESHOLD);
    if !args.no_store {
        session = session.with_prompt_cache_key(args.session.clone());
    }
    session
}

struct ChatApp {
    agent: Arc<AsyncMutex<Agent>>,
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
                    self.running_turn = Some(spawn_agent_turn(
                        Arc::clone(&self.agent),
                        line,
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
        handle.abort();
        let _ = handle.await;
        if let Err(error) = self
            .agent
            .lock()
            .await
            .cancel_current_turn("cancelled")
            .await
        {
            self.term.print_error(&error.to_string());
        }
        self.term.renderer_lock().finish_turn();
        if print_cancelled {
            self.term.print_system("cancelled");
        }
        self.term.set_status("/quit");
    }
}

fn spawn_agent_turn(
    agent: Arc<AsyncMutex<Agent>>,
    line: String,
    renderer: UpdateRenderer,
    handle: TermHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = {
            let mut agent = agent.lock().await;
            agent.push_user_message(line);
            agent.run_until_idle(MAX_AGENT_STEPS_PER_PROMPT).await
        };
        if let Ok(mut renderer) = renderer.lock() {
            renderer.finish_turn();
        }
        if let Err(error) = result {
            print_error_on_handle(&handle, &error.to_string());
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

    fn renderer_lock(&self) -> std::sync::MutexGuard<'_, StreamingRenderer> {
        self.renderer.lock().expect("renderer lock")
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

    fn print_error(&self, text: &str) {
        print_error_on_handle(&self.handle, text);
    }

    fn print_history(&self, blocks: &[ItemBlock]) {
        if blocks.is_empty() {
            self.print_system("rho ready");
            return;
        }
        self.print_system("loaded previous session");
        for block in blocks {
            for item in match block {
                ItemBlock::Local { items } | ItemBlock::ProviderResponse { items, .. } => items,
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

fn print_error_on_handle(handle: &TermHandle, text: &str) {
    handle.print_output(
        "error",
        StyledBlock::new(StyledText::from(vec![
            Span::new("error\n", Style::default().fg(Color::DarkRed).bold()),
            Span::plain(text.to_owned()),
        ]))
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

    fn handle_provider(&mut self, update: ResponsesUpdate) {
        match update {
            ResponsesUpdate::TextDelta { text, .. } => {
                self.assistant_text.push_str(&text);
                self.render_assistant();
            }
            ResponsesUpdate::ReasoningTextDelta { kind, text, .. } => {
                if kind == ReasoningTextKind::Summary {
                    self.thinking_text.push_str(&text);
                    self.render_thinking();
                }
            }
            ResponsesUpdate::ToolCall { call, .. } => self.render_tool_call(&call),
            ResponsesUpdate::OutputItem { item, .. } => {
                if self.assistant_text.is_empty()
                    && let ItemKind::Message(message) = item
                {
                    self.assistant_text.push_str(&message.text_content());
                    self.render_assistant();
                }
            }
            ResponsesUpdate::CompactionStarted { .. } => self.render_notice("compacting context"),
            ResponsesUpdate::Usage(_) | ResponsesUpdate::ResponseId(_) => {}
            ResponsesUpdate::Finished(response) => {
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
    Auth(AuthCommand),
    Provider(ProviderCommand),
}

#[derive(Clone)]
enum AuthCommand {
    Path { name: String },
    Status { name: String },
    Import { name: String, path: Option<PathBuf> },
}

#[derive(Clone)]
enum ProviderCommand {
    Add,
    List,
    Remove { name: String },
}

#[derive(Clone)]
struct ChatArgs {
    model: String,
    auth: ResponsesAuth,
    session: String,
    session_path: Option<PathBuf>,
    prompt_stdin: bool,
    no_store: bool,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let first = args.next();
        if matches!(first.as_deref(), Some("auth")) {
            return Ok(Self {
                command: Command::Auth(AuthCommand::parse(args)?),
            });
        }
        if matches!(first.as_deref(), Some("provider")) {
            return Ok(Self {
                command: Command::Provider(ProviderCommand::parse(args)?),
            });
        }

        Ok(Self {
            command: Command::Chat(ChatArgs::parse(first.into_iter().chain(args))?),
        })
    }
}

impl ProviderCommand {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let Some(subcommand) = args.next() else {
            bail!("missing provider command\n\n{HELP}");
        };
        match subcommand.as_str() {
            "add" => {
                if args.next().is_some() {
                    bail!(
                        "rho provider add does not accept arguments; it prompts for provider details"
                    );
                }
                Ok(Self::Add)
            }
            "list" | "status" => {
                if args.next().is_some() {
                    bail!("rho provider {subcommand} does not accept arguments");
                }
                Ok(Self::List)
            }
            "remove" | "delete" => {
                let name = args.next().unwrap_or_else(|| DEFAULT_AUTH_NAME.to_owned());
                if args.next().is_some() {
                    bail!("rho provider {subcommand} accepts at most one provider namespace");
                }
                Ok(Self::Remove { name })
            }
            unknown => bail!("unknown provider command `{unknown}`\n\n{HELP}"),
        }
    }
}

impl AuthCommand {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let Some(subcommand) = args.next() else {
            bail!("missing auth command\n\n{HELP}");
        };
        match subcommand.as_str() {
            "path" => {
                let mut name = DEFAULT_AUTH_NAME.to_owned();
                while let Some(arg) = args.next() {
                    match arg.as_str() {
                        "--name" => name = take_arg(&mut args, "--name")?,
                        unknown => bail!("unknown auth path argument `{unknown}`\n\n{HELP}"),
                    }
                }
                Ok(Self::Path { name })
            }
            "status" => {
                let mut name = DEFAULT_AUTH_NAME.to_owned();
                while let Some(arg) = args.next() {
                    match arg.as_str() {
                        "--name" => name = take_arg(&mut args, "--name")?,
                        unknown => bail!("unknown auth status argument `{unknown}`\n\n{HELP}"),
                    }
                }
                Ok(Self::Status { name })
            }
            "import" => {
                let mut name = DEFAULT_AUTH_NAME.to_owned();
                let mut path = None;
                while let Some(arg) = args.next() {
                    match arg.as_str() {
                        "--name" => name = take_arg(&mut args, "--name")?,
                        "--file" => path = Some(take_arg(&mut args, "--file")?.into()),
                        unknown => bail!("unknown auth import argument `{unknown}`\n\n{HELP}"),
                    }
                }
                Ok(Self::Import { name, path })
            }
            unknown => bail!("unknown auth command `{unknown}`\n\n{HELP}"),
        }
    }
}

impl ChatArgs {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut output = Self {
            model: DEFAULT_MODEL.to_owned(),
            auth: ResponsesAuth::oauth_file_named(DEFAULT_AUTH_NAME)
                .context("opening default OAuth file")?,
            session: DEFAULT_SESSION_NAME.to_owned(),
            session_path: None,
            prompt_stdin: false,
            no_store: false,
        };

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--model" => output.model = take_arg(&mut args, "--model")?,
                "--auth-file" => {
                    let value = take_arg(&mut args, "--auth-file")?;
                    output.auth = if value.contains('/') {
                        ResponsesAuth::oauth_file(value)
                    } else {
                        ResponsesAuth::oauth_file_named(value)?
                    };
                }
                "--session" => output.session = take_arg(&mut args, "--session")?,
                "--session-path" => {
                    output.session_path = Some(take_arg(&mut args, "--session-path")?.into())
                }
                "--prompt-stdin" => output.prompt_stdin = true,
                "--no-store" => output.no_store = true,
                "-h" | "--help" => bail!("{}", HELP),
                unknown => bail!("unknown argument `{unknown}`\n\n{HELP}"),
            }
        }
        if output.no_store && output.session_path.is_some() {
            bail!("--no-store cannot be used with --session-path");
        }

        Ok(output)
    }

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

fn take_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .with_context(|| format!("{name} requires a value"))
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

const HELP: &str = "\
rho

Usage:
  rho [--model MODEL] [--auth-file NAME_OR_PATH] [--session NAME]
  rho --prompt-stdin [--model MODEL] [--auth-file NAME_OR_PATH] [--session NAME]
  rho auth path [--name NAME]
  rho auth status [--name NAME]
  rho auth import [--name NAME] [--file PATH]
  rho provider add
  rho provider list
  rho provider remove [NAME]

Options:
  --model MODEL              Responses model to use [default: gpt-5]
  --auth-file NAME_OR_PATH   OAuth file name under state/rho/auth.d or explicit path [default: default]
  --session NAME             Persistent local session name [default: default]
  --session-path PATH        Explicit CBOR transcript path
  --no-store                 Run without reading or writing a transcript store
  --prompt-stdin             Read one prompt from stdin and print the final answer
  -h, --help                 Show this help

Auth:
  auth path                  Print the OAuth credentials path
  auth status                Show whether OAuth credentials are installed
  auth import                Save OAuth credentials JSON from stdin or --file
  provider add               Browser OAuth setup and save credentials
  provider list              List file-based provider credentials
  provider remove            Remove file-based provider credentials

Controls:
  Enter                       Send the prompt
  Shift-Enter, Alt-Enter      Insert a newline
  Ctrl-C twice                Cancel the running response
  Ctrl-D                      Exit from an empty prompt
";
