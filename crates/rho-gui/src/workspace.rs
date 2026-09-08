//! Root entity: owns the attached daemons, the canonical agent states, the
//! registry, and one persistent [`AgentModel`] per opened agent.
//!
//! All protocol events flow through [`Workspace`]; queued frame runs are
//! merged per agent, and views receive summarized changes rather than the
//! protocol itself.
//!
//! Several daemons can be attached at once. Agent ids are
//! already unique across machines, so the client-side state stays keyed by
//! id alone; what the host is needed for is routing — which socket a command
//! travels down — and for the few places where a daemon-side *name* (a
//! repository path, a short agent label) is only unique within one machine.

#[path = "workspace_phone.rs"]
mod phone;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
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
use rho_agents::agent_view::AgentModel;
use rho_agents::create::{StartBase, cycle_agent_role_text, parse_agent_role, parse_start};
use rho_agents::draft::DraftModel;
use rho_agents::messages::MessageLog;
use rho_agents::session::ActiveAgents;
use rho_agents::store::FrameSummary;
use rho_agents::{
    AgentMap, DraftFieldClear, DraftFieldSubmit, DraftValueCycle, HostId, RoleCycle,
    RoleCycleGroup, TranscriptFrame,
};
use rho_core::ContentPart;
use rho_hosts::connection::{ConnEvent, Connection, GitApprovalDecision};
use rho_hosts::hosts::{HostStatus, Hosts};
#[cfg(test)]
use rho_ui_proto::AdvisorIntelligence;
use rho_ui_proto::{AgentId, AgentRole, ClientMessage, EngineerIntelligence, MessageDelivery};
use rho_window::selection::{ActivePane, Selection};
use rho_window::style::StyleClass;
use settings::Settings as _;
use theme::ActiveTheme as _;

use crate::chime::Chime;
use crate::desk_view::DeskCells;
use crate::minibuffer::{ECHO_DURATION, Echo, Minibuffer, bottom_strip};
use crate::pane::SurfaceKey;
use crate::search;

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
    AgentDone, AgentHide, AgentNew, AgentNext, AgentPrevious, BrowserExit, DashboardArchive,
    DashboardBack, DashboardCancelDraft, DashboardCycleGlobal, DashboardDealDone,
    DashboardDealExit, DashboardDealFile, DashboardDealMute, DashboardDealNext,
    DashboardDealRefresh, DashboardDealReply, DashboardDealRoomSnooze, DashboardDealSnooze,
    DashboardDealTodo, DashboardDeleteEmpty, DashboardDeleteRow, DashboardDemote, DashboardGoto,
    DashboardHeadingAbove, DashboardHeadingBelow, DashboardJump, DashboardMoveSubtreeDown,
    DashboardMoveSubtreeUp, DashboardNewChild, DashboardNewSibling, DashboardNow,
    DashboardPasteRow, DashboardPasteRowBefore, DashboardPromote, DashboardRenameTopic,
    DashboardReply, DashboardSubmit, DashboardToggleAgentTree, DashboardToggleSubagents,
    DashboardUndo, DashboardYankRow, DealCloseAndNext, DealOpen, FindNode, GitApprovalAllow,
    GitApprovalDeny, HomeOpenRow, MessagesOpen, MinibufferCancel, MinibufferComplete,
    MinibufferConfirm, MinibufferNext, MinibufferPrevious, OverviewToggle, PastePrompt, RailFocus,
    RailOpen, SearchRepeat, SearchRepeatReverse, ShellEof, ShellInterrupt, ShellPagerAll,
    ShellPagerMore, ShellPagerQuit, SlackCancelEdit, SlackCompose, SlackEditLast, SlackEditMessage,
    SlackMarkReadBefore, SlackNextUnread, SlackOpenRow, SlackReactTo, SlackSearch,
    SlackWatchChannel, SubmitPrompt, SurfaceBack, SurfaceClose, TaskBoard, TranscriptTop,
    UndoVerdict, UploadGuiTelemetry, VoiceToggle, ZulipLoadOlder, ZulipNextUnread, ZulipOpenRow,
};

const SHELL_SWIPE_DISTANCE: gpui::Pixels = px(64.);

/// The longest the dealer's signals go unexamined. A card's priority grows
/// with how long it has waited, so a wake is owed even when nothing on the
/// desk comes due.
const DEALER_SIGNAL_CEILING: Duration = Duration::from_secs(60);
/// The shortest wait between two examinations, so that a desk full of
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

/// A bind request waiting for the daemon's answer, and what the journal
/// should say when it comes back.
struct PendingGitApproval {
    request_id: u64,
    prompt: String,
    response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
}

#[derive(Clone)]
pub(crate) enum SurfaceView {
    Draft {
        editor: Entity<editor::Editor>,
    },
    Home(Entity<crate::home::HomeView>),
    Messages(Entity<editor::Editor>),
    Usage(Entity<crate::usage::UsageView>),
    DeskNode(Entity<editor::Editor>),
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
    Diff(Entity<rho_files::DiffView>),
    Terminal(Entity<rho_terminal::TerminalView>),
    Browser(Entity<rho_browser::PageView>),
    ZulipInbox(Entity<rho_zulip::ui::InboxView>),
    ZulipNarrow(Entity<rho_zulip::ui::NarrowView>),
    SlackList(Entity<rho_slack::ui::ListView>),
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
            Self::DeskNode(_) => SurfaceKind::Dashboard,
            Self::Transcript { .. } => SurfaceKind::Transcript,
            Self::File(_) => SurfaceKind::File,
            Self::Shell { .. } => SurfaceKind::Shell,
            Self::Diff(_) => SurfaceKind::Diff,
            Self::Terminal(_) => SurfaceKind::Terminal,
            Self::Browser(_) => SurfaceKind::Browser,
            Self::ZulipInbox(_) => SurfaceKind::ZulipInbox,
            Self::ZulipNarrow(_) => SurfaceKind::ZulipNarrow,
            Self::SlackList(_) => SurfaceKind::SlackList,
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
    /// Zulip's own window arrangement: entering it from the dashboard
    /// leaves the agent surface exactly as it was, and leaving it comes
    /// back to them.
    Zulip,
    /// Slack's own arrangement, on the same terms as Zulip's.
    Slack,
}

pub use rho_hosts::{AttachTarget, HostPath, HostSpec};

/// Where a host's events go from here: onto the model thread's queue, which
/// is the one place that decides what a frame means. `rho-hosts` knows only
/// that somebody is listening.
struct ModelSink(futures::channel::mpsc::UnboundedSender<rho_mirror::model::ToModel>);

