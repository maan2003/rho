//! `rho-tui`: an agent working here, in this terminal, drawn by the GUI's
//! own agent screen.
//!
//! The transcript and the prompt are one Zed editor over the same
//! `AgentModel` the GUI opens, with vim mode, running on `rho-tty`: the
//! terminal is the window. A bare `rho-tui` opens a draft for a new agent
//! in the current directory, and the first prompt creates it; `rho-tui
//! attach <agent>` opens one that exists.
//!
//! The draft is the GUI's: a workdir, a role and a start row over the
//! message, with the workdir set to this directory and the agent started
//! in it, in place. Tab walks the rows, Ctrl-T cycles the value of the row
//! under the cursor, and Ctrl-U empties it.
//!
//! Enter in insert mode sends; Alt-Enter is a newline. Ctrl-C stops a
//! working agent; Ctrl-D in an empty prompt, or Ctrl-Q, leaves.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, Styled as _,
    StyledText, Task, Window, WindowOptions, actions, div, px,
};
use rho_agent_types::{AgentId, AgentPos, ContentPart};
use rho_agents_client::create::{
    AUTO_BASE_REV, DEFAULT_ROLE, DEFAULT_START, StartFieldMode, cycle_agent_role_text,
    parse_agent_role,
};
use rho_agents_client::protocol::transcript::{Live, RuntimeState, TranscriptEvent};
use rho_agents_client::protocol::{
    AgentCommand, AgentLog, ClientFrame, NewAgent, ServerFrame, StartMode, UserMessage,
};
use rho_agents_client::state::UiAgentStatus;
use rho_agents_client::store::FrameSummary;
use rho_agents_view::draft::DraftModel;
use rho_agents_view::{AgentModel, AgentModelEvent, DraftFieldClear, DraftValueCycle};
use rho_term::agent::{Directory, Followed, resolve, short_handle};
use rho_term::host::Host;
use settings::{KeymapFile, SettingsStore};

actions!(
    rho_tui,
    [
        Submit,
        SubmitFromRow,
        Stop,
        Leave,
        LeaveIfEmpty,
        NextField,
        PreviousField
    ]
);

#[derive(Parser)]
#[command(
    name = "rho-tui",
    about = "Start an agent here, or attach to one, in a full-screen editor"
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

/// The editor's text is the terminal's: one 16 px line is one row.
const SETTINGS: &str = r#"{
  "buffer_font_size": 16,
  "ui_font_size": 16,
  "agent_buffer_font_size": 16,
  "agent_ui_font_size": 16,
  "buffer_line_height": { "custom": 1.0 },
  "cursor_blink": false,
  "theme": "One Dark"
}"#;

/// Where the session stands before the screen is up: a new agent not yet
/// asked for, or one being followed.
enum Phase {
    Draft { role: String },
    Following(Box<Followed>),
}

/// What the screen shows: the draft, or the agent it became.
enum Screen {
    Draft {
        model: Entity<DraftModel>,
        editor: Entity<Editor>,
    },
    Agent {
        followed: Box<Followed>,
        model: Entity<AgentModel>,
        editor: Entity<Editor>,
        _subscription: gpui::Subscription,
    },
}

/// What the session leaves behind for the terminal once the screen is gone.
#[derive(Default)]
struct Outcome {
    /// The agent's handle, to attach to it again.
    handle: Option<String>,
    clean: bool,
}

struct Connected {
    host: Arc<Host>,
    to_host: tokio::sync::mpsc::UnboundedSender<ClientFrame>,
    from_host: tokio::sync::mpsc::UnboundedReceiver<Result<ServerFrame>>,
    session: tokio::task::JoinHandle<()>,
    directory: Directory,
    phase: Phase,
}

