//! Root entity: owns the attached agent hosts, the canonical agent states, the
//! registry, and one persistent [`AgentModel`] per opened agent.
//!
//! All protocol events flow through [`Workspace`]; queued frame runs are
//! merged per agent, and views receive summarized changes rather than the
//! protocol itself.
//!
//! Several agent hosts can be attached at once. Agent ids are
//! already unique across machines, so the client-side state stays keyed by
//! id alone; what the host is needed for is routing — which socket a command
//! travels down — and for the few places where a host-side *name* (a
//! repository path, a short agent label) is only unique within one machine.

#[path = "workspace_phone.rs"]
mod phone;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::prelude::*;
use gpui::{
    App, ClipboardEntry, Context, Entity, Focusable as _, Point, Task, TouchEvent, TouchId,
    TouchPhase, Window, div, px,
};
#[cfg(test)]
pub(crate) use phone::set_touch_modal_editing;
use rho_agent_hosts::connection::{ConnEvent, GitApprovalDecision};
use rho_agent_hosts::hosts::{HostStatus, Hosts};
#[cfg(test)]
use rho_agent_types::AdvisorIntelligence;
use rho_agent_types::{AgentId, AgentRole, ContentPart, EngineerIntelligence, MessageDelivery};
use rho_agents_client::create::{
    StartBase, cycle_agent_role_text, cycle_workset_mode_text, parse_agent_role, parse_start,
    parse_workset_mode,
};
use rho_agents_client::protocol::{AgentCommand, NewAgent};
use rho_agents_client::remote::AgentsLink;
use rho_agents_client::session::ActiveAgents;
use rho_agents_client::store::FrameSummary;
use rho_agents_client::{AgentMap, HostId, protocol as agents};
use rho_agents_view::agent_view::AgentModel;
use rho_agents_view::draft::DraftModel;
use rho_agents_view::messages::MessageLog;
use rho_agents_view::{
    DraftFieldClear, DraftFieldSubmit, DraftValueCycle, RoleCycle, RoleCycleGroup, TranscriptFrame,
};
use rho_window::style::StyleClass;
use settings::Settings as _;
use theme::ActiveTheme as _;

use crate::chime::Chime;
use crate::minibuffer::{ECHO_DURATION, Echo, Minibuffer, bottom_strip};
use crate::pane::SurfaceKey;
use crate::search;
use crate::selection::{ActivePane, Selection};

/// One context's viewport and the stack behind it, over Rho's own surface
/// identity. The machine is `rho-window`'s and names nothing above it.
/// A surface in history, with the context it was opened in. History is one
/// list across every context — the user's ruling — so an entry has to carry
/// the context back with it, or walking back would land the reader on the
/// right buffer in the wrong arrangement.
#[derive(Clone)]
pub(crate) struct WarmSurface {
    pub(crate) context: ContextId,
    pub(crate) surface: Surface,
}

type SurfaceHistory = rho_window::history::History<SurfaceKey, WarmSurface>;

use rho_files::{FileView, RemoteProject};

use crate::{
    AgentNew, AgentNext, AgentPrevious, BrowserExit, DealCloseAndNext, DealDone, DealExit,
    DealFile, DealMute, DealNext, DealOpen, DealRefresh, DealReply, DealRoomSnooze, DealSnooze,
    DealTodo, FindNode, GitApprovalAllow, GitApprovalDeny, HomeOpenRow, MessagesOpen,
    MinibufferCancel, MinibufferComplete, MinibufferConfirm, MinibufferNext, MinibufferPrevious,
    OverviewToggle, PastePrompt, SearchRepeat, SearchRepeatReverse, ShellEof, ShellInterrupt,
    ShellPagerAll, ShellPagerMore, ShellPagerQuit, SlackCancelEdit, SlackCompose, SlackEditLast,
    SlackEditMessage, SlackFindFile, SlackFindMessage, SlackMarkReadBefore, SlackMarkUnread,
    SlackNextUnread, SlackOpenFound, SlackOpenRow, SlackReactTo, SlackSaveForLater, SlackSearch,
    SlackSearchNextPage, SlackSearchPreviousPage, SubmitPrompt, SurfaceBack, SurfaceClose,
    TaskBoard, TranscriptTop, UndoVerdict, UploadGuiTelemetry, VerdictMenu, VoiceToggle,
};

const SHELL_SWIPE_DISTANCE: gpui::Pixels = px(64.);

/// The longest the dealer's signals go unexamined. A card's priority grows
/// with how long it has waited, so a wake is owed even when nothing the
/// user marked comes due.
const DEALER_SIGNAL_CEILING: Duration = Duration::from_secs(60);
/// The shortest wait between two examinations, so that a hand full of
/// deadlines a second apart does not spin.
const DEALER_SIGNAL_FLOOR: Duration = Duration::from_secs(1);

struct ShellTouchContact {
    start: Point<gpui::Pixels>,
    position: Point<gpui::Pixels>,
}

/// Stable surface identity plus its live view.
#[derive(Clone)]
pub struct Surface {
    pub(crate) key: SurfaceKey,
    pub(crate) view: SurfaceView,
}

#[derive(Clone)]
pub(crate) enum SurfaceView {
    Draft {
        editor: Entity<editor::Editor>,
    },
    Home(Entity<crate::home::HomeView>),
    Messages(Entity<editor::Editor>),
    Usage(Entity<crate::usage::UsageView>),
    Note(Entity<editor::Editor>),
    Transcript {
        model: Entity<AgentModel>,
        /// The editor over the model's multibuffer.
        editor: Entity<editor::Editor>,
    },
    File(Entity<FileView>),
    Shell {
        model: Entity<rho_shell_view::ShellModel>,
        editor: Entity<editor::Editor>,
    },
    Terminal(Entity<rho_terminal::TerminalView>),
    Browser(Entity<rho_browser::PageView>),
    SlackList(Entity<rho_slack::ui::ListView>),
    SlackResults(Entity<rho_slack::ui::ResultsView>),
    SlackConversation(Entity<rho_slack::ui::ConversationView>),
    Image(Entity<rho_window::image_view::ImageView>),
}

impl SurfaceView {
    fn telemetry_kind(&self) -> crate::telemetry::SurfaceKind {
        use crate::telemetry::SurfaceKind;
        match self {
            Self::Draft { .. } => SurfaceKind::Draft,
            // Home stands where the desk map stood, and is read the same
            // way, so it counts as the same kind of screen.
            Self::Home(_) => SurfaceKind::Dashboard,
            Self::Messages(_) => SurfaceKind::Messages,
            Self::Usage(_) => SurfaceKind::Usage,
            Self::Note(_) => SurfaceKind::Dashboard,
            Self::Transcript { .. } => SurfaceKind::Transcript,
            Self::File(_) => SurfaceKind::File,
            Self::Shell { .. } => SurfaceKind::Shell,
            Self::Terminal(_) => SurfaceKind::Terminal,
            Self::Browser(_) => SurfaceKind::Browser,
            Self::SlackList(_) => SurfaceKind::SlackList,
            Self::SlackResults(_) => SurfaceKind::SlackResults,
            Self::SlackConversation(_) => SurfaceKind::SlackConversation,
            Self::Image(_) => SurfaceKind::Image,
        }
    }
}

impl PartialEq for Surface {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

/// Which task's window arrangement fills the window. The draft composer
/// has its own context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ContextId {
    Draft,
    Agent(AgentId),
    /// Slack's own window arrangement: entering it from the dashboard
    /// leaves the agent surfaces exactly as they were, and leaving it
    /// comes back to them.
    Slack,
}

pub use rho_agent_hosts::{AttachTarget, HostPath, HostSpec};

/// The menu on screen while it is open. Nothing here says where it is
/// drawn: the desk pins it to the bottom edge of the window and the phone
/// draws it as a sheet, and in neither case does the buffer change, so the
/// open menu is the menu and the way back and nothing else.
struct MenuBuffer {
    menu: crate::transient::Menu,
    /// A count typed in this menu that belongs to the one it opened: `45 s m`
    /// is forty-five minutes, and the digits were typed before the `s`.
    carried_count: Option<u32>,
    /// The menus this one is standing on, oldest first. Escape pops one
    /// rather than going out: back returns, here as everywhere, and it
    /// returns all the way down `space s u` and not one step of it.
    under: Vec<crate::transient::Menu>,
    /// Whether this is the verdict menu (or a menu opened from it), which
    /// is what makes the next `shift` Home rather than another open.
    verdict: bool,
}

/// The open menu, ready to be drawn as a list of targets rather than as a
/// block: what the phone needs and all it needs.
pub(crate) struct MenuSheet {
    pub(crate) title: String,
    pub(crate) rows: Vec<MenuRow>,
    /// Whether anything is under this menu for `back` to return to.
    pub(crate) has_back: bool,
}

pub(crate) struct MenuRow {
    pub(crate) description: String,
    pub(crate) value: Option<String>,
}

/// What escape goes back to when a menu is dismissed.
enum Back {
    /// Out. Nothing is under this menu.
    Out,
    /// The menu on screen now, and whatever is under that.
    Over,
    /// A stack handed back, when escape is restoring a menu.
    Under(Vec<crate::transient::Menu>),
}

/// The unit half of the snooze operator (`s` then `m`, `h`, `d`, `w` or `s`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SnoozeUnit {
    Minutes,
    Hours,
    Days,
    Weeks,
}

/// A named hour of the day, for the phone's `tonight` and `tomorrow`.
/// `tonight` is this evening while it is still ahead and the next one after
/// that; `tomorrow` is always the next day, even when read before nine.
fn named_hour(hour: u32, tomorrow: bool) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone as _;
    let now = chrono::Local::now();
    let mut day = now.date_naive();
    let time = chrono::NaiveTime::from_hms_opt(hour, 0, 0).unwrap_or_default();
    if tomorrow || day.and_time(time) <= now.naive_local() {
        day += chrono::Duration::days(1);
    }
    chrono::Local
        .from_local_datetime(&day.and_time(time))
        .earliest()
        .unwrap_or(now)
}

/// Where a snooze lands and how the bar says it. Minutes and hours keep the
/// clock, so a card can come back this afternoon; days and weeks land on a
/// date, which is what a defer has always been.
pub(crate) fn snooze_target(
    unit: SnoozeUnit,
    count: i64,
    now: chrono::DateTime<chrono::Local>,
) -> (rho_dealer::DateMark, String) {
    match unit {
        SnoozeUnit::Minutes | SnoozeUnit::Hours => {
            let ahead = match unit {
                SnoozeUnit::Minutes => chrono::Duration::minutes(count),
                _ => chrono::Duration::hours(count),
            };
            let at = now + ahead;
            (
                rho_dealer::DateMark::at(at.timestamp_millis()),
                snooze_said(at, now),
            )
        }
        SnoozeUnit::Days | SnoozeUnit::Weeks => {
            let days = match unit {
                SnoozeUnit::Days => count,
                _ => count * 7,
            };
            let date = now.date_naive() + chrono::Duration::days(days);
            (
                rho_dealer::DateMark::day(date),
                format!("snooze until {}", date.format("%a %-d %b")),
            )
        }
    }
}

/// The bar's words for a snooze with a clock time: the hour alone when it
/// is still today, the day in front of it when it is not.
fn snooze_said(
    at: chrono::DateTime<chrono::Local>,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    match at.date_naive() == now.date_naive() {
        true => format!("snooze until {}", at.format("%H:%M")),
        false => format!("snooze until {}", at.format("%a %-d %b %H:%M")),
    }
}

pub struct Workspace {
    pub(crate) hosts: Hosts,
    /// The agents held whole: their events, the transcript folded from
    /// them, and the agent host's live tail. Also the focus set every host
    /// is told. Everyone else is a digest in the registry.
    active: ActiveAgents,
    /// Every transcript this client holds open, and the rendered state a
    /// screen draws from. `rho-agents` owns what a transcript is; the
    /// shell only says which agent and hands the rows on.
    transcripts: rho_agents_view::Transcripts,
    pub(crate) registry: AgentMap,
    /// Which pane the point is in. The window's, not the map's.
    pub(crate) selection: Selection,
    models: HashMap<AgentId, Entity<AgentModel>>,
    /// Weak project cache keyed by host-side workspace identity, qualified
    /// by host — the same repository path on two machines is two projects.
    /// Artifact surfaces hold the strong references; when the last file
    /// closes, the remote channel and cache entry naturally expire.
    remote_projects: HashMap<
        (HostId, rho_agent_types::WorkspaceInfo),
        gpui::WeakEntity<rho_files::RemoteProjectState>,
    >,
    /// Accumulated change summaries for materialized but hidden views; they
    /// render once, with the merged summary, when next selected.
    pending_syncs: HashMap<AgentId, FrameSummary>,
    /// What the main thread asks of the model thread: which hosts exist,
    /// and whose rows it wants. The journal cursor is the model's.
    pub(crate) agents_client: rho_agents_client::model::AgentsClient,
    desktop_streams: rho_desktop_client::stream::DesktopStreams,
    draft_model: Entity<DraftModel>,
    /// What rho has said, and the surface it says it on. The log owns its
    /// own buffer, editor and highlights; the host records a line and shows
    /// the surface.
    messages: Entity<MessageLog>,
    /// Where `n a` files the agent the draft page is composing. `None`
    /// is the root, which is also what an ordinary draft sends.
    draft_area: Option<rho_dealer::NodeId>,
    /// Stands in for an editor when the surface in view has none.
    chrome_editor: Entity<editor::Editor>,
    _save_notes_on_quit: gpui::Subscription,
    /// A NewAgent request from the draft is in flight; the draft buffer is
    /// kept intact until the agent host confirms creation, so a rejected
    /// request (bad working directory, say) never loses the message.
    /// Which host the pending draft agent was sent to, so its confirmation
    /// can be recognized and the compose surface reset.
    awaiting_draft_agent: Option<HostId>,
    /// A surface was opened to be written in and is waiting for the
    /// frame its focus lands in to enter insert. The action goes to the
    /// focused node of the frame already on screen, so dispatching
    /// before that frame types into the surface the reader is leaving.
    insert_when_shown: bool,
    /// The area the next agent this client asks for is filed under. The
    /// agent host never writes it: the agent exists because the registry says
    /// so, and where it is shown is the user's own fact.
    pending_agent_filing: Option<(HostId, rho_dealer::NodeId)>,
    /// Hosts that have been reached at least once. A host attaches blind;
    /// until it is reached, its agents do not exist for this client.
    ready_hosts: HashSet<HostId>,
    /// Hosts to replay to once they are back: armed only by an actual
    /// disconnect.
    replay_hosts: HashSet<HostId>,
    /// Everything the usage screen is drawn from, and the screen itself:
    /// see [`crate::usage::Usage`].
    usage: crate::usage::Usage,
    quotas: rho_agents_client::quota::Quotas,
    duration_timer: Option<Task<()>>,
    /// Attention chime output; lazily opened on the first play.
    chime: Chime,
    /// Each context retains one viewport over its surfaces, and the stack
    /// of where the reader was in it. One machine, per context, because a
    /// back that changes context moves two things at once (eng-en1p's
    /// ruling under the Emacs rule).
    /// The one history: every surface the reader has been on, in the order
    /// they were opened, with a cursor on where they are. `None` only
    /// before the first surface is shown.
    history: Option<SurfaceHistory>,
    /// Per-context surface list, the emacs buffer list: every surface
    /// opened in a context lives here for the context's lifetime,
    /// regardless of what its viewport currently displays. Covering one never
    /// loses a file or terminal; the views (and any workspace file channel
    /// behind them) release when the context itself closes.
    surfaces: HashMap<ContextId, Vec<Surface>>,
    /// Always present in `contexts` (the draft context never closes).
    pub(crate) active_context: ContextId,
    shell_touches: HashMap<TouchId, ShellTouchContact>,
    shell_touch_was_multi: bool,
    shell_touch_committed: bool,
    deal_gesture_active: bool,
    deal_controls_visible: bool,
    pub(crate) agent_last_interaction: HashMap<AgentId, i64>,
    dealer_signal_eval_scheduled: bool,
    /// Agents whose wants are made again on the next frame: rows arrive
    /// one `Log` at a time. `Some(None)` makes every want again.
    wants_pending: Option<Option<BTreeSet<AgentId>>>,
    _dealer_signal_task: Task<()>,
    lamp_on: bool,
    dealer_signals_initialized: bool,
    chime_above_threshold: bool,
    /// The vendored modal engine's status item, kept visible in Rho's frame.
    mode_indicator: Entity<vim::ModeIndicator>,
    /// The user's marks, what every source wants of them, and the dealer:
    /// see [`crate::attention::Attention`].
    pub(crate) attention: crate::attention::Attention,
    /// One note surface per note or label the reader has opened, kept so
    /// the cursor and scroll survive leaving and coming back.
    pub(crate) note_views: HashMap<rho_dealer::NodeId, crate::note_view::NoteView>,
    /// Agent shown beside the dashboard cursor. Kept separate from the
    /// focused task so cursor previews do not rebuild or reorder the rail.
    /// The browser pages the desk refers to, the ones on their way out, and
    /// the one shown in the right-hand preview card: see
    /// [`crate::browser::Pages`].
    pages: crate::browser::Pages,

    /// The Slack client and whether it can be trusted to be current: see
    /// [`crate::slack::Slack`].
    pub(crate) slack: crate::slack::Slack,
    /// A readable name per open conversation, so naming a surface never has
    /// to reach into the session.
    pub(crate) slack_labels: HashMap<rho_slack::session::Source, String>,
    /// The message a reaction menu is open over, held while the menu is up
    /// so the emoji lands on the message the reader pressed `r` on rather
    /// than on whatever the point is over when they choose. A timestamp,
    /// because that is what a message is; the line it sits on can move
    /// under a menu the same as under anything else.
    pub(crate) slack_reacting: Option<rho_slack::types::Ts>,
    /// The narrowing the Slack list stood at when its search prompt opened,
    /// held so escape puts back what the reader was looking at. The prompt
    /// owns what "back" means here, not the minibuffer: only this prompt
    /// knows that its narrowing is a state of the list behind it.
    pub(crate) slack_search_before: Option<String>,
    pub(crate) _slack_subscription: Option<gpui::Subscription>,
    pub(crate) _slack_observer: Option<gpui::Subscription>,
    /// One per open conversation surface, for what a conversation asks the
    /// frame to show: a picture full-window, so far.
    pub(crate) _slack_view_subscriptions: Vec<gpui::Subscription>,
    /// One per agent screen, for as long as the screen lives: what it says
    /// when its transcript is ready.
    /// The draft says when it has been edited; what that means for the rest
    /// of the screen is the host's.
    _draft_subscription: gpui::Subscription,
    agent_model_subscriptions: Vec<gpui::Subscription>,
    /// What was last searched for and what is waiting to be searched, for
    /// every surface: see [`search`].
    search: search::Search,
    /// The label paths the filing prompt offers, and what each says.
    pending_filing_destinations: Vec<(String, String)>,
    /// What the finder's highlighted row opens, carried from the prompt to
    /// its submit handler: the submitted text cannot tell two rows with the
    /// same path apart.
    pub(crate) pending_find_target: Option<crate::find::FindTarget>,
    /// Everything there is to find, taken when the finder opened and held
    /// until it closes. A keystroke ranks this; it does not rebuild it.
    pub(crate) find_snapshot: Option<std::rc::Rc<crate::find::FindSnapshot>>,
    scroll_journal_task: Option<Task<()>>,
    /// The completing-read strip at the bottom of the window, when open.
    pub(crate) minibuffer: Option<Minibuffer>,

    /// The keyboard while a menu is open, on the desk as a block and on the
    /// phone as a sheet.
    transient_focus: gpui::FocusHandle,
    /// The menu under the point, when one is open: a transient buffer
    /// under the point (`rho_window::transient`). Every menu is one of
    /// these now.
    menu_buffer: Option<MenuBuffer>,
    /// Evil's one-shot `SPC u` prefix. The next supported Desk command
    /// consumes it; every other non-modifier key clears it.
    /// Focus beneath the single modal overlay. Transients, minibuffers,
    /// menus and Git approval hand this target between them so borrowing
    /// keyboard focus never changes dashboard/work mode: see
    /// [`crate::overlay::OverlayFocus`].
    overlay_focus: crate::overlay::OverlayFocus,
    desktop: Option<Entity<crate::wayland_view::WaylandView>>,
    desktop_name: String,
    desktop_sessions: HashMap<HostId, Vec<rho_desktop_client::protocol::DesktopSession>>,
    /// The last system notice, flashed in the bottom strip (emacs echo
    /// area). Cleared by its own timer or when the minibuffer opens.
    echo: Option<Echo>,
    /// The SSH Git approval prompt, one of the three modal overlays:
    /// see [`crate::git_approval::GitApproval`].
    git_approval: crate::git_approval::GitApproval,
    /// Whether the desk is listening, on whose agent host, and whether the
    /// microphone is open: see [`crate::voice::Voice`].
    voice: crate::voice::Voice,
    _event_task: Task<()>,
    _host_event_task: Task<()>,
    _ledger_event_task: Task<()>,
    _desktop_event_task: Task<()>,
    _keystroke_subscription: gpui::Subscription,
    _transient_keystroke_interceptor: gpui::Subscription,
    _window_activation_subscription: gpui::Subscription,
    phone: phone::PhoneUi,
}

/// Target-independent application state transitions. Transport adapters feed
/// these methods; native and browser layout code only decide when to render
/// the resulting canonical registry/store/model state.
impl Workspace {
    fn ensure_agent_model(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<AgentModel>, bool) {
        let model = if let Some(model) = self.models.get(&agent_id).cloned() {
            model
        } else {
            let completions = crate::commands::WorkspaceCompletionProvider::new(
                cx.entity().downgrade(),
                None,
                None,
                None,
                None,
            );
            let visualization_client = self
                .agents_for(agent_id)
                .unwrap_or_else(AgentsLink::detached);
            let model = cx.new(|cx| AgentModel::new(completions, visualization_client, cx));
            // The screen says when its transcript is composed; what that
            // means for the rest of the shell is decided here.
            self.agent_model_subscriptions.push(cx.subscribe_in(
                &model,
                window,
                |workspace, _, event, window, cx| match event {
                    rho_agents_view::agent_view::AgentModelEvent::Loaded(agent_id) => {
                        workspace.finish_initial_agent_load(*agent_id, cx);
                    }
                    rho_agents_view::agent_view::AgentModelEvent::HistoryComposed(agent_id) => {
                        workspace.finish_transcript_search(*agent_id, window, cx);
                    }
                },
            ));
            self.refresh_view_status(&agent_id, &model, cx);
            self.models.insert(agent_id, model.clone());
            model
        };
        let started = self.start_initial_agent_load(agent_id, &model, cx);
        model.update(cx, |model, cx| {
            model.preview_editor(window, cx);
        });
        (model, started)
    }

    pub(crate) fn finish_initial_agent_load(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        self.finish_agent_load(agent_id, cx);
        if let Some(model) = self.models.get(&agent_id).cloned() {
            self.refresh_view_status(&agent_id, &model, cx);
        }
        cx.notify();
    }

    /// What the model holds for a host is now all there is of it: the disk
    /// copy read at startup, or nothing after the copy started over.
    /// Whatever this client had of the host goes first.
    fn loaded(
        &mut self,
        host: HostId,
        agents: Vec<rho_agents_client::MirroredAgent>,
        verdicts: Vec<(AgentId, rho_agents_client::Verdict)>,
    ) {
        for agent_id in self.registry.host_agents(host) {
            self.transcripts.forget(agent_id);
            self.active.remove(agent_id);
        }
        let departed = self.registry.reset_host(host);
        self.selection
            .forget(|agent_id| departed.contains(&agent_id));
        self.registry.restore(agents);
        for (agent_id, verdict) in verdicts {
            self.registry.set_agent_verdict(agent_id, verdict);
        }
        self.note_followed();
    }

    /// The agents whose rows the model hands up: the transcripts this
    /// client has open. Everything else it says as a digest.
    pub(crate) fn followed(&self) -> std::collections::BTreeSet<AgentId> {
        self.transcripts.open_agents()
    }

    fn note_followed(&self) {
        self.agents_client.follow(self.followed());
    }

    fn note_agent_created(&mut self, host: HostId, agent_id: AgentId) {
        self.registry.note_agent_created(host, agent_id);
    }

    /// Shows an agent's transcript from the mirror the client already
    /// holds: the fold, for a reader who opened it before any live frame,
    /// or with the agent host down. The live frame rides on its tail.
    fn seed_transcript_from_mirror(&mut self, agent_id: AgentId) -> bool {
        if self.transcripts.is_open(&agent_id) {
            return false;
        }
        let events = self.agents_client.read_rows(agent_id);
        if !self.transcripts.seed(agent_id, &events) {
            return false;
        }
        self.note_followed();
        true
    }

    /// Rows of an agent whose transcript is open, folded into the copy
    /// behind it. Positions already held are skipped, so the rows the
    /// reader's own open read already picked up cost nothing.
    fn refold_open_transcript(
        &mut self,
        agent_id: AgentId,
        rows: &[(
            rho_agent_types::AgentPos,
            rho_agents_client::protocol::transcript::TranscriptEvent,
        )],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The rows the telling moved, not the transcript they are in.
        let Some(delta) = self.transcripts.refold(agent_id, rows) else {
            return;
        };
        self.handle_frame_batch(vec![(agent_id, TranscriptFrame::Folded(delta))], window, cx);
    }

    fn apply_frame_state(
        &mut self,
        agent_id: AgentId,
        frame: TranscriptFrame,
    ) -> Option<(FrameSummary, Option<u64>, bool, bool)> {
        let change = self.transcripts.apply(agent_id, frame);
        // Liveness is the map's fact about an agent, not the transcript's.
        let live_changed = change.was_live && self.registry.mark_live(agent_id);
        Some((
            change.summary,
            change.context_before,
            change.usage_changed,
            live_changed,
        ))
    }

    fn start_initial_agent_load(
        &self,
        agent_id: AgentId,
        model: &Entity<AgentModel>,
        cx: &mut Context<Self>,
    ) -> bool {
        if model.read(cx).initial_load_started() {
            return false;
        }
        let Some(state) = self.transcripts.state(&agent_id).cloned() else {
            return false;
        };
        let labels = self
            .registry
            .known_agents()
            .copied()
            .map(|id| (id, self.registry.agent_display_label(id)))
            .collect();
        model.update(cx, |model, cx| {
            model.start_initial_load(agent_id, state, labels, now_ms(), cx)
        });
        true
    }

    fn sync_agent_model(
        &mut self,
        agent_id: AgentId,
        model: &Entity<AgentModel>,
        summary: FrameSummary,
        started: bool,
        cx: &mut Context<Self>,
    ) {
        if started {
            return;
        }
        if !model.read(cx).initial_load_ready() {
            self.pending_syncs
                .entry(agent_id)
                .and_modify(|pending| *pending = pending.merge(summary))
                .or_insert(summary);
        } else if let Some(state) = self.transcripts.state(&agent_id) {
            model.update(cx, |model, cx| {
                model.sync(
                    state,
                    summary,
                    now_ms(),
                    &|id| self.registry.agent_display_label(id),
                    cx,
                )
            });
        }
    }

    fn finish_agent_load(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        let Some(model) = self.models.get(&agent_id).cloned() else {
            return;
        };
        if let Some(summary) = self.pending_syncs.remove(&agent_id)
            && let Some(state) = self.transcripts.state(&agent_id)
        {
            model.update(cx, |model, cx| {
                model.sync(
                    state,
                    summary,
                    now_ms(),
                    &|id| self.registry.agent_display_label(id),
                    cx,
                )
            });
        }
    }
}

/// Who a command speaks about: the rail row under the cursor, or the open
/// agent. Both answer the same three questions, which is what lets one
/// resolver serve every command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Subject {
    /// The single agent the subject stands for. A stream row's is its root
    /// — the one `enter` opens — so `space a` on a row acts on the agent
    /// the user would have opened anyway.
    pub agent: Option<AgentId>,
    /// Everything the subject's rail row aggregates. Verdicts need all of
    /// it: acking only the root leaves the row lit by a child's lamp, and
    /// the row never reaches settled.
    pub agents: Vec<AgentId>,
}

impl Subject {
    pub fn has_agent(&self) -> bool {
        self.agent.is_some()
    }
}

