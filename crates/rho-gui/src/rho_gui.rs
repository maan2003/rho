use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow};
use audio::{Audio, Sound};
use clap::Parser;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use editor::scroll::AutoscrollStrategy;
use editor::{
    DisplayElisionId, DisplayElisionProperties, Editor, EditorMode, EditorRightPrompt,
    HighlightKey, Inlay, SelectionEffects, SizingBehavior,
};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, Focusable as _, FontStyle, FontWeight, HighlightStyle, Hsla, MouseButton,
    Rgba, Subscription, Task, TextStyle, WeakEntity, Window, WindowOptions, actions, div, px, svg,
};
use language::{
    Buffer, BufferEvent, Capability, Language, LanguageConfig, LanguageMatcher, LanguageQueries,
    Point, Rope,
};
use multi_buffer::{MultiBuffer, PathKey};
use project::InlayId;
use rho_ui_proto::client::AgentClient as RhoAgentClient;
use rho_ui_proto::remote::{
    AgentRemoteFrame as RhoAgentRemoteFrame, UiAgentState as RhoUiAgentState,
    UiBlock as RhoUiBlock, UiMessagePhase as RhoUiMessagePhase,
    UiStreamingItem as RhoUiStreamingItem, UiTool as RhoUiTool, UiToolStatus as RhoUiToolStatus,
};
use rho_ui_proto::{AgentId as RhoAgentId, UiTopic as RhoUiTopic};
use settings::SettingsStore;
use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextRole, Event, HarnessInputMessage, ModelParams,
    PeerInputMessage, PromptMessageClass, PromptOriginator, ToolCallItem, UiPromptSubmitted,
};
use text::ToOffset as _;
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, IconSize};

mod activity_state;
mod agent_state;
mod cli_theme;
mod commands;
mod completion_state;
mod prompt_state;
mod role_state;
mod shell_state;
mod socket_client;
mod status_line;
mod task_state;
mod tool_render;
mod tool_state;
mod transcript;
use activity_state::MainToolActivity;
use agent_state::{AgentContextUsage, AgentState};
use commands::parse_role_setting_update;
use completion_state::{CompletionCandidate, TauCompletionProvider, TauCompletionState};
use prompt_state::{PromptState, QueuedPrompt};
use role_state::{RoleCycleKind, RoleCycleOutcome, RoleState};
use shell_state::{ShellCommandState, ShellState};
use socket_client::{SocketEvent, Writer};
use task_state::TaskState;
use tool_state::ToolState;
#[cfg(test)]
use transcript::buffer_range_starts_with;
use transcript::{InsertedTranscript, Transcript};

actions!(
    rho_gui,
    [
        SubmitPrompt,
        AgentPrevious,
        AgentNext,
        AgentNew,
        RoleCycle,
        RoleCycleGroup,
        TaskBoard,
        TaskOpen
    ]
);

fn main() {
    if let Err(error) = run() {
        eprintln!("rho-gui: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let attach_target = attach_target_from_args(Args::parse())?;

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            if let Err(error) = init_app(cx) {
                eprintln!("rho-gui: {error:#}");
                cx.quit();
                return;
            }

            cx.activate(true);

            if let Err(error) = cx.open_window(WindowOptions::default(), move |window, cx| {
                cx.new(|cx| RhoGui::new(attach_target.clone(), window, cx))
            }) {
                eprintln!("rho-gui: failed to open window: {error:#}");
                cx.quit();
            }
        });

    Ok(())
}

#[derive(Clone)]
struct AttachTarget {
    socket_path: PathBuf,
    project_root: PathBuf,
}

#[derive(Parser)]
#[command(
    name = "rho-gui",
    about = "Attach a native GUI to a running Rho daemon"
)]
struct Args {
    /// Connect directly to this Tau harness Unix socket.
    #[arg(long)]
    socket: Option<PathBuf>,
}

fn attach_target_from_args(args: Args) -> Result<AttachTarget> {
    match args.socket {
        Some(socket_path) => attach_target_for_socket(socket_path),
        None => attach_target_for_current_dir(),
    }
}

fn attach_target_for_socket(socket_path: PathBuf) -> Result<AttachTarget> {
    let project_root = std::env::current_dir().context("failed to read current directory")?;
    Ok(AttachTarget {
        socket_path,
        project_root,
    })
}

fn attach_target_for_current_dir() -> Result<AttachTarget> {
    let project_root = std::env::current_dir().context("failed to read current directory")?;
    Ok(AttachTarget {
        socket_path: rho_daemon::default_socket_path()?,
        project_root,
    })
}
fn init_app(cx: &mut App) -> Result<()> {
    assets::Assets.load_fonts(cx)?;
    let settings_path = rho_gui_settings_path()?;
    let user_settings = load_or_create_rho_gui_settings(&settings_path)?;
    let mut store = SettingsStore::new(cx, settings::default_settings().as_ref());
    store
        .set_user_settings(&user_settings, cx)
        .result()
        .with_context(|| format!("failed to load settings from {}", settings_path.display()))?;
    cx.set_global(store);
    theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
    release_channel::init(semver::Version::new(0, 1, 0), cx);
    editor::init(cx);
    command_palette::init(cx);
    search::init(cx);
    vim::init(cx);
    let default_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)
            .context("failed to load default keymap")?;
    eprintln!(
        "rho-gui: loaded {} default key bindings from {}",
        default_key_bindings.len(),
        settings::DEFAULT_KEYMAP_PATH
    );
    cx.bind_keys(default_key_bindings);
    let vim_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::VIM_KEYMAP_PATH, cx)
            .context("failed to load vim keymap")?;
    eprintln!(
        "rho-gui: loaded {} vim key bindings from {}",
        vim_key_bindings.len(),
        settings::VIM_KEYMAP_PATH
    );
    cx.bind_keys(vim_key_bindings);
    Ok(())
}

const PROMPT_PLACEHOLDER_INLAY_ID: usize = 0;
const PROMPT_DRAFT_HIGHLIGHT_KEY: usize = usize::MAX - 1;
static RHO_MARKDOWN_LANGUAGE: OnceLock<Option<Arc<Language>>> = OnceLock::new();
static RHO_MARKDOWN_INLINE_LANGUAGE: OnceLock<Option<Arc<Language>>> = OnceLock::new();
const DEFAULT_RHO_GUI_SETTINGS: &str = r#"// Rho GUI user settings. Values here override bundled defaults.
{}
"#;

const STARTUP_PUNS: &[&str] = &[
    "Rho is ready.",
    "Rows, roles, and rho.",
    "Rho-native, Unix-shaped.",
    "A small symbol for a large context.",
    "rho marks the prompt.",
    "Good tools, tight loops.",
    "Protocol first, pixels last.",
    "Streaming at terminal speed.",
    "A fresh path through the graph.",
    "Keep the context flowing.",
];

fn rho_gui_settings_path() -> Result<PathBuf> {
    let config_dir = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(config_home) => PathBuf::from(config_home),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".config"))
            .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?,
    };

    Ok(config_dir.join("rho-gui").join("settings.json"))
}

fn load_or_create_rho_gui_settings(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(settings) => Ok(settings),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create settings directory {}", parent.display())
                })?;
            }
            fs::write(path, DEFAULT_RHO_GUI_SETTINGS).with_context(|| {
                format!("failed to write default settings to {}", path.display())
            })?;
            Ok(DEFAULT_RHO_GUI_SETTINGS.to_owned())
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to read settings from {}", path.display()))
        }
    }
}

#[cfg(test)]
fn buffer_text_ends_with(buffer: &Buffer, end: usize, character: char) -> bool {
    if end == 0 {
        return false;
    }

    buffer
        .text_for_range(0..end)
        .collect::<String>()
        .ends_with(character)
}