fn main() -> Result<()> {
    let args = Args::parse();
    parse_agent_role(&args.role).map_err(|error| anyhow::anyhow!(error))?;
    let cwd = std::env::current_dir().context("current directory")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    // Everything that can fail before the screen is up fails here, where
    // an error can still be printed.
    let connected = runtime.block_on(connect(&args))?;
    let host = connected.host.clone();
    let outcome = Rc::new(RefCell::new(Outcome::default()));
    let rt = runtime.handle().clone();
    let screen_outcome = outcome.clone();
    let mut slot = Some(connected);
    rho_tty::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            if let Err(error) = init(cx) {
                screen_outcome.borrow_mut().handle = Some(format!("{error:#}"));
                cx.quit();
                return;
            }
            let connected = slot.take().expect("the screen opens once");
            let outcome = screen_outcome.clone();
            let opened = cx.open_window(WindowOptions::default(), move |window, cx| {
                cx.new(|cx| Root::new(connected, rt, cwd, outcome, window, cx))
            });
            if let Err(error) = opened {
                screen_outcome.borrow_mut().handle = Some(format!("{error:#}"));
                cx.quit();
            }
        });

    let outcome = outcome.take();
    if let Some(handle) = outcome.handle {
        eprintln!("agent {handle}: `rho-tui attach {handle}` opens it again");
    }
    if outcome.clean
        && let Some(reason) = runtime.block_on(host.leave())
    {
        eprintln!("the agent host keeps running: {reason}");
    }
    Ok(())
}

async fn connect(args: &Args) -> Result<Connected> {
    let host = Host::connect_or_start(args.socket_path.clone()).await?;
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
    to_host.send(ClientFrame::Follow {
        since: journal_head,
    })?;
    let mut directory = Directory::default();
    let phase = match &args.command {
        None => Phase::Draft {
            role: args.role.clone(),
        },
        Some(Command::Attach { agent }) => {
            let agent_id = resolve(agent, machine_seed, agent_counter)
                .map_err(|error| anyhow::anyhow!(error))?;
            let mut followed = Followed::new(agent_id);
            let rows = read_log(&host, agent_id).await?;
            for (_, event) in &rows {
                directory.tell_one(agent_id, event);
            }
            followed.tell_rows(&rows);
            to_host.send(ClientFrame::Focus {
                agent_ids: vec![agent_id],
            })?;
            Phase::Following(Box::new(followed))
        }
    };
    Ok(Connected {
        host: Arc::new(host),
        to_host,
        from_host,
        session,
        directory,
        phase,
    })
}

/// One agent's whole log, page by page.
async fn read_log(host: &Host, agent_id: AgentId) -> Result<Vec<(AgentPos, TranscriptEvent)>> {
    let mut rows = Vec::new();
    let mut from = AgentPos(0);
    loop {
        let page = host.call(AgentLog { agent_id, from }).await?;
        rows.extend(page.rows);
        match page.next {
            Some(next) => from = next,
            None => return Ok(rows),
        }
    }
}

fn init(cx: &mut App) -> Result<()> {
    let mut store = SettingsStore::new(cx, &settings::default_settings());
    store.set_user_settings(SETTINGS, cx).result()?;
    cx.set_global(store);
    let settings = cx.global_mut::<SettingsStore>();
    settings.override_global(vim_mode_setting::VimModeSetting(true));
    theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
    release_channel::init(semver::Version::new(0, 1, 0), cx);
    editor::init(cx);
    vim::init(cx);
    for path in [settings::DEFAULT_KEYMAP_PATH, settings::VIM_KEYMAP_PATH] {
        cx.bind_keys(KeymapFile::load_asset_allow_partial_failure(path, cx)?);
    }
    // Bound under `Editor` so they outrank the stock keymaps' bindings of
    // the same keys, which gpui resolves by depth.
    cx.bind_keys([
        KeyBinding::new(
            "enter",
            Submit,
            Some("RhoTui > Editor && vim_mode == insert && !showing_completions"),
        ),
        KeyBinding::new("ctrl-c", Stop, Some("RhoTui > Editor")),
        KeyBinding::new("ctrl-c", Stop, Some("RhoTui")),
        KeyBinding::new("ctrl-q", Leave, Some("RhoTui > Editor")),
        KeyBinding::new("ctrl-q", Leave, Some("RhoTui")),
        KeyBinding::new(
            "ctrl-d",
            LeaveIfEmpty,
            Some("RhoTui > Editor && vim_mode == insert"),
        ),
        // A terminal sends shift-enter as enter; alt-enter is what it can tell apart.
        KeyBinding::new(
            "alt-enter",
            editor::actions::Newline,
            Some("RhoTui > Editor && vim_mode == insert"),
        ),
        KeyBinding::new(
            "shift-enter",
            editor::actions::Newline,
            Some("RhoTui > Editor && vim_mode == insert"),
        ),
        // The draft's rows. Enter outside insert mode sends from a row
        // and is vim's own motion in the body.
        KeyBinding::new(
            "enter",
            SubmitFromRow,
            Some("RhoDraft > Editor && vim_mode != insert && !showing_completions"),
        ),
        KeyBinding::new(
            "tab",
            NextField,
            Some("RhoDraft > Editor && !showing_completions"),
        ),
        KeyBinding::new(
            "shift-tab",
            PreviousField,
            Some("RhoDraft > Editor && !showing_completions"),
        ),
        KeyBinding::new("ctrl-t", DraftValueCycle, Some("RhoDraft > Editor")),
        KeyBinding::new(
            "ctrl-u",
            DraftFieldClear,
            Some("RhoDraft > Editor && !showing_completions"),
        ),
    ]);
    Ok(())
}