impl Workspace {
    pub fn new(specs: Vec<HostSpec>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // A test drives the model inline from its story, so it gets channels
        // with nothing behind them; the crate's own cfg is the right one.
        #[cfg(not(test))]
        let (agents_client, changes) = rho_agents_client::model::AgentsClient::spawn();
        #[cfg(test)]
        let (agents_client, changes) = rho_agents_client::model::AgentsClient::detached();
        // Each stream of a host has its own reader: the agents stream goes
        // to the agents client, the control and desk streams come here.
        let (host_events, host_events_rx) = futures_mpsc::unbounded::<rho_agent_hosts::HostEvent>();
        // The ledger is this device's, in its own database; a test has
        // one in memory.
        let db = rho_db::client::shared().unwrap_or_else(rho_db::RhoDb::in_memory);
        let (attention, ledger_events) = crate::attention::Attention::open(db);
        let ledger_event_task = Self::listen_to_ledger(ledger_events, cx);
        let (desktop_streams, desktop_events_rx) =
            rho_desktop_client::stream::DesktopStreams::new();
        let hosts = Hosts::new(std::sync::Arc::new(host_events));
        let workspace = cx.entity().downgrade();
        let mode_indicator = cx.new(|cx| vim::ModeIndicator::new(window, cx));
        let draft_model = cx.new(|cx| {
            DraftModel::new(
                rho_agents_view::draft::Hooks::new(move |editor, fields, _, _| {
                    editor.set_completion_provider(Some(
                        crate::commands::WorkspaceCompletionProvider::new(
                            workspace.clone(),
                            Some(fields.workdir),
                            Some(fields.role),
                            Some(fields.start),
                            Some(fields.filesystem),
                        ),
                    ));
                }),
                cx,
            )
        });
        let draft_subscription =
            cx.subscribe(&draft_model, |workspace, _, event, cx| match event {
                rho_agents_view::draft::Event::Edited => workspace.mark_draft_active_from_edit(cx),
            });
        let messages = cx.new(|cx| MessageLog::new(window, cx));
        let event_task = cx.spawn(async move |this, cx| {
            let mut changes: UnboundedReceiver<rho_agents_client::model::ModelEvent> = changes;
            while let Some(change) = changes.next().await {
                let mut batch = vec![change];
                while let Ok(change) = changes.try_recv() {
                    batch.push(change);
                }
                let updated = this.update_in(cx, |this, window, cx| {
                    this.handle_model_events(batch, window, cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let host_event_task = cx.spawn(async move |this, cx| {
            let mut events = host_events_rx;
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                let updated = this.update_in(cx, |this, window, cx| {
                    for rho_agent_hosts::HostEvent { host, event } in batch {
                        this.handle_event(host, event, window, cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let desktop_event_task = cx.spawn(async move |this, cx| {
            let mut events = desktop_events_rx;
            while let Some(rho_desktop_client::stream::DesktopsEvent { host, sessions }) =
                events.next().await
            {
                if this
                    .update(cx, |this, cx| this.desktops_arrived(host, sessions, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        // The ranking only changes on its own when a dated thing comes
        // due, so the wake is keyed to the next one rather than to a
        // minute that is usually spent finding nothing has moved. A
        // priority still slides with the clock between two of those,
        // which is what the ceiling is for.
        let dealer_signal_task = cx.spawn(async move |this, cx| {
            loop {
                let wait = this
                    .read_with(cx, |this, _| {
                        this.attention
                            .dealer
                            .next_change(chrono::Local::now().fixed_offset())
                            .and_then(|until| until.to_std().ok())
                            .unwrap_or(DEALER_SIGNAL_CEILING)
                            .clamp(DEALER_SIGNAL_FLOOR, DEALER_SIGNAL_CEILING)
                    })
                    .unwrap_or(DEALER_SIGNAL_CEILING);
                cx.background_executor().timer(wait).await;
                if this
                    .update(cx, |this, cx| this.invalidate_dealer_signals(cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        // Settings recomputes (language registration installing semantic
        // token rules, a settings file reload) rebuild every setting global
        // from file contents, silently dropping `override_global` values.
        // Phone mode depends on its modal-editing override staying in force,
        // so re-assert it whenever the store changes underneath us.
        cx.observe_global::<settings::SettingsStore>(|this, cx| {
            if this.phone.enabled
                && (vim_mode_setting::VimModeSetting::get_global(cx).0
                    || vim_mode_setting::HelixModeSetting::get_global(cx).0)
            {
                phone::set_touch_modal_editing(false, cx);
            }
        })
        .detach();

        // A note being typed is saved a moment after the typing stops; a
        // quit inside that moment saves it now.
        let save_notes_on_quit = cx.on_app_quit(|this, cx| {
            this.save_notes(cx);
            async {}
        });
        let chrome_editor = cx.new(|cx| {
            let mut editor = editor::Editor::single_line(window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor
        });
        // A menu owns its focused keys before GPUI resolves keybindings. In particular,
        // Vim binds `g` as the prefix of several multi-stroke commands, so an ordinary
        // `on_key_down` handler would not see a menu's one-stroke `g` until another key
        // arrived (or the prefix timer expired).
        let transient_keystroke_listener =
            cx.listener(|this, event: &gpui::KeystrokeEvent, window, cx| {
                if this.menu_buffer.is_some() && this.transient_focus.is_focused(window) {
                    this.menu_keystroke(&event.keystroke, window, cx);
                }
            });
        let transient_keystroke_interceptor = cx.intercept_keystrokes(transient_keystroke_listener);
        let keystroke_subscription = cx.observe_keystrokes(|_this, event, _window, _cx| {
            tracing::debug!(
                key = %event.keystroke.key,
                shift = event.keystroke.modifiers.shift,
                control = event.keystroke.modifiers.control,
                alt = event.keystroke.modifiers.alt,
                platform = event.keystroke.modifiers.platform,
                "keystroke"
            );
        });
        let mut last_window_active = None;
        let window_activation_subscription =
            cx.observe_window_activation(window, move |_this, window, _cx| {
                let focused = window.is_window_active();
                if last_window_active == Some(focused) {
                    return;
                }
                last_window_active = Some(focused);
                rho_journal::record(rho_journal::Event::WindowFocusChanged { focused });
            });
        let mut this = Self {
            hosts,
            active: ActiveAgents::default(),
            transcripts: rho_agents_view::Transcripts::default(),
            registry: AgentMap::default(),
            selection: Selection::default(),
            models: HashMap::new(),
            remote_projects: HashMap::new(),
            pending_syncs: HashMap::new(),
            agents_client,
            desktop_streams,
            draft_model,
            messages,
            draft_area: None,
            chrome_editor,
            _save_notes_on_quit: save_notes_on_quit,
            awaiting_draft_agent: None,
            insert_when_shown: false,
            pending_agent_filing: None,
            ready_hosts: HashSet::new(),
            replay_hosts: HashSet::new(),
            usage: crate::usage::Usage::default(),
            quotas: Default::default(),
            duration_timer: None,
            chime: Chime,
            history: None,
            surfaces: HashMap::new(),
            active_context: ContextId::Draft,
            shell_touches: HashMap::new(),
            shell_touch_was_multi: false,
            shell_touch_committed: false,
            deal_gesture_active: false,
            deal_controls_visible: false,
            agent_last_interaction: HashMap::new(),
            dealer_signal_eval_scheduled: false,
            wants_pending: None,
            _dealer_signal_task: dealer_signal_task,
            lamp_on: false,
            dealer_signals_initialized: false,
            chime_above_threshold: false,
            mode_indicator,
            attention,
            note_views: HashMap::new(),
            pages: crate::browser::Pages::default(),
            slack: crate::slack::Slack::default(),
            slack_labels: HashMap::new(),
            slack_reacting: None,
            slack_search_before: None,
            _slack_subscription: None,
            _slack_observer: None,
            _slack_view_subscriptions: Vec::new(),
            _draft_subscription: draft_subscription,
            agent_model_subscriptions: Vec::new(),
            search: search::Search::default(),
            pending_filing_destinations: Vec::new(),
            pending_find_target: None,
            find_snapshot: None,
            scroll_journal_task: None,
            minibuffer: None,
            transient_focus: cx.focus_handle(),
            menu_buffer: None,
            overlay_focus: crate::overlay::OverlayFocus::default(),
            desktop: None,
            desktop_name: String::new(),
            desktop_sessions: HashMap::new(),
            echo: None,
            git_approval: crate::git_approval::GitApproval::new(cx),
            voice: crate::voice::Voice::default(),
            _event_task: event_task,
            _host_event_task: host_event_task,
            _ledger_event_task: ledger_event_task,
            _desktop_event_task: desktop_event_task,
            _keystroke_subscription: keystroke_subscription,
            _transient_keystroke_interceptor: transient_keystroke_interceptor,
            _window_activation_subscription: window_activation_subscription,
            phone: phone::PhoneUi::new(cx),
        };
        let no_hosts = specs.is_empty();
        for spec in specs {
            this.attach_host(spec, cx);
        }
        // The marks are on this disk, so Home's first draw already shows
        // the user's own verdicts.
        this.refresh_workdirs();
        this.rebuild_wants(cx);
        // A cold start lands on Home: what is running, what is next, and
        // what sits just under the line, without dealing a card.
        let home = this.make_surface(SurfaceKey::Home, window, cx);
        this.display_surface(home, cx);
        this.refresh_home(cx);
        this.focus_active_surface(window, cx);
        // Seed the listing before any event arrives ("+ new agent").
        this.invalidate_dealer_signals(cx);
        // Slack runs from startup, not from the first time the surface is
        // opened: a mention has to become a card whether or not anyone is
        // looking at Slack. And when there is no session it says so: rho
        // deciding in silence that it has no Slack is how a device with no
        // workspace on it looks exactly like one whose Slack went quiet.
        if this.slack_session(window, cx).is_none() {
            // In the log and not the echo line: startup does not get to put
            // a line in the place the next thing the reader does will
            // answer in.
            this.append_message(
                "no slack workspace on this device".to_owned(),
                StyleClass::SystemInfo,
                cx,
            );
        }
        if no_hosts {
            // Same place, same reason: a first start has nothing attached
            // and nothing saved, and the reader needs to know what to do
            // about it rather than watch an empty rail.
            this.append_message(
                "no hosts attached: `space h` attaches one, and what is attached is remembered"
                    .to_owned(),
                StyleClass::SystemInfo,
                cx,
            );
        }
        this
    }

    /// Attaches an agent host. The name is registered with the registry first
    /// so that labels and chrome can qualify by host from the moment the
    /// host exists, not only once it answers.
    pub(crate) fn attach_host(&mut self, spec: HostSpec, cx: &App) -> HostId {
        let agents_client = &self.agents_client;
        let ledger = self.attention.stream();
        let desktop_streams = &self.desktop_streams;
        // The agents client is told the host exists, and gets its agents
        // stream, before any frame from it can arrive.
        let host = self.hosts.attach(
            spec.name.clone(),
            spec.target,
            |host, _| {
                agents_client.attach_host(host, spec.name.clone());
                vec![
                    agents_client.stream(host),
                    ledger.clone(),
                    desktop_streams.stream(host),
                ]
            },
            &gpui_tokio::Tokio::handle(cx),
        );
        self.registry.attach_host(host, spec.name);
        self.refresh_workdirs();
        self.save_hosts();
        host
    }

    /// Writes the attached set down, so that the next start attaches the
    /// same hosts in the same order. A session without a database — a
    /// test — remembers nothing, which is what it wants.
    fn save_hosts(&self) {
        let Some(db) = rho_db::client::shared() else {
            return;
        };
        let specs = self
            .hosts
            .iter()
            .map(|host| HostSpec {
                name: host.name.clone(),
                target: host.target.clone(),
            })
            .collect::<Vec<_>>();
        rho_agent_hosts::saved::save(&db, &specs);
    }

    /// Forgets an agent host: its transcripts, surfaces, and cached projects go
    /// with it, and its connection is torn down by the drop.
    pub(crate) fn detach_host(
        &mut self,
        host: HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let departed = self
            .registry
            .known_agents()
            .copied()
            .filter(|agent_id| self.registry.host_of_agent(*agent_id) == Some(host))
            .collect::<Vec<_>>();
        let contexts = departed
            .iter()
            .copied()
            .map(ContextId::Agent)
            .collect::<HashSet<_>>();
        if self.voice.is_on(host) {
            self.voice.stop();
        }
        self.hosts.detach(host);
        self.quotas.forget(host);
        self.save_hosts();
        self.agents_client.detach_host(host);
        self.ready_hosts.remove(&host);
        self.replay_hosts.remove(&host);
        self.usage.forget_host(host);
        self.remote_projects.retain(|(owner, _), _| *owner != host);
        let gone = self.registry.detach_host(host);
        self.selection.forget(|agent_id| gone.contains(&agent_id));
        self.refresh_agent_wants(departed.clone());
        self.invalidate_dealer_signals(cx);
        for agent_id in departed {
            // The agent is gone with its agent host, so its transcript is a
            // place that no longer exists: one call, and no context can
            // land on it again.
            self.forget_surface(&SurfaceKey::Transcript(agent_id));
            self.active.remove(agent_id);
            self.transcripts.forget(agent_id);
            self.models.remove(&agent_id);
            self.pending_syncs.remove(&agent_id);
        }
        self.note_followed();
        self.forget_contexts(|context| !contexts.contains(context));
        if !self.surfaces.contains_key(&self.active_context) {
            self.active_context = ContextId::Draft;
            let draft = self.make_surface(SurfaceKey::Draft, window, cx);
            self.display_surface(draft, cx);
            self.focus_active_surface(window, cx);
        }
        self.refresh_draft_agent_targets(cx);
        cx.notify();
    }

    /// The agent host an agent lives on. `None` only before its first summary
    /// or creation notice has landed.
    fn host_of(&self, agent_id: AgentId) -> Option<HostId> {
        self.registry.host_of_agent(agent_id)
    }

    /// A host's agents, while the host is attached.
    fn agents(&self, host: HostId) -> Option<AgentsLink> {
        let connection = self.hosts.connection(host)?;
        Some(AgentsLink::new(connection.link()))
    }

    /// The agents of the host an agent lives on, while it is attached.
    fn agents_for(&self, agent_id: AgentId) -> Option<AgentsLink> {
        self.agents(self.host_of(agent_id)?)
    }

    /// How to reach the host an agent lives on, while it is attached.
    fn link_for(&self, agent_id: AgentId) -> Option<rho_agent_hosts::Link> {
        Some(self.hosts.connection(self.host_of(agent_id)?)?.link())
    }

    /// Routes an agent-scoped command to the agent host that owns the agent.
    /// Commands for an agent whose host is unknown or gone are dropped: the
    /// agent host that could act on it is not there to hear them.
    fn send_to_agent(&self, agent_id: AgentId, command: AgentCommand, cx: &mut Context<Self>) {
        if let Some(host) = self.host_of(agent_id) {
            self.call(host, command, cx, |_, (), _| {});
        }
    }

    /// Makes one call of a host's agents. `on_reply` hears the answer; a
    /// refusal, or a host that went before answering, is a notice instead.
    fn call<C: rho_rpc::protocol::Call>(
        &self,
        host: HostId,
        call: C,
        cx: &mut Context<Self>,
        on_reply: impl FnOnce(&mut Self, C::Reply, &mut Context<Self>) + 'static,
    ) {
        let Some(agents) = self.agents(host) else {
            return;
        };
        let reply = agents.call(call);
        cx.spawn(async move |this, cx| {
            let reply = reply.await;
            this.update(cx, |this, cx| match reply {
                Ok(reply) => on_reply(this, reply, cx),
                Err(error) => this.report_refusal(host, &format!("{error:#}"), cx),
            })
            .ok();
        })
        .detach();
    }

    fn report_refusal(&mut self, host: HostId, reason: &str, cx: &mut Context<Self>) {
        let source = self.error_source(host);
        let text = format!("[{source} error: {reason}]");
        self.notice_on(None, &text, StyleClass::SystemImportant, cx);
    }

    /// Whether the agent host behind an agent is answering. Acting on an agent
    /// whose own host is down must fail even when other hosts are fine.
    fn agent_online(&self, agent_id: AgentId) -> bool {
        self.host_of(agent_id)
            .is_some_and(|host| self.hosts.is_online(host))
    }

    /// Any agent host answering: the precondition for actions that choose their
    /// host from user input rather than an existing agent.
    fn connected(&self) -> bool {
        self.hosts.any_online()
    }

    pub(crate) fn active_pane(&self) -> &SurfaceHistory {
        self.history.as_ref().expect("the reader is on a surface")
    }

    pub(crate) fn active_pane_mut(&mut self) -> &mut SurfaceHistory {
        self.history.as_mut().expect("the reader is on a surface")
    }

    /// Where the reader is.
    pub(crate) fn active_surface(&self) -> &Surface {
        &self.active_pane().current().surface
    }

    /// Every surface the workspace is holding, for a test that has to reach
    /// one the reader is not looking at: a message arriving costs the list
    /// its redraw whether or not the list is the surface on screen.
    #[cfg(test)]
    pub(crate) fn open_surfaces_for_test(&self) -> impl Iterator<Item = &Surface> {
        self.surfaces.values().flatten()
    }

    /// Back one surface, or nowhere if the reader is at the oldest entry.
    ///
    /// The surface handed back is the one that was left, still holding its
    /// own point, scroll and folds — except a transcript whose agent has
    /// since been let go, which is rebuilt here because its view is gone
    /// and nothing else would notice. History is one list across contexts,
    /// so this walks into the context the reader came from, and takes them
    /// with it.
    fn show_previous_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(warm) = self.active_pane_mut().back().cloned() else {
            return false;
        };
        self.enter_warm_surface(warm, window, cx);
        true
    }

    /// Forward one surface. `false` means the reader is at the newest entry,
    /// which is where down deals instead — the golden rule.
    fn show_next_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(warm) = self.active_pane_mut().forward().cloned() else {
            return false;
        };
        self.enter_warm_surface(warm, window, cx);
        true
    }

    fn enter_warm_surface(
        &mut self,
        warm: WarmSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_context = warm.context;
        let surface = self.warm_surface(warm.surface, window, cx);
        *self.active_pane_mut().current_mut() = WarmSurface {
            context: warm.context,
            surface: surface.clone(),
        };
        self.ensure_surface_subscription(&surface.key, cx);
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: Self::journal_surface(&surface.key),
            method: rho_journal::SurfaceShowMethod::Mru,
        });
        self.rebuild_home_when_shown(&surface.key, cx);
        cx.notify();
    }

    /// Home's rows are the dealer's hand and the desk's verdicts as they
    /// stood when they were last built, and `refresh_home` can only build
    /// them while Home is a surface. A verdict taken on a transcript is
    /// taken with Home gone, so the refresh it asks for lands nowhere, and
    /// the rows the reader steps back to are the rows from before the
    /// verdict: the agent they had just put away, still listed, and still
    /// listed until some later verdict happened to be taken while Home was
    /// up. Coming back is the moment the answer is wanted, so it is built
    /// then, at both of the places a surface is shown.
    fn rebuild_home_when_shown(&mut self, key: &SurfaceKey, cx: &mut Context<Self>) {
        if *key == SurfaceKey::Home {
            self.refresh_home(cx);
        }
    }

    /// A surface out of history is only as live as what it holds. A
    /// transcript whose agent was evicted while it sat in the stack has a
    /// view over a model nobody is feeding, so it is made again.
    fn warm_surface(
        &mut self,
        surface: Surface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Surface {
        if let SurfaceKey::Transcript(agent_id) = surface.key
            && !self.active.contains(agent_id)
        {
            self.activate_agent(agent_id, cx);
            return self.make_surface(SurfaceKey::Transcript(agent_id), window, cx);
        }
        surface
    }

    fn close_current_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The overview and Home are both floors: there is nothing under
        // them to reveal, so `q` on either stays put.
        if self.active_surface().key == SurfaceKey::Home {
            return;
        }
        let key = self.active_surface().key.clone();
        rho_journal::record(rho_journal::Event::SurfaceClosed {
            surface: Self::journal_surface(&key),
            dealt_untouched: false,
        });
        // Closed, so it is not somewhere back can land — one call, and the
        // entries for it are stepped over rather than hunted down.
        self.forget_surface(&key);
        if !self.show_previous_surface(window, cx) {
            // Nothing behind it, so Home is the floor. Opening Home is an
            // ordinary show and would push what the reader was on, which is
            // the surface that was just closed, so it goes again.
            self.open_home(window, cx);
            self.active_pane_mut().forget(&key);
        }
        cx.notify();
    }

    /// The one place a surface leaves history: it is closed, discarded, or
    /// the thing behind it is gone. Every context forgets it, because a
    /// dead surface is dead everywhere.
    fn forget_surface(&mut self, key: &SurfaceKey) {
        if let Some(history) = self.history.as_mut() {
            history.forget(key);
        }
        rho_journal::record(rho_journal::Event::HistoryRemoved {
            identity: Self::journal_surface(key),
            method: rho_journal::HistoryRemoveMethod::Close,
        });
    }

    /// Home is the front door: a cold start and an emptied queue both
    /// land here.
    pub(crate) fn open_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let surface = self.make_surface(SurfaceKey::Home, window, cx);
        self.display_surface(surface, cx);
        self.refresh_home(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    pub(crate) fn home_view(&self) -> Option<Entity<crate::home::HomeView>> {
        self.find_surface(|surface| surface.key == SurfaceKey::Home)
            .and_then(|surface| match &surface.view {
                SurfaceView::Home(view) => Some(view.clone()),
                _ => None,
            })
    }

    /// Rebuilds Home from the dealer's own hand. Called wherever the dealer
    /// is invalidated, so nothing here polls.
    pub(crate) fn refresh_home(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.home_view() else {
            return;
        };
        let rows = self.home_rows();
        view.update(cx, |view, cx| view.set_rows(rows, cx));
    }

    fn home_rows(&mut self) -> crate::home::HomeRows {
        let now = chrono::Local::now().fixed_offset();
        let hand = self.hand();
        let registry = &self.registry;
        // The name the user gave it, with the handle beside it to tell two
        // of the same name apart — the same label a transcript tab carries,
        // because a card and the surface it opens are the same agent and
        // were reading as two.
        let mut rows = crate::home::split_hand(&hand, |card| {
            crate::home::card_title(card, |agent_id| {
                registry.agent_name_with_labels(agent_id, registry.agent_display_label(agent_id))
            })
        });
        let now_ms = now.timestamp_millis();
        // An agent created by an agent belongs to its creator and is not
        // the reader's to watch; only the ones the reader made are listed.
        // Nor one the user put away. A running turn decides how loudly an
        // agent may ask; a mute and a snooze decide whether it may ask at
        // all, and neither is a cursor, so a turn starting does not take
        // either back. This list read neither, which is why muting or
        // snoozing a working agent did nothing a reader could see until
        // the turn ended.
        let mut running = self
            .registry
            .known_agents()
            .copied()
            .filter(|agent_id| {
                self.registry.created_by_user(*agent_id)
                    && !self
                        .attention
                        .marks
                        .get(&rho_dealer::NodeId::Agent(*agent_id))
                        .put_away(now)
                    && self.registry.agent_facts(*agent_id).turn_running
            })
            .collect::<Vec<_>>();
        // Sorted by what the row shows, or the order is of something the
        // reader cannot see.
        running.sort_by_key(|agent_id| self.registry.agent_display_label(*agent_id));
        rows.running = running
            .into_iter()
            .map(|agent_id| {
                let facts = self.registry.agent_facts(agent_id);
                crate::home::RunningRow {
                    agent_id,
                    // The name, then where the user filed it: two agents
                    // doing the same thing in different places read as two
                    // rows rather than as one name said twice.
                    name: self.registry.agent_name_with_labels(
                        agent_id,
                        self.registry.agent_display_label(agent_id),
                    ),
                    // Where it is filed, not the whole path: the row is
                    // about the agent, and the leaf is what names the work.
                    topic: self
                        .node_context(&rho_dealer::NodeId::Agent(agent_id))
                        .split(", ")
                        .next()
                        .and_then(|path| path.rsplit('/').next())
                        .unwrap_or_default()
                        .to_owned(),
                    elapsed: crate::home::running_elapsed_label(&facts, now_ms),
                    last_line: self
                        .registry
                        .agent_activity(agent_id)
                        .unwrap_or_default()
                        .to_owned(),
                }
            })
            .collect();
        rows
    }

    /// Enter on a Home row: the card it stands for opens, the same way a
    /// pull opens one. Home chooses which; nothing else about it differs.
    fn home_open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.home_view() else {
            return;
        };
        let target = view.update(cx, |view, cx| view.cursor_target(cx));
        let wanted = match target {
            crate::home::HomeTarget::Card(card) => card,
            // A running agent is not a card: its row opens the agent.
            crate::home::HomeTarget::Agent(agent_id) => {
                self.select_agent_inner(Some(agent_id), true, window, cx);
                return;
            }
            crate::home::HomeTarget::None => return,
        };
        let card = self.card_for(&wanted);
        self.open_card(card, window, cx);
        self.invalidate_dealer_signals(cx);
    }

    fn toggle_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_surface().key == SurfaceKey::Home {
            // Already home: the key shows what the reader was reading, the
            // way closing the overview used to reveal it again. Home was
            // pushed like any other surface, so this is one step back and
            // pressing the key twice is a round trip.
            self.show_previous_surface(window, cx);
        } else {
            self.open_home(window, cx);
        }
    }

    fn shell_touch(&mut self, event: &TouchEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.phone.enabled {
            self.phone_debug_touch(event, cx);
            return;
        }
        match event.phase {
            TouchPhase::Started => {
                self.shell_touches.insert(
                    event.id,
                    ShellTouchContact {
                        start: event.position,
                        position: event.position,
                    },
                );
                if self.shell_touches.len() > 1 {
                    self.shell_touch_was_multi = true;
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }
            TouchPhase::Moved => {
                let Some(contact) = self.shell_touches.get_mut(&event.id) else {
                    return;
                };
                contact.position = event.position;

                let action = if !self.shell_touch_committed && self.shell_touch_was_multi {
                    let contacts = self.shell_touches.values().take(2).collect::<Vec<_>>();
                    (contacts.len() == 2)
                        .then(|| {
                            let dx = contacts
                                .iter()
                                .map(|contact| (contact.position.x - contact.start.x).as_f32())
                                .sum::<f32>()
                                / 2.;
                            let dy = contacts
                                .iter()
                                .map(|contact| (contact.position.y - contact.start.y).as_f32())
                                .sum::<f32>()
                                / 2.;
                            if dy.abs() >= SHELL_SWIPE_DISTANCE.as_f32()
                                && dy.abs() >= dx.abs() * 1.25
                            {
                                if dy < 0. {
                                    Some(Box::new(DealOpen) as Box<dyn gpui::Action>)
                                } else {
                                    Some(Box::new(OverviewToggle) as Box<dyn gpui::Action>)
                                }
                            } else {
                                None
                            }
                        })
                        .flatten()
                } else {
                    None
                };

                if let Some(action) = action {
                    self.shell_touch_committed = true;
                    window.dispatch_action(action, cx);
                }
                if self.shell_touch_committed || self.shell_touch_was_multi {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if !self.shell_touches.contains_key(&event.id) {
                    return;
                }
                if self.shell_touch_committed || self.shell_touch_was_multi {
                    window.prevent_default();
                    cx.stop_propagation();
                }
                self.shell_touches.remove(&event.id);
                if self.shell_touches.is_empty() {
                    self.shell_touch_was_multi = false;
                    self.shell_touch_committed = false;
                }
            }
        }
        if self.phone.touch_debug_enabled() {
            cx.notify();
        }
    }

    fn step_surface_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.show_previous_surface(window, cx) {
            return;
        }
        rho_journal::record(rho_journal::Event::HistoryStepped {
            direction: rho_journal::HistoryDirection::Back,
            position: self.active_pane().behind(),
            len: self.active_pane().len(),
        });
        cx.notify();
    }

    /// Down, the golden rule's half that moves: forward through history if
    /// the reader has stepped back, and only at the newest entry does it
    /// deal. Restored from the workspace that had it before `0707dff59a6`
    /// made every down a pull.
    pub(crate) fn cmd_surface_forward_or_deal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_pane().at_newest() {
            self.pull_card(window, cx);
            return;
        }
        if !self.show_next_surface(window, cx) {
            self.pull_card(window, cx);
            return;
        }
        rho_journal::record(rho_journal::Event::HistoryStepped {
            direction: rho_journal::HistoryDirection::Forward,
            position: self.active_pane().behind(),
            len: self.active_pane().len(),
        });
        cx.notify();
    }

    pub(crate) fn journal_card_identity(
        node: &rho_dealer::NodeId,
    ) -> rho_journal::DealerCardIdentity {
        rho_journal::DealerCardIdentity {
            host: 0,
            node_id: node.clone().into(),
        }
    }

    /// Makes the wants of the agents that moved again on the next frame,
    /// once for everything a batch of rows moved. `None` makes every want
    /// again.
    fn schedule_wants(
        &mut self,
        moved: Option<Vec<AgentId>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scheduled = self.wants_pending.is_some();
        let pending = self
            .wants_pending
            .get_or_insert_with(|| Some(BTreeSet::new()));
        match (pending.as_mut(), moved) {
            (Some(pending), Some(moved)) => pending.extend(moved),
            (Some(_), None) => *pending = None,
            (None, _) => {}
        }
        if scheduled {
            return;
        }
        cx.on_next_frame(window, move |this, _window, cx| {
            match this.wants_pending.take().flatten() {
                Some(moved) => this.refresh_agent_wants(moved),
                None => this.rebuild_wants(cx),
            }
            this.invalidate_dealer_signals(cx);
            cx.notify();
        });
    }

    pub(crate) fn invalidate_dealer_signals(&mut self, cx: &mut Context<Self>) {
        if self.dealer_signal_eval_scheduled {
            return;
        }
        self.dealer_signal_eval_scheduled = true;
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| {
                this.dealer_signal_eval_scheduled = false;
                this.evaluate_dealer_signals(cx);
            });
        })
        .detach();
    }

    fn evaluate_dealer_signals(&mut self, cx: &mut Context<Self>) {
        let mut candidates = self.hand();
        // What the lamp is about is what is *not* in front of the reader:
        // the card they are already reading is not news.
        let in_view = self.surface_node(cx);
        candidates.retain(|card| Some(&card.node) != in_view.as_ref());
        // Home is a window onto this same ranking, so it is rebuilt wherever
        // the dealer is invalidated and never on a timer.
        self.refresh_home(cx);
        let top = candidates.first();
        if self.phone.enabled && top.is_some() {
            self.phone.feed_retry = true;
        }
        let max_priority = top.map(|card| card.priority);
        let card = top.map(|card| Self::journal_card_identity(&card.node));
        let mut lamp_on =
            max_priority.is_some_and(|priority| priority >= rho_dealer::curve::LAMP_THRESHOLD);
        // A Slack session that has lost touch is worth the lamp on its own:
        // the queue cannot rank a mention nobody has received yet.
        {
            lamp_on = lamp_on || self.slack.degraded().is_some();
        }
        if lamp_on != self.lamp_on {
            self.lamp_on = lamp_on;
            rho_journal::record(rho_journal::Event::LampTransition {
                state: if lamp_on {
                    rho_journal::SignalState::On
                } else {
                    rho_journal::SignalState::Off
                },
                top_priority: max_priority,
                card: card.clone(),
            });
            cx.notify();
        }
        let chime_above =
            max_priority.is_some_and(|priority| priority >= rho_dealer::curve::CHIME_THRESHOLD);
        if !self.dealer_signals_initialized {
            self.dealer_signals_initialized = true;
            self.chime_above_threshold = chime_above;
            return;
        }
        if chime_above
            && !self.chime_above_threshold
            && let (Some(priority), Some(card)) = (max_priority, card)
        {
            if !cfg!(test) {
                self.chime.play();
            }
            rho_journal::record(rho_journal::Event::ChimeRing {
                top_priority: priority,
                card,
            });
        }
        self.chime_above_threshold = chime_above;
    }

    fn mark_agent_prompt_sent(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        let sent_at = now_ms();
        // The story's own `UserMessage` arrives on the round trip; until
        // then this is what keeps the user's own reply from chiming back
        // at them.
        self.agent_last_interaction.insert(agent_id, sent_at as i64);
        self.invalidate_dealer_signals(cx);
    }

    fn context_for_agent(&self, agent_id: AgentId) -> ContextId {
        ContextId::Agent(agent_id)
    }

    /// Drops contexts for tasks that no longer exist; their views (and any
    /// workspace file channels behind them) release with them.
    fn prune_contexts(&mut self) {
        let live = self
            .registry
            .known_agents()
            .copied()
            .collect::<HashSet<_>>();
        let keep = |context: &ContextId| match context {
            ContextId::Draft => true,
            ContextId::Agent(agent_id) => live.contains(agent_id),
            ContextId::Slack => true,
        };
        self.forget_contexts(keep);
        self.phone.retain_contexts(keep);
        if !self.surfaces.contains_key(&self.active_context) {
            self.active_context = ContextId::Draft;
        }
    }

    /// A context is going: every surface it held leaves history with it.
    /// One list means one place to say so, and a surface whose context is
    /// gone is a place that no longer exists whatever its key says.
    fn forget_contexts(&mut self, keep: impl Fn(&ContextId) -> bool) {
        let mut gone = Vec::new();
        self.surfaces.retain(|context, surfaces| {
            if keep(context) {
                return true;
            }
            gone.extend(surfaces.iter().map(|surface| surface.key.clone()));
            false
        });
        if let Some(history) = self.history.as_mut() {
            for key in gone {
                history.forget(&key);
            }
        }
    }

    pub(crate) fn handle_model_events(
        &mut self,
        events: Vec<rho_agents_client::model::ModelEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Main-thread work outside a frame. The frame ring accounts for
        // time inside `Window::draw` and nothing else, so this batch is
        // invisible in it — and it is exactly the work that makes the next
        // frame late. It is recorded with its own count so a slow batch can
        // be divided by the events it reconciled.
        let start = std::time::Instant::now();
        let count = events.len() as u64;
        for rho_agents_client::model::ModelEvent { host, msg } in events {
            self.handle_model_event(host, msg, window, cx);
        }
        gpui::profiler::record_main_thread_work(gpui::profiler::MainThreadWork {
            owner: gpui::profiler::MainThreadWorkKind::ModelEvent,
            start,
            end: std::time::Instant::now(),
            work_units: count,
        });
    }

    pub(crate) fn handle_model_event(
        &mut self,
        host: HostId,
        msg: rho_agents_client::model::ModelMsg,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match msg {
            rho_agents_client::model::ModelMsg::Loaded { agents, verdicts } => {
                self.loaded(host, agents, verdicts);
                self.rebuild_wants(cx);
                self.invalidate_dealer_signals(cx);
                cx.notify();
            }
            rho_agents_client::model::ModelMsg::Changed { agents } => {
                let changed = self.registry.told(agents);
                if changed.is_empty() {
                    return;
                }
                // The wants these agents own are made again here, not when
                // the frame comes round: a fact that has moved is exactly
                // when a want is made, and everything that reads the
                // ranking in between must see it. Their marks go with them:
                // an agent that just arrived may already have a name.
                self.push_agent_marks(&changed);
                self.refresh_agent_wants(changed.iter().copied());
                self.schedule_wants(Some(changed), window, cx);
            }
            rho_agents_client::model::ModelMsg::Rows { agent_id, rows } => {
                self.refold_open_transcript(agent_id, &rows, window, cx);
            }
            rho_agents_client::model::ModelMsg::Live { agent_id, live } => {
                self.handle_frame_batch(vec![(agent_id, TranscriptFrame::Live(live))], window, cx);
            }
            rho_agents_client::model::ModelMsg::Host {
                machine_seed,
                agent_counter,
            } => {
                self.registry
                    .set_host_data(host, machine_seed, agent_counter);
                cx.notify();
            }
            rho_agents_client::model::ModelMsg::Auth { auth } => {
                self.quotas.set_auth(host, auth);
                if let Some(view) = self.usage.opened_view() {
                    let history = self.quotas.merged_history(&self.hosts);
                    let active = self.quotas.active_namespaces(&self.hosts);
                    view.update(cx, |view, cx| view.quota_arrived(history, active, cx));
                }
                cx.notify();
            }
            rho_agents_client::model::ModelMsg::AgentCreated {
                agent_id,
                agent_counter,
            } => {
                let machine_seed = self.registry.host_machine_seed(host);
                self.registry
                    .set_host_data(host, machine_seed, agent_counter);
                self.note_agent_created(host, agent_id);
                cx.notify();
            }
            rho_agents_client::model::ModelMsg::QuotaUsage { summaries } => {
                self.quotas.set_summaries(host, summaries);
                cx.notify();
            }
        }
    }

    fn handle_frame_batch(
        &mut self,
        frames: Vec<(AgentId, TranscriptFrame)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut order = Vec::new();
        let mut changes: HashMap<AgentId, (FrameSummary, Option<u64>, bool)> = HashMap::new();
        let mut live_changed = false;

        for (agent_id, frame) in frames {
            let Some((summary, old_context, usage_changed, became_live)) =
                self.apply_frame_state(agent_id, frame)
            else {
                continue;
            };
            live_changed |= became_live;
            changes
                .entry(agent_id)
                .and_modify(|(pending, _, refresh_usage)| {
                    *pending = pending.merge(summary);
                    *refresh_usage |= usage_changed;
                })
                .or_insert_with(|| {
                    order.push(agent_id);
                    (summary, old_context, usage_changed)
                });
        }

        if live_changed {
            self.refresh_draft_agent_targets(cx);
        }

        for agent_id in &order {
            let old_context = changes[agent_id].1;
            let new_context = self.transcripts.context_used(agent_id);
            if old_context != new_context
                && let Some(view) = self.models.get(agent_id).cloned()
            {
                self.refresh_view_status(agent_id, &view, cx);
            }
            if changes[agent_id].2
                && let Some(view) = self.models.get(agent_id).cloned()
            {
                self.refresh_view_status(agent_id, &view, cx);
            }
        }

        for agent_id in order {
            let summary = changes[&agent_id].0;
            let (view, started) = self.ensure_agent_model(agent_id, window, cx);
            self.sync_agent_model(agent_id, &view, summary, started, cx);
        }

        self.ensure_duration_timer(cx);
        // Selected views notify themselves when their editor changes. Only a
        // newly-live agent changes workspace chrome; background transcript
        // frames should not dirty the window.
        if live_changed {
            cx.notify();
        }
    }

    pub(crate) fn handle_event(
        &mut self,
        host: HostId,
        event: ConnEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConnEvent::Ready => {
                self.replay_hosts.remove(&host);
                let first_ready = self.ready_hosts.insert(host);
                self.prune_contexts();
                self.refresh_workdirs();
                self.hosts.set_status(host, HostStatus::Online);
                self.refresh_draft_agent_targets(cx);
                if first_ready && matches!(self.selection.active_pane(), ActivePane::Startup) {
                    // The startup scaffold guessed before agent host data existed;
                    // refresh it now that workdir names and topics are known.
                    self.seed_draft(false, window, cx);
                }
                // The focus set is this client's to keep; an agent host that
                // just came up is told it whole.
                self.send_agent_focus_to(host);
                self.update_statuses(cx);
                cx.notify();
            }
            ConnEvent::Many(events) => {
                for event in events {
                    self.handle_event(host, event, window, cx);
                }
            }
            ConnEvent::ServerError(message) => self.report_refusal(host, &message, cx),
            ConnEvent::Recovering(elapsed) => {
                let changed = !self
                    .hosts
                    .get(host)
                    .is_some_and(|entry| matches!(entry.status, HostStatus::Recovering(_)));
                self.hosts.set_status(host, HostStatus::Recovering(elapsed));
                if changed {
                    let source = self.hosts.host_label(host);
                    self.notice_on(
                        None,
                        &format!("[{source} reconnecting]"),
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
                cx.notify();
            }
            ConnEvent::Recovered => {
                self.hosts.set_status(host, HostStatus::Online);
                let source = self.hosts.host_label(host);
                self.notice_on(
                    None,
                    &format!("[{source} connected]"),
                    StyleClass::SystemInfo,
                    cx,
                );
                cx.notify();
            }
            ConnEvent::Disconnected(reason) => {
                self.desktop_sessions.remove(&host);
                // An agent host that goes is an agent host that is no longer asking;
                // the request still has to be answered, or it is left
                // blocked on a channel nobody will send on.
                if self.git_approval.answer(GitApprovalDecision::Done) {
                    self.finish_overlay_focus(window, cx);
                }
                // The host's agents stay in the rail with their retained
                // transcripts: losing a connection is not losing the work.
                // Only detaching (`space h d`) forgets an agent host.
                self.hosts
                    .set_status(host, HostStatus::Disconnected(reason.clone()));
                self.replay_hosts.insert(host);
                let source = self.hosts.host_label(host);
                self.notice_on(
                    None,
                    &format!("[{source} disconnected: {reason}]"),
                    StyleClass::SystemImportant,
                    cx,
                );
                // Keep the ready marker so the next handshake can replay the
                // retained session instead of treating it as first startup.
                if self.awaiting_draft_agent == Some(host) {
                    self.awaiting_draft_agent = None;
                }
                if self.voice.is_on(host) {
                    self.voice.stop();
                }
                self.update_statuses(cx);
                cx.notify();
            }
            ConnEvent::GitTransportApproval {
                request_id,
                prompt,
                response,
            } => {
                // Deliberately not has_modal_overlay: an open menu does
                // not deny the request. Denying answers the agent host with a
                // no, and a menu is a choice with nothing typed into it,
                // reopened at no cost — a prompt has the reader's text in
                // it, and that is what "another prompt is active" means.
                if self.minibuffer.is_some() || self.git_approval.waiting() {
                    let _ = response.send(GitApprovalDecision::Deny);
                    let source = self.error_source(host);
                    self.notice_on(
                        None,
                        &format!(
                            "[SSH Git request from {source} denied: another prompt is active]"
                        ),
                        StyleClass::SystemImportant,
                        cx,
                    );
                    return;
                }
                // The prompt names its host: approving an SSH Git operation
                // is a decision about which machine reaches out.
                let prompt = match self.hosts.len() > 1 {
                    true => format!("{}: {prompt}", self.hosts.host_label(host)),
                    false => prompt,
                };
                self.git_approval.ask(request_id, prompt, response);
                self.overlay_focus.capture(window, cx);
                window.focus(self.git_approval.focus_handle(), cx);
                self.echo = None;
                cx.notify();
            }
            ConnEvent::GitTransportDone { request_id } => {
                if self.git_approval.done(request_id) {
                    self.finish_overlay_focus(window, cx);
                    cx.notify();
                }
            }
        }
        // Every agent host event funnels through here, so this one call is
        // the event-driven replacement for reconciling on render.
        self.invalidate_dealer_signals(cx);
    }

    /// How an agent host names itself in error text: bare when it is the only
    /// one, otherwise by host.
    fn error_source(&self, host: HostId) -> String {
        match self.hosts.len() > 1 {
            true => format!("rho-agent-host {}", self.hosts.host_label(host)),
            false => "rho-agent-host".to_owned(),
        }
    }

    /// Quota headroom across hosts. A named account stands on its own -
    /// ChatGPT's OAuth namespaces and Claude's accounts alike, since each is
    /// its own subscription. Unnamed rows keep the historical
    /// binding-constraint merge: they say nothing about whose quota they are.
    /// Enter with the cursor in one of the draft's header rows: the draft is
    /// one message however many rows it has, so this sends it. In the body,
    /// where the prompt's own insert-mode enter lives, the key stays vim's
    /// motion and is passed straight on.
    fn submit_from_draft_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let in_field = self
            .focused_draft_editor()
            .is_some_and(|editor| self.draft_model.read(cx).cursor_in_a_field(&editor, cx));
        if in_field {
            self.submit_prompt(&SubmitPrompt, window, cx);
        } else if let Ok(action) = cx.build_action("vim::NextLineStart", None) {
            window.dispatch_action(action, cx);
        }
    }

    /// `ctrl-u` on a header row: the row is emptied and the cursor stays in
    /// it, ready to type. In the body the key is vim's own scroll.
    fn clear_draft_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let in_field = self
            .focused_draft_editor()
            .is_some_and(|editor| self.draft_model.read(cx).cursor_in_a_field(&editor, cx));
        if !in_field {
            if let Ok(action) = cx.build_action("vim::ScrollUp", None) {
                window.dispatch_action(action, cx);
            }
            return;
        }
        let Some(editor) = self.focused_draft_editor() else {
            return;
        };
        self.draft_model
            .update(cx, |view, cx| view.clear_field(&editor, window, cx));
        self.enter_insert_mode(window, cx);
    }

    fn submit_prompt(&mut self, _: &SubmitPrompt, window: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_surface().view {
            model.clone().update(cx, |model, cx| model.submit(cx));
            return;
        }
        if matches!(
            self.active_surface().view,
            SurfaceView::SlackConversation(_)
        ) {
            self.slack_submit(cx);
            return;
        }
        match self.selection.selected_agent() {
            Some(agent_id) => {
                let Some(view) = self.models.get(&agent_id).cloned() else {
                    return;
                };
                let Some(content) = view.update(cx, |view, cx| view.take_prompt(cx)) else {
                    return;
                };
                self.handle_submit(agent_id, content, cx);
            }
            None => self.submit_draft(window, cx),
        }
    }

    fn shell_interrupt(&mut self, _: &ShellInterrupt, _: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_surface().view {
            model.clone().update(cx, |model, _| model.interrupt());
        }
    }

    pub(crate) fn cmd_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_voice(&VoiceToggle, window, cx);
    }

    fn toggle_voice(&mut self, _: &VoiceToggle, _: &mut Window, cx: &mut Context<Self>) {
        if self.voice.running() {
            let message = match self.voice.toggle_mute() {
                true => "voice microphone muted",
                false => "voice microphone unmuted",
            };
            self.notice_on(None, message, StyleClass::SystemInfo, cx);
            return;
        }
        self.voice.unmute();
        // Voice follows what the user is looking at: start on the selected
        // agent's agent host.
        let host = self
            .selection
            .selected_agent()
            .and_then(|agent_id| self.host_of(agent_id))
            .filter(|host| self.hosts.is_online(*host))
            .or_else(|| self.hosts.primary());
        self.start_voice(host, cx);
    }

    pub(crate) fn cmd_end_voice(&mut self, cx: &mut Context<Self>) {
        self.voice.end();
        if self.voice.running() {
            self.voice.stop();
            self.notice_on(None, "ending voice session…", StyleClass::SystemInfo, cx);
        } else {
            self.notice_on(None, "voice is not active", StyleClass::SystemInfo, cx);
        }
    }

    fn start_voice(&mut self, host: Option<HostId>, cx: &mut Context<Self>) {
        if self.voice.running() {
            return;
        }
        let Some(host) = host.or(self.voice.host()).or_else(|| self.hosts.primary()) else {
            self.notice_on(
                None,
                "voice: no agent host attached",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.voice.wants_host(host);
        let Some(connection) = self.hosts.connection(host) else {
            return;
        };
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let (input_muted, input_muted_rx) = tokio::sync::watch::channel(self.voice.muted());
        let task = crate::voice::rtc::start(&connection.link(), stop_rx, input_muted_rx);
        let starting = match self.hosts.len() > 1 {
            true => format!("starting voice on {}…", self.hosts.host_label(host)),
            false => "starting voice…".to_owned(),
        };
        self.notice_on(None, &starting, StyleClass::SystemInfo, cx);
        let session = cx.spawn(async move |this, cx| {
            let result = task.await;
            if result.is_err() {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
            }
            let _ = this.update(cx, |this, cx| {
                this.voice.finished();
                let message = match result {
                    Ok(()) => "voice stopped listening".to_owned(),
                    Err(error) => format!("voice failed: {error:#}"),
                };
                this.notice_on(None, &message, StyleClass::SystemInfo, cx);
                let host = this.voice.host().filter(|host| this.hosts.is_online(*host));
                if host.is_some() && this.voice.wanted() {
                    this.start_voice(host, cx);
                }
            });
        });
        self.voice.started(session, stop, input_muted);
    }

    fn shell_eof(&mut self, _: &ShellEof, _: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_surface().view {
            model.clone().update(cx, |model, cx| model.eof(cx));
        }
    }

    fn shell_pager_action(
        &mut self,
        action: rho_shell_view::protocol::PagerAction,
        cx: &mut Context<Self>,
    ) {
        if let SurfaceView::Shell { model, .. } = &self.active_surface().view {
            model.update(cx, |model, _| model.pager_action(action));
        }
    }

    fn handle_submit(
        &mut self,
        agent_id: AgentId,
        content: Vec<ContentPart>,
        cx: &mut Context<Self>,
    ) {
        if !self.connected() {
            self.notice_on(
                Some(&agent_id),
                "not connected to an agent host",
                StyleClass::SystemImportant,
                cx,
            );
            return;
        }
        self.send_to_agent(
            agent_id,
            AgentCommand::Send {
                agent_id,
                content,
                delivery: MessageDelivery::NextRequest,
            },
            cx,
        );
        // Engagement bump: keeps display-time staleness correct between
        // topic refreshes (the agent host persists the same timestamp).
        self.registry.touch_agent(agent_id);
        self.mark_agent_prompt_sent(agent_id, cx);
        cx.notify();
    }

    /// What the map says about the label in the start field. The map is
    /// still the shell's; `rho-agents` is handed the answer, not the map.
    fn start_base(&self, target: &str) -> StartBase {
        let agent = self.registry.agent_by_label(target);
        StartBase {
            host: agent.and_then(|agent_id| self.host_of(agent_id)),
            workspace: agent.and_then(|agent_id| self.registry.agent_workspace(agent_id)),
        }
    }

    /// Why a submission did not become an agent. The echo area says it once,
    /// and the draft keeps it: a refusal the reader has to act on outlives
    /// the two seconds an echo lasts.
    pub(crate) fn refuse_draft(&mut self, message: &str, cx: &mut Context<Self>) {
        self.notice_on(None, message, StyleClass::SystemImportant, cx);
        self.draft_model.update(cx, |draft, cx| {
            draft.set_refusal(Some(message.to_owned()), cx)
        });
    }

    /// Submitting the compose surface creates the agent: the workdir field
    /// picks the working directory, the topic is whatever the draft
    /// inherited. The buffers are not cleared here — they survive until the
    /// agent host confirms creation.
    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(content) = self.draft_model.read(cx).content(cx) else {
            // Enter in the workdir field with nothing to send: jump to the
            // body instead of submitting.
            if let Some(editor) = self.focused_draft_editor() {
                self.draft_model
                    .update(cx, |view, cx| view.focus_body(&editor, window, cx));
            }
            return;
        };
        // Whatever refused the last submission is answered by this one.
        self.draft_model
            .update(cx, |draft, cx| draft.set_refusal(None, cx));
        if !self.connected() {
            self.refuse_draft("not connected to an agent host", cx);
            return;
        }
        let field = self.draft_model.read(cx).workdir_text(cx).trim().to_owned();
        let working_directory = if field.is_empty() {
            self.draft_default_workdir()
        } else {
            match rho_agents_client::create::resolve_workdir(&self.hosts, &field) {
                Ok(workdir) => Some(workdir),
                Err(message) => {
                    self.refuse_draft(&message, cx);
                    return;
                }
            }
        };
        let (host, start) = {
            let draft = self.draft_model.read(cx);
            let mode = draft.start_mode();
            let target = draft.start_text(cx).trim().to_owned();
            match parse_start(
                &self.hosts,
                mode,
                &target,
                working_directory,
                None,
                self.start_base(&target),
            ) {
                Ok(start) => start,
                Err(message) => {
                    self.refuse_draft(&message, cx);
                    return;
                }
            }
        };
        let role = match parse_agent_role(self.draft_model.read(cx).role_text(cx).trim()) {
            Ok(role) => role,
            Err(message) => {
                self.refuse_draft(&message, cx);
                return;
            }
        };
        let mode = match parse_workset_mode(&self.draft_model.read(cx).filesystem_text(cx)) {
            Ok(mode) => mode,
            Err(message) => {
                self.refuse_draft(&message, cx);
                return;
            }
        };
        self.awaiting_draft_agent = Some(host);
        // `n a` chose an area, and that is where the agent is filed; an
        // ordinary draft has none and starts at the root.
        self.pending_agent_filing = self.draft_area.take().map(|area| (host, area));
        let Some(agents) = self.agents(host) else {
            return;
        };
        let reply = agents.call(NewAgent {
            role,
            start,
            mode,
            content: Some(content),
        });
        cx.spawn_in(window, async move |this, cx| {
            let reply = reply.await;
            this.update_in(cx, |this, window, cx| {
                this.draft_answered(host, reply, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// The host's answer to the draft's submission: the agent it became,
    /// filed and followed, or why there is none.
    fn draft_answered(
        &mut self,
        host: HostId,
        reply: anyhow::Result<AgentId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let awaited = self.awaiting_draft_agent == Some(host);
        if awaited {
            self.awaiting_draft_agent = None;
        }
        let filing = self
            .pending_agent_filing
            .take_if(|(filing_host, _)| *filing_host == host);
        let agent_id = match reply {
            Ok(agent_id) => agent_id,
            // A failed creation keeps the draft buffers; the user fixes the
            // workdir and submits again. The agent host's whole cause is what
            // the draft shows, so the reason a creation refused is readable
            // for longer than an echo.
            Err(error) => {
                let source = self.error_source(host);
                let text = format!("[{source} error: {error:#}]");
                if awaited {
                    self.refuse_draft(&text, cx);
                } else {
                    self.notice_on(None, &text, StyleClass::SystemImportant, cx);
                }
                return;
            }
        };
        self.note_agent_created(host, agent_id);
        if let Some((_, area)) = filing {
            let writes = self.new_thing_marks(&rho_dealer::NodeId::Agent(agent_id), Some(&area));
            self.write_marks(writes, cx);
        }
        if awaited {
            self.activate_agent(agent_id, cx);
            // The draft became this agent: reset the compose surface and
            // follow the new agent.
            let label = self
                .draft_default_workdir()
                .map(|path| self.hosts.workdir_label(&path))
                .unwrap_or_default();
            self.draft_model.update(cx, |view, cx| {
                view.set_body_text("", cx);
                view.clear_attachments(cx);
                view.set_workdir_text(&label, cx);
                view.set_role_text(rho_agents_client::create::DEFAULT_ROLE, cx);
                view.set_start_text(rho_agents_client::create::DEFAULT_START, cx);
                view.set_filesystem_text(rho_agents_client::create::DEFAULT_FILESYSTEM, cx);
            });
            self.select_agent(Some(agent_id), window, cx);
        }
        self.invalidate_dealer_signals(cx);
        cx.notify();
    }

    fn paste_prompt(&mut self, _: &PastePrompt, window: &mut Window, cx: &mut Context<Self>) {
        self.cmd_paste_prompt(window, cx);
    }

    pub(crate) fn cmd_paste_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let pane_prompt = matches!(
            self.active_surface().view,
            SurfaceView::Draft { .. }
                | SurfaceView::Transcript { .. }
                | SurfaceView::SlackConversation(_)
        );
        let images = item
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) if !image.bytes.is_empty() => Some(image),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !pane_prompt || images.is_empty() {
            let editor = self.active_editor(cx);
            editor.update(cx, |editor, cx| editor.paste_item(&item, window, cx));
            return;
        }
        let mut accepted = 0;
        for image in images {
            let media_type = match image.format {
                gpui::ImageFormat::Png => "image/png",
                gpui::ImageFormat::Jpeg => "image/jpeg",
                gpui::ImageFormat::Webp => "image/webp",
                gpui::ImageFormat::Gif => "image/gif",
                _ => {
                    self.notice_on(
                        None,
                        "unsupported clipboard image format (use PNG, JPEG, WebP, or GIF)",
                        StyleClass::SystemImportant,
                        cx,
                    );
                    continue;
                }
            };
            let added = match &self.active_surface().view {
                // A picture pasted into a conversation is an attachment on
                // the next message, not bytes pasted into the composer.
                SurfaceView::SlackConversation(_) => {
                    let name = format!("image.{}", extension(image.format));
                    self.slack_attach_bytes(name, image.bytes.clone(), cx)
                }
                SurfaceView::Draft { .. } => {
                    self.draft_model.update(cx, |model, cx| {
                        model.add_image(media_type.to_owned(), image.bytes.clone(), cx)
                    });
                    true
                }
                SurfaceView::Transcript { model, .. } => {
                    model.update(cx, |model, cx| {
                        model.add_image(media_type.to_owned(), image.bytes.clone(), cx)
                    });
                    true
                }
                _ => false,
            };
            accepted += usize::from(added);
        }
        if accepted > 0 {
            cx.stop_propagation();
        }
    }

    pub(crate) fn cmd_clear_prompt_attachments(&mut self, cx: &mut Context<Self>) {
        let cleared = match &self.active_surface().view {
            SurfaceView::SlackConversation(_) => self.slack_clear_attachment(cx),
            SurfaceView::Draft { .. } => self
                .draft_model
                .update(cx, |model, cx| model.clear_attachments(cx)),
            SurfaceView::Transcript { model, .. } => {
                model.update(cx, |model, cx| model.clear_attachments(cx))
            }
            _ => false,
        };
        if cleared {
            let agent_id = self.selection.selected_agent();
            self.notice_on(
                agent_id.as_ref(),
                "image attachments cleared",
                StyleClass::SystemInfo,
                cx,
            );
        }
    }

    /// Interprets the draft's start field (`auto` selects the first available
    /// local `main`, local `master`, or `trunk()`). An agent label resolves to
    /// the agent's workspace — `<ws-id>@` as a stacking base, or the workspace
    /// itself for Join; anything else is a revset (stacking only). `user` is
    /// only meaningful for Join — your own checkout. Agent targets carry their
    /// own repo; `workdir` is only needed (and only checked) for the other
    /// arms.
    /// Every command a transient can run goes through one of these
    /// `cmd_*` methods: no textual grammar, no dispatch enum — the menu
    /// item closure is the command.
    fn require_connected(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.connected() {
            self.notice_on(
                None,
                "not connected to an agent host",
                StyleClass::SystemInfo,
                cx,
            );
        }
        self.connected()
    }

    /// The selected agent's agent host must be answering for an agent-scoped
    /// command to mean anything; another host being up is no help.
    fn require_agent_online(&mut self, agent_id: AgentId, cx: &mut Context<Self>) -> bool {
        if !self.agent_online(agent_id) {
            let host = self
                .host_of(agent_id)
                .map(|host| self.hosts.host_label(host))
                .unwrap_or_else(|| "its agent host".to_owned());
            let message = format!("not connected to {host}");
            self.notice_on(Some(&agent_id), &message, StyleClass::SystemInfo, cx);
        }
        self.agent_online(agent_id)
    }

    pub(crate) fn cmd_agent_cancel(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("cancel", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(agent_id, AgentCommand::Cancel { agent_id }, cx);
        }
    }

    pub(crate) fn cmd_rewind(&mut self, turns: u32, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("rewind", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(agent_id, AgentCommand::Rewind { agent_id, turns }, cx);
        }
    }

    pub(crate) fn cmd_continue_turn(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("continue", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(agent_id, AgentCommand::Continue { agent_id }, cx);
        }
    }

    pub(crate) fn cmd_compact(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("compact", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(
                agent_id,
                AgentCommand::Compact {
                    agent_id,
                    delivery: rho_agent_types::MessageDelivery::NextRequest,
                },
                cx,
            );
            self.notice_on(
                Some(&agent_id),
                "compacting context",
                StyleClass::SystemInfo,
                cx,
            );
        }
    }

    pub(crate) fn cmd_change_prompt_cache_key(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("change-prompt-cache-key", window, cx)
        {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(
                agent_id,
                AgentCommand::ChangePromptCacheKey { agent_id },
                cx,
            );
            self.notice_on(
                Some(&agent_id),
                "changed prompt cache key",
                StyleClass::SystemInfo,
                cx,
            );
        }
    }

    pub(crate) fn cmd_change_agent_role(
        &mut self,
        intelligence: EngineerIntelligence,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = self.subject_agent_or_notice("change-role", window, cx) else {
            return;
        };
        if !self.require_agent_online(agent_id, cx) {
            return;
        }
        self.send_to_agent(
            agent_id,
            AgentCommand::ChangeRole {
                agent_id,
                role: AgentRole::Engineer { intelligence },
            },
            cx,
        );
    }

    pub(crate) fn cmd_change_agent_mode(
        &mut self,
        mode: rho_agent_types::WorksetMode,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = self.subject_agent_or_notice("change-filesystem", window, cx) else {
            return;
        };
        if !self.require_agent_online(agent_id, cx) {
            return;
        }
        self.send_to_agent(agent_id, AgentCommand::ChangeMode { agent_id, mode }, cx);
        self.notice_on(
            Some(&agent_id),
            &format!(
                "filesystem set to {}: the agent restarts in it, and its notebook starts over",
                mode_label(mode)
            ),
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// `space a f`: which filesystem the agent works in, `view` or
    /// `exposed`. The current one is named in the prompt.
    pub(crate) fn prompt_change_agent_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.subject_agent_or_notice("change-filesystem", window, cx) else {
            return;
        };
        let current = self
            .registry
            .agent_place(agent_id)
            .map(|place| place.mode)
            .unwrap_or_default();
        let complete = std::rc::Rc::new(move |_: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_ascii_lowercase();
            crate::commands::filesystem_field_candidates("")
                .into_iter()
                .filter(|candidate| candidate.value.contains(&needle))
                .map(|mut candidate| {
                    if candidate.value == mode_label(current) {
                        candidate.description = format!("{} (now)", candidate.description);
                    }
                    candidate
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                match parse_workset_mode(&input) {
                    Ok(mode) => workspace.cmd_change_agent_mode(mode, window, cx),
                    Err(message) => workspace.notice_on(
                        None,
                        &format!("change-filesystem: {message}"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            },
        );
        self.open_prompt("filesystem:", complete, on_submit, window, cx);
    }

    pub(crate) fn prompt_change_agent_role(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.subject_agent_or_notice("change-role", window, cx) else {
            return;
        };
        let Some(role) = self.registry.agent_role(agent_id) else {
            return;
        };
        let roles: &[&str] = match role {
            AgentRole::Engineer {
                intelligence:
                    EngineerIntelligence::Mini
                    | EngineerIntelligence::Medium
                    | EngineerIntelligence::High,
            } => &["mini-eng", "med-eng", "high-eng"],
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium1 | EngineerIntelligence::High1,
            } => &["med1-eng", "high1-eng"],
            _ => {
                self.notice_on(
                    Some(&agent_id),
                    "role changes are not available for this agent",
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            }
        };
        let complete = std::rc::Rc::new(move |_: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_ascii_lowercase();
            roles
                .iter()
                .filter(|role| role.contains(&needle))
                .map(|role| crate::commands::Candidate {
                    value: (*role).to_owned(),
                    description: "engineer role".to_owned(),
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let intelligence = match input.trim().to_ascii_lowercase().as_str() {
                    "mini-eng" => Some(EngineerIntelligence::Mini),
                    "med-eng" => Some(EngineerIntelligence::Medium),
                    "high-eng" => Some(EngineerIntelligence::High),
                    "med1-eng" => Some(EngineerIntelligence::Medium1),
                    "high1-eng" => Some(EngineerIntelligence::High1),
                    _ => None,
                };
                match intelligence {
                    Some(intelligence) => workspace.cmd_change_agent_role(intelligence, window, cx),
                    None => workspace.notice_on(
                        None,
                        "change-role: choose a listed engineer role",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            },
        );
        self.open_prompt("role:", complete, on_submit, window, cx);
    }

    /// `space a n`: what the user calls this agent. The runtime titles an
    /// agent from the first thing said to it, which is a guess and often
    /// wrong by the time the agent is doing something else; a name is the
    /// user saying which agent this is, and it is theirs, so it lives on
    /// the ledger as the agent's `name` mark rather than anywhere the
    /// runtime can overwrite it.
    ///
    /// An empty name takes theirs off and the runtime's title comes back.
    /// `n` in the verdict menu. The subject is the card in view, the same
    /// one every other key in that menu acts on, so naming cannot land on
    /// a different thing from the one the user is looking at — which is
    /// what the agent menu's own `n` could do. An agent today; a Slack
    /// unit is the same name on its own cell and comes with the mirror.
    pub(crate) fn cmd_verdict_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.card_in_view(cx) else {
            self.echo("name: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        };
        let Some(agent_id) = card.node.agent() else {
            self.echo(
                "name: only an agent can be named",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.prompt_name_agent(agent_id, window, cx);
    }

    pub(crate) fn prompt_name_agent(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &gpui::App| Vec::new());
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                workspace.name_agent(agent_id, input.trim().to_owned(), window, cx);
            },
        );
        self.open_prompt("name:", complete, on_submit, window, cx);
    }

    /// The name written where the user's own words about a thing go. The
    /// registry reads it back through the agent's marks, so every place
    /// that shows the agent's title shows this one instead.
    pub(crate) fn name_agent(
        &mut self,
        agent_id: AgentId,
        name: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let node = rho_dealer::NodeId::Agent(agent_id);
        let name = name.trim().to_owned();
        let written = (!name.is_empty()).then(|| name.clone());
        self.write_marks(vec![rho_dealer::marks::name(&node, written)], cx);
        let said = match name.is_empty() {
            true => "name removed".to_owned(),
            false => format!("name: {name}"),
        };
        self.echo(&said, StyleClass::SystemInfo, cx);
    }

    pub(crate) fn verdict_done(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.deal_card_is_target(cx) {
            self.echo("done: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        }
        if !self.submit_verdict(
            None,
            crate::attention::Verdict::Done,
            rho_journal::DealerVerdict::Done,
            rho_journal::PhoneVerdict::Done,
            "done".to_owned(),
            window,
            cx,
        ) {
            self.echo("done: nothing to mark done", StyleClass::SystemInfo, cx);
        }
    }

    /// `x`: nothing from this reaches the user again. A Slack unit is
    /// muted in Slack; anything else is muted here, for good.
    pub(crate) fn verdict_mute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.deal_card_is_target(cx) {
            self.echo("mute: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        }
        if !self.submit_verdict(
            None,
            crate::attention::Verdict::Mute,
            rho_journal::DealerVerdict::Mute,
            rho_journal::PhoneVerdict::Mute,
            "mute".to_owned(),
            window,
            cx,
        ) {
            self.echo("mute: nothing to mute", StyleClass::SystemInfo, cx);
        }
    }

    /// `t`: handled for now, and owed again: the card comes back on a
    /// pace, in days, defaulting to a week.
    pub(crate) fn verdict_todo(
        &mut self,
        count: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let days = count.unwrap_or(7).max(1) as u32;
        if !self.deal_card_is_target(cx) {
            self.echo("todo: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        }
        if !self.submit_verdict(
            None,
            crate::attention::Verdict::Todo { pace_days: days },
            rho_journal::DealerVerdict::Done,
            rho_journal::PhoneVerdict::Todo,
            "todo".to_owned(),
            window,
            cx,
        ) {
            self.echo("todo: nothing to mark", StyleClass::SystemInfo, cx);
        }
    }

    /// `shift-s`: the room the card sits in goes quiet, not the card. A
    /// Slack thread's room is its channel; a conversation is its own room.
    pub(crate) fn verdict_room_snooze(
        &mut self,
        count: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let days = count.unwrap_or(1).max(1) as i64;
        let today = chrono::Local::now().date_naive();
        let Some(card) = self.card_in_view(cx) else {
            self.echo("snooze: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        };
        let rho_dealer::NodeId::Slack(unit) = &card.node else {
            self.echo(
                "room snooze: only a Slack card sits in a room",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let room = rho_dealer::NodeId::Slack(rho_dealer::SlackUnit {
            thread: None,
            ..unit.clone()
        });
        let until = rho_dealer::DateMark::day(today + chrono::Duration::days(days));
        if !self.submit_verdict(
            Some(room),
            crate::attention::Verdict::Snooze(until),
            rho_journal::DealerVerdict::Defer,
            rho_journal::PhoneVerdict::Defer,
            format!("snooze {days}d"),
            window,
            cx,
        ) {
            self.echo(
                "room snooze: the room is unavailable",
                StyleClass::SystemInfo,
                cx,
            );
        }
    }

    pub(crate) fn deal_snooze(
        &mut self,
        unit: SnoozeUnit,
        count: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = count.unwrap_or(1).max(1) as i64;
        let (until, said) = snooze_target(unit, count, chrono::Local::now());
        self.deal_snooze_until(until, said, window, cx);
    }

    /// A snooze that already knows its time: the phone's chips, which name
    /// an hour of the day rather than a distance from now.
    pub(crate) fn deal_snooze_at(
        &mut self,
        at: chrono::DateTime<chrono::Local>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let until = rho_dealer::DateMark::at(at.timestamp_millis());
        self.deal_snooze_until(until, snooze_said(at, chrono::Local::now()), window, cx);
    }

    fn deal_snooze_until(
        &mut self,
        until: rho_dealer::DateMark,
        said: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.deal_card_is_target(cx) {
            self.echo("snooze: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        }
        if !self.submit_verdict(
            None,
            crate::attention::Verdict::Snooze(until),
            rho_journal::DealerVerdict::Defer,
            rho_journal::PhoneVerdict::Defer,
            said,
            window,
            cx,
        ) {
            self.echo("snooze: nothing to snooze", StyleClass::SystemInfo, cx);
        }
    }

    pub(crate) fn cmd_project_add(
        &mut self,
        path: String,
        name: Option<String>,
        description: String,
        cx: &mut Context<Self>,
    ) {
        if !self.require_connected(cx) {
            return;
        }
        // A project is a label carrying the URL the agent host clones. A path
        // would have the agent host read the user's checkout, which it no
        // longer does.
        if !rho_agents_client::create::is_repository_url(&path) {
            let message = format!("a project is a repository URL, not a path: `{path}`");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            return;
        }
        let workdir = match rho_agents_client::create::resolve_workdir(&self.hosts, &path) {
            Ok(workdir) => workdir,
            Err(message) => {
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                return;
            }
        };
        // The name the user gave is the label's path, else the
        // repository's own name; the description was the agent host's and has
        // no fact to live in.
        let _ = description;
        let path_name = name.unwrap_or_else(|| {
            workdir
                .path
                .file_name()
                .map(|name| name.strip_suffix(".git").unwrap_or(name).to_owned())
                .unwrap_or_else(|| workdir.path.to_string())
        });
        let Some(label) = self.mint_label(&path_name, cx) else {
            return;
        };
        let node = rho_dealer::NodeId::Label(label);
        self.write_marks(
            vec![rho_dealer::marks::repository(
                &node,
                Some(workdir.path.to_string()),
            )],
            cx,
        );
    }

    pub(crate) fn cmd_project_remove(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        match self.hosts.registered_workdir(&path) {
            Some(workdir) => {
                let Some(name) = self
                    .hosts
                    .workdirs()
                    .iter()
                    .find(|candidate| {
                        candidate.host == workdir.host && candidate.path == workdir.path
                    })
                    .map(|candidate| candidate.name.clone())
                else {
                    return;
                };
                let Some(label) = self.attention.marks.label_at(&name) else {
                    return;
                };
                let node = rho_dealer::NodeId::Label(label);
                self.write_marks(vec![rho_dealer::marks::repository(&node, None)], cx);
            }
            None => {
                let message = format!("no registered project `{path}`");
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            }
        }
    }

    pub(crate) fn cmd_open(&mut self, path: Utf8PathBuf, window: &Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.subject_agent_or_notice("open", window, cx) else {
            return;
        };
        if !self.require_agent_online(agent_id, cx) {
            return;
        }
        let Some(workspace) = self.registry.agent_workspace(agent_id) else {
            self.notice_on(
                None,
                "open: agent has no workspace",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.open_file_surface(agent_id, workspace, path, cx);
    }

    pub(crate) fn cmd_shell(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("shell", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.open_shell_surface(agent_id, cx);
        }
    }

    pub(crate) fn cmd_shell_close(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.subject_agent_or_notice("close shell", window, cx) else {
            return;
        };
        if !self.require_agent_online(agent_id, cx) {
            return;
        }
        let Some(link) = self.link_for(agent_id) else {
            return;
        };
        let task = rho_shell_view::channel::close(&link, agent_id.encoded());
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.notice_on(Some(&agent_id), "shell closed", StyleClass::SystemInfo, cx)
                }
                Err(error) => this.notice_on(
                    Some(&agent_id),
                    &format!("close shell failed: {error:#}"),
                    StyleClass::SystemInfo,
                    cx,
                ),
            });
        })
        .detach();
    }

    pub(crate) fn cmd_term(&mut self, new: bool, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("term", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.open_terminal_surface(agent_id, new, cx);
        }
    }

    pub fn open_browser_page(
        &mut self,
        id: rho_browser::PageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = rho_browser::open_page(id, cx) else {
            let message = format!("browser page not found: {id}");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            return;
        };
        self.scan_browser_pages_for_gc(cx);
        let view = cx.new(|cx| rho_browser::PageView::new(model, id, cx));
        let surface = Self::wrap_surface(SurfaceKey::Browser(id), SurfaceView::Browser(view));
        self.display_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Creates a browser page and opens it.
    pub(crate) fn create_browser_page(
        &mut self,
        url: String,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let _ = window;
        let create = rho_browser::create_page(url, cx);
        cx.spawn(async move |this, cx| {
            let record = create.await;
            let _ = this.update_in(cx, |this, _, cx| match record {
                Ok(record) => rho_journal::record(rho_journal::Event::Created {
                    node_id: rho_journal::NodeIdentity::Page {
                        uuid: *record.id.0.as_bytes(),
                    },
                    kind: rho_journal::CreatedKind::Page,
                    method: rho_journal::CreateMethod::TabBirth,
                    at_root: true,
                }),
                Err(error) => {
                    tracing::error!(%error, "browser page creation failed");
                    let message = format!("browser: {error:#}");
                    this.notice_on(None, &message, StyleClass::SystemInfo, cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn cmd_version(&mut self, cx: &mut Context<Self>) {
        self.notice_on(None, env!("CARGO_PKG_VERSION"), StyleClass::SystemInfo, cx);
    }

    pub(crate) fn cmd_upload_gui_telemetry(&mut self, cx: &mut Context<Self>) {
        let host = match self.selection.selected_agent() {
            Some(agent_id) => self.host_of(agent_id),
            None => self.hosts.primary(),
        };
        let Some(host) = host.filter(|host| self.hosts.is_online(*host)) else {
            self.notice_on(
                None,
                "performance snapshot: no agent host is connected",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let snapshot = match crate::telemetry::snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.notice_on(
                    None,
                    &format!("performance snapshot failed: {error:#}"),
                    StyleClass::StatusError,
                    cx,
                );
                return;
            }
        };
        let Some(connection) = self.hosts.connection(host) else {
            return;
        };
        let task = connection.upload_gui_telemetry(snapshot);
        self.notice_on(
            None,
            "uploading GUI performance snapshot…",
            StyleClass::SystemInfo,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(path) => this.notice_on(
                    None,
                    &format!("GUI performance snapshot stored at {path}"),
                    StyleClass::SystemInfo,
                    cx,
                ),
                Err(error) => this.notice_on(
                    None,
                    &format!("performance snapshot upload failed: {error:#}"),
                    StyleClass::StatusError,
                    cx,
                ),
            });
        })
        .detach();
    }

    /// The attached agent hosts and how each is doing, as one notice line.
    pub(crate) fn cmd_hosts(&mut self, cx: &mut Context<Self>) {
        let listing = self
            .hosts
            .iter()
            .map(|host| {
                format!(
                    "{} {} · {}",
                    host.name,
                    host.status.label(),
                    host.target.describe()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.notice_on(None, &listing, StyleClass::SystemInfo, cx);
    }

    /// Attaches an agent host named on the spot, for a machine that is not
    /// worth putting in the host list.
    pub(crate) fn cmd_host_attach(&mut self, spec: &str, cx: &mut Context<Self>) {
        let spec = match HostSpec::parse(spec, "rho") {
            Ok(spec) => spec,
            Err(error) => {
                return self.notice_on(
                    None,
                    &format!("attach: {error}"),
                    StyleClass::StatusError,
                    cx,
                );
            }
        };
        if self.hosts.by_name(&spec.name).is_some() {
            let message = format!("attach: host `{}` is already attached", spec.name);
            return self.notice_on(None, &message, StyleClass::StatusError, cx);
        }
        let name = spec.name.clone();
        self.attach_host(spec, cx);
        self.notice_on(
            None,
            &format!("attaching {name}…"),
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// Detaches an agent host by name, dropping everything the client held for
    /// it.
    pub(crate) fn cmd_host_detach(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.hosts.by_name(name.trim()).map(|host| host.id) else {
            let message = format!("detach: no host named `{}`", name.trim());
            return self.notice_on(None, &message, StyleClass::StatusError, cx);
        };
        self.detach_host(host, window, cx);
        self.notice_on(
            None,
            &format!("detached {}", name.trim()),
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// Prompt for `<name>=unix:<socket>` or `<name>=iroh:<id>@<ssh-dest>`.
    pub(crate) fn prompt_host_attach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &gpui::App| Vec::new());
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                if !input.trim().is_empty() {
                    workspace.cmd_host_attach(&input, cx);
                }
            },
        );
        self.open_prompt("attach host:", complete, on_submit, window, cx);
    }

    /// Prompt (completing over attached hosts) for one to detach.
    pub(crate) fn prompt_host_detach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_lowercase();
            workspace
                .hosts
                .iter()
                .filter(|host| host.name.to_lowercase().contains(&needle))
                .map(|host| crate::commands::Candidate {
                    value: host.name.clone(),
                    description: host.target.describe(),
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                if !input.trim().is_empty() {
                    workspace.cmd_host_detach(&input, window, cx);
                }
            },
        );
        self.open_prompt("detach host:", complete, on_submit, window, cx);
    }

    pub(crate) fn open_host_auth_transient(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.hosts.len() {
            0 => self.notice_on(None, "no attached hosts", StyleClass::SystemInfo, cx),
            1 => {
                let host = self.hosts.iter().next().expect("one host").id;
                self.prompt_host_auth_namespace(host, window, cx);
            }
            _ => self.prompt_host_auth(window, cx),
        }
    }

    fn prompt_host_auth(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_lowercase();
            workspace
                .hosts
                .iter()
                .filter(|host| host.name.to_lowercase().contains(&needle))
                .map(|host| crate::commands::Candidate {
                    value: host.name.clone(),
                    description: host.status.label(),
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let name = input.trim();
                if let Some(host) = workspace.hosts.by_name(name).map(|host| host.id) {
                    workspace.prompt_host_auth_namespace(host, window, cx);
                } else if !name.is_empty() {
                    workspace.notice_on(
                        None,
                        &format!("no attached host named `{name}`"),
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            },
        );
        self.open_prompt("host auth:", complete, on_submit, window, cx);
    }

    pub(crate) fn prompt_host_auth_namespace(
        &mut self,
        host: HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let complete =
            std::rc::Rc::new(move |workspace: &Workspace, input: &str, _: &gpui::App| {
                let needle = input.trim().to_lowercase();
                workspace
                    .quotas
                    .auth(host)
                    .into_iter()
                    .flat_map(|auth| &auth.namespaces)
                    .filter(|name| name.to_lowercase().contains(&needle))
                    .map(|name| {
                        let disabled = workspace
                            .quotas
                            .auth(host)
                            .is_some_and(|auth| auth.disabled_namespaces.contains(name));
                        crate::commands::Candidate {
                            value: name.clone(),
                            description: if disabled {
                                "disabled account"
                            } else {
                                "enabled account"
                            }
                            .to_owned(),
                        }
                    })
                    .filter(|candidate| candidate.value.to_lowercase().contains(&needle))
                    .collect()
            });
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  _window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let name = input.trim();
                if name.is_empty() {
                    return;
                }
                let enabled = workspace
                    .quotas
                    .auth(host)
                    .is_some_and(|auth| auth.disabled_namespaces.iter().any(|item| item == name));
                workspace.call(
                    host,
                    agents::SetAuthAccountEnabled {
                        name: name.to_owned(),
                        enabled,
                    },
                    cx,
                    |_, (), _| {},
                );
                workspace.notice_on(
                    None,
                    &format!(
                        "{} account {name} on {}",
                        if enabled { "enabling" } else { "disabling" },
                        workspace.hosts.host_label(host)
                    ),
                    StyleClass::SystemInfo,
                    cx,
                );
            },
        );
        self.open_prompt(
            format!("auth on {}:", self.hosts.host_label(host)),
            complete,
            on_submit,
            window,
            cx,
        );
    }

    /// Opens the draft compose view. `working_directory` is an explicit
    /// choice (`:agent new <path>`, rewrites the header even mid-draft);
    /// otherwise the scaffold default is derived from the inherited topic.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn enter_draft(
        &mut self,
        working_directory: Option<Utf8PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match working_directory {
            Some(argument) => {
                let workdir = match rho_agents_client::create::resolve_workdir(
                    &self.hosts,
                    argument.as_str(),
                ) {
                    Ok(workdir) => workdir,
                    Err(message) => {
                        self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                        return;
                    }
                };
                let label = self.hosts.workdir_label(&workdir);
                let editor = self.focused_draft_editor();
                self.draft_model.update(cx, |view, cx| {
                    view.seed(&label, true, editor.as_ref(), window, cx)
                });
            }
            None => self.seed_draft(false, window, cx),
        }
        self.select_agent(None, window, cx);
    }

    pub(crate) fn mark_draft_active_from_edit(&mut self, cx: &mut Context<Self>) {
        if matches!(self.selection.active_pane(), ActivePane::Startup) {
            self.selection.enter_draft();
            cx.notify();
        }
    }

    /// Holds an agent whole from here on: its transcript is being looked
    /// at, or about to be. Whoever this pushes past the bound is let go,
    /// unless they are on screen.
    fn activate_agent(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        let joined = self.active.touch(agent_id);
        let shown = match self
            .history
            .as_ref()
            .map(|history| &history.current().surface.key)
        {
            Some(SurfaceKey::Transcript(agent_id)) => HashSet::from([*agent_id]),
            _ => HashSet::new(),
        };
        let evicted = self.active.evict(|agent_id| shown.contains(&agent_id));
        for agent_id in &evicted {
            self.release_agent(*agent_id, cx);
        }
        if joined || !evicted.is_empty() {
            self.send_agent_focus();
        }
    }

    /// Tells every host which of its agents this client wants live frames
    /// for: the active set, replaced whole. Nothing durable travels on it,
    /// so a set that lags by one frame costs nothing.
    fn send_agent_focus(&mut self) {
        for host in self.hosts.ids() {
            self.send_agent_focus_to(host);
        }
    }

    fn send_agent_focus_to(&mut self, host: HostId) {
        let agent_ids = self
            .active
            .iter()
            .filter(|agent_id| self.registry.host_of_agent(*agent_id) == Some(host))
            .collect();
        self.agents_client.focus(host, agent_ids);
    }

    /// Lets go of everything held for an agent that left the active set:
    /// its events, its transcript, its live tail, and the view unless a
    /// pane still shows it. The digest stays; the rails read that.
    fn release_agent(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        self.transcripts.forget(agent_id);
        self.note_followed();
        self.pending_syncs.remove(&agent_id);
        self.registry.mark_not_live(agent_id);
        if let Some(model) = self.models.get(&agent_id).cloned() {
            model.update(cx, |model, _| model.clear_preview_editor());
        }

        // Not a death: an evicted agent still exists, and a transcript of
        // it that is still in someone's history is rebuilt on the way back
        // (`warm_surface`). What leaves history is what has gone, not what
        // has been let go of.
        let shown = self.history.as_ref().is_some_and(|history| {
            history.current().surface.key == SurfaceKey::Transcript(agent_id)
        });
        if shown {
            return;
        }

        for surfaces in self.surfaces.values_mut() {
            surfaces.retain(|surface| surface.key != SurfaceKey::Transcript(agent_id));
        }
        self.phone.remove_key(&SurfaceKey::Transcript(agent_id));
        self.models.remove(&agent_id);
    }

    pub fn open_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        rho_journal::record(rho_journal::Event::AgentOpened {
            agent_id: agent_id.into(),
        });
        self.activate_agent(agent_id, cx);
        self.select_agent(Some(agent_id), window, cx);
    }

    /// What every command acts on. Which one is asking has to be read from
    /// focus, not from whether a tab happens to be open: triaging the rail
    /// with a conversation open is the normal way to work, and taking the
    /// tab's agent there would act on something the user is not looking at.
    ///
    /// The map used to answer this from the row under its cursor, and rows
    /// that named nobody fell through to the open agent. With the map gone
    /// there is no row, so the fall-through is the whole answer.
    pub(crate) fn subject(&self, _window: &Window, _cx: &mut Context<Self>) -> Subject {
        self.selection
            .selected_agent()
            .map_or_else(Subject::default, |agent_id| Subject {
                agent: Some(agent_id),
                agents: self.registry.agent_subtree(agent_id),
            })
    }

    /// The subject's agent, or a `{verb}: no agent in focus` notice.
    fn subject_agent_or_notice(
        &mut self,
        verb: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AgentId> {
        let agent = self.subject(window, cx).agent;
        if agent.is_none() {
            let message = format!("{verb}: no agent in focus");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
        }
        agent
    }

    /// Tab in the draft cycles the `Workdir:` field, the start field, and
    /// the body. On agent views it does nothing, and says so, so that the
    /// key can be the verdicts there.
    fn cycle_draft_field(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.selection.selected_agent().is_none()
            && let Some(editor) = self.focused_draft_editor()
        {
            self.draft_model
                .update(cx, |view, cx| view.toggle_field(&editor, window, cx));
            return true;
        }
        false
    }

    /// Shift-Tab in the draft walks the rows the other way round, the way
    /// it does in every form. It used to cycle the value under the cursor
    /// instead, so from a field the only way back was forwards through all
    /// of them; the values have `ctrl-tab` now. On agent views it does
    /// nothing.
    fn cycle_draft_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.selected_agent().is_none()
            && let Some(editor) = self.focused_draft_editor()
        {
            self.draft_model
                .update(cx, |view, cx| view.toggle_field_back(&editor, window, cx));
        }
    }

    /// Ctrl-Tab cycles the value the cursor is on: the role, the start
    /// field's mode (on top of → join), or the filesystem (view →
    /// exposed). Elsewhere in the draft it does nothing, there being no
    /// value to cycle.
    fn cycle_draft_value(&mut self, cx: &mut Context<Self>) {
        if self.selection.selected_agent().is_none()
            && let Some(editor) = self.focused_draft_editor()
        {
            self.draft_model.update(cx, |view, cx| {
                if view.cursor_in_role_field(&editor, cx) {
                    let next = cycle_agent_role_text(&view.role_text(cx));
                    view.set_role_text(next, cx);
                } else if view.cursor_in_start_field(&editor, cx) {
                    view.cycle_start_mode(cx);
                } else if view.cursor_in_filesystem_field(&editor, cx) {
                    let next = cycle_workset_mode_text(&view.filesystem_text(cx));
                    view.set_filesystem_text(next, cx);
                }
            });
        }
    }

    /// (Re)writes the draft scaffold with the derived default workdir; the
    /// field stays empty when nothing host-side suggests one.
    fn seed_draft(&mut self, force_header: bool, window: &mut Window, cx: &mut Context<Self>) {
        let label = self
            .draft_default_workdir()
            .map(|path| self.hosts.workdir_label(&path))
            .unwrap_or_default();
        let editor = self.focused_draft_editor();
        self.draft_model.update(cx, |view, cx| {
            view.seed(&label, force_header, editor.as_ref(), window, cx)
        });
    }

    /// Where a new agent works when the draft doesn't say: the selected
    /// agent sets the precedent, else the first registered workdir.
    fn draft_default_workdir(&self) -> Option<HostPath> {
        self.selection
            .selected_agent()
            .and_then(|agent_id| self.agent_workdir(agent_id))
            .or_else(|| {
                self.hosts.workdirs().first().map(|workdir| HostPath {
                    host: workdir.host,
                    path: workdir.path.clone(),
                })
            })
    }

    /// What an agent's workset was cloned from, as a host-qualified workdir:
    /// what a new sibling agent should inherit.
    fn agent_workdir(&self, agent_id: AgentId) -> Option<HostPath> {
        Some(HostPath {
            host: self.host_of(agent_id)?,
            path: self.registry.agent_origin(agent_id)?,
        })
    }

    /// Records a notice outside the conversation and flashes it in the echo
    /// area.
    pub(crate) fn notice_on(
        &mut self,
        agent_id: Option<&AgentId>,
        text: &str,
        class: StyleClass,
        cx: &mut Context<Self>,
    ) {
        let logged = match agent_id {
            Some(agent_id) => format!("{}: {text}", self.registry.agent_display_label(*agent_id)),
            None => text.to_owned(),
        };
        self.append_message(logged, class, cx);
        self.show_echo(text, class, cx);
    }

    /// Records and shows a message in the echo area.
    pub(crate) fn echo(&mut self, text: &str, class: StyleClass, cx: &mut Context<Self>) {
        self.append_message(text.to_owned(), class, cx);
        self.show_echo(text, class, cx);
    }

    pub(crate) fn append_message(
        &mut self,
        text: String,
        class: StyleClass,
        cx: &mut Context<Self>,
    ) {
        self.messages
            .update(cx, |messages, cx| messages.append(text, class, cx));
        cx.notify();
    }

    /// Replacing a message cancels its predecessor's dismiss timer.
    fn show_echo(&mut self, text: &str, class: StyleClass, cx: &mut Context<Self>) {
        let dismiss = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ECHO_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                this.echo = None;
                cx.notify();
            });
        });
        self.echo = Some(Echo::new(text, class, dismiss));
        cx.notify();
    }

    pub(crate) fn cmd_messages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let surface = self.make_surface(SurfaceKey::Messages, window, cx);
        self.display_surface_with_method(surface, rho_journal::SurfaceShowMethod::Command, cx);
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn message_log_texts(&self, cx: &App) -> Vec<String> {
        self.messages
            .read(cx)
            .texts()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn append_test_log_entry(&mut self, text: String, cx: &mut Context<Self>) {
        self.messages
            .update(cx, |messages, _| messages.append_unrendered(text));
    }

    #[cfg(test)]
    pub(crate) fn seed_messages_for_test(
        &mut self,
        entries: impl IntoIterator<Item = (StyleClass, String)>,
        cx: &mut Context<Self>,
    ) {
        self.messages
            .update(cx, |messages, cx| messages.seed(entries, cx));
    }

    #[cfg(test)]
    pub(crate) fn append_test_message(
        &mut self,
        text: String,
        class: StyleClass,
        cx: &mut Context<Self>,
    ) {
        self.append_message(text, class, cx);
    }

    #[cfg(test)]
    pub(crate) fn messages_buffer_id(&self, cx: &App) -> gpui::EntityId {
        self.messages.read(cx).buffer_id()
    }

    #[cfg(test)]
    pub(crate) fn notice_for_test(
        &mut self,
        agent_id: Option<&AgentId>,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        self.notice_on(agent_id, text, StyleClass::SystemInfo, cx);
    }

    #[cfg(test)]
    pub(crate) fn agent_model_for_test(&self, agent_id: AgentId) -> Entity<AgentModel> {
        self.models
            .get(&agent_id)
            .cloned()
            .expect("an agent model for this agent")
    }

    #[cfg(test)]
    pub(crate) fn echo_text_for_test(&self) -> Option<&str> {
        self.echo.as_ref().map(|echo| echo.text())
    }

    #[cfg(test)]
    pub(crate) fn messages_following(&self, cx: &App) -> bool {
        self.messages.read(cx).following(cx)
    }

    pub fn select_agent(
        &mut self,
        agent_id: Option<AgentId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_agent_inner(agent_id, true, window, cx);
    }

    fn select_agent_inner(
        &mut self,
        agent_id: Option<AgentId>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        rho_journal::record(rho_journal::Event::AgentSelected {
            agent_id: agent_id.map(|id| id.encoded()),
        });
        // Any other route to the draft page composes at the root; only
        // `n a` sets an area, and it sets it after this.
        self.draft_area = None;
        if let Some(agent_id) = agent_id {
            // Selection is the strongest signal: the transcript about to be
            // shown is the newest of the active set.
            self.activate_agent(agent_id, cx);
        }
        if let Some(agent_id) = &agent_id {
            let view = self.materialize_model(agent_id, window, cx);
            view.update(cx, |view, cx| view.tick_timers(now_ms(), cx));
        }
        let (context, key) = match agent_id {
            Some(agent_id) => {
                self.selection.select_agent(agent_id);
                (
                    self.context_for_agent(agent_id),
                    SurfaceKey::Transcript(agent_id),
                )
            }
            None => {
                self.selection.enter_draft();
                (ContextId::Draft, SurfaceKey::Draft)
            }
        };
        self.active_context = context;
        let surface = self.make_surface(key, window, cx);
        self.display_surface(surface, cx);
        if focus {
            self.focus_active_surface(window, cx);
        }
        self.ensure_duration_timer(cx);
        cx.notify();
    }

    /// The active context's surface with the given key, whether or not
    /// the viewport currently displays it.
    pub(crate) fn find_surface(&self, pred: impl Fn(&Surface) -> bool) -> Option<&Surface> {
        self.surfaces
            .get(&self.active_context)?
            .iter()
            .find(|surface| pred(surface))
    }

    /// Human name of a surface, as `:buffer`/`:close` address it.
    fn surface_name(&self, key: &SurfaceKey) -> String {
        match key {
            SurfaceKey::Draft => "draft".to_owned(),
            SurfaceKey::Home => "home".to_owned(),
            SurfaceKey::Messages => "messages".to_owned(),
            SurfaceKey::Usage => "usage".to_owned(),
            SurfaceKey::Note(_) => "note".to_owned(),
            SurfaceKey::Transcript(agent_id) => self
                .registry
                .agent_name_with_labels(*agent_id, self.registry.agent_display_label(*agent_id)),
            SurfaceKey::File { path, .. } => path.to_string(),
            SurfaceKey::Shell(agent_id) => {
                format!("shell {}", self.registry.agent_display_label(*agent_id))
            }
            SurfaceKey::Terminal {
                agent_id,
                terminal_id,
            } => format!(
                "term {}/{terminal_id}",
                self.registry.agent_display_label(*agent_id)
            ),
            SurfaceKey::Browser(browser) => browser.to_string(),
            SurfaceKey::SlackList => "slack".to_owned(),
            SurfaceKey::SlackResults { query, kind } => match kind {
                rho_slack::session::SearchKind::Messages => query.clone(),
                rho_slack::session::SearchKind::Files => format!("files {query}"),
            },
            SurfaceKey::SlackInventory(kind) => kind.title().to_owned(),
            SurfaceKey::SlackConversation(source) => self
                .slack_labels
                .get(source)
                .cloned()
                .unwrap_or_else(|| "slack".to_owned()),
            SurfaceKey::Image { title, .. } => title.clone(),
        }
    }

    fn surface_kind(key: &SurfaceKey) -> &'static str {
        match key {
            SurfaceKey::Draft => "compose",
            SurfaceKey::Home => "home",
            SurfaceKey::Messages => "messages",
            SurfaceKey::Usage => "usage",
            SurfaceKey::Note(_) => "note",
            SurfaceKey::Transcript(_) => "transcript",
            SurfaceKey::File { .. } => "file",
            SurfaceKey::Shell(_) => "shell",
            SurfaceKey::Terminal { .. } => "terminal",
            SurfaceKey::Browser(_) => "browser",
            SurfaceKey::SlackList => "slack list",
            SurfaceKey::SlackResults { .. } => "slack search",
            SurfaceKey::SlackInventory(_) => "slack inventory",
            SurfaceKey::SlackConversation(_) => "slack",
            SurfaceKey::Image { .. } => "image",
        }
    }

    /// The active context's surfaces as `(name, kind)` for completion.
    pub fn buffer_table(&self) -> Vec<(String, String)> {
        let list = self.surfaces.get(&self.active_context);
        let mut rows = list
            .map(|list| {
                list.iter()
                    .map(|surface| {
                        (
                            self.surface_name(&surface.key),
                            Self::surface_kind(&surface.key).to_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // Home is every context's, shown in it or not: a reader inside
        // Slack is in a context whose list has never held Home, and the
        // picker is the way back from any surface to it.
        if !list.is_some_and(|list| list.iter().any(|surface| surface.key == SurfaceKey::Home)) {
            rows.insert(
                0,
                (
                    self.surface_name(&SurfaceKey::Home),
                    Self::surface_kind(&SurfaceKey::Home).to_owned(),
                ),
            );
        }
        rows
    }

    /// Resolves a `:buffer`/`:close` argument: exact name first, then a
    /// unique case-insensitive substring match.
    fn surface_named(&self, name: &str) -> Option<&Surface> {
        let list = self.surfaces.get(&self.active_context)?;
        if let Some(surface) = list
            .iter()
            .find(|surface| self.surface_name(&surface.key) == name)
        {
            return Some(surface);
        }
        let needle = name.to_lowercase();
        let mut matches = list.iter().filter(|surface| {
            self.surface_name(&surface.key)
                .to_lowercase()
                .contains(&needle)
        });
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    /// Shows the named surface in the context's viewport.
    pub(crate) fn switch_buffer(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(surface) = self.surface_named(name).cloned() else {
            // The row the picker offers from a context that has never
            // shown Home: opening it is what makes it that context's.
            if name == self.surface_name(&SurfaceKey::Home) {
                self.open_home(window, cx);
                return;
            }
            self.notice_on(
                None,
                &format!("no surface matching `{name}`"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.display_surface(surface, cx);
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    fn journal_surface(key: &SurfaceKey) -> rho_journal::SurfaceIdentity {
        use rho_journal::SurfaceIdentity;
        match key {
            SurfaceKey::Draft => SurfaceIdentity::Draft,
            SurfaceKey::Home => SurfaceIdentity::Home,
            SurfaceKey::Messages => SurfaceIdentity::Messages,
            SurfaceKey::Usage => SurfaceIdentity::Usage,
            SurfaceKey::Note(node) => SurfaceIdentity::DeskNode {
                host: 0,
                node_id: node.clone().into(),
            },
            SurfaceKey::Transcript(agent_id) => SurfaceIdentity::Transcript {
                agent_id: agent_id.into(),
            },
            SurfaceKey::File { agent_id, path } => SurfaceIdentity::File {
                agent_id: agent_id.into(),
                path: path.to_string(),
            },
            SurfaceKey::Shell(agent_id) => SurfaceIdentity::Shell {
                agent_id: agent_id.into(),
            },
            SurfaceKey::Terminal {
                agent_id,
                terminal_id,
            } => SurfaceIdentity::Terminal {
                agent_id: agent_id.into(),
                terminal_id: *terminal_id,
            },
            SurfaceKey::Browser(page_id) => SurfaceIdentity::Browser {
                page_id: page_id.to_string(),
            },
            SurfaceKey::SlackList => SurfaceIdentity::SlackList,
            SurfaceKey::SlackResults { query, kind } => SurfaceIdentity::SlackSearch {
                query: match kind {
                    rho_slack::session::SearchKind::Messages => query.clone(),
                    rho_slack::session::SearchKind::Files => format!("files:{query}"),
                },
            },
            SurfaceKey::SlackInventory(kind) => SurfaceIdentity::SlackInventory {
                name: kind.title().to_owned(),
            },
            SurfaceKey::SlackConversation(source) => SurfaceIdentity::SlackConversation {
                thread: crate::slack::journal_thread(source),
            },
            SurfaceKey::Image { title, .. } => SurfaceIdentity::Image {
                title: title.clone(),
            },
        }
    }

    fn journal_scroll(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            event.touch_phase,
            gpui::TouchPhase::Ended | gpui::TouchPhase::Cancelled
        ) {
            self.deal_gesture_active = false;
        } else if event.delta.precise() && !self.deal_gesture_active {
            let delta = event.delta.pixel_delta(px(20.));
            if delta.y > px(12.) && delta.y.abs() > delta.x.abs() {
                self.deal_gesture_active = true;
                window.dispatch_action(Box::new(DealOpen), cx);
            }
        }
        self.journal_scroll_burst(window, cx);
    }

    fn journal_linux_scroll(
        &mut self,
        _: &gpui::LinuxPointerAxisEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.journal_scroll_burst(window, cx);
    }

    fn journal_scroll_burst(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane();
        let position = match &pane.current().surface.view {
            SurfaceView::Home(view) => {
                let editor = view.read(cx).editor().clone();
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
            }
            SurfaceView::Usage(view) => {
                let editor = view.read(cx).editor().clone();
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
            }
            SurfaceView::Draft { editor, .. }
            | SurfaceView::Messages(editor)
            | SurfaceView::Note(editor)
            | SurfaceView::Transcript { editor, .. }
            | SurfaceView::Shell { editor, .. } => {
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
            }
            SurfaceView::File(view) => {
                let editor = view.read(cx).editor().clone();
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
            }
            SurfaceView::Terminal(view) => view.read(cx).scroll_offset() as i64,
            SurfaceView::Browser(_) => 0,
            SurfaceView::SlackList(view) => {
                let editor = view.read(cx).editor().clone();
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
            }
            SurfaceView::SlackResults(view) => {
                let editor = view.read(cx).editor().clone();
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
            }
            SurfaceView::SlackConversation(view) => {
                let editor = view.read(cx).editor().clone();
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
            }
            SurfaceView::Image(_) => 0,
        };
        let (surface, rough_position) =
            (Self::journal_surface(&pane.current().surface.key), position);
        self.scroll_journal_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;
            rho_journal::record(rho_journal::Event::Scroll {
                surface,
                rough_position,
            });
        }));
    }

    /// Emacs `display-buffer`: the one place surface display happens. The
    /// surface joins the context's surface list first, so it stays alive
    /// while hidden. The context's single viewport shows it, and is founded
    /// on the context's first visit.
    /// A completing-read picker over the context's surface list, emacs
    /// `C-x b`.
    pub(crate) fn open_buffer_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, _cx: &gpui::App| {
            let needle = input.trim().to_lowercase();
            workspace
                .buffer_table()
                .into_iter()
                .filter(|(name, _)| name.to_lowercase().contains(&needle))
                .map(|(name, kind)| crate::commands::Candidate {
                    value: name,
                    description: kind,
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let input = input.trim();
                if !input.is_empty() {
                    workspace.switch_buffer(input, window, cx);
                }
            },
        );
        self.open_prompt("buffer:", complete, on_submit, window, cx);
    }

    pub(crate) fn display_surface(&mut self, surface: Surface, cx: &mut Context<Self>) {
        self.display_surface_with_method(surface, rho_journal::SurfaceShowMethod::Open, cx);
    }

    pub(crate) fn display_surface_with_method(
        &mut self,
        surface: Surface,
        method: rho_journal::SurfaceShowMethod,
        cx: &mut Context<Self>,
    ) {
        self.ensure_surface_subscription(&surface.key, cx);
        let list = self.surfaces.entry(self.active_context).or_default();
        match list.iter_mut().find(|s| **s == surface) {
            Some(existing) => *existing = surface.clone(),
            None => list.push(surface.clone()),
        }
        // Home is not a card on the phone: it is what the feed shows when
        // there is nothing to deal, so it never joins the stack.
        if self.phone.enabled && surface.key != SurfaceKey::Home {
            if method == rho_journal::SurfaceShowMethod::Deal {
                self.phone
                    .show_feed(self.active_context, surface.key.clone());
            } else {
                self.phone.show(self.active_context, surface.key.clone());
            }
        }
        // The one push. Where the reader was goes on the stack and where
        // they are now is the current surface; how they got here is a
        // journal fact and not a different kind of history.
        let warm = WarmSurface {
            context: self.active_context,
            surface: surface.clone(),
        };
        let shown = match self.history.as_mut() {
            None => {
                self.history = Some(SurfaceHistory::new(surface.key.clone(), warm));
                surface
            }
            Some(history) => {
                history.show(surface.key.clone(), warm);
                history.current().surface.clone()
            }
        };
        if let Some(method) = match method {
            rho_journal::SurfaceShowMethod::Deal => Some(rho_journal::HistoryAppendMethod::Deal),
            rho_journal::SurfaceShowMethod::Overview => {
                Some(rho_journal::HistoryAppendMethod::Overview)
            }
            rho_journal::SurfaceShowMethod::Command => {
                Some(rho_journal::HistoryAppendMethod::Command)
            }
            rho_journal::SurfaceShowMethod::Open | rho_journal::SurfaceShowMethod::Mru => None,
        } {
            rho_journal::record(rho_journal::Event::HistoryAppended {
                identity: Self::journal_surface(&shown.key),
                method,
            });
        }
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: Self::journal_surface(&shown.key),
            method,
        });
        self.rebuild_home_when_shown(&shown.key, cx);
    }

    /// Put a surface at the head of history in a named context, without any
    /// of the focus and subscription work `display_surface` does: the phone
    /// walks its own stack and has already done the rest.
    pub(crate) fn show_history_surface(&mut self, context: ContextId, surface: Surface) {
        let warm = WarmSurface {
            context,
            surface: surface.clone(),
        };
        match self.history.as_mut() {
            None => self.history = Some(SurfaceHistory::new(surface.key, warm)),
            Some(history) => history.show(surface.key, warm),
        }
    }

    fn ensure_surface_subscription(&mut self, key: &SurfaceKey, cx: &mut Context<Self>) {
        let agent_id = match key {
            SurfaceKey::Transcript(agent_id) | SurfaceKey::File { agent_id, .. } => Some(*agent_id),
            _ => None,
        };
        if let Some(agent_id) = agent_id {
            self.activate_agent(agent_id, cx);
        }
    }

    /// `:open`: reuses the agent workspace's remote buffer registry and shows
    /// the file surface in the context's viewport.
    fn open_file_surface(
        &mut self,
        agent_id: AgentId,
        workspace: rho_agent_types::WorkspaceInfo,
        path: Utf8PathBuf,
        cx: &mut Context<Self>,
    ) {
        let key = SurfaceKey::File {
            agent_id,
            path: path.clone(),
        };
        if let Some(surface) = self.find_surface(|s| s.key == key).cloned() {
            self.display_surface(surface, cx);
            cx.notify();
            return;
        }
        let Some(host) = self.host_of(agent_id) else {
            return;
        };
        let cached = self.cached_remote_project(host, &workspace);
        let project_task = cached.is_none().then(|| {
            let link = self.hosts.connection(host)?.link();
            Some(rho_files::open_remote_project(&link, workspace.clone(), cx))
        });
        if matches!(project_task, Some(None)) {
            return;
        }
        let project_task = project_task.flatten();
        cx.spawn(async move |this, cx| {
            let opened = match cached {
                Some(project) => Ok(project),
                None => match project_task.expect("missing project task").await {
                    Ok(project) => Ok(project),
                    Err(error) => Err(error),
                },
            };
            let result = match opened {
                Ok(project) => {
                    let Ok(project) = this.update(cx, |this, _| {
                        this.cache_remote_project(host, workspace, project)
                    }) else {
                        return;
                    };
                    rho_files::open_file_buffer(&project, path, cx)
                        .await
                        .map(|buffer| (project, buffer))
                }
                Err(error) => Err(error),
            };
            match result {
                Ok((project, buffer)) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        let view = cx.new(|cx| FileView::new(project, buffer, window, cx));
                        let surface = Self::wrap_surface(key, SurfaceView::File(view));
                        this.display_surface(surface, cx);
                        this.focus_active_surface(window, cx);
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.notice_on(
                            None,
                            &format!(":open failed: {error:#}"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                    });
                }
            }
        })
        .detach();
    }

    /// Explicitly starts the agent's editor-native shell when absent, or
    /// attaches to the existing persistent kernel.
    fn open_shell_surface(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        let key = SurfaceKey::Shell(agent_id);
        if let Some(surface) = self.find_surface(|surface| surface.key == key).cloned() {
            self.display_surface(surface, cx);
            cx.notify();
            return;
        }
        let Some(link) = self.link_for(agent_id) else {
            return;
        };
        let task = rho_shell_view::channel::open(&link, agent_id.encoded());
        cx.spawn(async move |this, cx| {
            let result = task.await;
            match result {
                Ok(channel) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        let model = cx.new(|cx| rho_shell_view::ShellModel::new(channel, cx));
                        let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
                        let surface = Self::wrap_surface(key, SurfaceView::Shell { model, editor });
                        this.display_surface(surface, cx);
                        this.focus_active_surface(window, cx);
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.notice_on(
                            None,
                            &format!("shell failed: {error:#}"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                    });
                }
            }
        })
        .detach();
    }

    fn cached_remote_project(
        &mut self,
        host: HostId,
        workspace: &rho_agent_types::WorkspaceInfo,
    ) -> Option<RemoteProject> {
        let key = (host, workspace.clone());
        let state = self.remote_projects.get(&key)?.clone();
        match state.upgrade() {
            Some(state) => Some(RemoteProject { state }),
            _ => {
                self.remote_projects.remove(&key);
                None
            }
        }
    }

    fn cache_remote_project(
        &mut self,
        host: HostId,
        workspace: rho_agent_types::WorkspaceInfo,
        opened: RemoteProject,
    ) -> RemoteProject {
        if let Some(existing) = self.cached_remote_project(host, &workspace) {
            return existing;
        }
        self.remote_projects
            .insert((host, workspace), opened.state.downgrade());
        opened
    }

    /// `:term`: dials a dedicated terminal stream for the agent (attaching
    /// its first running terminal, spawning the default one when none run,
    /// or a fresh one with `new`) and shows the terminal surface.
    fn open_terminal_surface(&mut self, agent_id: AgentId, new: bool, cx: &mut Context<Self>) {
        if !new && let Some(surface) = self
            .find_surface(
                |s| matches!(s.key, SurfaceKey::Terminal { agent_id: id, .. } if id == agent_id),
            )
            .cloned()
        {
            self.display_surface(surface, cx);
            cx.notify();
            return;
        }
        let Some(link) = self.link_for(agent_id) else {
            return;
        };
        let task = rho_terminal::channel::open(&link, agent_id.encoded(), new, 80, 24);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            match result {
                Ok(channel) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        let key = SurfaceKey::Terminal {
                            agent_id,
                            terminal_id: channel.terminal_id,
                        };
                        let model = cx.new(|cx| rho_terminal::TerminalModel::new(channel, cx));
                        let view = cx.new(|cx| rho_terminal::TerminalView::new(model, cx));
                        let surface = Self::wrap_surface(key, SurfaceView::Terminal(view));
                        this.display_surface(surface, cx);
                        this.focus_active_surface(window, cx);
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.notice_on(
                            None,
                            &format!(":term failed: {error:#}"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                    });
                }
            }
        })
        .detach();
    }

    pub fn switch_agent_by_delta(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = self
            .registry
            .next_agent(self.selection.selected_agent(), delta)
        else {
            self.notice_on(
                None,
                "agent-switch: no visible agents available",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        if self.selection.selected_agent() == Some(agent_id) {
            return;
        }
        self.select_agent(Some(agent_id), window, cx);
    }

    fn materialize_model(
        &mut self,
        agent_id: &AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<AgentModel> {
        // Every route to a transcript comes through here, so this is where
        // the story stands in until a frame arrives.
        let told = self.seed_transcript_from_mirror(*agent_id);
        let (view, _) = self.ensure_agent_model(*agent_id, window, cx);
        // Seeding the store is not showing it: a view that already exists
        // (the agent host answers for every agent on connecting, with nothing
        // loaded) renders what it last synced, which was a blank page.
        if told {
            self.sync_agent_model(*agent_id, &view, FrameSummary::everything(), false, cx);
        }
        if view.read(cx).initial_load_ready()
            && let (Some(summary), Some(state)) = (
                self.pending_syncs.remove(agent_id),
                self.transcripts.state(agent_id),
            )
        {
            view.update(cx, |view, cx| {
                view.sync(
                    state,
                    summary,
                    now_ms(),
                    &|id| self.registry.agent_display_label(id),
                    cx,
                );
            });
        }
        view
    }

    /// Recomputes the right-prompt status chips for one agent's view.
    fn refresh_view_status(
        &self,
        agent_id: &AgentId,
        view: &Entity<AgentModel>,
        cx: &mut Context<Self>,
    ) {
        let context_used = self.transcripts.context_used(agent_id);
        view.update(cx, |view, cx| {
            view.set_status("", None, None, None, context_used, cx)
        });
    }

    #[cfg(test)]
    pub(crate) fn draft_area_for_test(&self) -> Option<rho_dealer::NodeId> {
        self.draft_area.clone()
    }

    /// The title of the menu under the point, which is how a test says
    /// which menu came back when escape retraced a step.
    #[cfg(test)]
    pub(crate) fn menu_title_for_test(&self) -> Option<&str> {
        self.menu_buffer.as_ref().map(|open| open.menu.title())
    }

    /// Puts the host in this process: the streams its calls open arrive on
    /// the receiver, for the test to answer as the agent host would.
    #[cfg(test)]
    pub(crate) fn host_in_process_for_test(
        &self,
        host: HostId,
    ) -> tokio::sync::mpsc::UnboundedReceiver<rho_rpc::Stream> {
        let connection = self.hosts.connection(host).expect("the host is attached");
        connection.link().connect_in_process()
    }

    #[cfg(test)]
    pub(crate) fn merged_quota_summaries_for_test(
        &self,
    ) -> Vec<rho_agents_client::protocol::QuotaSummary> {
        self.quotas.merged_summaries(&self.hosts)
    }

    #[cfg(test)]
    pub(crate) fn configure_surface_history_for_test(
        &mut self,
        names: &[&str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A surface is named after what it is about, so the test history is
        // built from named conversations. `names` reads the way back walks:
        // the first is where the reader is, the rest are behind it, and
        // whatever the context held before is replaced rather than pushed
        // onto, so a test says the whole stack rather than its tail.
        for (position, name) in names.iter().rev().enumerate() {
            let surface = self.test_named_surface(name, cx);
            let warm = WarmSurface {
                context: self.active_context,
                surface: surface.clone(),
            };
            if position == 0 {
                self.history = Some(SurfaceHistory::new(surface.key, warm));
            } else {
                self.active_pane_mut().show(surface.key, warm);
            }
        }
        if !names.is_empty() {
            self.focus_active_surface(window, cx);
            cx.notify();
        }
    }

    #[cfg(test)]
    fn test_named_surface(&mut self, name: &str, cx: &mut Context<Self>) -> Surface {
        let editor = self.active_editor(cx);
        Self::wrap_surface(
            SurfaceKey::SlackResults {
                query: name.to_owned(),
                kind: rho_slack::session::SearchKind::Messages,
            },
            SurfaceView::Note(editor),
        )
    }

    #[cfg(test)]
    pub(crate) fn current_surface_key_for_test(&self) -> SurfaceKey {
        self.active_surface().key.clone()
    }

    #[cfg(test)]
    pub(crate) fn current_surface_name_for_test(&self) -> String {
        self.surface_name(&self.active_surface().key)
    }

    /// Which chart the usage screen is showing, and how many usage surfaces
    /// exist: picking a second chart must redraw the one screen rather than
    /// open another.
    #[cfg(test)]
    pub(crate) fn usage_chart_for_test(
        &self,
        cx: &App,
    ) -> Option<(crate::usage::Chart, usize, bool)> {
        let view = self.usage.opened_view()?.read(cx);
        let screens = self
            .surfaces
            .values()
            .flatten()
            .filter(|surface| surface.key == SurfaceKey::Usage)
            .count();
        Some((view.chart(), screens, view.has_block()))
    }

    #[cfg(test)]
    pub(crate) fn step_surface_back_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_surface_back(window, cx);
    }

    /// What back would visit, nearest first. The current surface is not in
    /// it: that is `current_surface_name_for_test`.
    #[cfg(test)]
    pub(crate) fn surface_history_for_test(&self) -> Vec<String> {
        self.active_pane()
            .keys_back()
            .map(|key| self.surface_name(key))
            .collect()
    }

    /// What down would step forward through, nearest first. Empty means the
    /// reader is at the newest entry and down deals.
    #[cfg(test)]
    pub(crate) fn surface_history_ahead_for_test(&self) -> Vec<String> {
        self.active_pane()
            .keys_forward()
            .map(|key| self.surface_name(key))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn show_current_history_for_test(
        &mut self,
        method: rho_journal::SurfaceShowMethod,
        cx: &mut Context<Self>,
    ) {
        let surface = self.active_surface().clone();
        self.display_surface_with_method(surface, method, cx);
    }

    /// Open a surface the test named earlier, the ordinary way a reader
    /// would rather than through history.
    #[cfg(test)]
    pub(crate) fn open_named_surface_for_test(&mut self, name: &str, cx: &mut Context<Self>) {
        let surface = self.test_named_surface(name, cx);
        self.display_surface_with_method(surface, rho_journal::SurfaceShowMethod::Open, cx);
    }

    #[cfg(test)]
    pub(crate) fn current_deal_card_for_test(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(rho_dealer::NodeId, rho_dealer::CardKind)> {
        self.open_card_in_view(cx)
            .map(|card| (card.node, card.kind))
    }

    /// The state a cold start used to leave behind before Home took the
    /// front door: a seeded prompt with the desk map drawn over it. Tests
    /// about the map, the prompt, or the tree still start there.
    #[cfg(test)]
    pub(crate) fn open_startup_overview_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = self.make_surface(SurfaceKey::Draft, window, cx);
        self.display_surface(draft, cx);
        self.seed_draft(false, window, cx);
        self.focus_active_surface(window, cx);
    }

    /// A transcript handed in whole, for a test that drives the view
    /// without a mirror to fold. Not an event: `rho-agent-hosts` carries what a
    /// agent host said, and no agent host says this.
    #[cfg(any(test, feature = "walk-support"))]
    pub(crate) fn seed_transcript_for_test(
        &mut self,
        agent_id: AgentId,
        state: rho_agents_client::state::UiAgentState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_frame_batch(vec![(agent_id, TranscriptFrame::Fold(state))], window, cx);
    }

    fn scan_browser_pages_for_gc(&mut self, cx: &mut Context<Self>) {
        let Some(list) = rho_browser::list_pages_if_running(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let pages = list.await;
            let _ = this.update(cx, |this, cx| match pages {
                Ok(pages) => {
                    for page in pages {
                        this.schedule_browser_page_gc(page.id, cx);
                    }
                }
                Err(error) => tracing::warn!(%error, "list browser pages for reconciliation"),
            });
        })
        .detach();
    }

    fn schedule_browser_page_gc(&mut self, page: rho_browser::PageId, cx: &mut Context<Self>) {
        if self.pages.is_closing(page) {
            return;
        }
        let gc = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(crate::browser::GRACE)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.pages.not_closing(page);
                tracing::info!(page_id = %page, "closing unreferenced browser page after grace period");
                if let Some(close) = rho_browser::close_page_if_running(page, cx) {
                    close.detach();
                }
            });
        });
        self.pages.closing(page, gc);
    }

    #[cfg(test)]
    /// The transcript this workspace shows for an agent, for a test that
    /// feeds it back changed.
    #[cfg(test)]
    pub(crate) fn transcript_for_test(
        &self,
        agent_id: AgentId,
    ) -> rho_agents_client::state::UiAgentState {
        self.transcripts
            .state(&agent_id)
            .cloned()
            .unwrap_or_else(|| rho_agents_client::state::UiAgentState {
                exec_timings: Default::default(),
                blocks: Vec::new(),
                status: rho_agents_client::state::UiAgentStatus::Idle,
                context_used: None,
                usage: Default::default(),
            })
    }

    /// Whether nothing has been opened yet. Only the tests ask; the
    /// startup pane is a state the shell moves out of on its own.
    #[cfg(test)]
    pub(crate) fn is_startup_pane(&self) -> bool {
        matches!(self.selection.active_pane(), ActivePane::Startup)
    }

    pub(crate) fn active_agent_model(&self) -> Option<Entity<AgentModel>> {
        self.selection
            .selected_agent()
            .and_then(|agent_id| self.models.get(&agent_id))
            .cloned()
    }

    /// The editor the user is typing into. Terminal surfaces have no editor;
    /// the draft's stands in for text-style queries.
    pub(crate) fn active_editor(&self, cx: &gpui::App) -> Entity<editor::Editor> {
        match &self.active_surface().view {
            SurfaceView::Draft { editor, .. } => editor.clone(),
            SurfaceView::Home(view) => view.read(cx).editor().clone(),
            SurfaceView::Messages(editor) => editor.clone(),
            SurfaceView::Usage(view) => view.read(cx).editor().clone(),
            SurfaceView::Note(editor) => editor.clone(),
            SurfaceView::Transcript { editor, .. } => editor.clone(),
            SurfaceView::File(view) => view.read(cx).editor().clone(),
            SurfaceView::Shell { editor, .. } => editor.clone(),
            SurfaceView::Terminal(_) => self.chrome_editor(),
            SurfaceView::Browser(_) => self.chrome_editor(),
            SurfaceView::SlackList(view) => view.read(cx).editor().clone(),
            SurfaceView::SlackResults(view) => view.read(cx).editor().clone(),
            SurfaceView::SlackConversation(view) => view.read(cx).editor().clone(),
            SurfaceView::Image(_) => self.chrome_editor(),
        }
    }

    /// The draft editor, when the active viewport shows the draft.
    fn focused_draft_editor(&self) -> Option<Entity<editor::Editor>> {
        match &self.active_surface().view {
            SurfaceView::Draft { editor, .. } => Some(editor.clone()),
            _ => None,
        }
    }

    /// An editor to answer text-style questions with while the surface in
    /// view has none of its own (a terminal, a page, an image). Any will
    /// do; the workspace keeps one for the purpose.
    fn chrome_editor(&self) -> Entity<editor::Editor> {
        self.any_draft_editor()
            .unwrap_or_else(|| self.chrome_editor.clone())
    }

    /// Some draft editor, when one is open. Used only where any editor
    /// serves, e.g. text style for chrome while a terminal is focused.
    fn any_draft_editor(&self) -> Option<Entity<editor::Editor>> {
        self.surfaces
            .get(&ContextId::Draft)?
            .iter()
            .find_map(|surface| match &surface.view {
                SurfaceView::Draft { editor, .. } => Some(editor.clone()),
                _ => None,
            })
    }

    fn active_surface_focus(&self, cx: &App) -> gpui::FocusHandle {
        if self.phone.enabled
            && matches!(self.active_surface().view, SurfaceView::Transcript { .. })
        {
            return self.phone.feed_focus.clone();
        }
        match &self.active_surface().view {
            SurfaceView::Draft { editor, .. } => editor.focus_handle(cx),
            SurfaceView::Home(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Messages(editor) => editor.focus_handle(cx),
            SurfaceView::Usage(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Note(editor) => editor.focus_handle(cx),
            SurfaceView::Transcript { editor, .. } => editor.focus_handle(cx),
            SurfaceView::File(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Shell { editor, .. } => editor.focus_handle(cx),
            SurfaceView::Terminal(view) => view.read(cx).focus_handle(cx),
            SurfaceView::Browser(view) => view.read(cx).focus_handle(cx),
            SurfaceView::SlackList(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::SlackResults(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::SlackConversation(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Image(view) => view.read(cx).focus_handle(cx),
        }
    }

    /// Moves gpui focus to the active surface. If a modal overlay
    /// owns the keyboard, update where it will return instead of stealing
    /// focus from it.
    pub(crate) fn focus_active_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let agent_id = match &self.active_surface().key {
            SurfaceKey::Transcript(agent_id)
            | SurfaceKey::Shell(agent_id)
            | SurfaceKey::File { agent_id, .. }
            | SurfaceKey::Terminal { agent_id, .. } => Some(*agent_id),
            SurfaceKey::Draft
            | SurfaceKey::Home
            | SurfaceKey::Messages
            | SurfaceKey::Usage
            | SurfaceKey::Note(_) => None,
            SurfaceKey::SlackList
            | SurfaceKey::SlackResults { .. }
            | SurfaceKey::SlackInventory(_)
            | SurfaceKey::SlackConversation(_) => None,
            SurfaceKey::Image { .. } => None,
            SurfaceKey::Browser(_) => None,
        };
        if let Some(agent_id) = agent_id {
            self.agent_last_interaction
                .insert(agent_id, now_ms() as i64);
            self.invalidate_dealer_signals(cx);
        }
        let handle = self.active_surface_focus(cx);
        if self.has_modal_overlay() {
            self.overlay_focus.set(handle);
        } else {
            window.focus(&handle, cx);
        }
    }

    /// The surface for `key`, reusing the live one (and its focus observer)
    /// when the active context already retains it.
    /// File surfaces are created asynchronously by
    /// [`Self::open_file_surface`] instead.
    pub(crate) fn make_surface(
        &mut self,
        key: SurfaceKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Surface {
        if let Some(existing) = self.find_surface(|s| s.key == key) {
            return existing.clone();
        }
        let view = match &key {
            SurfaceKey::Draft => {
                let model = self.draft_model.clone();
                let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
                SurfaceView::Draft { editor }
            }
            SurfaceKey::Home => {
                SurfaceView::Home(cx.new(|cx| crate::home::HomeView::new(window, cx)))
            }
            SurfaceKey::Messages => SurfaceView::Messages(self.messages.read(cx).editor().clone()),
            SurfaceKey::Usage => SurfaceView::Usage(self.usage.view(window, cx)),
            SurfaceKey::Note(node) => {
                let node = node.clone();
                SurfaceView::Note(self.note_view_for(&node, window, cx).editor().clone())
            }
            SurfaceKey::Transcript(agent_id) => {
                let agent_id = *agent_id;
                let model = self.materialize_model(&agent_id, window, cx);
                let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
                // `/` is the buffer's search here as everywhere; nothing
                // else in this app hosts one, so the surface does.
                self.agent_model_subscriptions.push(cx.subscribe_in(
                    &editor,
                    window,
                    |this, _, event: &editor::EditorEvent, window, cx| {
                        if let editor::EditorEvent::SearchRequested { backwards } = event {
                            this.prompt_transcript_search(
                                search::Direction::of(*backwards),
                                window,
                                cx,
                            );
                        }
                    },
                ));
                SurfaceView::Transcript { model, editor }
            }
            SurfaceKey::File { .. } => {
                unreachable!("file surfaces are created by open_file_surface")
            }
            SurfaceKey::Shell(_) => {
                unreachable!("shell surfaces are created by open_shell_surface")
            }
            SurfaceKey::Terminal { .. } => {
                unreachable!("terminal surfaces are created by open_terminal_surface")
            }
            SurfaceKey::Browser(_) => {
                unreachable!("browser surfaces are created by create_browser_page")
            }
            SurfaceKey::SlackList => SurfaceView::SlackList(
                self.slack_list_view(window, cx)
                    .expect("the slack list is only opened once a session exists"),
            ),
            SurfaceKey::SlackResults { .. } => {
                unreachable!("results surfaces are created by open_slack_results")
            }
            SurfaceKey::SlackInventory(_) => {
                unreachable!("inventory surfaces are created by open_slack_inventory")
            }
            SurfaceKey::SlackConversation(_) => {
                unreachable!("slack conversations are created by open_slack_source")
            }
            SurfaceKey::Image { .. } => {
                unreachable!("image surfaces are created by open_image")
            }
        };
        Self::wrap_surface(key, view)
    }

    pub(crate) fn wrap_surface(key: SurfaceKey, view: SurfaceView) -> Surface {
        Surface { key, view }
    }

    /// Keeps the registry's notion of "current agent" in step with the
    /// visible surface, so `:` commands resolve against what the user sees.
    fn sync_selection_to_focus(&mut self, cx: &mut Context<Self>) {
        let selected = match self.active_surface().key.clone() {
            SurfaceKey::Transcript(agent_id) | SurfaceKey::Shell(agent_id) => {
                self.selection.select_agent(agent_id);
                Some(agent_id)
            }
            SurfaceKey::Terminal { agent_id, .. } => {
                self.selection.select_agent(agent_id);
                Some(agent_id)
            }
            SurfaceKey::Browser(_) => None,
            SurfaceKey::Draft => {
                self.selection.enter_draft();
                None
            }
            // Files and chat keep whatever agent context was current.
            SurfaceKey::Home
            | SurfaceKey::Note(_)
            | SurfaceKey::Messages
            | SurfaceKey::Usage
            | SurfaceKey::File { .. } => None,
            SurfaceKey::SlackList
            | SurfaceKey::SlackResults { .. }
            | SurfaceKey::SlackInventory(_)
            | SurfaceKey::SlackConversation(_) => None,
            SurfaceKey::Image { .. } => None,
        };
        if let Some(agent_id) = selected {
            self.activate_agent(agent_id, cx);
        }
        cx.notify();
    }

    /// Recomputes candidates after an edit; subscribed by [`Minibuffer`].
    /// Recomputes the prompt's candidates after an edit, then tells the
    /// prompt what the input now is. Called once per edit and never per
    /// frame.
    ///
    /// The candidates are recomputed first and the minibuffer put back before
    /// the handler runs, so the handler sees the workspace as the reader does
    /// — including the prompt it belongs to, which it may read, replace or
    /// close.
    pub(crate) fn refresh_minibuffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut minibuffer) = self.minibuffer.take() else {
            return;
        };
        minibuffer.refresh(self, cx);
        let on_change = minibuffer.on_change();
        let input = minibuffer.input(cx);
        self.minibuffer = Some(minibuffer);
        if let Some(on_change) = on_change {
            on_change(self, &input, window, cx);
        }
        cx.notify();
    }

    fn minibuffer_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut minibuffer) = self.minibuffer.take() else {
            return;
        };
        let prompt = minibuffer.prompt().to_owned();
        self.pending_find_target = None;
        // Which row is chosen has to be read before `accept_selected`
        // rewrites the input into that row's text.
        if prompt == "find:" && minibuffer.accepts_selected(cx) {
            self.pending_find_target =
                self.find_target_at(&minibuffer.input(cx), minibuffer.selected_row());
        }
        if prompt == "file under:" && minibuffer.selected_candidate().is_some() {
            minibuffer.complete_selected(window, cx);
        } else {
            minibuffer.accept_selected(window, cx);
        }
        let (input, on_submit) = minibuffer.into_submission(cx);
        rho_journal::record(rho_journal::Event::MinibufferSubmitted {
            prompt,
            input: input.clone(),
        });
        self.finish_overlay_focus(window, cx);
        // Submitting keeps the narrowing, so there is nothing to put back.
        self.slack_search_before = None;
        on_submit(self, input, window, cx);
        cx.notify();
    }

    pub(crate) fn phone_choose_minibuffer(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.select(index);
        }
        self.minibuffer_confirm(window, cx);
    }

    fn minibuffer_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(minibuffer) = self.minibuffer.take() {
            rho_journal::record(rho_journal::Event::MinibufferCancelled {
                prompt: minibuffer.prompt().to_owned(),
                input: minibuffer.input(cx),
            });
            // What there was to find goes with the prompt that asked. A
            // snapshot is only honest for as long as the reader is looking
            // at the rows it made.
            self.find_snapshot = None;
            let on_cancel = minibuffer.on_cancel();
            self.finish_overlay_focus(window, cx);
            self.restore_slack_search(window, cx);
            if let Some(on_cancel) = on_cancel {
                on_cancel(self, window, cx);
            }
            cx.notify();
        }
    }

    fn finish_git_approval(
        &mut self,
        decision: GitApprovalDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.git_approval.answer(decision) {
            self.finish_overlay_focus(window, cx);
            cx.notify();
        }
    }

    /// Opens a completing-read prompt in the bottom strip: the primitive
    /// menu items drop into for values.
    pub(crate) fn open_prompt(
        &mut self,
        prompt: impl Into<gpui::SharedString>,
        complete: crate::minibuffer::CandidateSource,
        on_submit: crate::minibuffer::SubmitHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_prompt_inner(prompt, complete, None, on_submit, None, window, cx);
    }

    pub(crate) fn open_prompt_cancellable(
        &mut self,
        prompt: impl Into<gpui::SharedString>,
        complete: crate::minibuffer::CandidateSource,
        on_submit: crate::minibuffer::SubmitHandler,
        on_cancel: crate::minibuffer::CancelHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_prompt_inner(
            prompt,
            complete,
            None,
            on_submit,
            Some(on_cancel),
            window,
            cx,
        );
    }

    pub(crate) fn set_prompt_complete_whole_input(&mut self) {
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    /// [`Self::open_prompt`] with a handler that runs after each edit, for a
    /// prompt that narrows what is behind it as the reader types rather than
    /// only on submit.
    pub(crate) fn open_prompt_watching(
        &mut self,
        prompt: impl Into<gpui::SharedString>,
        complete: crate::minibuffer::CandidateSource,
        on_change: Option<crate::minibuffer::ChangeHandler>,
        on_submit: crate::minibuffer::SubmitHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_prompt_inner(prompt, complete, on_change, on_submit, None, window, cx);
    }

    fn open_prompt_inner(
        &mut self,
        prompt: impl Into<gpui::SharedString>,
        complete: crate::minibuffer::CandidateSource,
        on_change: Option<crate::minibuffer::ChangeHandler>,
        on_submit: crate::minibuffer::SubmitHandler,
        on_cancel: Option<crate::minibuffer::CancelHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = prompt.into();
        rho_journal::record(rho_journal::Event::MinibufferOpened {
            prompt: prompt.to_string(),
        });
        self.overlay_focus.capture(window, cx);
        let text_style = self
            .active_editor(cx)
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let mut minibuffer = Minibuffer::open(
            prompt,
            &text_style,
            complete,
            on_change,
            on_submit,
            on_cancel,
            window,
            cx,
        );
        minibuffer.refresh(self, cx);
        self.minibuffer = Some(minibuffer);
        self.clear_menu();
        // The strip is single-occupancy; a stale message reappearing after
        // the prompt closes would be confusing.
        self.echo = None;
        cx.notify();
    }

    /// How long `shift` may be held and still count as a tap. Longer than
    /// this and the reader is holding it for something, even if the
    /// something never arrived.
    /// `tab`, wherever it lands. On the draft it walks the fields. With the
    /// verdicts open it is Home, which the menu's own `tab` row says. With
    /// another menu or a prompt holding the keyboard it is theirs. Over a
    /// card it opens the verdicts; over a surface that is no card it goes
    /// Home, and from Home it is the way back.
    fn verdict_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cycle_draft_field(window, cx) {
            return;
        }
        if self.verdict_transient_open() {
            self.close_menu(window, cx);
            self.toggle_overview(window, cx);
        } else if self.has_modal_overlay() {
        } else if !self.open_verdict_transient(window, cx) {
            self.toggle_overview(window, cx);
        }
    }

    /// `tab`: the verdicts, over whatever card is in view. The card is the
    /// surface the reader is on (or the row under the cursor on Home), so a
    /// verdict follows the eye rather than a mode. Returns whether there
    /// was a card to open it over; with none, the key goes Home instead.
    pub(crate) fn open_verdict_transient(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.label_target(cx).is_none() {
            return false;
        }
        if self.menu_buffer.is_some() {
            return true;
        }
        self.overlay_focus.capture(window, cx);
        self.show_menu(crate::transient::verdict_menu(), None, Back::Out, true);
        self.minibuffer = None;
        self.find_snapshot = None;
        self.slack_search_before = None;
        self.echo = None;
        window.focus(&self.transient_focus, cx);
        cx.notify();
        true
    }

    /// Open a menu at the bottom of the window, the way Magit's transient
    /// sits at the bottom of the frame. The buffer is not touched and the
    /// point does not move; the reader looks down, and looks back up.
    pub(crate) fn open_menu(
        &mut self,
        menu: crate::transient::Menu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlay_focus.capture(window, cx);
        self.show_menu(menu, None, Back::Out, false);
        self.minibuffer = None;
        self.find_snapshot = None;
        self.slack_search_before = None;
        self.echo = None;
        window.focus(&self.transient_focus, cx);
        cx.notify();
    }

    /// Put a menu on screen. Replaces whatever menu is there, which is how
    /// `s` becomes the snooze units in the same place at the bottom of the
    /// window.
    ///
    /// `verdict` is what a menu opened from nothing declares itself to be; a
    /// menu that replaces one inherits it, so the snooze units under the
    /// verdicts are still the verdicts as far as `shift` is concerned.
    fn show_menu(
        &mut self,
        menu: crate::transient::Menu,
        carried_count: Option<u32>,
        back: Back,
        verdict: bool,
    ) {
        let previous = self.menu_buffer.take();
        let verdict = previous.as_ref().map_or(verdict, |open| open.verdict);
        let under = match back {
            Back::Out => Vec::new(),
            Back::Over => previous.map_or_else(Vec::new, |open| {
                let mut under = open.under;
                under.push(open.menu);
                under
            }),
            Back::Under(under) => under,
        };
        self.menu_buffer = Some(MenuBuffer {
            menu,
            carried_count,
            under,
            verdict,
        });
    }

    /// Close the menu, drawing nothing: there is no block to take out of a
    /// buffer, because nothing was ever put into one.
    fn clear_menu(&mut self) {
        self.menu_buffer = None;
    }

    /// One step back: to the menu this one is standing on, or out of the
    /// menus altogether. Escape and the phone sheet's `back` are the same
    /// motion, so they are the same code.
    pub(crate) fn menu_dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.menu_buffer.as_mut() else {
            return;
        };
        match open.under.pop() {
            // Out of the submenu, back to the menu it came from, over the
            // same row and with the rest of the way back kept.
            Some(parent) => {
                let count = open.carried_count;
                let under = std::mem::take(&mut open.under);
                self.show_menu(parent, count, Back::Under(under), false);
                cx.notify();
            }
            None => self.close_menu(window, cx),
        }
    }

    /// The open menu as the phone draws it: its title, a row per item, and
    /// whether there is anything under it to go back to. The same rows the
    /// desk draws — one menu, read two ways, not two menus.
    pub(crate) fn menu_sheet(&self) -> Option<MenuSheet> {
        let open = self.menu_buffer.as_ref()?;
        Some(MenuSheet {
            title: open.menu.title().to_owned(),
            rows: open
                .menu
                .items()
                .iter()
                .map(|item| MenuRow {
                    description: item.description().to_owned(),
                    value: item.value().map(str::to_owned),
                })
                .collect(),
            has_back: !open.under.is_empty(),
        })
    }

    /// A tap on the phone's sheet: the same item the same key would have
    /// reached, run the same way.
    pub(crate) fn run_menu_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.menu_buffer.as_ref() else {
            return;
        };
        let Some(item) = open.menu.items().get(index) else {
            return;
        };
        let action = item.action().clone();
        let closes = item.kind() == rho_window::transient::Kind::Suffix;
        let count = open.carried_count;
        self.run_menu_action(action, count, closes, window, cx);
        cx.notify();
    }

    /// Close the menu and give the keyboard back to the surface it opened
    /// over. The point has not moved: the menu was never in the buffer.
    pub(crate) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu_buffer.is_none() {
            return;
        }
        self.clear_menu();
        self.finish_overlay_focus(window, cx);
        cx.notify();
    }

    /// A key while a menu is open. The menu says what the key meant; the
    /// doing is here, which is the whole point of the primitive.
    fn menu_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu_keystroke(&event.keystroke, window, cx);
    }

    fn menu_keystroke(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Bare modifiers arrive as key events too; holding shift for an
        // uppercase key must not dismiss the menu.
        if matches!(
            keystroke.key.as_str(),
            "shift" | "control" | "alt" | "platform" | "function"
        ) {
            return;
        }
        let Some(open) = self.menu_buffer.as_mut() else {
            return;
        };
        let press = open.menu.press(keystroke);
        let carried = open.carried_count;
        match press {
            rho_window::transient::Press::Count(_) => {
                // The count is drawn in the menu's heading, and the menu is
                // drawn from `menu_buffer`: a redraw is the whole of it.
                cx.notify();
            }
            rho_window::transient::Press::Dismiss => self.menu_dismiss(window, cx),
            rho_window::transient::Press::Unbound => {}
            rho_window::transient::Press::Run {
                item,
                count,
                closes,
            } => {
                let action = open.menu.items()[item].action().clone();
                let count = count.or(carried);
                self.run_menu_action(action, count, closes, window, cx);
            }
        }
        cx.stop_propagation();
    }

    /// What a menu item meant, done. A submenu replaces the menu over the
    /// same row; everything else gives the keyboard back first, so the
    /// command sees normal focus the way the strip menus did.
    fn run_menu_action(
        &mut self,
        action: crate::transient::MenuAction,
        count: Option<u32>,
        closes: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::transient::MenuAction;
        match action {
            MenuAction::Open(id) => self.open_menu_by_id(id, count, window, cx),
            MenuAction::Verdict(verdict) => {
                if closes {
                    self.close_menu(window, cx);
                }
                self.run_verdict(verdict, count.map(|count| count as usize), window, cx);
            }
            MenuAction::Command(command) => {
                if closes {
                    self.close_menu(window, cx);
                }
                self.run_command(command, window, cx);
            }
        }
    }

    /// A menu named by an item of another menu: it replaces the menu on
    /// screen over the same row, so a submenu is a step rather than a new
    /// place, and escape comes back to what named it.
    fn open_menu_by_id(
        &mut self,
        id: crate::transient::MenuId,
        count: Option<u32>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::transient::MenuId;
        let menu = match id {
            MenuId::Slack => crate::transient::slack_menu(),
            MenuId::Hosts => crate::transient::hosts_menu(),
            MenuId::Projects => crate::transient::projects_menu(),
            MenuId::VerdictSnooze => crate::transient::verdict_snooze_menu(),
            MenuId::Input => crate::transient::input_menu(),
            MenuId::Agent => crate::transient::agent_menu(),
            MenuId::New => crate::transient::new_menu(),
            MenuId::Status => crate::transient::status_menu(),
            MenuId::UsageRoot => crate::transient::usage_root_menu(),
        };
        self.show_menu(menu, count, Back::Over, false);
        cx.notify();
    }

    /// The only place that knows what a verdict item means.
    fn run_verdict(
        &mut self,
        action: crate::transient::VerdictAction,
        count: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::transient::VerdictAction;
        match action {
            VerdictAction::Done => self.verdict_done(window, cx),
            VerdictAction::Mute => self.verdict_mute(window, cx),
            VerdictAction::RoomSnooze => self.verdict_room_snooze(count, window, cx),
            VerdictAction::Todo => self.verdict_todo(count, window, cx),
            VerdictAction::File => self.prompt_file_deal_card(window, cx),
            VerdictAction::Undo => self.undo_verdict(window, cx),
            VerdictAction::Pull => self.pull_card(window, cx),
            VerdictAction::Snooze(None) => {
                self.deal_snooze(SnoozeUnit::Days, None, window, cx);
            }
            VerdictAction::Snooze(Some(unit)) => self.deal_snooze(unit, count, window, cx),
        }
    }

    /// The only place that knows what a menu command means. One arm per
    /// item, which makes this the readable list of what the menus can do.
    fn run_command(
        &mut self,
        command: crate::transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::transient::Command;
        match command {
            Command::Voice => self.cmd_voice(window, cx),
            Command::SwitchBuffer => self.open_buffer_picker(window, cx),
            Command::MessageLog => self.cmd_messages(window, cx),
            Command::SurfaceBack => self.cmd_surface_back(window, cx),
            Command::PullCard => self.pull_card(window, cx),
            Command::CloseAndDeal => self.cmd_close_and_deal(window, cx),
            Command::OpenFile => self.prompt_open_file(window, cx),
            Command::FindNode => self.open_find(window, cx),
            Command::NotesForThis => self.open_notes_for_surface(window, cx),
            Command::Wayland => self.open_desktop(window, cx),
            Command::Shell => self.cmd_shell(window, cx),
            Command::ShellClose => self.cmd_shell_close(window, cx),
            Command::Terminal => self.cmd_term(false, window, cx),
            Command::NewTerminal => self.cmd_term(true, window, cx),
            Command::UndoVerdict => window.dispatch_action(Box::new(crate::UndoVerdict), cx),
            Command::Quit => cx.quit(),
            Command::Home => self.toggle_overview(window, cx),
            Command::SlackReact(name) => self.slack_react(&name, window, cx),
            Command::SlackReactByName => self.prompt_slack_react(window, cx),
            Command::SlackConversations => self.open_slack(window, cx),
            Command::SlackSwitch => self.prompt_slack_switch(window, cx),
            Command::SlackPeople => self.prompt_slack_people(window, cx),
            Command::SlackBrowse => self.slack_browse_channels(window, cx),
            Command::SlackFind => self.prompt_slack_find_all(window, cx),
            Command::SlackFiles => self.prompt_slack_find_files(window, cx),
            Command::SlackActivity => self.open_slack_activity(window, cx),
            Command::SlackSaved => self.open_slack_saved(window, cx),
            Command::SlackDrafts => self.open_slack_drafts(window, cx),
            Command::SlackMessageActions => {
                self.prompt_slack_message_actions(window, cx);
            }
            Command::SlackDetach => self.prompt_slack_detach(window, cx),
            Command::SlackBroadcast => self.slack_toggle_broadcast(cx),
            Command::SlackFavorite => self.slack_toggle_favorite(cx),
            Command::SlackFollow => self.slack_toggle_follow(cx),
            Command::SlackAttach => self.prompt_slack_attach(window, cx),
            Command::SlackMessageEdit(ts) => self.slack_edit_message_at(ts, window, cx),
            Command::SlackMessageDelete(ts) => self.confirm_slack_delete_message(ts, window, cx),
            Command::SlackMessageReact(ts) => self.slack_react_at(ts, window, cx),
            Command::SlackMessageCopyLink(ts) => self.slack_copy_message_link(ts, cx),
            Command::SlackMessageForward(ts) => self.prompt_slack_forward_message(ts, window, cx),
            Command::SlackMarkReadBefore => self.prompt_slack_mark_read_before(window, cx),
            Command::SlackMarkUnread => self.slack_mark_unread(window, cx),
            Command::SlackSaveForLater => self.slack_save_for_later(window, cx),
            Command::SlackRegister => self.prompt_slack_register(window, cx),
            Command::HostsList => self.cmd_hosts(cx),
            Command::HostAttach => self.prompt_host_attach(window, cx),
            Command::HostDetach => self.prompt_host_detach(window, cx),
            Command::HostAuth => self.open_host_auth_transient(window, cx),
            Command::LedgerKey => self.prompt_ledger_key(window, cx),
            Command::ProjectAdd => self.prompt_project_add(window, cx),
            Command::ProjectRemove => self.prompt_project_remove(window, cx),
            Command::EndVoice => self.cmd_end_voice(cx),
            Command::PastePrompt => self.cmd_paste_prompt(window, cx),
            Command::ClearPromptImages => self.cmd_clear_prompt_attachments(cx),
            Command::NewAgent => self.begin_new(crate::create::NewKind::Agent, window, cx),
            Command::NewPage => self.begin_new(crate::create::NewKind::Page, window, cx),
            Command::NewNote => self.begin_new(crate::create::NewKind::Note, window, cx),
            Command::Usage(chart, days) => self.open_usage_chart(chart, days, window, cx),
            Command::UploadTelemetry => self.cmd_upload_gui_telemetry(cx),
            Command::Version => self.cmd_version(cx),
            Command::AgentCancel => self.cmd_agent_cancel(window, cx),
            Command::AgentRole => self.prompt_change_agent_role(window, cx),
            Command::AgentMode => self.prompt_change_agent_mode(window, cx),
            Command::VerdictName => self.cmd_verdict_name(window, cx),
            Command::AgentCompact => self.cmd_compact(window, cx),
            Command::AgentRewind => self.cmd_rewind(1, window, cx),
            Command::AgentRewindMany => self.prompt_rewind(window, cx),
            Command::AgentContinue => self.cmd_continue_turn(window, cx),
            Command::AgentCacheKey => self.cmd_change_prompt_cache_key(window, cx),
            Command::PhoneSnoozeAhead(unit, count) => {
                self.phone_verdict_with(
                    rho_journal::PhoneVerdict::Defer,
                    move |workspace, window, cx| {
                        workspace.deal_snooze(unit, Some(count), window, cx)
                    },
                    window,
                    cx,
                );
            }
            Command::PhoneSnoozeAt { hour, tomorrow } => {
                let at = named_hour(hour, tomorrow);
                self.phone_verdict_with(
                    rho_journal::PhoneVerdict::Defer,
                    move |workspace, window, cx| workspace.deal_snooze_at(at, window, cx),
                    window,
                    cx,
                );
            }
        }
    }

    /// Whether the menu under the point is the verdicts, which is what
    /// makes the next `shift` Home rather than another open. A root menu
    /// under the point is not: `shift` there belongs to the menu.
    pub(crate) fn verdict_transient_open(&self) -> bool {
        self.menu_buffer.as_ref().is_some_and(|open| open.verdict)
    }

    fn has_modal_overlay(&self) -> bool {
        self.desktop.is_some()
            || self.minibuffer.is_some()
            || self.menu_buffer.is_some()
            || self.git_approval.waiting()
    }

    /// Captures normal focus on the first overlay in a chain. Replacements
    /// such as transient -> minibuffer inherit the original target.
    /// Gives focus back to whatever the chain of overlays borrowed it
    /// from, and ends the chain. With nothing remembered the reader goes
    /// to the surface they are on, which is where they would have been.
    fn finish_overlay_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.overlay_focus.finish() {
            Some(handle) => {
                window.focus(&handle, cx);
                cx.notify();
            }
            None => self.focus_active_surface(window, cx),
        }
    }

    fn open_desktop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.subject_agent_or_notice("Desktop", window, cx) else {
            return;
        };
        let sessions = self.available_desktops(agent);
        if sessions.len() == 1 {
            self.open_desktop_session(agent, sessions[0].clone(), window, cx);
            return;
        }
        if sessions.is_empty() {
            self.notice_on(
                None,
                "No desktop available; the agent has not started one",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        let complete = std::rc::Rc::new(move |workspace: &Workspace, input: &str, _: &App| {
            workspace
                .available_desktops(agent)
                .into_iter()
                .filter(|name| name.to_lowercase().contains(&input.to_lowercase()))
                .map(|name| crate::commands::Candidate {
                    value: name,
                    description: "available".into(),
                })
                .collect()
        });
        let submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let sessions = workspace.available_desktops(agent);
                let name = if input.is_empty() {
                    sessions.first().cloned()
                } else {
                    sessions.into_iter().find(|name| name == &input)
                };
                if let Some(name) = name {
                    workspace.open_desktop_session(agent, name, window, cx);
                } else {
                    workspace.notice_on(
                        None,
                        "That desktop is no longer available",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            },
        );
        self.open_prompt("desktop:", complete, submit, window, cx);
        self.set_prompt_complete_whole_input();
    }

    pub(crate) fn desktops_arrived(
        &mut self,
        host: HostId,
        sessions: Vec<rho_desktop_client::protocol::DesktopSession>,
        cx: &mut Context<Self>,
    ) {
        self.desktop_sessions.insert(host, sessions);
        cx.notify();
    }

    pub(crate) fn available_desktops(&self, agent: AgentId) -> Vec<String> {
        let owner = agent.encoded();
        let mut sessions: Vec<_> = self
            .desktop_sessions
            .values()
            .flatten()
            .filter(|session| session.agent == owner)
            .map(|session| session.name.clone())
            .collect();
        sessions.sort();
        sessions.dedup();
        sessions
    }

    fn open_desktop_session(
        &mut self,
        agent: AgentId,
        session: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self
            .host_of(agent)
            .and_then(|host| self.hosts.connection(host))
        else {
            return;
        };
        let target = self.models.get(&agent).cloned();
        let task =
            rho_desktop_client::viewer::open(&connection.link(), agent.encoded(), session.clone());
        cx.spawn_in(window, async move |this, cx| match task.await {
            Ok(viewer) => {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.overlay_focus.capture(window, cx);
                    this.desktop_name = session;
                    this.desktop = Some(cx.new(|cx| {
                        crate::wayland_view::WaylandView::new(viewer, window, cx)
                            .with_target(target)
                    }));
                    cx.notify();
                });
            }
            Err(error) => {
                let _ = this.update(cx, |this, cx| {
                    this.notice_on(
                        None,
                        &format!("Desktop: {error:#}"),
                        StyleClass::SystemInfo,
                        cx,
                    )
                });
            }
        })
        .detach();
    }

    /// Prompt for a path to open from the current agent's workspace.
    pub(crate) fn prompt_open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &gpui::App| Vec::new());
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let path = input.trim().to_owned();
                if !path.is_empty() {
                    workspace.cmd_open(camino::Utf8PathBuf::from(path), window, cx);
                }
            },
        );
        self.open_prompt("open:", complete, on_submit, window, cx);
    }

    /// Prompt for how many turns to rewind; empty means one.
    pub(crate) fn prompt_rewind(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &gpui::App| Vec::new());
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let input = input.trim();
                let turns = if input.is_empty() {
                    Some(1)
                } else {
                    input.parse::<u32>().ok().filter(|turns| *turns > 0)
                };
                match turns {
                    Some(turns) => workspace.cmd_rewind(turns, window, cx),
                    None => workspace.notice_on(
                        None,
                        &format!("rewind: bad turn count `{input}`"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            },
        );
        self.open_prompt("rewind turns (1):", complete, on_submit, window, cx);
    }

    /// Prompt for `<path> [name] [description…]` to register a project.
    pub(crate) fn prompt_project_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &gpui::App| Vec::new());
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                let mut tokens = input.split_whitespace();
                let Some(path) = tokens.next() else {
                    return;
                };
                let name = tokens.next().map(str::to_owned);
                let description = tokens.collect::<Vec<_>>().join(" ");
                workspace.cmd_project_add(path.to_owned(), name, description, cx);
            },
        );
        self.open_prompt("project url [name]:", complete, on_submit, window, cx);
    }

    /// Prompt (completing over registered projects) for one to remove.
    pub(crate) fn prompt_project_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_lowercase();
            workspace
                .hosts
                .workdir_table()
                .into_iter()
                .filter(|(name, path)| {
                    name.to_lowercase().contains(&needle) || path.to_lowercase().contains(&needle)
                })
                .map(|(name, path)| crate::commands::Candidate {
                    value: name,
                    description: path,
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                let path = input.trim().to_owned();
                if !path.is_empty() {
                    workspace.cmd_project_remove(path, cx);
                }
            },
        );
        self.open_prompt("remove project:", complete, on_submit, window, cx);
    }

    pub(crate) fn cmd_surface_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step_surface_back(window, cx);
    }

    /// `j` in the verdict transient, and `ctrl-j`: one pull.
    pub(crate) fn cmd_close_and_deal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_current_surface(window, cx);
        self.pull_card(window, cx);
    }

    /// Opens a card as an ordinary surface: the note, the transcript, the
    /// conversation. Nothing about it is a mode; the verdict keys reach it
    /// because it is what the reader is on.
    pub(crate) fn open_card(
        &mut self,
        card: rho_dealer::Card,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let surface = match &card.node {
            rho_dealer::NodeId::Agent(agent_id) => {
                let agent_id = *agent_id;
                rho_journal::record(rho_journal::Event::AgentOpened {
                    agent_id: agent_id.into(),
                });
                self.selection.select_agent(agent_id);
                self.active_context = self.context_for_agent(agent_id);
                self.activate_agent(agent_id, cx);
                self.make_surface(SurfaceKey::Transcript(agent_id), window, cx)
            }
            // A Slack card is a conversation: the deal view is the
            // conversation surface itself, opened the way `enter` opens
            // it, with the message that raised the card on screen.
            rho_dealer::NodeId::Slack(unit) => {
                if self.open_slack_deal(unit, window, cx) {
                    return true;
                }
                self.make_surface(SurfaceKey::Note(card.node.clone()), window, cx)
            }
            node => self.make_surface(SurfaceKey::Note(node.clone()), window, cx),
        };
        self.display_surface_with_method(surface, rho_journal::SurfaceShowMethod::Deal, cx);
        if self.phone.enabled {
            window.focus(&self.phone.feed_focus, cx);
        } else {
            self.focus_active_surface(window, cx);
        }
        cx.notify();
        true
    }

    /// One pull: rank everything that is open, pass over the card in view,
    /// and open the most important of the rest. Nothing is retained, so two
    /// pulls in a row see the same world and the skip is what moves them on.
    pub(crate) fn pull_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phone.enabled && self.phone_snap_in_progress() {
            return;
        }
        let now = chrono::Local::now().fixed_offset();
        let in_view = self.open_card_in_view(cx);
        // Reading a card and pulling again is what a skip is: the card is
        // still owed, it is just not what to look at next.
        if let Some(card) = &in_view
            && let Some(held) = self.hand().into_iter().find(|held| held.node == card.node)
        {
            self.attention
                .dealer
                .skip(held.node.clone(), held.cursor.clone(), now);
            Self::record_dealer_verdict(
                &held,
                rho_journal::DealerVerdict::Skip,
                now,
                Some(now + rho_dealer::curve::SKIP_COOLDOWN),
            );
        }
        let Some(card) = self
            .attention
            .dealer
            .top(now, in_view.as_ref().map(|card| &card.node))
        else {
            // Empty lands on Home rather than on whatever was last open:
            // there is nothing to deal, so the glance is the answer. Home
            // says so in the buffer, so the echo area is left alone and the
            // title still reads "home".
            self.append_message(
                "nothing needs attention".to_owned(),
                StyleClass::SystemInfo,
                cx,
            );
            self.open_home(window, cx);
            return;
        };
        self.open_card(card, window, cx);
        self.invalidate_dealer_signals(cx);
    }

    /// What the timeline records about a verdict on a card.
    fn record_dealer_verdict(
        card: &rho_dealer::Card,
        verdict: rho_journal::DealerVerdict,
        at: chrono::DateTime<chrono::FixedOffset>,
        skip_until: Option<chrono::DateTime<chrono::FixedOffset>>,
    ) {
        let kind = match card.kind {
            rho_dealer::CardKind::Agent => rho_journal::DealerCardKind::Agent,
            rho_dealer::CardKind::Slack => rho_journal::DealerCardKind::Thread,
            rho_dealer::CardKind::Dated => rho_journal::DealerCardKind::Note,
        };
        rho_journal::record(rho_journal::Event::Dealer {
            card: Self::journal_card_identity(&card.node),
            kind,
            verdict,
            occurred_at: at.to_rfc3339(),
            skip_until: skip_until.map(|until| until.to_rfc3339()),
        });
    }

    /// What `f` files: the card in front of the reader, else the row under
    /// the cursor on Home.
    pub(crate) fn label_target(&mut self, cx: &mut Context<Self>) -> Option<rho_dealer::NodeId> {
        if let Some(node) = self.surface_node(cx) {
            return Some(node);
        }
        self.context_area(cx)
    }

    /// The node a card in view is about: the row Home's cursor is on, or
    /// the thing the surface in view stands for. A surface that stands for
    /// nothing — a draft, the message log, a picker — has no card at all,
    /// so a verdict pressed over it lands on nothing.
    fn card_target(&mut self, cx: &mut Context<Self>) -> Option<rho_dealer::NodeId> {
        if self.home_in_view() {
            return self.context_area(cx);
        }
        self.surface_node(cx)
    }

    /// The card the reader is on. When the hand holds no card for that
    /// node the node itself is the card, because reading a thing can be
    /// what quiets it and it is still what is on screen.
    pub(crate) fn card_in_view(&mut self, cx: &mut Context<Self>) -> Option<rho_dealer::Card> {
        let node = self.card_target(cx)?;
        Some(self.card_for(&node))
    }

    /// Whether Home itself is what the reader has open. Its cursor row is a
    /// card, but Home is a list rather than a card: a pull from it opens
    /// the top card instead of passing over the row, and the bar still
    /// says "home".
    pub(crate) fn home_in_view(&self) -> bool {
        self.active_surface().key == SurfaceKey::Home
    }

    /// The card a surface in view stands for, which is nothing on Home.
    pub(crate) fn open_card_in_view(&mut self, cx: &mut Context<Self>) -> Option<rho_dealer::Card> {
        match self.home_in_view() {
            true => None,
            false => self.card_in_view(cx),
        }
    }

    fn deal_card_is_target(&mut self, cx: &mut Context<Self>) -> bool {
        self.card_target(cx).is_some()
    }

    /// Puts the label at `path` on `node`, minting it if nobody has, or
    /// takes it off a node that already carries it.
    pub(crate) fn label_card(
        &mut self,
        node: rho_dealer::NodeId,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.trim().trim_matches('/');
        if path.is_empty() {
            return;
        }
        // The smallest set that says where the thing is: a label it already
        // carries a deeper one of says nothing, so it is not added.
        let deeper = self
            .attention
            .marks
            .get(&node)
            .labels
            .iter()
            .map(|label| self.attention.marks.label_path(*label))
            .find(|held| held.starts_with(&format!("{path}/")));
        if let Some(deeper) = deeper {
            self.echo(
                &format!("already under {deeper}"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        let Some((present, writes)) = self.toggle_label(&node, path, cx) else {
            self.echo("label: nothing to label", StyleClass::SystemInfo, cx);
            return;
        };
        let said = match present {
            true => format!("label: {path}"),
            false => format!("label removed: {path}"),
        };
        // Filing the card in view is a verdict on it like any other, so it
        // is registered for undo. Undo takes the label back off.
        if let Some(card) = self.card_in_view(cx).filter(|card| card.node == node) {
            let sequence = self.attention.push_undo(crate::attention::Undo {
                sequence: 0,
                verb: said.clone(),
                writes,
                card: Some((card.clone(), rho_journal::DealerVerdict::File)),
                slack_cursors: Vec::new(),
                slack_muted: None,
            });
            let phone_verdict = self
                .phone
                .enabled
                .then_some(rho_journal::PhoneVerdict::File);
            self.complete_verdict(
                card,
                rho_journal::DealerVerdict::File,
                phone_verdict,
                sequence,
                said,
                window,
                cx,
            );
            return;
        }
        self.echo(&said, StyleClass::SystemInfo, cx);
    }

    /// `f`: the one filing key, over the card in view or the row under the
    /// cursor. A label path puts that label on the thing, and the same path
    /// again takes it off, so a thing carries as many labels as the user
    /// says. A path nobody has made yet is minted by naming it.
    pub(crate) fn prompt_file_deal_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.label_target(cx) else {
            self.echo("file: nothing under the cursor", StyleClass::SystemInfo, cx);
            return;
        };
        let carried = self.attention.marks.get(&target).labels.clone();
        self.pending_filing_destinations = self
            .attention
            .marks
            .labels()
            .into_iter()
            .map(|(label, path)| {
                let description = match carried.contains(&label) {
                    true => "label · enter takes it off",
                    false => "label",
                };
                (path, description.to_owned())
            })
            .collect();
        self.open_prompt(
            "file under:",
            std::rc::Rc::new(|workspace, needle, _cx| {
                let needle = needle.to_lowercase();
                workspace
                    .pending_filing_destinations
                    .iter()
                    .filter(|(value, description)| {
                        value.to_lowercase().contains(&needle)
                            || description.to_lowercase().contains(&needle)
                    })
                    .map(|(value, description)| crate::minibuffer::Candidate {
                        value: value.clone(),
                        description: description.clone(),
                    })
                    .collect()
            }),
            std::rc::Rc::new(move |workspace, heading, window, cx| {
                workspace.label_card(target.clone(), &heading, window, cx);
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    fn finish_deal_verdict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A verdict from Home is a row leaving the list. The reader is
        // looking at the list, so nothing is opened for them.
        if self.home_in_view() {
            self.invalidate_dealer_signals(cx);
            return;
        }
        // The card was read as an ordinary surface, so a verdict just closes
        // it; what comes next is the next pull, not a queue step.
        self.close_current_surface(window, cx);
        self.pull_card(window, cx);
    }

    /// Marks Slack units handled through the given messages, and leaves one
    /// undo entry for the lot: `mark read before` closed a backlog in one
    /// keystroke, so it comes back in one.
    pub(crate) fn mark_slack_done(
        &mut self,
        units: Vec<(rho_dealer::SlackUnit, rho_slack::types::Ts)>,
        verb: String,
        cx: &mut Context<Self>,
    ) -> usize {
        let mut cursors = Vec::new();
        for (unit, at) in units {
            // A unit is done at the cutoff and no further: anything newer
            // is still the reader's.
            cursors.extend(self.advance_slack_cursor(&unit, Some(at), cx));
        }
        let count = cursors.len();
        if count > 0 {
            self.attention.push_undo(crate::attention::Undo {
                sequence: 0,
                verb,
                writes: Vec::new(),
                card: None,
                slack_cursors: cursors,
                slack_muted: None,
            });
            self.refresh_slack_wants(cx);
            self.invalidate_dealer_signals(cx);
        }
        count
    }

    /// The verdict is made: the undo is armed, the timeline told, and the
    /// card leaves.
    #[allow(clippy::too_many_arguments)]
    fn complete_verdict(
        &mut self,
        card: rho_dealer::Card,
        verdict: rho_journal::DealerVerdict,
        phone_verdict: Option<rho_journal::PhoneVerdict>,
        sequence: u64,
        echo: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .open_card_in_view(cx)
            .is_some_and(|held| held.node == card.node);
        if phone_verdict.is_some() && current {
            self.phone_completed_verdict(sequence);
        }
        self.attention.dealer.clear_skip(&card.node);
        Self::record_dealer_verdict(&card, verdict, chrono::Local::now().fixed_offset(), None);
        if let Some(phone_verdict) = phone_verdict {
            self.record_phone_verdict(phone_verdict, cx);
        }
        if current {
            if phone_verdict.is_some() {
                self.restore_phone_feed(window, cx);
            }
            self.finish_deal_verdict(window, cx);
        }
        self.echo(&echo, StyleClass::SystemInfo, cx);
    }

    /// A verdict on the card in view, or on `target` when the verdict is
    /// about somewhere else: a room snooze is about the room.
    #[allow(clippy::too_many_arguments)]
    fn submit_verdict(
        &mut self,
        target: Option<rho_dealer::NodeId>,
        verdict: crate::attention::Verdict,
        journal: rho_journal::DealerVerdict,
        phone: rho_journal::PhoneVerdict,
        verb: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.phone.enabled && self.phone_snap_in_progress() {
            return false;
        }
        let Some(card) = self.card_in_view(cx) else {
            return false;
        };
        let node = target.unwrap_or_else(|| card.node.clone());
        let Some(mut undo) = self.take_verdict(&node, verdict, cx) else {
            return false;
        };
        // A verdict names the card it took: an agent by the name it
        // answers to, anything else by its title.
        let subject = match &card.node {
            rho_dealer::NodeId::Agent(agent_id) => self.registry.agent_human_name(*agent_id),
            _ => card.title.clone(),
        };
        let subject = match subject.trim().is_empty() {
            true => card.title.clone(),
            false => subject,
        };
        undo.verb = verb.clone();
        undo.card = Some((card.clone(), journal.clone()));
        let sequence = self.attention.push_undo(undo);
        let phone_verdict = self.phone.enabled.then_some(phone);
        self.complete_verdict(
            card,
            journal,
            phone_verdict,
            sequence,
            format!("{verb}: {subject}"),
            window,
            cx,
        );
        true
    }

    /// `n a` with an area chosen: the draft page carries the fields, so
    /// there is no transient in front of it. The area is the agent's
    /// parent and where its workdir is inherited from; the body is focused
    /// so typing composes the first message straight away.
    pub(crate) fn new_agent_in_area(
        &mut self,
        area: Option<rho_dealer::NodeId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workdir = area
            .as_ref()
            .and_then(|area| self.area_workdir(area))
            .or_else(|| self.only_workdir());
        let label = workdir
            .as_ref()
            .map(|workdir| self.hosts.workdir_label(workdir))
            .unwrap_or_default();
        self.select_agent_inner(None, true, window, cx);
        self.draft_area = area;
        let editor = self.focused_draft_editor();
        self.draft_model.update(cx, |view, cx| {
            view.set_body_text("", cx);
            view.clear_attachments(cx);
            view.set_role_text(rho_agents_client::create::DEFAULT_ROLE, cx);
            view.set_start_text(rho_agents_client::create::DEFAULT_START, cx);
            view.set_filesystem_text(rho_agents_client::create::DEFAULT_FILESYSTEM, cx);
            view.seed(&label, true, editor.as_ref(), window, cx);
        });
        // The draft exists to be written in, so it opens ready to type.
        self.enter_insert_when_shown(window, cx);
    }

    /// The workdir to fall back on when nothing else names one: a single
    /// registered project is not a choice worth asking about.
    fn only_workdir(&self) -> Option<HostPath> {
        match self.hosts.workdirs() {
            [workdir] => Some(HostPath {
                host: workdir.host,
                path: workdir.path.clone(),
            }),
            _ => None,
        }
    }

    /// The workdir a new thing under an area inherits: the repository of
    /// the area or the labels it carries, then the agent itself when the
    /// area is an agent. The caller falls back to the host's only workdir.
    fn area_workdir(&self, area: &rho_dealer::NodeId) -> Option<HostPath> {
        if let Some(url) = self.attention.marks.repository_of(area) {
            return Some(HostPath {
                host: self.hosts.primary()?,
                path: url.into(),
            });
        }
        self.agent_workdir(area.agent()?)
    }

    /// The usage screen, built once and kept. A series that arrives while
    /// another screen is in view still lands in it.
    /// Show `chart` over `days`: ask the agent host for the range it needs,
    /// hand the screen what this client already holds so it draws at once,
    /// and display it. Picking another chart from the menu comes back
    /// through here and redraws the same surface.
    /// Asks every host the same usage question; the answers merge as
    /// they come.
    fn ask_every_host<C: rho_rpc::protocol::Call + Clone>(
        &self,
        call: C,
        cx: &mut Context<Self>,
        arrived: fn(&mut Self, HostId, C::Reply, &mut Context<Self>),
    ) {
        for host in self.hosts.ids() {
            self.call(host, call.clone(), cx, move |this, reply, cx| {
                arrived(this, host, reply, cx);
                cx.notify();
            });
        }
    }

    fn quota_history_arrived(
        &mut self,
        host: HostId,
        series: Vec<rho_agents_client::protocol::QuotaSeries>,
        cx: &mut Context<Self>,
    ) {
        self.quotas.set_history(host, series);
        if let Some(view) = self.usage.opened_view() {
            let history = self.quotas.merged_history(&self.hosts);
            let active = self.quotas.active_namespaces(&self.hosts);
            view.update(cx, |view, cx| view.quota_arrived(history, active, cx));
        }
    }

    fn global_usage_arrived(
        &mut self,
        host: HostId,
        series: Vec<rho_agents_client::protocol::AgentUsageSeries>,
        cx: &mut Context<Self>,
    ) {
        self.usage.record_global(host, series);
        if let Some(view) = self.usage.opened_view() {
            let usage = self.usage.merged_global();
            view.update(cx, |view, cx| view.global_usage_arrived(usage, cx));
        }
    }

    fn agent_cost_arrived(
        &mut self,
        host: HostId,
        series: Vec<rho_agents_client::protocol::AgentCostSeries>,
        cx: &mut Context<Self>,
    ) {
        self.usage.record_agent_cost(host, series);
        if let Some(view) = self.usage.opened_view() {
            let usage = self.usage.merged_agent_cost();
            view.update(cx, |view, cx| view.agent_cost_arrived(usage, cx));
        }
    }

    pub(crate) fn open_usage_chart(
        &mut self,
        chart: crate::usage::Chart,
        days: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.usage.view(window, cx);
        view.update(cx, |view, cx| view.show(chart, days, cx));
        // Ask every host for the range the chart needs, then hand the screen
        // what this client already holds so it draws now rather than when
        // the answers come back.
        match crate::usage::Usage::request_for(chart, days, now_ms()) {
            crate::usage::Request::QuotaHistory => {
                self.ask_every_host(agents::QuotaHistory, cx, Self::quota_history_arrived);
                let history = self.quotas.merged_history(&self.hosts);
                let active = self.quotas.active_namespaces(&self.hosts);
                view.update(cx, |view, cx| view.quota_arrived(history, active, cx));
            }
            crate::usage::Request::GlobalUsage { since_ms } => {
                let call = agents::GlobalUsage { since_ms };
                self.ask_every_host(call, cx, Self::global_usage_arrived);
                let usage = self.usage.merged_global();
                view.update(cx, |view, cx| view.global_usage_arrived(usage, cx));
            }
            crate::usage::Request::AgentCostDistribution { since_ms } => {
                let call = agents::AgentCostDistribution { since_ms };
                self.ask_every_host(call, cx, Self::agent_cost_arrived);
                let usage = self.usage.merged_agent_cost();
                view.update(cx, |view, cx| view.agent_cost_arrived(usage, cx));
            }
        }
        let surface = self.make_surface(SurfaceKey::Usage, window, cx);
        self.display_surface_with_method(surface, rho_journal::SurfaceShowMethod::Command, cx);
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Vim's insert, for a surface that was opened to be written in. It is
    /// dispatched to whatever the window has focused, so a caller that just
    /// changed surfaces has to wait for the frame that focus lands in.
    pub(crate) fn enter_insert_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(action) = cx.build_action("vim::InsertBefore", None) {
            window.dispatch_action(action, cx);
        }
    }

    /// Enters insert once the surface just shown is on screen. Anything
    /// opened for the reader to type in wants this: dispatched in the same
    /// breath as the surface change, the insert reaches the old surface and
    /// the first characters of what they write are read as commands.
    pub(crate) fn enter_insert_when_shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_when_shown = true;
        let workspace = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                if std::mem::take(&mut workspace.insert_when_shown) {
                    workspace.enter_insert_mode(window, cx);
                }
            });
        });
    }

    #[cfg(test)]
    pub(crate) fn draft_model_for_test(&self) -> Entity<DraftModel> {
        self.draft_model.clone()
    }

    #[cfg(test)]
    pub(crate) fn cursor_in_draft_field_for_test(&self, cx: &mut Context<Self>) -> bool {
        self.focused_draft_editor()
            .is_some_and(|editor| self.draft_model.read(cx).cursor_in_a_field(&editor, cx))
    }

    #[cfg(test)]
    pub(crate) fn cursor_in_draft_start_field_for_test(&self, cx: &mut Context<Self>) -> bool {
        self.focused_draft_editor()
            .is_some_and(|editor| self.draft_model.read(cx).cursor_in_start_field(&editor, cx))
    }

    #[cfg(test)]
    pub(crate) fn cursor_in_draft_filesystem_field_for_test(&self, cx: &mut Context<Self>) -> bool {
        self.focused_draft_editor().is_some_and(|editor| {
            self.draft_model
                .read(cx)
                .cursor_in_filesystem_field(&editor, cx)
        })
    }

    #[cfg(test)]
    pub(crate) fn cursor_in_draft_role_field_for_test(&self, cx: &mut Context<Self>) -> bool {
        self.focused_draft_editor()
            .is_some_and(|editor| self.draft_model.read(cx).cursor_in_role_field(&editor, cx))
    }

    #[cfg(test)]
    pub(crate) fn submit_from_draft_field_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_from_draft_field(window, cx);
    }

    /// The transcript model and editor of the surface the reader is on.
    fn active_transcript(&self) -> Option<(Entity<AgentModel>, Entity<editor::Editor>)> {
        match &self.active_surface().view {
            SurfaceView::Transcript { model, editor } => Some((model.clone(), editor.clone())),
            _ => None,
        }
    }

    /// `gg` in a transcript: the top of the transcript, which is the top of
    /// its history, so everything is composed on the way. The reader asked
    /// for everything; the echo line says so while it happens.
    pub(crate) fn transcript_top(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((model, editor)) = self.active_transcript() else {
            return false;
        };
        if model.read(cx).uncomposed_blocks() > 0 {
            self.echo("composing history", StyleClass::SystemInfo, cx);
        }
        model.update(cx, |model, cx| {
            model.go_to_store_point(
                &editor,
                rho_agents_view::transcript::StorePoint {
                    block: 0,
                    offset: 0,
                },
                window,
                cx,
            );
        });
        true
    }

    /// Asks for a query and runs `search` with it. Empty input is not a
    /// search, it is a reader who changed their mind.
    fn prompt_for_query(
        &mut self,
        direction: search::Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
        run: impl Fn(&mut Self, search::Query, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let text = input.trim().to_owned();
                if text.is_empty() {
                    return;
                }
                run(workspace, search::Query { text, direction }, window, cx);
            },
        );
        self.open_prompt(
            direction.prompt(),
            std::rc::Rc::new(|_, _, _| Vec::new()),
            on_submit,
            window,
            cx,
        );
    }

    /// Runs `query` in `editor` from the point, selects what it found and
    /// says so if it wrapped; false if there is no match, because who is
    /// told about that differs by surface. A match is where the reader
    /// wanted to be, so the surface stops following its tail.
    fn search_editor(
        &mut self,
        editor: &Entity<editor::Editor>,
        query: search::Query,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = editor.read(cx).text(cx);
        let from = search::point_offset(editor, cx);
        let Some(found) = search::find(&text, &query, from) else {
            return false;
        };
        if found.wrapped {
            self.echo(query.direction.wrap_notice(), StyleClass::SystemInfo, cx);
        }
        let end = found.start + query.text.len();
        self.search.record(query);
        editor.update(cx, |editor, cx| {
            editor.clear_autoscroll_pin(cx);
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([
                    editor::MultiBufferOffset(found.start)..editor::MultiBufferOffset(end)
                ]);
            });
        });
        window.focus(&editor.read(cx).focus_handle(cx), cx);
        true
    }

    /// `/` in a transcript. The buffer's search, as everywhere: history it
    /// has not composed yet is composed first, so what the reader is
    /// looking through is the whole transcript.
    fn prompt_transcript_search(
        &mut self,
        direction: search::Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((model, _)) = self.active_transcript() else {
            return;
        };
        let Some(agent_id) = self.selection.selected_agent() else {
            return;
        };
        if model.read(cx).uncomposed_blocks() > 0 {
            // The reader is typing a query; the history they will look
            // through is composed while they do.
            self.echo("composing history", StyleClass::SystemInfo, cx);
            model.update(cx, |model, cx| {
                model.request_history(rho_agents_view::agent_view::HistoryWant::All, window, cx);
            });
        }
        self.prompt_for_query(
            direction,
            window,
            cx,
            move |workspace, query, window, cx| {
                workspace.run_transcript_search(agent_id, query, window, cx);
            },
        );
    }

    /// Runs a transcript search once every row it could match is composed;
    /// until then it waits, and the composition that is already running
    /// finishes it.
    fn run_transcript_search(
        &mut self,
        agent_id: AgentId,
        query: search::Query,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((model, editor)) = self.active_transcript() else {
            return;
        };
        if model.read(cx).uncomposed_blocks() > 0 {
            self.search.wait_for(search::Pending {
                agent: agent_id,
                query,
            });
            model.update(cx, |model, cx| {
                model.request_history(rho_agents_view::agent_view::HistoryWant::All, window, cx);
            });
            return;
        }
        if !self.search_editor(&editor, query, window, cx) {
            self.notice_on(
                Some(&agent_id),
                "search: no match",
                StyleClass::SystemInfo,
                cx,
            );
        }
    }

    /// `n` and `N`: the last search again, from the point, in its own
    /// direction or the other one. The query is the workspace's, so a
    /// search typed in one surface repeats in the next; the buffer it looks
    /// through is whichever one the reader is in.
    pub(crate) fn repeat_search(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(query) = self.search.last().cloned() else {
            return false;
        };
        let query = search::Query {
            direction: if reverse {
                query.direction.reversed()
            } else {
                query.direction
            },
            ..query
        };
        if self.active_transcript().is_some() {
            let Some(agent_id) = self.selection.selected_agent() else {
                return false;
            };
            self.run_transcript_search(agent_id, query, window, cx);
            return true;
        }
        false
    }

    /// A search that was waiting for history runs now that it is composed.
    fn finish_transcript_search(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(query) = self.search.take_waiting_for(agent_id) else {
            return;
        };
        self.run_transcript_search(agent_id, query, window, cx);
    }

    /// Where a card is, as the status line and the phone's header name it.
    pub(crate) fn card_path(card: &rho_dealer::Card) -> String {
        match &card.node {
            // A Slack deal shows the conversation and nothing else: the
            // words are on screen already, and the state segment says whose
            // turn it is.
            rho_dealer::NodeId::Slack(unit) => {
                let conversation = match card.context.as_str() {
                    "" => "slack".to_owned(),
                    context => context.to_owned(),
                };
                match unit.thread.is_some() {
                    true => format!("{conversation} / thread"),
                    false => conversation,
                }
            }
            _ if card.context.is_empty() => card.title.clone(),
            _ => format!("{} / {}", card.context, card.title),
        }
    }

    fn render_deal_why(
        &self,
        card: &rho_dealer::Card,
        text_style: &gpui::TextStyle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let path = Self::truncate_outline_path(&Self::card_path(card));
        let line = div()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.deal_controls_visible = !this.deal_controls_visible;
                    cx.notify();
                }),
            )
            .child(self.render_status_path(&path, cx))
            .child(
                div()
                    .text_color(cx.theme().status().warning)
                    .child(card.label.clone()),
            );
        let right = self.render_status_right(cx);
        if self.deal_controls_visible {
            return self.status_row(
                line.child(
                    div()
                        .id("deal-touch-close")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(DealExit), cx))
                        .child("close"),
                )
                .child(
                    div()
                        .id("deal-touch-done")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(DealDone), cx))
                        .child("done"),
                )
                .child(
                    div()
                        .id("deal-touch-defer")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(DealSnooze), cx))
                        .child("defer"),
                )
                .child(
                    div()
                        .id("deal-touch-mute")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(DealMute), cx))
                        .child("mute"),
                )
                .child(
                    div()
                        .id("deal-touch-next")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(DealNext), cx))
                        .child("next"),
                ),
                right,
                text_style,
                window,
                cx,
            );
        }
        self.status_row(line, right, text_style, window, cx)
    }

    fn truncate_outline_path(path: &str) -> String {
        const MAX_CHARS: usize = 80;
        if path.chars().count() <= MAX_CHARS {
            return path.to_owned();
        }
        let parts = path.split(" / ").collect::<Vec<_>>();
        if parts.len() < 3 {
            return path.chars().take(MAX_CHARS - 1).collect::<String>() + "…";
        }
        format!("{} / … / {}", parts[0], parts[parts.len() - 1])
    }

    fn abnormal_connection_text(&self) -> Option<String> {
        let (name, status) = self.hosts.worst_status()?;
        let subject = (self.hosts.len() > 1).then(|| format!("{name} "));
        match status {
            HostStatus::Connecting => Some(format!(
                "{}connecting",
                subject.as_deref().unwrap_or_default()
            )),
            HostStatus::Recovering(_) => Some(format!(
                "{}reconnecting",
                subject.as_deref().unwrap_or_default()
            )),
            HostStatus::Disconnected(_) => Some(format!(
                "{}disconnected",
                subject.as_deref().unwrap_or_default()
            )),
            HostStatus::Online => None,
        }
    }

    fn render_status_path(&self, path: &str, cx: &App) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let parts = path.split(" / ").collect::<Vec<_>>();
        div()
            .flex()
            .flex_row()
            .children(parts.into_iter().enumerate().flat_map(|(index, part)| {
                let color = if index == 0 {
                    colors.terminal_ansi_bright_magenta
                } else if index + 1 == path.split(" / ").count() {
                    colors.text
                } else {
                    colors.terminal_ansi_bright_green
                };
                let separator = (index > 0).then(|| {
                    div()
                        .text_color(colors.text_muted)
                        .child(" / ")
                        .into_any_element()
                });
                separator.into_iter().chain(std::iter::once(
                    div()
                        .text_color(color)
                        .child(part.to_owned())
                        .into_any_element(),
                ))
            }))
            .into_any_element()
    }

    fn render_status_right(&self, cx: &App) -> gpui::AnyElement {
        /// The status line's mode word: three letters, the way helix does
        /// it, so the segment never moves when the mode changes.
        fn mode_word(mode: &str) -> String {
            match mode {
                "normal" => "NOR",
                "insert" => "INS",
                "replace" => "REP",
                "visual" => "VIS",
                "visual line" => "V-LINE",
                "visual block" => "V-BLOCK",
                "select" => "SEL",
                other => return other.to_uppercase(),
            }
            .to_owned()
        }
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let mode = self
            .mode_indicator
            .read(cx)
            .plain_mode(cx)
            .unwrap_or_else(|| "normal".to_owned());
        let mode_color = if mode.contains("insert") {
            status.warning
        } else {
            colors.terminal_ansi_bright_cyan
        };
        let quota = self.quotas.merged_summaries(&self.hosts);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(div().flex().flex_row().items_center().gap_1p5().children(
                quota.into_iter().enumerate().flat_map(|(index, summary)| {
                    // Colour is the provider's, always; the number says how low.
                    let color = match summary.model.as_str() {
                        "gpt" => colors.terminal_ansi_cyan,
                        "opus" | "fable" => gpui::rgb(0xd97757).into(),
                        _ => colors.text_muted,
                    };
                    let separator = (index > 0).then(|| {
                        div()
                            .text_color(colors.text_muted)
                            .child("·")
                            .into_any_element()
                    });
                    separator.into_iter().chain(std::iter::once(
                        div()
                            .text_color(color)
                            // Colour alone names the provider; no model text, and
                            // no reset time: the usage transient carries that.
                            .child(format!("{}%", summary.remaining_percent))
                            .into_any_element(),
                    ))
                }),
            ))
            .children(self.abnormal_connection_text().map(|connection| {
                div()
                    .id(gpui::LiveOwner::every(
                        "rho-host-connection-status",
                        Duration::from_secs(1),
                    ))
                    .text_color(status.error)
                    .child(connection)
            }))
            .children(self.lamp_on.then(|| {
                div()
                    .flex_none()
                    .size(px(7.))
                    .rounded_full()
                    .bg(status.error)
            }))
            .child(div().text_color(mode_color).child(mode_word(&mode)))
            .into_any_element()
    }

    /// The notch cutout in logical pixels, from `RHO_NOTCH=WxH` in physical
    /// pixels (the M2 Air's is about 290x56). The window is taken to be
    /// fullscreen: nothing checks where it sits on the output.
    fn notch(window: &Window) -> Option<gpui::Size<gpui::Pixels>> {
        static NOTCH: std::sync::OnceLock<Option<(f32, f32)>> = std::sync::OnceLock::new();
        let (width, height) = (*NOTCH.get_or_init(|| {
            let spec = std::env::var("RHO_NOTCH").ok()?;
            let (width, height) = spec.split_once('x')?;
            Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
        }))?;
        let scale = window.scale_factor();
        Some(gpui::size(px(width / scale), px(height / scale)))
    }

    /// The status line: one row across the top of the window. With a notch
    /// the row is the notch's height and its middle, the notch's width,
    /// stays empty, so `left` and `right` sit either side of it.
    fn status_row(
        &self,
        left: gpui::Div,
        right: gpui::AnyElement,
        text_style: &gpui::TextStyle,
        window: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        let notch = Self::notch(window);
        let height = notch.map_or(px(26.), |notch| notch.height.max(px(26.)));
        let row = div()
            .id("rho-status-line")
            .h(height)
            .min_h_0()
            .w_full()
            .px_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant.opacity(0.6))
            .flex()
            .flex_row()
            .items_center()
            .bg(cx.theme().colors().editor_background)
            .text_color(cx.theme().colors().text)
            .font_family(text_style.font_family.clone())
            .text_size(text_style.font_size)
            .line_height(text_style.line_height);
        let left = left
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_row()
            .items_center()
            .gap_3();
        match notch {
            Some(notch) => row
                .child(left)
                .child(div().flex_none().w(notch.width))
                .child(div().flex_1().flex().flex_row().justify_end().child(right)),
            None => row.child(left).child(right),
        }
        .into_any_element()
    }

    fn render_status_line(
        &mut self,
        text_style: &gpui::TextStyle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // An echo is the answer to the key just pressed, so it takes the
        // line for its two seconds. The card's why used to win, and every
        // refusal behind a card went unsaid — a new agent with nowhere to
        // run said nothing at all.
        let echo = self.echo.as_ref().map(|echo| echo.text().to_owned());
        // Otherwise the label is about whatever is in view: with a card
        // behind the surface it says which card and why, with the map open
        // it is the map's own breadcrumb.
        // An agent surface's state comes from its head, never from the card
        // that dealt it: the card says why the dealer raised the agent, and
        // a running agent has no card at all, which left the line blank.
        let agent_in_view = match &self.active_surface().key {
            SurfaceKey::Transcript(agent_id) => Some(*agent_id),
            _ => None,
        };
        if let Some(card) = self.open_card_in_view(cx)
            && echo.is_none()
            && agent_in_view.is_none()
        {
            return self.render_deal_why(&card, text_style, window, cx);
        }
        let path = {
            match &self.active_surface().key {
                SurfaceKey::Transcript(agent_id) => {
                    let leaf = self.registry.agent_display_label(*agent_id);
                    match self.node_context(&rho_dealer::NodeId::Agent(*agent_id)) {
                        context if context.is_empty() => leaf,
                        context => format!("{context} / {leaf}"),
                    }
                }
                SurfaceKey::Browser(page) => {
                    rho_browser::live_page_name(*page).unwrap_or_else(|| "page".to_owned())
                }
                SurfaceKey::SlackConversation(source) => self
                    .slack
                    .session()
                    .map(|session| session.read(cx).label(source))
                    .unwrap_or_else(|| self.surface_name(&self.active_surface().key)),
                key => self.surface_name(key),
            }
        };
        let state = agent_in_view
            .filter(|_| echo.is_none())
            .and_then(|agent_id| {
                let facts = self.registry.agent_facts(agent_id);
                crate::attention::agent_state_label(&facts, chrono::Local::now().fixed_offset())
            });
        let state = state.map(|state| div().text_color(cx.theme().status().warning).child(state));
        let left = echo.map_or_else(
            || self.render_status_path(&Self::truncate_outline_path(&path), cx),
            |echo| {
                div()
                    .text_color(cx.theme().status().info)
                    .child(echo)
                    .into_any_element()
            },
        );
        let right = self.render_status_right(cx);
        // What arrived at the end of the conversation while the reader was
        // further up. It sits beside the surface's name because that is
        // what it is about, and it goes out when they reach the end.
        let unseen = self.slack_unseen(cx).map(|unseen| {
            div()
                .text_color(cx.theme().colors().text_accent)
                .child(format!("{unseen} new"))
        });
        let desktops = agent_in_view
            .map(|agent| self.available_desktops(agent).len())
            .unwrap_or(0);
        let available = (desktops > 0).then(|| {
            div()
                .id("desktop-available")
                .cursor_pointer()
                .text_color(cx.theme().colors().text_accent)
                .child(if desktops == 1 {
                    "desktop available · SPC w".to_owned()
                } else {
                    format!("{desktops} desktops · SPC w")
                })
                .on_click(cx.listener(|this, _, window, cx| this.open_desktop(window, cx)))
        });
        self.status_row(
            div()
                .child(left)
                .children(state)
                .children(unseen)
                .children(available),
            right,
            text_style,
            window,
            cx,
        )
    }

    fn render_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        {
            let focused_surface = self.active_surface().view.telemetry_kind();
            crate::telemetry::record_surfaces(focused_surface, focused_surface.bit());
        }
        let sidebar = if self.active_context == ContextId::Slack
            && !matches!(self.active_surface().view, SurfaceView::SlackList(_))
        {
            self.render_slack_sidebar(window, cx)
        } else {
            None
        };
        div()
            .flex()
            .flex_row()
            .w_full()
            .flex_grow(1.0)
            .min_h_0()
            .children(sidebar)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .child(self.render_surface(self.active_surface())),
            )
            .into_any_element()
    }

    fn render_surface(&self, surface: &Surface) -> gpui::AnyElement {
        match &surface.view {
            SurfaceView::Draft { editor, .. } => div()
                .id("rho-surface-draft")
                .key_context("RhoDraft")
                .size_full()
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            SurfaceView::Home(view) => div()
                .id("rho-surface-home")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::Messages(editor) => div()
                .id("rho-surface-messages")
                .size_full()
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            SurfaceView::Usage(view) => div()
                .id("rho-surface-usage")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::Note(editor) => div()
                .id("rho-surface-note")
                .key_context("RhoNote")
                .size_full()
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            SurfaceView::Transcript { editor, .. } => div()
                .id("rho-surface-transcript")
                // Named because keys are bound to it: a transcript is one of
                // the two surfaces with a search of its own, so it is one of
                // the two that has `n`.
                .key_context("RhoTranscript")
                .size_full()
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            SurfaceView::File(view) => div()
                .id("rho-surface-file")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::SlackList(view) => div()
                .id("rho-surface-slack-list")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::SlackResults(view) => div()
                .id("rho-surface-slack-results")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::SlackConversation(view) => div()
                .id("rho-surface-slack-conversation")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::Image(view) => div()
                .id("rho-surface-image")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::Shell { editor, .. } => div()
                .id("rho-surface-shell")
                .key_context("RhoShell")
                .size_full()
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            SurfaceView::Terminal(view) => div()
                .id("rho-surface-terminal")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::Browser(view) => div()
                .id("rho-surface-browser")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
        }
    }

    fn update_statuses(&self, cx: &mut Context<Self>) {
        for (agent_id, view) in &self.models {
            self.refresh_view_status(agent_id, view, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn connection_status_label(&self) -> Option<String> {
        match self.hosts.worst_status()?.1 {
            HostStatus::Connecting => Some("connecting".to_owned()),
            HostStatus::Recovering(elapsed) => Some(format!("recovering {}s", elapsed.as_secs())),
            HostStatus::Disconnected(reason) => Some(format!("disconnected {reason}")),
            HostStatus::Online => None,
        }
    }

    /// The agents the draft's start field offers. An agent the user hid is
    /// not offered — the finder leaves it out for the same reason — but it
    /// is not made unreachable: its handle still resolves, and its row on
    /// the map is where the user takes the hiding back.
    pub fn live_agent_targets(&self) -> Vec<crate::commands::Candidate> {
        let mut candidates = Vec::new();
        for agent_id in self
            .registry
            .known_agents()
            .filter(|agent_id| !self.registry.agent_muted(**agent_id))
        {
            let id_label = self.registry.agent_id_label(*agent_id);
            let display_name = self
                .registry
                .agent_display_name(*agent_id)
                .map(str::to_owned);
            candidates.push(crate::commands::Candidate {
                // The handle stays the value: this is the token the reader
                // types to name an agent, and a name with a space in it is
                // not one. What they read while choosing is the name, and
                // "agent" told them nothing about which.
                value: id_label.clone(),
                description: display_name
                    .clone()
                    .unwrap_or_else(|| self.registry.agent_human_name(*agent_id)),
            });
        }
        candidates
    }

    fn agent_target_hints(&self) -> Vec<(String, String)> {
        let mut hints = Vec::new();
        for agent_id in self.registry.known_agents() {
            let id_label = self.registry.agent_id_label(*agent_id);
            if let Some(display_name) = self.registry.agent_display_name(*agent_id) {
                hints.push((id_label, display_name.to_owned()));
            }
        }
        hints
    }

    fn refresh_draft_agent_targets(&mut self, cx: &mut Context<Self>) {
        let hints = self.agent_target_hints();
        self.draft_model
            .update(cx, |view, cx| view.set_start_target_hints(hints, cx));
    }

    fn ensure_duration_timer(&mut self, cx: &mut Context<Self>) {
        if self.duration_timer.is_some() {
            return;
        }
        if !self
            .active_agent_model()
            .is_some_and(|view| view.read(cx).has_timers())
        {
            return;
        }
        self.duration_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let keep_going = this.update(cx, |this, cx| {
                    let Some(view) = this.active_agent_model() else {
                        return false;
                    };
                    view.update(cx, |view, cx| {
                        view.tick_timers(now_ms(), cx);
                        view.has_timers()
                    })
                });
                if !matches!(keep_going, Ok(true)) {
                    break;
                }
            }
            let _ = this.update(cx, |this, _| this.duration_timer = None);
        }));
    }
}