fn empty_rho_ui_agent_state() -> RhoUiAgentState {
    RhoUiAgentState {
        blocks: Vec::new(),
        status: rho_ui_proto::remote::UiAgentStatus::Idle,
        pending_response: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum TranscriptStyle {
    UserPrompt,
    UserPromptQueued,
    ToolProgress,
    SystemInfo,
    SystemImportant,
    SystemDisconnect,
}

impl TranscriptStyle {
    fn style_name(self) -> &'static str {
        match self {
            Self::UserPrompt => tau_themes::names::USER_PROMPT,
            Self::UserPromptQueued => tau_themes::names::USER_PROMPT_QUEUED,
            Self::ToolProgress => tau_themes::names::PROGRESS_INDICATOR,
            Self::SystemInfo => tau_themes::names::SYSTEM_INFO,
            Self::SystemImportant => tau_themes::names::SYSTEM_INFO_IMPORTANT,
            Self::SystemDisconnect => tau_themes::names::SYSTEM_DISCONNECT,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainView {
    Agent,
    Tasks,
}

enum RhoEvent {
    Connected(RhoAgentClient),
    Topics(Vec<RhoUiTopic>),
    KnownAgents(Vec<String>),
    State(String, RhoUiAgentState),
    Frame(String, RhoAgentRemoteFrame),
    Disconnected,
    Error(String),
}

struct TaskBoardUiState {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    rows: Vec<TaskRowAnchor>,
    _subscriptions: Vec<Subscription>,
}

struct TaskRowAnchor {
    task_id: task_state::TaskId,
    start: text::Anchor,
    end: text::Anchor,
}

struct AgentUiState {
    editor: Entity<Editor>,
    prompt_buffer: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    transcript: Transcript,
    prompt_end: text::Anchor,
    draft_end: text::Anchor,
    _subscriptions: Vec<Subscription>,
    prompt_state: PromptState,
    tool_state: ToolState,
    shell_state: ShellState,
    main_tool_activity: MainToolActivity,
    previous_provider_usage: Option<tau_proto::ProviderTokenUsage>,
    current_context_percent: Option<u8>,
    current_context_input_tokens: Option<u64>,
    current_context_window: Option<u64>,
    rho_state: Option<RhoUiAgentState>,
    rho_inserted_blocks: Vec<Option<InsertedTranscript>>,
    rho_pending_inserted: Option<InsertedTranscript>,
    rho_working_elisions: Vec<RhoWorkingElision>,
    user_message_gutter_ranges: HashMap<usize, std::ops::Range<text::Anchor>>,
}

struct RhoGui {
    editor: Entity<Editor>,
    prompt_buffer: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    transcript: Transcript,
    prompt_end: text::Anchor,
    draft_end: text::Anchor,
    writer: Option<Writer>,
    rx: mpsc::Receiver<SocketEvent>,
    rho_agent: Option<RhoAgentClient>,
    rho_topics: Vec<RhoUiTopic>,
    rho_rx: mpsc::Receiver<RhoEvent>,
    rho_state: Option<RhoUiAgentState>,
    rho_inserted_blocks: Vec<Option<InsertedTranscript>>,
    rho_pending_inserted: Option<InsertedTranscript>,
    rho_working_elisions: Vec<RhoWorkingElision>,
    user_message_gutter_ranges: HashMap<usize, std::ops::Range<text::Anchor>>,
    _poll_task: Task<()>,
    _subscriptions: Vec<Subscription>,
    project_root: PathBuf,
    prompt_state: PromptState,
    cli_theme: tau_themes::Theme,
    tool_state: ToolState,
    shell_state: ShellState,
    current_model: Option<tau_proto::ModelId>,
    current_role: Option<String>,
    baseline_params: Option<ModelParams>,
    role_state: RoleState,
    current_params: ModelParams,
    current_context_percent: Option<u8>,
    current_context_input_tokens: Option<u64>,
    current_context_window: Option<u64>,
    main_tool_activity: MainToolActivity,
    previous_provider_usage: Option<tau_proto::ProviderTokenUsage>,
    agents: AgentState,
    tasks: TaskState,
    task_board: TaskBoardUiState,
    main_view: MainView,
    completion_state: Arc<Mutex<TauCompletionState>>,
    displayed_agent_id: Option<String>,
    no_agent_ui_state: Option<AgentUiState>,
    agent_ui_states: HashMap<String, AgentUiState>,
}

struct PromptGutterHighlight;
struct UserMessageGutterHighlight;

fn user_prompt_gutter_color(cx: &App) -> Hsla {
    cx.theme().colors().text_accent
}

#[derive(Clone)]
struct RhoWorkingElision {
    id: DisplayElisionId,
    range: std::ops::Range<text::Anchor>,
    tool_count: usize,
    tail_rows: u32,
}

#[derive(Clone)]
struct RhoWorkingElisionCandidate {
    range: std::ops::Range<text::Anchor>,
    tool_count: usize,
    tail_rows: u32,
}

const RHO_LIMITED_TAIL_ROWS: u32 = 12;

impl RhoGui {
    fn new(attach_target: AttachTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let completion_state = Arc::new(Mutex::new(TauCompletionState::default()));
        let this = cx.entity().downgrade();
        let ui_state = Self::new_agent_ui_state(this, completion_state.clone(), window, cx);
        let task_board = Self::new_task_board_ui_state(cx.entity().downgrade(), window, cx);
        let editor = ui_state.editor.clone();
        let prompt_buffer = ui_state.prompt_buffer.clone();
        let multi_buffer = ui_state.multi_buffer.clone();
        let transcript = ui_state.transcript;
        let prompt_end = ui_state.prompt_end;
        let draft_end = ui_state.draft_end;
        let ui_subscriptions = ui_state._subscriptions;

        let (_tx, rx) = mpsc::channel();
        let writer = None;
        let (rho_tx, rho_rx) = mpsc::channel();
        Self::spawn_rho_client(attach_target.socket_path.clone(), rho_tx);

        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| this.drain_socket_events(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut this = Self {
            editor,
            prompt_buffer,
            multi_buffer,
            transcript,
            prompt_end,
            draft_end,
            writer,
            rx,
            rho_agent: None,
            rho_topics: Vec::new(),
            rho_rx,
            rho_state: None,
            rho_inserted_blocks: Vec::new(),
            rho_pending_inserted: None,
            rho_working_elisions: Vec::new(),
            user_message_gutter_ranges: HashMap::new(),
            _poll_task: poll_task,
            _subscriptions: ui_subscriptions,
            project_root: attach_target.project_root,
            prompt_state: PromptState::default(),
            cli_theme: cli_theme::select_theme(tau_config::settings::CliTheme::default()),
            tool_state: ToolState::default(),
            shell_state: ShellState::default(),
            current_model: None,
            current_role: None,
            baseline_params: None,
            role_state: RoleState::default(),
            current_params: ModelParams::default(),
            current_context_percent: None,
            current_context_input_tokens: None,
            current_context_window: None,
            main_tool_activity: MainToolActivity::default(),
            previous_provider_usage: None,
            agents: AgentState::default(),
            tasks: TaskState::default(),
            task_board,
            main_view: MainView::Agent,
            completion_state,
            displayed_agent_id: None,
            no_agent_ui_state: None,
            agent_ui_states: HashMap::new(),
        };
        this.update_prompt_inlay(cx);
        this.update_status_line(cx);
        this.insert_rho_banner_block(cx);
        this.focus_editor(window, cx);
        this
    }

    #[cfg(test)]
    fn new_for_test(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let completion_state = Arc::new(Mutex::new(TauCompletionState::default()));
        let this = cx.entity().downgrade();
        let ui_state = Self::new_agent_ui_state(this, completion_state.clone(), window, cx);
        let task_board = Self::new_task_board_ui_state(cx.entity().downgrade(), window, cx);
        let editor = ui_state.editor.clone();
        let prompt_buffer = ui_state.prompt_buffer.clone();
        let multi_buffer = ui_state.multi_buffer.clone();
        let transcript = ui_state.transcript;
        let prompt_end = ui_state.prompt_end;
        let draft_end = ui_state.draft_end;
        let ui_subscriptions = ui_state._subscriptions;
        let (_tx, rx) = mpsc::channel();
        let (_rho_tx, rho_rx) = mpsc::channel();
        let poll_task = cx.spawn(async move |_, _| {});

        let mut this = Self {
            editor,
            prompt_buffer,
            multi_buffer,
            transcript,
            prompt_end,
            draft_end,
            writer: None,
            rx,
            rho_agent: None,
            rho_topics: Vec::new(),
            rho_rx,
            rho_state: None,
            rho_inserted_blocks: Vec::new(),
            rho_pending_inserted: None,
            rho_working_elisions: Vec::new(),
            user_message_gutter_ranges: HashMap::new(),
            _poll_task: poll_task,
            _subscriptions: ui_subscriptions,
            project_root: std::env::temp_dir(),
            prompt_state: PromptState::default(),
            cli_theme: cli_theme::select_theme(tau_config::settings::CliTheme::default()),
            tool_state: ToolState::default(),
            shell_state: ShellState::default(),
            current_model: None,
            current_role: None,
            baseline_params: None,
            role_state: RoleState::default(),
            current_params: ModelParams::default(),
            current_context_percent: None,
            current_context_input_tokens: None,
            current_context_window: None,
            main_tool_activity: MainToolActivity::default(),
            previous_provider_usage: None,
            agents: AgentState::default(),
            tasks: TaskState::default(),
            task_board,
            main_view: MainView::Agent,
            completion_state,
            displayed_agent_id: None,
            no_agent_ui_state: None,
            agent_ui_states: HashMap::new(),
        };
        this.update_prompt_inlay(cx);
        this.update_status_line(cx);
        this
    }

    fn insert_rho_banner_block(&self, cx: &mut Context<Self>) {
        let anchor = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_before(Point::new(0, 0));
        let (version, build) = build_label_parts();
        let pun = startup_pun().to_owned();
        self.editor.update(cx, |editor, cx| {
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Above(anchor),
                    height: Some(4),
                    style: BlockStyle::Fixed,
                    render: Arc::new(move |cx| {
                        render_rho_banner_block(&version, &build, &pun, cx).into_any_element()
                    }),
                    priority: 0,
                }],
                None,
                cx,
            );
        });
    }

    fn spawn_rho_client(socket_path: PathBuf, tx: mpsc::Sender<RhoEvent>) {
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = tx.send(RhoEvent::Error(format!(
                        "failed to start async runtime: {error:#}"
                    )));
                    return;
                }
            };
            runtime.block_on(async move {
                let agent = match RhoAgentClient::connect(&socket_path).await {
                    Ok(agent) => agent,
                    Err(error) => {
                        let _ = tx.send(RhoEvent::Error(format!(
                            "failed to connect to {}: {error:#}",
                            socket_path.display()
                        )));
                        return;
                    }
                };
                if tx.send(RhoEvent::Connected(agent.clone())).is_err() {
                    return;
                }
                if tx.send(RhoEvent::Topics(agent.topics())).is_err() {
                    return;
                }
                if tx
                    .send(RhoEvent::KnownAgents(
                        agent
                            .known_agent_ids()
                            .into_iter()
                            .map(|agent_id| agent_id.to_string())
                            .collect(),
                    ))
                    .is_err()
                {
                    return;
                }
                for (agent_id, state) in agent.states() {
                    if tx
                        .send(RhoEvent::State(agent_id.to_string(), state))
                        .is_err()
                    {
                        return;
                    }
                }
                let mut frames = Box::pin(agent.subscribe_frames());
                while let Some((agent_id, frame)) = futures::StreamExt::next(&mut frames).await {
                    if tx
                        .send(RhoEvent::Frame(agent_id.to_string(), frame))
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = tx.send(RhoEvent::Disconnected);
            });
        });
    }

    #[allow(dead_code)]
    fn request_task_sync(&self) {}

    fn show_task_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_task_board(cx);
        self.main_view = MainView::Tasks;
        window.focus(&self.task_board.editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn refresh_task_board(&mut self, cx: &mut Context<Self>) {
        let render = self.tasks.render_full_board();
        self.task_board.buffer.update(cx, |buffer, cx| {
            buffer.set_text(render.text.as_str(), cx);
            self.task_board.rows = render
                .rows
                .into_iter()
                .map(|row| TaskRowAnchor {
                    task_id: row.task_id,
                    start: buffer.anchor_before(row.range.start),
                    end: buffer.anchor_after(row.range.end),
                })
                .collect();
        });
        self.task_board.multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                self.task_board.buffer.clone(),
                [Point::zero()..self.task_board.buffer.read(cx).max_point()],
                0,
                cx,
            );
        });
    }

    fn open_task_under_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task_id) = self.task_under_task_cursor(cx) else {
            return;
        };
        let Some(agent_id) = self.tasks.task_agent(task_id) else {
            return;
        };
        self.main_view = MainView::Agent;
        self.switch_to_agent_tab(Some(agent_id), window, cx);
        cx.notify();
    }

    fn task_under_task_cursor(&self, cx: &mut Context<Self>) -> Option<task_state::TaskId> {
        let cursor = self.task_board.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .anchor_to_buffer_anchor(editor.selections.newest_anchor().head())
                .map(|(anchor, _)| anchor)
        })?;
        let buffer = self.task_board.buffer.read(cx);
        let cursor = cursor.to_offset(buffer);
        self.task_board.rows.iter().find_map(|row| {
            let start = row.start.to_offset(buffer);
            let end = row.end.to_offset(buffer);
            (start <= cursor && cursor <= end).then_some(row.task_id)
        })
    }

    fn new_task_board_ui_state(
        this: WeakEntity<Self>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TaskBoardUiState {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::Read);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                buffer.clone(),
                [Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: true,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                None,
                window,
                cx,
            );
            editor.set_show_gutter(false, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_vertical_scrollbar(false, cx);
            editor.set_show_horizontal_scrollbar(false, cx);
            editor.set_offset_content(false, cx);
            editor.set_mouse_click_selection_enabled(true, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_autoindent(false);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_use_selection_highlight(false);
            editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
            editor.disable_expand_excerpt_buttons(cx);
            editor
        });
        let task_board_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &TaskBoard, window, cx| {
                if let Err(error) = this.update(cx, |this, cx| {
                    this.main_view = MainView::Agent;
                    this.focus_editor(window, cx);
                    cx.notify();
                }) {
                    eprintln!("rho-gui: failed to leave task board: {error:#}");
                }
            })
        });
        let task_open_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &TaskOpen, window, cx| {
                if let Err(error) =
                    this.update(cx, |this, cx| this.open_task_under_cursor(window, cx))
                {
                    eprintln!("rho-gui: failed to open selected task: {error:#}");
                }
            })
        });
        let submit_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &SubmitPrompt, window, cx| {
                if let Err(error) =
                    this.update(cx, |this, cx| this.open_task_under_cursor(window, cx))
                {
                    eprintln!("rho-gui: failed to open selected task: {error:#}");
                }
            })
        });
        TaskBoardUiState {
            editor,
            buffer,
            multi_buffer,
            rows: Vec::new(),
            _subscriptions: vec![
                task_board_subscription,
                task_open_subscription,
                submit_subscription,
            ],
        }
    }

    fn new_agent_ui_state(
        this: WeakEntity<Self>,
        completion_state: Arc<Mutex<TauCompletionState>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AgentUiState {
        let transcript_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let prompt_buffer = cx.new(|cx| Buffer::local("", cx));
        let prompt_start = prompt_buffer.read(cx).anchor_before(0);
        let prompt_end = prompt_start;
        let draft_end = prompt_buffer.read(cx).anchor_after(0);
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                transcript_buffer.clone(),
                [Point::zero()..transcript_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(1),
                prompt_buffer.clone(),
                [Point::zero()..prompt_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                None,
                window,
                cx,
            );
            editor.set_show_gutter(false, cx);
            editor.set_show_compact_gutter(true, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_vertical_scrollbar(false, cx);
            editor.set_show_horizontal_scrollbar(false, cx);
            editor.set_offset_content(false, cx);
            editor.set_mouse_click_selection_enabled(false, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_autoindent(false);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_use_selection_highlight(false);
            editor.disable_header_for_buffer(transcript_buffer.read(cx).remote_id(), cx);
            editor.disable_header_for_buffer(prompt_buffer.read(cx).remote_id(), cx);
            editor.disable_expand_excerpt_buttons(cx);
            editor.set_completion_provider(Some(Rc::new(TauCompletionProvider::new(
                completion_state.clone(),
            ))));
            editor
        });
        let prompt_buffer_subscription = cx.subscribe(&prompt_buffer, |this, _, event, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                this.update_prompt_inlay(cx);
            }
        });
        let submit_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &SubmitPrompt, window, cx| {
                eprintln!("rho-gui: SubmitPrompt action dispatched to editor");
                if let Err(error) = this.update(cx, |this, cx| this.submit_prompt(window, cx)) {
                    eprintln!("rho-gui: failed to submit prompt: {error:#}");
                }
            })
        });
        let agent_previous_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &AgentPrevious, window, cx| {
                if let Err(error) =
                    this.update(cx, |this, cx| this.switch_agent_by_delta(-1, window, cx))
                {
                    eprintln!("rho-gui: failed to switch to previous agent: {error:#}");
                }
            })
        });
        let agent_next_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &AgentNext, window, cx| {
                if let Err(error) =
                    this.update(cx, |this, cx| this.switch_agent_by_delta(1, window, cx))
                {
                    eprintln!("rho-gui: failed to switch to next agent: {error:#}");
                }
            })
        });
        let role_cycle_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &RoleCycle, _window, cx| {
                if let Err(error) = this.update(cx, |this, cx| {
                    this.cycle_role(RoleCycleKind::InnerGroup, cx)
                }) {
                    eprintln!("rho-gui: failed to cycle role: {error:#}");
                }
            })
        });
        let role_cycle_group_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &RoleCycleGroup, _window, cx| {
                if let Err(error) =
                    this.update(cx, |this, cx| this.cycle_role(RoleCycleKind::Group, cx))
                {
                    eprintln!("rho-gui: failed to cycle role group: {error:#}");
                }
            })
        });
        let agent_new_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &AgentNew, window, cx| {
                if let Err(error) = this.update(cx, |this, cx| {
                    this.clear_selected_agent(window, cx);
                    this.focus_editor(window, cx);
                }) {
                    eprintln!("rho-gui: failed to start a new agent draft: {error:#}");
                }
            })
        });
        let task_board_subscription = editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.register_action(move |_: &TaskBoard, window, cx| {
                if let Err(error) = this.update(cx, |this, cx| this.show_task_board(window, cx)) {
                    eprintln!("rho-gui: failed to show task board: {error:#}");
                }
            })
        });
        let draft_anchor = multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(draft_end);
        if let Some(draft_anchor) = draft_anchor {
            editor.update(cx, |editor, cx| {
                editor.set_autoscroll_pin(draft_anchor, AutoscrollStrategy::Bottom, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_anchor_ranges([draft_anchor..draft_anchor]);
                });
            });
        }
        let transcript =
            Transcript::new(transcript_buffer, editor.clone(), multi_buffer.clone(), cx);
        AgentUiState {
            editor,
            prompt_buffer,
            multi_buffer,
            transcript,
            prompt_end,
            draft_end,
            _subscriptions: vec![
                submit_subscription,
                role_cycle_subscription,
                role_cycle_group_subscription,
                agent_previous_subscription,
                agent_next_subscription,
                agent_new_subscription,
                task_board_subscription,
                prompt_buffer_subscription,
            ],
            prompt_state: PromptState::default(),
            tool_state: ToolState::default(),
            shell_state: ShellState::default(),
            main_tool_activity: MainToolActivity::default(),
            previous_provider_usage: None,
            current_context_percent: None,
            current_context_input_tokens: None,
            current_context_window: None,
            rho_state: None,
            rho_inserted_blocks: Vec::new(),
            rho_pending_inserted: None,
            rho_working_elisions: Vec::new(),
            user_message_gutter_ranges: HashMap::new(),
        }
    }

    fn drain_socket_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Ok(event) = self.rho_rx.try_recv() {
            self.handle_rho_event(event, window, cx);
        }
        while let Ok(event) = self.rx.try_recv() {
            match event {
                SocketEvent::Message(message) => self.handle_message(message, window, cx),
                SocketEvent::Disconnected(reason) => {
                    self.insert_before_draft_styled(
                        &format!("\n[disconnected: {reason}]\n"),
                        TranscriptStyle::SystemDisconnect,
                        cx,
                    );
                }
            }
        }
    }

    fn handle_rho_event(&mut self, event: RhoEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            RhoEvent::Connected(agent) => {
                self.rho_agent = Some(agent);
                self.rho_state = None;
                self.clear_rho_rendered_blocks(cx);
                self.rho_pending_inserted = None;
                self.current_role = Some("rho".to_owned());
                self.current_model = None;
                self.update_status_line(cx);
            }
            RhoEvent::Topics(topics) => {
                self.rho_topics = topics;
            }
            RhoEvent::KnownAgents(agent_ids) => {
                for agent_id in agent_ids {
                    self.agents.remember(agent_id);
                }
                self.refresh_agent_completions();
                self.update_status_line(cx);
            }
            RhoEvent::State(agent_id, state) => self.handle_rho_state(agent_id, state, window, cx),
            RhoEvent::Frame(agent_id, frame) => self.handle_rho_frame(agent_id, frame, window, cx),
            RhoEvent::Disconnected => {
                self.insert_before_draft_styled(
                    "\n[disconnected from rho daemon]\n",
                    TranscriptStyle::SystemDisconnect,
                    cx,
                );
            }
            RhoEvent::Error(message) => {
                self.insert_before_draft_styled(
                    &format!("\n[{message}]\n"),
                    TranscriptStyle::SystemDisconnect,
                    cx,
                );
            }
        }
    }

    fn handle_rho_state(
        &mut self,
        agent_id: String,
        state: RhoUiAgentState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agents.mark_live(agent_id.clone());
        if self.agents.current_agent_id().is_none() {
            self.agents.select(agent_id.clone());
            self.show_agent_transcript(Some(agent_id.clone()), window, cx);
            if self.main_view == MainView::Agent {
                self.focus_editor(window, cx);
            }
        }
        if self.displayed_agent_id.as_deref() == Some(agent_id.as_str()) {
            self.render_rho_state(&state, window, cx);
        } else {
            self.hidden_agent_ui_state_mut(agent_id, window, cx)
                .rho_state = Some(state);
        }
        self.update_status_line(cx);
    }

    fn handle_rho_frame(
        &mut self,
        agent_id: String,
        frame: RhoAgentRemoteFrame,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agents.mark_live(agent_id.clone());
        if self.agents.current_agent_id().is_none() {
            self.agents.select(agent_id.clone());
            self.show_agent_transcript(Some(agent_id.clone()), window, cx);
            if self.main_view == MainView::Agent {
                self.focus_editor(window, cx);
            }
        }
        if self.displayed_agent_id.as_deref() == Some(agent_id.as_str()) {
            let mut state = self
                .rho_state
                .clone()
                .unwrap_or_else(empty_rho_ui_agent_state);
            frame.apply_diff(&mut state);
            self.render_rho_state(&state, window, cx);
            self.rho_state = Some(state);
        } else {
            let ui_state = self.hidden_agent_ui_state_mut(agent_id, window, cx);
            let mut state = ui_state
                .rho_state
                .clone()
                .unwrap_or_else(empty_rho_ui_agent_state);
            frame.apply_diff(&mut state);
            ui_state.rho_state = Some(state);
        }
        self.update_status_line(cx);
    }

    fn hidden_agent_ui_state_mut(
        &mut self,
        agent_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> &mut AgentUiState {
        if self.displayed_agent_id.as_deref() == Some(agent_id.as_str()) {
            unreachable!("visible agent state is stored directly on RhoGui");
        }
        if !self.agent_ui_states.contains_key(&agent_id) {
            let state = self.empty_agent_ui_state(window, cx);
            self.agent_ui_states.insert(agent_id.clone(), state);
        }
        self.agent_ui_states
            .get_mut(&agent_id)
            .expect("hidden agent state just inserted")
    }

    fn render_rho_state(
        &mut self,
        state: &RhoUiAgentState,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first_changed = self
            .rho_state
            .as_ref()
            .map(|previous| {
                previous
                    .blocks
                    .iter()
                    .zip(&state.blocks)
                    .position(|(previous, current)| previous != current)
                    .unwrap_or_else(|| previous.blocks.len().min(state.blocks.len()))
            })
            .unwrap_or(0);
        let blocks_changed =
            first_changed < self.rho_inserted_blocks.len() || first_changed < state.blocks.len();

        if blocks_changed {
            self.remove_rho_pending(cx);
        }

        if first_changed < self.rho_inserted_blocks.len() {
            self.remove_rho_rendered_blocks_from(first_changed, cx);
        }

        for block in &state.blocks[self.rho_inserted_blocks.len()..] {
            let inserted = match block {
                RhoUiBlock::UserMessage { text } => {
                    self.insert_user_message(text, TranscriptStyle::UserPrompt, cx)
                }
                _ => {
                    let spans = render_rho_block_spans(block, &self.cli_theme, cx);
                    self.insert_rho_spans(spans, cx)
                }
            };
            self.rho_inserted_blocks.push(inserted);
        }

        let pending_spans = render_rho_pending_spans(&state.pending_response, &self.cli_theme, cx);
        if pending_spans.is_empty() {
            self.remove_rho_pending(cx);
        } else if !blocks_changed && let Some(inserted) = self.rho_pending_inserted.take() {
            self.rho_pending_inserted = self.replace_rho_spans(inserted, pending_spans, cx);
        } else {
            self.rho_pending_inserted = self.insert_rho_spans(pending_spans, cx);
        }
        self.elide_rho_working_blocks(state, cx);

        self.sync_rendered_user_message_gutter_ranges(&state.blocks, cx);
        self.rho_state = Some(state.clone());
        self.current_context_percent = None;
        self.current_context_input_tokens = None;
        self.current_context_window = None;
        self.update_status_line(cx);
        cx.notify();
    }

    fn rerender_current_rho_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.rho_state.clone() else {
            return;
        };
        self.clear_rho_working_elisions(cx);
        self.remove_rho_pending(cx);
        self.clear_rho_rendered_blocks(cx);
        self.rho_state = None;
        self.render_rho_state(&state, window, cx);
    }

    fn clear_rho_rendered_blocks(&mut self, cx: &mut Context<Self>) {
        self.remove_rho_rendered_blocks_from(0, cx);
    }

    fn remove_rho_rendered_blocks_from(&mut self, index: usize, cx: &mut Context<Self>) {
        let removed = self.rho_inserted_blocks.split_off(index);
        for inserted in removed.into_iter().rev().flatten() {
            self.remove_transcript_highlights(inserted.highlight_keys, cx);
            self.remove_transcript_range(inserted.range, cx);
        }
    }

    fn remove_rho_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(inserted) = self.rho_pending_inserted.take() {
            self.remove_transcript_highlights(inserted.highlight_keys, cx);
            self.remove_transcript_range(inserted.range, cx);
        }
    }

    fn insert_rho_spans(
        &mut self,
        spans: Vec<(String, HighlightStyle)>,
        cx: &mut Context<Self>,
    ) -> Option<InsertedTranscript> {
        self.insert_before_draft_spans(
            spans.iter().map(|(text, style)| (text.as_str(), *style)),
            cx,
        )
    }

    fn replace_rho_spans(
        &mut self,
        inserted: InsertedTranscript,
        spans: Vec<(String, HighlightStyle)>,
        cx: &mut Context<Self>,
    ) -> Option<InsertedTranscript> {
        self.remove_transcript_highlights(inserted.highlight_keys, cx);
        self.replace_transcript_range_with_spans(
            inserted.range,
            spans.iter().map(|(text, style)| (text.as_str(), *style)),
            cx,
        )
    }

    fn clear_rho_working_elisions(&mut self, cx: &mut Context<Self>) {
        if self.rho_working_elisions.is_empty() {
            return;
        }

        let ids = std::mem::take(&mut self.rho_working_elisions)
            .into_iter()
            .map(|elision| elision.id)
            .collect::<rustc_hash::FxHashSet<_>>();
        self.editor.update(cx, |editor, cx| {
            editor.remove_display_elisions(ids, None, cx);
        });
    }

    fn elide_rho_working_blocks(&mut self, state: &RhoUiAgentState, cx: &mut Context<Self>) {
        let candidates = self.rho_working_elision_candidates(state);
        if candidates.is_empty() {
            self.clear_rho_working_elisions(cx);
            return;
        }

        let mut removed_ids = self.rho_working_elisions
            [candidates.len().min(self.rho_working_elisions.len())..]
            .iter()
            .map(|elision| elision.id)
            .collect::<rustc_hash::FxHashSet<_>>();
        let mut updates = Vec::new();
        let mut inserted_candidates = Vec::new();
        let mut inserted_properties = Vec::new();
        let mut elisions = Vec::new();

        for (index, candidate) in candidates.iter().enumerate() {
            let Some(properties) = self.rho_working_elision_properties(
                candidate.range.clone(),
                candidate.tool_count,
                candidate.tail_rows,
                cx,
            ) else {
                if let Some(elision) = self.rho_working_elisions.get(index) {
                    removed_ids.insert(elision.id);
                }
                continue;
            };

            if let Some(elision) = self.rho_working_elisions.get(index) {
                if elision.range != candidate.range
                    || elision.tool_count != candidate.tool_count
                    || elision.tail_rows != candidate.tail_rows
                {
                    updates.push((elision.id, properties));
                }
                elisions.push(RhoWorkingElision {
                    id: elision.id,
                    range: candidate.range.clone(),
                    tool_count: candidate.tool_count,
                    tail_rows: candidate.tail_rows,
                });
            } else {
                inserted_candidates.push(candidate.clone());
                inserted_properties.push(properties);
            }
        }

        let inserted_ids = self.editor.update(cx, |editor, cx| {
            if !removed_ids.is_empty() {
                editor.remove_display_elisions(removed_ids, None, cx);
            }
            if !updates.is_empty() {
                editor.update_display_elisions(updates, None, cx);
            }
            editor.insert_display_elisions(inserted_properties, None, cx)
        });

        elisions.extend(inserted_ids.into_iter().zip(inserted_candidates).map(
            |(id, candidate)| RhoWorkingElision {
                id,
                range: candidate.range,
                tool_count: candidate.tool_count,
                tail_rows: candidate.tail_rows,
            },
        ));
        self.rho_working_elisions = elisions;
    }

    fn rho_working_elision_candidates(
        &self,
        state: &RhoUiAgentState,
    ) -> Vec<RhoWorkingElisionCandidate> {
        let mut ranges = Vec::new();
        let mut current: Option<(std::ops::Range<text::Anchor>, usize, u32)> = None;
        let active_turn_has_pending_non_working_response = state
            .pending_response
            .iter()
            .any(|item| !rho_pending_item_is_working(item));
        let pending_tool_count = state
            .pending_response
            .iter()
            .filter(|item| matches!(item, RhoUiStreamingItem::Tool(_)))
            .count();

        for (index, (block, inserted)) in state
            .blocks
            .iter()
            .zip(&self.rho_inserted_blocks)
            .enumerate()
        {
            let Some(inserted) = inserted else {
                continue;
            };
            let mut turn_tool_count = rho_turn_tool_count(state, index);
            if rho_block_is_in_active_turn(state, index) {
                turn_tool_count += pending_tool_count;
            }
            let tail_rows = if rho_turn_has_non_working_response(state, index)
                || (active_turn_has_pending_non_working_response
                    && rho_block_is_in_active_turn(state, index))
            {
                0
            } else {
                RHO_LIMITED_TAIL_ROWS
            };
            let range = if rho_block_is_working(block) {
                Some(inserted.range.clone())
            } else {
                None
            };

            match (range, current.take()) {
                (Some(range), Some((current_range, current_tool_count, current_tail_rows)))
                    if current_tool_count == turn_tool_count && current_tail_rows == tail_rows =>
                {
                    current = Some((
                        current_range.start..range.end,
                        current_tool_count,
                        current_tail_rows,
                    ));
                }
                (Some(range), previous) => {
                    if let Some(previous) = previous {
                        ranges.push(previous);
                    }
                    current = Some((range, turn_tool_count, tail_rows));
                }
                (None, Some(previous)) => ranges.push(previous),
                (None, None) => {}
            }
        }

        if !state.pending_response.is_empty()
            && state
                .pending_response
                .iter()
                .all(rho_pending_item_is_working)
            && let Some(inserted) = &self.rho_pending_inserted
        {
            let tool_count = rho_active_turn_tool_count(state) + pending_tool_count;
            let range = inserted.range.clone();
            match current.take() {
                Some((current_range, current_tool_count, current_tail_rows))
                    if current_tool_count == tool_count
                        && current_tail_rows == RHO_LIMITED_TAIL_ROWS =>
                {
                    current = Some((
                        current_range.start..range.end,
                        current_tool_count,
                        RHO_LIMITED_TAIL_ROWS,
                    ));
                }
                previous => {
                    if let Some(previous) = previous {
                        ranges.push(previous);
                    }
                    current = Some((range, tool_count, RHO_LIMITED_TAIL_ROWS));
                }
            }
        }

        if let Some(previous) = current {
            ranges.push(previous);
        }

        ranges
            .into_iter()
            .map(
                |(range, tool_count, tail_rows)| RhoWorkingElisionCandidate {
                    range,
                    tool_count,
                    tail_rows,
                },
            )
            .collect()
    }

    fn rho_working_elision_properties(
        &self,
        range: std::ops::Range<text::Anchor>,
        tool_count: usize,
        tail_rows: u32,
        cx: &Context<Self>,
    ) -> Option<DisplayElisionProperties<multi_buffer::Anchor>> {
        let label = rho_working_elision_label(tool_count);
        self.transcript
            .multibuffer_range(range, cx)
            .map(|range| DisplayElisionProperties {
                range,
                tail_rows,
                height: Some(1),
                style: BlockStyle::Flex,
                render: Arc::new(move |cx| {
                    render_rho_working_elision_block(&label, cx).into_any_element()
                }),
                priority: 0,
                type_tag: None,
            })
    }

    fn handle_message(
        &mut self,
        message: PeerInputMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match message {
            PeerInputMessage::Deliver(delivery) => {
                self.handle_event(delivery.into_event(), window, cx)
            }
            PeerInputMessage::Disconnect(disconnect) => {
                self.insert_before_draft_styled(
                    &format!(
                        "\n[daemon disconnected: {}]\n",
                        disconnect.reason.unwrap_or_else(|| "no reason".to_owned())
                    ),
                    TranscriptStyle::SystemDisconnect,
                    cx,
                );
            }
            _ => {}
        }
    }

    fn handle_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        let previous_agent_id = self.agents.current_agent_id_owned();
        if let Some(agent_id) = self.agents.agent_id_for_event(&event) {
            self.agents.remember(agent_id);
        }
        self.agents.observe_event(&event);
        if self.tasks.observe_event(&event) {
            self.refresh_task_board(cx);
            cx.notify();
        }
        self.refresh_agent_completions();
        if self.agents.current_agent_id() != previous_agent_id.as_deref() {
            let current_agent_id = self.agents.current_agent_id_owned();
            if self.displayed_agent_id != current_agent_id {
                self.show_agent_transcript(current_agent_id, window, cx);
            }
            self.apply_selected_agent_context_usage();
            self.update_status_line(cx);
            self.update_prompt_inlay(cx);
            self.focus_editor(window, cx);
        }
        match event {
            Event::TermBell(_) => {
                Audio::play_sound(Sound::AgentDone, cx);
            }
            Event::UiPromptSubmitted(_) => {}
            Event::AgentPromptSubmitted(prompt)
                if prompt.originator.is_user() && !prompt.message_class.is_internal() =>
            {
                self.handle_submitted_user_prompt(&prompt.text, cx);
            }
            Event::AgentPromptQueued(queued) if !queued.message_class.is_internal() => {
                self.handle_agent_prompt_queued(&queued.text, cx);
            }
            Event::AgentMessageSent(message) => {
                self.insert_before_draft_styled(
                    &format!(
                        "{}:\n{}\n",
                        agent_message_sent_summary(&message),
                        message.message
                    ),
                    TranscriptStyle::SystemInfo,
                    cx,
                );
            }
            Event::AgentMessageReceived(message) => {
                self.insert_before_draft_styled(
                    &format!(
                        "Message from {} to {}:\n{}\n",
                        message.sender_id, message.recipient_id, message.message
                    ),
                    TranscriptStyle::SystemInfo,
                    cx,
                );
            }
            Event::ProviderResponseUpdated(update) if update.originator.is_user() => {
                let key = update.agent_prompt_id.to_string();
                self.update_live_compaction(
                    key.as_str(),
                    provider_update_compaction_status(&update),
                    cx,
                );
                let text = assistant_text_from_update(&update).unwrap_or_default();
                let text = self
                    .prompt_state
                    .append_streamed_response(key.clone(), text);
                self.upsert_live_response(key, text.as_str(), cx);
            }
            Event::ProviderResponseFinished(finished) if finished.originator.is_user() => {
                let key = finished.agent_prompt_id.to_string();
                self.remove_live_compaction(key.as_str(), cx);
                if let Some(text) = assistant_text(&finished.output_items) {
                    match self.prompt_state.remove_streamed_response(&key) {
                        Some(_) | None => {
                            if !text.is_empty() {
                                self.finalize_live_response(key.as_str(), &text, cx);
                            }
                        }
                    }
                } else {
                    self.remove_live_response(key.as_str(), cx);
                }
                if finished.output_items.is_empty() {
                    let text = finished
                        .error
                        .as_deref()
                        .unwrap_or("(provider returned an empty response)");
                    self.insert_before_draft_styled(
                        &format!("{text}\n"),
                        TranscriptStyle::SystemImportant,
                        cx,
                    );
                    self.render_turn_stats(&finished, cx);
                    self.ensure_transcript_gap(cx);
                    return;
                }
                if let Some(error) = &finished.error {
                    self.insert_before_draft_styled(
                        &format!("[provider error: {error}]\n"),
                        TranscriptStyle::SystemImportant,
                        cx,
                    );
                }
                for item in &finished.output_items {
                    if matches!(item, ContextItem::Compaction(_)) {
                        let block = tool_render::render_compaction_block(
                            &self.cli_theme,
                            compaction_success_status(
                                finished.compaction_original_input_tokens,
                                finished.compaction_compacted_input_tokens,
                            ),
                            tool_render::CompactionStatus::Success,
                        );
                        self.insert_before_draft_block(block, cx);
                    }
                }
                let tool_calls = tool_calls_from_output_items(&finished.output_items);
                if !tool_calls.is_empty() {
                    self.main_tool_activity
                        .add_requested_tools(tool_calls.len());
                    self.update_status_line(cx);
                }
                for call in tool_calls {
                    let block = render_tool_call_block(&self.cli_theme, &call);
                    if let Some(inserted) = self.insert_before_draft_block(block, cx) {
                        self.tool_state
                            .insert_pending(call.call_id.to_string(), inserted);
                    }
                }
                self.render_turn_stats(&finished, cx);
                self.ensure_transcript_gap(cx);
            }
            Event::AgentPromptRecalled(recalled) => {
                if let Some(queued) = self.prompt_state.pop_back_queued_prompt() {
                    self.remove_queued_prompt(queued, cx);
                }
                let agent_id = recalled.agent_id.to_string();
                self.show_agent_transcript(Some(agent_id.clone()), window, cx);
                self.agents.select(agent_id);
                self.replace_draft_text(&recalled.text, cx);
                self.insert_before_draft_styled(
                    "> recalled queued prompt for editing\n",
                    TranscriptStyle::SystemInfo,
                    cx,
                );
            }
            Event::AgentPromptSteered(steered) if !steered.message_class.is_internal() => {
                self.handle_agent_prompt_steered(&steered.text, cx);
            }
            Event::AgentPromptCreated(_) => {
                self.promote_next_queued_prompt(cx);
            }
            Event::AgentCompactionTriggered(triggered) if triggered.originator.is_user() => {
                let block = tool_render::render_compaction_block(
                    &self.cli_theme,
                    format!("requested #{}", triggered.agent_id),
                    tool_render::CompactionStatus::Progress,
                );
                self.insert_before_draft_block(block, cx);
            }
            Event::AgentPromptTerminated(terminated) if terminated.originator.is_user() => {
                let key = terminated.agent_prompt_id.to_string();
                self.remove_live_response(key.as_str(), cx);
                self.remove_live_compaction(key.as_str(), cx);
                self.insert_before_draft_styled(
                    &format!(
                        "[prompt {}: {key}]\n",
                        agent_prompt_termination_reason(terminated.reason)
                    ),
                    TranscriptStyle::SystemInfo,
                    cx,
                );
            }
            Event::ToolDelegateProgress(progress) => {
                if let Some(agent_id) = &progress.agent_id {
                    self.agents.mark_live(agent_id.clone());
                }
                let display = progress
                    .display
                    .clone()
                    .unwrap_or_else(|| tau_proto::ToolUseState {
                        args: progress.task_name.clone(),
                        status: tau_proto::ToolUseStatus::InProgress,
                        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                        ..Default::default()
                    });
                self.upsert_delegate_display(
                    progress.call_id.as_str(),
                    &display,
                    progress.agent_id.as_deref(),
                    progress.role.as_deref(),
                    cx,
                );
                self.update_status_line(cx);
            }
            Event::ToolProgress(progress) => {
                if let Some(display) = progress.display.as_ref() {
                    self.upsert_tool_display(
                        progress.call_id.as_str(),
                        &progress.tool_name,
                        display,
                        cx,
                    );
                } else {
                    let text = tau_harness::format_tool_progress(&progress);
                    if !text.is_empty() {
                        self.insert_before_draft_styled(
                            &format!("{text}\n"),
                            TranscriptStyle::ToolProgress,
                            cx,
                        );
                    }
                }
            }
            Event::ToolResult(result) if result.originator.is_user() => {
                if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
                    self.record_main_tool_backgrounded(result.call_id.as_str());
                } else {
                    let block = render_tool_result_block(&self.cli_theme, &result);
                    self.finish_tool_call(result.call_id.as_str(), block, cx);
                    self.record_main_tool_completed(result.call_id.as_str());
                }
                self.update_status_line(cx);
            }
            Event::ProviderToolResult(result)
                if result.originator.is_user()
                    || self.tool_state.contains_pending(result.call_id.as_str()) =>
            {
                if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
                    self.record_main_tool_backgrounded(result.call_id.as_str());
                } else {
                    let block = render_tool_result_parts_block(
                        &self.cli_theme,
                        &result.tool_name,
                        &result.result,
                        result.display.as_ref(),
                    );
                    self.finish_tool_call(result.call_id.as_str(), block, cx);
                    self.record_main_tool_completed(result.call_id.as_str());
                }
                self.update_status_line(cx);
            }
            Event::ToolBackgroundResult(result)
                if result.originator.is_user()
                    || self.tool_state.contains_pending(result.call_id.as_str())
                    || self
                        .main_tool_activity
                        .is_backgrounded(result.call_id.as_str()) =>
            {
                let block = render_tool_result_parts_block(
                    &self.cli_theme,
                    &result.tool_name,
                    &result.result,
                    result.display.as_ref(),
                );
                self.finish_tool_call(result.call_id.as_str(), block, cx);
                self.record_main_tool_completed(result.call_id.as_str());
                self.update_status_line(cx);
            }
            Event::ToolError(error) if error.originator.is_user() => {
                let block = render_tool_error_block(&self.cli_theme, &error);
                self.finish_tool_call(error.call_id.as_str(), block, cx);
                self.record_main_tool_completed(error.call_id.as_str());
                self.update_status_line(cx);
            }
            Event::ProviderToolError(error)
                if error.originator.is_user()
                    || self.tool_state.contains_pending(error.call_id.as_str()) =>
            {
                let block = render_tool_error_parts_block(
                    &self.cli_theme,
                    &error.tool_name,
                    &error.message,
                    error.display.as_ref(),
                );
                self.finish_tool_call(error.call_id.as_str(), block, cx);
                self.record_main_tool_completed(error.call_id.as_str());
                self.update_status_line(cx);
            }
            Event::ToolBackgroundError(error)
                if error.originator.is_user()
                    || self.tool_state.contains_pending(error.call_id.as_str())
                    || self
                        .main_tool_activity
                        .is_backgrounded(error.call_id.as_str()) =>
            {
                let block = render_tool_error_parts_block(
                    &self.cli_theme,
                    &error.tool_name,
                    &error.message,
                    error.display.as_ref(),
                );
                self.finish_tool_call(error.call_id.as_str(), block, cx);
                self.record_main_tool_completed(error.call_id.as_str());
                self.update_status_line(cx);
            }
            Event::ToolRejected(rejected) if rejected.originator.is_user() => {
                let block = render_tool_error_parts_block(
                    &self.cli_theme,
                    &rejected.tool_name,
                    &rejected.message,
                    None,
                );
                self.finish_tool_call(rejected.call_id.as_str(), block, cx);
                self.record_main_tool_completed(rejected.call_id.as_str());
                self.update_status_line(cx);
            }
            Event::ToolCancelled(cancelled) => {
                if self.tool_state.contains_pending(cancelled.call_id.as_str())
                    || self
                        .main_tool_activity
                        .is_backgrounded(cancelled.call_id.as_str())
                {
                    let block = render_tool_error_parts_block(
                        &self.cli_theme,
                        &cancelled.tool_name,
                        "cancelled",
                        None,
                    );
                    self.finish_tool_call(cancelled.call_id.as_str(), block, cx);
                    self.record_main_tool_completed(cancelled.call_id.as_str());
                    self.update_status_line(cx);
                }
            }
            Event::UiShellCommand(command) => {
                let label = shell_running_label(command.include_in_context);
                let block = tool_render::render_shell_block(
                    &self.cli_theme,
                    &command.command,
                    "",
                    Some(label.as_str()),
                );
                if let Some(inserted) = self.insert_before_draft_block(block, cx) {
                    self.shell_state.insert(
                        command.command_id.to_string(),
                        ShellCommandState {
                            inserted,
                            command: command.command,
                            include_in_context: command.include_in_context,
                            output: String::new(),
                        },
                    );
                }
            }
            Event::ShellCommandProgress(progress) => {
                if let Some(mut state) = self.shell_state.take(progress.command_id.as_str()) {
                    state.output.push_str(&progress.chunk);
                    let label = shell_running_label(state.include_in_context);
                    let block = tool_render::render_shell_block(
                        &self.cli_theme,
                        &state.command,
                        &state.output,
                        Some(label.as_str()),
                    );
                    if let Some(inserted) = self.replace_transcript_block(state.inserted, block, cx)
                    {
                        state.inserted = inserted;
                        self.shell_state
                            .insert(progress.command_id.to_string(), state);
                    }
                }
            }
            Event::ShellCommandFinished(finished) => {
                let include_in_context =
                    if let Some(state) = self.shell_state.take(finished.command_id.as_str()) {
                        self.remove_transcript_highlights(state.inserted.highlight_keys, cx);
                        self.remove_transcript_range(state.inserted.range, cx);
                        state.include_in_context
                    } else {
                        finished.include_in_context
                    };
                let status = shell_finished_suffix(&finished, include_in_context);
                let block = tool_render::render_shell_block(
                    &self.cli_theme,
                    &finished.command,
                    &finished.output,
                    Some(status.as_str()),
                );
                self.insert_before_draft_block(block, cx);
            }
            Event::ActionResult(result) => {
                let text = match result.output {
                    tau_proto::ActionOutput::Text { text } => text,
                    tau_proto::ActionOutput::EditorBuffer {
                        title,
                        text,
                        editable,
                    } => {
                        let mut rendered = format!("{title}\n{text}");
                        if editable {
                            rendered.push_str("\n[editable buffer]");
                        }
                        rendered
                    }
                };
                let block = tool_render::render_action_output_block(&self.cli_theme, &text);
                self.insert_before_draft_block(block, cx);
            }
            Event::ActionError(error) => {
                let block = tool_render::render_action_error_block(
                    &self.cli_theme,
                    &error.action_id,
                    &error.message,
                );
                self.insert_before_draft_block(block, cx);
            }
            Event::ActionSchemaPublished(published) => {
                if let Ok(mut state) = self.completion_state.lock() {
                    state.apply_action_schema(&published);
                }
            }
            Event::ExtensionStarting(starting) => {
                let status = starting.pid.map_or_else(
                    || "starting".to_owned(),
                    |pid| format!("starting pid {pid}"),
                );
                let block = tool_render::extension_status_block(
                    &self.cli_theme,
                    &starting.extension_name,
                    &status,
                );
                self.insert_before_draft_block(block, cx);
            }
            Event::ExtensionReady(ready) => {
                let status = ready
                    .pid
                    .map_or_else(|| "ready".to_owned(), |pid| format!("ready pid {pid}"));
                let block = tool_render::extension_status_block(
                    &self.cli_theme,
                    &ready.extension_name,
                    &status,
                );
                self.insert_before_draft_block(block, cx);
            }
            Event::ExtensionExited(exited) => {
                if let Ok(mut state) = self.completion_state.lock() {
                    state.remove_extension(&exited.extension_name, exited.instance_id);
                }
                let status = match (exited.exit_code, exited.signal) {
                    (Some(code), _) => format!("exited {code}"),
                    (_, Some(signal)) => format!("signal {signal}"),
                    (None, None) => "exited".to_owned(),
                };
                let block = tool_render::extension_status_block(
                    &self.cli_theme,
                    &exited.extension_name,
                    &status,
                );
                self.insert_before_draft_block(block, cx);
            }
            Event::ExtensionRestarting(restarting) => {
                let status = restarting.reason.as_ref().map_or_else(
                    || format!("restarting #{}", restarting.attempt),
                    |reason| format!("restarting #{}: {reason}", restarting.attempt),
                );
                let block = tool_render::extension_status_block(
                    &self.cli_theme,
                    &restarting.extension_name,
                    &status,
                );
                self.insert_before_draft_block(block, cx);
            }
            Event::ExtAgentsMdAvailable(agents_md) => {
                let block = tool_render::system_loaded_block(
                    &self.cli_theme,
                    &agents_md.file_path,
                    &agents_md.content,
                );
                self.insert_before_draft_block(block, cx);
            }
            Event::ExtensionContextReady(ready) => {
                let block =
                    tool_render::agent_context_ready_block(&self.cli_theme, &ready.agent_id);
                self.insert_before_draft_block(block, cx);
            }
            Event::HarnessUiDir(ui_dir) => {
                let block = tool_render::ui_dir_block(&self.cli_theme, &ui_dir.path);
                self.insert_before_draft_block(block, cx);
            }
            Event::HarnessNotice(notice) => {
                let block = tool_render::render_harness_notice(&self.cli_theme, &notice);
                self.insert_before_draft_block(block, cx);
            }
            Event::HarnessRolesAvailable(roles) => {
                self.role_state.update_available(&roles);
                self.refresh_role_completions(&roles);
                self.update_status_line(cx);
            }
            Event::HarnessRoleSelected(selected) => {
                self.current_model = selected.model.clone();
                self.current_role = Some(selected.role);
                self.baseline_params = selected.baseline_params;
                self.current_params = selected.model_params;
                self.current_context_window = selected.context_window;
                self.update_status_line(cx);
                self.update_prompt_inlay(cx);
            }
            Event::HarnessContextUsageChanged(changed) => {
                self.current_context_input_tokens = changed.input_tokens;
                self.current_context_percent = changed.percent_used;
                self.update_status_line(cx);
            }
            Event::HarnessAgentContextUsageChanged(changed) => {
                let agent_id = changed.agent_id.to_string();
                self.agents.record_context_usage(
                    agent_id.clone(),
                    AgentContextUsage {
                        input_tokens: changed.input_tokens,
                        percent_used: changed.percent_used,
                        context_window: changed.context_window,
                    },
                );
                if self.agents.current_agent_id() == Some(agent_id.as_str()) {
                    self.apply_selected_agent_context_usage();
                    self.update_status_line(cx);
                }
            }
            Event::HarnessStarted(_) => {
                self.main_tool_activity.reset();
                self.previous_provider_usage = None;
                self.agents.clear_context_usage();
                self.agents.clear_routing();
                self.update_status_line(cx);
            }
            _ => {}
        }
    }

    fn apply_selected_agent_context_usage(&mut self) {
        if let Some(usage) = self.agents.selected_context_usage() {
            self.current_context_input_tokens = usage.input_tokens;
            self.current_context_percent = usage.percent_used;
            self.current_context_window = usage.context_window;
        } else {
            self.current_context_input_tokens = None;
            self.current_context_percent = None;
        }
    }

    fn selected_agent_is_active(&self) -> bool {
        self.agents.selected_is_active()
    }

    fn selected_agent_proto_id(&self) -> Option<tau_proto::AgentId> {
        let agent_id = self.agents.current_agent_id_owned()?;
        match tau_proto::AgentId::parse(&agent_id) {
            Ok(agent_id) => Some(agent_id),
            Err(error) => {
                eprintln!("rho-gui: invalid selected agent id {agent_id:?}: {error}");
                None
            }
        }
    }

    fn send_event(&mut self, event: Event, cx: &mut Context<Self>) -> bool {
        let Some(writer) = &self.writer else {
            eprintln!("rho-gui: command ignored because socket writer is unavailable");
            return false;
        };
        let message = HarnessInputMessage::emit(event);
        if let Err(error) = socket_client::send_message(writer, &message) {
            eprintln!("rho-gui: send failed: {error:#}");
            self.insert_before_draft_styled(
                &format!("\n[send failed: {error}]\n"),
                TranscriptStyle::SystemDisconnect,
                cx,
            );
            return false;
        }
        true
    }

    fn send_command_event(&mut self, event: Event, cx: &mut Context<Self>) -> bool {
        self.send_event(event, cx);
        true
    }

    fn is_prompt_command(text: &str) -> bool {
        text == "/cancel"
            || text == "/tree"
            || text.starts_with("/tree ")
            || text == "/compact"
            || text.starts_with("/compact ")
            || text == "/new"
            || text.starts_with("/load ")
            || text == "/model"
            || text.starts_with("/model ")
            || text == "/role"
            || text.starts_with("/role ")
            || text.starts_with('!')
    }

    fn handle_prompt_command(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if text == "/cancel" {
            if let Some(agent) = &self.rho_agent {
                if let Some(agent_id) = self.agents.current_agent_id_owned() {
                    if let Ok(agent_id) = RhoAgentId::from_str(&agent_id) {
                        agent.cancel(agent_id);
                    }
                }
                return true;
            }
            return self.send_command_event(
                Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
                    target_agent_id: self.selected_agent_proto_id(),
                    agent_prompt_id: None,
                }),
                cx,
            );
        }
        if text == "/tree" {
            return self.send_command_event(
                Event::UiTreeRequest(tau_proto::UiTreeRequest {
                    target_agent_id: self.selected_agent_proto_id(),
                }),
                cx,
            );
        }
        if let Some(node_id) = text.strip_prefix("/tree ") {
            let Ok(node_id) = node_id.trim().parse::<u64>() else {
                self.insert_before_draft_styled(
                    "/tree <id>: id must be a non-negative integer\n",
                    TranscriptStyle::SystemInfo,
                    cx,
                );
                return true;
            };
            return self.send_command_event(
                Event::UiNavigateTree(tau_proto::UiNavigateTree {
                    target_agent_id: self.selected_agent_proto_id(),
                    node_id,
                }),
                cx,
            );
        }
        if text == "/compact" {
            return self.send_command_event(
                Event::UiCompactRequest(tau_proto::UiCompactRequest {
                    target_agent_id: self.selected_agent_proto_id(),
                }),
                cx,
            );
        }
        if text.starts_with("/compact ") {
            self.insert_before_draft_styled(
                "/compact forces a compaction pass and takes no arguments\n",
                TranscriptStyle::SystemInfo,
                cx,
            );
            return true;
        }
        if text == "/new" {
            self.clear_selected_agent(window, cx);
            return true;
        }
        if let Some(agent_id) = text.strip_prefix("/load ") {
            let agent_id = agent_id.trim();
            if agent_id.is_empty() {
                self.insert_before_draft_styled(
                    "/load <agent-id>\n",
                    TranscriptStyle::SystemInfo,
                    cx,
                );
                return true;
            }
            if let Some(agent) = &self.rho_agent {
                let Ok(parsed_agent_id) = RhoAgentId::from_str(agent_id) else {
                    self.insert_before_draft_styled(
                        "/load <agent-id>: invalid agent id\n",
                        TranscriptStyle::SystemInfo,
                        cx,
                    );
                    return true;
                };
                agent.load_agent(parsed_agent_id);
                self.agents.remember(agent_id.to_owned());
                self.agents.select(agent_id.to_owned());
                self.show_agent_transcript(Some(agent_id.to_owned()), window, cx);
                self.update_status_line(cx);
                self.update_prompt_inlay(cx);
                self.focus_editor(window, cx);
            } else {
                self.insert_before_draft_styled(
                    "/load is only available when connected to rho-daemon\n",
                    TranscriptStyle::SystemInfo,
                    cx,
                );
            }
            return true;
        }
        if let Some(role) = text.strip_prefix("/model ") {
            let role = role.trim();
            if !role.is_empty() {
                return self.select_role(role, cx);
            }
            return true;
        }
        if text == "/model" {
            self.insert_before_draft_styled("/model <role>\n", TranscriptStyle::SystemInfo, cx);
            return true;
        }
        if text == "/role" || text.starts_with("/role ") {
            self.handle_role_command(text, cx);
            return true;
        }
        if text.starts_with('/') {
            if let Some(handled) = self.handle_dynamic_action(text, cx) {
                return handled;
            }
        }
        if let Some(command) = text.strip_prefix("!!") {
            return self.send_shell_command(command, false, cx);
        }
        if let Some(command) = text.strip_prefix('!') {
            return self.send_shell_command(command, true, cx);
        }
        false
    }

    fn handle_dynamic_action(&mut self, text: &str, cx: &mut Context<Self>) -> Option<bool> {
        let dispatch = {
            let state = self.completion_state.lock().ok()?;
            state.parse_action_line(text)?
        };
        let dispatch = match dispatch {
            Ok(dispatch) => dispatch,
            Err(error) => {
                self.insert_before_draft_styled(
                    &format!("{error}\n"),
                    TranscriptStyle::SystemInfo,
                    cx,
                );
                return Some(true);
            }
        };
        let parsed = dispatch.parsed;
        Some(self.send_command_event(
            Event::ActionInvoke(tau_proto::ActionInvoke {
                invocation_id: mint_action_invocation_id().into(),
                extension_name: dispatch.extension_name,
                instance_id: dispatch.instance_id,
                action_id: parsed.action_id.clone(),
                raw_line: text.to_owned(),
                argv: parsed.argv.clone(),
                arguments: parsed_action_arguments(&parsed.named_args),
            }),
            cx,
        ))
    }

    fn handle_role_command(&mut self, text: &str, cx: &mut Context<Self>) {
        let rest = text.strip_prefix("/role").unwrap_or("").trim();
        let mut parts = rest.split_whitespace();
        let role = parts.next();
        let command = parts.next();
        let value = parts.next();
        let extra = parts.next();
        let Some(role) = role else {
            self.insert_before_draft_styled(
                "/role <role> [delete|model|effort|verbosity|thinking-summary|service-tier|compaction-threshold|tools|enable-tools|disable-tools] [value]\n",
                TranscriptStyle::SystemInfo,
                cx,
            );
            return;
        };
        let Some(command) = command else {
            self.select_role(role, cx);
            return;
        };
        if command == "delete" {
            if value.is_some() {
                self.insert_before_draft_styled(
                    "/role <role> delete takes no value\n",
                    TranscriptStyle::SystemInfo,
                    cx,
                );
                return;
            }
            self.send_command_event(
                Event::UiRoleUpdate(tau_proto::UiRoleUpdate {
                    role: role.to_owned(),
                    action: tau_proto::UiRoleUpdateAction::Delete,
                }),
                cx,
            );
            return;
        }
        let Some(value) = value else {
            self.insert_before_draft_styled(
                "/role <role> <setting> <value>\n",
                TranscriptStyle::SystemInfo,
                cx,
            );
            return;
        };
        if extra.is_some() {
            self.insert_before_draft_styled(
                "/role: too many arguments\n",
                TranscriptStyle::SystemInfo,
                cx,
            );
            return;
        }
        let action = match parse_role_setting_update(command, value) {
            Ok(action) => action,
            Err(error) => {
                self.insert_before_draft_styled(
                    &format!("/role: {error}\n"),
                    TranscriptStyle::SystemInfo,
                    cx,
                );
                return;
            }
        };
        self.send_command_event(
            Event::UiRoleUpdate(tau_proto::UiRoleUpdate {
                role: role.to_owned(),
                action,
            }),
            cx,
        );
    }

    fn select_role(&mut self, role: &str, cx: &mut Context<Self>) -> bool {
        self.send_command_event(
            Event::UiRoleSelect(tau_proto::UiRoleSelect {
                role: role.to_owned(),
            }),
            cx,
        )
    }

    fn cycle_role(&mut self, kind: RoleCycleKind, cx: &mut Context<Self>) {
        if self.agents.current_agent_id().is_some() {
            return;
        }
        match self
            .role_state
            .cycle_role(self.current_role.as_deref(), kind)
        {
            RoleCycleOutcome::Selected(role) => {
                self.select_role(&role, cx);
            }
            RoleCycleOutcome::NoRolesAvailable => {
                self.insert_before_draft_styled(
                    "cycle-role: no agent roles are available yet\n",
                    TranscriptStyle::SystemInfo,
                    cx,
                );
            }
            RoleCycleOutcome::Noop => {}
        }
    }

    fn swap_visible_agent_ui_state(&mut self, state: &mut AgentUiState) {
        std::mem::swap(&mut self.editor, &mut state.editor);
        std::mem::swap(&mut self.prompt_buffer, &mut state.prompt_buffer);
        std::mem::swap(&mut self.multi_buffer, &mut state.multi_buffer);
        std::mem::swap(&mut self.transcript, &mut state.transcript);
        std::mem::swap(&mut self.prompt_end, &mut state.prompt_end);
        std::mem::swap(&mut self.draft_end, &mut state.draft_end);
        std::mem::swap(&mut self._subscriptions, &mut state._subscriptions);
        std::mem::swap(&mut self.prompt_state, &mut state.prompt_state);
        std::mem::swap(&mut self.tool_state, &mut state.tool_state);
        std::mem::swap(&mut self.shell_state, &mut state.shell_state);
        std::mem::swap(&mut self.main_tool_activity, &mut state.main_tool_activity);
        std::mem::swap(
            &mut self.previous_provider_usage,
            &mut state.previous_provider_usage,
        );
        std::mem::swap(
            &mut self.current_context_percent,
            &mut state.current_context_percent,
        );
        std::mem::swap(
            &mut self.current_context_input_tokens,
            &mut state.current_context_input_tokens,
        );
        std::mem::swap(
            &mut self.current_context_window,
            &mut state.current_context_window,
        );
        std::mem::swap(&mut self.rho_state, &mut state.rho_state);
        std::mem::swap(
            &mut self.rho_inserted_blocks,
            &mut state.rho_inserted_blocks,
        );
        std::mem::swap(
            &mut self.rho_pending_inserted,
            &mut state.rho_pending_inserted,
        );
        std::mem::swap(
            &mut self.rho_working_elisions,
            &mut state.rho_working_elisions,
        );
        std::mem::swap(
            &mut self.user_message_gutter_ranges,
            &mut state.user_message_gutter_ranges,
        );
    }

    fn show_current_transcript_buffer(&mut self, cx: &mut Context<Self>) {
        self.transcript.refresh_highlights(cx);
        if let Some(blocks) = self.rho_state.as_ref().map(|state| state.blocks.clone()) {
            self.sync_rendered_user_message_gutter_ranges(&blocks, cx);
        }
        self.refresh_user_message_gutter_highlights(cx);
        self.update_prompt_inlay(cx);
        cx.notify();
    }

    fn empty_agent_ui_state(&self, window: &mut Window, cx: &mut Context<Self>) -> AgentUiState {
        Self::new_agent_ui_state(
            cx.entity().downgrade(),
            self.completion_state.clone(),
            window,
            cx,
        )
    }

    fn show_agent_transcript(
        &mut self,
        agent_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.displayed_agent_id == agent_id {
            return;
        }
        let previous_agent_id = std::mem::replace(&mut self.displayed_agent_id, agent_id.clone());
        let mut state = match &agent_id {
            Some(agent_id) => self.agent_ui_states.remove(agent_id),
            None => self.no_agent_ui_state.take(),
        }
        .unwrap_or_else(|| self.empty_agent_ui_state(window, cx));
        self.swap_visible_agent_ui_state(&mut state);
        if let Some(previous_agent_id) = previous_agent_id {
            self.agent_ui_states.insert(previous_agent_id, state);
        } else {
            self.no_agent_ui_state = Some(state);
        }
        self.rerender_current_rho_state(window, cx);
        self.show_current_transcript_buffer(cx);
        self.update_prompt_inlay(cx);
    }

    fn switch_to_agent_tab(
        &mut self,
        agent_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match agent_id {
            Some(agent_id) => self.switch_agent(Some(agent_id.as_str()), window, cx),
            None => self.clear_selected_agent(window, cx),
        }
        window.focus(&self.editor.focus_handle(cx), cx);
    }

    fn clear_selected_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_agent_transcript(None, window, cx);
        self.agents.clear_current_agent();
        self.update_status_line(cx);
        self.update_prompt_inlay(cx);
        self.focus_editor(window, cx);
    }

    fn switch_agent(&mut self, target: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = target
            .map(str::trim)
            .filter(|agent_id| !agent_id.is_empty())
        else {
            self.insert_before_draft_styled(
                "/load <agent-id> or /new\n",
                TranscriptStyle::SystemInfo,
                cx,
            );
            return;
        };
        if agent_id == "none" {
            self.clear_selected_agent(window, cx);
            return;
        }
        if !self.agents.known(agent_id) {
            self.insert_before_draft_styled(
                &format!("unknown agent: {agent_id}\n"),
                TranscriptStyle::SystemInfo,
                cx,
            );
            return;
        }
        self.show_agent_transcript(Some(agent_id.to_owned()), window, cx);
        self.agents.select(agent_id.to_owned());
        self.apply_selected_agent_context_usage();
        self.update_status_line(cx);
        self.update_prompt_inlay(cx);
        self.focus_editor(window, cx);
    }

    fn switch_agent_by_delta(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.agents.next_active_agent(delta) else {
            self.insert_before_draft_styled(
                "agent-switch: no active agents available yet\n",
                TranscriptStyle::SystemInfo,
                cx,
            );
            return;
        };
        if self.agents.current_agent_id() == Some(agent_id.as_str()) {
            return;
        }
        self.show_agent_transcript(Some(agent_id.clone()), window, cx);
        self.agents.select(agent_id);
        self.apply_selected_agent_context_usage();
        self.update_status_line(cx);
        self.update_prompt_inlay(cx);
        self.focus_editor(window, cx);
    }

    fn send_shell_command(
        &mut self,
        command: &str,
        include_in_context: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let command = command.trim();
        if command.is_empty() {
            return true;
        }
        let command_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| format!("ui-sh-{}", duration.as_nanos()))
            .unwrap_or_else(|_| "ui-sh-0".to_owned());
        self.send_command_event(
            Event::UiShellCommand(tau_proto::UiShellCommand {
                command_id: command_id.into(),
                command: command.to_owned(),
                include_in_context,
                target_agent_id: self.selected_agent_proto_id(),
            }),
            cx,
        )
    }

    fn clear_prompt_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_buffer.update(cx, |buffer, cx| {
            let start = self.prompt_end.to_offset(buffer);
            let end = self.draft_end.to_offset(buffer);
            buffer.edit([(start..end, "")], None, cx);
            self.prompt_end = buffer.anchor_before(start);
            self.draft_end = buffer.anchor_after(start);
        });
        self.move_cursor_to_prompt_end(window, cx);
        self.pin_tail_to_bottom(cx);
        cx.notify();
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        eprintln!("rho-gui: submit_prompt called");
        let buffer = self.prompt_buffer.read(cx);
        let prompt_end = self.prompt_end.to_offset(buffer);
        let draft_end = self.draft_end.to_offset(buffer);
        let text = buffer
            .text_for_range(prompt_end..draft_end)
            .collect::<String>();
        let text = text.trim().to_owned();
        if text.is_empty() {
            eprintln!("rho-gui: submit ignored because draft is empty");
            return;
        }
        eprintln!("rho-gui: submitting prompt with {} bytes", text.len());

        if Self::is_prompt_command(&text) {
            self.clear_prompt_draft(window, cx);
            self.handle_prompt_command(&text, window, cx);
            return;
        }

        if let Some(agent) = self.rho_agent.clone() {
            if let Some(agent_id) = self.agents.current_agent_id_owned() {
                if let Ok(agent_id) = RhoAgentId::from_str(&agent_id) {
                    agent.send_user_message(agent_id, text);
                }
            } else {
                let Some(topic_id) = self.rho_topics.first().map(|topic| topic.topic_id) else {
                    self.insert_before_draft_styled(
                        "no rho topic is available\n",
                        TranscriptStyle::SystemInfo,
                        cx,
                    );
                    return;
                };
                agent.new_agent_with_user_message_in_topic(topic_id, text);
            }
            self.clear_prompt_draft(window, cx);
            return;
        }

        if !self.selected_agent_is_active() {
            self.insert_before_draft_styled(
                "selected agent is unavailable; choose a different agent or start a new one\n",
                TranscriptStyle::SystemImportant,
                cx,
            );
            return;
        }

        let event = if self.agents.current_agent_id_owned().is_some() {
            let Some(agent_id) = self.selected_agent_proto_id() else {
                self.insert_before_draft_styled(
                    "selected agent id is invalid; choose a different agent or start a new one\n",
                    TranscriptStyle::SystemImportant,
                    cx,
                );
                return;
            };
            Event::UiPromptSubmitted(UiPromptSubmitted {
                text: text.clone(),
                agent_id,
                message_class: PromptMessageClass::User,
                originator: PromptOriginator::User,
                ctx_id: None,
            })
        } else {
            Event::UiCreateAgent(tau_proto::UiCreateAgent {
                role: self
                    .current_role
                    .clone()
                    .unwrap_or_else(|| "engineer".to_owned()),
                model_override: None,
                metadata: Vec::new(),
                initial_prompt: Some(text.clone()),
                message_class: PromptMessageClass::User,
                originator: PromptOriginator::User,
                ctx_id: None,
                parent_agent: None,
            })
        };
        if !self.send_event(event, cx) {
            return;
        }
        eprintln!("rho-gui: prompt frame sent");

        self.clear_prompt_draft(window, cx);
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.editor.focus_handle(cx), cx);
    }

    fn move_cursor_to_prompt_end(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(anchor) = self.anchor_in_excerpt(self.draft_end, cx) else {
            eprintln!("rho-gui: failed to map draft end anchor into editor excerpt");
            return;
        };
        self.select_anchor(anchor, window, cx);
    }

    fn select_anchor(
        &self,
        anchor: multi_buffer::Anchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    }

    fn update_prompt_inlay(&self, cx: &mut Context<Self>) {
        let Some(prompt_anchor) = self.anchor_in_excerpt(self.prompt_end, cx) else {
            eprintln!("rho-gui: failed to map prompt anchor into editor excerpt");
            return;
        };
        let prompt_style = self.highlight_style(TranscriptStyle::UserPrompt, cx);
        let draft_range = if self.draft_is_empty(cx) {
            Vec::new()
        } else {
            let Some(draft_end) = self.anchor_in_excerpt(self.draft_end, cx) else {
                eprintln!("rho-gui: failed to map prompt draft anchor into editor excerpt");
                return;
            };
            vec![prompt_anchor.clone()..draft_end]
        };
        let mut to_insert = Vec::new();
        if self.draft_is_empty(cx) {
            let Some(anchor) = self.anchor_in_excerpt(self.draft_end, cx) else {
                eprintln!("rho-gui: failed to map prompt placeholder anchor into editor excerpt");
                return;
            };
            to_insert.push(Inlay::custom(
                PROMPT_PLACEHOLDER_INLAY_ID,
                anchor,
                self.prompt_placeholder_text(),
            ));
        }
        let Some(prompt_end_anchor) = self.anchor_in_excerpt(self.draft_end, cx) else {
            eprintln!("rho-gui: failed to map prompt gutter end anchor into editor excerpt");
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(
                &[InlayId::Custom(PROMPT_PLACEHOLDER_INLAY_ID)],
                to_insert,
                cx,
            );
            editor.highlight_text(
                HighlightKey::SyntaxTreeView(PROMPT_DRAFT_HIGHLIGHT_KEY),
                draft_range,
                prompt_style,
                cx,
            );
            editor.highlight_gutter::<PromptGutterHighlight>(
                vec![prompt_anchor..prompt_end_anchor],
                user_prompt_gutter_color,
                cx,
            );
        });
    }

    fn prompt_placeholder_text(&self) -> String {
        "Write a message…".to_owned()
    }

    fn project_root_label(&self) -> String {
        self.project_root
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| self.project_root.display().to_string())
    }

    fn draft_text(&self, cx: &mut Context<Self>) -> String {
        let buffer = self.prompt_buffer.read(cx);
        let start = self.prompt_end.to_offset(buffer);
        let end = self.draft_end.to_offset(buffer);
        buffer.text_for_range(start..end).collect()
    }

    fn draft_is_empty(&self, cx: &mut Context<Self>) -> bool {
        self.draft_text(cx).is_empty()
    }

    fn anchor_in_excerpt(
        &self,
        anchor: text::Anchor,
        cx: &mut Context<Self>,
    ) -> Option<multi_buffer::Anchor> {
        self.multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(anchor)
    }

    fn pin_tail_to_bottom(&self, cx: &mut Context<Self>) {
        let Some(anchor) = self.anchor_in_excerpt(self.draft_end, cx) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.set_autoscroll_pin(anchor, AutoscrollStrategy::Bottom, cx);
        });
    }
    fn handle_submitted_user_prompt(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(queued) = self.prompt_state.pop_matching_queued_prompt(text) {
            let text = queued.text.clone();
            self.remove_queued_prompt(queued, cx);
            self.insert_user_message(&text, TranscriptStyle::UserPrompt, cx);
            return;
        }
        self.insert_user_message(text, TranscriptStyle::UserPrompt, cx);
    }

    fn handle_agent_prompt_queued(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(inserted) = self.insert_user_message(
            &format!("{text} (queued)"),
            TranscriptStyle::UserPromptQueued,
            cx,
        ) {
            self.prompt_state
                .push_queued_prompt(text.to_owned(), inserted);
        }
    }

    fn insert_user_message(
        &mut self,
        text: &str,
        style: TranscriptStyle,
        cx: &mut Context<Self>,
    ) -> Option<InsertedTranscript> {
        let style = self.highlight_style(style, cx);
        let spans = self.user_message_spans(text, style, cx);
        let inserted = self.insert_before_draft_spans(
            spans.iter().map(|(text, style)| (text.as_str(), *style)),
            cx,
        )?;
        self.insert_user_message_gutter_highlight(&inserted, cx);
        Some(inserted)
    }

    fn user_message_spans(
        &self,
        text: &str,
        style: HighlightStyle,
        cx: &mut Context<Self>,
    ) -> Vec<(String, HighlightStyle)> {
        let mut spans = Vec::new();
        if !self.transcript.is_empty(cx) {
            match self.transcript_trailing_newlines(cx) {
                0 => spans.push(("\n\n".to_owned(), HighlightStyle::default())),
                1 => spans.push(("\n".to_owned(), HighlightStyle::default())),
                _ => {}
            }
        }
        spans.push((format!("{text}\n\n"), style));
        spans
    }

    fn insert_user_message_gutter_highlight(
        &mut self,
        inserted: &InsertedTranscript,
        cx: &mut Context<Self>,
    ) {
        if let Some((highlight_key, range)) = self.user_message_gutter_range(inserted, cx) {
            self.user_message_gutter_ranges.insert(highlight_key, range);
            self.refresh_user_message_gutter_highlights(cx);
        }
    }

    fn user_message_gutter_range(
        &self,
        inserted: &InsertedTranscript,
        cx: &mut Context<Self>,
    ) -> Option<(usize, std::ops::Range<text::Anchor>)> {
        let Some(highlight_key) = inserted.highlight_keys.last().copied() else {
            return None;
        };
        let Some(highlight_range) = inserted.highlight_ranges.last() else {
            return None;
        };
        let range = self
            .transcript
            .range_without_trailing_newlines(highlight_range, cx);
        Some((highlight_key, range))
    }

    fn sync_rendered_user_message_gutter_ranges(
        &mut self,
        blocks: &[RhoUiBlock],
        cx: &mut Context<Self>,
    ) {
        let ranges = blocks
            .iter()
            .zip(&self.rho_inserted_blocks)
            .filter_map(|(block, inserted)| {
                matches!(block, RhoUiBlock::UserMessage { .. }).then_some(inserted.as_ref())?
            })
            .filter_map(|inserted| self.user_message_gutter_range(inserted, cx))
            .collect::<Vec<_>>();
        for (highlight_key, range) in ranges {
            self.user_message_gutter_ranges.insert(highlight_key, range);
        }
        self.refresh_user_message_gutter_highlights(cx);
    }

    fn refresh_user_message_gutter_highlights(&self, cx: &mut Context<Self>) {
        let ranges = self
            .user_message_gutter_ranges
            .values()
            .filter_map(|range| self.transcript.multibuffer_range(range.clone(), cx))
            .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            editor.highlight_gutter::<UserMessageGutterHighlight>(
                ranges,
                user_prompt_gutter_color,
                cx,
            );
        });
    }

    fn handle_agent_prompt_steered(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(queued) = self.prompt_state.pop_front_queued_prompt() {
            let text = queued.text.clone();
            self.remove_queued_prompt(queued, cx);
            self.insert_user_message(&text, TranscriptStyle::UserPrompt, cx);
        } else {
            self.insert_user_message(text, TranscriptStyle::UserPrompt, cx);
        }
    }

    fn promote_next_queued_prompt(&mut self, cx: &mut Context<Self>) {
        if let Some(queued) = self.prompt_state.pop_front_queued_prompt() {
            let text = queued.text.clone();
            self.remove_queued_prompt(queued, cx);
            self.insert_user_message(&text, TranscriptStyle::UserPrompt, cx);
        }
    }

    fn remove_queued_prompt(&mut self, queued: QueuedPrompt, cx: &mut Context<Self>) {
        self.remove_transcript_highlights(queued.inserted.highlight_keys, cx);
        self.remove_transcript_range(queued.inserted.range, cx);
    }

    fn insert_before_draft_styled(
        &mut self,
        text: &str,
        style: TranscriptStyle,
        cx: &mut Context<Self>,
    ) {
        let style = self.highlight_style(style, cx);
        self.insert_before_draft_highlighted(text, style, cx);
    }

    fn insert_before_draft_highlighted(
        &mut self,
        text: &str,
        style: HighlightStyle,
        cx: &mut Context<Self>,
    ) -> Option<InsertedTranscript> {
        self.insert_before_draft_spans([(text, style)], cx)
    }

    fn upsert_live_response(&mut self, key: String, text: &str, cx: &mut Context<Self>) {
        let spans = rho_markdown_spans(text, cx);
        if let Some(inserted) = self.prompt_state.take_live_response(&key) {
            self.remove_transcript_highlights(inserted.highlight_keys, cx);
            if let Some(inserted) = self.replace_transcript_range_with_spans(
                inserted.range,
                spans.iter().map(|(text, style)| (text.as_str(), *style)),
                cx,
            ) {
                self.prompt_state.insert_live_response(key, inserted);
            }
        } else if let Some(inserted) = self.insert_before_draft_spans(
            spans.iter().map(|(text, style)| (text.as_str(), *style)),
            cx,
        ) {
            self.prompt_state.insert_live_response(key, inserted);
        }
    }

    fn finalize_live_response(&mut self, key: &str, text: &str, cx: &mut Context<Self>) {
        let spans = rho_markdown_spans(text, cx);
        if let Some(inserted) = self.prompt_state.take_live_response(key) {
            self.remove_transcript_highlights(inserted.highlight_keys, cx);
            self.replace_transcript_range_with_spans(
                inserted.range,
                spans.iter().map(|(text, style)| (text.as_str(), *style)),
                cx,
            );
        } else {
            self.insert_before_draft_spans(
                spans.iter().map(|(text, style)| (text.as_str(), *style)),
                cx,
            );
        }
    }

    fn update_live_compaction(
        &mut self,
        key: &str,
        status: Option<(tool_render::CompactionStatus, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some((status, text)) = status else {
            self.remove_live_compaction(key, cx);
            return;
        };
        let block = tool_render::render_compaction_block(&self.cli_theme, text, status);
        if let Some(inserted) = self.prompt_state.take_live_compaction(key) {
            if let Some(inserted) = self.replace_transcript_block(inserted, block, cx) {
                self.prompt_state
                    .insert_live_compaction(key.to_owned(), inserted);
            }
        } else if let Some(inserted) = self.insert_before_draft_block(block, cx) {
            self.prompt_state
                .insert_live_compaction(key.to_owned(), inserted);
        }
    }

    fn remove_live_compaction(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(inserted) = self.prompt_state.take_live_compaction(key) {
            self.remove_transcript_highlights(inserted.highlight_keys, cx);
            self.remove_transcript_range(inserted.range, cx);
        }
    }

    fn remove_live_response(&mut self, key: &str, cx: &mut Context<Self>) {
        let cleanup = self.prompt_state.remove_prompt(key);
        if let Some(inserted) = cleanup.live_compaction {
            self.remove_transcript_highlights(inserted.highlight_keys, cx);
            self.remove_transcript_range(inserted.range, cx);
        }
        if let Some(inserted) = cleanup.live_response {
            self.remove_transcript_highlights(inserted.highlight_keys, cx);
            self.remove_transcript_range(inserted.range, cx);
        }
    }

    fn upsert_delegate_display(
        &mut self,
        call_id: &str,
        display: &tau_proto::ToolUseState,
        agent_id: Option<&str>,
        role: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let display = tool_render::render_delegate_display(display, agent_id, role);
        let block = tool_render::render_tool_block(&self.cli_theme, &display);
        if let Some(inserted) = self.tool_state.take_pending(call_id) {
            if let Some(inserted) = self.replace_transcript_block(inserted, block, cx) {
                self.tool_state.insert_pending(call_id.to_owned(), inserted);
            }
        } else if let Some(inserted) = self.insert_before_draft_block(block, cx) {
            self.tool_state.insert_pending(call_id.to_owned(), inserted);
        }
    }

    fn upsert_tool_display(
        &mut self,
        call_id: &str,
        tool_name: &str,
        display: &tau_proto::ToolUseState,
        cx: &mut Context<Self>,
    ) {
        let display = tool_render::render_tool_use_state(tool_name, display);
        let block = tool_render::render_tool_block(&self.cli_theme, &display);
        if let Some(inserted) = self.tool_state.take_pending(call_id) {
            if let Some(inserted) = self.replace_transcript_block(inserted, block, cx) {
                self.tool_state.insert_pending(call_id.to_owned(), inserted);
            }
        } else if let Some(inserted) = self.insert_before_draft_block(block, cx) {
            self.tool_state.insert_pending(call_id.to_owned(), inserted);
        }
    }

    fn replace_draft_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.prompt_buffer.update(cx, |buffer, cx| {
            let start = self.prompt_end.to_offset(buffer);
            let end = self.draft_end.to_offset(buffer);
            buffer.edit([(start..end, text)], None, cx);
            self.prompt_end = buffer.anchor_before(start);
            self.draft_end = buffer.anchor_after(start + text.len());
        });
        self.update_prompt_inlay(cx);
        cx.notify();
    }

    fn remove_transcript_range(
        &mut self,
        range: std::ops::Range<text::Anchor>,
        cx: &mut Context<Self>,
    ) {
        self.transcript.remove_range(range, cx);
        self.update_prompt_inlay(cx);
        cx.notify();
    }

    fn remove_transcript_highlights(&mut self, highlight_keys: Vec<usize>, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            for key in &highlight_keys {
                editor.clear_highlights(HighlightKey::SyntaxTreeView(*key), cx);
            }
        });
        for key in &highlight_keys {
            self.user_message_gutter_ranges.remove(key);
        }
        self.refresh_user_message_gutter_highlights(cx);
        self.transcript.remove_highlights(highlight_keys);
    }

    fn render_turn_stats(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        cx: &mut Context<Self>,
    ) {
        let Some(usage) = finished.usage.as_ref() else {
            return;
        };
        let block = tool_render::render_turn_stats_block(
            &self.cli_theme,
            usage,
            self.previous_provider_usage.as_ref(),
            None,
            None,
        );
        self.insert_before_draft_block(block, cx);
        self.previous_provider_usage = Some(usage.clone());
    }

    fn finish_tool_call(
        &mut self,
        call_id: &str,
        block: tau_cli_term::StyledBlock,
        cx: &mut Context<Self>,
    ) {
        if let Some(inserted) = self.tool_state.take_pending(call_id) {
            self.replace_transcript_block(inserted, block, cx);
        } else {
            self.insert_before_draft_block(block, cx);
        }
    }

    fn insert_before_draft_block(
        &mut self,
        block: tau_cli_term::StyledBlock,
        cx: &mut Context<Self>,
    ) -> Option<InsertedTranscript> {
        let mut spans = Vec::new();
        if self.transcript_trailing_newlines(cx) == 0 {
            spans.push(("\n", HighlightStyle::default()));
        }
        spans.extend(block_spans(&block, cx));
        if !spans.last().is_some_and(|(text, _)| text.ends_with('\n')) {
            spans.push(("\n", HighlightStyle::default()));
        }
        self.insert_before_draft_spans(spans, cx)
    }

    fn replace_transcript_block(
        &mut self,
        inserted: InsertedTranscript,
        block: tau_cli_term::StyledBlock,
        cx: &mut Context<Self>,
    ) -> Option<InsertedTranscript> {
        self.remove_transcript_highlights(inserted.highlight_keys, cx);

        let mut spans = Vec::new();
        let starts_with_newline = self.transcript.range_starts_with(&inserted.range, '\n', cx);
        if starts_with_newline {
            spans.push(("\n", HighlightStyle::default()));
        }
        spans.extend(block_spans(&block, cx));
        if !spans.last().is_some_and(|(text, _)| text.ends_with('\n')) {
            spans.push(("\n", HighlightStyle::default()));
        }
        self.replace_transcript_range_with_spans(inserted.range, spans, cx)
    }

    fn ensure_transcript_gap(&mut self, cx: &mut Context<Self>) {
        if self.transcript_trailing_newlines(cx) == 0 {
            self.insert_before_draft_highlighted("\n", HighlightStyle::default(), cx);
        }
    }

    fn transcript_trailing_newlines(&self, cx: &mut Context<Self>) -> usize {
        self.transcript.trailing_newlines(cx)
    }

    fn insert_before_draft_spans<'a>(
        &mut self,
        spans: impl IntoIterator<Item = (&'a str, HighlightStyle)>,
        cx: &mut Context<Self>,
    ) -> Option<InsertedTranscript> {
        let inserted = self.transcript.insert_spans(spans, cx);
        self.update_prompt_inlay(cx);
        cx.notify();
        inserted
    }

    fn replace_transcript_range_with_spans<'a>(
        &mut self,
        range: std::ops::Range<text::Anchor>,
        spans: impl IntoIterator<Item = (&'a str, HighlightStyle)>,
        cx: &mut Context<Self>,
    ) -> Option<InsertedTranscript> {
        let inserted = self.transcript.replace_range_with_spans(range, spans, cx);
        self.update_prompt_inlay(cx);
        cx.notify();
        inserted
    }

    fn refresh_agent_completions(&mut self) {
        let (known_agents, live_agents) = self.agents.completion_snapshot();
        if let Ok(mut state) = self.completion_state.lock() {
            state.set_agents(known_agents, live_agents);
        }
    }

    fn refresh_role_completions(&mut self, roles: &tau_proto::HarnessRolesAvailable) {
        let candidates = roles
            .roles
            .iter()
            .map(|role| CompletionCandidate::new(role.name.clone(), role.description.clone()))
            .collect();
        if let Ok(mut state) = self.completion_state.lock() {
            state.set_roles(candidates);
        }
    }

    fn update_status_line(&mut self, cx: &mut Context<Self>) {
        self.update_right_prompt(cx);
        cx.notify();
    }

    fn update_right_prompt(&mut self, cx: &mut Context<Self>) {
        let right_prompt = self.right_prompt(cx);
        self.editor
            .update(cx, |editor, cx| editor.set_right_prompt(right_prompt, cx));
    }

    fn right_prompt(&self, cx: &App) -> Option<EditorRightPrompt> {
        let anchor = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(self.draft_end)?;
        let separator_style = self.highlight_style_for_name(tau_themes::names::MODEL_STATUS, cx);
        let mut spans = Vec::new();
        if let Some(context) = self.context_status_chip() {
            spans.push((format!("#{context}"), self.context_status_style(cx)));
        }
        let status_spans = self.status_chip_spans(self.status_line().prompt_chips, cx);
        if !status_spans.is_empty() {
            if !spans.is_empty() {
                spans.push((" ".to_owned(), separator_style));
            }
            spans.extend(status_spans);
        }
        if !spans.is_empty() {
            spans.push((" ".to_owned(), separator_style));
        }
        spans.push((self.project_root_label(), self.cwd_status_style(cx)));
        Some(EditorRightPrompt { anchor, spans })
    }

    fn status_line(&self) -> status_line::StatusLine {
        status_line::build(status_line::StatusLineInput {
            current_role: self.current_role.as_deref(),
            current_model: self.current_model.as_ref(),
            baseline_params: self.baseline_params,
            current_params: self.current_params,
            role_default_effort: self.role_state.default_effort(self.current_role.as_deref()),
            role_default_verbosity: self
                .role_state
                .default_verbosity(self.current_role.as_deref()),
        })
    }

    fn status_chip_spans(
        &self,
        chips: Vec<status_line::Chip>,
        cx: &App,
    ) -> Vec<(String, HighlightStyle)> {
        let separator_style = self.highlight_style_for_name(tau_themes::names::MODEL_STATUS, cx);
        let mut spans = Vec::new();
        for (index, chip) in chips.into_iter().enumerate() {
            if index > 0 {
                spans.push((" ".to_owned(), separator_style));
            }
            spans.push((
                chip.text,
                self.highlight_style_for_name(chip.style_name, cx),
            ));
        }
        spans
    }

    fn highlight_style_for_name(&self, style_name: &'static str, cx: &App) -> HighlightStyle {
        let theme_style = self
            .cli_theme
            .resolve_style(&tau_themes::StyleName::new(style_name));
        HighlightStyle {
            color: theme_style.fg.map(|color| tau_color_to_hsla(color, cx)),
            background_color: theme_style.bg.map(|color| tau_color_to_hsla(color, cx)),
            font_weight: theme_style.bold.then_some(FontWeight::BOLD),
            font_style: theme_style.italic.then_some(FontStyle::Italic),
            underline: None,
            strikethrough: None,
            fade_out: None,
        }
    }

    fn record_main_tool_backgrounded(&mut self, call_id: &str) {
        self.main_tool_activity.record_backgrounded(call_id);
    }

    fn record_main_tool_completed(&mut self, call_id: &str) {
        self.main_tool_activity.record_completed(call_id);
    }

    fn context_status_chip(&self) -> Option<String> {
        if let Some(input) = self.current_context_input_tokens {
            Some(format_whole_token_count(input))
        } else {
            self.current_context_percent
                .map(|percent| format!("{percent}%"))
        }
    }

    fn context_status_style(&self, cx: &App) -> HighlightStyle {
        let color = if self.current_context_fullness_percent() >= Some(70) {
            cx.theme().colors().terminal_ansi_yellow
        } else {
            cx.theme().colors().terminal_ansi_blue
        };
        HighlightStyle {
            color: Some(color),
            background_color: None,
            font_weight: None,
            font_style: None,
            underline: None,
            strikethrough: None,
            fade_out: None,
        }
    }

    fn current_context_fullness_percent(&self) -> Option<u8> {
        if let Some(percent) = self.current_context_percent {
            return Some(percent);
        }
        let input = self.current_context_input_tokens?;
        let window = self.current_context_window?;
        if window == 0 {
            return None;
        }
        let percent = input.saturating_mul(100) / window;
        Some(percent.min(100) as u8)
    }

    fn cwd_status_style(&self, cx: &App) -> HighlightStyle {
        HighlightStyle {
            color: Some(cx.theme().colors().terminal_foreground),
            background_color: None,
            font_weight: None,
            font_style: None,
            underline: None,
            strikethrough: None,
            fade_out: None,
        }
    }

    fn highlight_style(&self, style: TranscriptStyle, cx: &App) -> HighlightStyle {
        match style {
            TranscriptStyle::UserPrompt | TranscriptStyle::UserPromptQueued => {
                return HighlightStyle {
                    color: Some(cx.theme().colors().text_accent),
                    background_color: None,
                    font_weight: None,
                    font_style: None,
                    underline: None,
                    strikethrough: None,
                    fade_out: None,
                };
            }
            _ => {}
        }

        let theme_style = self
            .cli_theme
            .resolve_style(&tau_themes::StyleName::new(style.style_name()));
        HighlightStyle {
            color: theme_style.fg.map(|color| tau_color_to_hsla(color, cx)),
            background_color: theme_style.bg.map(|color| tau_color_to_hsla(color, cx)),
            font_weight: theme_style.bold.then_some(FontWeight::BOLD),
            font_style: theme_style.italic.then_some(FontStyle::Italic),
            underline: None,
            strikethrough: None,
            fade_out: None,
        }
    }

    fn render_topic_rail(
        &self,
        text_style: &TextStyle,
        active_topic_color: Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let current_agent = self.agents.current_agent_id();
        let rows = self
            .tasks
            .topic_groups()
            .into_iter()
            .map(|topic| {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .w_full()
                            .pt(px(5.))
                            .pl(px(4.))
                            .text_color(text_style.color.opacity(0.65))
                            .child(topic.name),
                    )
                    .children(topic.agents.into_iter().map(|agent_id| {
                        let selected = current_agent == Some(agent_id.as_str());
                        let text_color = if selected {
                            active_topic_color
                        } else {
                            text_style.color
                        };
                        div()
                            .relative()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl(px(12.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener({
                                    let agent_id = agent_id.clone();
                                    move |this, _, window, cx| {
                                        this.main_view = MainView::Agent;
                                        this.switch_to_agent_tab(
                                            Some(agent_id.clone()),
                                            window,
                                            cx,
                                        );
                                    }
                                }),
                            )
                            .child(
                                Icon::new(if selected {
                                    IconName::PlayFilled
                                } else {
                                    IconName::Circle
                                })
                                .size(IconSize::XSmall)
                                .color(Color::Custom(
                                    if selected {
                                        active_topic_color
                                    } else {
                                        text_style.color.opacity(0.6)
                                    },
                                )),
                            )
                            .child(
                                div()
                                    .flex_grow(1.0)
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(text_color)
                                    .child(agent_id),
                            )
                    }))
            })
            .collect::<Vec<_>>();

        div()
            .id("rho-gui-topic-rail")
            .h_full()
            .w(px(224.))
            .flex_none()
            .border_r_1()
            .border_color(colors.border_variant.opacity(0.6))
            .pr(px(6.))
            .py(px(2.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .font_family(text_style.font_family.clone())
            .text_size(text_style.font_size)
            .line_height(text_style.line_height)
            .text_color(text_style.color)
            .child(
                div()
                    .id("rho-gui-topic-list")
                    .w_full()
                    .flex_grow(1.0)
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

impl Render for RhoGui {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text_style = self
            .editor
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let active_agent_color = self
            .highlight_style_for_name(tau_themes::names::STATUS_ROLE, cx)
            .color
            .unwrap_or(text_style.color);

        let editor = match self.main_view {
            MainView::Agent => self.editor.clone(),
            MainView::Tasks => self.task_board.editor.clone(),
        };

        div()
            .id("rho-gui")
            .size_full()
            .flex()
            .flex_row()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .key_context("RhoGui")
            .when(self.main_view == MainView::Agent, |this| {
                this.child(self.render_topic_rail(&text_style, active_agent_color, cx))
            })
            .child(
                div()
                    .id("rho-gui-editor")
                    .h_full()
                    .flex_grow(1.0)
                    .min_w_0()
                    .overflow_hidden()
                    .child(editor),
            )
    }
}

fn mint_action_invocation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("action-{nanos}")
}

fn parsed_action_arguments(
    args: &std::collections::BTreeMap<String, tau_proto::ParsedArgValue>,
) -> CborValue {
    CborValue::Map(
        args.iter()
            .map(|(name, value)| {
                let value = match value {
                    tau_proto::ParsedArgValue::String(value) => CborValue::Text(value.clone()),
                    tau_proto::ParsedArgValue::Integer(value) => {
                        CborValue::Integer((*value).into())
                    }
                };
                (CborValue::Text(name.clone()), value)
            })
            .collect(),
    )
}

fn startup_pun() -> &'static str {
    let index = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as usize % STARTUP_PUNS.len())
        .unwrap_or(0);
    STARTUP_PUNS[index]
}