impl rho_hosts::HostSink for ModelSink {
    fn send(&self, event: rho_hosts::HostEvent) -> Result<(), rho_hosts::SinkClosed> {
        self.0
            .unbounded_send(rho_mirror::model::ToModel::Event(event))
            .map_err(|_| rho_hosts::SinkClosed)
    }

    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

#[derive(Clone)]
struct PendingTreeVerdict {
    event: crate::dashboard::DealerEvent,
    echo: String,
    undo: VerdictUndo,
    phone_verdict: Option<rho_journal::PhoneVerdict>,
}

#[derive(Clone)]
struct VerdictUndo {
    sequence: u64,
    verb: String,
    state: VerdictUndoState,
}

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
    /// returns all the way down `space a s` and not one step of it.
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
) -> (rho_desk::cells::Timestamp, String) {
    match unit {
        SnoozeUnit::Minutes | SnoozeUnit::Hours => {
            let ahead = match unit {
                SnoozeUnit::Minutes => chrono::Duration::minutes(count),
                _ => chrono::Duration::hours(count),
            };
            let at = now + ahead;
            (
                rho_desk::cells::Timestamp {
                    unix_ms: at.timestamp_millis(),
                    precision: rho_desk::cells::TimestampPrecision::Millisecond,
                },
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
                crate::desk_view::day_timestamp(date),
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

#[derive(Clone)]
enum VerdictUndoState {
    /// The applied verdict this undo appends `Undone { of }` against.
    DeskVerdict {
        /// Boxed: the card dwarfs everything else an undo entry holds.
        card: Box<crate::dashboard::DealCard>,
        verdict: crate::dashboard::DealerVerdict,
        host: HostId,
        node: rho_desk::cells::Id,
        at: rho_desk::cells::Stamp,
    },
    /// The done verdicts one `mark read before` wrote. It closed a backlog
    /// in one keystroke, so it comes back in one: `shift-u` undoes every
    /// node it touched, not the last of them.
    MarkedReadBefore {
        host: HostId,
        nodes: Vec<(rho_desk::cells::Id, rho_desk::cells::Stamp)>,
    },
}

struct PendingTreeUndo {
    entry: VerdictUndo,
}

fn undo_sequence_insert_position(existing: impl Iterator<Item = u64>, sequence: u64) -> usize {
    existing
        .take_while(|candidate| *candidate < sequence)
        .count()
}

/// A structure verb, as the writes that put the desk back. Undo of a
/// creation is the note's deletion; undo of a deletion or a move is the
/// cell it replaced.
struct DeskSemanticUndo {
    host: HostId,
    writes: Vec<rho_desk::cells::CellWrite>,
}

pub struct Workspace {
    pub(crate) hosts: Hosts,
    /// The agents held whole: their events, the transcript folded from
    /// them, and the daemon's live tail. Also the focus set every host
    /// is told. Everyone else is a digest in the registry.
    active: ActiveAgents,
    /// Every transcript this client holds open, and the rendered state a
    /// screen draws from. `rho-agents` owns what a transcript is; the
    /// shell only says which agent and hands the rows on.
    transcripts: rho_agents::Transcripts,
    pub(crate) registry: AgentMap,
    /// Which pane the point is in. The window's, not the map's.
    pub(crate) selection: Selection,
    models: HashMap<AgentId, Entity<AgentModel>>,
    /// Weak project cache keyed by daemon-side workspace identity, qualified
    /// by host — the same repository path on two machines is two projects.
    /// Artifact surfaces hold the strong references; when the last file/diff
    /// closes, the remote channel and cache entry naturally expire.
    remote_projects: HashMap<
        (HostId, rho_ui_proto::WorkspaceInfo),
        gpui::WeakEntity<rho_files::RemoteProjectState>,
    >,
    pending_diff_loads: HashMap<AgentId, Task<()>>,
    /// Accumulated change summaries for materialized but hidden views; they
    /// render once, with the merged summary, when next selected.
    pending_syncs: HashMap<AgentId, FrameSummary>,
    /// What the main thread asks of the model thread: which hosts exist,
    /// and whose rows it wants. The journal cursor is the model's.
    model: futures_mpsc::UnboundedSender<rho_mirror::model::ToModel>,
    draft_model: Entity<DraftModel>,
    /// What rho has said, and the surface it says it on. The log owns its
    /// own buffer, editor and highlights; the host records a line and shows
    /// the surface.
    messages: Entity<MessageLog>,
    /// Launch arguments for a configured Desk staffing or quick-spawn. The
    /// transient edits these; the writable dashboard row owns the message.
    /// Where `n a` files the agent the draft page is composing. `None`
    /// is the root, which is also what an ordinary draft sends.
    draft_area: Option<(HostId, rho_desk::cells::Id)>,
    /// A NewAgent request from the draft is in flight; the draft buffer is
    /// kept intact until the daemon confirms creation, so a rejected request
    /// (bad working directory, say) never loses the message.
    /// Which host the pending draft agent was sent to, so its confirmation
    /// can be recognized and the compose surface reset.
    awaiting_draft_agent: Option<HostId>,
    /// A surface was opened to be written in and is waiting for the
    /// frame its focus lands in to enter insert. The action goes to the
    /// focused node of the frame already on screen, so dispatching
    /// before that frame types into the surface the reader is leaving.
    insert_when_shown: bool,
    /// The area the next agent this client asks for is filed under. The
    /// daemon never writes it: the agent exists because the registry says
    /// so, and where it is shown is the user's own fact.
    pending_agent_filing: Option<(HostId, rho_desk::cells::Id)>,
    /// Hosts that have delivered at least one `Ready`. A host attaches
    /// blind; until it answers, its agents do not exist for this client.
    ready_hosts: HashSet<HostId>,
    /// A routine registry refresh also sends `Ready`, so replay is armed
    /// separately and only by an actual disconnect.
    replay_hosts: HashSet<HostId>,
    /// Everything the usage screen is drawn from, and the screen itself:
    /// see [`crate::usage::Usage`].
    usage: crate::usage::Usage,
    duration_timer: Option<Task<()>>,
    /// Attention chime output; lazily opened on the first play.
    chime: Chime,
    /// Each context retains one viewport over its surfaces, and the stack
    /// of where the reader was in it. One machine, per context, because a
    /// back that changes context moves two things at once (eng-en1p's
    /// ruling under the Emacs rule; `RHO-WINDOW-DESIGN.md`).
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
    overview_open: bool,
    last_shift_tap: Option<std::time::Instant>,
    /// When `shift` went down on its own, if it is still down and still a
    /// candidate for a tap. Cleared the moment another key or modifier
    /// joins it, which is what makes holding it for a chord silent.
    shift_down_at: Option<std::time::Instant>,
    shell_touches: HashMap<TouchId, ShellTouchContact>,
    shell_touch_was_multi: bool,
    shell_touch_committed: bool,
    deal_gesture_active: bool,
    deal_controls_visible: bool,
    agent_last_interaction: HashMap<AgentId, i64>,
    dealer_signal_eval_scheduled: bool,
    /// Hosts whose desk is rebuilt on the next frame: rows arrive one
    /// `Log` at a time, and the rebuild walks every agent.
    desk_sync_pending: HashMap<HostId, Option<BTreeSet<AgentId>>>,
    _dealer_signal_task: Task<()>,
    lamp_on: bool,
    dealer_signals_initialized: bool,
    chime_above_threshold: bool,
    /// The dashboard: the rail as a real editor buffer, ambient chrome
    /// beside the active tree.
    pub(crate) dashboard: crate::dashboard::Dashboard,
    /// The vendored modal engine's status item, kept visible in Rho's frame.
    mode_indicator: Entity<vim::ModeIndicator>,
    /// Compact Helix-style key guide shown on deal entry and `?`.
    /// Canonical per-host CRDT Desk buffers shared by dashboard and source
    /// views.
    pub(crate) desk_cells: DeskCells,
    /// One note surface per node the reader has opened, kept so the body's
    /// cursor and scroll survive leaving and coming back.
    note_views: HashMap<(HostId, rho_desk::cells::Id), crate::note_view::NoteView>,
    /// Set by `InputHandled` and consumed by the following buffer edit. The
    /// editor announces input before mutating its buffer, while heading
    /// recognition must run immediately after that mutation so subsequent
    /// typing lands in the newly-created node buffer.
    pending_heading_recognition: Option<(HostId, rho_desk::cells::Id, usize)>,
    pending_heading_undo: Option<clock::Lamport>,
    pending_tree_verdicts: BTreeMap<(HostId, rho_desk::cells::Stamp), PendingTreeVerdict>,
    pending_tree_undos: BTreeMap<(HostId, rho_desk::cells::Stamp), PendingTreeUndo>,
    /// Text a paste owes its new notes, held until the daemon accepts the
    /// creation those notes came from.
    pub(crate) pending_desk_texts:
        BTreeMap<(HostId, rho_desk::cells::Stamp), Vec<(rho_desk::cells::Id, String)>>,
    verdict_undo: Vec<VerdictUndo>,
    next_verdict_undo_sequence: u64,
    desk_semantic_clipboard: Option<crate::desk_view::DeskCapture>,
    /// One-shot recovery for `p` while Vim still holds the removed excerpt.
    desk_semantic_paste_target: Option<(HostId, rho_desk::cells::Id)>,
    desk_semantic_undo: BTreeMap<clock::Lamport, DeskSemanticUndo>,
    pending_semantic_batches: BTreeMap<(HostId, rho_desk::cells::Stamp), clock::Lamport>,
    pub(crate) pending_semantic_group: Option<clock::Lamport>,
    /// Agent shown beside the dashboard cursor. Kept separate from the
    /// focused task so cursor previews do not rebuild or reorder the rail.
    dashboard_preview: Option<AgentId>,
    /// The browser pages the desk refers to, the ones on their way out, and
    /// the one shown in the right-hand preview card: see
    /// [`crate::browser::Pages`].
    pages: crate::browser::Pages,
    /// The Zulip client, started the first time its dashboard row is
    /// opened. Chat costs nothing until asked for.
    zulip: Option<Entity<rho_zulip::session::Session>>,
    pub(crate) slack: Option<Entity<rho_slack::session::Session>>,
    /// Set while the Slack session cannot be trusted to be current. It lights
    /// the lamp on its own, because nothing else in the queue knows.
    pub(crate) slack_degraded: Option<String>,
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
    pending_filing_destinations: Vec<(String, String, HostId, rho_desk::cells::Id)>,
    pending_filing_selected: Option<(HostId, rho_desk::cells::Id)>,
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
    git_approval_focus: gpui::FocusHandle,
    /// Focus beneath the single modal overlay. Transients, minibuffers, and
    /// Git approval hand this target between them so borrowing keyboard
    /// focus never changes dashboard/work mode.
    overlay_return_focus: Option<gpui::FocusHandle>,
    /// The last system notice, flashed in the bottom strip (emacs echo
    /// area). Cleared by its own timer or when the minibuffer opens.
    echo: Option<Echo>,
    pending_git_approval: Option<PendingGitApproval>,
    realtime_task: Option<Task<()>>,
    realtime_stop: Option<tokio::sync::oneshot::Sender<()>>,
    realtime_input_muted: Option<tokio::sync::watch::Sender<bool>>,
    voice_input_muted: bool,
    voice_session_enabled: bool,
    /// The daemon running the voice session; the session is torn down with
    /// that host.
    voice_host: Option<HostId>,
    _event_task: Task<()>,
    _dashboard_subscription: gpui::Subscription,
    _keystroke_subscription: gpui::Subscription,
    _window_activation_subscription: gpui::Subscription,
    phone: phone::PhoneUi,
}

/// Target-independent application state transitions. Transport adapters feed
/// these methods; native and browser layout code only decide when to render
/// the resulting canonical registry/store/model state.
/// Runs a closure when it is dropped, so a function that returns from
/// several places still records itself once.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(run) = self.0.take() {
            run();
        }
    }
}

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
            );
            let visualization_client = self
                .connection_for(agent_id)
                .map(Connection::visualization_client)
                .unwrap_or_else(rho_hosts::connection::VisualizationClient::detached);
            let model = cx.new(|cx| AgentModel::new(completions, visualization_client, cx));
            // The screen says when its transcript is composed; what that
            // means for the rest of the shell is decided here.
            self.agent_model_subscriptions.push(cx.subscribe_in(
                &model,
                window,
                |workspace, _, event, window, cx| match event {
                    rho_agents::agent_view::AgentModelEvent::Loaded(agent_id) => {
                        workspace.finish_initial_agent_load(*agent_id, cx);
                    }
                    rho_agents::agent_view::AgentModelEvent::HistoryComposed(agent_id) => {
                        workspace.finish_transcript_search(*agent_id, window, cx);
                    }
                    rho_agents::agent_view::AgentModelEvent::BodiesWanted {
                        agent_id,
                        positions,
                    } => {
                        // One request for the chunk, naming every position
                        // its calls came from. The daemon answers each one
                        // on its own, and each answer names what it answers.
                        let mut positions = positions.iter().copied();
                        if let Some(pos) = positions.next() {
                            workspace.send_to_agent(
                                *agent_id,
                                ClientMessage::Detail {
                                    agent_id: *agent_id,
                                    pos,
                                    more: positions.collect(),
                                },
                            );
                        }
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

    /// The workdirs this daemon offers: the labels in its store that carry
    /// a `Project`.
    fn refresh_workdirs(&mut self, host: HostId) {
        let projects = self
            .desk_cells
            .projects(host)
            .into_iter()
            .map(|(name, project)| (name, project.path))
            .collect();
        self.hosts.set_workdirs(host, projects);
    }

    fn apply_ready(&mut self, host: HostId, machine_seed: u64, agent_counter: u64) -> bool {
        let first_ready = self.ready_hosts.insert(host);
        self.registry
            .set_host_data(host, machine_seed, agent_counter);
        first_ready
    }

    /// What the model holds for a host is now all there is of it: the disk
    /// copy read at startup, or nothing after the copy started over.
    /// Whatever this client had of the host goes first.
    fn loaded(
        &mut self,
        host: HostId,
        agents: Vec<rho_agents::MirroredAgent>,
        verdicts: Vec<(AgentId, rho_agents::Verdict)>,
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
        let _ = self
            .model
            .unbounded_send(rho_mirror::model::ToModel::Command(
                rho_mirror::model::ModelCommand::Follow(self.followed()),
            ));
    }

    fn note_agent_created(&mut self, host: HostId, agent_id: AgentId) {
        self.registry.note_agent_created(host, agent_id);
    }

    /// Shows an agent's transcript from the mirror the client already
    /// holds: the fold, for a reader who opened it before any live frame,
    /// or with the daemon down. The live frame rides on its tail.
    fn seed_transcript_from_mirror(&mut self, agent_id: AgentId) -> bool {
        if self.transcripts.is_open(&agent_id) {
            return false;
        }
        // The disk copy may trail the rows just heard by a queued write;
        // waiting for it is what makes the fold whole.
        rho_mirror::mirror::flush();
        let events = rho_mirror::mirror::read_events(agent_id);
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
            rho_ui_proto::mirror::AgentPos,
            rho_ui_proto::mirror::MirrorEvent,
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
        let channels = rho_mirror::model::spawn();
        #[cfg(test)]
        let channels = rho_mirror::model::detached();
        let rho_mirror::model::ModelChannels { incoming, changes } = channels;
        let model_commands = incoming.clone();
        let hosts = Hosts::new(std::sync::Arc::new(ModelSink(incoming)));
        let workspace = cx.entity().downgrade();
        let mode_indicator = cx.new(|cx| vim::ModeIndicator::new(window, cx));
        let draft_model = cx.new(|cx| {
            DraftModel::new(
                rho_agents::draft::Hooks::new(move |editor, fields, _, _| {
                    editor.set_completion_provider(Some(
                        crate::commands::WorkspaceCompletionProvider::new(
                            workspace.clone(),
                            Some(fields.workdir),
                            Some(fields.role),
                            Some(fields.start),
                        ),
                    ));
                }),
                cx,
            )
        });
        let draft_subscription =
            cx.subscribe(&draft_model, |workspace, _, event, cx| match event {
                rho_agents::draft::Event::Edited => workspace.mark_draft_active_from_edit(cx),
            });
        let messages = cx.new(|cx| MessageLog::new(window, cx));
        let event_task = cx.spawn(async move |this, cx| {
            let mut changes: UnboundedReceiver<rho_mirror::model::ModelEvent> = changes;
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
        // The ranking only changes on its own when a dated thing comes
        // due, so the wake is keyed to the next one rather than to a
        // minute that is usually spent finding nothing has moved. A
        // priority still slides with the clock between two of those,
        // which is what the ceiling is for.
        let dealer_signal_task = cx.spawn(async move |this, cx| {
            loop {
                let wait = this
                    .read_with(cx, |this, _| {
                        this.dashboard
                            .next_deal_expiry(chrono::Local::now().fixed_offset())
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

        let dashboard = crate::dashboard::Dashboard::new(window, cx);
        // The preview follows the dashboard cursor: any local selection
        // change while the dashboard is focused re-aims the surface.
        let dashboard_subscription = cx.subscribe_in(
            dashboard.editor(),
            window,
            |this, _, event: &editor::EditorEvent, window, cx| match event {
                editor::EditorEvent::InputHandled { text, .. } if text.as_ref() == " " => {
                    if let Some((host, node_id, offset)) =
                        this.dashboard.tree_node_cursor_offset(cx)
                    {
                        this.pending_heading_recognition = Some((host, node_id, offset + 1));
                    }
                }
                editor::EditorEvent::BuffersEdited { .. } => {
                    if let Some((host, node_id, line_end)) = this.pending_heading_recognition.take()
                    {
                        // Replacing the editor composition from inside its
                        // BuffersEdited dispatch can leave the next queued
                        // keystroke attached to the row we just deleted.
                        // Reconcile at the end of this GPUI update instead,
                        // before another platform input event is dispatched.
                        cx.defer_in(window, move |this, window, cx| {
                            this.recognize_desk_note_after_edit(
                                host, node_id, line_end, window, cx,
                            );
                        });
                    }
                }
                editor::EditorEvent::SemanticRowAction { buffer_id, action } => {
                    this.handle_desk_semantic_row_action(*buffer_id, *action, window, cx);
                }
                editor::EditorEvent::Edited { .. } => {
                    if let Some(transaction_id) = this.pending_semantic_group.take() {
                        this.dashboard.group_until_transaction(transaction_id, cx);
                    }
                }
                editor::EditorEvent::TransactionUndone { transaction_id } => {
                    this.undo_desk_semantic_action(*transaction_id, window, cx);
                }
                editor::EditorEvent::SearchRequested { backwards } => {
                    this.prompt_dashboard_search(search::Direction::of(*backwards), window, cx);
                }
                editor::EditorEvent::SelectionsChanged { local: true } => {
                    this.refresh_dashboard(window, cx);
                    this.dashboard_cursor_moved(window, cx);
                }
                _ => {}
            },
        );
        let keystroke_subscription = cx.observe_keystrokes(|this, event, _window, _cx| {
            if this.desk_semantic_paste_target.is_some()
                && !event.keystroke.key.eq_ignore_ascii_case("p")
                && !matches!(
                    event.keystroke.key.as_str(),
                    "shift" | "control" | "alt" | "platform" | "function"
                )
            {
                this.desk_semantic_paste_target = None;
            }
            tracing::debug!(
                key = %event.keystroke.key,
                shift = event.keystroke.modifiers.shift,
                control = event.keystroke.modifiers.control,
                alt = event.keystroke.modifiers.alt,
                platform = event.keystroke.modifiers.platform,
                shift_down = this.shift_down_at.is_some(),
                "keystroke"
            );
            // A key arriving while `shift` is down means it was being held
            // for that key, so it was never a tap. `shift`'s own keystroke
            // is not such a key and never decides anything: a platform may
            // deliver it before it updates the modifier state, and deciding
            // here opened the menu on every press. Only modifiers-changed
            // decides a tap.
            let bare_shift = event.keystroke.key == "shift"
                && !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.alt
                && !event.keystroke.modifiers.platform;
            if !bare_shift {
                this.shift_down_at = None;
                this.last_shift_tap = None;
            }
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
            transcripts: rho_agents::Transcripts::default(),
            registry: AgentMap::default(),
            selection: Selection::default(),
            models: HashMap::new(),
            remote_projects: HashMap::new(),
            pending_diff_loads: HashMap::new(),
            pending_syncs: HashMap::new(),
            model: model_commands,
            draft_model,
            messages,
            draft_area: None,
            awaiting_draft_agent: None,
            insert_when_shown: false,
            pending_agent_filing: None,
            ready_hosts: HashSet::new(),
            replay_hosts: HashSet::new(),
            usage: crate::usage::Usage::default(),
            duration_timer: None,
            chime: Chime,
            history: None,
            surfaces: HashMap::new(),
            active_context: ContextId::Draft,
            overview_open: false,
            last_shift_tap: None,
            shift_down_at: None,
            shell_touches: HashMap::new(),
            shell_touch_was_multi: false,
            shell_touch_committed: false,
            deal_gesture_active: false,
            deal_controls_visible: false,
            agent_last_interaction: HashMap::new(),
            dealer_signal_eval_scheduled: false,
            desk_sync_pending: HashMap::new(),
            _dealer_signal_task: dealer_signal_task,
            lamp_on: false,
            dealer_signals_initialized: false,
            chime_above_threshold: false,
            dashboard,
            mode_indicator,
            desk_cells: DeskCells::new(crate::desk_view::desk_device()),
            note_views: HashMap::new(),
            pending_heading_recognition: None,
            pending_heading_undo: None,
            pending_tree_verdicts: BTreeMap::new(),
            pending_desk_texts: BTreeMap::new(),
            pending_tree_undos: BTreeMap::new(),
            verdict_undo: Vec::new(),
            next_verdict_undo_sequence: 0,
            desk_semantic_clipboard: None,
            desk_semantic_paste_target: None,
            desk_semantic_undo: BTreeMap::new(),
            pending_semantic_batches: BTreeMap::new(),
            pending_semantic_group: None,
            dashboard_preview: None,
            pages: crate::browser::Pages::default(),
            zulip: None,
            slack: None,
            slack_degraded: None,
            slack_labels: HashMap::new(),
            slack_reacting: None,
            slack_search_before: None,
            _slack_subscription: None,
            _slack_view_subscriptions: Vec::new(),
            _draft_subscription: draft_subscription,
            agent_model_subscriptions: Vec::new(),
            search: search::Search::default(),
            pending_filing_destinations: Vec::new(),
            pending_filing_selected: None,
            pending_find_target: None,
            find_snapshot: None,
            scroll_journal_task: None,
            minibuffer: None,
            transient_focus: cx.focus_handle(),
            menu_buffer: None,
            git_approval_focus: cx.focus_handle(),
            overlay_return_focus: None,
            echo: None,
            pending_git_approval: None,
            realtime_task: None,
            realtime_stop: None,
            realtime_input_muted: None,
            voice_input_muted: false,
            voice_session_enabled: false,
            voice_host: None,
            _event_task: event_task,
            _dashboard_subscription: dashboard_subscription,
            _keystroke_subscription: keystroke_subscription,
            _window_activation_subscription: window_activation_subscription,
            phone: phone::PhoneUi::new(cx),
        };
        for spec in specs {
            this.attach_host(spec, cx);
        }
        // A cold start lands on Home: what is running, what is next, and
        // what sits just under the line, without dealing a card.
        this.overview_open = false;
        let home = this.make_surface(SurfaceKey::Home, window, cx);
        this.display_surface(home, cx);
        this.refresh_home(cx);
        this.focus_active_surface(window, cx);
        // Seed the listing before any event arrives ("+ new agent").
        this.refresh_dashboard(window, cx);
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
        this
    }

    /// Attaches a daemon. The name is registered with the registry first so
    /// that labels and chrome can qualify by host from the moment the host
    /// exists, not only once it answers.
    pub(crate) fn attach_host(&mut self, spec: HostSpec, cx: &App) -> HostId {
        let (host, commands) = self.hosts.attach(spec.name.clone(), spec.target, cx);
        // The model is told the host exists, and how to speak to it, before
        // any frame from it can arrive.
        let _ = self
            .model
            .unbounded_send(rho_mirror::model::ToModel::Command(
                rho_mirror::model::ModelCommand::AttachHost {
                    host,
                    name: spec.name.clone(),
                },
            ));
        let _ = self
            .model
            .unbounded_send(rho_mirror::model::ToModel::Command(
                rho_mirror::model::ModelCommand::HostCommands { host, commands },
            ));
        self.registry.attach_host(host, spec.name);
        host
    }

    /// Forgets a daemon: its transcripts, surfaces, and cached projects go
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
        if self.voice_host == Some(host) {
            self.stop_voice();
        }
        self.hosts.detach(host);
        let _ = self
            .model
            .unbounded_send(rho_mirror::model::ToModel::Command(
                rho_mirror::model::ModelCommand::DetachHost(host),
            ));
        self.ready_hosts.remove(&host);
        self.replay_hosts.remove(&host);
        self.usage.forget_host(host);
        self.remote_projects.retain(|(owner, _), _| *owner != host);
        let gone = self.registry.detach_host(host);
        self.selection.forget(|agent_id| gone.contains(&agent_id));
        self.refresh_dashboard(window, cx);
        for agent_id in departed {
            // The agent is gone with its daemon, so its transcript is a
            // place that no longer exists: one call, and no context can
            // land on it again.
            self.forget_surface(&SurfaceKey::Transcript(agent_id));
            self.active.remove(agent_id);
            self.transcripts.forget(agent_id);
            self.models.remove(&agent_id);
            self.pending_syncs.remove(&agent_id);
            self.pending_diff_loads.remove(&agent_id);
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

    /// The daemon an agent lives on. `None` only before its first summary or
    /// creation notice has landed.
    fn host_of(&self, agent_id: AgentId) -> Option<HostId> {
        self.registry.host_of_agent(agent_id)
    }

    fn connection_for(&self, agent_id: AgentId) -> Option<&Connection> {
        self.hosts.connection(self.host_of(agent_id)?)
    }

    /// Routes an agent-scoped command to the daemon that owns the agent.
    /// Commands for an agent whose host is unknown or gone are dropped: the
    /// daemon that could act on it is not there to hear them.
    fn send_to_agent(&self, agent_id: AgentId, message: ClientMessage) {
        if let Some(connection) = self.connection_for(agent_id) {
            connection.send(message);
        }
    }

    pub(crate) fn send_to_host(&self, host: HostId, message: ClientMessage) {
        if let Some(connection) = self.hosts.connection(host) {
            connection.send(message);
        }
    }

    /// Whether the daemon behind an agent is answering. Acting on an agent
    /// whose own host is down must fail even when other hosts are fine.
    fn agent_online(&self, agent_id: AgentId) -> bool {
        self.host_of(agent_id)
            .is_some_and(|host| self.hosts.is_online(host))
    }

    /// Any daemon answering: the precondition for actions that choose their
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
        self.overview_open = false;
        // A dealt note's surface is the dashboard's own editor with the point
        // moved to the node; nothing else about it is on screen. Stepping to
        // it out of history has to put the point back the way `open_card`
        // put it there, or the reader is told the surface changed and shown
        // the rows they were already reading.
        if let SurfaceKey::DeskNode { host, node_id } = &surface.key
            && let SurfaceView::DeskNode(editor) = &surface.view
            && editor.entity_id() == self.dashboard.editor().entity_id()
        {
            self.dashboard
                .move_to_tree_node_when_ready(*host, node_id.clone());
            // The pending cursor is consumed by the next composition, and a
            // step through history is not otherwise one; `open_card` gets its
            // composition from the deal that follows it.
            self.refresh_dashboard(window, cx);
        }
        self.ensure_surface_subscription(&surface.key, cx);
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: Self::journal_surface(&surface.key),
            method: rho_journal::SurfaceShowMethod::Mru,
        });
        cx.notify();
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
        if self.dashboard.is_focused(window, cx)
            && matches!(
                self.dashboard.cursor_target(&self.registry, cx),
                Some(
                    crate::dashboard::RowTarget::NewDraft
                        | crate::dashboard::RowTarget::NewTreeDraft(_)
                )
            )
            && self.dashboard.discard_new_draft(cx)
        {
            self.forget_discarded_draft(window, cx);
            self.refresh_dashboard(window, cx);
            return;
        }
        // The overview and Home are both floors: there is nothing under
        // them to reveal, so `q` on either stays put.
        if self.overview_open || self.active_surface().key == SurfaceKey::Home {
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

    fn forget_discarded_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft_area = None;
        let key = SurfaceKey::Draft;
        rho_journal::record(rho_journal::Event::SurfaceClosed {
            surface: Self::journal_surface(&key),
            dealt_untouched: false,
        });
        self.forget_surface(&key);
        // A discarded draft is a place that no longer exists, and the
        // reader may be standing on it: whatever the overview is doing over
        // the top, what is under it must be somewhere real.
        if self.active_surface().key == key {
            // The reader is looking at the overview, not at what is under
            // it, so moving the surface underneath must not pull the
            // overview down with it.
            let overview = self.overview_open;
            if !self.show_previous_surface(window, cx) {
                self.open_home(window, cx);
                self.active_pane_mut().forget(&key);
            }
            self.overview_open = overview;
        }
        cx.notify();
    }

    /// Home is the front door: a cold start, an emptied queue, and the
    /// overview key all land here.
    pub(crate) fn open_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overview_open = false;
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
        let rows = self.home_rows(cx);
        view.update(cx, |view, cx| view.set_rows(rows, cx));
    }

    fn home_rows(&mut self, cx: &mut Context<Self>) -> crate::home::HomeRows {
        let now = chrono::Local::now().fixed_offset();
        let hand = self
            .dashboard
            .dealer_hand(now, &self.agent_last_interaction);
        let registry = &self.registry;
        let mut rows = crate::home::split_hand(&hand.cards, |card| {
            crate::home::card_title(card, |agent_id| registry.agent_id_label(agent_id))
        });
        let now_ms = now.timestamp_millis();
        let mut running = self
            .registry
            .known_agents()
            .copied()
            .filter(|agent_id| self.registry.agent_facts(*agent_id).turn_running)
            .collect::<Vec<_>>();
        running.sort_by_key(|agent_id| self.registry.agent_id_label(*agent_id));
        rows.running = running
            .into_iter()
            .map(|agent_id| {
                let facts = self.registry.agent_facts(agent_id);
                crate::home::RunningRow {
                    agent_id,
                    name: self.registry.agent_id_label(agent_id),
                    // Where it is filed, not the whole path: the row is
                    // about the agent, and the leaf is what names the work.
                    topic: self
                        .dashboard
                        .breadcrumb_for_agent(agent_id, cx)
                        .and_then(|path| path.rsplit(" › ").next().map(str::to_owned))
                        .unwrap_or_default(),
                    elapsed: crate::home::elapsed_label(
                        facts.last_user_message_at.0 as i64,
                        now_ms,
                    ),
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
        let Some(card) = self.hand(cx).card(&wanted).cloned() else {
            return;
        };
        self.open_card(card, window, cx);
        self.refresh_dashboard(window, cx);
    }

    pub(crate) fn open_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overview_open = true;
        self.refresh_dashboard(window, cx);
        window.focus(&self.dashboard.focus_handle(cx), cx);
        rho_journal::record(rho_journal::Event::OverviewOpened);
        cx.notify();
    }

    fn toggle_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overview_open {
            self.overview_open = false;
            self.focus_active_surface(window, cx);
            cx.notify();
        } else if self.active_surface().key == SurfaceKey::Home {
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
        if self.overview_open {
            return;
        }
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
        if self.overview_open || self.active_pane().at_newest() {
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

    fn journal_card_identity(
        identity: &crate::dashboard::DealCardId,
    ) -> rho_journal::DealerCardIdentity {
        rho_journal::DealerCardIdentity {
            host: identity.host.0,
            node_id: identity.node_id.clone().into(),
        }
    }

    /// One desk rebuild per frame for a host's rows. A catch-up says
    /// nothing until it reaches the head, so this never runs per page.
    ///
    /// `moved` names the agents a `Changed` moved, and `None` asks for the
    /// whole desk: a `Loaded`, a host reset, or a desk delta, where what is
    /// on the desk at all can be different. Scopes merge, and a whole one
    /// swallows the rest.
    fn schedule_desk_sync(
        &mut self,
        host: HostId,
        moved: Option<Vec<AgentId>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scheduled = self.desk_sync_pending.contains_key(&host);
        let pending = self
            .desk_sync_pending
            .entry(host)
            .or_insert_with(|| Some(BTreeSet::new()));
        match (pending.as_mut(), moved) {
            (Some(pending), Some(moved)) => pending.extend(moved),
            (Some(_), None) => *pending = None,
            (None, _) => {}
        }
        if scheduled {
            return;
        }
        cx.on_next_frame(window, move |this, window, cx| {
            let moved = this.desk_sync_pending.remove(&host).flatten();
            this.sync_tree_rows(host, moved.as_ref(), window, cx);
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
        let now = chrono::Local::now().fixed_offset();
        let mut candidates = self
            .dashboard
            .dealer_hand(now, &self.agent_last_interaction);
        // What the lamp is about is what is *not* in front of the reader:
        // the card they are already reading is not news.
        candidates
            .cards
            .retain(|card| match &self.active_surface().key {
                SurfaceKey::Transcript(agent_id) => {
                    Some(card.identity.clone()) != self.dashboard.agent_card_id(*agent_id)
                }
                SurfaceKey::Browser(page) => {
                    Some(card.identity.clone()) != self.dashboard.page_card_id(*page)
                }
                _ => true,
            });
        // Home is a window onto this same ranking, so it is rebuilt wherever
        // the dealer is invalidated and never on a timer.
        self.refresh_home(cx);
        let top = candidates.cards.first();
        if self.phone.enabled && top.is_some() {
            self.phone.feed_retry = true;
        }
        let max_priority = top.map(|card| card.priority);
        let card = top.map(|card| Self::journal_card_identity(&card.identity));
        let mut lamp_on =
            max_priority.is_some_and(|priority| priority >= crate::dashboard::LAMP_THRESHOLD);
        // A Slack session that has lost touch is worth the lamp on its own:
        // the queue cannot rank a mention nobody has received yet.
        {
            lamp_on = lamp_on || self.slack_degraded.is_some();
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
            max_priority.is_some_and(|priority| priority >= crate::dashboard::CHIME_THRESHOLD);
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
            ContextId::Zulip | ContextId::Slack => true,
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
        events: Vec<rho_mirror::model::ModelEvent>,
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
        for rho_mirror::model::ModelEvent { host, msg } in events {
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
        msg: rho_mirror::model::ModelMsg,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match msg {
            rho_mirror::model::ModelMsg::Loaded { agents, verdicts } => {
                self.loaded(host, agents, verdicts);
                self.refresh_deal_cards(host, crate::dashboard::DealScope::Whole, cx);
                self.refresh_dashboard(window, cx);
                self.schedule_desk_sync(host, None, window, cx);
                cx.notify();
            }
            rho_mirror::model::ModelMsg::Changed { agents } => {
                let changed = self.registry.told(agents);
                if changed.is_empty() {
                    return;
                }
                // The log is a source of rows, not only of facts: an agent
                // that has just asked for the user is on the map for its own
                // sake, so the tree the dealer reads has to be made again.
                // Only for the agents that moved: the rest of the desk is
                // what it was.
                // The cards these agents own are made again here, not when
                // the frame gets round to the map: a fact that has moved is
                // exactly when a card is made, and everything that reads the
                // ranking in between must see it.
                self.refresh_deal_cards(host, crate::dashboard::DealScope::Agents(&changed), cx);
                self.schedule_desk_sync(host, Some(changed), window, cx);
            }
            rho_mirror::model::ModelMsg::Rows { agent_id, rows } => {
                self.refold_open_transcript(agent_id, &rows, window, cx);
            }
            rho_mirror::model::ModelMsg::Event(event) => self.handle_event(host, event, window, cx),
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
            ConnEvent::DeskSynced {
                node_namespace,
                delta,
                bodies,
            } => {
                let (again, delta) =
                    self.desk_cells
                        .synced(host, node_namespace, delta, bodies, cx);
                if let Some(again) = again {
                    self.send_to_host(host, again);
                }
                self.sync_tree_delta(host, &delta, window, cx);
                self.carry_over_captures(host, window, cx);
            }
            ConnEvent::DeskCellsAvailable { frontier } => {
                if let Some(sync) = self.desk_cells.cells_available(host, frontier) {
                    self.send_to_host(host, sync);
                }
            }
            ConnEvent::DeskResyncRequired => {
                let sync = self.desk_cells.resync_required(host);
                self.send_to_host(host, sync);
            }
            ConnEvent::DeskMutationAccepted { stamp } => {
                // The cells are already in the view and the map: the write
                // that made them went through this client. Nothing about
                // the map has moved, so nothing about it is drawn again.
                self.desk_cells.mutation_accepted(host, stamp);
                self.complete_desk_mutation(host, stamp, window, cx);
            }
            ConnEvent::DeskMutationRejected { stamp, reason } => {
                self.desk_cells.mutation_rejected(host, stamp, cx);
                self.reject_desk_mutation(host, stamp, cx);
                self.sync_tree_dashboard(host, window, cx);
                self.notice_on(None, &format!("desk: {reason}"), StyleClass::SystemInfo, cx);
            }
            ConnEvent::DeskTextApplied { id, operation } => {
                // A body edit from another device moves that note's words
                // and the breadcrumbs made of them, which is its subtree
                // and nothing else. Where the rows sit does not move, so
                // nothing is composed.
                self.desk_cells
                    .text_applied(host, id.clone(), operation, cx);
                let touched = self
                    .dashboard
                    .subtree_ids(host, &id)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let delta = crate::desk_view::DeskDelta {
                    touched,
                    shape: false,
                };
                self.sync_tree_delta(host, &delta, window, cx);
            }
            ConnEvent::Ready {
                auth,
                machine_seed,
                agent_counter,
                journal_head: _,
            } => {
                self.replay_hosts.remove(&host);
                // The model asked for the journal this client lacks before
                // this arrived: the cursor is its to keep.
                let first_ready = self.apply_ready(host, machine_seed, agent_counter);
                self.prune_contexts();
                self.refresh_workdirs(host);
                if let Some(entry) = self.hosts.get_mut(host) {
                    entry.auth = Some(auth);
                }
                self.hosts.set_status(host, HostStatus::Online);
                self.refresh_draft_agent_targets(cx);
                if first_ready && matches!(self.selection.active_pane(), ActivePane::Startup) {
                    // The startup scaffold guessed before daemon data existed;
                    // refresh it now that workdir names and topics are known.
                    self.seed_draft(false, window, cx);
                }
                // The focus set is this client's to keep; a daemon that
                // just came up is told it whole.
                self.send_agent_focus_to(host);
                self.update_statuses(cx);
                self.dashboard_cursor_moved(window, cx);
                cx.notify();
            }
            ConnEvent::AuthState(auth) => {
                if let Some(entry) = self.hosts.get_mut(host) {
                    entry.auth = Some(auth);
                }
                if let Some(view) = self.usage.opened_view() {
                    let history = self.hosts.merged_quota_history();
                    let active = self.hosts.active_quota_namespaces();
                    view.update(cx, |view, cx| view.quota_arrived(history, active, cx));
                }
                cx.notify();
            }
            ConnEvent::AgentCreated { agent_id } => {
                self.note_agent_created(host, agent_id);
                if let Some((filing_host, area)) = self.pending_agent_filing.take()
                    && filing_host == host
                {
                    let writes = vec![rho_desk::cells::CellWrite {
                        id: rho_desk::cells::Id::Agent(agent_id),
                        property: filing_property(area),
                    }];
                    self.apply_desk_writes(host, writes, None, window, cx);
                }
                if self.awaiting_draft_agent == Some(host) {
                    self.awaiting_draft_agent = None;
                    self.activate_agent(agent_id, cx);
                    // The draft became this agent: reset the compose surface
                    // and follow the new agent.
                    let label = self
                        .draft_default_workdir()
                        .map(|path| self.hosts.workdir_label(&path))
                        .unwrap_or_default();
                    self.draft_model.update(cx, |view, cx| {
                        view.set_body_text("", cx);
                        view.clear_attachments(cx);
                        view.set_workdir_text(&label, cx);
                        view.set_role_text(rho_agents::create::DEFAULT_ROLE, cx);
                        view.set_start_text(rho_agents::create::DEFAULT_START, cx);
                    });
                    self.select_agent(Some(agent_id), window, cx);
                }
                cx.notify();
            }
            ConnEvent::Live { agent_id, live } => {
                self.handle_frame_batch(vec![(agent_id, TranscriptFrame::Live(live))], window, cx);
            }
            ConnEvent::Many(events) => {
                for event in events {
                    self.handle_event(host, event, window, cx);
                }
            }
            // Rows never reach the main thread as rows: the model folds
            // them and says which agents moved.
            ConnEvent::Log { .. } => {}
            // The bodies a composed chunk asked for. A chunk that went
            // away while its answer was in flight is not waiting for it,
            // and the model drops it.
            ConnEvent::Detail {
                agent_id,
                pos,
                body,
            } => {
                if let rho_ui_proto::mirror::DetailBody::Results(results) = body
                    && let Some(model) = self.models.get(&agent_id).cloned()
                {
                    model.update(cx, |model, cx| {
                        model.splice_results(pos, &results, now_ms(), cx);
                    });
                }
            }
            ConnEvent::ChatGptUsage {
                used_percent,
                reset_at_unix,
            } => {
                self.hosts.set_quota_summaries(
                    host,
                    vec![rho_ui_proto::QuotaSummary {
                        model: "gpt".to_owned(),
                        auth_namespace: None,
                        remaining_percent: 100u8
                            .saturating_sub(used_percent.clamp(0.0, 100.0).round() as u8),
                        burn_10m: 0,
                        burn_2h: 0,
                        burn_1d: 0,
                        burn_3d: 0,
                        reset_at_unix: Some(reset_at_unix),
                    }],
                );
                cx.notify();
            }
            ConnEvent::QuotaUsage(summaries) => {
                self.hosts.set_quota_summaries(host, summaries);
                cx.notify();
            }
            ConnEvent::QuotaHistory(series) => {
                self.hosts.set_quota_history(host, series);
                if let Some(view) = self.usage.opened_view() {
                    let history = self.hosts.merged_quota_history();
                    let active = self.hosts.active_quota_namespaces();
                    view.update(cx, |view, cx| view.quota_arrived(history, active, cx));
                }
                cx.notify();
            }
            ConnEvent::GlobalUsage(series) => {
                self.usage.record_global(host, series);
                if let Some(view) = self.usage.opened_view() {
                    let usage = self.usage.merged_global();
                    view.update(cx, |view, cx| view.global_usage_arrived(usage, cx));
                }
                cx.notify();
            }
            ConnEvent::AgentCostDistribution(series) => {
                self.usage.record_agent_cost(host, series);
                if let Some(view) = self.usage.opened_view() {
                    let usage = self.usage.merged_agent_cost();
                    view.update(cx, |view, cx| view.agent_cost_arrived(usage, cx));
                }
                cx.notify();
            }
            ConnEvent::TurnCancelled => {
                // Cancellation is an acknowledgement for an in-flight action,
                // not transcript content. The system notice buffer is
                // intentionally persistent, so rendering it there leaves
                // "[turn cancelled]" visible forever.
            }
            ConnEvent::ServerError(message) => {
                // A failed creation keeps the draft buffers; the user fixes
                // the workdir and submits again. The daemon's whole cause is
                // what the draft shows, so the reason a creation refused is
                // readable for longer than an echo.
                let refused_draft = self.awaiting_draft_agent == Some(host);
                if refused_draft {
                    self.awaiting_draft_agent = None;
                }
                let source = self.error_source(host);
                let text = format!("[{source} error: {message}]");
                if refused_draft {
                    self.refuse_draft(&text, cx);
                } else {
                    self.notice_on(None, &text, StyleClass::SystemImportant, cx);
                }
            }
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
                // The Desk handshake belongs to the connection, not to the
                // window: a reconnect asks only for what it is missing.
                let sync = self.desk_cells.sync(host);
                self.send_to_host(host, sync);
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
                let had_git_approval = if let Some(pending) = self.pending_git_approval.take() {
                    let _ = pending.response.send(GitApprovalDecision::Done);
                    true
                } else {
                    false
                };
                if had_git_approval {
                    self.finish_overlay_focus(window, cx);
                }
                // The host's agents stay in the rail with their retained
                // transcripts: losing a connection is not losing the work.
                // Only detaching (`space h d`) forgets a daemon.
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
                if self.voice_host == Some(host) {
                    self.stop_voice();
                }
                self.update_statuses(cx);
                cx.notify();
            }
            ConnEvent::GitTransportApproval {
                request_id,
                prompt,
                response,
            } => {
                if self.minibuffer.is_some() || self.pending_git_approval.is_some() {
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
                self.pending_git_approval = Some(PendingGitApproval {
                    request_id,
                    prompt,
                    response,
                });
                self.capture_overlay_focus(window, cx);
                window.focus(&self.git_approval_focus, cx);
                self.echo = None;
                cx.notify();
            }
            ConnEvent::GitTransportDone { request_id } => {
                if self
                    .pending_git_approval
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request_id)
                {
                    if let Some(pending) = self.pending_git_approval.take() {
                        let _ = pending.response.send(GitApprovalDecision::Done);
                    }
                    self.finish_overlay_focus(window, cx);
                    cx.notify();
                }
            }
        }
        // Every daemon event funnels through here, so this one call is
        // the event-driven replacement for reconciling on render.
        self.refresh_dashboard(window, cx);
    }

    /// How a daemon names itself in error text: bare when it is the only
    /// one, otherwise by host.
    fn error_source(&self, host: HostId) -> String {
        match self.hosts.len() > 1 {
            true => format!("rho daemon {}", self.hosts.host_label(host)),
            false => "rho daemon".to_owned(),
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
        if matches!(self.active_surface().view, SurfaceView::ZulipNarrow(_)) {
            self.zulip_submit(cx);
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
        if self.realtime_task.is_some() {
            self.voice_input_muted = !self.voice_input_muted;
            if let Some(input_muted) = &self.realtime_input_muted {
                input_muted.send_replace(self.voice_input_muted);
            }
            let message = match self.voice_input_muted {
                true => "voice microphone muted",
                false => "voice microphone unmuted",
            };
            self.notice_on(None, message, StyleClass::SystemInfo, cx);
            return;
        }
        self.voice_input_muted = false;
        // Voice follows what the user is looking at: start on the selected
        // agent's daemon.
        let host = self
            .selection
            .selected_agent()
            .and_then(|agent_id| self.host_of(agent_id))
            .filter(|host| self.hosts.is_online(*host))
            .or_else(|| self.hosts.primary());
        self.start_voice(host, cx);
    }

    pub(crate) fn cmd_end_voice(&mut self, cx: &mut Context<Self>) {
        self.voice_session_enabled = false;
        if self.realtime_task.is_some() {
            self.stop_voice();
            self.notice_on(None, "ending voice session…", StyleClass::SystemInfo, cx);
        } else {
            self.notice_on(None, "voice is not active", StyleClass::SystemInfo, cx);
        }
    }

    fn stop_voice(&mut self) {
        if let Some(stop) = self.realtime_stop.take() {
            let _ = stop.send(());
        }
    }

    fn start_voice(&mut self, host: Option<HostId>, cx: &mut Context<Self>) {
        if self.realtime_task.is_some() {
            return;
        }
        let Some(host) = host.or(self.voice_host).or_else(|| self.hosts.primary()) else {
            self.notice_on(
                None,
                "voice: no daemon attached",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.voice_host = Some(host);
        self.voice_session_enabled = true;
        let Some(connection) = self.hosts.connection(host) else {
            return;
        };
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let (input_muted, input_muted_rx) = tokio::sync::watch::channel(self.voice_input_muted);
        let task = connection.start_native_realtime(stop_rx, input_muted_rx, cx);
        self.realtime_stop = Some(stop);
        self.realtime_input_muted = Some(input_muted);
        let starting = match self.hosts.len() > 1 {
            true => format!("starting voice on {}…", self.hosts.host_label(host)),
            false => "starting voice…".to_owned(),
        };
        self.notice_on(None, &starting, StyleClass::SystemInfo, cx);
        self.realtime_task = Some(cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!("realtime task failed: {error}")),
            };
            if result.is_err() {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
            }
            let _ = this.update(cx, |this, cx| {
                this.realtime_task = None;
                this.realtime_stop = None;
                this.realtime_input_muted = None;
                let message = match result {
                    Ok(()) => "voice stopped listening".to_owned(),
                    Err(error) => format!("voice failed: {error:#}"),
                };
                this.notice_on(None, &message, StyleClass::SystemInfo, cx);
                let host = this.voice_host.filter(|host| this.hosts.is_online(*host));
                if host.is_some() && this.voice_session_enabled {
                    this.start_voice(host, cx);
                }
            });
        }));
    }

    /// `enter` on the dashboard's Zulip row: switch to the Zulip context
    /// and show its inbox. The client starts on first entry.
    pub(crate) fn open_zulip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.zulip_session(cx);
        self.active_context = ContextId::Zulip;
        let surface = self.make_surface(SurfaceKey::ZulipInbox, window, cx);
        self.display_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    fn zulip_session(&mut self, cx: &mut Context<Self>) -> Entity<rho_zulip::session::Session> {
        self.zulip
            .get_or_insert_with(|| cx.new(rho_zulip::session::Session::new))
            .clone()
    }

    /// The host services the Zulip surfaces borrow: editor chrome and the
    /// transcript's Markdown pipeline, so chat reads like every other
    /// buffer in the frame.
    fn zulip_hooks() -> rho_zulip::ui::Hooks {
        rho_zulip::ui::Hooks {
            configure_editor: rho_window::editor_config::configure,
            configure_markdown: rho_window::markdown::configure_buffer,
        }
    }

    /// Shows one Zulip conversation, marking the conversation being left
    /// as read — a Gnus summary buffer's exit, which is what makes `n`
    /// walk unreads down to nothing.
    pub(crate) fn open_zulip_narrow(
        &mut self,
        narrow: rho_zulip::Narrow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.leave_zulip_narrow(cx);
        let key = SurfaceKey::ZulipNarrow {
            label: narrow.label(),
        };
        self.active_context = ContextId::Zulip;
        let surface = match self.find_surface(|surface| surface.key == key).cloned() {
            Some(surface) => surface,
            None => {
                let session = self.zulip_session(cx);
                let hooks = Self::zulip_hooks();
                let view =
                    cx.new(|cx| rho_zulip::ui::NarrowView::new(session, narrow, hooks, window, cx));
                Self::wrap_surface(key, SurfaceView::ZulipNarrow(view))
            }
        };
        self.display_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Marks the conversation on screen read, if one is.
    fn leave_zulip_narrow(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::ZulipNarrow(view) = &self.active_surface().view {
            view.clone().update(cx, |view, cx| view.mark_read(cx));
        }
    }

    /// `enter` inside the Zulip inbox: open the conversation under the
    /// cursor.
    fn zulip_open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let SurfaceView::ZulipInbox(view) = &self.active_surface().view else {
            return;
        };
        let Some(narrow) = view.clone().update(cx, |view, cx| view.cursor_narrow(cx)) else {
            return;
        };
        self.open_zulip_narrow(narrow, window, cx);
    }

    /// The reading loop: the next unread conversation anywhere, marking
    /// the one being left as read. With nothing unread it returns to the
    /// inbox rather than sitting on a read conversation.
    fn zulip_next_unread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.zulip.clone() else {
            return;
        };
        let current = match &self.active_surface().view {
            SurfaceView::ZulipNarrow(view) => Some(view.read(cx).narrow().clone()),
            _ => None,
        };
        let next = session.read(cx).next_unread(current.as_ref());
        match next {
            Some(narrow) => self.open_zulip_narrow(narrow, window, cx),
            None => {
                self.leave_zulip_narrow(cx);
                self.open_zulip(window, cx);
            }
        }
    }

    /// `P`: page further back in the conversation on screen.
    fn zulip_load_older(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::ZulipNarrow(view) = &self.active_surface().view {
            view.clone().update(cx, |view, cx| view.load_older(cx));
        }
    }

    /// `enter` in a Zulip conversation: send the composed message.
    fn zulip_submit(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::ZulipNarrow(view) = &self.active_surface().view {
            view.clone().update(cx, |view, cx| view.submit(cx));
        }
    }

    fn shell_eof(&mut self, _: &ShellEof, _: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_surface().view {
            model.clone().update(cx, |model, cx| model.eof(cx));
        }
    }

    fn shell_pager_action(
        &mut self,
        action: rho_ui_proto::shell::PagerAction,
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
                "not connected to rho-daemon",
                StyleClass::SystemImportant,
                cx,
            );
            return;
        }
        self.send_to_agent(
            agent_id,
            ClientMessage::SendUserMessage {
                agent_id,
                content,
                delivery: MessageDelivery::NextRequest,
            },
        );
        // Engagement bump: keeps display-time staleness correct between
        // topic refreshes (the daemon persists the same timestamp).
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
            workspace: agent
                .and_then(|agent_id| self.registry.agent_workspace(agent_id))
                .cloned(),
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
    /// daemon confirms creation.
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
            self.refuse_draft("not connected to rho-daemon", cx);
            return;
        }
        let field = self.draft_model.read(cx).workdir_text(cx).trim().to_owned();
        let working_directory = if field.is_empty() {
            self.draft_default_workdir()
        } else {
            match rho_agents::create::resolve_workdir(&self.hosts, &field) {
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
        self.awaiting_draft_agent = Some(host);
        // `n a` chose an area, and that is where the agent is filed; an
        // ordinary draft has none and starts at the root.
        self.pending_agent_filing = self
            .draft_area
            .take()
            .and_then(|(area_host, node_id)| (area_host == host).then_some((host, node_id)));
        self.hosts.send(
            host,
            ClientMessage::NewAgent {
                role,
                start,
                content: Some(content),
            },
        );
    }

    fn paste_prompt(&mut self, _: &PastePrompt, window: &mut Window, cx: &mut Context<Self>) {
        self.cmd_paste_prompt(window, cx);
    }

    pub(crate) fn cmd_paste_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let dashboard_mode = self.dashboard_mode(window, cx);
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
            let editor = if dashboard_mode {
                self.dashboard.editor().clone()
            } else {
                self.active_editor(cx)
            };
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

    pub(crate) fn cmd_clear_prompt_attachments(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cleared = if self.dashboard_mode(window, cx) {
            false
        } else {
            match &self.active_surface().view {
                SurfaceView::SlackConversation(_) => self.slack_clear_attachment(cx),
                SurfaceView::Draft { .. } => self
                    .draft_model
                    .update(cx, |model, cx| model.clear_attachments(cx)),
                SurfaceView::Transcript { model, .. } => {
                    model.update(cx, |model, cx| model.clear_attachments(cx))
                }
                _ => false,
            }
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
                "not connected to rho-daemon",
                StyleClass::SystemInfo,
                cx,
            );
        }
        self.connected()
    }

    /// The selected agent's daemon must be answering for an agent-scoped
    /// command to mean anything; another host being up is no help.
    fn require_agent_online(&mut self, agent_id: AgentId, cx: &mut Context<Self>) -> bool {
        if !self.agent_online(agent_id) {
            let host = self
                .host_of(agent_id)
                .map(|host| self.hosts.host_label(host))
                .unwrap_or_else(|| "its daemon".to_owned());
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
            self.send_to_agent(agent_id, ClientMessage::CancelTurn { agent_id });
        }
    }

    pub(crate) fn cmd_rewind(&mut self, turns: u32, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("rewind", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(agent_id, ClientMessage::RewindAgent { agent_id, turns });
        }
    }

    pub(crate) fn cmd_continue_turn(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("continue", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(agent_id, ClientMessage::ContinueTurn { agent_id });
        }
    }

    pub(crate) fn cmd_compact(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.subject_agent_or_notice("compact", window, cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(
                agent_id,
                ClientMessage::CompactAgent {
                    agent_id,
                    delivery: rho_ui_proto::MessageDelivery::NextRequest,
                },
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
            self.send_to_agent(agent_id, ClientMessage::ChangePromptCacheKey { agent_id });
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
            ClientMessage::ChangeAgentRole {
                agent_id,
                role: AgentRole::Engineer { intelligence },
            },
        );
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
                    EngineerIntelligence::Low
                    | EngineerIntelligence::Cheap
                    | EngineerIntelligence::Medium
                    | EngineerIntelligence::High,
            } => &["eng-low", "eng-cheap", "eng", "eng-high"],
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Ultra | EngineerIntelligence::Alt,
            } => &["eng-ultra", "eng-alt"],
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
                    "eng-low" => Some(EngineerIntelligence::Low),
                    "eng-cheap" => Some(EngineerIntelligence::Cheap),
                    "eng" => Some(EngineerIntelligence::Medium),
                    "eng-high" => Some(EngineerIntelligence::High),
                    "eng-ultra" => Some(EngineerIntelligence::Ultra),
                    "eng-alt" => Some(EngineerIntelligence::Alt),
                    "eng-gemini" => Some(EngineerIntelligence::Gemini),
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

    pub(crate) fn cmd_agent_done(
        &mut self,
        hide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dashboard.is_focused(window, cx)
            && let Some(crate::dashboard::RowTarget::TreeTopic { host, node_id, .. }) =
                self.dashboard.cursor_target(&self.registry, cx)
        {
            let state = if hide {
                rho_desk::cells::State::Muted
            } else {
                rho_desk::cells::State::Done
            };
            let writes = vec![rho_desk::cells::CellWrite {
                id: node_id,
                property: rho_desk::cells::Property::State(state),
            }];
            self.apply_desk_writes(host, writes, None, window, cx);
            return;
        }
        if !self.require_connected(cx) {
            return;
        }
        let verdict = if hide {
            crate::desk_view::DeskVerdict::Mute
        } else {
            crate::desk_view::DeskVerdict::Done
        };
        let targets = self.subject(window, cx).agents;
        let hid_open_agent = self
            .selection
            .selected_agent()
            .is_some_and(|agent_id| targets.contains(&agent_id));
        let sent = self.deal_agents(targets, "done", verdict, window, cx);
        // Hiding the open agent closes its tab, or it would stay
        // rail-visible through the selection exemption.
        if hide && sent && hid_open_agent {
            self.select_agent(None, window, cx);
        }
    }

    /// The snooze operator: `s` and a unit, with vim's count in front, so
    /// `45sm` is 45 minutes, `3sh` three hours, `2sd` two days, `sw` a week
    /// and `ss` the default day. The deal bar echoes the time it comes back.
    /// `d` in the verdict transient. The verdict lands on the card the
    /// transient was opened over, which is the surface in view.
    pub(crate) fn verdict_done(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.deal_card_is_target(cx) {
            self.echo("done: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        }
        if !self.submit_tree_verdict(
            None,
            crate::desk_view::DeskVerdict::Done,
            crate::dashboard::DealerVerdict::Done,
            "done".to_owned(),
            window,
            cx,
        ) {
            self.echo("done: the note is unavailable", StyleClass::SystemInfo, cx);
        }
    }

    /// `x`: done, plus the silence the source has a place for.
    pub(crate) fn verdict_mute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.deal_card_is_target(cx) {
            self.echo("mute: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        }
        if !self.submit_tree_verdict(
            None,
            crate::desk_view::DeskVerdict::Mute,
            crate::dashboard::DealerVerdict::Mute,
            "mute".to_owned(),
            window,
            cx,
        ) {
            self.echo("mute: the note is unavailable", StyleClass::SystemInfo, cx);
        }
    }

    /// `t`: the card is handled by a note that comes back on a pace, in
    /// days, defaulting to a week.
    pub(crate) fn verdict_todo(
        &mut self,
        count: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let days = count.unwrap_or(7).max(1) as u32;
        let today = chrono::Local::now().date_naive();
        if !self.deal_card_is_target(cx) {
            self.echo("todo: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        }
        if !self.submit_tree_verdict(
            None,
            crate::desk_view::DeskVerdict::Todo {
                defer_until: crate::desk_view::day_timestamp(today),
                pace: days,
            },
            crate::dashboard::DealerVerdict::Done,
            "todo".to_owned(),
            window,
            cx,
        ) {
            self.echo("todo: the note is unavailable", StyleClass::SystemInfo, cx);
            return;
        }
        self.echo(&format!("todo: {days}d"), StyleClass::SystemInfo, cx);
    }

    /// `shift-s`: the room the card sits in goes quiet, not the card.
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
        let Some((_host, room_node)) = self.dashboard.tree_room_node(&card) else {
            return;
        };
        if !self.submit_tree_verdict(
            Some(room_node),
            crate::desk_view::DeskVerdict::Defer {
                until: crate::desk_view::day_timestamp(today + chrono::Duration::days(days)),
            },
            crate::dashboard::DealerVerdict::Defer,
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
        let until = rho_desk::cells::Timestamp {
            unix_ms: at.timestamp_millis(),
            precision: rho_desk::cells::TimestampPrecision::Millisecond,
        };
        self.deal_snooze_until(until, snooze_said(at, chrono::Local::now()), window, cx);
    }

    fn deal_snooze_until(
        &mut self,
        until: rho_desk::cells::Timestamp,
        said: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.deal_card_is_target(cx) {
            self.echo("snooze: nothing under the deal", StyleClass::SystemInfo, cx);
            return;
        }
        if !self.submit_tree_verdict(
            None,
            crate::desk_view::DeskVerdict::Defer { until },
            crate::dashboard::DealerVerdict::Defer,
            said,
            window,
            cx,
        ) {
            self.echo(
                "snooze: the note is unavailable",
                StyleClass::SystemInfo,
                cx,
            );
        }
    }

    pub(crate) fn cmd_agent_snooze(
        &mut self,
        duration_ms: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require_connected(cx) {
            return;
        }
        let until = rho_desk::cells::Timestamp {
            unix_ms: now_ms().saturating_add(duration_ms) as i64,
            precision: rho_desk::cells::TimestampPrecision::Minute,
        };
        let targets = self.subject(window, cx).agents;
        self.deal_agents(
            targets,
            "snooze",
            crate::desk_view::DeskVerdict::Defer { until },
            window,
            cx,
        );
    }

    pub(crate) fn cmd_project_add(
        &mut self,
        path: String,
        name: Option<String>,
        description: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require_connected(cx) {
            return;
        }
        let workdir = match rho_agents::create::resolve_workdir(&self.hosts, &path) {
            Ok(workdir) => workdir,
            Err(message) => {
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                return;
            }
        };
        // A project is a label carrying a workdir. The name the user gave
        // is the label's path; the description was the daemon's and has no
        // fact to live in.
        let _ = description;
        let path_name = name.unwrap_or_else(|| {
            workdir
                .path
                .file_name()
                .map(str::to_owned)
                .unwrap_or_else(|| workdir.path.to_string())
        });
        let seed = self.registry.host_machine_seed(workdir.host);
        let Some(writes) = self.desk_cells.project_writes(
            workdir.host,
            &path_name,
            Some(rho_desk::cells::Project {
                host: seed,
                path: workdir.path,
            }),
        ) else {
            return;
        };
        self.apply_desk_writes(workdir.host, writes, None, window, cx);
        self.refresh_workdirs(workdir.host);
    }

    pub(crate) fn cmd_project_remove(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                let Some(writes) = self.desk_cells.project_writes(workdir.host, &name, None) else {
                    return;
                };
                self.apply_desk_writes(workdir.host, writes, None, window, cx);
                self.refresh_workdirs(workdir.host);
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
        let Some(workspace) = self.registry.agent_workspace(agent_id).cloned() else {
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
        let Some(connection) = self.connection_for(agent_id) else {
            return;
        };
        let task = connection.close_shell_task(agent_id.encoded(), cx);
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
        self.observe_browser_metadata(&model, window, cx);
        let view = cx.new(|cx| rho_browser::PageView::new(model, id, cx));
        let surface = Self::wrap_surface(SurfaceKey::Browser(id), SurfaceView::Browser(view));
        self.display_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    fn observe_browser_metadata(
        &mut self,
        model: &Entity<rho_browser::PageModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.pages.observed() {
            // A tab the reader opened from a page is a row on the map, so
            // the browser moving is the same kind of news as the Slack
            // mirror moving: the join has to be rebuilt, not just redrawn.
            // The model polls the metadata revision, so a burst of
            // ctrl-clicks arrives as one event and costs one reconcile.
            self.pages.observe(cx.subscribe_in(
                model,
                window,
                |workspace, _, _: &rho_browser::PageMetadataChanged, window, cx| {
                    if let Some(host) = workspace.hosts.primary() {
                        workspace.sync_tree_dashboard(host, window, cx);
                    }
                    cx.notify();
                },
            ));
        }
    }

    /// Creates a browser page and files it as a node under `parent`
    /// (`None` is the root), then opens it.
    pub(crate) fn create_browser_page(
        &mut self,
        url: String,
        parent: Option<(HostId, rho_desk::cells::Id)>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let _ = window;
        let create = rho_browser::create_page(url, cx);
        cx.spawn(async move |this, cx| {
            let record = create.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let record = match record {
                    Ok(record) => record,
                    Err(error) => {
                        tracing::error!(%error, "browser page creation failed");
                        let message = format!("browser: {error:#}");
                        this.notice_on(None, &message, StyleClass::SystemInfo, cx);
                        return;
                    }
                };
                let id = record.id;
                this.file_page(id, parent, rho_journal::CreateMethod::TabBirth, window, cx);
                this.preview_browser_page(id, window, cx);
                this.focus_rail(window, cx);
            });
        })
        .detach();
    }

    /// Files a page the user just opened. A page is not created here: it
    /// exists because the browser opened it, and the store only hears where
    /// the user put it.
    pub(crate) fn file_page(
        &mut self,
        page: rho_browser::PageId,
        parent: Option<(HostId, rho_desk::cells::Id)>,
        method: rho_journal::CreateMethod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = parent
            .as_ref()
            .map(|(host, _)| *host)
            .or_else(|| self.hosts.primary())
        else {
            self.notice_on(
                None,
                "new page: no daemon is connected",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let id = rho_desk::cells::Id::Page(rho_desk::PageId(*page.0.as_bytes()));
        let at_root = parent.is_none();
        let writes = vec![
            rho_desk::cells::CellWrite {
                id: id.clone(),
                property: rho_desk::cells::Property::Parent(parent.map(|(_, parent)| parent)),
            },
            rho_desk::cells::CellWrite {
                id: id.clone(),
                property: rho_desk::cells::Property::CreatedAt(crate::desk_view::now_timestamp()),
            },
        ];
        if self
            .apply_desk_writes(host, writes, None, window, cx)
            .is_none()
        {
            return;
        }
        rho_journal::record(rho_journal::Event::Created {
            node_id: id.into(),
            kind: rho_journal::CreatedKind::Page,
            method,
            at_root,
        });
        self.invalidate_dealer_signals(cx);
        self.refresh_dashboard(window, cx);
    }

    pub(crate) fn cmd_diff(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.subject_agent_or_notice("diff", window, cx) else {
            return;
        };
        if !self.require_agent_online(agent_id, cx) {
            return;
        }
        let Some(workspace) = self.registry.agent_workspace(agent_id).cloned() else {
            self.notice_on(
                None,
                "diff: agent has no workspace",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.open_diff_surface(agent_id, workspace, cx);
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
                "performance snapshot: no daemon is connected",
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
        let task = connection.upload_gui_telemetry_task(snapshot, cx);
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

    /// The attached daemons and how each is doing, as one notice line.
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

    /// Attaches a daemon named on the spot, for a machine that is not worth
    /// putting in the host list.
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

    /// Detaches a daemon by name, dropping everything the client held for it.
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
                    .hosts
                    .get(host)
                    .and_then(|host| host.auth.as_ref())
                    .into_iter()
                    .flat_map(|auth| &auth.namespaces)
                    .filter(|name| name.to_lowercase().contains(&needle))
                    .map(|name| {
                        let disabled = workspace
                            .hosts
                            .get(host)
                            .and_then(|host| host.auth.as_ref())
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
                    .hosts
                    .get(host)
                    .and_then(|host| host.auth.as_ref())
                    .is_some_and(|auth| auth.disabled_namespaces.iter().any(|item| item == name));
                workspace.hosts.send(
                    host,
                    ClientMessage::SetAuthAccountEnabled {
                        name: name.to_owned(),
                        enabled,
                    },
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
                let workdir =
                    match rho_agents::create::resolve_workdir(&self.hosts, argument.as_str()) {
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
        self.hosts
            .send(host, ClientMessage::AgentStreamFocus { agent_ids });
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
    /// Rows that name nobody — the draft, the rail tail — fall through
    /// to the open agent, so a chord from the dashboard still lands.
    pub(crate) fn subject(&self, window: &Window, cx: &mut Context<Self>) -> Subject {
        use crate::dashboard::RowTarget;
        let row = self
            .dashboard
            .focus_handle(cx)
            .is_focused(window)
            .then(|| self.dashboard.cursor_target(&self.registry, cx))
            .flatten();
        let subject = match row {
            Some(RowTarget::TreeAgent { agent_id, .. }) => Some(Subject {
                agent: Some(agent_id),
                agents: self.registry.agent_subtree(agent_id),
            }),
            Some(RowTarget::TreeTopic {
                host,
                node_id,
                first_attention,
                ..
            }) => first_attention
                .or_else(|| self.dashboard.first_tree_agent_for_topic((host, node_id)))
                .map(|agent_id| Subject {
                    agent: Some(agent_id),
                    agents: self.registry.agent_subtree(agent_id),
                }),
            _ => None,
        };
        subject.unwrap_or_else(|| {
            self.selection
                .selected_agent()
                .map_or_else(Subject::default, |agent_id| Subject {
                    agent: Some(agent_id),
                    agents: self.registry.agent_subtree(agent_id),
                })
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

    /// One verdict per agent of the row, written into the store like any
    /// other verdict: the cursor lands on where the agent's story stands,
    /// so what it said before the press is handled and what it says after
    /// is not.
    fn deal_agents(
        &mut self,
        targets: Vec<AgentId>,
        command: &str,
        verdict: crate::desk_view::DeskVerdict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(&first) = targets.first() else {
            let message = format!("{command}: no agent under the cursor");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            return false;
        };
        let Some(host) = self.registry.host_of_agent(first) else {
            return false;
        };
        // Name what the press covered. A verdict is otherwise the one action
        // whose success looks exactly like a key that did nothing, which is
        // how a row that will not settle stays a mystery.
        let subject = match targets.as_slice() {
            [agent_id] => self.registry.agent_display_label(*agent_id),
            agents => format!("{} agents", agents.len()),
        };
        self.echo(&format!("{command}: {subject}"), StyleClass::SystemInfo, cx);
        for agent_id in targets {
            let id = rho_desk::cells::Id::Agent(agent_id);
            let Some((writes, verdict_entry)) =
                self.desk_cells.verdict_writes(host, &id, verdict.clone())
            else {
                continue;
            };
            self.apply_desk_writes(host, writes, Some(verdict_entry), window, cx);
        }
        self.refresh_desk_sources(host, None, cx);
        self.invalidate_dealer_signals(cx);
        cx.notify();
        true
    }

    /// Tab in the draft cycles the `Workdir:` field, the start field, and
    /// the body. On agent views it does nothing.
    fn cycle_draft_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.selected_agent().is_none()
            && let Some(editor) = self.focused_draft_editor()
        {
            self.draft_model
                .update(cx, |view, cx| view.toggle_field(&editor, window, cx));
        }
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

    /// Ctrl-Tab cycles the value the cursor is on: the role, or the start
    /// field's mode (on top of → join → sandbox). Elsewhere in the draft it
    /// does nothing, there being no value to cycle.
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
                }
            });
        }
    }

    /// (Re)writes the draft scaffold with the derived default workdir; the
    /// field stays empty when nothing daemon-side suggests one.
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

    /// An agent's working directory as a host-qualified workdir: what a new
    /// sibling agent should inherit.
    fn agent_workdir(&self, agent_id: AgentId) -> Option<HostPath> {
        Some(HostPath {
            host: self.host_of(agent_id)?,
            path: self.registry.working_directory(agent_id)?,
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

    fn append_message(&mut self, text: String, class: StyleClass, cx: &mut Context<Self>) {
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
    pub(crate) fn desk_cells_snapshot_for_test(
        &self,
        host: HostId,
    ) -> Vec<crate::desk_view::DeskNode> {
        self.desk_cells.nodes(host).to_vec()
    }

    #[cfg(test)]
    pub(crate) fn filing_destinations_for_test(
        &self,
    ) -> &[(String, String, HostId, rho_desk::cells::Id)] {
        &self.pending_filing_destinations
    }

    /// The workdir a new thing under an area inherits, which is what
    /// `n a` puts in the draft.
    #[cfg(test)]
    pub(crate) fn area_workdir_for_test(
        &self,
        host: HostId,
        node_id: rho_desk::cells::Id,
    ) -> Option<HostPath> {
        self.area_workdir(host, node_id)
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

    /// Shows an agent beside the dashboard cursor without changing the
    /// focused task or the dashboard's layout.
    fn preview_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard_preview == Some(agent_id) {
            return;
        }
        self.activate_agent(agent_id, cx);
        let view = self.materialize_model(&agent_id, window, cx);
        view.update(cx, |view, cx| view.tick_timers(now_ms(), cx));
        self.dashboard_preview = Some(agent_id);
        self.pages.clear_preview();
        self.ensure_duration_timer(cx);
        cx.notify();
    }

    fn preview_browser_page(
        &mut self,
        id: rho_browser::PageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pages.previewing(id) {
            return;
        }
        let Some(model) = rho_browser::open_page(id, cx) else {
            return;
        };
        self.scan_browser_pages_for_gc(cx);
        self.observe_browser_metadata(&model, window, cx);
        let view = cx.new(|cx| rho_browser::PageView::new(model, id, cx));
        self.dashboard_preview = None;
        self.pages.preview_page(id, view);
        cx.notify();
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

    /// The dashboard cursor moved: preview the row it landed on, and hide
    /// the preview when the cursor leaves every staffed region. Only
    /// while the dashboard owns the keyboard — programmatic cursor
    /// restoration and unfocused syncs never drive the visible surface.
    fn dashboard_cursor_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        if !self.dashboard.focus_handle(cx).is_focused(window) {
            return;
        }
        let target = self.dashboard.cursor_target(&self.registry, cx);
        if let Some(RowTarget::TreePage { page_id, .. }) = target {
            self.preview_browser_page(page_id, window, cx);
            return;
        }
        let agent = match target {
            Some(RowTarget::TreeAgent { agent_id, .. }) => Some(agent_id),
            Some(RowTarget::TreeTopic {
                host,
                node_id,
                first_attention,
                ..
            }) => first_attention
                .or_else(|| self.dashboard.first_tree_agent_for_topic((host, node_id))),
            _ => None,
        };
        match agent {
            Some(agent_id) if self.dashboard_preview != Some(agent_id) => {
                self.preview_agent(agent_id, window, cx)
            }
            Some(_) => {}
            None => self.clear_dashboard_preview(cx),
        }
    }

    /// Hides the preview pane: the cursor is on a header, prose, or an
    /// unstaffed heading, so no agent claims the frame.
    fn clear_dashboard_preview(&mut self, cx: &mut Context<Self>) {
        if self.dashboard_preview.is_none() && self.pages.preview().is_none() {
            return;
        }
        self.dashboard_preview = None;
        self.pages.clear_preview();
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
            SurfaceKey::DeskNode { .. } => "note".to_owned(),
            SurfaceKey::Transcript(agent_id) => self.registry.agent_display_label(*agent_id),
            SurfaceKey::File { path, .. } => path.to_string(),
            SurfaceKey::Shell(agent_id) => {
                format!("shell {}", self.registry.agent_id_label(*agent_id))
            }
            SurfaceKey::Diff { agent_id } => {
                format!("changes {}", self.registry.agent_display_label(*agent_id))
            }
            SurfaceKey::Terminal {
                agent_id,
                terminal_id,
            } => format!(
                "term {}/{terminal_id}",
                self.registry.agent_id_label(*agent_id)
            ),
            SurfaceKey::Browser(browser) => browser.to_string(),
            SurfaceKey::ZulipInbox => "zulip".to_owned(),
            SurfaceKey::ZulipNarrow { label } => label.clone(),
            SurfaceKey::SlackList => "slack".to_owned(),
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
            SurfaceKey::DeskNode { .. } => "note",
            SurfaceKey::Transcript(_) => "transcript",
            SurfaceKey::File { .. } => "file",
            SurfaceKey::Shell(_) => "shell",
            SurfaceKey::Diff { .. } => "diff",
            SurfaceKey::Terminal { .. } => "terminal",
            SurfaceKey::Browser(_) => "browser",
            SurfaceKey::ZulipInbox => "zulip inbox",
            SurfaceKey::ZulipNarrow { .. } => "zulip",
            SurfaceKey::SlackList => "slack list",
            SurfaceKey::SlackConversation(_) => "slack",
            SurfaceKey::Image { .. } => "image",
        }
    }

    /// The active context's surfaces as `(name, kind)` for completion.
    pub fn buffer_table(&self) -> Vec<(String, String)> {
        self.surfaces
            .get(&self.active_context)
            .map(|list| {
                list.iter()
                    .map(|surface| {
                        (
                            self.surface_name(&surface.key),
                            Self::surface_kind(&surface.key).to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
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
    fn switch_buffer(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(surface) = self.surface_named(name).cloned() else {
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
            SurfaceKey::DeskNode { host, node_id } => SurfaceIdentity::DeskNode {
                host: host.0,
                node_id: node_id.clone().into(),
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
            SurfaceKey::Diff { agent_id } => SurfaceIdentity::Diff {
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
            SurfaceKey::ZulipInbox => SurfaceIdentity::ZulipInbox,
            SurfaceKey::ZulipNarrow { label } => SurfaceIdentity::ZulipNarrow {
                label: label.clone(),
            },
            SurfaceKey::SlackList => SurfaceIdentity::SlackList,
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

    fn journal_scroll_burst(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (surface, rough_position) = if self.dashboard_mode(window, cx) {
            let editor = self.dashboard.editor().clone();
            (
                rho_journal::SurfaceIdentity::Dashboard,
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64),
            )
        } else {
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
                | SurfaceView::DeskNode(editor)
                | SurfaceView::Transcript { editor, .. }
                | SurfaceView::Shell { editor, .. } => {
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
                SurfaceView::File(view) => {
                    let editor = view.read(cx).editor().clone();
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
                SurfaceView::Diff(view) => {
                    let editor = view.read(cx).editor().clone();
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
                SurfaceView::Terminal(view) => view.read(cx).scroll_offset() as i64,
                SurfaceView::Browser(_) => 0,
                SurfaceView::ZulipInbox(view) => {
                    let editor = view.read(cx).editor().clone();
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
                SurfaceView::ZulipNarrow(view) => {
                    let editor = view.read(cx).editor().clone();
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
                SurfaceView::SlackList(view) => {
                    let editor = view.read(cx).editor().clone();
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
                SurfaceView::SlackConversation(view) => {
                    let editor = view.read(cx).editor().clone();
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
                SurfaceView::Image(_) => 0,
            };
            (Self::journal_surface(&pane.current().surface.key), position)
        };
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
        let method = if self.overview_open {
            rho_journal::SurfaceShowMethod::Overview
        } else {
            rho_journal::SurfaceShowMethod::Open
        };
        self.display_surface_with_method(surface, method, cx);
    }

    pub(crate) fn display_surface_with_method(
        &mut self,
        surface: Surface,
        method: rho_journal::SurfaceShowMethod,
        cx: &mut Context<Self>,
    ) {
        // Leaving a Slack conversation is the only thing that tells Slack
        // it has been read, so the surface being replaced is marked on the
        // way out — the same summary-buffer exit the Zulip narrows do.
        let leaving = self
            .history
            .as_ref()
            .map(|history| history.current().surface.key.clone());
        if leaving.is_some_and(|key| key != surface.key) {
            self.leave_slack_conversation(cx);
        }
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
        self.overview_open = false;
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
        workspace: rho_ui_proto::WorkspaceInfo,
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
            let connection = self.connection_for(agent_id)?;
            Some(rho_files::open_remote_project(
                connection,
                workspace.clone(),
                cx,
            ))
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
        let Some(connection) = self.connection_for(agent_id) else {
            return;
        };
        let task = connection.open_shell_task(agent_id.encoded(), cx);
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
        workspace: &rho_ui_proto::WorkspaceInfo,
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
        workspace: rho_ui_proto::WorkspaceInfo,
        opened: RemoteProject,
    ) -> RemoteProject {
        if let Some(existing) = self.cached_remote_project(host, &workspace) {
            return existing;
        }
        self.remote_projects
            .insert((host, workspace), opened.state.downgrade());
        opened
    }

    /// Persists the agent's jj working-copy snapshot, then projects its
    /// parent-side manifest over the workspace's shared live buffers.
    /// Reopening refreshes the existing shared model.
    fn open_diff_surface(
        &mut self,
        agent_id: AgentId,
        workspace: rho_ui_proto::WorkspaceInfo,
        cx: &mut Context<Self>,
    ) {
        let key = SurfaceKey::Diff { agent_id };
        if let Some(surface) = self.find_surface(|surface| surface.key == key).cloned() {
            if let SurfaceView::Diff(view) = &surface.view {
                view.update(cx, |view, cx| {
                    view.model().update(cx, |model, cx| model.refresh_now(cx));
                });
            }
            self.display_surface(surface, cx);
            cx.notify();
            return;
        }

        let Some(host) = self.host_of(agent_id) else {
            return;
        };
        let Some(diff_client) = self.connection_for(agent_id).map(Connection::diff_client) else {
            return;
        };
        let cached = self.cached_remote_project(host, &workspace);
        let project_task = cached.is_none().then(|| {
            let connection = self.connection_for(agent_id).expect("host still attached");
            rho_files::open_remote_project(connection, workspace.clone(), cx)
        });
        let task = cx.spawn(async move |this, cx| {
            let result: anyhow::Result<(RemoteProject, rho_files::PreparedDiff)> = async {
                let opened = match cached {
                    Some(project) => project,
                    None => project_task
                        .expect("missing project task")
                        .await
                        .context("project dial task failed")?,
                };
                let project = this
                    .update(cx, |this, _| {
                        this.cache_remote_project(host, workspace.clone(), opened)
                    })
                    .map_err(|_| anyhow::anyhow!("GUI closed while loading diff"))?;
                let live_paths = cx.update(|cx| rho_files::dirty_paths(&project, cx));
                let snapshot_task = cx.update(|cx| {
                    diff_client.snapshot(workspace.clone(), None, live_paths.clone(), cx)
                });
                let snapshot = snapshot_task
                    .await?
                    .context("initial diff snapshot unexpectedly unchanged")?;
                let prepared = rho_files::PreparedDiff::load(
                    &project,
                    &diff_client,
                    workspace.clone(),
                    snapshot,
                    live_paths,
                    None,
                    cx,
                )
                .await?;
                Ok((project, prepared))
            }
            .await;

            match result {
                Ok((project, prepared)) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        let model = cx.new(|cx| {
                            rho_files::DiffModel::new(project, diff_client, workspace, prepared, cx)
                        });
                        let view = cx.new(|cx| rho_files::DiffView::new(model, window, cx));
                        let surface = Self::wrap_surface(key, SurfaceView::Diff(view));
                        this.display_surface(surface, cx);
                        this.focus_active_surface(window, cx);
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.notice_on(
                            None,
                            &format!("diff failed: {error:#}"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                    });
                }
            }
        });
        self.pending_diff_loads.insert(agent_id, task);
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
        let Some(connection) = self.connection_for(agent_id) else {
            return;
        };
        let task = connection.open_terminal_task(agent_id.encoded(), new, 80, 24, cx);
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
        // (the daemon answers for every agent on connecting, with nothing
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
    pub(crate) fn dashboard_editor(&self) -> Entity<editor::Editor> {
        self.dashboard.editor().clone()
    }

    #[cfg(test)]
    pub(crate) fn tree_buffer_for_test(
        &self,
        host: HostId,
        node_id: rho_desk::cells::Id,
    ) -> Option<Entity<language::Buffer>> {
        self.desk_cells.buffer(host, &node_id).cloned()
    }

    #[cfg(test)]
    pub(crate) fn tree_nodes_for_test(
        &self,
        host: HostId,
        cx: &App,
    ) -> Vec<(rho_desk::cells::Id, Option<rho_desk::cells::Id>, String)> {
        self.desk_cells
            .nodes(host)
            .iter()
            .filter_map(|node| {
                let text = self.desk_cells.buffer(host, &node.id)?.read(cx).text();
                Some((node.id.clone(), node.parent.clone(), text))
            })
            .collect()
    }

    /// The map exactly as it is drawn, prefixes included, which is where a
    /// row's place shows up: two rows of one thing must each carry their
    /// own bullet.
    #[cfg(test)]
    pub(crate) fn dashboard_display_text_for_test(&self, cx: &mut App) -> String {
        self.dashboard.display_text_for_test(cx)
    }

    #[cfg(test)]
    pub(crate) fn focus_tree_node_for_test(
        &mut self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dashboard.move_to_tree_node_when_ready(host, node_id);
        self.refresh_dashboard(window, cx);
        window.focus(&self.dashboard.focus_handle(cx), cx);
    }

    #[cfg(test)]
    pub(crate) fn pending_agent_filing_for_test(&self) -> Option<(HostId, rho_desk::cells::Id)> {
        self.pending_agent_filing.clone()
    }

    #[cfg(test)]
    pub(crate) fn dashboard_has_new_draft_for_test(&self) -> bool {
        self.dashboard.has_new_draft_for_test()
    }

    #[cfg(test)]
    pub(crate) fn draft_area_for_test(&self) -> Option<(HostId, rho_desk::cells::Id)> {
        self.draft_area.clone()
    }

    /// The title of the menu under the point, which is how a test says
    /// which menu came back when escape retraced a step.
    #[cfg(test)]
    pub(crate) fn menu_title_for_test(&self) -> Option<&str> {
        self.menu_buffer.as_ref().map(|open| open.menu.title())
    }

    /// The reconnect loop marks test hosts disconnected (their sockets
    /// don't exist); verbs gated on connectivity need this to run.
    #[cfg(test)]
    pub(crate) fn force_host_online(&mut self, host: HostId) {
        self.hosts
            .set_status(host, rho_hosts::hosts::HostStatus::Online);
    }

    #[cfg(test)]
    pub(crate) fn take_host_messages_for_test(&self, host: HostId) -> Vec<ClientMessage> {
        self.hosts
            .connection(host)
            .map(Connection::take_sent_for_test)
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn dashboard_deal_mode_for_test(&mut self, cx: &mut Context<Self>) -> bool {
        self.open_card_in_view(cx).is_some()
    }

    #[cfg(test)]
    pub(crate) fn merged_quota_summaries_for_test(&self) -> Vec<rho_ui_proto::QuotaSummary> {
        self.hosts.merged_quota_summaries()
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
            self.overview_open = false;
            self.focus_active_surface(window, cx);
            cx.notify();
        }
    }

    #[cfg(test)]
    fn test_named_surface(&mut self, name: &str, cx: &mut Context<Self>) -> Surface {
        let editor = self.active_editor(cx);
        Self::wrap_surface(
            SurfaceKey::ZulipNarrow {
                label: name.to_owned(),
            },
            SurfaceView::DeskNode(editor),
        )
    }

    /// The children a note surface is showing, in order.
    #[cfg(test)]
    pub(crate) fn note_children_for_test(
        &self,
        host: HostId,
        node_id: rho_desk::cells::Id,
    ) -> Vec<rho_desk::cells::Id> {
        self.note_views
            .get(&(host, node_id))
            .map(|view| view.children())
            .unwrap_or_default()
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
    pub(crate) fn overview_open_for_test(&self) -> bool {
        self.overview_open
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
    ) -> Option<(crate::dashboard::DealCardId, crate::dashboard::DealCardKind)> {
        self.open_card_in_view(cx)
            .map(|card| (card.identity, card.kind))
    }

    #[cfg(test)]
    pub(crate) fn card_target_for_test(
        &self,
        card: crate::dashboard::DealCardId,
    ) -> crate::dashboard::CardTarget {
        self.dashboard.card_target(card)
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
        self.open_overview(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn verdict_undo_count_for_test(&self) -> usize {
        self.verdict_undo.len()
    }

    /// Reconciles the dashboard against the current world. Event-driven,
    /// with no flag to remember: the daemon funnel (`handle_event`),
    /// desk buffer edit subscriptions, draft edit subscriptions, the
    /// editor selection subscription, and the verbs each call this at
    /// their source. The reconcile is idempotent and cheap, so calling
    /// it from several funnels is fine.
    pub(crate) fn sync_tree_dashboard(
        &mut self,
        host: HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_tree_rows(host, None, window, cx);
    }

    /// Makes the cards `scope` names again, and leaves the rest of the
    /// ranking standing.
    fn refresh_deal_cards(
        &mut self,
        host: HostId,
        scope: crate::dashboard::DealScope<'_>,
        cx: &mut Context<Self>,
    ) {
        let now = chrono::Local::now().fixed_offset();
        let threads = self.slack_thread_facts(cx);
        self.dashboard.refresh_deal_cards(
            host,
            scope,
            &self.registry,
            &threads,
            now,
            &self.agent_last_interaction,
        );
    }

    /// The map brought up to a desk delta. A delta that kept the shape
    /// costs the rows it names: the desk has already patched them, the
    /// dashboard draws those rows again where they sit, and the cards of
    /// those rows are made again. A shape that moved is the one case that
    /// composes the map, and it says so.
    fn sync_tree_delta(
        &mut self,
        host: HostId,
        delta: &crate::desk_view::DeskDelta,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if delta.is_quiet() {
            return;
        }
        if !delta.shape {
            let nodes = self.desk_cells.nodes(host).to_vec();
            let threads = self.slack_thread_facts(cx);
            if self.dashboard.redraw_tree_rows(
                host,
                &delta.touched,
                &nodes,
                &self.registry,
                &threads,
                cx,
            ) {
                let touched = delta.touched.iter().cloned().collect::<Vec<_>>();
                self.refresh_deal_cards(host, crate::dashboard::DealScope::Nodes(&touched), cx);
                // The deal bar reads the hand; the map is not composed.
                self.dashboard.sync_hand(&self.agent_last_interaction);
                self.sync_note_views(host, cx);
                cx.notify();
                return;
            }
        }
        self.sync_tree_dashboard(host, window, cx);
    }

    /// The map for a host, made again from the desk. `moved` names the
    /// agents a `Changed` moved; everything else the sources hold stands.
    ///
    /// Times itself on the way out, including the early return, which is
    /// the path a cheap event takes and so the one worth measuring.
    fn sync_tree_rows(
        &mut self,
        host: HostId,
        moved: Option<&BTreeSet<AgentId>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The desk half of the same accounting: this runs from
        // `on_next_frame`, before the frame, so it lands outside every
        // frame span. Its work unit is the agents the event named, or zero
        // for a whole-desk sync, which is the distinction that matters —
        // the two cost differently and are the same function.
        let start = std::time::Instant::now();
        let work_units = moved.map_or(0, |agents| agents.len() as u64);
        let _record = OnDrop(Some(move || {
            gpui::profiler::record_main_thread_work(gpui::profiler::MainThreadWork {
                owner: gpui::profiler::MainThreadWorkKind::DeskSync,
                start,
                end: std::time::Instant::now(),
                work_units,
            });
        }));
        self.refresh_desk_sources(host, moved, cx);
        // The sources decide who is on the desk at all, so the map is
        // built again here and the delta paths patch what it left.
        self.desk_cells.rebuild_map(host);
        // A row that exists only because a source says so — a tab the
        // browser has just opened, a unit the mirror has just raised — has
        // nothing written, so no store event will ever give it the buffer
        // the map draws it from. It gets one here.
        self.desk_cells.reconcile_buffers(host, cx);
        if let Some((nodes, buffers, titles)) = self.desk_cells.tree_source(host, cx) {
            let shape_held =
                self.dashboard
                    .set_tree_source(host, nodes.clone(), buffers, titles, cx);
            // The cards this moved, and only those. A `Changed` names its
            // agents and costs them; a desk that arrived or changed shape
            // names nothing and is made again.
            // The dealer's own source, read from the store client rather
            // than from what the map just composed.
            //
            // Made on every sync, not only when the shape changed: a note
            // whose first line was edited keeps its shape and moves every
            // breadcrumb beneath it. This is the nodes and the indexes and
            // no rope, which is what the map's own index cost here before.
            let source = crate::candidates::HostNodes::of_notes(&mut self.desk_cells, host, cx);
            self.dashboard.set_deal_source(host, source);
            let moved = moved.map(|agents| agents.iter().copied().collect::<Vec<_>>());
            let scope = match &moved {
                Some(agents) => crate::dashboard::DealScope::Agents(agents),
                None => crate::dashboard::DealScope::Whole,
            };
            self.refresh_deal_cards(host, scope, cx);
            // An agent that is streaming says so constantly, and each time
            // it names itself and nobody else. If the shape held, the map
            // the editor has is still the right map: what moved is the
            // marker in front of those rows, the hint at the end of them,
            // and the words of a machine row. Drawing those is the desk
            // delta's path, and a model event has as much right to it —
            // composing the map again would take every inlay off the tree
            // and splice them all back for one agent's news, which is what
            // the user's telemetry caught.
            if shape_held && let Some(agents) = &moved {
                let touched = agents
                    .iter()
                    .map(|agent_id| rho_desk::cells::Id::Agent(*agent_id))
                    .collect::<BTreeSet<_>>();
                let threads = self.slack_thread_facts(cx);
                if self.dashboard.redraw_tree_rows(
                    host,
                    &touched,
                    &nodes,
                    &self.registry,
                    &threads,
                    cx,
                ) {
                    // The deal bar reads the hand; the map is not composed.
                    self.dashboard.sync_hand(&self.agent_last_interaction);
                    self.sync_note_views(host, cx);
                    cx.notify();
                    return;
                }
            }
            self.refresh_dashboard(window, cx);
            self.sync_note_views(host, cx);
        }
    }

    /// What the desk knows of one agent, or nothing when the agent is not
    /// this host's or the user filed it away.
    fn agent_source(&self, host: HostId, agent: AgentId) -> Option<crate::desk_view::AgentSource> {
        /// The log's positions and the store's are the same number; the
        /// two crates just name it themselves.
        fn story_pos(pos: rho_ui_proto::mirror::AgentPos) -> rho_desk::cells::StoryPos {
            rho_desk::cells::StoryPos(pos.0)
        }

        if self.registry.host_of_agent(agent) != Some(host) || self.registry.agent_hidden(agent) {
            return None;
        }
        let digest = self.registry.agent_digest(agent);
        Some(crate::desk_view::AgentSource {
            agent,
            spawned_by: self.registry.agent_parent(agent),
            workdir: self.registry.working_directory(agent),
            newest: digest
                .map(|digest| story_pos(digest.newest))
                .unwrap_or_default(),
            turn_running: digest.is_some_and(|digest| digest.turn_running),
            errored: digest.and_then(|digest| digest.errored).map(story_pos),
            wants: digest.and_then(|digest| {
                digest
                    .wants
                    .as_ref()
                    .map(|wants| (wants.want, story_pos(wants.at)))
            }),
        })
    }

    /// What the sources say right now, handed to the store's views. None
    /// of it is written: an agent's spawner is the registry's fact and a
    /// thread's conversation is the mirror's, and a copy could only go
    /// stale.
    fn refresh_desk_sources(
        &mut self,
        host: HostId,
        moved: Option<&BTreeSet<AgentId>>,
        cx: &Context<Self>,
    ) {
        // A filing that moved changes who is on the desk at all, so the
        // whole set is built again; otherwise only the agents named are.
        let filed = self
            .registry
            .set_agent_filings(self.desk_cells.agent_filing(host));
        let held = self
            .desk_cells
            .sources(host)
            .map(|sources| sources.agents.clone());
        let agents = match (moved, held) {
            (Some(moved), Some(mut agents)) if !filed => {
                for agent in moved {
                    let place = agents.binary_search_by(|source| source.agent.cmp(agent));
                    let source = self.agent_source(host, *agent);
                    match (place, source) {
                        (Ok(at), Some(source)) => agents[at] = source,
                        (Ok(at), None) => {
                            agents.remove(at);
                        }
                        (Err(at), Some(source)) => agents.insert(at, source),
                        (Err(_), None) => {}
                    }
                }
                agents
            }
            _ => self
                .registry
                .known_agents()
                .copied()
                .filter_map(|agent| self.agent_source(host, agent))
                .collect::<Vec<_>>(),
        };
        // The Slack mirror lives on this client, and its conversations are
        // the primary host's desk.
        // With no session there is nothing new to say about Slack, which is
        // not the same as saying every unit went quiet: the facts already
        // read from the mirror stand until a session replaces them.
        let slack = if self.slack.is_none() {
            self.desk_cells
                .sources(host)
                .map(|sources| sources.slack.clone())
                .unwrap_or_default()
        } else if self.hosts.primary() == Some(host) {
            self.slack_thread_facts(cx)
                .into_iter()
                .map(|(unit, facts)| crate::desk_view::SlackSource {
                    unit,
                    title: facts.title,
                    newest: rho_desk::cells::SlackTs(facts.latest),
                    newest_from_other: facts.newest_from_other.map(rho_desk::cells::SlackTs),
                    reason: facts.reason,
                })
                .collect()
        } else {
            Vec::new()
        };
        // The browser runs on this client, so its tabs are the primary
        // host's desk, the same way the Slack mirror's units are.
        let pages = if self.hosts.primary() == Some(host) && rho_browser::is_configured(cx) {
            rho_browser::live_pages()
                .into_iter()
                .map(|(page, opened_from)| crate::desk_view::PageSource {
                    page: rho_desk::PageId(*page.0.as_bytes()),
                    opened_from: opened_from.map(|id| rho_desk::PageId(*id.0.as_bytes())),
                })
                .collect()
        } else {
            Vec::new()
        };
        let sources = crate::desk_view::Sources {
            host: self.registry.host_machine_seed(host),
            agents,
            slack,
            pages,
        };
        self.desk_cells.set_sources(host, sources);
        // The user's verdicts are the one thing attention needs that no
        // row carries; the registry derives it from them and the digest,
        // and the mirror keeps them so a restart ranks the same way.
        for (agent_id, verdict) in self.desk_cells.agent_verdicts(host) {
            if self.registry.set_agent_verdict(agent_id, verdict) {
                rho_mirror::mirror::write_verdict(agent_id, verdict);
            }
        }
    }

    /// A transcript handed in whole, for a test that drives the view
    /// without a mirror to fold. Not an event: `rho-hosts` carries what a
    /// daemon said, and no daemon says this.
    #[cfg(any(test, feature = "walk-support"))]
    pub(crate) fn seed_transcript_for_test(
        &mut self,
        agent_id: AgentId,
        state: rho_agents::state::UiAgentState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_frame_batch(vec![(agent_id, TranscriptFrame::Fold(state))], window, cx);
    }

    /// One verdict, applied the way the dealer applies it, for a test that
    /// is about what the verdict writes rather than about the keystroke.
    #[cfg(test)]
    pub(crate) fn apply_verdict_for_test(
        &mut self,
        host: HostId,
        id: &rho_desk::cells::Id,
        verdict: crate::desk_view::DeskVerdict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((writes, event)) = self.desk_cells.verdict_writes(host, id, verdict) else {
            return false;
        };
        self.apply_desk_writes(host, writes, Some(event), window, cx)
            .is_some()
    }

    /// What the mirror would say, for a test with no Slack session: the
    /// join a card is derived from needs the unit's timestamps, and they are
    /// the one thing no store fixture can hold.
    #[cfg(test)]
    pub(crate) fn set_slack_sources_for_test(
        &mut self,
        host: HostId,
        slack: Vec<crate::desk_view::SlackSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut sources = self.desk_cells.sources(host).cloned().unwrap_or_default();
        sources.slack = slack;
        self.desk_cells.set_sources(host, sources);
        self.sync_tree_dashboard(host, window, cx);
    }

    /// Rebuilds every open note surface against the tree, so a child
    /// created or renamed elsewhere shows under the note it belongs to.
    fn sync_note_views(&mut self, host: HostId, cx: &mut Context<Self>) {
        if self.note_views.is_empty() {
            return;
        }
        let Some((nodes, buffers, note_titles)) = self.desk_cells.tree_source(host, cx) else {
            return;
        };
        // A note's title is the first line of its body, which the desk
        // keeps; a machine row's buffer holds the title the map derived
        // for it, and only those are read here.
        let mut titles = (*note_titles).clone();
        for (id, buffer) in &buffers {
            if !titles.contains_key(id) {
                titles.insert(
                    id.clone(),
                    crate::dashboard::note_title(&buffer.read(cx).text()).to_owned(),
                );
            }
        }
        let mut views = std::mem::take(&mut self.note_views);
        for ((view_host, _), view) in views.iter_mut() {
            if *view_host != host {
                continue;
            }
            view.sync(&nodes, &titles, cx);
        }
        self.note_views = views;
    }

    /// The note surface for a node, built on first open and kept after, so
    /// the cursor and scroll survive leaving and coming back. `None` while
    /// the node's body has not arrived from the daemon yet.
    fn note_view_for(
        &mut self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<&crate::note_view::NoteView> {
        let body = self.desk_cells.buffer(host, &node_id)?.clone();
        // A resync can hand out a fresh buffer for the same node; the old
        // surface is then over text nothing writes to any more.
        if self
            .note_views
            .get(&(host, node_id.clone()))
            .is_some_and(|view| view.body() != &body)
        {
            self.note_views.remove(&(host, node_id.clone()));
        }
        self.note_views
            .entry((host, node_id.clone()))
            .or_insert_with(|| {
                crate::note_view::NoteView::new(host, node_id.clone(), body, window, cx)
            });
        self.sync_note_views(host, cx);
        self.note_views.get(&(host, node_id))
    }

    /// Opens whatever a node is: a transcript, a page, a conversation, or
    /// the note surface. What `enter` on a row means, wherever the row is.
    pub(crate) fn open_tree_node(
        &mut self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let card = crate::dashboard::DealCardId {
            host,
            node_id: node_id.clone(),
        };
        match self.dashboard.card_target(card) {
            crate::dashboard::CardTarget::Agent(agent_id) => {
                self.open_agent(agent_id, window, cx);
                true
            }
            crate::dashboard::CardTarget::Page(page) => {
                self.open_browser_page(page, window, cx);
                true
            }
            crate::dashboard::CardTarget::Thread(thread) => {
                self.open_slack_source(crate::slack::unit_source(&thread), window, cx);
                true
            }
            crate::dashboard::CardTarget::Note | crate::dashboard::CardTarget::Missing => {
                self.open_note(host, node_id, window, cx)
            }
        }
    }

    /// Opens a node's own surface: the note, with its children under it.
    pub(crate) fn open_note(
        &mut self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .note_view_for(host, node_id.clone(), window, cx)
            .is_none()
        {
            return false;
        }
        let surface = self.make_surface(SurfaceKey::DeskNode { host, node_id }, window, cx);
        self.display_surface(surface, cx);
        self.focus_active_surface(window, cx);
        true
    }

    /// "Notes for this": the note filed under whatever the reader is
    /// looking at, created the first time the key is pressed. A note
    /// surface answers with itself, so the key is idempotent there.
    pub(crate) fn open_notes_for_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((host, node_id)) = self.surface_node() else {
            self.notice_on(
                None,
                "notes: nothing here to file a note under",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        if self
            .desk_cells
            .node(host, &node_id)
            .is_some_and(|node| node.is_note())
        {
            self.open_note(host, node_id, window, cx);
            return;
        }
        let existing = self
            .desk_cells
            .tree_source(host, cx)
            .into_iter()
            .flat_map(|(nodes, _, _)| nodes)
            .find(|node| node.parent == Some(node_id.clone()) && node.is_note())
            .map(|node| node.id);
        if let Some(existing) = existing {
            self.open_note(host, existing, window, cx);
            return;
        }
        if !self.require_connected(cx) {
            return;
        }
        let Some((created, writes)) = self.desk_cells.create_note_writes(host, Some(node_id))
        else {
            return;
        };
        if self
            .apply_desk_writes(host, writes, None, window, cx)
            .is_none()
        {
            return;
        }
        self.open_note(host, created, window, cx);
    }

    /// `enter` on a child row of a note surface opens that child. In the
    /// body it is an ordinary newline, so the handler propagates.
    fn note_open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let SurfaceKey::DeskNode { host, node_id } = self.active_surface().key.clone() else {
            return false;
        };
        let Some(child) = self
            .note_views
            .get(&(host, node_id))
            .and_then(|view| view.child_at_cursor(cx))
        else {
            return false;
        };
        self.open_tree_node(host, child, window, cx)
    }

    /// The node the current surface is about: the thing a note would be
    /// filed under. Every kind that has a row in the tree answers.
    pub(crate) fn surface_node(&self) -> Option<(HostId, rho_desk::cells::Id)> {
        let card = match &self.active_surface().key {
            SurfaceKey::DeskNode { host, node_id } => return Some((*host, node_id.clone())),
            SurfaceKey::Transcript(agent_id)
            | SurfaceKey::Shell(agent_id)
            | SurfaceKey::Diff { agent_id }
            | SurfaceKey::File { agent_id, .. }
            | SurfaceKey::Terminal { agent_id, .. } => self.dashboard.agent_card_id(*agent_id),
            SurfaceKey::Browser(page) => self.dashboard.page_card_id(*page),
            SurfaceKey::SlackConversation(rho_slack::session::Source::Thread(key)) => self
                .dashboard
                .thread_card_id(&crate::slack::store_unit_of(key)),
            // A channel surface is dealt for one of its messages, and that
            // message is what has a node. With several open in the same
            // channel, the newest is the one the reader was sent to.
            SurfaceKey::SlackConversation(rho_slack::session::Source::Conversation(channel)) => {
                self.dashboard
                    .open_thread_cards()
                    .into_iter()
                    .filter(|(_, unit)| unit.channel == channel.0)
                    .max_by(|(_, left), (_, right)| left.thread.cmp(&right.thread))
                    .map(|(card, _)| card)
            }
            _ => None,
        }?;
        Some((card.host, card.node_id))
    }

    /// A note-body edit, on its way to the daemon as a text operation.
    pub(crate) fn send_desk_text(
        &mut self,
        host: HostId,
        id: rho_desk::cells::Id,
        operation: rho_desk::TextOperation,
        transaction: rho_desk::TextTransaction,
        _cx: &mut Context<Self>,
    ) {
        self.send_to_host(
            host,
            ClientMessage::DeskTextApply {
                id,
                operation,
                transaction: Some(transaction),
            },
        );
    }

    /// Sends one mutation and remembers what its acceptance still owes:
    /// the editor's undo entry, a dealt verdict, or text for pasted notes.
    pub(crate) fn apply_desk_writes(
        &mut self,
        host: HostId,
        writes: Vec<rho_desk::cells::CellWrite>,
        verdict: Option<(rho_desk::cells::Id, rho_desk::cells::VerdictEvent)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<rho_desk::cells::Stamp> {
        let (message, delta) = self.desk_cells.apply(host, writes, verdict)?;
        let ClientMessage::DeskMutationApply { mutation } = &message else {
            return None;
        };
        let stamp = mutation.stamp;
        // A created note needs its buffer before anything can be typed into
        // it, and the daemon's answer may be a frame away.
        self.desk_cells.give_buffers(host, &delta, cx);
        self.sync_tree_delta(host, &delta, window, cx);
        self.send_to_host(host, message);
        Some(stamp)
    }

    /// The daemon took the mutation. Everything that was waiting on that
    /// answer happens here, in the order the user sees it.
    fn complete_desk_mutation(
        &mut self,
        host: HostId,
        stamp: rho_desk::cells::Stamp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_semantic_batches.remove(&(host, stamp));
        // Pasted notes exist now, so their bodies can be typed into the
        // buffers the sync just created. Each edit sends its own text op.
        for (node_id, text) in self
            .pending_desk_texts
            .remove(&(host, stamp))
            .unwrap_or_default()
        {
            let Some(buffer) = self.desk_cells.buffer(host, &node_id).cloned() else {
                continue;
            };
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, text.as_str())], None, cx);
            });
        }
        if let Some(verdict) = self.pending_tree_verdicts.remove(&(host, stamp)) {
            let submitted_card_is_current = self
                .open_card_in_view(cx)
                .is_some_and(|card| card.identity == verdict.event.card);
            let undo_sequence = verdict.undo.sequence;
            self.restore_verdict_undo(verdict.undo);
            if verdict.phone_verdict.is_some() && submitted_card_is_current {
                self.phone_completed_verdict(undo_sequence);
            }
            self.dashboard.record_dealer_event(verdict.event);
            if let Some(phone_verdict) = verdict.phone_verdict {
                self.record_phone_verdict(phone_verdict, cx);
            }
            if submitted_card_is_current {
                if verdict.phone_verdict.is_some() {
                    self.restore_phone_feed(window, cx);
                }
                self.finish_deal_verdict(window, cx);
            }
            self.echo(&verdict.echo, StyleClass::SystemInfo, cx);
        }
        if let Some(undone) = self.pending_tree_undos.remove(&(host, stamp)) {
            self.complete_verdict_undo(undone.entry, window, cx);
        }
    }

    /// The daemon refused it. `DeskCells` has already restored the last
    /// merged cells; what is left is to take back what the answer promised.
    fn reject_desk_mutation(
        &mut self,
        host: HostId,
        stamp: rho_desk::cells::Stamp,
        cx: &mut Context<Self>,
    ) {
        self.pending_desk_texts.remove(&(host, stamp));
        self.pending_tree_verdicts.remove(&(host, stamp));
        if let Some(transaction_id) = self.pending_semantic_batches.remove(&(host, stamp)) {
            self.discard_desk_semantic_transaction(transaction_id, cx);
        }
        if let Some(undone) = self.pending_tree_undos.remove(&(host, stamp)) {
            self.restore_verdict_undo(undone.entry);
        }
    }

    /// Records the editor undo entry a structure verb owns, so `u` emits the
    /// inverse writes rather than replaying text.
    pub(crate) fn record_desk_semantic_undo(
        &mut self,
        host: HostId,
        stamp: rho_desk::cells::Stamp,
        writes: Vec<rho_desk::cells::CellWrite>,
        cx: &mut Context<Self>,
    ) -> clock::Lamport {
        let transaction_id = self.dashboard.push_external_undo_transaction(cx);
        self.desk_semantic_undo
            .insert(transaction_id, DeskSemanticUndo { host, writes });
        self.pending_semantic_batches
            .insert((host, stamp), transaction_id);
        transaction_id
    }

    /// `* ` typed at the start of a line becomes a note: the line-local
    /// recognition the design keeps. Nothing else is ever parsed.
    fn recognize_desk_note_after_edit(
        &mut self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        line_end: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_heading_undo.take();
        let Some(buffer) = self.desk_cells.buffer(host, &node_id).cloned() else {
            return;
        };
        let text = buffer.read(cx).text();
        let cursor = line_end.min(text.len());
        let line_start = text[..cursor].rfind('\n').map_or(0, |index| index + 1);
        if !text[line_start..].starts_with("* ") || cursor < line_start + 2 {
            return;
        }
        let line_end_offset = text[line_start..]
            .find('\n')
            .map_or(text.len(), |index| line_start + index);
        let body = text[line_start + 2..line_end_offset].to_owned();
        let Some((created, writes)) = self.desk_cells.new_note_writes(host, &node_id, false) else {
            return;
        };
        // The recognized line leaves the source note; the star is a
        // keystroke, never stored text.
        let removed = line_start..if text[line_end_offset..].starts_with('\n') {
            line_end_offset + 1
        } else {
            line_end_offset
        };
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(removed, "")], None, cx);
        });
        let undo = self.desk_cells.delete_writes(created.clone());
        let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
            return;
        };
        if !body.is_empty() {
            self.pending_desk_texts
                .insert((host, stamp), vec![(created.clone(), body)]);
        }
        self.record_desk_semantic_undo(host, stamp, undo, cx);
        self.dashboard.move_to_tree_node_when_ready(host, created);
        self.sync_tree_dashboard(host, window, cx);
    }

    fn discard_desk_semantic_transaction(
        &mut self,
        transaction_id: clock::Lamport,
        cx: &mut Context<Self>,
    ) {
        self.desk_semantic_undo.remove(&transaction_id);
        self.dashboard
            .forget_external_undo_transaction(transaction_id, cx);
    }

    pub(crate) fn refresh_dashboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let threads = self.slack_thread_facts(cx);
        self.dashboard.sync(
            &self.registry,
            &threads,
            &self.agent_last_interaction,
            window,
            cx,
        );
        if let Some(unreferenced) = self.pages.reconcile(self.dashboard.page_ids()) {
            for page in unreferenced {
                self.schedule_browser_page_gc(page, cx);
            }
            self.scan_browser_pages_for_gc(cx);
        }
        self.invalidate_dealer_signals(cx);
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
                        if this.browser_page_retained(page.id) {
                            this.pages.not_closing(page.id);
                        } else {
                            this.schedule_browser_page_gc(page.id, cx);
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "list browser pages for reconciliation"),
            });
        })
        .detach();
    }

    fn schedule_browser_page_gc(&mut self, page: rho_browser::PageId, cx: &mut Context<Self>) {
        if self.pages.is_closing(page) || self.browser_page_retained(page) {
            return;
        }
        let gc = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(crate::browser::GRACE)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.pages.not_closing(page);
                if this.browser_page_retained(page) {
                    return;
                }
                tracing::info!(page_id = %page, "closing unreferenced browser page after grace period");
                if let Some(close) = rho_browser::close_page_if_running(page, cx) {
                    close.detach();
                }
            });
        });
        self.pages.closing(page, gc);
    }

    fn browser_page_retained(&self, page: rho_browser::PageId) -> bool {
        self.dashboard.page_ids().contains(&page)
    }

    #[cfg(test)]
    pub(crate) fn is_dashboard_mode(&self, window: &Window, cx: &App) -> bool {
        self.dashboard_mode(window, cx)
    }

    #[cfg(test)]
    /// The transcript this workspace shows for an agent, for a test that
    /// feeds it back changed.
    #[cfg(test)]
    pub(crate) fn transcript_for_test(&self, agent_id: AgentId) -> rho_agents::state::UiAgentState {
        self.transcripts
            .state(&agent_id)
            .cloned()
            .unwrap_or_else(|| rho_agents::state::UiAgentState {
                blocks: Vec::new(),
                status: rho_agents::state::UiAgentStatus::Idle,
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
            SurfaceView::DeskNode(editor) => editor.clone(),
            SurfaceView::Transcript { editor, .. } => editor.clone(),
            SurfaceView::File(view) => view.read(cx).editor().clone(),
            SurfaceView::Shell { editor, .. } => editor.clone(),
            SurfaceView::Diff(view) => view.read(cx).editor().clone(),
            SurfaceView::Terminal(_) => self.chrome_editor(),
            SurfaceView::Browser(_) => self.chrome_editor(),
            SurfaceView::ZulipInbox(view) => view.read(cx).editor().clone(),
            SurfaceView::ZulipNarrow(view) => view.read(cx).editor().clone(),
            SurfaceView::SlackList(view) => view.read(cx).editor().clone(),
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
    /// do; the desk's own editor exists for the life of the workspace.
    fn chrome_editor(&self) -> Entity<editor::Editor> {
        self.any_draft_editor()
            .unwrap_or_else(|| self.dashboard.editor().clone())
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
            return self.phone.dashboard_focus.clone();
        }
        match &self.active_surface().view {
            SurfaceView::Draft { editor, .. } => editor.focus_handle(cx),
            SurfaceView::Home(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Messages(editor) => editor.focus_handle(cx),
            SurfaceView::Usage(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::DeskNode(editor) => editor.focus_handle(cx),
            SurfaceView::Transcript { editor, .. } => editor.focus_handle(cx),
            SurfaceView::File(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Shell { editor, .. } => editor.focus_handle(cx),
            SurfaceView::Diff(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Terminal(view) => view.read(cx).focus_handle(cx),
            SurfaceView::Browser(view) => view.read(cx).focus_handle(cx),
            SurfaceView::ZulipInbox(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::ZulipNarrow(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::SlackList(view) => view.read(cx).editor().focus_handle(cx),
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
            | SurfaceKey::Diff { agent_id }
            | SurfaceKey::Terminal { agent_id, .. } => Some(*agent_id),
            SurfaceKey::Draft
            | SurfaceKey::Home
            | SurfaceKey::Messages
            | SurfaceKey::Usage
            | SurfaceKey::DeskNode { .. }
            | SurfaceKey::ZulipInbox
            | SurfaceKey::ZulipNarrow { .. } => None,
            SurfaceKey::SlackList | SurfaceKey::SlackConversation(_) => None,
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
            self.overlay_return_focus = Some(handle);
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
            SurfaceKey::DeskNode { host, node_id } => {
                let (host, node_id) = (*host, node_id.clone());
                match self.note_view_for(host, node_id, window, cx) {
                    Some(view) => SurfaceView::DeskNode(view.editor().clone()),
                    // Nothing opens a node whose body has not arrived; the
                    // dashboard's editor keeps the surface honest if one does.
                    None => SurfaceView::DeskNode(self.dashboard.editor().clone()),
                }
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
            SurfaceKey::Diff { .. } => {
                unreachable!("diff surfaces are created by open_diff_surface")
            }
            SurfaceKey::Terminal { .. } => {
                unreachable!("terminal surfaces are created by open_terminal_surface")
            }
            SurfaceKey::Browser(_) => {
                unreachable!("browser surfaces are created by create_browser_page")
            }
            SurfaceKey::ZulipInbox => {
                let session = self.zulip_session(cx);
                let hooks = Self::zulip_hooks();
                SurfaceView::ZulipInbox(
                    cx.new(|cx| rho_zulip::ui::InboxView::new(session, hooks, window, cx)),
                )
            }
            SurfaceKey::ZulipNarrow { .. } => {
                unreachable!("conversation surfaces are created by open_zulip_narrow")
            }
            SurfaceKey::SlackList => {
                let session = self
                    .slack_session(window, cx)
                    .expect("the slack list is only opened once a session exists");
                let hooks = Self::slack_hooks();
                SurfaceView::SlackList(
                    cx.new(|cx| rho_slack::ui::ListView::new(session, hooks, window, cx)),
                )
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
            SurfaceKey::Diff { agent_id } => {
                self.selection.select_agent(agent_id);
                Some(agent_id)
            }
            SurfaceKey::Draft => {
                self.selection.enter_draft();
                None
            }
            // Files and chat keep whatever agent context was current.
            SurfaceKey::Home
            | SurfaceKey::DeskNode { .. }
            | SurfaceKey::Messages
            | SurfaceKey::Usage
            | SurfaceKey::File { .. }
            | SurfaceKey::ZulipInbox
            | SurfaceKey::ZulipNarrow { .. } => None,
            SurfaceKey::SlackList | SurfaceKey::SlackConversation(_) => None,
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
        self.pending_filing_selected = None;
        self.pending_find_target = None;
        // Which row is chosen has to be read before `accept_selected`
        // rewrites the input into that row's text.
        if prompt == "find:" && minibuffer.accepts_selected(cx) {
            self.pending_find_target =
                self.find_target_at(&minibuffer.input(cx), minibuffer.selected_row());
        }
        if prompt == "file under:"
            && let Some((candidate, occurrence)) = minibuffer.selected_candidate()
        {
            self.pending_filing_selected = resolve_filing_destination(
                &self.pending_filing_destinations,
                &candidate,
                occurrence,
            );
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
            self.finish_overlay_focus(window, cx);
            self.restore_slack_search(window, cx);
            cx.notify();
        }
    }

    fn finish_git_approval(
        &mut self,
        decision: GitApprovalDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = self.pending_git_approval.take() {
            let _ = pending.response.send(decision);
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
        self.open_prompt_watching(prompt, complete, None, on_submit, window, cx);
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
        let prompt = prompt.into();
        rho_journal::record(rho_journal::Event::MinibufferOpened {
            prompt: prompt.to_string(),
        });
        self.capture_overlay_focus(window, cx);
        let text_style = self
            .active_editor(cx)
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let mut minibuffer = Minibuffer::open(
            prompt,
            &text_style,
            complete,
            on_change,
            on_submit,
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
    const SHIFT_TAP_HOLD: Duration = Duration::from_millis(300);

    /// The `shift` tap, decided on release. Opening the menu on the press
    /// put it under the reader's uppercase letter, which then landed in it;
    /// a tap is `shift` down and up with nothing in between.
    fn shift_modifiers_changed(
        &mut self,
        event: &gpui::ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.modifiers;
        tracing::debug!(
            shift = modifiers.shift,
            control = modifiers.control,
            alt = modifiers.alt,
            platform = modifiers.platform,
            held_ms = self.shift_down_at.map(|down| down.elapsed().as_millis()),
            "modifiers changed"
        );
        if modifiers.shift {
            // Down. Only `shift` on its own can still become a tap: with
            // another modifier already held this is a chord being built.
            self.shift_down_at = (!modifiers.control
                && !modifiers.alt
                && !modifiers.platform
                && !modifiers.function)
                .then(std::time::Instant::now);
            return;
        }
        let Some(down) = self.shift_down_at.take() else {
            return;
        };
        if down.elapsed() > Self::SHIFT_TAP_HOLD {
            return;
        }
        self.shift_tapped(window, cx);
    }

    fn shift_tapped(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        // The second tap is Home, whether it comes quickly on a surface that
        // is not a card or as the `shift` row of the verdict transient the
        // first tap opened. There is no timer on the second one: the menu is
        // on screen saying so.
        if self.verdict_transient_open() {
            self.last_shift_tap = None;
            self.close_menu(window, cx);
            self.toggle_overview(window, cx);
        } else if self.has_modal_overlay() {
            // A menu or a prompt is already holding the keyboard: shift
            // belongs to whatever the reader is typing there.
            self.last_shift_tap = None;
        } else if self
            .last_shift_tap
            .is_some_and(|last| now.duration_since(last) <= Self::SHIFT_TAP_HOLD)
        {
            self.last_shift_tap = None;
            self.toggle_overview(window, cx);
        } else {
            self.last_shift_tap = Some(now);
            self.open_verdict_transient(window, cx);
        }
    }

    /// One tap of `shift`: the verdicts, over whatever card is in view. The
    /// card is the surface the reader is on (or the row under the cursor on
    /// Home), so a verdict follows the eye rather than a mode. Returns
    /// whether there was a card to open it over; with none, the tap is left
    /// to the double tap that reaches Home.
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
        self.capture_overlay_focus(window, cx);
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
        self.capture_overlay_focus(window, cx);
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
        // Bare modifiers arrive as key events too; holding shift for an
        // uppercase key must not dismiss the menu.
        if matches!(
            event.keystroke.key.as_str(),
            "shift" | "control" | "alt" | "platform" | "function"
        ) {
            return;
        }
        let Some(open) = self.menu_buffer.as_mut() else {
            return;
        };
        let press = open.menu.press(&event.keystroke);
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
            MenuId::Snooze => crate::transient::snooze_menu(),
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
            Command::Rail => self.focus_rail(window, cx),
            Command::Map => self.open_overview(window, cx),
            Command::MapRawSource => self.cmd_toggle_raw_desk(window, cx),
            Command::SwitchBuffer => self.open_buffer_picker(window, cx),
            Command::MessageLog => self.cmd_messages(window, cx),
            Command::SurfaceBack => self.cmd_surface_back(window, cx),
            Command::PullCard => self.pull_card(window, cx),
            Command::CloseAndDeal => self.cmd_close_and_deal(window, cx),
            Command::OpenFile => self.prompt_open_file(window, cx),
            Command::FindNode => self.open_find(window, cx),
            Command::NotesForThis => self.open_notes_for_surface(window, cx),
            Command::Shell => self.cmd_shell(window, cx),
            Command::ShellClose => self.cmd_shell_close(window, cx),
            Command::Changes => self.cmd_diff(window, cx),
            Command::Terminal => self.cmd_term(false, window, cx),
            Command::NewTerminal => self.cmd_term(true, window, cx),
            Command::UndoVerdict => window.dispatch_action(Box::new(crate::UndoVerdict), cx),
            Command::Quit => cx.quit(),
            Command::SlackReact(name) => self.slack_react(&name, window, cx),
            Command::SlackReactByName => self.prompt_slack_react(window, cx),
            Command::SlackConversations => self.open_slack(window, cx),
            Command::SlackAttach => self.prompt_slack_attach(window, cx),
            Command::SlackMarkReadBefore => self.prompt_slack_mark_read_before(window, cx),
            Command::SlackRegister => self.prompt_slack_register(window, cx),
            Command::HostsList => self.cmd_hosts(cx),
            Command::HostAttach => self.prompt_host_attach(window, cx),
            Command::HostDetach => self.prompt_host_detach(window, cx),
            Command::HostAuth => self.open_host_auth_transient(window, cx),
            Command::ProjectAdd => self.prompt_project_add(window, cx),
            Command::ProjectRemove => self.prompt_project_remove(window, cx),
            Command::EndVoice => self.cmd_end_voice(cx),
            Command::PastePrompt => self.cmd_paste_prompt(window, cx),
            Command::ClearPromptImages => self.cmd_clear_prompt_attachments(window, cx),
            Command::NewAgent => self.begin_new(crate::create::NewKind::Agent, window, cx),
            Command::NewPage => self.begin_new(crate::create::NewKind::Page, window, cx),
            Command::NewNote => self.begin_new(crate::create::NewKind::Note, window, cx),
            Command::Usage(chart, days) => self.open_usage_chart(chart, days, window, cx),
            Command::UploadTelemetry => self.cmd_upload_gui_telemetry(cx),
            Command::Version => self.cmd_version(cx),
            Command::AgentDone => self.cmd_agent_done(false, window, cx),
            Command::AgentHide => self.cmd_agent_done(true, window, cx),
            Command::AgentCancel => self.cmd_agent_cancel(window, cx),
            Command::AgentRole => self.prompt_change_agent_role(window, cx),
            Command::AgentCompact => self.cmd_compact(window, cx),
            Command::AgentRewind => self.cmd_rewind(1, window, cx),
            Command::AgentRewindMany => self.prompt_rewind(window, cx),
            Command::AgentContinue => self.cmd_continue_turn(window, cx),
            Command::AgentCacheKey => self.cmd_change_prompt_cache_key(window, cx),
            Command::AgentSnooze(ms) => self.cmd_agent_snooze(ms, window, cx),
            Command::PhoneOpenDesk => self.phone_open_desk(window, cx),
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

    /// What the bar will say once the daemon takes the verdict. The echo
    /// waits for that, so a test that stops at the mutation has to look
    /// here to see the words the reader is promised.
    #[cfg(test)]
    pub(crate) fn pending_verdict_echo_for_test(&self) -> Option<&str> {
        self.pending_tree_verdicts
            .values()
            .next_back()
            .map(|pending| pending.echo.as_str())
    }

    fn has_modal_overlay(&self) -> bool {
        self.minibuffer.is_some()
            || self.menu_buffer.is_some()
            || self.pending_git_approval.is_some()
    }

    /// Captures normal focus on the first overlay in a chain. Replacements
    /// such as transient -> minibuffer inherit the original target.
    fn capture_overlay_focus(&mut self, window: &Window, cx: &App) {
        if self.overlay_return_focus.is_none() {
            self.overlay_return_focus = window.focused(cx);
        }
    }

    fn restore_overlay_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.overlay_return_focus.clone() {
            Some(handle) => {
                window.focus(&handle, cx);
                cx.notify();
            }
            None => self.focus_active_surface(window, cx),
        }
    }

    fn finish_overlay_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_overlay_focus(window, cx);
        self.overlay_return_focus = None;
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
                workspace.cmd_project_add(path.to_owned(), name, description, _window, cx);
            },
        );
        self.open_prompt("project path [name]:", complete, on_submit, window, cx);
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
                    workspace.cmd_project_remove(path, _window, cx);
                }
            },
        );
        self.open_prompt("remove project:", complete, on_submit, window, cx);
    }

    /// `space r`: focus jumps directly to the dashboard.
    pub(crate) fn focus_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overview(window, cx);
    }

    pub(crate) fn cmd_surface_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step_surface_back(window, cx);
    }

    /// `j` in the verdict transient, and `ctrl-j`: one pull.
    pub(crate) fn cmd_close_and_deal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_current_surface(window, cx);
        self.pull_card(window, cx);
    }

    pub(crate) fn cmd_toggle_raw_desk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dashboard.toggle_raw_mode(cx);
        rho_journal::record(rho_journal::Event::DeskRawModeToggled {
            enabled: self.dashboard.raw_mode(),
        });
        self.refresh_dashboard(window, cx);
    }

    /// Opens a card as an ordinary surface: the note, the transcript, the
    /// conversation. Nothing about it is a mode; the verdict keys reach it
    /// because it is what the reader is on.
    pub(crate) fn open_card(
        &mut self,
        card: crate::dashboard::DealCard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.dashboard
            .move_to_tree_node_when_ready(card.host, card.topic_node_id.clone());
        let host = card.identity.host;
        let node_id = card.identity.node_id.clone();
        let surface = match self.dashboard.card_target(card.identity.clone()) {
            crate::dashboard::CardTarget::Note | crate::dashboard::CardTarget::Missing => {
                self.dashboard
                    .move_to_tree_node_when_ready(host, node_id.clone());
                Self::wrap_surface(
                    SurfaceKey::DeskNode { host, node_id },
                    SurfaceView::DeskNode(self.dashboard.editor().clone()),
                )
            }
            crate::dashboard::CardTarget::Agent(agent_id) => {
                rho_journal::record(rho_journal::Event::AgentOpened {
                    agent_id: agent_id.into(),
                });
                self.selection.select_agent(agent_id);
                self.active_context = self.context_for_agent(agent_id);
                self.activate_agent(agent_id, cx);
                self.make_surface(SurfaceKey::Transcript(agent_id), window, cx)
            }
            // A thread is a conversation: the deal view is the conversation
            // surface itself, opened the way `enter` opens it, with the
            // message that raised the card on screen.
            crate::dashboard::CardTarget::Thread(unit) => {
                if self.open_slack_deal(&unit, window, cx) {
                    return true;
                }
                self.dashboard
                    .move_to_tree_node_when_ready(host, node_id.clone());
                Self::wrap_surface(
                    SurfaceKey::DeskNode { host, node_id },
                    SurfaceView::DeskNode(self.dashboard.editor().clone()),
                )
            }
            crate::dashboard::CardTarget::Page(page) => {
                match (self.phone.enabled, rho_browser::open_page(page, cx)) {
                    (false, Some(model)) => {
                        self.observe_browser_metadata(&model, window, cx);
                        let view = cx.new(|cx| rho_browser::PageView::new(model, page, cx));
                        Self::wrap_surface(SurfaceKey::Browser(page), SurfaceView::Browser(view))
                    }
                    _ => {
                        self.dashboard
                            .move_to_tree_node_when_ready(host, node_id.clone());
                        Self::wrap_surface(
                            SurfaceKey::DeskNode { host, node_id },
                            SurfaceView::DeskNode(self.dashboard.editor().clone()),
                        )
                    }
                }
            }
        };
        self.display_surface_with_method(surface, rho_journal::SurfaceShowMethod::Deal, cx);
        if self.phone.enabled {
            window.focus(&self.phone.dashboard_focus, cx);
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
        if self.phone.enabled
            && (self.phone_snap_in_progress()
                || self.phone_current_deal_has_pending_tree_verdict(cx))
        {
            return;
        }
        let now = chrono::Local::now().fixed_offset();
        let in_view = self.open_card_in_view(cx);
        let hand = self.hand(cx);
        // Reading a card and pulling again is what a skip is: the card is
        // still owed, it is just not what to look at next.
        if let Some(card) = &in_view
            && let Some(cursor) = hand.cursor(&card.identity).cloned()
        {
            self.dashboard.skip_card(card, cursor, now);
        }
        let Some(card) = hand
            .top(in_view.as_ref().map(|card| &card.identity))
            .cloned()
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
        self.refresh_dashboard(window, cx);
    }

    /// What `f` files: the card in front of the reader, else the row under
    /// the cursor on the map.
    pub(crate) fn label_target(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(HostId, rho_desk::cells::Id)> {
        // What the reader is on: the thing behind the surface in view, or
        // the row under the cursor when the map is what they are reading.
        // The card in hand is the target only when it is that thing, so
        // filing a page while a Slack card sits in the queue files the page.
        if !self.overview_open
            && let Some(node) = self.surface_node()
        {
            return Some(node);
        }
        // The map is the overlay in front, so its cursor row is what the
        // reader is on even when Home is the surface underneath with a
        // cursor of its own on some other card.
        if self.overview_open
            && let Some(node) = self.dashboard.tree_node_at_cursor(cx)
        {
            return Some(node);
        }
        self.context_area(cx)
    }

    /// The ranking as it stands. A pull, Home and a verdict all read the
    /// same kept set, brought up to the moment they read it.
    pub(crate) fn hand(&mut self, _cx: &mut Context<Self>) -> crate::dashboard::DealQueue {
        let now = chrono::Local::now().fixed_offset();
        self.dashboard
            .dealer_hand(now, &self.agent_last_interaction)
    }

    /// The node a card in view is about: the row the map's cursor is on,
    /// the row Home's cursor is on, or the thing the surface in view stands
    /// for. A surface that stands for nothing — a draft, the message log, a
    /// picker, a list — has no card at all. Falling through to the context
    /// the way filing does made those surfaces borrow whichever card the
    /// map's cursor had left behind: they wore its label and its why, and a
    /// verdict pressed over them landed on it.
    fn card_target(&mut self, cx: &mut Context<Self>) -> Option<(HostId, rho_desk::cells::Id)> {
        // The map is the overlay in front, so its cursor row is what the
        // reader is on even when Home is the surface underneath with a
        // cursor of its own on some other card.
        if self.overview_open {
            return self
                .dashboard
                .tree_node_at_cursor(cx)
                .or_else(|| self.context_area(cx));
        }
        // Home is a list of cards, so its cursor names one the same way the
        // map's does.
        if self.home_in_view() {
            return self.context_area(cx);
        }
        self.surface_node()
    }

    /// The card the reader is on: the one behind the surface in view, or
    /// the row under Home's or the map's cursor. When the ranking holds no
    /// card for that node the node itself is the card, because reading a
    /// thing can be what quiets it and it is still what is on screen.
    pub(crate) fn card_in_view(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<crate::dashboard::DealCard> {
        let (host, node_id) = self.card_target(cx)?;
        let hand = self.hand(cx);
        hand.card(&crate::dashboard::DealCardId {
            host,
            node_id: node_id.clone(),
        })
        .cloned()
        .or_else(|| self.dashboard.card_for_node(host, node_id, cx))
    }

    /// Whether Home itself is what the reader has open. Its cursor row is a
    /// card like the map's is, but Home is a list rather than a card: a pull
    /// from it opens the top card instead of passing over the row, and the
    /// bar still says "home".
    pub(crate) fn home_in_view(&self) -> bool {
        !self.overview_open && self.active_surface().key == SurfaceKey::Home
    }

    /// The card a surface in view stands for, which is nothing on Home:
    /// Home's cursor names a card to act on, but Home itself is a list, not
    /// the card. Everything else follows [`Self::card_target`], so a draft,
    /// the message log and every other list or log stand for no card.
    pub(crate) fn open_card_in_view(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<crate::dashboard::DealCard> {
        match self.home_in_view() {
            true => None,
            false => self.card_in_view(cx),
        }
    }

    fn deal_card_is_target(&mut self, cx: &mut Context<Self>) -> bool {
        self.card_in_view(cx).is_some()
    }

    pub(crate) fn label_card(
        &mut self,
        host: HostId,
        id: rho_desk::cells::Id,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let Some((writes, event)) = self.desk_cells.label_writes(host, &id, path) else {
            self.echo("label: nothing to label", StyleClass::SystemInfo, cx);
            return;
        };
        let removed = matches!(
            &event.1,
            rho_desk::cells::VerdictEvent::Applied {
                verdict: rho_desk::cells::Verdict::Label { present: false, .. },
                ..
            }
        );
        // `u` on the map takes a label back the same way it takes any other
        // structure verb back: the inverse writes, not replayed text.
        let undo = self.desk_cells.inverse_writes(host, &writes);
        let Some(stamp) = self.apply_desk_writes(host, writes, Some(event), window, cx) else {
            return;
        };
        self.record_desk_semantic_undo(host, stamp, undo, cx);
        let said = match removed {
            true => format!("label removed: {path}"),
            false => format!("label: {path}"),
        };
        self.echo(&said, StyleClass::SystemInfo, cx);
    }

    /// `f`: the one filing key, over the card in view or the row under the
    /// cursor. A label path in the picker puts that label on the thing, and
    /// the same path again takes it off, so a thing carries as many labels
    /// as the user says. Anything else picked is a place, and a thing is in
    /// one place: it sets the parent, replacing whatever it was under.
    pub(crate) fn prompt_file_deal_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((host, target)) = self.label_target(cx) else {
            self.echo("file: nothing under the cursor", StyleClass::SystemInfo, cx);
            return;
        };
        let carried = self
            .desk_cells
            .facts(host, &target)
            .map(|facts| facts.labels)
            .unwrap_or_default();
        let mut destinations = self
            .desk_cells
            .label_paths(host)
            .into_iter()
            .map(|(label, path)| {
                let description = match carried.contains(&label) {
                    true => "label · enter takes it off",
                    false => "label",
                };
                (path, description.to_owned(), host, label)
            })
            .collect::<Vec<_>>();
        let threads = self.slack_thread_facts(cx);
        // Labels are offered as their own paths, above, so the places are
        // everything else: one row per label, not two.
        destinations.extend(
            self.dashboard
                .area_candidates(&self.registry, &threads, cx)
                .into_iter()
                .filter(|(_, _, _, node_id)| !matches!(node_id, rho_desk::cells::Id::Label(_)))
                .map(|(path, kind, host, node_id)| (path, kind.to_owned(), host, node_id)),
        );
        self.pending_filing_destinations = destinations;
        self.pending_filing_selected = None;
        self.open_prompt(
            "file under:",
            std::rc::Rc::new(|workspace, needle, _cx| {
                let needle = needle.to_lowercase();
                workspace
                    .pending_filing_destinations
                    .iter()
                    .filter(|(value, description, _, _)| {
                        value.to_lowercase().contains(&needle)
                            || description.to_lowercase().contains(&needle)
                    })
                    .map(|(value, description, _, _)| crate::minibuffer::Candidate {
                        value: value.clone(),
                        description: description.clone(),
                    })
                    .collect()
            }),
            std::rc::Rc::new(move |workspace, heading, window, cx| {
                let Some((_, node_id)) = workspace
                    .pending_filing_destinations
                    .iter()
                    .find(|(value, ..)| *value == heading)
                    .map(|(_, _, host, node_id)| (*host, node_id.clone()))
                else {
                    // An unknown path is a label the user is naming as they
                    // type it: labelling mints what the path names.
                    workspace.label_card(host, target.clone(), &heading, window, cx);
                    return;
                };
                if matches!(node_id, rho_desk::cells::Id::Label(_)) {
                    workspace.label_card(host, target.clone(), &heading, window, cx);
                    return;
                }
                workspace.file_under(host, target.clone(), node_id, &heading, window, cx);
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    /// Puts a thing under a place. On a dealt card it is a verdict like any
    /// other, so the dealer sees it and the journal records it; on a map row
    /// it is the one parent cell and its undo.
    fn file_under(
        &mut self,
        host: HostId,
        target: rho_desk::cells::Id,
        parent: rho_desk::cells::Id,
        heading: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dealt = self
            .card_in_view(cx)
            .is_some_and(|card| card.identity.node_id == target);
        if dealt {
            if !self.submit_tree_verdict(
                None,
                crate::desk_view::DeskVerdict::File { parent },
                crate::dashboard::DealerVerdict::File,
                format!("file under {heading}"),
                window,
                cx,
            ) {
                self.echo(
                    "file: the deal card disappeared",
                    StyleClass::SystemInfo,
                    cx,
                );
            }
            return;
        }
        let writes = vec![rho_desk::cells::CellWrite {
            id: target,
            property: rho_desk::cells::Property::Parent(Some(parent)),
        }];
        let undo = self.desk_cells.inverse_writes(host, &writes);
        let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
            return;
        };
        self.record_desk_semantic_undo(host, stamp, undo, cx);
        self.echo(&format!("file under {heading}"), StyleClass::SystemInfo, cx);
    }

    fn finish_deal_verdict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A verdict from Home is a row leaving the list. The reader is
        // looking at the list, so nothing is opened for them.
        if self.home_in_view() {
            self.refresh_dashboard(window, cx);
            return;
        }
        // The card was read as an ordinary surface, so a verdict just closes
        // it; what comes next is the next pull, not a queue step.
        self.close_current_surface(window, cx);
        self.pull_card(window, cx);
    }

    fn journal_dealer_verdict(
        verdict: crate::dashboard::DealerVerdict,
    ) -> rho_journal::DealerVerdict {
        match verdict {
            crate::dashboard::DealerVerdict::Skip => rho_journal::DealerVerdict::Skip,
            crate::dashboard::DealerVerdict::Done => rho_journal::DealerVerdict::Done,
            crate::dashboard::DealerVerdict::Mute => rho_journal::DealerVerdict::Mute,
            crate::dashboard::DealerVerdict::Defer => rho_journal::DealerVerdict::Defer,
            crate::dashboard::DealerVerdict::Open => rho_journal::DealerVerdict::Open,
            crate::dashboard::DealerVerdict::File => rho_journal::DealerVerdict::File,
        }
    }

    fn complete_verdict_undo(
        &mut self,
        entry: VerdictUndo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let VerdictUndo { verb, state, .. } = entry;
        let VerdictUndoState::DeskVerdict { card, verdict, .. } = state else {
            return;
        };
        // A discarded thread was ignored in Slack, so taking the verdict
        // back means following it again there; the card is dealt as before
        // once it is the user's again.
        if matches!(verdict, crate::dashboard::DealerVerdict::Mute)
            && let Some(thread) = self.dashboard.card_thread(card.identity.clone())
        {
            self.slack_follow_thread(&thread, cx);
        }
        self.dashboard.clear_skip(&card.identity);
        rho_journal::record(rho_journal::Event::VerdictUndone {
            card: Self::journal_card_identity(&card.identity),
            verdict: Self::journal_dealer_verdict(verdict),
        });
        self.echo(
            &format!("undid {verb}: {}", card.breadcrumb),
            StyleClass::SystemInfo,
            cx,
        );
        self.open_card(*card, window, cx);
        self.refresh_dashboard(window, cx);
    }

    /// Closes a thread card because Slack said the thread is not the user's
    /// any more. No undo entry: the verdict was made in another client, and
    /// `shift-u` here could not take it back there.
    pub(crate) fn mute_thread_card(
        &mut self,
        card: crate::dashboard::DealCardId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((writes, verdict)) = self.desk_cells.verdict_writes(
            card.host,
            &card.node_id,
            crate::desk_view::DeskVerdict::Mute,
        ) else {
            return;
        };
        self.apply_desk_writes(card.host, writes, Some(verdict), window, cx);
    }

    /// Writes a done verdict on each node and leaves one undo entry for the
    /// lot. Unlike a dealt verdict this does not wait for the daemon's
    /// answer before arming the undo: there is no card in front of the user
    /// to hold, and a mutation the daemon refuses simply has no verdict
    /// event for the undo to find, which reports itself.
    pub(crate) fn mark_cards_done(
        &mut self,
        host: HostId,
        nodes: Vec<(rho_desk::cells::Id, rho_desk::cells::SlackTs)>,
        verb: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let mut applied = Vec::new();
        for (node, cursor) in nodes {
            // Each unit gets its own log entry, so `shift-u` puts the whole
            // batch back and the daemon checks each cursor against the one
            // that was there.
            let writes = match &node {
                rho_desk::cells::Id::Slack(unit) => {
                    self.desk_cells.slack_done_writes(host, unit, cursor)
                }
                _ => {
                    self.desk_cells
                        .verdict_writes(host, &node, crate::desk_view::DeskVerdict::Done)
                }
            };
            let Some((writes, event)) = writes else {
                continue;
            };
            let Some(stamp) = self.apply_desk_writes(host, writes, Some(event), window, cx) else {
                continue;
            };
            applied.push((node, stamp));
        }
        let count = applied.len();
        if count > 0 {
            let undo = self.next_verdict_undo(
                verb,
                VerdictUndoState::MarkedReadBefore {
                    host,
                    nodes: applied,
                },
            );
            self.restore_verdict_undo(undo);
        }
        count
    }

    fn undo_marked_read_before(
        &mut self,
        entry: VerdictUndo,
        host: HostId,
        nodes: Vec<(rho_desk::cells::Id, rho_desk::cells::Stamp)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut undone = 0;
        for (node, at) in nodes {
            let Some((writes, verdict)) = self.desk_cells.undo_verdict_writes(host, &node, at)
            else {
                continue;
            };
            if self
                .apply_desk_writes(host, writes, Some(verdict), window, cx)
                .is_some()
            {
                undone += 1;
            }
        }
        if undone == 0 {
            self.restore_verdict_undo(entry);
            self.echo("undo: notes are unavailable", StyleClass::SystemInfo, cx);
            return;
        }
        rho_journal::record(rho_journal::Event::SlackMarkReadBeforeUndone { cards: undone });
        self.echo(
            &format!("undid {}: {undone} reopened", entry.verb),
            StyleClass::SystemInfo,
            cx,
        );
        self.refresh_dashboard(window, cx);
    }

    fn next_verdict_undo(&mut self, verb: String, state: VerdictUndoState) -> VerdictUndo {
        let sequence = self.next_verdict_undo_sequence;
        self.next_verdict_undo_sequence = self
            .next_verdict_undo_sequence
            .checked_add(1)
            .expect("verdict undo sequence overflow");
        VerdictUndo {
            sequence,
            verb,
            state,
        }
    }

    fn restore_verdict_undo(&mut self, entry: VerdictUndo) {
        let index = undo_sequence_insert_position(
            self.verdict_undo.iter().map(|candidate| candidate.sequence),
            entry.sequence,
        );
        self.verdict_undo.insert(index, entry);
    }

    pub(crate) fn undo_verdict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phone.enabled
            && (self.phone_snap_in_progress()
                || self.phone_current_deal_has_pending_tree_verdict(cx))
        {
            return;
        }
        let Some(entry) = self.verdict_undo.pop() else {
            self.echo("nothing to undo", StyleClass::SystemInfo, cx);
            return;
        };
        match entry.state.clone() {
            VerdictUndoState::MarkedReadBefore { host, nodes } => {
                self.undo_marked_read_before(entry, host, nodes, window, cx);
            }
            VerdictUndoState::DeskVerdict { host, node, at, .. } => {
                // Undo is the log's own inverse: the daemon accepts `Undone`
                // only while the cells still hold what the verdict wrote.
                let Some((writes, verdict)) = self.desk_cells.undo_verdict_writes(host, &node, at)
                else {
                    self.restore_verdict_undo(entry);
                    self.echo("undo: the note is unavailable", StyleClass::SystemInfo, cx);
                    return;
                };
                let Some(stamp) = self.apply_desk_writes(host, writes, Some(verdict), window, cx)
                else {
                    self.restore_verdict_undo(entry);
                    self.echo("undo: the note is unavailable", StyleClass::SystemInfo, cx);
                    return;
                };
                self.pending_tree_undos
                    .insert((host, stamp), PendingTreeUndo { entry });
            }
        }
    }

    fn submit_tree_verdict(
        &mut self,
        target_node: Option<rho_desk::cells::Id>,
        dealt: crate::desk_view::DeskVerdict,
        verdict: crate::dashboard::DealerVerdict,
        verb: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.phone.enabled
            && (self.phone_snap_in_progress()
                || self.phone_current_deal_has_pending_tree_verdict(cx))
        {
            return false;
        }
        let Some(card) = self.card_in_view(cx) else {
            return false;
        };
        let event = crate::dashboard::DealerEvent {
            card: card.identity.clone(),
            kind: card.kind,
            verdict,
            at: chrono::Local::now().fixed_offset(),
            skip_until: None,
        };
        // The verdict lands on the card's own node: an agent card marks the
        // agent, a thread card marks the thread. `target_node` is the room
        // snooze, which deliberately files one level up.
        let node_id = target_node
            .clone()
            .unwrap_or_else(|| card.identity.node_id.clone());
        let phone_verdict = self.phone.enabled.then_some(match dealt {
            crate::desk_view::DeskVerdict::Done => rho_journal::PhoneVerdict::Done,
            crate::desk_view::DeskVerdict::Mute => rho_journal::PhoneVerdict::Mute,
            crate::desk_view::DeskVerdict::Defer { .. } => rho_journal::PhoneVerdict::Defer,
            crate::desk_view::DeskVerdict::Todo { .. } => rho_journal::PhoneVerdict::Todo,
            crate::desk_view::DeskVerdict::File { .. } => rho_journal::PhoneVerdict::File,
        });
        // `x` on a Slack card silences the unit in Slack too: the same
        // keystroke that closes the card here stops Slack raising it
        // anywhere else, by unfollowing a thread or marking a conversation
        // read.
        if matches!(dealt, crate::desk_view::DeskVerdict::Mute)
            && target_node.is_none()
            && let Some(unit) = self.dashboard.card_thread(card.identity.clone())
        {
            self.slack_silence_unit(&unit, cx);
        }
        let Some((writes, applied)) = self.desk_cells.verdict_writes(card.host, &node_id, dealt)
        else {
            return false;
        };
        // A verdict names the card it took, not the place the card was
        // filed. The breadcrumb is the path above an agent's card, so `done`
        // over an agent under a label said the label's name and never the
        // agent's, and two agents in one label read identically.
        let subject = card
            .agent_id
            .map(|agent_id| self.registry.agent_human_name(agent_id))
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| card.breadcrumb.clone());
        let echo = format!("{verb}: {subject}");
        // A todo hangs a note under the card. Empty, it comes back in a week
        // reading only `defer …`, so it is given the card's own words.
        let todo_note = match &applied.1 {
            rho_desk::cells::VerdictEvent::Applied {
                verdict: rho_desk::cells::Verdict::Todo { note },
                ..
            } => Some(note.clone()),
            _ => None,
        };
        let Some(stamp) = self.apply_desk_writes(card.host, writes, Some(applied), window, cx)
        else {
            return false;
        };
        if let Some(note) = todo_note {
            self.pending_desk_texts
                .insert((card.host, stamp), vec![(note, card.breadcrumb.clone())]);
        }
        let undo = self.next_verdict_undo(
            verb,
            VerdictUndoState::DeskVerdict {
                card: Box::new(card.clone()),
                verdict,
                host: card.host,
                node: node_id,
                at: stamp,
            },
        );
        self.pending_tree_verdicts.insert(
            (card.host, stamp),
            PendingTreeVerdict {
                event,
                echo,
                undo,
                phone_verdict,
            },
        );
        true
    }

    /// Sibling order is derived from `(CreatedAt, Id)`, so moving a row
    /// among its siblings has no cell to write in this slice.
    fn reordering_unavailable(&mut self, cx: &mut Context<Self>) {
        self.notice_on(
            None,
            "reordering rows is not available",
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// Writes one fact outside the dealer: no verdict, no undo entry.
    fn set_node_fact(
        &mut self,
        host: HostId,
        id: rho_desk::cells::Id,
        property: rho_desk::cells::Property,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let writes = vec![rho_desk::cells::CellWrite { id, property }];
        self.apply_desk_writes(host, writes, None, window, cx)
            .is_some()
    }

    /// `n a` with an area chosen: the draft page carries the fields, so
    /// there is no transient in front of it. The area is the agent's
    /// parent and where its workdir is inherited from; the body is focused
    /// so typing composes the first message straight away.
    pub(crate) fn new_agent_in_area(
        &mut self,
        area: Option<(HostId, rho_desk::cells::Id)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workdir = area
            .clone()
            .and_then(|(host, node_id)| self.area_workdir(host, node_id))
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
            view.set_role_text(rho_agents::create::DEFAULT_ROLE, cx);
            view.set_start_text(rho_agents::create::DEFAULT_START, cx);
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

    /// The workdir a new thing under an area inherits: the area's own
    /// file, the nearest ancestor with one, then the agent that owns the
    /// area (or the area itself when it is an agent node). The caller
    /// falls back to the host's only workdir.
    fn area_workdir(&self, host: HostId, node_id: rho_desk::cells::Id) -> Option<HostPath> {
        if let Some(project) = self.desk_cells.inherited_workdir(host, &node_id) {
            // A label's project names its own machine, which is usually
            // the one the area is on and need not be.
            let host = self
                .registry
                .hosts()
                .map(|(id, _)| id)
                .find(|id| self.registry.host_machine_seed(*id) == project.host)
                .unwrap_or(host);
            return Some(HostPath {
                host,
                path: project.path,
            });
        }
        let agent_id = self.desk_cells.nearest_agent(host, &node_id)?;
        self.agent_workdir(agent_id)
    }

    /// The usage screen, built once and kept. A series that arrives while
    /// another screen is in view still lands in it.
    /// Show `chart` over `days`: ask the daemon for the range it needs, hand
    /// the screen what this client already holds so it draws at once, and
    /// display it. Picking another chart from the menu comes back through
    /// here and redraws the same surface.
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
                self.hosts.broadcast(|| ClientMessage::QuotaHistory);
                let history = self.hosts.merged_quota_history();
                let active = self.hosts.active_quota_namespaces();
                view.update(cx, |view, cx| view.quota_arrived(history, active, cx));
            }
            crate::usage::Request::GlobalUsage { since_ms } => {
                self.hosts
                    .broadcast(|| ClientMessage::GlobalUsage { since_ms });
                let usage = self.usage.merged_global();
                view.update(cx, |view, cx| view.global_usage_arrived(usage, cx));
            }
            crate::usage::Request::AgentCostDistribution { since_ms } => {
                self.hosts
                    .broadcast(|| ClientMessage::AgentCostDistribution { since_ms });
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

    /// What a new agent under an area starts as: the area's inherited
    /// workdir, a fresh workspace on the auto base, and the default role.
    /// The fields on the draft page override this; a heading draft has no
    /// page, so this is all it gets.
    fn launch_for_area(
        &self,
        host: HostId,
        node_id: rho_desk::cells::Id,
    ) -> Result<(HostId, rho_ui_proto::StartMode, AgentRole), String> {
        let workdir = self
            .area_workdir(host, node_id)
            .or_else(|| self.only_workdir())
            .ok_or_else(|| "new agent: no working directory for this area".to_owned())?;
        let role = parse_agent_role(rho_agents::create::DEFAULT_ROLE)?;
        Ok((
            workdir.host,
            rho_ui_proto::StartMode::NewOn {
                repo: workdir.path,
                revset: rho_agents::create::AUTO_BASE_REVSET.to_owned(),
            },
            role,
        ))
    }

    /// `enter` on a bound Desk heading opens its agent.
    fn dashboard_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        match self.dashboard.cursor_target(&self.registry, cx) {
            Some(RowTarget::TreeAgent { agent_id, .. }) => self.open_agent(agent_id, window, cx),
            Some(RowTarget::TreePage { page_id, .. }) => {
                self.open_browser_page(page_id, window, cx)
            }
            // `enter` opens what the row is, and a note's surface is the
            // note. Staffing one is `r`, which writes the draft.
            Some(RowTarget::TreeTopic {
                host,
                node_id,
                first_attention,
                ..
            }) => {
                if !self.open_note(host, node_id.clone(), window, cx) {
                    match first_attention.or_else(|| {
                        self.dashboard
                            .first_tree_agent_for_topic((host, node_id.clone()))
                    }) {
                        Some(agent_id) => self.open_agent(agent_id, window, cx),
                        None => {
                            self.dashboard
                                .open_new_tree_draft((host, node_id), window, cx);
                            self.dashboard_focus_draft(window, cx);
                        }
                    }
                }
            }
            Some(RowTarget::NewTreeDraft((topic_host, node_id))) => {
                if !self.require_connected(cx) {
                    return;
                }
                let Some(body) = self.dashboard.take_new_draft(cx) else {
                    return;
                };
                let (host, start, role) = match self.launch_for_area(topic_host, node_id.clone()) {
                    Ok(launch) => launch,
                    Err(message) => {
                        self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                        return;
                    }
                };
                self.pending_agent_filing = Some((host, node_id));
                self.send_to_host(
                    host,
                    ClientMessage::NewAgent {
                        role,
                        start,
                        content: Some(vec![ContentPart::Text { text: body }]),
                    },
                );
                self.refresh_dashboard(window, cx);
            }
            _ => {}
        }
    }

    /// Insert-mode enter: send when the cursor is in a draft, and drop
    /// back to normal mode on the row the send leaves behind. In document
    /// text it is a newline — dispatched explicitly, because propagating
    /// would fall through to the transcript prompt's `RhoGui > Editor`
    /// SubmitPrompt binding, which swallows the key.
    fn dashboard_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.dashboard.cursor_target(&self.registry, cx),
            Some(
                crate::dashboard::RowTarget::NewDraft
                    | crate::dashboard::RowTarget::NewTreeDraft(_)
            )
        ) {
            self.dashboard_open(window, cx);
            if let Ok(action) = cx.build_action("vim::NormalBefore", None) {
                window.dispatch_action(action, cx);
            }
        } else {
            // Not propagate: the fall-through lands on the transcript
            // prompt's SubmitPrompt binding, which eats the key and leaves
            // the note without its newline.
            window.dispatch_action(Box::new(editor::actions::Newline), cx);
        }
    }

    fn dashboard_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.dashboard.cursor_target(&self.registry, cx) {
            Some(crate::dashboard::RowTarget::TreeAgent { agent_id, .. }) => {
                self.open_agent(agent_id, window, cx)
            }
            Some(crate::dashboard::RowTarget::TreeTopic {
                host,
                node_id,
                first_attention,
                ..
            }) => match first_attention.or_else(|| {
                self.dashboard
                    .first_tree_agent_for_topic((host, node_id.clone()))
            }) {
                Some(agent_id) => self.open_agent(agent_id, window, cx),
                None => {
                    self.dashboard
                        .open_new_tree_draft((host, node_id), window, cx);
                    self.dashboard_focus_draft(window, cx);
                }
            },
            _ => cx.propagate(),
        }
    }

    pub(crate) fn dashboard_enter_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.enter_insert_mode(window, cx);
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

    #[cfg(test)]
    pub(crate) fn insert_when_shown_for_test(&self) -> bool {
        self.insert_when_shown
    }

    /// A freshly opened draft row only exists on screen after a sync:
    /// splice it in now so the pending cursor lands on it, then enter
    /// insert there — never on the read-only row the cursor came from.
    pub(crate) fn dashboard_focus_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_dashboard(window, cx);
        self.dashboard_enter_insert(window, cx);
    }

    /// Vim-style `o`/`O` on a heading line: insert a sibling node below or
    /// above. Anywhere else the action propagates so vim's own open-line
    /// binding runs.
    /// `above` makes no difference: sibling order is `(CreatedAt, NodeId)`
    /// and `CreatedAt` cannot be rewritten, so a new row lands after its
    /// siblings either way. The semantic `O` says so in the echo area.
    fn dashboard_insert_heading(
        &mut self,
        _above: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  _window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let title = input.trim();
                if !title.is_empty()
                    && let Some((host, relative)) = workspace.dashboard.tree_node_at_cursor(cx)
                {
                    workspace.append_tree_heading(host, relative, false, title, _window, cx);
                }
            },
        );
        self.open_prompt(
            "new topic:",
            std::rc::Rc::new(|_, _, _| Vec::new()),
            on_submit,
            window,
            cx,
        );
    }

    /// Single-letter Desk verbs only apply on a heading line of the focused
    /// Desk; otherwise the caller propagates the key back to vim.
    fn dashboard_verb_applies(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        self.dashboard.is_focused(window, cx) && self.dashboard.cursor_on_heading_line(cx)
    }

    /// The new-heading verbs also apply when there is nothing to stand on:
    /// a desk with no rows has no heading line, and without this the very
    /// first note could never be written from the keyboard.
    fn dashboard_new_heading_applies(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        self.dashboard.is_focused(window, cx)
            && (self.dashboard.cursor_on_heading_line(cx) || self.dashboard.tree_is_empty())
    }

    fn dashboard_new_heading(&mut self, child: bool, window: &mut Window, cx: &mut Context<Self>) {
        // An empty desk has no row to hang the new one off, so the first
        // note is a root: the key still works on a desk with nothing in it.
        // A cursor left in a removed excerpt still names its old node, and
        // the verb must not be swallowed: an unknown row falls back to a root.
        let (host, relative) = match self.dashboard.tree_node_at_cursor(cx) {
            Some((host, node_id)) => (
                host,
                self.desk_cells.node(host, &node_id).map(|node| node.id),
            ),
            None => match self.hosts.primary() {
                Some(host) if self.desk_cells.is_synced(host) => (host, None),
                _ => return,
            },
        };
        let created = match relative {
            Some(relative) => self.desk_cells.new_note_writes(host, &relative, child),
            None => self.desk_cells.create_note_writes(host, None),
        };
        let Some((created, writes)) = created else {
            return;
        };
        let undo = self.desk_cells.delete_writes(created.clone());
        let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
            return;
        };
        self.dashboard.move_to_tree_node_when_ready(host, created);
        self.sync_tree_dashboard(host, window, cx);
        let transaction_id = self.record_desk_semantic_undo(host, stamp, undo, cx);
        self.pending_semantic_group = Some(transaction_id);
        // The structural shortcut is the equivalent of Vim's `o`: the
        // new row is ready for text immediately, rather than consuming
        // the first title characters as normal-mode commands.
        self.dashboard_enter_insert(window, cx);
    }

    fn paste_desk_semantic_subtree(
        &mut self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(capture) = self.desk_semantic_clipboard.clone() else {
            return;
        };
        let Some((root, writes, texts)) = self.desk_cells.paste_writes(host, &node_id, &capture)
        else {
            return;
        };
        let undo = self.desk_cells.delete_writes(root.clone());
        self.dashboard
            .move_to_tree_node_when_ready(host, root.clone());
        let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
            return;
        };
        if !texts.is_empty() {
            self.pending_desk_texts.insert((host, stamp), texts);
        }
        cx.on_next_frame(window, move |this, window, cx| {
            this.dashboard.move_to_tree_node_when_ready(host, root);
            this.sync_tree_dashboard(host, window, cx);
        });
        cx.notify();
        self.record_desk_semantic_undo(host, stamp, undo, cx);
    }

    fn handle_desk_semantic_row_action(
        &mut self,
        buffer_id: text::BufferId,
        action: editor::SemanticRowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((host, node_id)) = self.dashboard.tree_node_for_buffer(buffer_id, cx) else {
            return;
        };
        match action {
            editor::SemanticRowAction::Yank => {
                self.desk_semantic_clipboard = self.desk_cells.capture(host, &node_id, cx);
            }
            editor::SemanticRowAction::Delete => {
                let Some(capture) = self.desk_cells.capture(host, &node_id, cx) else {
                    return;
                };
                let writes = self.desk_cells.delete_writes(node_id.clone());
                let undo = self.desk_cells.inverse_writes(host, &writes);
                self.desk_semantic_clipboard = Some(capture);
                let focus = self.desk_cells.row_after_delete(host, &node_id);
                self.desk_semantic_paste_target = focus.clone().map(|focus| (host, focus));
                let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
                    return;
                };
                if let Some(focus) = focus {
                    // Vim finishes its linewise delete after emitting the
                    // semantic action and can overwrite a synchronous cursor
                    // move with an anchor into the removed excerpt. Re-aim at
                    // the surviving sibling after that dispatch completes.
                    cx.on_next_frame(window, move |this, window, cx| {
                        this.dashboard.move_to_tree_node_when_ready(host, focus);
                        this.sync_tree_dashboard(host, window, cx);
                        if let Ok(action) = cx.build_action("vim::NormalBefore", None) {
                            window.dispatch_action(action, cx);
                        }
                    });
                    cx.notify();
                }
                self.record_desk_semantic_undo(host, stamp, undo, cx);
            }
            editor::SemanticRowAction::Paste { .. } => {
                self.desk_semantic_paste_target = None;
                self.paste_desk_semantic_subtree(host, node_id, window, cx);
            }
            editor::SemanticRowAction::Open { above } => {
                // Sibling order is `(CreatedAt, NodeId)` and `CreatedAt`
                // cannot be rewritten, so `O` opens after the row like `o`.
                if above {
                    self.echo(
                        "open above: new rows land after their siblings",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
                let Some((created, writes)) =
                    self.desk_cells.new_note_writes(host, &node_id, false)
                else {
                    return;
                };
                let undo = self.desk_cells.delete_writes(created.clone());
                self.dashboard.move_to_tree_node_when_ready(host, created);
                let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
                    return;
                };
                let transaction_id = self.record_desk_semantic_undo(host, stamp, undo, cx);
                self.pending_semantic_group = Some(transaction_id);
                self.dashboard_enter_insert(window, cx);
            }
            editor::SemanticRowAction::Indent { outdent } => {
                let Some(writes) = self
                    .desk_cells
                    .structure_move_writes(host, &node_id, !outdent)
                else {
                    return;
                };
                let undo = self.desk_cells.inverse_writes(host, &writes);
                let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
                    return;
                };
                self.record_desk_semantic_undo(host, stamp, undo, cx);
            }
        }
    }

    fn undo_desk_semantic_action(
        &mut self,
        transaction_id: clock::Lamport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(undo) = self.desk_semantic_undo.remove(&transaction_id) else {
            return;
        };
        self.apply_desk_writes(undo.host, undo.writes, None, window, cx);
    }

    fn append_tree_heading(
        &mut self,
        host: HostId,
        relative: rho_desk::cells::Id,
        child: bool,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.append_tree_heading_at(host, relative, child, title, window, cx)
            .is_some()
    }

    fn append_tree_heading_at(
        &mut self,
        host: HostId,
        relative: rho_desk::cells::Id,
        child: bool,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<rho_desk::cells::Id> {
        let (created, writes) = self.desk_cells.new_note_writes(host, &relative, child)?;
        self.dashboard
            .move_to_tree_node_when_ready(host, created.clone());
        self.apply_desk_writes(host, writes, None, window, cx)?;
        self.sync_tree_dashboard(host, window, cx);
        self.dashboard
            .rename_cursor_topic(title, cx)
            .then_some(created)
    }

    fn dashboard_delete_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((host, node_id)) = self.dashboard.tree_node_at_cursor(cx) else {
            return;
        };
        let empty = self
            .desk_cells
            .buffer(host, &node_id)
            .is_some_and(|buffer| buffer.read(cx).is_empty());
        if !empty {
            self.notice_on(
                None,
                "delete: heading is not empty",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        let focus = self.desk_cells.row_above(host, &node_id);
        let writes = self.desk_cells.delete_writes(node_id);
        let undo = self.desk_cells.inverse_writes(host, &writes);
        if let Some(focus) = focus {
            self.dashboard.move_to_tree_node_when_ready(host, focus);
        }
        let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
            return;
        };
        self.record_desk_semantic_undo(host, stamp, undo, cx);
    }

    fn dashboard_undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(action) = cx.build_action("vim::Undo", None) {
            window.dispatch_action(action, cx);
        }
    }

    fn dashboard_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.dashboard.next_now(&self.registry, window, cx) else {
            self.notice_on(None, "NOW is clear", StyleClass::SystemInfo, cx);
            return;
        };
        self.preview_agent(agent_id, window, cx);
        window.focus(&self.dashboard.focus_handle(cx), cx);
    }

    fn dashboard_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard.back(&self.registry, window, cx) {
            window.focus(&self.dashboard.focus_handle(cx), cx);
        }
    }

    fn prompt_dashboard_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, cx: &gpui::App| {
            workspace
                .dashboard
                .heading_candidates(&workspace.registry, input.trim(), cx)
                .into_iter()
                .map(|(value, description)| crate::commands::Candidate { value, description })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let found = workspace.dashboard.jump_to_heading(
                    input.trim(),
                    &workspace.registry,
                    window,
                    cx,
                );
                rho_journal::record(rho_journal::Event::Find {
                    query: input.trim().to_owned(),
                    target: "desk_heading".to_owned(),
                    found,
                });
                if found {
                    window.focus(&workspace.dashboard.focus_handle(cx), cx);
                }
            },
        );
        self.open_prompt("Note:", complete, on_submit, window, cx);
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
                rho_agents::transcript::StorePoint {
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
                model.request_history(rho_agents::agent_view::HistoryWant::All, window, cx);
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
                model.request_history(rho_agents::agent_view::HistoryWant::All, window, cx);
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
        if !self.dashboard.is_focused(window, cx) {
            return false;
        }
        let editor = self.dashboard.editor().clone();
        if !self.search_editor(&editor, query, window, cx) {
            self.notice_on(None, "search: no match", StyleClass::SystemInfo, cx);
        }
        true
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

    /// `/` on the dashboard, which searches its own buffer with the same
    /// query register as everything else.
    fn prompt_dashboard_search(
        &mut self,
        direction: search::Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_for_query(direction, window, cx, |workspace, query, window, cx| {
            let editor = workspace.dashboard.editor().clone();
            if !workspace.search_editor(&editor, query, window, cx) {
                workspace.notice_on(None, "search: no match", StyleClass::SystemInfo, cx);
            }
        });
    }

    fn prompt_dashboard_rename_topic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                if !input.trim().is_empty()
                    && workspace.dashboard.rename_cursor_topic(input.trim(), cx)
                {
                    workspace.refresh_dashboard(_window, cx);
                }
            },
        );
        self.open_prompt(
            "rename topic:",
            std::rc::Rc::new(|_, _, _| Vec::new()),
            on_submit,
            window,
            cx,
        );
    }

    /// The home-mode dashboard beside the active context's preview.
    fn render_rail(
        &mut self,
        show_preview: bool,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let _ = &cx;
        let container = div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .py(px(2.))
            .flex()
            .flex_col()
            .font_family(text_style.font_family.clone())
            .text_size(text_style.font_size)
            .line_height(text_style.line_height)
            .text_color(text_style.color)
            .key_context("RhoDashboard");
        let compact_dashboard = self.phone.enabled;
        let container = container
            // The dashboard owns the preview card's reclaimed horizontal
            // space, rather than leaving a blank wrapper beside the card.
            .w(if show_preview {
                gpui::relative(0.55)
            } else {
                gpui::relative(1.0)
            })
            // The desktop gutter wastes too much of a phone's width.
            .pl(px(if compact_dashboard { 6. } else { 24. }))
            .pr(px(if compact_dashboard { 6. } else { 24. }));
        let dashboard = div()
            .id("dashboard-rail")
            .flex_grow(1.0)
            .min_h_0()
            .relative()
            .overflow_hidden()
            .child(self.dashboard.editor().clone());
        container.child(dashboard).into_any_element()
    }

    /// The selected agent's preview editor.
    fn selected_preview_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<editor::Editor>> {
        let agent_id = self.dashboard_preview?;
        let model = self.models.get(&agent_id)?.clone();
        Some(model.update(cx, |model, cx| model.preview_editor(window, cx)))
    }

    fn selected_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if let Some(preview) = self.pages.preview() {
            return Some(
                div()
                    .size_full()
                    .overflow_hidden()
                    .child(preview.view.clone())
                    .into_any_element(),
            );
        }
        self.selected_preview_editor(window, cx).map(|editor| {
            div()
                .size_full()
                .overflow_hidden()
                .child(editor)
                .into_any_element()
        })
    }

    fn render_deal_why(
        &self,
        card: &crate::dashboard::DealCard,
        text_style: &gpui::TextStyle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let path = match self.dashboard.card_target(card.identity.clone()) {
            crate::dashboard::CardTarget::Page(page) => {
                let leaf = rho_browser::live_page_name(page).unwrap_or_else(|| "page".to_owned());
                format!("{} / {leaf}", card.breadcrumb.replace(" › ", " / "))
            }
            // A Slack deal shows the conversation and nothing else: the
            // words are on screen already, and the state segment says whose
            // turn it is.
            crate::dashboard::CardTarget::Thread(unit) => {
                let conversation = card.room.clone().unwrap_or_else(|| "slack".to_owned());
                match unit.thread.is_some() {
                    true => format!("{conversation} / thread"),
                    false => conversation,
                }
            }
            _ => card.breadcrumb.replace(" › ", " / "),
        };
        let path = Self::truncate_outline_path(&path);
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
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DashboardDealExit), cx)
                        })
                        .child("close"),
                )
                .child(
                    div()
                        .id("deal-touch-done")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DashboardDealDone), cx)
                        })
                        .child("done"),
                )
                .child(
                    div()
                        .id("deal-touch-defer")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DashboardDealSnooze), cx)
                        })
                        .child("defer"),
                )
                .child(
                    div()
                        .id("deal-touch-mute")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DashboardDealMute), cx)
                        })
                        .child("mute"),
                )
                .child(
                    div()
                        .id("deal-touch-next")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DashboardDealNext), cx)
                        })
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
        let quota = self.hosts.merged_quota_summaries();
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
        let agent_in_view = match (self.overview_open, &self.active_surface().key) {
            (false, SurfaceKey::Transcript(agent_id)) => Some(*agent_id),
            _ => None,
        };
        if let Some(card) = self.open_card_in_view(cx)
            && echo.is_none()
            && agent_in_view.is_none()
        {
            return self.render_deal_why(&card, text_style, window, cx);
        }
        let path = if self.overview_open {
            let path = self
                .dashboard
                .cursor_breadcrumb(cx)
                .unwrap_or_else(|| "map".to_owned());
            if matches!(
                self.dashboard.cursor_target(&self.registry, cx),
                Some(crate::dashboard::RowTarget::NewTreeDraft(_))
            ) {
                format!("{path} / new agent")
            } else {
                path
            }
        } else {
            match &self.active_surface().key {
                SurfaceKey::Transcript(agent_id) => {
                    let leaf = self.registry.agent_display_label(*agent_id);
                    self.dashboard
                        .breadcrumb_for_agent(*agent_id, cx)
                        .map_or(leaf.clone(), |path| format!("{path} / {leaf}"))
                }
                SurfaceKey::Browser(page) => {
                    let leaf =
                        rho_browser::live_page_name(*page).unwrap_or_else(|| "page".to_owned());
                    self.dashboard
                        .breadcrumb_for_page(*page, cx)
                        .map_or(leaf.clone(), |path| format!("{path} / {leaf}"))
                }
                key => self.surface_name(key),
            }
        };
        let state = agent_in_view
            .filter(|_| echo.is_none())
            .and_then(|agent_id| {
                let facts = self.registry.agent_facts(agent_id);
                crate::dashboard::agent_state_label(&facts, chrono::Local::now().fixed_offset())
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
        self.status_row(
            div().child(left).children(state).children(unseen),
            right,
            text_style,
            window,
            cx,
        )
    }

    fn render_workspace(
        &mut self,
        window: &mut Window,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Home mode: the dashboard owns the keyboard, so it owns the frame;
        // the surface area is its preview. With nothing selected there is
        // nothing to preview — the dashboard takes the whole frame.
        // Modal overlays borrow keyboard focus; the frame stays in the mode
        // recorded beneath the overlay for its whole replacement chain.
        let home = self.overview_open;
        {
            let focused_surface = if home {
                crate::telemetry::SurfaceKind::Dashboard
            } else {
                self.active_surface().view.telemetry_kind()
            };
            let visible_surfaces = focused_surface.bit();
            crate::telemetry::record_surfaces(focused_surface, visible_surfaces);
        }
        self.sync_diff_visibility(!home, cx);
        let web_preview_visible = self.pages.preview().is_some();
        let show_surface = !home || self.dashboard_preview.is_some() || web_preview_visible;
        let rail = home.then(|| self.render_rail(show_surface, text_style, cx));
        // Same hairline the rail uses against the preview.
        let separator_color = cx.theme().colors().border_variant.opacity(0.6);
        let mut preview_text_style = text_style.clone();
        preview_text_style.font_size =
            (text_style.font_size.to_pixels(window.rem_size()) * 0.85).into();
        preview_text_style.line_height =
            (text_style.line_height_in_pixels(window.rem_size()) * 0.85).into();
        let preview = home.then(|| self.selected_preview(window, cx)).flatten();
        let surface = show_surface.then(|| {
            let element = div().flex_1().min_w_0().min_h_0();
            // Home mode uses a narrow preview card with the original top
            // inset, anchored to the bottom-right of the surface area rather
            // than competing with the dashboard for an equal split.
            // The sheet shows the agent's *document* editor: the same
            // transcript buffers composed without the prompt, ending where
            // the words end. Its bottom bar carries the context the prompt
            // row shows in work mode.
            if home {
                element.flex().flex_col().child(
                    div()
                        .w_full()
                        .h(gpui::relative(0.98))
                        .ml_auto()
                        .mt_auto()
                        .border_1()
                        .border_color(separator_color)
                        .rounded_t_md()
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .min_w_0()
                                .min_h_0()
                                .overflow_hidden()
                                .children(preview),
                        ),
                )
            } else {
                element
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .child(self.render_surface(self.active_surface()))
            }
        });
        div()
            .flex()
            .flex_row()
            .w_full()
            .flex_grow(1.0)
            .min_h_0()
            .children(rail)
            .children(surface)
            .into_any_element()
    }

    fn dashboard_mode(&self, window: &Window, cx: &App) -> bool {
        let dashboard = self.dashboard.focus_handle(cx);
        let browser_preview_focused = self
            .pages
            .preview()
            .is_some_and(|preview| preview.view.read(cx).focus_handle(cx).is_focused(window));
        self.overview_open
            || self.overlay_return_focus.as_ref() == Some(&dashboard)
            || browser_preview_focused
    }

    /// Hidden surfaces stay alive as editor buffers, but they must not turn
    /// worktree events into jj manifest traffic. Only the visible diff may
    /// refresh.
    fn sync_diff_visibility(&self, surface_visible: bool, cx: &mut Context<Self>) {
        let visible = if surface_visible {
            match &self.active_surface().view {
                SurfaceView::Diff(view) => HashSet::from([view.read(cx).model().entity_id()]),
                _ => HashSet::new(),
            }
        } else {
            HashSet::new()
        };
        let models = self
            .surfaces
            .values()
            .flatten()
            .filter_map(|surface| match &surface.view {
                SurfaceView::Diff(view) => Some(view.read(cx).model()),
                _ => None,
            })
            .fold(HashMap::new(), |mut models, model| {
                models.entry(model.entity_id()).or_insert(model);
                models
            });
        for (id, model) in models {
            model.update(cx, |model, cx| model.set_visible(visible.contains(&id), cx));
        }
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
            SurfaceView::DeskNode(editor) => div()
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
            SurfaceView::ZulipInbox(view) => div()
                .id("rho-surface-zulip-inbox")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            SurfaceView::ZulipNarrow(view) => div()
                .id("rho-surface-zulip-narrow")
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
            SurfaceView::Diff(view) => div()
                .id("rho-surface-diff")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
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

    pub fn live_agent_targets(&self) -> Vec<crate::commands::Candidate> {
        let mut candidates = Vec::new();
        for agent_id in self.registry.known_agents() {
            let id_label = self.registry.agent_id_label(*agent_id);
            let display_name = self
                .registry
                .agent_display_name(*agent_id)
                .map(str::to_owned);
            candidates.push(crate::commands::Candidate {
                value: id_label.clone(),
                description: display_name.clone().unwrap_or_else(|| "agent".to_owned()),
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

pub(crate) fn resolve_filing_destination(
    destinations: &[(String, String, HostId, rho_desk::cells::Id)],
    candidate: &crate::minibuffer::Candidate,
    occurrence: usize,
) -> Option<(HostId, rho_desk::cells::Id)> {
    destinations
        .iter()
        .filter(|(value, description, _, _)| {
            *value == candidate.value && *description == candidate.description
        })
        .nth(occurrence)
        .map(|(_, _, host, node_id)| (*host, node_id.clone()))
}

/// How a role reads in the chips a transcript shows. Only the tests ask
/// for it as a string; the chips themselves are styled from the family.
#[cfg(test)]
fn agent_role_label(config: AgentRole) -> String {
    match config {
        AgentRole::Advisor { intelligence } => match intelligence {
            AdvisorIntelligence::Medium => "advisor",
            AdvisorIntelligence::High => "advisor-high",
            AdvisorIntelligence::Cheap => "advisor-cheap",
        },
        AgentRole::Engineer { intelligence } => match intelligence {
            EngineerIntelligence::Mini => "eng-mini",
            EngineerIntelligence::Low => "eng-low",
            EngineerIntelligence::Cheap => "eng-cheap",
            EngineerIntelligence::Medium => "eng",
            EngineerIntelligence::High => "eng-high",
            EngineerIntelligence::Ultra => "eng-ultra",
            EngineerIntelligence::Alt => "eng-alt",
            EngineerIntelligence::Gemini => "eng-gemini",
        },
    }
    .to_owned()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .on_modifiers_changed(cx.listener(Self::shift_modifiers_changed))
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
                this.focus_rail(window, cx);
            }))
            .on_action(cx.listener(Self::shell_interrupt))
            .on_action(cx.listener(Self::toggle_voice))
            .on_action(cx.listener(Self::shell_eof))
            .on_action(cx.listener(|this, _: &ZulipOpenRow, window, cx| {
                this.zulip_open_row(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ZulipNextUnread, window, cx| {
                this.zulip_next_unread(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ZulipLoadOlder, _, cx| {
                this.zulip_load_older(cx);
            }))
            .on_action(cx.listener(|this, _: &SlackOpenRow, window, cx| {
                this.slack_open_row(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackCompose, window, cx| {
                this.slack_compose(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SlackSearch, window, cx| {
                this.prompt_slack_search(window, cx);
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
            .on_action(cx.listener(|this, _: &SlackWatchChannel, window, cx| {
                this.toggle_slack_watch(window, cx);
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
                this.shell_pager_action(rho_ui_proto::shell::PagerAction::Continue, cx);
            }))
            .on_action(cx.listener(|this, _: &ShellPagerAll, _, cx| {
                this.shell_pager_action(rho_ui_proto::shell::PagerAction::Drain, cx);
            }))
            .on_action(cx.listener(|this, _: &ShellPagerQuit, _, cx| {
                this.shell_pager_action(rho_ui_proto::shell::PagerAction::Quit, cx);
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
            .on_action(cx.listener(|this, _: &AgentDone, window, cx| {
                if this.dashboard.is_focused(window, cx)
                    && !this.dashboard.cursor_on_heading_line(cx)
                {
                    cx.propagate();
                    return;
                }
                this.cmd_agent_done(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentHide, window, cx| {
                if this.dashboard.is_focused(window, cx)
                    && !this.dashboard.cursor_on_heading_line(cx)
                {
                    cx.propagate();
                    return;
                }
                this.cmd_agent_done(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardReply, window, cx| {
                this.dashboard_reply(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardSubmit, window, cx| {
                this.dashboard_submit(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardCancelDraft, window, cx| {
                if matches!(
                    this.dashboard.cursor_target(&this.registry, cx),
                    Some(
                        crate::dashboard::RowTarget::NewDraft
                            | crate::dashboard::RowTarget::NewTreeDraft(_)
                    )
                ) && this.dashboard.discard_new_draft(cx)
                {
                    this.forget_discarded_draft(window, cx);
                    this.refresh_dashboard(window, cx);
                    if let Ok(action) = cx.build_action("vim::NormalBefore", None) {
                        window.dispatch_action(action, cx);
                    }
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardHeadingBelow, window, cx| {
                this.dashboard_insert_heading(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardHeadingAbove, window, cx| {
                this.dashboard_insert_heading(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardNow, window, cx| {
                this.dashboard_now(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardBack, window, cx| {
                this.dashboard_back(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardJump, window, cx| {
                this.prompt_dashboard_jump(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardRenameTopic, window, cx| {
                this.prompt_dashboard_rename_topic(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealExit, window, cx| {
                vim::take_count(cx);
                this.close_current_surface(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealNext, window, cx| {
                vim::take_count(cx);
                this.pull_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &UndoVerdict, window, cx| {
                vim::take_count(cx);
                this.undo_verdict(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealDone, window, cx| {
                vim::take_count(cx);
                this.verdict_done(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealMute, window, cx| {
                vim::take_count(cx);
                this.verdict_mute(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealSnooze, window, cx| {
                let count = vim::take_count(cx);
                this.deal_snooze(SnoozeUnit::Days, count, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::DashboardDealSnoozeMinutes, window, cx| {
                    let count = vim::take_count(cx);
                    this.deal_snooze(SnoozeUnit::Minutes, count, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::DashboardDealSnoozeHours, window, cx| {
                    let count = vim::take_count(cx);
                    this.deal_snooze(SnoozeUnit::Hours, count, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::DashboardDealSnoozeWeeks, window, cx| {
                    let count = vim::take_count(cx);
                    this.deal_snooze(SnoozeUnit::Weeks, count, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &DashboardDealRoomSnooze, window, cx| {
                    let count = vim::take_count(cx);
                    this.verdict_room_snooze(count, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &DashboardDealTodo, window, cx| {
                let count = vim::take_count(cx);
                this.verdict_todo(count, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealRefresh, window, cx| {
                vim::take_count(cx);
                this.pull_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealFile, window, cx| {
                vim::take_count(cx);
                this.prompt_file_deal_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealReply, window, cx| {
                vim::take_count(cx);
                let Some(card) = this.card_in_view(cx) else {
                    return;
                };
                let target = this.dashboard.card_target(card.identity.clone());
                let opens_page = matches!(target, crate::dashboard::CardTarget::Page(_));
                let opens_slack =
                    this.phone.enabled && matches!(target, crate::dashboard::CardTarget::Thread(_));
                if card.agent_id.is_none()
                    && !opens_page
                    && !opens_slack
                    && !matches!(card.kind, crate::dashboard::DealCardKind::Desk)
                {
                    return;
                }
                this.dashboard.record_verdict(
                    &card,
                    crate::dashboard::DealerVerdict::Open,
                    chrono::Local::now().fixed_offset(),
                );
                match target {
                    crate::dashboard::CardTarget::Page(page) => {
                        this.open_browser_page(page, window, cx);
                    }
                    crate::dashboard::CardTarget::Thread(unit) if this.phone.enabled => {
                        this.open_slack_source(crate::slack::unit_source(&unit), window, cx);
                        if let SurfaceView::SlackConversation(view) = &this.active_surface().view {
                            let view = view.clone();
                            view.update(cx, |view, cx| view.select_compose(window, cx));
                            window.focus(&view.read(cx).editor().focus_handle(cx), cx);
                        }
                    }
                    _ if card.agent_id.is_some() => {
                        let agent_id = card.agent_id.unwrap();
                        this.open_agent(agent_id, window, cx);
                        if this.phone.enabled
                            && let SurfaceView::Transcript { model, editor } =
                                &this.active_surface().view
                        {
                            let (model, editor) = (model.clone(), editor.clone());
                            model.update(cx, |model, cx| model.focus_prompt(&editor, window, cx));
                        }
                    }
                    _ if this.phone.enabled
                        && matches!(card.kind, crate::dashboard::DealCardKind::Desk) =>
                    {
                        this.phone_open_desk(window, cx);
                        this.phone_toggle_dashboard_editing(window, cx);
                    }
                    _ => {}
                }
            }))
            .on_action(
                cx.listener(|this, _: &DashboardToggleAgentTree, window, cx| {
                    if !this.dashboard.is_focused(window, cx)
                        || !this.dashboard.toggle_agent_tree(cx)
                    {
                        cx.propagate();
                        return;
                    }
                    this.refresh_dashboard(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &DashboardToggleSubagents, window, cx| {
                    if !this.dashboard.is_focused(window, cx)
                        || !this.dashboard.toggle_subagents(cx)
                    {
                        cx.propagate();
                        return;
                    }
                    this.refresh_dashboard(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &DashboardCycleGlobal, window, cx| {
                if !this.dashboard.is_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard.cycle_global_folds(cx);
                this.refresh_dashboard(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardArchive, window, cx| {
                if !this.dashboard.is_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                let archived =
                    this.dashboard
                        .tree_node_at_cursor(cx)
                        .is_some_and(|(host, node_id)| {
                            this.set_node_fact(
                                host,
                                node_id,
                                rho_desk::cells::Property::State(rho_desk::cells::State::Muted),
                                window,
                                cx,
                            )
                        });
                if !archived {
                    this.notice_on(
                        None,
                        "archive: heading unavailable",
                        StyleClass::SystemInfo,
                        cx,
                    );
                } else {
                    this.notice_on(None, "archived", StyleClass::SystemInfo, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDemote, window, cx| {
                if !this.dashboard_verb_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard.dispatch_semantic_row_action(
                    editor::SemanticRowAction::Indent { outdent: false },
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &DashboardPromote, window, cx| {
                if !this.dashboard_verb_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard.dispatch_semantic_row_action(
                    editor::SemanticRowAction::Indent { outdent: true },
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &DashboardNewSibling, window, cx| {
                if !this.dashboard_new_heading_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard_new_heading(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardNewChild, window, cx| {
                if !this.dashboard_new_heading_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard_new_heading(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardMoveSubtreeUp, window, cx| {
                if !this.dashboard_verb_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.reordering_unavailable(cx);
            }))
            .on_action(
                cx.listener(|this, _: &DashboardMoveSubtreeDown, window, cx| {
                    if !this.dashboard_verb_applies(window, cx) {
                        cx.propagate();
                        return;
                    }
                    this.reordering_unavailable(cx);
                }),
            )
            .on_action(cx.listener(|this, _: &DashboardDeleteEmpty, window, cx| {
                if !this.dashboard_verb_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard_delete_empty(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDeleteRow, _, cx| {
                if !this
                    .dashboard
                    .dispatch_semantic_row_action(editor::SemanticRowAction::Delete, cx)
                {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardYankRow, _, cx| {
                if !this
                    .dashboard
                    .dispatch_semantic_row_action(editor::SemanticRowAction::Yank, cx)
                {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardPasteRow, window, cx| {
                if !this.dashboard.dispatch_semantic_row_action(
                    editor::SemanticRowAction::Paste { before: false },
                    cx,
                ) {
                    if let Some((host, node_id)) = this.desk_semantic_paste_target.take() {
                        this.paste_desk_semantic_subtree(host, node_id, window, cx);
                    } else {
                        cx.propagate();
                    }
                }
            }))
            .on_action(
                cx.listener(|this, _: &DashboardPasteRowBefore, window, cx| {
                    if !this.dashboard.dispatch_semantic_row_action(
                        editor::SemanticRowAction::Paste { before: true },
                        cx,
                    ) {
                        if let Some((host, node_id)) = this.desk_semantic_paste_target.take() {
                            this.paste_desk_semantic_subtree(host, node_id, window, cx);
                        } else {
                            cx.propagate();
                        }
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &DashboardUndo, window, cx| {
                this.dashboard_undo(window, cx);
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
            .on_action(cx.listener(|this, _: &RailFocus, window, cx| {
                this.focus_rail(window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::NoteOpenRow, window, cx| {
                if !this.note_open_row(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &crate::NotesForThis, window, cx| {
                this.open_notes_for_surface(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RailOpen, window, cx| {
                this.dashboard_open(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardGoto, window, cx| {
                this.dashboard_open(window, cx);
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
                        self.render_workspace(window, &text_style, cx)
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
                    .child(bottom_strip(&text_style, cx).child(open.menu.render(&text_style, cx)))
                    .into_any_element()
            }))
            .children(match (&self.pending_git_approval, &self.minibuffer) {
                (Some(pending), _) => {
                    let colors = cx.theme().colors();
                    let focused = self.git_approval_focus.is_focused(window);
                    let mut deny = div().flex().flex_row().px_1().child("n deny");
                    if focused {
                        deny = deny.bg(colors.element_selected);
                    } else {
                        deny = deny.text_color(colors.text_muted);
                    }
                    Some(
                        div()
                            .key_context("RhoGitApproval")
                            .track_focus(&self.git_approval_focus)
                            .child(
                                bottom_strip(&text_style, cx)
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .gap_1()
                                            .px_2()
                                            .child(
                                                div()
                                                    .font_weight(gpui::FontWeight::BOLD)
                                                    .text_color(colors.text_accent)
                                                    .child("Git approval"),
                                            )
                                            .child("·")
                                            .child(pending.prompt.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap_4()
                                            .px_2()
                                            .child(
                                                div()
                                                    .text_color(colors.text_muted)
                                                    .child("Y allow"),
                                            )
                                            .child(deny),
                                    ),
                            )
                            .into_any_element(),
                    )
                }
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
            })
    }
}

/// How a new agent is filed into the area it was made in. A label is the
/// other axis: filing into one is being labelled, not being reparented
/// under it. Written as a parent, the agent went in and the map still drew
/// it at the root with the label it was made in left empty.
pub(crate) fn filing_property(area: rho_desk::cells::Id) -> rho_desk::cells::Property {
    match area {
        label @ rho_desk::cells::Id::Label(_) => rho_desk::cells::Property::Labeled {
            label,
            present: true,
        },
        parent => rho_desk::cells::Property::Parent(Some(parent)),
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
    fn labels_agent_role() {
        assert_eq!(agent_role_label(AgentRole::default()), "eng");
    }

    #[test]
    fn rejected_undos_return_to_their_original_lifo_positions() {
        let mut sequences = vec![0, 3];
        for rejected in [2, 1] {
            let index = undo_sequence_insert_position(sequences.iter().copied(), rejected);
            sequences.insert(index, rejected);
        }
        assert_eq!(sequences, vec![0, 1, 2, 3]);
        assert_eq!(sequences.pop(), Some(3));
    }
}