/// How a filesystem mode reads in a prompt: the draft field's words.
fn mode_label(mode: rho_agent_types::WorksetMode) -> &'static str {
    match mode {
        rho_agent_types::WorksetMode::View => "view",
        rho_agent_types::WorksetMode::Exposed => "exposed",
    }
}

/// How a role reads in the chips a transcript shows. Only the tests ask
/// for it as a string; the chips themselves are styled from the family.
#[cfg(test)]
fn agent_role_label(config: AgentRole) -> String {
    match config {
        AgentRole::Advisor { intelligence } => match intelligence {
            AdvisorIntelligence::Low => "low-adv",
            AdvisorIntelligence::Medium => "med-adv",
            AdvisorIntelligence::Medium1 => "med1-adv",
        },
        AgentRole::Engineer { intelligence } => match intelligence {
            EngineerIntelligence::Mini => "mini-eng",
            EngineerIntelligence::Medium => "med-eng",
            EngineerIntelligence::High => "high-eng",
            EngineerIntelligence::Medium1 => "med1-eng",
            EngineerIntelligence::High1 => "high1-eng",
        },
    }
    .to_owned()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(desktop) = self.desktop.clone() {
            let editor = self.active_editor(cx);
            let text_style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
            return div()
                .font_family(text_style.font_family)
                .size_full()
                .flex()
                .flex_col()
                .bg(cx.theme().colors().editor_background)
                .text_color(cx.theme().colors().text)
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .px(px(8.))
                        .py(px(3.))
                        .text_size(px(12.))
                        .text_color(cx.theme().colors().text_muted)
                        .child(format!("desktop / {}", self.desktop_name))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(12.))
                                .child(
                                    div()
                                        .id("annotate-desktop")
                                        .p(px(4.))
                                        .tooltip(ui::Tooltip::text("Toggle drawing mode"))
                                        .cursor_pointer()
                                        .child(
                                            gpui::svg()
                                                .path("icons/pencil.svg")
                                                .size(px(14.))
                                                .text_color(cx.theme().colors().text_muted),
                                        )
                                        .on_click({
                                            let desktop = desktop.clone();
                                            move |_, window, cx| {
                                                desktop.update(cx, |view, cx| {
                                                    view.toggle_annotation(window, cx)
                                                });
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .id("close-desktop")
                                        .px(px(4.))
                                        .tooltip(ui::Tooltip::text("Return to agent"))
                                        .cursor_pointer()
                                        .child("×")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.desktop = None;
                                            this.finish_overlay_focus(window, cx);
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .child(div().flex_1().min_h_0().child(desktop))
                .into_any_element();
        }
        let editor = self.active_editor(cx);
        let text_style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
        let phone = self.phone_mode(window, cx);
        div()
            .id("rho-gui")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .key_context("RhoGui")
            .capture_touch(cx.listener(Self::shell_touch))
            .on_action(cx.listener(|this, _: &VerdictMenu, window, cx| {
                this.verdict_key(window, cx);
            }))
            .on_scroll_wheel(cx.listener(Self::journal_scroll))
            .on_linux_pointer_axis(cx.listener(Self::journal_linux_scroll))
            .on_action(cx.listener(Self::submit_prompt))
            .on_action(cx.listener(Self::paste_prompt))
            .on_action(cx.listener(|this, _: &SurfaceBack, window, cx| {
                this.step_surface_back(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealOpen, window, cx| {
                this.cmd_surface_forward_or_deal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealCloseAndNext, window, cx| {
                this.close_current_surface(window, cx);
                this.pull_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OverviewToggle, window, cx| {
                this.toggle_overview(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SurfaceClose, window, cx| {
                this.close_current_surface(window, cx);
            }))
            .on_action(cx.listener(|this, _: &MessagesOpen, window, cx| {
                this.cmd_messages(window, cx);
            }))
            .on_action(cx.listener(|this, _: &HomeOpenRow, window, cx| {
                this.home_open_row(window, cx);
            }))
            .on_action(cx.listener(|this, _: &BrowserExit, window, cx| {
                // The rail was the map's; leaving the browser lands on
                // Home, which is the front door either way.
                this.open_home(window, cx);
            }))
            .on_action(cx.listener(Self::shell_interrupt))
            .on_action(cx.listener(Self::toggle_voice))
            .on_action(cx.listener(Self::shell_eof))
            .on_action(
                cx.listener(|this, _: &crate::SlackSidebarFocus, window, cx| {
                    if this.active_context == ContextId::Slack {
                        if let Some(list) = this.slack_list_view(window, cx) {
                            window.focus(&list.read(cx).editor().focus_handle(cx), cx);
                        }
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::SlackConversationFocus, window, cx| {
                    this.focus_active_surface(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::SlackQuickSwitch, window, cx| {
                    this.prompt_slack_switch(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::SlackNewMessage, window, cx| {
                this.prompt_slack_people(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::SlackBrowseChannels, window, cx| {
                    this.slack_browse_channels(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &SlackOpenRow, window, cx| {
                this.slack_open_row(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackCompose, window, cx| {
                this.slack_compose(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackSearch, window, cx| {
                this.prompt_slack_search(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackFindMessage, window, cx| {
                if matches!(
                    &this.active_surface().view,
                    SurfaceView::SlackConversation(_)
                ) {
                    this.prompt_slack_find(window, cx);
                } else {
                    this.prompt_slack_find_all(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SlackFindFile, window, cx| {
                this.prompt_slack_find_files(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackOpenFound, window, cx| {
                // Not on a hit: the line opens nothing, so `enter` is the
                // editor's own again.
                if !this.slack_open_found(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(
                cx.listener(|this, _: &SlackSearchPreviousPage, window, cx| {
                    if !this.slack_search_page(-1, window, cx) {
                        cx.propagate();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &SlackSearchNextPage, window, cx| {
                if !this.slack_search_page(1, window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &SlackEditMessage, window, cx| {
                // Not on a message of the reader's own: `e` is vim's own
                // word motion again.
                if !this.slack_edit_message(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &SlackReactTo, window, cx| {
                // Not on a message: `r` is vim's own key again.
                if !this.slack_open_react_menu(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &SlackEditLast, window, cx| {
                if !this.slack_edit_last(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &SlackCancelEdit, window, cx| {
                if !this.slack_cancel_edit(cx) {
                    cx.propagate();
                    return;
                }
                // One press, not two: cancelling puts the reader back in
                // normal mode with the composer as it was, the same as
                // escape does when there is no edit open.
                if let Ok(action) = cx.build_action("vim::NormalBefore", None) {
                    window.dispatch_action(action, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SlackNextUnread, window, cx| {
                this.slack_next_unread(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackMarkReadBefore, window, cx| {
                this.prompt_slack_mark_read_before(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackMarkUnread, window, cx| {
                this.slack_mark_unread(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackSaveForLater, window, cx| {
                this.slack_save_for_later(window, cx);
            }))
            .on_action(cx.listener(|this, _: &TranscriptTop, window, cx| {
                // Only a transcript composes its way to the top; anywhere
                // else `gg` is vim's own.
                if !this.transcript_top(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &SearchRepeat, window, cx| {
                if !this.repeat_search(false, window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &SearchRepeatReverse, window, cx| {
                if !this.repeat_search(true, window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &FindNode, window, cx| {
                this.open_find(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShellPagerMore, _, cx| {
                this.shell_pager_action(rho_shell_view::protocol::PagerAction::Continue, cx);
            }))
            .on_action(cx.listener(|this, _: &ShellPagerAll, _, cx| {
                this.shell_pager_action(rho_shell_view::protocol::PagerAction::Drain, cx);
            }))
            .on_action(cx.listener(|this, _: &ShellPagerQuit, _, cx| {
                this.shell_pager_action(rho_shell_view::protocol::PagerAction::Quit, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentPrevious, window, cx| {
                this.switch_agent_by_delta(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentNext, window, cx| {
                this.switch_agent_by_delta(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentNew, window, cx| {
                this.select_agent(None, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealExit, window, cx| {
                vim::take_count(cx);
                this.close_current_surface(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealNext, window, cx| {
                vim::take_count(cx);
                this.pull_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &UndoVerdict, window, cx| {
                vim::take_count(cx);
                this.undo_verdict(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealDone, window, cx| {
                vim::take_count(cx);
                this.verdict_done(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealMute, window, cx| {
                vim::take_count(cx);
                this.verdict_mute(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealSnooze, window, cx| {
                let count = vim::take_count(cx);
                this.deal_snooze(SnoozeUnit::Days, count, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::DealSnoozeMinutes, window, cx| {
                    let count = vim::take_count(cx);
                    this.deal_snooze(SnoozeUnit::Minutes, count, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::DealSnoozeHours, window, cx| {
                let count = vim::take_count(cx);
                this.deal_snooze(SnoozeUnit::Hours, count, window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::DealSnoozeWeeks, window, cx| {
                let count = vim::take_count(cx);
                this.deal_snooze(SnoozeUnit::Weeks, count, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealRoomSnooze, window, cx| {
                let count = vim::take_count(cx);
                this.verdict_room_snooze(count, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealTodo, window, cx| {
                let count = vim::take_count(cx);
                this.verdict_todo(count, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealRefresh, window, cx| {
                vim::take_count(cx);
                this.pull_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealFile, window, cx| {
                vim::take_count(cx);
                this.prompt_file_deal_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealReply, window, cx| {
                vim::take_count(cx);
                let Some(card) = this.card_in_view(cx) else {
                    return;
                };
                if matches!(card.node, rho_dealer::NodeId::Slack(_)) && !this.phone.enabled {
                    return;
                }
                Self::record_dealer_verdict(
                    &card,
                    rho_journal::DealerVerdict::Open,
                    chrono::Local::now().fixed_offset(),
                    None,
                );
                match &card.node {
                    rho_dealer::NodeId::Slack(unit) => {
                        this.open_slack_source(crate::slack::unit_source(unit), window, cx);
                        if let SurfaceView::SlackConversation(view) = &this.active_surface().view {
                            let view = view.clone();
                            view.update(cx, |view, cx| view.select_compose(window, cx));
                            window.focus(&view.read(cx).editor().focus_handle(cx), cx);
                        }
                    }
                    rho_dealer::NodeId::Agent(agent_id) => {
                        this.open_agent(*agent_id, window, cx);
                        if this.phone.enabled
                            && let SurfaceView::Transcript { model, editor } =
                                &this.active_surface().view
                        {
                            let (model, editor) = (model.clone(), editor.clone());
                            model.update(cx, |model, cx| model.focus_prompt(&editor, window, cx));
                        }
                    }
                    node => {
                        this.open_note(node, window, cx);
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &TaskBoard, _window, cx| {
                this.notice_on(
                    None,
                    "task board is not available yet",
                    StyleClass::SystemInfo,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &UploadGuiTelemetry, _window, cx| {
                this.cmd_upload_gui_telemetry(cx);
            }))
            .on_action(cx.listener(|this, _: &RoleCycle, window, cx| {
                this.cycle_draft_field(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RoleCycleGroup, window, cx| {
                this.cycle_draft_group(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DraftValueCycle, _window, cx| {
                this.cycle_draft_value(cx);
            }))
            .on_action(cx.listener(|this, _: &DraftFieldSubmit, window, cx| {
                this.submit_from_draft_field(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DraftFieldClear, window, cx| {
                this.clear_draft_field(window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::NoteOpenRow, window, cx| {
                if !this.note_open_row(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &crate::NotesForThis, window, cx| {
                this.open_notes_for_surface(window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::RootTransient, window, cx| {
                let subject = this.subject(window, cx);
                this.open_menu(crate::transient::root_menu(&subject), window, cx);
            }))
            .on_action(cx.listener(|this, _: &MinibufferConfirm, window, cx| {
                this.minibuffer_confirm(window, cx);
            }))
            .on_action(cx.listener(|this, _: &MinibufferCancel, window, cx| {
                this.minibuffer_cancel(window, cx);
            }))
            .on_action(cx.listener(|this, _: &MinibufferNext, _window, cx| {
                if let Some(minibuffer) = &mut this.minibuffer {
                    minibuffer.select_by_delta(1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &MinibufferPrevious, _window, cx| {
                if let Some(minibuffer) = &mut this.minibuffer {
                    minibuffer.select_by_delta(-1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &MinibufferComplete, window, cx| {
                if let Some(mut minibuffer) = this.minibuffer.take() {
                    minibuffer.complete_selected(window, cx);
                    this.minibuffer = Some(minibuffer);
                }
            }))
            .on_action(cx.listener(|this, _: &GitApprovalAllow, window, cx| {
                this.finish_git_approval(GitApprovalDecision::Allow, window, cx);
            }))
            .on_action(cx.listener(|this, _: &GitApprovalDeny, window, cx| {
                this.finish_git_approval(GitApprovalDecision::Deny, window, cx);
            }))
            .children((!phone).then(|| self.render_status_line(&text_style, window, cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(if phone {
                        self.render_phone_body(&text_style, window, cx)
                    } else {
                        self.render_workspace(window, cx)
                    }),
            )
            .children(if phone {
                self.render_phone_touch_debug(self.shell_touches.len())
            } else {
                None
            })
            // The transient, pinned to the bottom edge of the window and
            // drawn over the buffer rather than in it or above it: nothing
            // in the surface reflows, and the point stays where the reader
            // left it, in view. The phone draws the same menu as a sheet
            // further down, so here it takes the keyboard and nothing else.
            .children(self.menu_buffer.as_ref().map(|open| {
                let holder = div()
                    .track_focus(&self.transient_focus)
                    .on_key_down(cx.listener(Self::menu_key));
                if phone {
                    return holder.into_any_element();
                }
                holder
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(bottom_strip(&text_style, cx).child(open.menu.render_rows(
                        &text_style,
                        cx,
                        |index, row| {
                            row.id(("desktop-transient-row", index))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.run_menu_at(index, window, cx);
                                    cx.stop_propagation();
                                }))
                                .into_any_element()
                        },
                    )))
                    .into_any_element()
            }))
            .children(
                match (
                    self.git_approval.render(&text_style, window, cx),
                    &self.minibuffer,
                ) {
                    (Some(approval), _) => Some(approval),
                    (None, Some(minibuffer)) => Some(if phone {
                        minibuffer.render_phone(&text_style, cx)
                    } else {
                        minibuffer.render(&text_style, cx)
                    }),
                    // A menu is drawn as a block in the buffer on the desk and
                    // as a sheet on the phone, and the sheet is drawn from
                    // here: nothing else in this method knows the phone has an
                    // overlay to draw. Without this arm the phone opens a menu
                    // nobody can see — the buffer has no block on purpose.
                    (None, None) if phone && self.menu_buffer.is_some() => {
                        self.render_phone_menu_sheet(&text_style, cx)
                    }
                    (None, None) => None,
                },
            )
            .into_any_element()
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The name a pasted picture gets: the clipboard carries no filename, and
/// Slack shows one.
fn extension(format: gpui::ImageFormat) -> &'static str {
    match format {
        gpui::ImageFormat::Jpeg => "jpg",
        gpui::ImageFormat::Webp => "webp",
        gpui::ImageFormat::Gif => "gif",
        _ => "png",
    }
}

/// `30m`, `2h`, `1d`; a bare number means minutes.
pub(crate) fn parse_duration_ms(text: &str) -> Option<u64> {
    let (digits, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "m"),
    };
    let count: u64 = digits.parse().ok()?;
    let minutes = match unit {
        "m" | "min" => count,
        "h" | "hr" => count.checked_mul(60)?,
        "d" => count.checked_mul(60 * 24)?,
        _ => return None,
    };
    minutes.checked_mul(60 * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_current_agent_roles() {
        for (role, expected) in [
            (AgentRole::default(), "med-eng"),
            (
                AgentRole::Engineer {
                    intelligence: EngineerIntelligence::Mini,
                },
                "mini-eng",
            ),
            (
                AgentRole::Engineer {
                    intelligence: EngineerIntelligence::High,
                },
                "high-eng",
            ),
            (
                AgentRole::Engineer {
                    intelligence: EngineerIntelligence::Medium1,
                },
                "med1-eng",
            ),
            (
                AgentRole::Engineer {
                    intelligence: EngineerIntelligence::High1,
                },
                "high1-eng",
            ),
            (
                AgentRole::Advisor {
                    intelligence: AdvisorIntelligence::Low,
                },
                "low-adv",
            ),
            (
                AgentRole::Advisor {
                    intelligence: AdvisorIntelligence::Medium,
                },
                "med-adv",
            ),
            (
                AgentRole::Advisor {
                    intelligence: AdvisorIntelligence::Medium1,
                },
                "med1-adv",
            ),
        ] {
            assert_eq!(agent_role_label(role), expected);
        }
    }
}