fn build_label_parts() -> (String, String) {
    let version = format!("rho {}", env!("CARGO_PKG_VERSION"));
    let build = match tau_harness::version::build_last_modified() {
        Some(date) => format!("({}, {})", tau_harness::version::build_revision(), date),
        None => format!("({})", tau_harness::version::build_revision()),
    };
    (version, build)
}

fn render_rho_banner_block(
    version: &str,
    build: &str,
    pun: &str,
    cx: &mut BlockContext<'_, '_>,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    let text_style = cx.editor_style.text.clone();
    div()
        .block_mouse_except_scroll()
        .pl(cx.anchor_x)
        .ml(px(6.))
        .h(px(64.))
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            svg()
                .path("icons/rho.svg")
                .w(px(31.))
                .h(px(48.))
                .text_color(colors.text_accent),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(0.))
                .font_family(text_style.font_family.clone())
                .text_size(text_style.font_size)
                .line_height(text_style.line_height)
                .text_color(text_style.color)
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(6.))
                        .child(div().font_weight(FontWeight::BOLD).child("rho"))
                        .child(
                            div()
                                .text_color(text_style.color.opacity(0.7))
                                .child(version.trim_start_matches("rho").to_owned()),
                        )
                        .child(
                            div()
                                .text_color(text_style.color.opacity(0.5))
                                .child(build.to_owned()),
                        ),
                )
                .child(
                    div()
                        .text_color(text_style.color.opacity(0.7))
                        .child(pun.to_owned()),
                ),
        )
}