/// Completes nothing: the composer asks for a provider, and the terminal
/// has no workspace to complete from yet.
struct NoCompletions;

impl editor::CompletionProvider for NoCompletions {
    fn completions(
        &self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        _cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<project::CompletionResponse>>> {
        Task::ready(Ok(Vec::new()))
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<language::Buffer>,
        _position: language::Anchor,
        _text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        false
    }
}

struct Root {
    host: Arc<Host>,
    rt: tokio::runtime::Handle,
    to_host: tokio::sync::mpsc::UnboundedSender<ClientFrame>,
    session: Option<tokio::task::JoinHandle<()>>,
    cwd: PathBuf,
    screen: Screen,
    directory: Directory,
    runtime: Option<RuntimeState>,
    notice: Option<String>,
    outcome: Rc<RefCell<Outcome>>,
    _tasks: Vec<Task<()>>,
}

impl Root {
    fn new(
        connected: Connected,
        rt: tokio::runtime::Handle,
        cwd: PathBuf,
        outcome: Rc<RefCell<Outcome>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Connected {
            host,
            to_host,
            mut from_host,
            session,
            directory,
            phase,
        } = connected;
        let screen = match phase {
            Phase::Draft { role } => {
                let model = cx.new(|cx| {
                    DraftModel::new(rho_agents_view::draft::Hooks::new(|_, _, _, _| {}), cx)
                });
                let editor = model.update(cx, |draft, cx| {
                    draft.set_workdir_text(&cwd.display().to_string(), cx);
                    draft.set_role_text(&role, cx);
                    draft.set_start_mode(StartFieldMode::Here, cx);
                    // In place has no base to name.
                    draft.set_start_text("", cx);
                    draft.build_editor(window, cx)
                });
                window.focus(&editor.focus_handle(cx), cx);
                Screen::Draft { model, editor }
            }
            Phase::Following(followed) => Self::agent_screen(followed, window, cx),
        };
        let frames = cx.spawn(async move |this, cx| {
            while let Some(frame) = from_host.recv().await {
                let Ok(()) = this.update(cx, |this, cx| this.frame(frame, cx)) else {
                    return;
                };
            }
        });
        // Running calls count their seconds.
        let ticks = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    if let Screen::Agent { model, .. } = &this.screen {
                        model.update(cx, |model, cx| {
                            if model.has_timers() {
                                model.tick_timers(rho_agents_client::now_ms(), cx);
                            }
                        });
                    }
                }) else {
                    return;
                };
            }
        });
        let mut root = Self {
            host,
            rt,
            to_host,
            session: Some(session),
            cwd,
            screen,
            directory,
            runtime: None,
            notice: None,
            outcome,
            _tasks: vec![frames, ticks],
        };
        root.sync(FrameSummary::everything(), cx);
        root
    }

    /// The GUI's agent screen over one followed agent: the transcript with
    /// the prompt under it, the cursor in the prompt.
    fn agent_screen(
        followed: Box<Followed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Screen {
        let model = cx.new(|cx| {
            AgentModel::new(
                Rc::new(NoCompletions),
                rho_agents_client::remote::AgentsLink::detached(),
                cx,
            )
        });
        let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
        model.update(cx, |model, cx| model.focus_prompt(&editor, window, cx));
        // The model held what moved while it loaded; show it.
        let subscription = cx.subscribe(&model, |this, _, event, cx| {
            if let AgentModelEvent::Loaded(_) = event {
                this.sync(FrameSummary::nothing(), cx);
            }
        });
        Screen::Agent {
            followed,
            model,
            editor,
            _subscription: subscription,
        }
    }

    fn followed(&self) -> Option<&Followed> {
        match &self.screen {
            Screen::Agent { followed, .. } => Some(followed),
            Screen::Draft { .. } => None,
        }
    }

    fn editor(&self) -> &Entity<Editor> {
        match &self.screen {
            Screen::Agent { editor, .. } | Screen::Draft { editor, .. } => editor,
        }
    }

    fn frame(&mut self, frame: Result<ServerFrame>, cx: &mut Context<Self>) {
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                self.notice = Some(format!("lost the agent host: {error:#}"));
                cx.notify();
                return;
            }
        };
        if let ServerFrame::Log { entries } = &frame {
            self.directory.tell(entries);
        }
        let Screen::Agent { followed, .. } = &mut self.screen else {
            return;
        };
        let summary = match frame {
            ServerFrame::Log { entries } => followed.tell(&entries),
            ServerFrame::Live { agent_id, live } if agent_id == followed.agent_id => {
                if let Live::Snapshot { state, .. } = &live {
                    self.runtime = Some(state.clone());
                }
                Some(followed.live(live))
            }
            _ => None,
        };
        if let Some(summary) = summary {
            self.sync(summary, cx);
        }
    }

    /// Hands the followed agent's state to the screen: whole the first
    /// time, then what moved.
    fn sync(&mut self, summary: FrameSummary, cx: &mut Context<Self>) {
        let Screen::Agent {
            followed, model, ..
        } = &self.screen
        else {
            return;
        };
        let Some(state) = followed.state() else {
            return;
        };
        let agent_id = followed.agent_id;
        let now = rho_agents_client::now_ms();
        if !model.read(cx).initial_load_started() {
            let state = state.clone();
            let labels = self.directory.labels();
            model.update(cx, |model, cx| {
                model.start_initial_load(agent_id, state, labels, now, cx)
            });
        } else {
            let directory = &self.directory;
            model.update(cx, |model, cx| {
                model.sync(state, summary, now, &|id| directory.label(id), cx)
            });
        }
        cx.notify();
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        match &self.screen {
            Screen::Draft { .. } => self.submit_draft(window, cx),
            Screen::Agent {
                followed, model, ..
            } => {
                let Some(content) = model.update(cx, |model, cx| model.take_prompt(cx)) else {
                    return;
                };
                let host = self.host.clone();
                let command = AgentCommand::Send {
                    agent_id: followed.agent_id,
                    messages: vec![UserMessage {
                        id: rand::random(),
                        content: content.clone(),
                    }],
                };
                let call = self.rt.spawn(async move { host.call(command).await });
                cx.spawn(async move |this, cx| {
                    let result = call.await.map_err(anyhow::Error::from).and_then(|r| r);
                    if let Err(error) = result {
                        let _ = this.update(cx, |this, cx| {
                            this.notice = Some(format!("not sent: {error:#}"));
                            this.restore_prompt(&content, cx);
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
        }
        cx.notify();
    }

    /// Enter outside insert mode: it sends from a row, and is vim's own
    /// motion in the body.
    fn submit_from_row(&mut self, _: &SubmitFromRow, window: &mut Window, cx: &mut Context<Self>) {
        let Screen::Draft { model, editor } = &self.screen else {
            cx.propagate();
            return;
        };
        if model.read(cx).cursor_in_a_field(editor, cx) {
            self.submit_draft(window, cx);
        } else {
            cx.propagate();
        }
    }

    /// The draft as the host's `NewAgent`: the workdir is a path on this
    /// machine, relative ones from where `rho-tui` started.
    fn new_agent(&self, draft: &DraftModel, cx: &App) -> Result<NewAgent, String> {
        let content = draft.content(cx).ok_or("nothing to send yet")?;
        let field = draft.workdir_text(cx).trim().to_owned();
        let workdir = if field.is_empty() {
            self.cwd.clone()
        } else {
            self.cwd.join(field)
        };
        let workdir = camino::Utf8PathBuf::try_from(workdir)
            .map_err(|_| "the workdir is not UTF-8".to_owned())?;
        let role = parse_agent_role(draft.role_text(cx).trim())?;
        let start = match draft.start_mode() {
            StartFieldMode::Here => StartMode::Here { cwd: workdir },
            StartFieldMode::NewOn => {
                let target = draft.start_text(cx).trim().to_owned();
                StartMode::NewOn {
                    repo: workdir,
                    revset: if target.is_empty() || target.eq_ignore_ascii_case(DEFAULT_START) {
                        AUTO_BASE_REV.to_owned()
                    } else {
                        target
                    },
                }
            }
            StartFieldMode::Join => {
                return Err("joining another agent needs the GUI for now".to_owned());
            }
        };
        Ok(NewAgent {
            role,
            start,
            content: Some(content),
        })
    }

    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Screen::Draft { model, .. } = &self.screen else {
            return;
        };
        let model = model.clone();
        let new = match self.new_agent(model.read(cx), cx) {
            Ok(new) => new,
            Err(refusal) => {
                model.update(cx, |draft, cx| draft.set_refusal(Some(refusal), cx));
                return;
            }
        };
        model.update(cx, |draft, cx| draft.set_refusal(None, cx));
        self.notice = Some("starting the agent".into());
        let host = self.host.clone();
        let call = self.rt.spawn(async move { host.call(new).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = call.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| {
                this.notice = None;
                match result {
                    Ok(agent_id) => this.follow(agent_id, window, cx),
                    Err(error) => model.update(cx, |draft, cx| {
                        draft.set_refusal(Some(format!("could not start the agent: {error:#}")), cx)
                    }),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The draft became an agent: the screen follows it from here. Rows
    /// the session carried before now went by unfolded, so its log is read
    /// from the start; the fold drops the rows it is told twice.
    fn follow(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.to_host.send(ClientFrame::Focus {
            agent_ids: vec![agent_id],
        });
        self.screen = Self::agent_screen(Box::new(Followed::new(agent_id)), window, cx);
        window.focus(&self.editor().focus_handle(cx), cx);
        let host = self.host.clone();
        let log = self
            .rt
            .spawn(async move { read_log(&host, agent_id).await });
        cx.spawn(async move |this, cx| {
            let rows = log.await.map_err(anyhow::Error::from).and_then(|rows| rows);
            let _ = this.update(cx, |this, cx| match rows {
                Ok(rows) => {
                    for (_, event) in &rows {
                        this.directory.tell_one(agent_id, event);
                    }
                    if let Screen::Agent { followed, .. } = &mut this.screen
                        && let Some(summary) = followed.tell_rows(&rows)
                    {
                        this.sync(summary, cx);
                    }
                }
                Err(error) => {
                    this.notice = Some(format!("could not read the agent's log: {error:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn restore_prompt(&mut self, content: &[ContentPart], cx: &mut Context<Self>) {
        let Screen::Agent { model, .. } = &self.screen else {
            return;
        };
        let text = content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        model.update(cx, |model, cx| model.set_prompt_text(&text, cx));
    }

    fn next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Draft { model, editor } = &self.screen {
            let editor = editor.clone();
            model.update(cx, |draft, cx| draft.toggle_field(&editor, window, cx));
        }
    }

    fn previous_field(&mut self, _: &PreviousField, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Draft { model, editor } = &self.screen {
            let editor = editor.clone();
            model.update(cx, |draft, cx| draft.toggle_field_back(&editor, window, cx));
        }
    }

    fn cycle_value(&mut self, _: &DraftValueCycle, _: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Draft { model, editor } = &self.screen {
            let editor = editor.clone();
            model.update(cx, |draft, cx| {
                if draft.cursor_in_role_field(&editor, cx) {
                    let next = cycle_agent_role_text(&draft.role_text(cx));
                    draft.set_role_text(next, cx);
                } else if draft.cursor_in_start_field(&editor, cx) {
                    draft.cycle_start_mode(cx);
                }
            });
        }
    }

    fn clear_field(&mut self, _: &DraftFieldClear, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Draft { model, editor } = &self.screen {
            let editor = editor.clone();
            let cleared = model.update(cx, |draft, cx| draft.clear_field(&editor, window, cx));
            if !cleared {
                cx.propagate();
            }
        }
    }

    fn stop(&mut self, _: &Stop, _: &mut Window, cx: &mut Context<Self>) {
        let working = self.runtime.as_ref().is_some_and(RuntimeState::is_working);
        match self.followed() {
            Some(followed) if working => {
                let host = self.host.clone();
                let command = AgentCommand::Cancel {
                    agent_id: followed.agent_id,
                };
                let call = self.rt.spawn(async move { host.call(command).await });
                cx.spawn(async move |this, cx| {
                    if let Ok(Err(error)) = call.await {
                        let _ = this.update(cx, |this, cx| {
                            this.notice = Some(format!("could not stop the agent: {error:#}"));
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
            _ => self.notice = Some("Ctrl-Q leaves".into()),
        }
        cx.notify();
    }

    fn leave(&mut self, _: &Leave, _: &mut Window, cx: &mut Context<Self>) {
        let handle = self.followed().map(|followed| {
            self.directory.get(followed.agent_id).map_or_else(
                || followed.agent_id.encoded(),
                |identity| short_handle(identity.role.handle_prefix(), followed.agent_id),
            )
        });
        *self.outcome.borrow_mut() = Outcome {
            handle,
            clean: true,
        };
        // The session says goodbye before the process does.
        let (to_host, _) = tokio::sync::mpsc::unbounded_channel();
        drop(std::mem::replace(&mut self.to_host, to_host));
        if let Some(session) = self.session.take() {
            let _ = self
                .rt
                .block_on(async { tokio::time::timeout(Duration::from_secs(2), session).await });
        }
        cx.quit();
    }

    fn leave_if_empty(&mut self, _: &LeaveIfEmpty, window: &mut Window, cx: &mut Context<Self>) {
        let empty = match &self.screen {
            Screen::Agent { model, .. } => model.read(cx).prompt_text(cx).is_empty(),
            Screen::Draft { model, .. } => model.read(cx).body_text(cx).is_empty(),
        };
        if empty {
            self.leave(&Leave, window, cx);
        } else {
            cx.propagate();
        }
    }

    fn status_line(&self, cx: &App) -> StyledText {
        let theme = theme::ActiveTheme::theme(cx);
        let dim = theme.colors().text_muted;
        let status = theme.status();
        let mut text = String::new();
        let mut highlights = Vec::new();
        let mut push = |text: &mut String, part: &str, color: gpui::Color, bold: bool| {
            let start = text.len();
            text.push_str(part);
            highlights.push((
                start..text.len(),
                HighlightStyle {
                    color: Some(Hsla::from(color)),
                    font_weight: bold.then_some(gpui::FontWeight::BOLD),
                    ..Default::default()
                },
            ));
        };
        match &self.screen {
            Screen::Draft { .. } => {
                push(&mut text, " new agent", theme.colors().text, true);
                push(
                    &mut text,
                    " · Enter starts it · Tab rows · Ctrl-T cycles · Ctrl-Q leaves",
                    dim,
                    false,
                );
            }
            Screen::Agent { followed, .. } => {
                push(
                    &mut text,
                    &format!(" {}", self.directory.label(followed.agent_id)),
                    theme.colors().text,
                    true,
                );
                let (word, color) =
                    followed
                        .state()
                        .map_or(("connecting", dim), |state| match state.status {
                            UiAgentStatus::Idle => ("idle", status.success),
                            UiAgentStatus::Streaming => ("responding", status.warning),
                            UiAgentStatus::ToolCalling { .. } => ("working", status.warning),
                            UiAgentStatus::Retrying { .. } => ("retrying", status.warning),
                            UiAgentStatus::UnfinishedTurn { .. } => ("unfinished", status.warning),
                            UiAgentStatus::Error => ("stopped on an error", status.error),
                        });
                push(&mut text, " · ", dim, false);
                push(&mut text, word, color, false);
                if let Some(tasks) = self.runtime.as_ref().map(|r| r.running_tasks)
                    && tasks > 0
                {
                    push(&mut text, &format!(" · {tasks} running"), dim, false);
                }
                push(&mut text, " · Ctrl-C stops · Ctrl-Q leaves", dim, false);
            }
        }
        if let Some(notice) = &self.notice {
            push(&mut text, &format!(" · {notice}"), status.warning, false);
        }
        StyledText::new(text).with_highlights(highlights)
    }
}

impl Focusable for Root {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor().focus_handle(cx)
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::ActiveTheme::theme(&**cx);
        let colors = theme.colors();
        let draft = matches!(self.screen, Screen::Draft { .. });
        div()
            .key_context(if draft { "RhoTui RhoDraft" } else { "RhoTui" })
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::submit_from_row))
            .on_action(cx.listener(Self::stop))
            .on_action(cx.listener(Self::leave))
            .on_action(cx.listener(Self::leave_if_empty))
            .on_action(cx.listener(Self::next_field))
            .on_action(cx.listener(Self::previous_field))
            .on_action(cx.listener(Self::cycle_value))
            .on_action(cx.listener(Self::clear_field))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .text_color(colors.text)
            .child(div().flex_1().min_h_0().child(self.editor().clone()))
            .child(
                div()
                    .h(px(rho_tty::CELL_H))
                    .text_size(px(rho_tty::FONT_SIZE))
                    .line_height(px(rho_tty::CELL_H))
                    .bg(colors.status_bar_background)
                    .child(self.status_line(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{App, AsyncApp, WindowOptions};
    use rho_agent_types::{AgentIdDomain, AgentRole, Place, SendKind, UnixMs};
    use rho_agents_client::protocol::transcript::{RuntimeKind, SpawnedBy};
    use rho_tty::Headless;

    use super::*;

    fn agent() -> AgentId {
        AgentId::from_counter(1, &AgentIdDomain(0)).expect("an agent id")
    }

    /// An agent asked `hi` that answered `hello back`.
    fn answered() -> Phase {
        let at = UnixMs(0);
        let events = [
            TranscriptEvent::Created {
                role: AgentRole::default(),
                runtime: RuntimeKind::Rho,
                place: Place {
                    cwd: "/src/repo".into(),
                    workset: None,
                },
                spawned_by: SpawnedBy::Direct,
                spawn_name: None,
                parent: None,
                model: "test-model".to_owned(),
                at,
            },
            TranscriptEvent::Message {
                from: None,
                text: "hi".to_owned(),
                at,
            },
            TranscriptEvent::Sent {
                results: Vec::new(),
                compaction: false,
                at,
            },
            TranscriptEvent::MessageSent {
                to: None,
                text: "hello back".to_owned(),
                kind: SendKind::Result,
                at,
            },
        ];
        let rows: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(pos, event)| (AgentPos(pos as u64), event))
            .collect();
        let mut followed = Followed::new(agent());
        followed.tell_rows(&rows);
        Phase::Following(Box::new(followed))
    }

    /// Runs the screen over `phase` on a headless terminal, with no agent
    /// host behind it, and hands `script` the terminal to drive.
    fn run<F>(cols: u16, rows: u16, phase: Phase, script: F)
    where
        F: AsyncFnOnce(Headless, &mut AsyncApp) + 'static,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (to_host, _outgoing) = tokio::sync::mpsc::unbounded_channel();
        let (_incoming, from_host) = tokio::sync::mpsc::unbounded_channel();
        let connected = Connected {
            host: Arc::new(Host::at("/nonexistent/rho-tui-test.sock".into())),
            to_host,
            from_host,
            session: runtime.spawn(async {}),
            directory: Directory::default(),
            phase,
        };
        let (app, term) = rho_tty::headless(cols, rows);
        let rt = runtime.handle().clone();
        let mut slot = Some(connected);
        app.with_assets(assets::Assets).run(move |cx: &mut App| {
            init(cx).unwrap();
            let connected = slot.take().unwrap();
            let outcome = Rc::new(RefCell::new(Outcome::default()));
            cx.open_window(WindowOptions::default(), move |window, cx| {
                cx.new(|cx| Root::new(connected, rt, "/tmp/here".into(), outcome, window, cx))
            })
            .unwrap();
            cx.spawn(async move |cx| {
                script(term.clone(), cx).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }

    fn lines(term: &Headless) -> Vec<String> {
        term.text().lines().map(str::to_owned).collect()
    }

    /// The draft is the GUI's rows over the message, set to start here,
    /// with the cursor in the message, in insert mode.
    #[test]
    fn a_bare_start_drafts_an_agent_in_place_here() {
        run(
            50,
            8,
            Phase::Draft { role: "eng".into() },
            async |term, _| {
                term.settled().await;
                let screen = lines(&term);
                assert_eq!(
                    screen[..5],
                    [
                        " Workdir: /tmp/here",
                        " Role: eng",
                        " In place:",
                        "",
                        "│Write a message…"
                    ]
                );
                assert!(screen[7].starts_with(" new agent · Enter starts it"));
                assert_eq!(term.cursor(), Some((1, 4)));
                term.type_text("fix it");
                term.settled().await;
                assert_eq!(lines(&term)[4], "│fix it");
                assert_eq!(term.cursor(), Some((7, 4)), "after what was typed");
            },
        );
    }

    /// The transcript and the prompt are one editor: insert mode puts the
    /// terminal's cursor in the prompt; normal mode hides it for the block
    /// cursor the editor draws; visual mode selects with the editor's
    /// colours.
    #[test]
    fn an_agent_is_its_transcript_over_the_prompt_in_vim() {
        run(50, 8, answered(), async |term, _| {
            term.settled().await;
            let screen = lines(&term);
            assert_eq!(
                screen[..5],
                ["│hi", "", " hello back", "", "│Write a message…"]
            );
            assert!(screen[7].contains(" · idle · "), "{:?}", screen[7]);
            assert_eq!(term.cursor(), Some((1, 4)));
            let background = term.background(10, 4);

            term.press("escape");
            term.settled().await;
            assert_eq!(term.cursor(), None, "normal mode draws its own cursor");
            assert_ne!(term.background(1, 4), background, "the block cursor");

            // Up to `hello back`, and select `hel`.
            term.press("k");
            term.press("k");
            term.press("0");
            term.press("v");
            term.press("l");
            term.press("l");
            term.settled().await;
            let selected: Vec<u16> = (0..12)
                .filter(|&col| term.background(col, 2) != background)
                .collect();
            assert_eq!(selected, [1, 2, 3], "`hel`, and the block cursor on `l`");
        });
    }

    /// A smaller terminal lays the same screen out again: the status line
    /// stays on the last row.
    #[test]
    fn a_resize_keeps_the_status_line_at_the_bottom() {
        run(50, 8, answered(), async |term, _| {
            term.settled().await;
            term.resize(30, 6);
            term.settled().await;
            let screen = lines(&term);
            assert_eq!(screen.len(), 6);
            assert!(screen[5].starts_with(" agent-"), "{screen:?}");
            assert_eq!(screen[4], "│Write a message…");
        });
    }
}