fn render_rho_block_spans(
    block: &RhoUiBlock,
    theme: &tau_themes::Theme,
    cx: &App,
) -> Vec<(String, HighlightStyle)> {
    let mut spans = Vec::new();
    push_rho_block_spans(&mut spans, theme, block, cx);
    spans
}

fn render_rho_pending_spans(
    pending_response: &[RhoUiStreamingItem],
    theme: &tau_themes::Theme,
    cx: &App,
) -> Vec<(String, HighlightStyle)> {
    let mut spans = Vec::new();
    for item in pending_response {
        push_rho_pending_item_spans(&mut spans, theme, item, cx);
    }
    spans
}

fn push_rho_block_spans(
    spans: &mut Vec<(String, HighlightStyle)>,
    theme: &tau_themes::Theme,
    block: &RhoUiBlock,
    cx: &App,
) {
    match block {
        RhoUiBlock::UserMessage { text } => {
            push_rho_styled_line(spans, text, TranscriptStyle::UserPrompt, theme, cx)
        }
        RhoUiBlock::AssistantMessage { text, .. } => {
            push_rho_assistant_markdown_spans(spans, text, cx)
        }
        RhoUiBlock::Reasoning { .. } => {}
        RhoUiBlock::Tool(tool) => push_rho_tool_spans(spans, theme, tool, cx),
        RhoUiBlock::Notice { text } => {
            push_rho_styled_line(spans, text, TranscriptStyle::SystemInfo, theme, cx)
        }
    }
}

fn push_rho_pending_item_spans(
    spans: &mut Vec<(String, HighlightStyle)>,
    theme: &tau_themes::Theme,
    item: &RhoUiStreamingItem,
    cx: &App,
) {
    match item {
        RhoUiStreamingItem::AssistantMessage { text, .. } => {
            push_rho_assistant_markdown_spans(spans, text, cx)
        }
        RhoUiStreamingItem::Reasoning { .. } => {}
        RhoUiStreamingItem::Tool(tool) => push_rho_tool_spans(spans, theme, tool, cx),
        RhoUiStreamingItem::Notice { text } => {
            push_rho_styled_line(spans, text, TranscriptStyle::SystemInfo, theme, cx)
        }
    }
}

fn rho_block_is_working(block: &RhoUiBlock) -> bool {
    match block {
        RhoUiBlock::UserMessage { .. } => false,
        RhoUiBlock::AssistantMessage { phase, .. } => {
            *phase != Some(RhoUiMessagePhase::FinalAnswer)
        }
        RhoUiBlock::Reasoning { .. } | RhoUiBlock::Tool(_) | RhoUiBlock::Notice { .. } => true,
    }
}

fn rho_pending_item_is_working(item: &RhoUiStreamingItem) -> bool {
    match item {
        RhoUiStreamingItem::AssistantMessage { phase, .. } => {
            *phase != Some(RhoUiMessagePhase::FinalAnswer)
        }
        RhoUiStreamingItem::Reasoning { .. }
        | RhoUiStreamingItem::Tool(_)
        | RhoUiStreamingItem::Notice { .. } => true,
    }
}

fn rho_turn_tool_count(state: &RhoUiAgentState, block_index: usize) -> usize {
    let turn_range = rho_turn_range(state, block_index);
    state.blocks[turn_range]
        .iter()
        .filter(|block| matches!(block, RhoUiBlock::Tool(_)))
        .count()
}

fn rho_turn_has_non_working_response(state: &RhoUiAgentState, block_index: usize) -> bool {
    let turn_range = rho_turn_range(state, block_index);
    state.blocks[turn_range].iter().any(|block| {
        !matches!(block, RhoUiBlock::UserMessage { .. }) && !rho_block_is_working(block)
    })
}

fn rho_block_is_in_active_turn(state: &RhoUiAgentState, block_index: usize) -> bool {
    state
        .blocks
        .iter()
        .rposition(|block| matches!(block, RhoUiBlock::UserMessage { .. }))
        .is_none_or(|turn_start| block_index >= turn_start)
}

fn rho_turn_range(state: &RhoUiAgentState, block_index: usize) -> std::ops::Range<usize> {
    let turn_start = state.blocks[..=block_index]
        .iter()
        .rposition(|block| matches!(block, RhoUiBlock::UserMessage { .. }))
        .unwrap_or(0);
    let turn_end = state.blocks[block_index + 1..]
        .iter()
        .position(|block| matches!(block, RhoUiBlock::UserMessage { .. }))
        .map(|offset| block_index + 1 + offset)
        .unwrap_or(state.blocks.len());
    turn_start..turn_end
}

fn rho_active_turn_tool_count(state: &RhoUiAgentState) -> usize {
    let turn_start = state
        .blocks
        .iter()
        .rposition(|block| matches!(block, RhoUiBlock::UserMessage { .. }))
        .unwrap_or(0);
    state.blocks[turn_start..]
        .iter()
        .filter(|block| matches!(block, RhoUiBlock::Tool(_)))
        .count()
}

fn rho_working_elision_label(tool_count: usize) -> String {
    match tool_count {
        0 => "working".to_owned(),
        1 => "1 tool".to_owned(),
        count => format!("{count} tools"),
    }
}

fn render_rho_working_elision_block(
    label: &str,
    cx: &mut BlockContext<'_, '_>,
) -> impl IntoElement {
    let text_style = cx.editor_style.text.clone();
    let cursor_color = cx.editor_style.local_player.cursor;
    let text_color = if cx.selected {
        text_style.color
    } else {
        rho_hint_color(cx.app)
    };
    div()
        .block_mouse_except_scroll()
        .pl(cx.anchor_x)
        .h(cx.line_height)
        .flex()
        .items_center()
        .font_family(text_style.font_family.clone())
        .text_size(text_style.font_size)
        .line_height(text_style.line_height)
        .text_color(text_color)
        .child(
            div()
                .h(cx.line_height)
                .flex()
                .items_center()
                .gap_1()
                .pr_1()
                .when(cx.selected, |this| this.bg(cursor_color.opacity(0.22)))
                .child(
                    Icon::new(IconName::ChevronRight)
                        .size(IconSize::XSmall)
                        .color(text_color.into()),
                )
                .child(label.to_owned()),
        )
}

fn push_rho_styled_line(
    spans: &mut Vec<(String, HighlightStyle)>,
    text: &str,
    style: TranscriptStyle,
    theme: &tau_themes::Theme,
    cx: &App,
) {
    if text.is_empty() {
        return;
    }
    let mut text = text.to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    spans.push((text, highlight_style_for_theme(theme, style, cx)));
}

fn push_rho_assistant_markdown_spans(
    spans: &mut Vec<(String, HighlightStyle)>,
    text: &str,
    cx: &App,
) {
    let mut text = text.to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    spans.extend(rho_markdown_spans(&text, cx));
}

fn rho_markdown_spans(text: &str, cx: &App) -> Vec<(String, HighlightStyle)> {
    let Some(markdown_language) = rho_markdown_language(cx) else {
        return vec![(text.to_owned(), HighlightStyle::default())];
    };
    markdown_language.set_theme(cx.theme().syntax());
    let rope = Rope::from(text);
    let mut highlights = markdown_language.highlight_text(&rope, 0..text.len());
    if let Some(markdown_inline_language) = rho_markdown_inline_language(cx) {
        markdown_inline_language.set_theme(cx.theme().syntax());
        highlights.extend(markdown_inline_language.highlight_text(&rope, 0..text.len()));
    }
    highlights.sort_by_key(|(range, _)| range.start);

    let mut spans = Vec::new();
    let mut cursor = 0;
    let syntax = cx.theme().syntax();
    for (range, highlight_id) in highlights {
        if range.start > cursor {
            spans.push((
                text[cursor..range.start].to_owned(),
                HighlightStyle::default(),
            ));
        }
        let start = range.start.max(cursor);
        if range.end > start {
            let style = syntax.get(highlight_id).cloned().unwrap_or_default();
            spans.push((text[start..range.end].to_owned(), style));
        }
        cursor = cursor.max(range.end);
    }
    if cursor < text.len() {
        spans.push((text[cursor..].to_owned(), HighlightStyle::default()));
    }
    spans
}

fn rho_markdown_language(cx: &App) -> Option<&'static Arc<Language>> {
    RHO_MARKDOWN_LANGUAGE
        .get_or_init(|| {
            let language = Language::new(
                LanguageConfig {
                    name: "Markdown".into(),
                    matcher: LanguageMatcher {
                        path_suffixes: vec!["md".into()],
                        ..Default::default()
                    },
                    ..LanguageConfig::default()
                },
                Some(tree_sitter_md::LANGUAGE.into()),
            )
            .with_queries(LanguageQueries {
                highlights: Some(Cow::from(include_str!("grammars/markdown/highlights.scm"))),
                ..LanguageQueries::default()
            })
            .ok()?;
            let language = Arc::new(language);
            language.set_theme(cx.theme().syntax());
            Some(language)
        })
        .as_ref()
}

fn rho_markdown_inline_language(cx: &App) -> Option<&'static Arc<Language>> {
    RHO_MARKDOWN_INLINE_LANGUAGE
        .get_or_init(|| {
            let language = Language::new(
                LanguageConfig {
                    name: "Markdown-Inline".into(),
                    hidden: true,
                    ..LanguageConfig::default()
                },
                Some(tree_sitter_md::INLINE_LANGUAGE.into()),
            )
            .with_queries(LanguageQueries {
                highlights: Some(Cow::from(include_str!(
                    "grammars/markdown-inline/highlights.scm"
                ))),
                ..LanguageQueries::default()
            })
            .ok()?;
            let language = Arc::new(language);
            language.set_theme(cx.theme().syntax());
            Some(language)
        })
        .as_ref()
}

fn highlight_style_for_theme(
    theme: &tau_themes::Theme,
    style: TranscriptStyle,
    cx: &App,
) -> HighlightStyle {
    match style {
        TranscriptStyle::UserPrompt | TranscriptStyle::UserPromptQueued => {
            return HighlightStyle {
                color: Some(cx.theme().colors().text_accent),
                background_color: None,
                font_weight: None,
                font_style: None,
                underline: None,
                strikethrough: None,
                fade_out: None,
            };
        }
        _ => {}
    }

    let theme_style = theme.resolve_style(&tau_themes::StyleName::new(style.style_name()));
    HighlightStyle {
        color: theme_style.fg.map(|color| tau_color_to_hsla(color, cx)),
        background_color: theme_style.bg.map(|color| tau_color_to_hsla(color, cx)),
        font_weight: theme_style.bold.then_some(FontWeight::BOLD),
        font_style: theme_style.italic.then_some(FontStyle::Italic),
        underline: None,
        strikethrough: None,
        fade_out: None,
    }
}

fn push_rho_tool_spans(
    spans: &mut Vec<(String, HighlightStyle)>,
    _theme: &tau_themes::Theme,
    tool: &RhoUiTool,
    cx: &App,
) {
    let status = rho_tool_status_label(&tool.status);
    let is_shell_command = matches!(tool.name.as_str(), "shell" | "shell_command");
    let label = if is_shell_command {
        let command = shell_command_argument_label(&tool.arguments);
        if command.is_empty() {
            "$".to_owned()
        } else {
            format!("$ {command}")
        }
    } else if tool.arguments.is_empty() {
        tool.name.clone()
    } else {
        format!("{} {}", tool.name, tool.arguments)
    };
    spans.push((
        label,
        if is_shell_command {
            rho_tool_args_style(cx)
        } else {
            rho_tool_name_style(cx)
        },
    ));
    spans.push((" ".to_owned(), rho_tool_args_style(cx)));
    spans.push((
        status.to_owned(),
        rho_tool_status_highlight_style(status, cx),
    ));
    if let Some(text) = tool.preview.as_deref().or(tool.output.as_deref()) {
        spans.push(("\n".to_owned(), rho_tool_args_style(cx)));
        spans.push((text.to_owned(), rho_tool_args_style(cx)));
    }
    push_rho_spans_trailing_newline(spans);
}

fn shell_command_argument_label(arguments: &str) -> String {
    streaming_json_text_field(arguments, "command")
        .or_else(|| (!arguments.trim_start().starts_with('{')).then(|| arguments.to_owned()))
        .unwrap_or_default()
}

fn streaming_json_text_field(arguments: &str, key: &str) -> Option<String> {
    let mut parser = json_stream::JsonStreamParser::new();
    for character in arguments.chars() {
        if parser.add_char(character).is_err() {
            return None;
        }
    }
    parser
        .get_result()
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn rho_tool_name_style(cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: Some(cx.theme().colors().terminal_ansi_yellow),
        ..HighlightStyle::default()
    }
}

fn rho_tool_args_style(cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: Some(rho_hint_color(cx)),
        ..HighlightStyle::default()
    }
}

fn rho_hint_color(cx: &App) -> Hsla {
    cx.theme()
        .syntax()
        .style_for_name("hint")
        .and_then(|style| style.color)
        .unwrap_or(cx.theme().status().hint)
}

fn rho_tool_status_highlight_style(status: &str, cx: &App) -> HighlightStyle {
    let colors = cx.theme().colors();
    let color = match status {
        "ok" => colors.terminal_ansi_green,
        "error" => colors.terminal_ansi_red,
        "cancelled" => colors.terminal_ansi_yellow,
        tau_proto::PROGRESS_INDICATOR_TEXT => colors.terminal_ansi_cyan,
        _ => colors.text_muted,
    };
    HighlightStyle {
        color: Some(color),
        ..HighlightStyle::default()
    }
}

fn push_rho_spans_trailing_newline(spans: &mut Vec<(String, HighlightStyle)>) {
    if !spans.last().is_some_and(|(text, _)| text.ends_with('\n')) {
        spans.push(("\n".to_owned(), HighlightStyle::default()));
    }
}

fn rho_tool_status_label(status: &RhoUiToolStatus) -> &'static str {
    match status {
        RhoUiToolStatus::Running => tau_proto::PROGRESS_INDICATOR_TEXT,
        RhoUiToolStatus::Success => "ok",
        RhoUiToolStatus::Error => "error",
        RhoUiToolStatus::Cancelled => "cancelled",
    }
}

fn tool_calls_from_output_items(output_items: &[ContextItem]) -> Vec<ToolCallItem> {
    output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn render_tool_call_block(
    theme: &tau_themes::Theme,
    call: &ToolCallItem,
) -> tau_cli_term::StyledBlock {
    let display_payload = tool_display_from_call(call);
    let display = tool_render::render_tool_use_state(call.name.as_str(), &display_payload);
    tool_render::render_tool_block(theme, &display)
}

fn render_tool_result_block(
    theme: &tau_themes::Theme,
    result: &tau_proto::ToolResult,
) -> tau_cli_term::StyledBlock {
    let display = result
        .display
        .as_ref()
        .map(|display| tool_render::render_tool_use_state(&result.tool_name, display))
        .unwrap_or_else(|| {
            tool_render::render_tool_use_state(
                &result.tool_name,
                &tool_render::synthesize_fallback_display(&result.tool_name, None),
            )
        });
    let diff = result
        .display
        .as_ref()
        .and_then(|display| match &display.payload {
            Some(tau_proto::ToolUsePayload::Diff(summary)) => Some(summary.clone()),
            _ => None,
        })
        .or_else(|| tool_render::extract_diff(&result.result));
    match diff.as_ref() {
        Some(diff) => tool_render::render_diff_tool_block(theme, &display, diff, true),
        None => tool_render::render_tool_block(theme, &display),
    }
}

fn render_tool_result_parts_block(
    theme: &tau_themes::Theme,
    tool_name: &str,
    result: &CborValue,
    display: Option<&tau_proto::ToolUseState>,
) -> tau_cli_term::StyledBlock {
    let display = display
        .map(|display| tool_render::render_tool_use_state(tool_name, display))
        .unwrap_or_else(|| {
            tool_render::render_tool_use_state(
                tool_name,
                &tool_render::synthesize_fallback_display(tool_name, None),
            )
        });
    let diff = display
        .payload
        .as_ref()
        .and_then(|payload| match payload {
            tau_proto::ToolUsePayload::Diff(summary) => Some(summary.clone()),
            _ => None,
        })
        .or_else(|| tool_render::extract_diff(result));
    match diff.as_ref() {
        Some(diff) => tool_render::render_diff_tool_block(theme, &display, diff, true),
        None => tool_render::render_tool_block(theme, &display),
    }
}

fn render_tool_error_parts_block(
    theme: &tau_themes::Theme,
    tool_name: &str,
    message: &str,
    display: Option<&tau_proto::ToolUseState>,
) -> tau_cli_term::StyledBlock {
    let display = display
        .map(|display| tool_render::render_tool_use_state(tool_name, display))
        .unwrap_or_else(|| {
            tool_render::render_tool_use_state(
                tool_name,
                &tool_render::synthesize_fallback_display(tool_name, Some(message)),
            )
        });
    tool_render::render_tool_block(theme, &display)
}

fn render_tool_error_block(
    theme: &tau_themes::Theme,
    error: &tau_proto::ToolError,
) -> tau_cli_term::StyledBlock {
    let display = error
        .display
        .as_ref()
        .map(|display| tool_render::render_tool_use_state(&error.tool_name, display))
        .unwrap_or_else(|| {
            tool_render::render_tool_use_state(
                &error.tool_name,
                &tool_render::synthesize_fallback_display(&error.tool_name, Some(&error.message)),
            )
        });
    tool_render::render_tool_block(theme, &display)
}

fn tool_display_from_call(call: &ToolCallItem) -> tau_proto::ToolUseState {
    let args = match call.name.as_str() {
        "read" | "write" | "edit" | "ls" => cbor_text_field(&call.arguments, "path"),
        "grep" | "glob" => cbor_text_field(&call.arguments, "pattern"),
        "shell" | "shell_command" => cbor_text_field(&call.arguments, "command"),
        "agent_start" => cbor_text_field(&call.arguments, "task_name"),
        _ => cbor_text_field(&call.arguments, "path")
            .or_else(|| cbor_text_field(&call.arguments, "pattern"))
            .or_else(|| cbor_text_field(&call.arguments, "query")),
    }
    .unwrap_or_default();
    tau_proto::ToolUseState {
        args,
        status: tau_proto::ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    }
}

fn cbor_text_field(arguments: &CborValue, key: &str) -> Option<String> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    entries
        .iter()
        .find_map(|(entry_key, value)| match (entry_key, value) {
            (CborValue::Text(entry_key), CborValue::Text(value)) if entry_key == key => {
                Some(value.clone())
            }
            _ => None,
        })
}

fn agent_message_sent_summary(message: &tau_proto::AgentMessageSent) -> String {
    format!(
        "Message from {} to {}",
        message.sender_id,
        agent_message_sent_recipient_label(message)
    )
}

fn agent_message_sent_recipient_label(message: &tau_proto::AgentMessageSent) -> &str {
    match &message.recipient {
        tau_proto::AgentMessageRecipient::Agent { agent_id } => agent_id.as_str(),
        tau_proto::AgentMessageRecipient::User => "user",
    }
}

fn shell_running_label(include_in_context: bool) -> String {
    if include_in_context {
        "running".to_owned()
    } else {
        "running [no context]".to_owned()
    }
}

fn shell_finished_suffix(
    finished: &tau_proto::ShellCommandFinished,
    include_in_context: bool,
) -> String {
    let suffix = if finished.cancelled {
        "cancelled".to_owned()
    } else {
        match finished.exit_code {
            Some(0) => "[0]".to_owned(),
            Some(code) => format!("[{code}]"),
            None => "[?]".to_owned(),
        }
    };
    if include_in_context {
        suffix
    } else {
        format!("{suffix} [no context]")
    }
}

fn provider_update_compaction_status(
    update: &tau_proto::ProviderResponseUpdated,
) -> Option<(tool_render::CompactionStatus, String)> {
    let compaction = update.compaction.as_ref()?;
    match compaction.status {
        tau_proto::ProviderResponseCompactionStatus::Completed => Some((
            tool_render::CompactionStatus::Success,
            compaction_success_status(
                compaction.original_input_tokens,
                compaction.compacted_input_tokens,
            ),
        )),
        tau_proto::ProviderResponseCompactionStatus::Started => Some((
            tool_render::CompactionStatus::Progress,
            compaction_progress_status(compaction.original_input_tokens),
        )),
    }
}

fn compaction_token_chip(tokens: u64) -> String {
    format!("#{}", tool_render::format_token_count(tokens))
}

fn compaction_progress_status(original_input_tokens: Option<u64>) -> String {
    match original_input_tokens {
        Some(tokens) => format!("{} compacting", compaction_token_chip(tokens)),
        None => "compacting".to_owned(),
    }
}

fn compaction_success_status(
    original_input_tokens: Option<u64>,
    compacted_input_tokens: Option<u64>,
) -> String {
    match (original_input_tokens, compacted_input_tokens) {
        (Some(original), Some(compacted)) => format!(
            "{} → {} ok",
            compaction_token_chip(original),
            compaction_token_chip(compacted)
        ),
        (Some(original), None) => format!("{} ok", compaction_token_chip(original)),
        (None, Some(compacted)) => format!("ok: {}", compaction_token_chip(compacted)),
        (None, None) => "ok".to_owned(),
    }
}

fn agent_prompt_termination_reason(
    reason: tau_proto::AgentPromptTerminationReason,
) -> &'static str {
    match reason {
        tau_proto::AgentPromptTerminationReason::Stale => "stale",
        tau_proto::AgentPromptTerminationReason::Canceled => "cancelled",
    }
}

fn assistant_text_from_update(update: &tau_proto::ProviderResponseUpdated) -> Option<String> {
    let text = update
        .deltas
        .iter()
        .filter_map(|delta| match delta {
            tau_proto::ProviderResponseTextDelta::Message { text, .. } => Some(text.as_str()),
            tau_proto::ProviderResponseTextDelta::ReasoningText { .. } => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn assistant_text(items: &[ContextItem]) -> Option<String> {
    let text = items
        .iter()
        .filter_map(|item| match item {
            ContextItem::Message(message) if message.role == ContextRole::Assistant => {
                Some(message.content.iter().map(content_text).collect::<String>())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn content_text(part: &ContentPart) -> String {
    match part {
        ContentPart::Text { text } => text.clone(),
    }
}

fn block_spans<'a>(
    block: &'a tau_cli_term::StyledBlock,
    cx: &App,
) -> Vec<(&'a str, HighlightStyle)> {
    block
        .content
        .spans()
        .iter()
        .map(|span| {
            (
                span.text.as_str(),
                terminal_style_to_highlight(span.style, cx),
            )
        })
        .collect()
}

fn terminal_style_to_highlight(style: tau_cli_term::Style, cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: style.fg.map(|color| terminal_color_to_hsla(color, cx)),
        background_color: style.bg.map(|color| terminal_color_to_hsla(color, cx)),
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        underline: None,
        strikethrough: None,
        fade_out: None,
    }
}

fn format_whole_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let rounded = tokens.saturating_add(500) / 1_000;
        if rounded >= 1_000 {
            return "1m".to_owned();
        }
        return format!("{rounded}k");
    }
    let rounded = tokens.saturating_add(500_000) / 1_000_000;
    format!("{rounded}m")
}

fn terminal_color_to_hsla(color: tau_cli_term::Color, cx: &App) -> Hsla {
    match color {
        tau_cli_term::Color::Reset => cx.theme().colors().terminal_foreground,
        tau_cli_term::Color::Black => cx.theme().colors().terminal_ansi_black,
        tau_cli_term::Color::DarkGrey => cx.theme().colors().terminal_ansi_bright_black,
        tau_cli_term::Color::Red => cx.theme().colors().terminal_ansi_bright_red,
        tau_cli_term::Color::DarkRed => cx.theme().colors().terminal_ansi_red,
        tau_cli_term::Color::Green => cx.theme().colors().terminal_ansi_bright_green,
        tau_cli_term::Color::DarkGreen => cx.theme().colors().terminal_ansi_green,
        tau_cli_term::Color::Yellow => cx.theme().colors().terminal_ansi_bright_yellow,
        tau_cli_term::Color::DarkYellow => cx.theme().colors().terminal_ansi_yellow,
        tau_cli_term::Color::Blue => cx.theme().colors().terminal_ansi_bright_blue,
        tau_cli_term::Color::DarkBlue => cx.theme().colors().terminal_ansi_blue,
        tau_cli_term::Color::Magenta => cx.theme().colors().terminal_ansi_bright_magenta,
        tau_cli_term::Color::DarkMagenta => cx.theme().colors().terminal_ansi_magenta,
        tau_cli_term::Color::Cyan => cx.theme().colors().terminal_ansi_bright_cyan,
        tau_cli_term::Color::DarkCyan => cx.theme().colors().terminal_ansi_cyan,
        tau_cli_term::Color::White => cx.theme().colors().terminal_ansi_bright_white,
        tau_cli_term::Color::Grey => cx.theme().colors().terminal_ansi_white,
        tau_cli_term::Color::Rgb { r, g, b } => Rgba {
            r: r as f32 / 255.,
            g: g as f32 / 255.,
            b: b as f32 / 255.,
            a: 1.,
        }
        .into(),
        tau_cli_term::Color::AnsiValue(value) => ansi_color_to_hsla(value, cx),
    }
}

fn ansi_color_to_hsla(value: u8, cx: &App) -> Hsla {
    if value < 16 {
        return ansi_basic_color_to_hsla(value, cx);
    }
    ansi_color_to_rgba(value).into()
}

fn ansi_basic_color_to_hsla(value: u8, cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    match value {
        0 => colors.terminal_ansi_black,
        1 => colors.terminal_ansi_red,
        2 => colors.terminal_ansi_green,
        3 => colors.terminal_ansi_yellow,
        4 => colors.terminal_ansi_blue,
        5 => colors.terminal_ansi_magenta,
        6 => colors.terminal_ansi_cyan,
        7 => colors.terminal_ansi_white,
        8 => colors.terminal_ansi_bright_black,
        9 => colors.terminal_ansi_bright_red,
        10 => colors.terminal_ansi_bright_green,
        11 => colors.terminal_ansi_bright_yellow,
        12 => colors.terminal_ansi_bright_blue,
        13 => colors.terminal_ansi_bright_magenta,
        14 => colors.terminal_ansi_bright_cyan,
        _ => colors.terminal_ansi_bright_white,
    }
}

fn ansi_color_to_rgba(value: u8) -> Rgba {
    if (16..=231).contains(&value) {
        let value = value - 16;
        let r = value / 36;
        let g = (value % 36) / 6;
        let b = value % 6;
        return Rgba {
            r: cube_component(r),
            g: cube_component(g),
            b: cube_component(b),
            a: 1.,
        };
    }
    let level = 8 + (value.saturating_sub(232) as u16).min(23) * 10;
    let channel = level as f32 / 255.;
    Rgba {
        r: channel,
        g: channel,
        b: channel,
        a: 1.,
    }
}

fn cube_component(value: u8) -> f32 {
    if value == 0 {
        0.
    } else {
        (55 + value as u16 * 40) as f32 / 255.
    }
}

fn tau_color_to_hsla(color: tau_themes::Color, cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    match color {
        tau_themes::Color::Black => colors.terminal_ansi_black,
        tau_themes::Color::DarkRed => colors.terminal_ansi_red,
        tau_themes::Color::DarkGreen => colors.terminal_ansi_green,
        tau_themes::Color::DarkYellow => colors.terminal_ansi_yellow,
        tau_themes::Color::DarkBlue => colors.terminal_ansi_blue,
        tau_themes::Color::DarkMagenta => colors.terminal_ansi_magenta,
        tau_themes::Color::DarkCyan => colors.terminal_ansi_cyan,
        tau_themes::Color::DarkGrey => colors.terminal_ansi_bright_black,
        tau_themes::Color::Red => colors.terminal_ansi_bright_red,
        tau_themes::Color::Green => colors.terminal_ansi_bright_green,
        tau_themes::Color::Yellow => colors.terminal_ansi_bright_yellow,
        tau_themes::Color::Blue => colors.terminal_ansi_bright_blue,
        tau_themes::Color::Magenta => colors.terminal_ansi_bright_magenta,
        tau_themes::Color::Cyan => colors.terminal_ansi_bright_cyan,
        tau_themes::Color::White => colors.terminal_ansi_bright_white,
        tau_themes::Color::Grey => colors.terminal_ansi_white,
        tau_themes::Color::Rgb { r, g, b } => Rgba {
            r: r as f32 / 255.,
            g: g as f32 / 255.,
            b: b as f32 / 255.,
            a: 1.,
        }
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use editor::display_map::{Block, DisplayRow};
    use gpui::TestAppContext;

    use super::*;

    fn init_test_app(cx: &mut App) {
        assets::Assets.load_test_fonts(cx);
        let store = SettingsStore::new(cx, settings::default_settings().as_ref());
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
        command_palette::init(cx);
        search::init(cx);
        vim::init(cx);
    }

    fn bind_rho_test_keymaps(cx: &mut App) {
        let default_key_bindings = settings::KeymapFile::load_asset_allow_partial_failure(
            settings::DEFAULT_KEYMAP_PATH,
            cx,
        )
        .expect("load default keymap");
        cx.bind_keys(default_key_bindings);

        let vim_key_bindings =
            settings::KeymapFile::load_asset_allow_partial_failure(settings::VIM_KEYMAP_PATH, cx)
                .expect("load vim keymap");
        cx.bind_keys(vim_key_bindings);
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer.text_for_range(0..buffer.len()).collect()
    }

    fn long_assistant_text() -> String {
        "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\ngolf\nhotel\nindia\njuliet\nkilo\nlima\nmike\nnovember\noscar\npapa\n"
            .to_owned()
    }

    fn pending_assistant_state(phase: Option<RhoUiMessagePhase>) -> RhoUiAgentState {
        RhoUiAgentState {
            blocks: vec![
                RhoUiBlock::UserMessage {
                    text: "do work".to_owned(),
                },
                RhoUiBlock::Tool(RhoUiTool {
                    id: "tool-1".to_owned(),
                    name: "shell".to_owned(),
                    arguments: "echo ok".to_owned(),
                    preview: None,
                    status: RhoUiToolStatus::Success,
                    output: None,
                    error: None,
                    started_at: None,
                    finished_at: None,
                    metadata: None,
                }),
            ],
            status: rho_ui_proto::remote::UiAgentStatus::Streaming,
            pending_response: vec![RhoUiStreamingItem::AssistantMessage {
                text: long_assistant_text(),
                phase,
            }],
        }
    }

    fn committed_plus_pending_commentary_state() -> RhoUiAgentState {
        RhoUiAgentState {
            blocks: vec![
                RhoUiBlock::UserMessage {
                    text: "do work".to_owned(),
                },
                RhoUiBlock::AssistantMessage {
                    text: "committed-one\ncommitted-two\ncommitted-three\ncommitted-four\ncommitted-five\n"
                        .to_owned(),
                    phase: Some(RhoUiMessagePhase::Commentary),
                },
            ],
            status: rho_ui_proto::remote::UiAgentStatus::Streaming,
            pending_response: vec![RhoUiStreamingItem::AssistantMessage {
                text: "pending-six\npending-seven\npending-eight\npending-nine\npending-ten\npending-eleven\npending-twelve\npending-thirteen\npending-fourteen\n"
                    .to_owned(),
                phase: Some(RhoUiMessagePhase::Commentary),
            }],
        }
    }

    fn committed_commentary_plus_pending_final_state() -> RhoUiAgentState {
        RhoUiAgentState {
            blocks: vec![
                RhoUiBlock::UserMessage {
                    text: "do work".to_owned(),
                },
                RhoUiBlock::AssistantMessage {
                    text: "working-one\nworking-two\nworking-three\n".to_owned(),
                    phase: Some(RhoUiMessagePhase::Commentary),
                },
            ],
            status: rho_ui_proto::remote::UiAgentStatus::Streaming,
            pending_response: vec![RhoUiStreamingItem::AssistantMessage {
                text: "final answer begins\n".to_owned(),
                phase: Some(RhoUiMessagePhase::FinalAnswer),
            }],
        }
    }

    fn pending_tools_state(tool_count: usize) -> RhoUiAgentState {
        RhoUiAgentState {
            blocks: vec![RhoUiBlock::UserMessage {
                text: "run tools".to_owned(),
            }],
            status: rho_ui_proto::remote::UiAgentStatus::Streaming,
            pending_response: (0..tool_count)
                .map(|ix| {
                    RhoUiStreamingItem::Tool(RhoUiTool {
                        id: format!("tool-{ix}"),
                        name: format!("tool_{ix}"),
                        arguments: format!("arg-{ix}"),
                        preview: None,
                        status: RhoUiToolStatus::Running,
                        output: None,
                        error: None,
                        started_at: None,
                        finished_at: None,
                        metadata: None,
                    })
                })
                .collect(),
        }
    }

    fn committed_commentary_plus_pending_tools_state(tool_count: usize) -> RhoUiAgentState {
        let mut state = pending_tools_state(tool_count);
        state.blocks.insert(
            1,
                RhoUiBlock::AssistantMessage {
                    text: "before-tools-one\nbefore-tools-two\nbefore-tools-three\nbefore-tools-four\nbefore-tools-five\n"
                        .to_owned(),
                    phase: Some(RhoUiMessagePhase::Commentary),
                },
        );
        state
    }

    fn has_display_elision(
        gui: &mut RhoGui,
        window: &mut Window,
        cx: &mut Context<RhoGui>,
    ) -> bool {
        gui.editor.update(cx, |editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            snapshot
                .blocks_in_range(DisplayRow(0)..snapshot.max_point().row() + 1)
                .any(|(_, block)| matches!(block, Block::DisplayElision(_)))
        })
    }

    #[gpui::test]
    fn anchor_before_stays_before_insertions_at_same_offset(cx: &mut App) {
        let buffer = cx.new(|cx| Buffer::local("ab", cx));
        buffer.update(cx, |buffer, cx| {
            let left = buffer.anchor_before(1);
            let right = buffer.anchor_after(1);

            buffer.edit([(1..1, "X")], None, cx);

            assert_eq!(buffer_text(buffer), "aXb");
            assert_eq!(left.to_offset(buffer), 1);
            assert_eq!(right.to_offset(buffer), 2);
        });
    }

    #[gpui::test]
    fn rho_shell_command_tool_uses_hint_color(cx: &mut App) {
        init_test_app(cx);

        let spans = render_rho_block_spans(
            &RhoUiBlock::Tool(RhoUiTool {
                id: "tool-1".to_owned(),
                name: "shell_command".to_owned(),
                arguments: "echo ok".to_owned(),
                preview: None,
                status: RhoUiToolStatus::Running,
                output: None,
                error: None,
                started_at: None,
                finished_at: None,
                metadata: None,
            }),
            &cli_theme::select_theme(tau_config::settings::CliTheme::default()),
            cx,
        );

        assert_eq!(spans[0].0, "$ echo ok");
        assert_eq!(spans[0].1.color, Some(rho_hint_color(cx)));
    }

    #[gpui::test]
    fn rho_shell_command_tool_extracts_streaming_json_command(cx: &mut App) {
        init_test_app(cx);

        let spans = render_rho_block_spans(
            &RhoUiBlock::Tool(RhoUiTool {
                id: "tool-1".to_owned(),
                name: "shell_command".to_owned(),
                arguments: r#"{"command":"echo ok"}"#.to_owned(),
                preview: None,
                status: RhoUiToolStatus::Running,
                output: None,
                error: None,
                started_at: None,
                finished_at: None,
                metadata: None,
            }),
            &cli_theme::select_theme(tau_config::settings::CliTheme::default()),
            cx,
        );

        assert_eq!(spans[0].0, "$ echo ok");
    }

    #[test]
    fn shell_command_argument_label_suppresses_partial_json_without_command() {
        assert_eq!(shell_command_argument_label(r#"{"comm"#), "");
    }

    #[test]
    fn shell_command_argument_label_streams_partial_command() {
        assert_eq!(shell_command_argument_label(r#"{"command":"echo"#), "echo");
    }

    #[gpui::test]
    fn rho_user_messages_use_text_accent(cx: &mut App) {
        init_test_app(cx);

        let spans = render_rho_block_spans(
            &RhoUiBlock::UserMessage {
                text: "hello".to_owned(),
            },
            &cli_theme::select_theme(tau_config::settings::CliTheme::default()),
            cx,
        );

        assert_eq!(spans[0].0, "hello\n");
        assert_eq!(spans[0].1.color, Some(cx.theme().colors().text_accent));
    }

    #[gpui::test]
    fn rho_assistant_messages_use_markdown_highlights(cx: &mut App) {
        init_test_app(cx);

        let spans = render_rho_block_spans(
            &RhoUiBlock::AssistantMessage {
                text: "**bold** `code`".to_owned(),
                phase: Some(RhoUiMessagePhase::FinalAnswer),
            },
            &cli_theme::select_theme(tau_config::settings::CliTheme::default()),
            cx,
        );

        assert_eq!(
            spans
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<String>(),
            "**bold** `code`\n"
        );
        assert!(
            spans.len() > 1,
            "markdown should be split into syntax-highlighted spans: {spans:?}"
        );
    }

    #[gpui::test]
    fn rho_user_messages_have_turn_gap_after_first(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));
        let state = RhoUiAgentState {
            blocks: vec![
                RhoUiBlock::UserMessage {
                    text: "first".to_owned(),
                },
                RhoUiBlock::AssistantMessage {
                    text: "answer".to_owned(),
                    phase: Some(RhoUiMessagePhase::FinalAnswer),
                },
                RhoUiBlock::UserMessage {
                    text: "second".to_owned(),
                },
            ],
            status: rho_ui_proto::remote::UiAgentStatus::Streaming,
            pending_response: Vec::new(),
        };
        let (text, gutter_highlights) = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(&state, window, cx);
                gui.editor.update(cx, |editor, cx| {
                    (
                        editor.display_text(cx),
                        editor.all_gutter_highlights(window, cx),
                    )
                })
            })
            .expect("update rho gui");

        assert!(
            text.contains("first\n\nanswer\n\nsecond\n\n"),
            "subsequent user messages should start a new turn with an empty line: {text:?}"
        );
        assert!(
            !text.starts_with('\n'),
            "first user message should not get a leading gap: {text:?}"
        );
        assert!(
            gutter_highlights.len() >= 2,
            "historical user messages should render gutter highlights: {gutter_highlights:?}"
        );

        let gutter_highlights = gui
            .update(cx, |gui, window, cx| {
                gui.user_message_gutter_ranges.clear();
                gui.refresh_user_message_gutter_highlights(cx);
                gui.render_rho_state(&state, window, cx);
                gui.editor
                    .update(cx, |editor, cx| editor.all_gutter_highlights(window, cx))
            })
            .expect("update rho gui");
        assert!(
            gutter_highlights.len() >= 2,
            "rendering unchanged rho state should restore historical user gutter highlights: {gutter_highlights:?}"
        );
    }

    #[gpui::test]
    fn rho_rendering_elides_pending_unknown_phase_but_not_final_answer(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let unknown_phase_text = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(&pending_assistant_state(None), window, cx);
                assert!(has_display_elision(gui, window, cx));
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");
        assert!(
            unknown_phase_text.contains("do work"),
            "user prompt should render: {unknown_phase_text:?}"
        );
        assert!(
            !unknown_phase_text.contains("alpha"),
            "unknown phase pending assistant should be elided: {unknown_phase_text:?}"
        );
        assert!(
            unknown_phase_text.contains("echo"),
            "limited elision should leave tail rows visible: {unknown_phase_text:?}"
        );

        let final_answer_text = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(
                    &pending_assistant_state(Some(RhoUiMessagePhase::FinalAnswer)),
                    window,
                    cx,
                );
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");
        assert!(
            final_answer_text.contains("alpha"),
            "final answer pending assistant should not be elided: {final_answer_text:?}"
        );
        assert!(
            final_answer_text.contains("foxtrot"),
            "final answer pending assistant should render through the end: {final_answer_text:?}"
        );
    }

    #[gpui::test]
    fn rho_prompt_draft_has_gutter_highlight(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let (text, gutter_highlights) = gui
            .update(cx, |gui, _window, cx| {
                gui.replace_draft_text("hello", cx);
                gui.prompt_buffer.update(cx, |buffer, _| {
                    assert_eq!(buffer_text(buffer), "hello");
                });
                gui.editor.update(cx, |editor, cx| {
                    (
                        editor.display_text(cx),
                        editor.all_gutter_highlights(_window, cx),
                    )
                })
            })
            .expect("update rho gui");

        assert!(
            text.contains("hello"),
            "prompt draft should render: {text:?}"
        );
        assert!(
            !gutter_highlights.is_empty(),
            "prompt draft should have a gutter highlight"
        );
    }

    #[gpui::test]
    fn rho_empty_prompt_has_gutter_highlight(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let (text, gutter_highlights) = gui
            .update(cx, |gui, _window, cx| {
                gui.update_prompt_inlay(cx);
                gui.prompt_buffer.update(cx, |buffer, _| {
                    assert_eq!(buffer_text(buffer), "");
                });
                gui.editor.update(cx, |editor, cx| {
                    (
                        editor.display_text(cx),
                        editor.all_gutter_highlights(_window, cx),
                    )
                })
            })
            .expect("update rho gui");

        assert!(
            text.contains("Write a message…"),
            "empty prompt placeholder should render: {text:?}"
        );
        assert!(
            !gutter_highlights.is_empty(),
            "empty prompt should have a gutter highlight"
        );
    }

    #[gpui::test]
    fn rho_empty_prompt_gutter_survives_transcript_insert(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let gutter_highlights = gui
            .update(cx, |gui, _window, cx| {
                gui.insert_before_draft_styled("system message\n", TranscriptStyle::SystemInfo, cx);
                gui.prompt_buffer.update(cx, |buffer, _| {
                    assert_eq!(buffer_text(buffer), "");
                });
                gui.editor
                    .update(cx, |editor, cx| editor.all_gutter_highlights(_window, cx))
            })
            .expect("update rho gui");

        assert!(
            !gutter_highlights.is_empty(),
            "empty prompt gutter should survive transcript inserts"
        );
    }

    #[gpui::test]
    fn rho_empty_prompt_prefix_renders_after_agent_swap(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let gutter_highlights = gui
            .update(cx, |gui, window, cx| {
                gui.show_agent_transcript(Some("agent-1".to_owned()), window, cx);
                gui.prompt_buffer.update(cx, |buffer, _| {
                    assert_eq!(buffer_text(buffer), "");
                });
                gui.editor
                    .update(cx, |editor, cx| editor.all_gutter_highlights(window, cx))
            })
            .expect("update rho gui");

        assert!(
            !gutter_highlights.is_empty(),
            "empty prompt gutter should render after swapping to a fresh agent"
        );
    }

    #[gpui::test]
    fn rho_rendering_merges_pending_commentary_into_active_working_elision(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let text = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(&committed_plus_pending_commentary_state(), window, cx);
                assert!(has_display_elision(gui, window, cx));
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");

        assert!(
            !text.contains("committed-one"),
            "combined committed+pending commentary should elide from the start: {text:?}"
        );
        assert!(
            text.contains("committed-four"),
            "limited elision should leave combined tail rows visible: {text:?}"
        );
        assert!(
            text.contains("pending-six"),
            "pending commentary should be part of the visible tail: {text:?}"
        );
    }

    #[gpui::test]
    fn rho_rendering_collapses_working_items_when_final_answer_starts_streaming(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let text = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(&committed_commentary_plus_pending_final_state(), window, cx);
                assert!(has_display_elision(gui, window, cx));
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");

        assert!(
            !text.contains("working-one")
                && !text.contains("working-two")
                && !text.contains("working-three"),
            "working commentary should collapse completely once final answer streams: {text:?}"
        );
        assert!(
            text.contains("final answer begins"),
            "streaming final answer should remain visible: {text:?}"
        );
    }

    #[gpui::test]
    fn rho_rendering_elides_burst_of_pending_tools(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let text = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(&pending_tools_state(16), window, cx);
                assert!(has_display_elision(gui, window, cx));
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");

        assert!(
            !text.contains("tool_0"),
            "burst of pending tools should elide earliest tools immediately: {text:?}"
        );
        assert!(
            text.contains("tool_15"),
            "burst of pending tools should keep the tail visible: {text:?}"
        );
    }

    #[gpui::test]
    fn rho_rendering_elides_pending_tools_when_stream_grows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        gui.update(cx, |gui, window, cx| {
            gui.render_rho_state(&pending_tools_state(1), window, cx);
            assert!(!has_display_elision(gui, window, cx));
        })
        .expect("update rho gui");

        let text = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(&pending_tools_state(16), window, cx);
                assert!(has_display_elision(gui, window, cx));
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");

        assert!(
            !text.contains("tool_0"),
            "grown pending tools should elide earliest tools after replacement: {text:?}"
        );
        assert!(
            text.contains("tool_15"),
            "grown pending tools should keep the tail visible: {text:?}"
        );
    }

    #[gpui::test]
    fn rho_rendering_merges_committed_commentary_with_pending_tools(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
        });

        let gui = cx.add_window(|window, cx| RhoGui::new_for_test(window, cx));

        let text = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(
                    &committed_commentary_plus_pending_tools_state(16),
                    window,
                    cx,
                );
                assert!(has_display_elision(gui, window, cx));
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");

        assert!(
            !text.contains("before-tools-one"),
            "committed commentary before pending tools should be part of the same elision: {text:?}"
        );
        assert!(
            text.contains("tool_15"),
            "pending tool tail should remain visible: {text:?}"
        );
    }

    #[gpui::test]
    fn rho_display_elision_can_be_opened_and_closed_with_helix_fold_keys(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_test_app(cx);
            bind_rho_test_keymaps(cx);
        });

        let gui = cx.add_window(|window, cx| {
            let gui = RhoGui::new_for_test(window, cx);
            gui.focus_editor(window, cx);
            gui
        });

        let collapsed_text = gui
            .update(cx, |gui, window, cx| {
                gui.render_rho_state(&pending_assistant_state(None), window, cx);
                assert!(has_display_elision(gui, window, cx));
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");
        assert!(
            !collapsed_text.contains("alpha"),
            "rho working text should start collapsed: {collapsed_text:?}"
        );

        cx.simulate_keystrokes(*gui, "escape");
        gui.update(cx, |gui, window, cx| {
            let elision_start = gui.rho_working_elisions[0].range.start;
            let elision_start = gui
                .anchor_in_excerpt(elision_start, cx)
                .expect("elision start should be in the editor excerpt");
            gui.select_anchor(elision_start, window, cx);
        })
        .expect("update rho gui");
        cx.simulate_keystrokes(*gui, "z o");
        let expanded_text = gui
            .update(cx, |gui, _, cx| {
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");
        assert!(
            expanded_text.contains("alpha"),
            "z o should expand the rho working elision: {expanded_text:?}"
        );

        cx.simulate_keystrokes(*gui, "z c");
        let recollapsed_text = gui
            .update(cx, |gui, _, cx| {
                gui.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("update rho gui");
        assert!(
            !recollapsed_text.contains("alpha"),
            "z c should collapse the rho working elision again: {recollapsed_text:?}"
        );
    }

    #[gpui::test]
    fn transcript_newline_checks_handle_multibyte_characters(cx: &mut App) {
        let buffer = cx.new(|cx| Buffer::local("hello 🌍", cx));

        buffer.update(cx, |buffer, _cx| {
            assert!(!buffer_text_ends_with(buffer, buffer.len(), '\n'));
            assert!(buffer_text_ends_with(buffer, buffer.len(), '🌍'));
            assert!(!buffer_range_starts_with(buffer, 6..buffer.len(), '\n'));
            assert!(buffer_range_starts_with(buffer, 6..buffer.len(), '🌍'));
        });
    }

    #[gpui::test]
    fn split_buffers_compose_into_writable_prompt_with_read_only_transcript(cx: &mut App) {
        let transcript_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let prompt_buffer = cx.new(|cx| Buffer::local("", cx));
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                transcript_buffer.clone(),
                [Point::zero()..transcript_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(1),
                prompt_buffer.clone(),
                [Point::zero()..prompt_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });

        transcript_buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "hello")], None, cx);
        });
        prompt_buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "draft")], None, cx);
        });

        let snapshot = multi_buffer.read(cx).snapshot(cx);
        assert_eq!(snapshot.text(), "hello\ndraft");
        assert_eq!(buffer_text(prompt_buffer.read(cx)), "draft");
        assert_eq!(transcript_buffer.read(cx).capability(), Capability::Read);
        assert_eq!(prompt_buffer.read(cx).capability(), Capability::ReadWrite);
    }

    #[gpui::test]
    fn prompt_autoscroll_anchor_tracks_prompt_end_after_transcript_edits(cx: &mut App) {
        use multi_buffer::ToOffset as _;

        let transcript_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let prompt_buffer = cx.new(|cx| Buffer::local("", cx));
        let draft_end = prompt_buffer.read(cx).anchor_after(0);
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                transcript_buffer.clone(),
                [Point::zero()..transcript_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(1),
                prompt_buffer.clone(),
                [Point::zero()..prompt_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });

        transcript_buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "transcript\n")], None, cx);
        });
        prompt_buffer.update(cx, |buffer, cx| {
            let end = draft_end.to_offset(buffer);
            buffer.edit([(end..end, "draft")], None, cx);
        });

        let snapshot = multi_buffer.read(cx).snapshot(cx);
        let draft_anchor = snapshot
            .anchor_in_excerpt(draft_end)
            .expect("draft end should be present in multibuffer");
        assert_eq!(draft_anchor.to_offset(&snapshot), snapshot.len());
    }
    #[test]
    fn shell_running_label_marks_no_context_commands() {
        assert_eq!(shell_running_label(true), "running");
        assert_eq!(shell_running_label(false), "running [no context]");
    }

    #[test]
    fn shell_finished_suffix_matches_cli_labels() {
        let mut finished = tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::from("command"),
            command: "echo hi".to_owned(),
            include_in_context: true,
            target_agent_id: None,
            output: String::new(),
            exit_code: Some(0),
            cancelled: false,
        };

        assert_eq!(shell_finished_suffix(&finished, true), "[0]");
        assert_eq!(shell_finished_suffix(&finished, false), "[0] [no context]");

        finished.exit_code = None;
        assert_eq!(shell_finished_suffix(&finished, true), "[?]");

        finished.cancelled = true;
        assert_eq!(
            shell_finished_suffix(&finished, false),
            "cancelled [no context]"
        );
    }
}
