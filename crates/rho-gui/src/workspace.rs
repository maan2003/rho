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
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use camino::Utf8PathBuf;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::prelude::*;
use gpui::{
    App, ClipboardEntry, Context, Entity, Focusable as _, Point, Task, TouchEvent, TouchId,
    TouchPhase, Window, div, px,
};
#[cfg(test)]
pub(crate) use phone::set_touch_modal_editing;
use rho_core::ContentPart;
#[cfg(test)]
use rho_ui_proto::AdvisorIntelligence;
use rho_ui_proto::{AgentId, AgentRole, ClientMessage, EngineerIntelligence, MessageDelivery};
use settings::Settings as _;
use theme::ActiveTheme as _;

use crate::agent_view::AgentModel;
use crate::chime::Chime;
use crate::connection::{
    AgentFrameAllocation, ConnEvent, Connection, GitApprovalDecision, HostEvent,
};
use crate::desk_view::DeskCells;
use crate::draft_view::DraftModel;
use crate::hosts::{HostStatus, Hosts};
use crate::minibuffer::{ECHO_DURATION, Echo, Minibuffer, bottom_strip};
use crate::pane::{Pane, SurfaceKey};
use crate::registry::session::{
    AgentSubscriptions, INITIAL_AGENT_SUBSCRIPTIONS, recent_agent_roots,
};
use crate::registry::{ActivePane, AgentRegistry, HostId};
use crate::store::{AgentStore, FrameSummary};
#[cfg(test)]
use crate::style::RoleFamily;
use crate::style::StyleClass;
use crate::zed_remote::{FileView, RemoteProject};
use crate::{
    AgentDone, AgentHide, AgentNew, AgentNext, AgentPrevious, BrowserExit, DashboardArchive,
    DashboardBack, DashboardCancelDraft, DashboardCycleGlobal, DashboardDealAppend,
    DashboardDealDone, DashboardDealExit, DashboardDealFile, DashboardDealInsert,
    DashboardDealMute, DashboardDealNext, DashboardDealOpenLine, DashboardDealRefresh,
    DashboardDealReply, DashboardDealRoomSnooze, DashboardDealSnooze, DashboardDealTodo,
    DashboardDeleteEmpty, DashboardDeleteRow, DashboardDemote, DashboardGoto,
    DashboardHeadingAbove, DashboardHeadingBelow, DashboardJump, DashboardMoveSubtreeDown,
    DashboardMoveSubtreeUp, DashboardNewChild, DashboardNewSibling, DashboardNow,
    DashboardPasteRow, DashboardPasteRowBefore, DashboardPromote, DashboardRenameTopic,
    DashboardReply, DashboardSubmit, DashboardToggleAgentTree, DashboardToggleSubagents,
    DashboardUndo, DashboardYankRow, DealCloseAndNext, DealLeave, DealOpen, FindNode,
    GitApprovalAllow, GitApprovalDeny, HomeOpenRow, MessagesOpen, MinibufferCancel,
    MinibufferComplete, MinibufferConfirm, MinibufferNext, MinibufferPrevious, OverviewToggle,
    PastePrompt, RailFocus, RailOpen, RoleCycle, RoleCycleGroup, ShellEof, ShellInterrupt,
    ShellPagerAll, ShellPagerMore, ShellPagerQuit, SlackCancelEdit, SlackCompose, SlackEditLast,
    SlackEditMessage, SlackMarkReadBefore, SlackNextUnread, SlackOpenRow, SlackSearch,
    SubmitPrompt, SurfaceBack, SurfaceClose, TaskBoard, UndoVerdict, UploadGuiTelemetry,
    VoiceToggle, ZulipLoadOlder, ZulipNextUnread, ZulipOpenRow,
};

pub(crate) const MESSAGE_LOG_CAP: usize = 4096;
pub(crate) const MESSAGE_REBASE_EVICTIONS: usize = 512;

#[derive(Clone)]
struct MessageLogEntry {
    timestamp: chrono::DateTime<chrono::FixedOffset>,
    class: StyleClass,
    text: String,
}

#[derive(Default)]
struct MessageLog(VecDeque<MessageLogEntry>);

impl MessageLog {
    fn push(&mut self, entry: MessageLogEntry) -> bool {
        self.0.push_back(entry);
        if self.0.len() > MESSAGE_LOG_CAP {
            self.0.pop_front();
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
struct WarmSurface {
    context: ContextId,
    surface: Surface,
}

const SHELL_SWIPE_DISTANCE: gpui::Pixels = px(64.);

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

enum DealView {
    Desk {
        identity: crate::dashboard::DealCardId,
        editor: Entity<editor::Editor>,
    },
    Surface {
        identity: crate::dashboard::DealCardId,
        surface: Surface,
    },
}

impl DealView {
    fn matches(&self, identity: &crate::dashboard::DealCardId) -> bool {
        match self {
            Self::Desk {
                identity: current, ..
            }
            | Self::Surface {
                identity: current, ..
            } => current == identity,
        }
    }

    #[cfg(test)]
    fn identity(&self) -> crate::dashboard::DealCardId {
        match self {
            Self::Desk { identity, .. } | Self::Surface { identity, .. } => identity.clone(),
        }
    }
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
    DeskNode(Entity<editor::Editor>),
    Transcript {
        model: Entity<AgentModel>,
        /// The editor over the model's multibuffer.
        editor: Entity<editor::Editor>,
    },
    File(Entity<FileView>),
    Shell {
        model: Entity<crate::shell_view::ShellModel>,
        editor: Entity<editor::Editor>,
    },
    Diff(Entity<crate::diff_view::DiffView>),
    Terminal(Entity<crate::terminal_view::TerminalView>),
    Browser(Entity<rho_browser::PageView>),
    ZulipInbox(Entity<rho_zulip::ui::InboxView>),
    ZulipNarrow(Entity<rho_zulip::ui::NarrowView>),
    SlackList(Entity<rho_slack::ui::ListView>),
    SlackConversation(Entity<rho_slack::ui::ConversationView>),
    Image(Entity<crate::image_view::ImageView>),
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

/// How to reach the daemon. Deliberately holds no client-local paths: the
/// socket may be forwarded from another machine, so the GUI's own cwd and
/// home mean nothing to the daemon and must never leak into agent working
/// directories.
#[derive(Clone)]
pub enum AttachTarget {
    Unix(PathBuf),
    Iroh {
        endpoint_id: iroh::EndpointId,
        ssh_destination: String,
        remote_rho: String,
    },
}

impl AttachTarget {
    /// How the host reads in chrome and error text.
    pub fn describe(&self) -> String {
        match self {
            Self::Unix(path) => path.display().to_string(),
            Self::Iroh {
                ssh_destination, ..
            } => format!("iroh via {ssh_destination}"),
        }
    }
}

/// One daemon to attach: the short name it is known by in this client, and
/// how to reach it.
#[derive(Clone)]
pub struct HostSpec {
    pub name: String,
    pub target: AttachTarget,
}

impl HostSpec {
    /// Parses the one-line host form used both on the command line and in
    /// the attach prompt: `<name>=unix:<socket>` or
    /// `<name>=iroh:<endpoint-id>@<ssh-destination>`.
    pub fn parse(text: &str, remote_rho: &str) -> Result<Self, String> {
        let (name, target) = text
            .trim()
            .split_once('=')
            .ok_or("expected <name>=unix:<socket> or <name>=iroh:<endpoint-id>@<ssh-dest>")?;
        if name.is_empty() {
            return Err("host name is empty".to_owned());
        }
        let target = match target.split_once(':') {
            Some(("unix", path)) => AttachTarget::Unix(PathBuf::from(path)),
            Some(("iroh", rest)) => {
                let (endpoint_id, ssh_destination) = rest
                    .split_once('@')
                    .ok_or("iroh targets are <endpoint-id>@<ssh-dest>")?;
                AttachTarget::Iroh {
                    endpoint_id: endpoint_id
                        .parse()
                        .map_err(|error| format!("invalid iroh endpoint id: {error}"))?,
                    ssh_destination: ssh_destination.to_owned(),
                    remote_rho: remote_rho.to_owned(),
                }
            }
            _ => return Err(format!("unknown host target scheme in `{target}`")),
        };
        Ok(Self {
            name: name.to_owned(),
            target,
        })
    }
}

/// A working directory on a specific daemon. Two machines can both offer
/// `/home/you/src/rho`, so a bare path never identifies a project once more
/// than one host is attached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPath {
    pub host: HostId,
    pub path: Utf8PathBuf,
}

/// A registered project, with the daemon that offers it.
#[derive(Clone)]
struct HostProject {
    host: HostId,
    project: rho_ui_proto::UiProject,
}

#[derive(Clone)]
struct PendingTreeVerdict {
    event: crate::dashboard::DealerEvent,
    echo: String,
    undo: VerdictUndo,
    phone_verdict: Option<crate::journal::PhoneVerdict>,
}

#[derive(Clone)]
struct VerdictUndo {
    sequence: u64,
    verb: String,
    state: VerdictUndoState,
}

/// The unit half of the snooze operator (`s` then `m`, `h`, `d`, `w` or `s`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SnoozeUnit {
    Minutes,
    Hours,
    Days,
    Weeks,
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
        card: crate::dashboard::DealCard,
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
    subscriptions: AgentSubscriptions,
    store: AgentStore,
    pub(crate) registry: AgentRegistry,
    models: HashMap<AgentId, Entity<AgentModel>>,
    /// Weak project cache keyed by daemon-side workspace identity, qualified
    /// by host — the same repository path on two machines is two projects.
    /// Artifact surfaces hold the strong references; when the last file/diff
    /// closes, the remote channel and cache entry naturally expire.
    remote_projects: HashMap<
        (HostId, rho_ui_proto::WorkspaceInfo),
        gpui::WeakEntity<crate::zed_remote::RemoteProjectState>,
    >,
    pending_diff_loads: HashMap<AgentId, Task<()>>,
    /// Accumulated change summaries for materialized but hidden views; they
    /// render once, with the merged summary, when next selected.
    pending_syncs: HashMap<AgentId, FrameSummary>,
    /// Agent frames accumulated since the last draw. Applying a streaming
    /// update immediately can monopolize the foreground thread, preventing
    /// both a draw and input dispatch when several agents are active.
    pending_frames: Vec<PendingAgentFrame>,
    frame_flush_scheduled: bool,
    draft_model: Entity<DraftModel>,
    message_log: MessageLog,
    messages_buffer: Entity<language::Buffer>,
    messages_editor: Entity<editor::Editor>,
    messages_styles: Vec<(StyleClass, std::ops::Range<text::Anchor>)>,
    messages_line_lengths: VecDeque<usize>,
    messages_applied_classes: HashSet<StyleClass>,
    message_evictions_since_rebase: usize,
    message_rebase_scheduled: bool,
    /// Registered workdirs from every attached daemon; selection vocabulary
    /// for new agents, and what decides which host a new agent lands on.
    workdirs: Vec<HostProject>,
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
    /// Per-host quota and usage answers. The chrome merges them (the
    /// binding constraint is whichever host has least headroom); keeping
    /// them apart means one host's refresh never blanks another's.
    quota_summaries: HashMap<HostId, Vec<rho_ui_proto::QuotaSummary>>,
    quota_history: HashMap<HostId, Vec<rho_ui_proto::QuotaSeries>>,
    quota_history_days: u64,
    global_usage: HashMap<HostId, Vec<rho_ui_proto::AgentUsageSeries>>,
    global_usage_days: u64,
    agent_cost_usage: HashMap<HostId, Vec<rho_ui_proto::AgentCostSeries>>,
    agent_cost_days: u64,
    duration_timer: Option<Task<()>>,
    /// Attention chime output; lazily opened on the first play.
    chime: Chime,
    /// Each context retains one viewport over its surfaces.
    contexts: HashMap<ContextId, Pane<Surface>>,
    /// Per-context surface list, the emacs buffer list: every surface
    /// opened in a context lives here for the context's lifetime,
    /// regardless of what its viewport currently displays. Covering one never
    /// loses a file or terminal; the views (and any workspace file channel
    /// behind them) release when the context itself closes.
    surfaces: HashMap<ContextId, Vec<Surface>>,
    /// Always present in `contexts` (the draft context never closes).
    pub(crate) active_context: ContextId,
    /// Chronological history of surfaces dealt or opened from the overview.
    surface_history: Vec<WarmSurface>,
    history_cursor: usize,
    overview_open: bool,
    navigation_skips: HashMap<SurfaceKey, crate::dashboard::DealCardId>,
    last_shift_tap: Option<std::time::Instant>,
    shell_touches: HashMap<TouchId, ShellTouchContact>,
    shell_touch_was_multi: bool,
    shell_touch_committed: bool,
    deal_gesture_active: bool,
    deal_session_open: bool,
    deal_current_interacted: bool,
    deal_view: Option<DealView>,
    deal_focus_pending: bool,
    deal_controls_visible: bool,
    agent_last_interaction: HashMap<AgentId, i64>,
    dealer_signal_eval_scheduled: bool,
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
    /// Client-local web page shown in the same right-hand preview card.
    dashboard_web_preview: Option<(rho_browser::PageId, Entity<rho_browser::PageView>)>,
    /// Browser resources referenced by the last reconciled Desk documents.
    browser_pages: HashSet<rho_browser::PageId>,
    browser_metadata_subscription: Option<gpui::Subscription>,
    /// Unreferenced browser pages waiting out the Desk edit grace period.
    browser_page_gc: HashMap<rho_browser::PageId, Task<()>>,
    /// Read-only document shown when the synthetic Iris row is targeted.
    iris_preview: Entity<editor::Editor>,
    /// Each daemon's hidden persisted Iris coordinator. These identities stay
    /// outside the ordinary registry so Iris never enters agent/workstream
    /// lists, but still route transcript subscriptions to the owning host.
    iris_agents: HashMap<HostId, AgentId>,
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
    pub(crate) _slack_subscription: Option<gpui::Subscription>,
    /// One per open conversation surface, for what a conversation asks the
    /// frame to show: a picture full-window, so far.
    pub(crate) _slack_view_subscriptions: Vec<gpui::Subscription>,
    pending_filing_destinations: Vec<(String, String, HostId, rho_desk::cells::Id)>,
    pending_filing_selected: Option<(HostId, rho_desk::cells::Id)>,
    scroll_journal_task: Option<Task<()>>,
    /// The completing-read strip at the bottom of the window, when open.
    pub(crate) minibuffer: Option<Minibuffer>,
    /// An open transient menu in the bottom strip; captures the keyboard
    /// via `transient_focus` while shown.
    transient: Option<crate::transient::Transient>,
    /// Parent menus beneath the open one; escape pops one level (magit's
    /// quit-one) before a final escape closes the strip.
    transient_stack: Vec<crate::transient::Transient>,
    transient_focus: gpui::FocusHandle,
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
    iris_input_muted: bool,
    iris_session_enabled: bool,
    /// The daemon running the voice session. Iris delegates to an agent by
    /// id, and an id from another daemon means nothing there, so the session
    /// binds to one host; selecting an agent elsewhere leaves it without
    /// context rather than sending a foreign id.
    iris_host: Option<HostId>,
    _event_task: Task<()>,
    _dashboard_subscription: gpui::Subscription,
    _keystroke_subscription: gpui::Subscription,
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
            let workspace = cx.entity().downgrade();
            let visualization_client = self
                .connection_for(agent_id)
                .map(Connection::visualization_client)
                .unwrap_or_else(crate::connection::VisualizationClient::detached);
            let model = cx.new(|cx| AgentModel::new(workspace, visualization_client, cx));
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

    fn apply_agent_subscribed(&mut self, agent_id: AgentId) {
        self.registry.mark_known(agent_id);
    }

    fn apply_attention(&mut self, agent_id: AgentId, attention: rho_ui_proto::UiAttention) {
        self.registry.set_attention(agent_id, attention);
    }

    fn apply_turn_report(&mut self, agent_id: AgentId, report: rho_ui_proto::UiTurnReport) {
        self.registry.set_turn_report(agent_id, report);
    }

    fn apply_ready(
        &mut self,
        host: HostId,
        machine_seed: u64,
        agent_counter: u64,
        agents: Vec<rho_ui_proto::UiAgentSummary>,
        iris_agent: Option<AgentId>,
    ) -> (bool, Option<Vec<AgentId>>) {
        let first_ready = self.ready_hosts.insert(host);
        let initial = first_ready.then(|| {
            recent_agent_roots(
                &agents,
                self.registry.selected_agent().copied(),
                INITIAL_AGENT_SUBSCRIPTIONS,
            )
        });
        self.registry
            .set_host_data(host, machine_seed, agent_counter, agents);
        match iris_agent {
            Some(agent_id) => {
                self.iris_agents.insert(host, agent_id);
            }
            None => {
                self.iris_agents.remove(&host);
            }
        }
        (first_ready, initial)
    }

    fn note_agent_created(&mut self, host: HostId, agent_id: AgentId) {
        self.registry.note_agent_created(host, agent_id);
    }

    fn apply_frame_state(
        &mut self,
        agent_id: AgentId,
        frame: rho_ui_proto::remote::AgentRemoteFrame,
    ) -> Option<(FrameSummary, Option<u64>, bool, bool)> {
        if !self.subscriptions.accepts_frames(agent_id) {
            return None;
        }
        let old_context = self
            .store
            .get(&agent_id)
            .and_then(|state| state.context_used);
        let old_usage = self.store.get(&agent_id).map(|state| state.usage.clone());
        let summary = self.store.apply(agent_id, frame);
        let usage_changed = old_usage.as_ref() != self.store.get(&agent_id).map(|s| &s.usage);
        let live_changed = self.registry.mark_live(agent_id);
        Some((summary, old_context, usage_changed, live_changed))
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
        let Some(state) = self.store.get(&agent_id).cloned() else {
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
        } else if let Some(state) = self.store.get(&agent_id) {
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

    fn apply_agent_unloaded(
        &mut self,
        agent_id: AgentId,
        reason: rho_ui_proto::AgentUnloadReason,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.subscriptions.mark_unloaded(agent_id, reason) {
            return false;
        }
        self.registry.mark_not_live(agent_id);
        let summary = self.store.mark_unloaded(agent_id);
        if let Some(model) = self.models.get(&agent_id).cloned() {
            self.sync_agent_model(agent_id, &model, summary, false, cx);
        }
        true
    }

    fn finish_agent_load(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        let Some(model) = self.models.get(&agent_id).cloned() else {
            return;
        };
        if let Some(summary) = self.pending_syncs.remove(&agent_id)
            && let Some(state) = self.store.get(&agent_id)
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

struct PendingAgentFrame {
    agent_id: AgentId,
    frame: rho_ui_proto::remote::AgentRemoteFrame,
    /// Keeps the connection's decode-budget reservation until the frame is
    /// applied, preserving transport backpressure while it waits for a draw.
    allocation: Option<AgentFrameAllocation>,
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
        let (hosts, events) = Hosts::new();
        let workspace = cx.entity().downgrade();
        let mode_indicator = cx.new(|cx| vim::ModeIndicator::new(window, cx));
        let draft_model = cx.new(|cx| DraftModel::new(workspace, cx));
        let messages_buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local("", cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let messages_editor = cx.new(|cx| {
            let mut editor = editor::Editor::for_buffer(messages_buffer.clone(), None, window, cx);
            crate::editor_config::configure(&mut editor, window, cx);
            editor.set_read_only(true);
            editor.set_autoscroll_pin(
                multi_buffer::Anchor::Max,
                editor::scroll::AutoscrollStrategy::Bottom,
                cx,
            );
            editor
        });
        let event_task = cx.spawn(async move |this, cx| {
            let mut events: UnboundedReceiver<HostEvent> = events;
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                let updated = this.update_in(cx, |this, window, cx| {
                    this.handle_events(batch, window, cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let dealer_signal_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(60))
                    .await;
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
        let iris_buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local("iris\n\nlistening", cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let iris_preview = cx.new(|cx| {
            let mut editor = editor::Editor::for_buffer(iris_buffer, None, window, cx);
            crate::editor_config::configure_preview(&mut editor, window, cx);
            editor.set_read_only(true);
            editor
        });
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
                    this.prompt_dashboard_search(*backwards, window, cx);
                }
                editor::EditorEvent::SelectionsChanged { local: true } => {
                    this.refresh_dashboard(window, cx);
                    this.dashboard_cursor_moved(window, cx);
                }
                _ => {}
            },
        );
        let keystroke_subscription = cx.observe_keystrokes(|this, event, window, cx| {
            if this.desk_semantic_paste_target.is_some()
                && !event.keystroke.key.eq_ignore_ascii_case("p")
                && !matches!(
                    event.keystroke.key.as_str(),
                    "shift" | "control" | "alt" | "platform" | "function"
                )
            {
                this.desk_semantic_paste_target = None;
            }
            if this.dashboard.deal_mode()
                && !matches!(
                    event.keystroke.key.as_str(),
                    "shift"
                        | "control"
                        | "alt"
                        | "platform"
                        | "function"
                        | "q"
                        | "d"
                        | "x"
                        | "s"
                        | "t"
                        | "f"
                        | "i"
                        | "a"
                        | "o"
                        | "tab"
                        | "f16"
                        | "f20"
                )
            {
                this.mark_deal_interacted();
            }
            if event.keystroke.key == "shift"
                && !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.alt
                && !event.keystroke.modifiers.platform
            {
                let now = std::time::Instant::now();
                if this
                    .last_shift_tap
                    .is_some_and(|last| now.duration_since(last) <= Duration::from_millis(300))
                {
                    this.last_shift_tap = None;
                    this.toggle_overview(window, cx);
                } else {
                    this.last_shift_tap = Some(now);
                }
            } else {
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
                crate::journal::record(crate::journal::Event::WindowFocusChanged { focused });
            });
        let mut this = Self {
            hosts,
            subscriptions: AgentSubscriptions::default(),
            store: AgentStore::default(),
            registry: AgentRegistry::default(),
            models: HashMap::new(),
            remote_projects: HashMap::new(),
            pending_diff_loads: HashMap::new(),
            pending_syncs: HashMap::new(),
            pending_frames: Vec::new(),
            frame_flush_scheduled: false,
            draft_model,
            message_log: MessageLog::default(),
            messages_buffer,
            messages_editor,
            messages_styles: Vec::new(),
            messages_line_lengths: VecDeque::new(),
            messages_applied_classes: HashSet::new(),
            message_evictions_since_rebase: 0,
            message_rebase_scheduled: false,
            workdirs: Vec::new(),
            draft_area: None,
            awaiting_draft_agent: None,
            pending_agent_filing: None,
            ready_hosts: HashSet::new(),
            replay_hosts: HashSet::new(),
            quota_summaries: HashMap::new(),
            quota_history: HashMap::new(),
            quota_history_days: 7,
            global_usage: HashMap::new(),
            global_usage_days: 7,
            agent_cost_usage: HashMap::new(),
            agent_cost_days: 7,
            duration_timer: None,
            chime: Chime,
            contexts: HashMap::new(),
            surfaces: HashMap::new(),
            active_context: ContextId::Draft,
            surface_history: Vec::new(),
            history_cursor: 0,
            overview_open: false,
            navigation_skips: HashMap::new(),
            last_shift_tap: None,
            shell_touches: HashMap::new(),
            shell_touch_was_multi: false,
            shell_touch_committed: false,
            deal_gesture_active: false,
            deal_session_open: false,
            deal_current_interacted: false,
            deal_view: None,
            deal_focus_pending: false,
            deal_controls_visible: false,
            agent_last_interaction: HashMap::new(),
            dealer_signal_eval_scheduled: false,
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
            dashboard_web_preview: None,
            browser_pages: HashSet::new(),
            browser_metadata_subscription: None,
            browser_page_gc: HashMap::new(),
            iris_preview,
            iris_agents: HashMap::new(),
            zulip: None,
            slack: None,
            slack_degraded: None,
            slack_labels: HashMap::new(),
            _slack_subscription: None,
            _slack_view_subscriptions: Vec::new(),
            pending_filing_destinations: Vec::new(),
            pending_filing_selected: None,
            scroll_journal_task: None,
            minibuffer: None,
            transient: None,
            transient_stack: Vec::new(),
            transient_focus: cx.focus_handle(),
            git_approval_focus: cx.focus_handle(),
            overlay_return_focus: None,
            echo: None,
            pending_git_approval: None,
            realtime_task: None,
            realtime_stop: None,
            realtime_input_muted: None,
            iris_input_muted: false,
            iris_session_enabled: false,
            iris_host: None,
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
        // looking at Slack.
        this.slack_session(window, cx);
        this
    }

    /// Attaches a daemon. The name is registered with the registry first so
    /// that labels and chrome can qualify by host from the moment the host
    /// exists, not only once it answers.
    pub(crate) fn attach_host(&mut self, spec: HostSpec, cx: &App) -> HostId {
        let host = self.hosts.attach(spec.name.clone(), spec.target, cx);
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
        let mut departed = self
            .registry
            .known_agents()
            .copied()
            .filter(|agent_id| self.registry.host_of_agent(*agent_id) == Some(host))
            .collect::<Vec<_>>();
        if let Some(agent_id) = self.iris_agents.remove(&host) {
            departed.push(agent_id);
        }
        let contexts = departed
            .iter()
            .copied()
            .map(ContextId::Agent)
            .collect::<HashSet<_>>();
        if self.iris_host == Some(host) {
            self.stop_iris();
        }
        self.hosts.detach(host);
        self.ready_hosts.remove(&host);
        self.replay_hosts.remove(&host);
        self.quota_summaries.remove(&host);
        self.quota_history.remove(&host);
        self.global_usage.remove(&host);
        self.agent_cost_usage.remove(&host);
        self.workdirs.retain(|workdir| workdir.host != host);
        self.remote_projects.retain(|(owner, _), _| *owner != host);
        self.registry.detach_host(host);
        self.refresh_dashboard(window, cx);
        for agent_id in departed {
            self.subscriptions.forget(agent_id);
            self.store.forget(agent_id);
            self.models.remove(&agent_id);
            self.pending_syncs.remove(&agent_id);
            self.pending_diff_loads.remove(&agent_id);
        }
        self.contexts
            .retain(|context, _| !contexts.contains(context));
        self.surfaces
            .retain(|context, _| !contexts.contains(context));
        if !self.contexts.contains_key(&self.active_context) {
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
        self.registry.host_of_agent(agent_id).or_else(|| {
            self.iris_agents
                .iter()
                .find_map(|(host, iris_id)| (*iris_id == agent_id).then_some(*host))
        })
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

    fn host_label(&self, host: HostId) -> String {
        self.registry.host_name(host).to_owned()
    }

    /// Qualifies a daemon-side name with its host, but only when there is
    /// more than one host for it to be confused with.
    fn qualify(&self, host: HostId, name: &str) -> String {
        if self.hosts.len() > 1 {
            format!("{}/{name}", self.host_label(host))
        } else {
            name.to_owned()
        }
    }

    pub(crate) fn active_pane(&self) -> &Pane<Surface> {
        self.contexts
            .get(&self.active_context)
            .expect("active context has a pane")
    }

    pub(crate) fn active_pane_mut(&mut self) -> &mut Pane<Surface> {
        self.contexts
            .get_mut(&self.active_context)
            .expect("active context has a pane")
    }

    fn append_history(
        &mut self,
        surface: Surface,
        method: crate::journal::HistoryAppendMethod,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self
            .surface_history
            .iter()
            .position(|warm| warm.surface.key == surface.key)
        {
            let removed = self.surface_history.remove(index);
            crate::journal::record(crate::journal::Event::HistoryRemoved {
                identity: Self::journal_surface(&removed.surface.key),
                method: crate::journal::HistoryRemoveMethod::Dedupe,
            });
        }
        self.surface_history.push(WarmSurface {
            context: self.active_context,
            surface: surface.clone(),
        });
        self.history_cursor = self.surface_history.len() - 1;
        crate::journal::record(crate::journal::Event::HistoryAppended {
            identity: Self::journal_surface(&surface.key),
            method,
        });
        cx.notify();
    }

    fn show_warm_surface(
        &mut self,
        mut warm: WarmSurface,
        method: crate::journal::SurfaceShowMethod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let SurfaceKey::Transcript(agent_id) = warm.surface.key
            && !self.subscriptions.contains(agent_id)
        {
            self.subscribe_agent(agent_id, cx);
            warm.surface = self.make_surface(SurfaceKey::Transcript(agent_id), window, cx);
        }
        self.ensure_surface_subscription(&warm.surface.key, cx);
        if self.history_cursor < self.surface_history.len()
            && self.surface_history[self.history_cursor].surface.key == warm.surface.key
        {
            self.surface_history[self.history_cursor] = warm.clone();
        }
        self.active_context = warm.context;
        self.active_pane_mut().show(warm.surface.clone());
        self.overview_open = false;
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        crate::journal::record(crate::journal::Event::SurfaceShown {
            surface: Self::journal_surface(&warm.surface.key),
            method,
        });
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
            self.forget_discarded_draft(cx);
            self.refresh_dashboard(window, cx);
            return;
        }
        // The overview and Home are both floors: there is nothing under
        // them to reveal, so `q` on either stays put.
        if self.overview_open || self.active_pane().surface.key == SurfaceKey::Home {
            return;
        }
        let was_dealing = self.dashboard.deal_mode();
        let dealt_untouched = was_dealing && !self.deal_current_interacted;
        if was_dealing {
            self.skip_and_end_deal(window, cx);
        }
        let key = self.active_pane().surface.key.clone();
        let removed = self
            .surface_history
            .iter()
            .position(|warm| warm.surface.key == key);
        self.navigation_skips.remove(&key);
        crate::journal::record(crate::journal::Event::SurfaceClosed {
            surface: Self::journal_surface(&key),
            dealt_untouched,
        });
        let Some(removed) = removed else {
            if key == SurfaceKey::Draft {
                self.history_cursor = self.surface_history.len();
                self.open_home(window, cx);
            } else if let Some(previous) = self.surface_history.get(self.history_cursor).cloned() {
                self.show_warm_surface(
                    previous,
                    crate::journal::SurfaceShowMethod::Mru,
                    window,
                    cx,
                );
            } else {
                self.open_home(window, cx);
            }
            cx.notify();
            return;
        };
        self.surface_history.remove(removed);
        crate::journal::record(crate::journal::Event::HistoryRemoved {
            identity: Self::journal_surface(&key),
            method: crate::journal::HistoryRemoveMethod::Close,
        });
        if removed > 0 {
            self.history_cursor = removed - 1;
            let previous = self.surface_history[self.history_cursor].clone();
            self.show_warm_surface(previous, crate::journal::SurfaceShowMethod::Mru, window, cx);
        } else {
            self.history_cursor = self.surface_history.len();
            self.open_home(window, cx);
        }
        cx.notify();
    }

    fn forget_discarded_draft(&mut self, cx: &mut Context<Self>) {
        self.draft_area = None;
        let key = SurfaceKey::Draft;
        self.navigation_skips.remove(&key);
        crate::journal::record(crate::journal::Event::SurfaceClosed {
            surface: Self::journal_surface(&key),
            dealt_untouched: false,
        });
        if let Some(index) = self
            .surface_history
            .iter()
            .position(|warm| warm.surface.key == key)
        {
            self.surface_history.remove(index);
            if index < self.history_cursor {
                self.history_cursor -= 1;
            } else if index == self.history_cursor {
                self.history_cursor = self.surface_history.len();
            }
            crate::journal::record(crate::journal::Event::HistoryRemoved {
                identity: Self::journal_surface(&key),
                method: crate::journal::HistoryRemoveMethod::Close,
            });
        }
        cx.notify();
    }

    /// Home is the front door: a cold start, an emptied queue, and the
    /// overview key all land here.
    pub(crate) fn open_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard.deal_mode() {
            self.skip_and_end_deal(window, cx);
        }
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
        let threads = self.slack_thread_facts(cx);
        let hand = self.dashboard.dealer_hand(
            &self.registry,
            &threads,
            now,
            &self.agent_last_interaction,
            cx,
        );
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

    /// Enter on a Home row. Any row opens as a deal through the dealer, so
    /// the surface, the verdict keys, undo and the timeline behave exactly
    /// as if the card had been dealt from the top.
    fn home_open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.home_view() else {
            return;
        };
        let target = view.update(cx, |view, cx| view.cursor_target(cx));
        let card = match target {
            crate::home::HomeTarget::Card(card) => card,
            // A running agent is not a card: its row opens the agent.
            crate::home::HomeTarget::Agent(agent_id) => {
                self.select_agent_inner(Some(agent_id), true, window, cx);
                return;
            }
            crate::home::HomeTarget::None => return,
        };
        if self.dashboard.deal_mode() {
            self.skip_and_end_deal(window, cx);
        }
        let now = chrono::Local::now().fixed_offset();
        let threads = self.slack_thread_facts(cx);
        if self
            .dashboard
            .deal_chosen(
                &self.registry,
                &threads,
                now,
                &card,
                &self.agent_last_interaction,
                cx,
            )
            .is_some()
        {
            self.deal_session_open = true;
            self.present_current_deal(window, cx);
            self.refresh_dashboard(window, cx);
        }
    }

    pub(crate) fn open_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard.deal_mode() {
            self.skip_and_end_deal(window, cx);
        }
        self.overview_open = true;
        self.refresh_dashboard(window, cx);
        window.focus(&self.dashboard.focus_handle(cx), cx);
        crate::journal::record(crate::journal::Event::OverviewOpened);
        cx.notify();
    }

    fn toggle_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overview_open {
            if self.history_cursor >= self.surface_history.len() {
                return;
            }
            self.overview_open = false;
            self.focus_active_surface(window, cx);
            cx.notify();
        } else if self.active_pane().surface.key == SurfaceKey::Home {
            // Already home: the key shows what the reader was reading, the
            // way closing the overview used to reveal it again. It does not
            // step through history, so pressing it twice is a round trip.
            let index = self
                .history_cursor
                .min(self.surface_history.len().saturating_sub(1));
            if let Some(warm) = self.surface_history.get(index).cloned() {
                self.show_warm_surface(warm, crate::journal::SurfaceShowMethod::Mru, window, cx);
            }
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
                                } else if !self.dashboard.deal_mode() {
                                    Some(Box::new(OverviewToggle) as Box<dyn gpui::Action>)
                                } else {
                                    None
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
        if self.overview_open || self.history_cursor == 0 {
            return;
        }
        if self.dashboard.deal_mode()
            && let Some(card) = self.dashboard.current_deal_card().cloned()
        {
            let key = self.active_pane().surface.key.clone();
            self.navigation_skips.insert(key, card.identity.clone());
            self.skip_and_end_deal(window, cx);
        }
        self.history_cursor -= 1;
        let target = self.surface_history[self.history_cursor].clone();
        self.show_warm_surface(
            target.clone(),
            crate::journal::SurfaceShowMethod::Mru,
            window,
            cx,
        );
        crate::journal::record(crate::journal::Event::HistoryStepped {
            direction: crate::journal::HistoryDirection::Back,
            position: self.history_cursor + 1,
            len: self.surface_history.len(),
        });
        cx.notify();
    }

    fn step_surface_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.overview_open || self.history_cursor + 1 >= self.surface_history.len() {
            return false;
        }
        self.history_cursor += 1;
        let target = self.surface_history[self.history_cursor].clone();
        if let Some(identity) = self.navigation_skips.remove(&target.surface.key) {
            self.dashboard.clear_skip(&identity);
        }
        self.show_warm_surface(
            target.clone(),
            crate::journal::SurfaceShowMethod::Mru,
            window,
            cx,
        );
        crate::journal::record(crate::journal::Event::HistoryStepped {
            direction: crate::journal::HistoryDirection::Forward,
            position: self.history_cursor + 1,
            len: self.surface_history.len(),
        });
        cx.notify();
        true
    }

    fn journal_card_identity(
        identity: &crate::dashboard::DealCardId,
    ) -> crate::journal::DealerCardIdentity {
        crate::journal::DealerCardIdentity {
            host: identity.host.0,
            node_id: identity.node_id.clone().into(),
        }
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
        let threads = self.slack_thread_facts(cx);
        let mut candidates = self.dashboard.dealer_hand(
            &self.registry,
            &threads,
            now,
            &self.agent_last_interaction,
            cx,
        );
        candidates.cards.retain(|card| {
            if let Some(current) = self.dashboard.current_deal_card() {
                return card.identity != current.identity;
            }
            match &self.active_pane().surface.key {
                SurfaceKey::Transcript(agent_id) => {
                    Some(card.identity.clone()) != self.dashboard.agent_card_id(*agent_id)
                }
                SurfaceKey::Browser(page) => {
                    Some(card.identity.clone()) != self.dashboard.page_card_id(*page)
                }
                _ => true,
            }
        });
        let warm_agents = candidates
            .cards
            .iter()
            .filter(|card| matches!(card.kind, crate::dashboard::DealCardKind::Agent))
            .filter_map(|card| card.agent_id)
            .take(3)
            .filter(|agent_id| {
                self.agent_online(*agent_id) && !self.subscriptions.contains(*agent_id)
            })
            .collect::<Vec<_>>();
        for agent_id in warm_agents {
            self.subscribe_agent(agent_id, cx);
        }
        // Home is a window onto this same ranking, so it is rebuilt wherever
        // the dealer is invalidated and never on a timer.
        self.refresh_home(cx);
        let top = candidates.cards.first();
        if self.phone.enabled && top.is_some() && !self.dashboard.deal_mode() {
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
            crate::journal::record(crate::journal::Event::LampTransition {
                state: if lamp_on {
                    crate::journal::SignalState::On
                } else {
                    crate::journal::SignalState::Off
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
        if chime_above && !self.chime_above_threshold {
            if let (Some(priority), Some(card)) = (max_priority, card) {
                if !cfg!(test) {
                    self.chime.play();
                }
                crate::journal::record(crate::journal::Event::ChimeRing {
                    top_priority: priority,
                    card,
                });
            }
        }
        self.chime_above_threshold = chime_above;
    }

    fn mark_agent_prompt_sent(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        let sent_at = now_ms();
        let mut facts = self.registry.agent_facts(agent_id);
        // Optimistically retire the completed card. The user's own reply must
        // never become the action that chimes back at them while daemon state
        // is making its round trip.
        facts.last_user_message_at = rho_core::UnixMs(sent_at);
        self.registry.set_agent_facts(agent_id, facts);
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
        self.contexts.retain(|context, _| keep(context));
        self.surfaces.retain(|context, _| keep(context));
        self.phone.retain_contexts(keep);
        if !self.contexts.contains_key(&self.active_context) {
            self.active_context = ContextId::Draft;
        }
    }

    pub(crate) fn handle_events(
        &mut self,
        events: Vec<HostEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for HostEvent { host, event } in events {
            match event {
                ConnEvent::Frame {
                    agent_id,
                    frame,
                    allocation,
                } => self.queue_frame(agent_id, frame, allocation, window, cx),
                event => {
                    // Preserve protocol order: a control event always sees
                    // all preceding agent state before it is handled. Frames
                    // are queued per agent and agents never move between
                    // hosts, so batching across hosts cannot reorder one
                    // agent's stream.
                    self.flush_pending_frames(window, cx);
                    self.handle_event(host, event, window, cx);
                }
            }
        }
    }

    fn queue_frame(
        &mut self,
        agent_id: AgentId,
        frame: rho_ui_proto::remote::AgentRemoteFrame,
        allocation: Option<AgentFrameAllocation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_frames.push(PendingAgentFrame {
            agent_id,
            frame,
            allocation,
        });
        if self.frame_flush_scheduled {
            return;
        }
        self.frame_flush_scheduled = true;
        // GPUI's test platform reports every window as inactive, so tests use
        // the foreground path unless they explicitly exercise this policy.
        if window.is_window_active() || cfg!(test) {
            cx.on_next_frame(window, |this, window, cx| {
                this.frame_flush_scheduled = false;
                this.flush_pending_frames(window, cx);
            });
            // `on_next_frame` attaches to a draw; make sure an otherwise idle
            // window gets one to consume the queued transport state.
            cx.notify();
        } else {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.frame_flush_scheduled = false;
                    this.flush_pending_frames(window, cx);
                });
            })
            .detach();
        }
    }

    fn flush_pending_frames(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_frames.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_frames);
        let mut allocations = Vec::with_capacity(pending.len());
        let frames = pending
            .into_iter()
            .map(|frame| {
                allocations.push(frame.allocation);
                (frame.agent_id, frame.frame)
            })
            .collect();
        self.handle_frame_batch(frames, window, cx);
        drop(allocations);
    }

    fn handle_frame_batch(
        &mut self,
        frames: Vec<(AgentId, rho_ui_proto::remote::AgentRemoteFrame)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut order = Vec::new();
        let mut changes: HashMap<AgentId, (FrameSummary, Option<u64>, bool)> = HashMap::new();
        let mut live_changed = false;

        for (agent_id, frame) in frames {
            // A transport may still deliver already-buffered frames after
            // this GUI evicts a subscription. They must not resurrect its
            // connection-local Live state or mutate the retained snapshot.
            if !self.subscriptions.accepts_frames(agent_id) {
                continue;
            }
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
            let new_context = self
                .store
                .get(agent_id)
                .and_then(|state| state.context_used);
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
                if let Some(again) = self
                    .desk_cells
                    .synced(host, node_namespace, delta, bodies, cx)
                {
                    self.send_to_host(host, again);
                }
                self.sync_tree_dashboard(host, window, cx);
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
                self.desk_cells.mutation_accepted(host, stamp);
                self.complete_desk_mutation(host, stamp, window, cx);
                self.sync_tree_dashboard(host, window, cx);
            }
            ConnEvent::DeskMutationRejected { stamp, reason } => {
                self.desk_cells.mutation_rejected(host, stamp, cx);
                self.reject_desk_mutation(host, stamp, cx);
                self.sync_tree_dashboard(host, window, cx);
                self.notice_on(None, &format!("desk: {reason}"), StyleClass::SystemInfo, cx);
            }
            ConnEvent::DeskTextApplied { id, operation } => {
                self.desk_cells.text_applied(host, id, operation, cx);
                self.sync_tree_dashboard(host, window, cx);
            }
            ConnEvent::Ready {
                agents,
                iris_agent,
                projects: workdirs,
                auth,
                machine_seed,
                agent_counter,
            } => {
                let reconnecting = self.replay_hosts.remove(&host);
                let (first_ready, initial_subscriptions) =
                    self.apply_ready(host, machine_seed, agent_counter, agents, iris_agent);
                self.prune_contexts();
                self.workdirs.retain(|workdir| workdir.host != host);
                self.workdirs.extend(
                    workdirs
                        .into_iter()
                        .map(|project| HostProject { host, project }),
                );
                if let Some(entry) = self.hosts.get_mut(host) {
                    entry.auth = Some(auth);
                }
                self.hosts.set_status(host, HostStatus::Online);
                self.refresh_draft_agent_targets(cx);
                if first_ready && matches!(self.registry.active_pane(), ActivePane::Startup) {
                    // The startup scaffold guessed before daemon data existed;
                    // refresh it now that workdir names and topics are known.
                    self.seed_draft(false, window, cx);
                }
                let agent_ids = if reconnecting {
                    let retained = self
                        .subscriptions
                        .iter()
                        .filter(|agent_id| self.registry.host_of_agent(*agent_id) == Some(host))
                        .collect::<Vec<_>>();
                    (!retained.is_empty())
                        .then_some(retained)
                        .or(initial_subscriptions)
                } else {
                    initial_subscriptions
                };
                if let Some(agent_ids) = agent_ids
                    && !agent_ids.is_empty()
                {
                    self.set_initial_subscriptions(host, agent_ids, cx);
                }
                self.update_statuses(cx);
                self.dashboard_cursor_moved(window, cx);
                cx.notify();
            }
            ConnEvent::AuthState(auth) => {
                if let Some(entry) = self.hosts.get_mut(host) {
                    entry.auth = Some(auth);
                }
                if self
                    .transient
                    .as_ref()
                    .is_some_and(|transient| transient.title() == "rate limit")
                {
                    self.transient = Some(crate::transient::usage_menu(
                        self.merged_quota_history(),
                        self.active_quota_namespaces(),
                        self.quota_history_days,
                    ));
                }
                cx.notify();
            }
            ConnEvent::AgentCreated { agent_id } => {
                self.note_agent_created(host, agent_id);
                if let Some((filing_host, parent)) = self.pending_agent_filing.take()
                    && filing_host == host
                {
                    let writes = vec![rho_desk::cells::CellWrite {
                        id: rho_desk::cells::Id::Agent(agent_id),
                        property: rho_desk::cells::Property::Parent(Some(parent)),
                    }];
                    self.apply_desk_writes(host, writes, None, window, cx);
                }
                if self.awaiting_draft_agent == Some(host) {
                    self.awaiting_draft_agent = None;
                    self.subscribe_agent(agent_id, cx);
                    // The draft became this agent: reset the compose surface
                    // and follow the new agent.
                    let label = self
                        .draft_default_workdir()
                        .map(|path| self.workdir_label(&path))
                        .unwrap_or_default();
                    self.draft_model.update(cx, |view, cx| {
                        view.set_body_text("", cx);
                        view.clear_attachments(cx);
                        view.set_workdir_text(&label, cx);
                        view.set_role_text(crate::draft_view::DEFAULT_ROLE, cx);
                        view.set_start_text(crate::draft_view::DEFAULT_START, cx);
                    });
                    self.select_agent(Some(agent_id), window, cx);
                }
                cx.notify();
            }
            ConnEvent::AgentSubscribed(agent_id) => {
                self.apply_agent_subscribed(agent_id);
                cx.notify();
            }
            ConnEvent::AgentUnloaded { agent_id, reason } => {
                if !self.apply_agent_unloaded(agent_id, reason, cx) {
                    return;
                }
                self.release_agent_view_cache(agent_id, cx);
                self.refresh_draft_agent_targets(cx);
                cx.notify();
            }
            ConnEvent::Frame {
                agent_id,
                frame,
                allocation,
            } => {
                self.handle_frame_batch(vec![(agent_id, frame)], window, cx);
                drop(allocation);
            }
            ConnEvent::AgentAttention {
                agent_id,
                attention,
                facts,
            } => {
                self.registry.set_agent_facts(agent_id, facts);
                self.apply_attention(agent_id, attention);
                self.invalidate_dealer_signals(cx);
                cx.notify();
            }
            ConnEvent::AgentTurnReport { agent_id, report } => {
                let mut facts = self.registry.agent_facts(agent_id);
                facts.needs_you_hint = report.needs_you;
                self.registry.set_agent_facts(agent_id, facts);
                self.apply_turn_report(agent_id, report);
                self.invalidate_dealer_signals(cx);
                cx.notify();
            }
            ConnEvent::ChatGptUsage {
                used_percent,
                reset_at_unix,
            } => {
                self.quota_summaries.insert(
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
                self.quota_summaries.insert(host, summaries);
                cx.notify();
            }
            ConnEvent::QuotaHistory(series) => {
                self.quota_history.insert(host, series);
                if self
                    .transient
                    .as_ref()
                    .is_some_and(|transient| transient.title() == "rate limit")
                {
                    self.transient = Some(crate::transient::usage_menu(
                        self.merged_quota_history(),
                        self.active_quota_namespaces(),
                        self.quota_history_days,
                    ));
                }
                cx.notify();
            }
            ConnEvent::GlobalUsage(series) => {
                self.global_usage.insert(host, series);
                if self
                    .transient
                    .as_ref()
                    .is_some_and(|transient| transient.title() == "model cost")
                {
                    self.transient = Some(crate::transient::global_usage_menu(
                        self.merged_global_usage(),
                        self.global_usage_days,
                    ));
                } else if self
                    .transient
                    .as_ref()
                    .is_some_and(|transient| transient.title() == "model usage share")
                {
                    self.transient = Some(crate::transient::usage_share_menu(
                        self.merged_global_usage(),
                        self.global_usage_days,
                    ));
                }
                cx.notify();
            }
            ConnEvent::AgentCostDistribution(series) => {
                self.agent_cost_usage.insert(host, series);
                if self
                    .transient
                    .as_ref()
                    .is_some_and(|transient| transient.title() == "agent cost")
                {
                    self.transient = Some(crate::transient::agent_cost_menu(
                        self.merged_agent_cost_usage(),
                        self.agent_cost_days,
                    ));
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
                // the workdir and submits again.
                if self.awaiting_draft_agent == Some(host) {
                    self.awaiting_draft_agent = None;
                }
                let source = self.error_source(host);
                self.notice_on(
                    None,
                    &format!("[{source} error: {message}]"),
                    StyleClass::SystemImportant,
                    cx,
                );
            }
            ConnEvent::Recovering(elapsed) => {
                let changed = !self
                    .hosts
                    .get(host)
                    .is_some_and(|entry| matches!(entry.status, HostStatus::Recovering(_)));
                self.hosts.set_status(host, HostStatus::Recovering(elapsed));
                if changed {
                    let source = self.host_label(host);
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
                let source = self.host_label(host);
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
                let source = self.host_label(host);
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
                if self.iris_host == Some(host) {
                    self.stop_iris();
                }
                self.update_statuses(cx);
                cx.notify();
            }
            ConnEvent::GitTransportApproval {
                request_id,
                prompt,
                response,
            } => {
                if self.minibuffer.is_some()
                    || self.transient.is_some()
                    || self.pending_git_approval.is_some()
                {
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
                    true => format!("{}: {prompt}", self.host_label(host)),
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
            true => format!("rho daemon {}", self.host_label(host)),
            false => "rho daemon".to_owned(),
        }
    }

    /// Quota headroom across hosts. ChatGPT namespaces remain independent;
    /// Claude retains the historical binding-constraint merge.
    fn merged_quota_summaries(&self) -> Vec<rho_ui_proto::QuotaSummary> {
        let mut merged: Vec<rho_ui_proto::QuotaSummary> = Vec::new();
        for (host, summaries) in &self.quota_summaries {
            for summary in summaries {
                if summary.model == "gpt" {
                    let Some(namespace) = &summary.auth_namespace else {
                        match merged.iter_mut().find(|existing| {
                            existing.model == summary.model && existing.auth_namespace.is_none()
                        }) {
                            Some(existing)
                                if summary.remaining_percent < existing.remaining_percent =>
                            {
                                *existing = summary.clone();
                            }
                            Some(_) => {}
                            None => merged.push(summary.clone()),
                        }
                        continue;
                    };
                    let mut summary = summary.clone();
                    if self.hosts.len() > 1 {
                        summary.auth_namespace =
                            Some(format!("{}/{}", self.host_label(*host), namespace));
                    }
                    merged.push(summary);
                    continue;
                }
                match merged
                    .iter_mut()
                    .find(|existing| existing.model == summary.model)
                {
                    Some(existing) if summary.remaining_percent < existing.remaining_percent => {
                        *existing = summary.clone();
                    }
                    Some(_) => {}
                    None => merged.push(summary.clone()),
                }
            }
        }
        merged.sort_by(|a, b| (&a.model, &a.auth_namespace).cmp(&(&b.model, &b.auth_namespace)));
        // An unnamed legacy entry and a named namespace can describe the
        // same account; showing identical numbers twice says nothing.
        merged.dedup_by(|a, b| {
            a.model == b.model
                && a.remaining_percent == b.remaining_percent
                && a.reset_at_unix == b.reset_at_unix
        });
        merged
    }

    /// ChatGPT history is one line per host/namespace. Claude history keeps
    /// the previous tightest-host merge because it has no named auth scope.
    fn merged_quota_history(&self) -> Vec<rho_ui_proto::QuotaSeries> {
        let mut merged: Vec<rho_ui_proto::QuotaSeries> = Vec::new();
        for (host, series_set) in &self.quota_history {
            for series in series_set {
                if series.model == "gpt" {
                    let Some(namespace) = &series.auth_namespace else {
                        continue;
                    };
                    let mut series = series.clone();
                    if self.hosts.len() > 1 {
                        series.auth_namespace =
                            Some(format!("{}/{}", self.host_label(*host), namespace));
                    }
                    merged.push(series);
                    continue;
                }
                let Some(existing) = merged
                    .iter_mut()
                    .find(|existing| existing.model == series.model)
                else {
                    merged.push(series.clone());
                    continue;
                };
                for point in &series.points {
                    match existing
                        .points
                        .iter_mut()
                        .find(|candidate| candidate.observed_at_ms == point.observed_at_ms)
                    {
                        Some(candidate)
                            if point.remaining_percent < candidate.remaining_percent =>
                        {
                            *candidate = *point;
                        }
                        Some(_) => {}
                        None => existing.points.push(*point),
                    }
                }
                existing.points.sort_by_key(|point| point.observed_at_ms);
            }
        }
        merged.sort_by(|a, b| (&a.model, &a.auth_namespace).cmp(&(&b.model, &b.auth_namespace)));
        merged
    }

    fn active_quota_namespaces(&self) -> Vec<String> {
        let qualify = self.hosts.len() > 1;
        self.hosts
            .iter()
            .filter_map(|host| {
                let namespace = host.auth.as_ref()?.active_namespace.as_ref()?;
                Some(if qualify {
                    format!("{}/{}", host.name, namespace)
                } else {
                    namespace.clone()
                })
            })
            .collect()
    }

    /// Spend and token usage sum across hosts: unlike quota headroom, cost
    /// incurred on two machines is cost incurred twice.
    fn merged_global_usage(&self) -> Vec<rho_ui_proto::AgentUsageSeries> {
        let mut merged: Vec<rho_ui_proto::AgentUsageSeries> = Vec::new();
        for series in self.global_usage.values().flatten() {
            let Some(existing) = merged
                .iter_mut()
                .find(|existing| existing.model == series.model)
            else {
                merged.push(series.clone());
                continue;
            };
            for bucket in &series.buckets {
                match existing
                    .buckets
                    .iter_mut()
                    .find(|candidate| candidate.bucket_start_ms == bucket.bucket_start_ms)
                {
                    Some(candidate) => {
                        candidate.input_tokens += bucket.input_tokens;
                        candidate.cache_read_tokens += bucket.cache_read_tokens;
                        candidate.cache_write_tokens += bucket.cache_write_tokens;
                        candidate.cache_write_1h_tokens += bucket.cache_write_1h_tokens;
                        candidate.output_tokens += bucket.output_tokens;
                        candidate.requests += bucket.requests;
                        candidate.approximate |= bucket.approximate;
                    }
                    None => existing.buckets.push(bucket.clone()),
                }
            }
            existing
                .buckets
                .sort_by_key(|bucket| bucket.bucket_start_ms);
        }
        merged
    }

    fn merged_agent_cost_usage(&self) -> Vec<Vec<rho_ui_proto::AgentCostSeries>> {
        self.agent_cost_usage.values().cloned().collect()
    }

    fn submit_prompt(&mut self, _: &SubmitPrompt, window: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_pane().surface.view {
            model.clone().update(cx, |model, cx| model.submit(cx));
            return;
        }
        if matches!(self.active_pane().surface.view, SurfaceView::ZulipNarrow(_)) {
            self.zulip_submit(cx);
            return;
        }
        if matches!(
            self.active_pane().surface.view,
            SurfaceView::SlackConversation(_)
        ) {
            self.slack_submit(cx);
            return;
        }
        match self.registry.selected_agent().copied() {
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
        if let SurfaceView::Shell { model, .. } = &self.active_pane().surface.view {
            model.clone().update(cx, |model, _| model.interrupt());
        }
    }

    pub(crate) fn cmd_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_voice(&VoiceToggle, window, cx);
    }

    fn toggle_voice(&mut self, _: &VoiceToggle, _: &mut Window, cx: &mut Context<Self>) {
        if self.realtime_task.is_some() {
            self.iris_input_muted = !self.iris_input_muted;
            if let Some(input_muted) = &self.realtime_input_muted {
                input_muted.send_replace(self.iris_input_muted);
            }
            let message = match self.iris_input_muted {
                true => "Iris microphone muted",
                false => "Iris microphone unmuted",
            };
            self.notice_on(None, message, StyleClass::SystemInfo, cx);
            return;
        }
        self.iris_input_muted = false;
        // Voice follows what the user is looking at: start Iris on the
        // selected agent's daemon, so its delegations land where the work is.
        let host = self
            .registry
            .selected_agent()
            .copied()
            .and_then(|agent_id| self.host_of(agent_id))
            .filter(|host| self.hosts.is_online(*host))
            .or_else(|| self.hosts.primary());
        self.start_iris(host, cx);
    }

    pub(crate) fn cmd_end_iris(&mut self, cx: &mut Context<Self>) {
        self.iris_session_enabled = false;
        if self.realtime_task.is_some() {
            self.stop_iris();
            self.notice_on(None, "ending Iris session…", StyleClass::SystemInfo, cx);
        } else {
            self.notice_on(None, "Iris is not active", StyleClass::SystemInfo, cx);
        }
    }

    fn stop_iris(&mut self) {
        if let Some(stop) = self.realtime_stop.take() {
            let _ = stop.send(());
        }
    }

    /// Moves the voice session to another daemon: Iris delegates by agent id,
    /// and an id minted on one daemon means nothing on another, so the
    /// session is torn down and reopened rather than re-pointed.
    pub(crate) fn cmd_iris_follow_selection(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self
            .registry
            .selected_agent()
            .copied()
            .and_then(|agent_id| self.host_of(agent_id))
        else {
            self.notice_on(
                None,
                "iris: no agent selected to follow",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        if self.iris_host == Some(host) {
            return;
        }
        let name = self.host_label(host);
        if self.realtime_task.is_some() {
            // The restart path in `start_iris` reopens on `iris_host`.
            self.iris_host = Some(host);
            self.stop_iris();
        } else {
            self.start_iris(Some(host), cx);
        }
        self.notice_on(
            None,
            &format!("moving Iris to {name}…"),
            StyleClass::SystemInfo,
            cx,
        );
    }

    fn start_iris(&mut self, host: Option<HostId>, cx: &mut Context<Self>) {
        if self.realtime_task.is_some() {
            return;
        }
        let Some(host) = host.or(self.iris_host).or_else(|| self.hosts.primary()) else {
            self.notice_on(None, "iris: no daemon attached", StyleClass::SystemInfo, cx);
            return;
        };
        self.iris_host = Some(host);
        self.iris_session_enabled = true;
        let Some(connection) = self.hosts.connection(host) else {
            return;
        };
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let (input_muted, input_muted_rx) = tokio::sync::watch::channel(self.iris_input_muted);
        let task = connection.start_native_realtime(stop_rx, input_muted_rx, cx);
        self.realtime_stop = Some(stop);
        self.realtime_input_muted = Some(input_muted);
        let starting = match self.hosts.len() > 1 {
            true => format!("starting Iris on {}…", self.host_label(host)),
            false => "starting Iris…".to_owned(),
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
                    Ok(()) => "Iris stopped listening".to_owned(),
                    Err(error) => format!("Iris failed: {error:#}"),
                };
                this.notice_on(None, &message, StyleClass::SystemInfo, cx);
                let host = this.iris_host.filter(|host| this.hosts.is_online(*host));
                if host.is_some() && this.iris_session_enabled {
                    this.start_iris(host, cx);
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
            configure_editor: crate::editor_config::configure,
            configure_markdown: crate::render::markdown::configure_buffer,
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
        if let SurfaceView::ZulipNarrow(view) = &self.active_pane().surface.view {
            view.clone().update(cx, |view, cx| view.mark_read(cx));
        }
    }

    /// `enter` inside the Zulip inbox: open the conversation under the
    /// cursor.
    fn zulip_open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let SurfaceView::ZulipInbox(view) = &self.active_pane().surface.view else {
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
        let current = match &self.active_pane().surface.view {
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
        if let SurfaceView::ZulipNarrow(view) = &self.active_pane().surface.view {
            view.clone().update(cx, |view, cx| view.load_older(cx));
        }
    }

    /// `enter` in a Zulip conversation: send the composed message.
    fn zulip_submit(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::ZulipNarrow(view) = &self.active_pane().surface.view {
            view.clone().update(cx, |view, cx| view.submit(cx));
        }
    }

    fn shell_eof(&mut self, _: &ShellEof, _: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_pane().surface.view {
            model.clone().update(cx, |model, cx| model.eof(cx));
        }
    }

    fn shell_pager_action(
        &mut self,
        action: rho_ui_proto::shell::PagerAction,
        cx: &mut Context<Self>,
    ) {
        if let SurfaceView::Shell { model, .. } = &self.active_pane().surface.view {
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
        if !self.connected() {
            self.notice_on(
                None,
                "not connected to rho-daemon",
                StyleClass::SystemImportant,
                cx,
            );
            return;
        }
        let field = self.draft_model.read(cx).workdir_text(cx).trim().to_owned();
        let working_directory = if field.is_empty() {
            self.draft_default_workdir()
        } else {
            match self.resolve_workdir(&field) {
                Ok(workdir) => Some(workdir),
                Err(message) => {
                    self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                    return;
                }
            }
        };
        let (host, start) = {
            let draft = self.draft_model.read(cx);
            let mode = draft.start_mode();
            let target = draft.start_text(cx).trim().to_owned();
            match self.parse_start(mode, &target, working_directory, None) {
                Ok(start) => start,
                Err(message) => {
                    self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                    return;
                }
            }
        };
        let role = match parse_agent_role(self.draft_model.read(cx).role_text(cx).trim()) {
            Ok(role) => role,
            Err(message) => {
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
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
            self.active_pane().surface.view,
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
            let added = match &self.active_pane().surface.view {
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
            match &self.active_pane().surface.view {
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
            let agent_id = self.registry.selected_agent().copied();
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
    fn parse_start(
        &self,
        mode: crate::draft_view::StartFieldMode,
        target: &str,
        workdir: Option<HostPath>,
        selected_host: Option<HostId>,
    ) -> Result<(HostId, rho_ui_proto::StartMode), String> {
        use rho_ui_proto::{JoinTarget, StartMode, WorkspaceInfo};

        use crate::draft_view::StartFieldMode;
        let require_workdir = || {
            workdir.clone().ok_or_else(|| {
                "no working directory for the new agent: type one in the \
                 Workdir field, or register one with :projects add <path>"
                    .to_owned()
            })
        };
        // An agent target settles the host by itself: the new agent shares
        // that agent's repository, which only exists on that agent's daemon.
        // Where the workdir also names a host, the two must agree — nothing
        // downstream could reconcile a checkout on one machine with a base
        // revision on another.
        let base_agent = self.registry.agent_by_label(target);
        let base_host = base_agent.and_then(|agent_id| self.host_of(agent_id));
        if let Some(selected) = selected_host {
            if let Some(workdir) = &workdir
                && workdir.host != selected
            {
                return Err("the selected project belongs to a different host".to_owned());
            }
            if let Some(base) = base_host
                && base != selected
            {
                return Err(format!(
                    "`{target}` is on {}, not the selected host {}",
                    self.host_label(base),
                    self.host_label(selected),
                ));
            }
        }
        let host = match (base_host, &workdir) {
            (Some(base), Some(workdir)) if base != workdir.host => {
                return Err(format!(
                    "`{target}` is on {}, but the working directory is on {}: \
                     an agent cannot start from a base on another host",
                    self.host_label(base),
                    self.host_label(workdir.host),
                ));
            }
            (Some(base), _) => base,
            (None, Some(workdir)) => workdir.host,
            (None, None) => selected_host
                .or_else(|| self.hosts.primary())
                .ok_or_else(|| "not connected to rho-daemon".to_owned())?,
        };
        let workspace = base_agent
            .and_then(|agent_id| self.registry.agent_workspace(agent_id))
            .cloned();
        let start = match (mode, target, workspace) {
            (StartFieldMode::Sandbox, "", _) => {
                return Err("pick a sandbox base: a revset like `@-` or an agent label".to_owned());
            }
            (
                StartFieldMode::Sandbox,
                _,
                Some(WorkspaceInfo::Workspace { repo, id } | WorkspaceInfo::Sandbox { repo, id }),
            ) => StartMode::Sandbox {
                repo,
                revset: format!("{}@", id.encoded()),
            },
            (StartFieldMode::Sandbox, _, Some(WorkspaceInfo::UserCheckout { repo })) => {
                StartMode::Sandbox {
                    repo,
                    revset: "@".to_owned(),
                }
            }
            (StartFieldMode::Sandbox, _, None) => StartMode::Sandbox {
                repo: require_workdir()?.path,
                revset: if target.eq_ignore_ascii_case(crate::draft_view::DEFAULT_START) {
                    crate::draft_view::AUTO_BASE_REVSET
                } else {
                    target
                }
                .to_owned(),
            },
            (StartFieldMode::NewOn, "", _) => {
                return Err("pick a base: a revset like `@-` or an agent label".to_owned());
            }
            (
                StartFieldMode::NewOn,
                _,
                Some(WorkspaceInfo::Workspace { repo, id } | WorkspaceInfo::Sandbox { repo, id }),
            ) => StartMode::NewOn {
                repo,
                revset: format!("{}@", id.encoded()),
            },
            // An agent in the user's checkout works on the user's own change.
            (StartFieldMode::NewOn, _, Some(WorkspaceInfo::UserCheckout { repo })) => {
                StartMode::NewOn {
                    repo,
                    revset: "@".to_owned(),
                }
            }
            (StartFieldMode::NewOn, _, None) => {
                if target.eq_ignore_ascii_case("user") {
                    return Err("`user` is a join target; base on a revset like `@-`, \
                         or Shift-Tab to Join mode"
                        .to_owned());
                }
                if target
                    .strip_prefix('@')
                    .is_some_and(|label| label.starts_with('a'))
                {
                    return Err(format!("no agent named `{target}`"));
                }
                StartMode::NewOn {
                    repo: require_workdir()?.path,
                    revset: if target.eq_ignore_ascii_case(crate::draft_view::DEFAULT_START) {
                        crate::draft_view::AUTO_BASE_REVSET
                    } else {
                        target
                    }
                    .to_owned(),
                }
            }
            (StartFieldMode::Join, _, Some(workspace)) => {
                StartMode::Join(JoinTarget::Workspace(workspace))
            }
            (StartFieldMode::Join, target, None) => {
                if target.is_empty() || target.eq_ignore_ascii_case("user") {
                    StartMode::Join(JoinTarget::User {
                        repo: require_workdir()?.path,
                    })
                } else {
                    return Err(format!(
                        "join target must be `user` or an agent label, not `{target}`"
                    ));
                }
            }
        };
        Ok((host, start))
    }

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
                .map(|host| self.host_label(host))
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
            }
            | AgentRole::WorkflowEngineer {
                intelligence:
                    EngineerIntelligence::Low
                    | EngineerIntelligence::Cheap
                    | EngineerIntelligence::Medium
                    | EngineerIntelligence::High,
                ..
            } => &["eng-low", "eng-cheap", "eng", "eng-high"],
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Ultra | EngineerIntelligence::Alt,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Ultra | EngineerIntelligence::Alt,
                ..
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
        let disposition = if hide {
            rho_ui_proto::AgentDisposition::Hidden
        } else {
            rho_ui_proto::AgentDisposition::Done
        };
        let targets = self.subject(window, cx).agents;
        let hid_open_agent = self
            .registry
            .selected_agent()
            .is_some_and(|agent_id| targets.contains(agent_id));
        let sent = self.set_agent_disposition(targets, "done", disposition, cx);
        // Hiding the open agent closes its tab, or it would stay
        // rail-visible through the selection exemption.
        if hide && sent && hid_open_agent {
            self.select_agent(None, window, cx);
        }
    }

    /// The snooze operator: `s` and a unit, with vim's count in front, so
    /// `45sm` is 45 minutes, `3sh` three hours, `2sd` two days, `sw` a week
    /// and `ss` the default day. The deal bar echoes the time it comes back.
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
        if self.dashboard.current_deal_card().is_none() {
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
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require_connected(cx) {
            return;
        }
        let until = rho_core::UnixMs(now_ms().saturating_add(duration_ms));
        let targets = self.subject(window, cx).agents;
        self.set_agent_disposition(
            targets,
            "snooze",
            rho_ui_proto::AgentDisposition::Snoozed { until },
            cx,
        );
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
        let workdir = match self.resolve_workdir(&path) {
            Ok(workdir) => workdir,
            Err(message) => {
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                return;
            }
        };
        self.hosts.send(
            workdir.host,
            ClientMessage::ProjectSet {
                path: workdir.path,
                name,
                description,
            },
        );
    }

    pub(crate) fn cmd_project_remove(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        match self.registered_workdir(&path) {
            Some(workdir) => self.hosts.send(
                workdir.host,
                ClientMessage::ProjectRemove { path: workdir.path },
            ),
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
        self.observe_browser_metadata(&model, cx);
        let view = cx.new(|cx| rho_browser::PageView::new(model, id, cx));
        let surface = Self::wrap_surface(SurfaceKey::Browser(id), SurfaceView::Browser(view));
        self.display_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    fn observe_browser_metadata(
        &mut self,
        model: &Entity<rho_browser::PageModel>,
        cx: &mut Context<Self>,
    ) {
        if self.browser_metadata_subscription.is_none() {
            self.browser_metadata_subscription = Some(
                cx.subscribe(model, |_, _, _: &rho_browser::PageMetadataChanged, cx| {
                    cx.notify()
                }),
            );
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
                this.file_page(
                    id,
                    parent,
                    crate::journal::CreateMethod::TabBirth,
                    window,
                    cx,
                );
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
        method: crate::journal::CreateMethod,
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
        crate::journal::record(crate::journal::Event::Created {
            node_id: id.into(),
            kind: crate::journal::CreatedKind::Page,
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
        let host = match self.registry.selected_agent().copied() {
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
                        workspace.host_label(host)
                    ),
                    StyleClass::SystemInfo,
                    cx,
                );
            },
        );
        self.open_prompt(
            format!("auth on {}:", self.host_label(host)),
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
                let workdir = match self.resolve_workdir(argument.as_str()) {
                    Ok(workdir) => workdir,
                    Err(message) => {
                        self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                        return;
                    }
                };
                let label = self.workdir_label(&workdir);
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
        if matches!(self.registry.active_pane(), ActivePane::Startup) {
            self.registry.enter_draft();
            cx.notify();
        }
    }

    /// Seeds this GUI's bounded transcript subscription set for a host that
    /// has just come up. The LRU is shared across hosts — it bounds this
    /// client's memory, not any one daemon's — so a newly attached host adds
    /// its warm set rather than resetting everyone else's.
    fn set_initial_subscriptions(
        &mut self,
        host: HostId,
        agent_ids: Vec<AgentId>,
        cx: &mut Context<Self>,
    ) {
        for agent_id in &agent_ids {
            let (_, evicted) = self.subscriptions.touch(*agent_id);
            if let Some(evicted) = evicted {
                self.send_to_agent(
                    evicted,
                    ClientMessage::UnsubscribeAgents {
                        agent_ids: vec![evicted],
                    },
                );
                self.release_agent_view_cache(evicted, cx);
            }
        }
        self.hosts
            .send(host, ClientMessage::SubscribeAgents { agent_ids });
    }

    fn subscribe_agent(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        let (subscribe, evicted) = self.subscriptions.touch(agent_id);
        if let Some(evicted) = evicted {
            self.send_to_agent(
                evicted,
                ClientMessage::UnsubscribeAgents {
                    agent_ids: vec![evicted],
                },
            );
            self.release_agent_view_cache(evicted, cx);
        }
        if subscribe {
            self.send_to_agent(
                agent_id,
                ClientMessage::SubscribeAgents {
                    agent_ids: vec![agent_id],
                },
            );
        }
    }

    /// Applies the subscription LRU's eviction to client-side editor state.
    /// Visible transcripts stay pinned; dashboard previews, viewport history,
    /// and hidden transcript surfaces are cache and can be rebuilt lazily.
    fn release_agent_view_cache(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        if let Some(model) = self.models.get(&agent_id).cloned() {
            model.update(cx, |model, _| model.clear_preview_editor());
        }

        for pane in self.contexts.values_mut() {
            pane.purge_history(|surface| surface.key == SurfaceKey::Transcript(agent_id));
        }
        let shown = self
            .contexts
            .values()
            .any(|pane| pane.surface.key == SurfaceKey::Transcript(agent_id));
        if shown {
            return;
        }

        for surfaces in self.surfaces.values_mut() {
            surfaces.retain(|surface| surface.key != SurfaceKey::Transcript(agent_id));
        }
        self.phone.remove_key(&SurfaceKey::Transcript(agent_id));
        self.pending_syncs.remove(&agent_id);
        self.models.remove(&agent_id);
    }

    pub fn open_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        crate::journal::record(crate::journal::Event::AgentOpened {
            agent_id: agent_id.into(),
        });
        if self.agent_online(agent_id) && !self.subscriptions.contains(agent_id) {
            self.subscribe_agent(agent_id, cx);
        }
        self.select_agent(Some(agent_id), window, cx);
    }

    /// What every command acts on. Which one is asking has to be read from
    /// focus, not from whether a tab happens to be open: triaging the rail
    /// with a conversation open is the normal way to work, and taking the
    /// tab's agent there would act on something the user is not looking at.
    ///
    /// Rows that name nobody — Iris, the draft, the rail tail — fall through
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
            self.registry
                .selected_agent()
                .copied()
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

    /// Sends one verdict per agent of the row. All of them live on the same
    /// daemon, so one online check covers the batch.
    fn set_agent_disposition(
        &mut self,
        targets: Vec<AgentId>,
        command: &str,
        disposition: rho_ui_proto::AgentDisposition,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(&first) = targets.first() else {
            let message = format!("{command}: no agent under the cursor");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            return false;
        };
        if !self.require_agent_online(first, cx) {
            return false;
        }
        // Every verdict but a lapsed snooze puts the row down.
        let quieting = match disposition {
            rho_ui_proto::AgentDisposition::Done | rho_ui_proto::AgentDisposition::Hidden => true,
            rho_ui_proto::AgentDisposition::Snoozed { until } => until.0 > now_ms(),
            rho_ui_proto::AgentDisposition::Pending => false,
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
            self.send_to_agent(
                agent_id,
                ClientMessage::SetAgentDisposition {
                    agent_id,
                    disposition,
                },
            );
            // Show the verdict now rather than waiting for the round trip.
            // A still-working agent keeps its lamp: the daemon reads
            // attention off the live runtime first, so predicting quiet
            // there would flicker.
            if quieting && self.registry.attention(agent_id) != rho_ui_proto::UiAttention::Working {
                self.registry
                    .expect_attention(agent_id, rho_ui_proto::UiAttention::Quiet);
            }
        }
        self.invalidate_dealer_signals(cx);
        cx.notify();
        true
    }

    /// Tab in the draft cycles the `Workdir:` field, the start field, and
    /// the body. On agent views it does nothing.
    fn cycle_draft_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.registry.selected_agent().is_none()
            && let Some(editor) = self.focused_draft_editor()
        {
            self.draft_model
                .update(cx, |view, cx| view.toggle_field(&editor, window, cx));
        }
    }

    /// Shift-Tab in the draft: with the cursor in the start field, cycle its
    /// mode (on top of ↔ join); anywhere else, cycle fields like Tab. On agent
    /// views it does nothing.
    fn cycle_draft_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.registry.selected_agent().is_none()
            && let Some(editor) = self.focused_draft_editor()
        {
            self.draft_model.update(cx, |view, cx| {
                if view.cursor_in_role_field(&editor, cx) {
                    let next = cycle_agent_role_text(&view.role_text(cx));
                    view.set_role_text(next, cx);
                } else if view.cursor_in_start_field(&editor, cx) {
                    view.cycle_start_mode(cx);
                } else {
                    view.toggle_field(&editor, window, cx);
                }
            });
        }
    }

    /// (Re)writes the draft scaffold with the derived default workdir; the
    /// field stays empty when nothing daemon-side suggests one.
    fn seed_draft(&mut self, force_header: bool, window: &mut Window, cx: &mut Context<Self>) {
        let label = self
            .draft_default_workdir()
            .map(|path| self.workdir_label(&path))
            .unwrap_or_default();
        let editor = self.focused_draft_editor();
        self.draft_model.update(cx, |view, cx| {
            view.seed(&label, force_header, editor.as_ref(), window, cx)
        });
    }

    /// Where a new agent works when the draft doesn't say: the selected
    /// agent sets the precedent, else the first registered workdir.
    fn draft_default_workdir(&self) -> Option<HostPath> {
        self.registry
            .selected_agent()
            .copied()
            .and_then(|agent_id| self.agent_workdir(agent_id))
            .or_else(|| {
                self.workdirs.first().map(|workdir| HostPath {
                    host: workdir.host,
                    path: workdir.project.path.clone(),
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

    /// How a workdir reads in the draft header: its registered project name
    /// when it has one, else the full path — qualified by host whenever more
    /// than one daemon could be meant.
    fn workdir_label(&self, workdir: &HostPath) -> String {
        let name = self
            .workdirs
            .iter()
            .find(|candidate| {
                candidate.host == workdir.host && candidate.project.path == workdir.path
            })
            .map(|candidate| candidate.project.name.clone());
        match name {
            Some(name) => self.qualify(workdir.host, &name),
            None if self.hosts.len() > 1 => {
                format!("{}:{}", self.host_label(workdir.host), workdir.path)
            }
            None => workdir.path.to_string(),
        }
    }

    /// Registered workdirs as the `(name, description)` table the shared
    /// command layer expects. Names carry their host once more than one is
    /// attached, since two machines can register the same project name.
    pub fn workdir_table(&self) -> Vec<(String, String)> {
        self.workdirs
            .iter()
            .map(|workdir| {
                (
                    self.qualify(workdir.host, &workdir.project.name),
                    match self.hosts.len() > 1 {
                        true => {
                            format!("{}:{}", self.host_label(workdir.host), workdir.project.path)
                        }
                        false => workdir.project.path.to_string(),
                    },
                )
            })
            .collect()
    }

    /// Matches an argument against the registered projects, by qualified
    /// name, bare name, or path. A bare name that several hosts register is
    /// ambiguous and matches nothing.
    fn registered_workdir(&self, argument: &str) -> Option<HostPath> {
        let workdir = |candidate: &HostProject| HostPath {
            host: candidate.host,
            path: candidate.project.path.clone(),
        };
        if let Some(exact) = self.workdirs.iter().find(|candidate| {
            self.qualify(candidate.host, &candidate.project.name) == argument
                || candidate.project.path == argument
        }) {
            return Some(workdir(exact));
        }
        let mut bare = self
            .workdirs
            .iter()
            .filter(|candidate| candidate.project.name == argument);
        let first = bare.next()?;
        bare.next().is_none().then(|| workdir(first))
    }

    /// Resolves a workdir argument to a directory on a specific daemon. A
    /// registered project name resolves to its registration; anything else
    /// is a raw daemon-side path, which may name its host as `fern:/src/rho`.
    /// Paths name directories on the daemon's machine, so the GUI never
    /// joins its own cwd or expands its own home — the daemon expands `~`
    /// and validates.
    fn resolve_workdir(&self, argument: &str) -> Result<HostPath, String> {
        if let Some(registered) = self.registered_workdir(argument) {
            return Ok(registered);
        }
        // A Windows-style drive letter is not a thing on a daemon host, so a
        // colon before any separator is unambiguously a host prefix.
        if let Some((name, path)) = argument.split_once(':')
            && !name.contains('/')
        {
            let host = self
                .hosts
                .by_name(name)
                .ok_or_else(|| format!("no attached host named `{name}`"))?;
            return Ok(HostPath {
                host: host.id,
                path: Utf8PathBuf::from(path),
            });
        }
        let host = match self.hosts.len() {
            0 => return Err("not connected to rho-daemon".to_owned()),
            1 => self.hosts.iter().next().expect("one host").id,
            _ => {
                return Err(format!(
                    "`{argument}` does not say which host: write `<host>:{argument}` \
                     or use a registered project name"
                ));
            }
        };
        Ok(HostPath {
            host,
            path: Utf8PathBuf::from(argument),
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
        let entry = MessageLogEntry {
            timestamp: chrono::Local::now().fixed_offset(),
            class,
            text: text.lines().collect::<Vec<_>>().join(" "),
        };
        let line = Self::render_message_entry(&entry);
        let evicted = self.message_log.push(entry);
        let removed_len = if evicted {
            self.messages_line_lengths
                .pop_front()
                .expect("capped log has a rendered first line")
        } else {
            0
        };
        self.messages_line_lengths.push_back(line.len());
        let range = self.messages_buffer.update(cx, |buffer, cx| {
            let old_len = buffer.len();
            let mut edits = Vec::with_capacity(2);
            if removed_len > 0 {
                edits.push((0..removed_len, ""));
            }
            edits.push((old_len..old_len, line.as_str()));
            buffer.edit(edits, None, cx);
            let start = buffer.len() - line.len();
            buffer.anchor_before(start)..buffer.anchor_before(buffer.len())
        });
        if evicted {
            self.messages_styles.remove(0);
            self.message_evictions_since_rebase += 1;
        }
        self.messages_styles.push((class, range));
        self.apply_message_styles(cx);
        if self.message_evictions_since_rebase >= MESSAGE_REBASE_EVICTIONS
            && !self.message_rebase_scheduled
        {
            self.message_rebase_scheduled = true;
            cx.spawn(async move |this, cx| {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.rebase_messages_buffer(window, cx)
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn render_message_entry(entry: &MessageLogEntry) -> String {
        format!("{}  {}\n", entry.timestamp.format("%H:%M"), entry.text)
    }

    fn apply_message_styles(&mut self, cx: &mut Context<Self>) {
        let multi_buffer = self.messages_editor.read(cx).buffer().clone();
        let current_classes = self
            .messages_styles
            .iter()
            .map(|(class, _)| *class)
            .collect::<HashSet<_>>();
        let mut by_class = self
            .messages_applied_classes
            .union(&current_classes)
            .copied()
            .map(|class| (class, Vec::new()))
            .collect::<Vec<(StyleClass, Vec<std::ops::Range<text::Anchor>>)>>();
        for (class, range) in &self.messages_styles {
            by_class
                .iter_mut()
                .find(|(existing, _)| existing == class)
                .expect("current class was seeded")
                .1
                .push(range.clone());
        }
        crate::highlights::apply_class_highlights(
            &self.messages_editor,
            &multi_buffer,
            crate::style::Region::System,
            by_class
                .iter()
                .map(|(class, ranges)| (*class, ranges.as_slice())),
            cx,
        );
        self.messages_applied_classes = current_classes;
    }

    fn rebase_messages_buffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.message_rebase_scheduled = false;
        self.message_evictions_since_rebase = 0;
        let scroll_position = self
            .messages_editor
            .update(cx, |editor, cx| editor.scroll_position(cx));
        let following = self.messages_editor.read(cx).has_active_autoscroll_pin();
        let rendered = self
            .message_log
            .0
            .iter()
            .map(Self::render_message_entry)
            .collect::<String>();
        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local(rendered, cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let mut offset = 0;
        self.messages_styles = buffer.update(cx, |buffer, _| {
            self.message_log
                .0
                .iter()
                .zip(&self.messages_line_lengths)
                .map(|(entry, len)| {
                    let start = offset;
                    offset += *len;
                    (
                        entry.class,
                        buffer.anchor_before(start)..buffer.anchor_before(offset),
                    )
                })
                .collect()
        });
        let multi_buffer = self.messages_editor.read(cx).buffer().clone();
        multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_path(
                multi_buffer::PathKey::sorted(0),
                buffer.clone(),
                [language::Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
        });
        self.messages_buffer = buffer;
        self.apply_message_styles(cx);
        if !following {
            self.messages_editor.update(cx, |editor, cx| {
                editor.set_scroll_position(scroll_position, window, cx);
            });
        }
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
        self.display_surface_with_method(surface, crate::journal::SurfaceShowMethod::Command, cx);
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn message_log_texts(&self) -> Vec<&str> {
        self.message_log
            .0
            .iter()
            .map(|entry| entry.text.as_str())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn append_test_log_entry(&mut self, text: String) {
        let _ = self.message_log.push(MessageLogEntry {
            timestamp: chrono::Local::now().fixed_offset(),
            class: StyleClass::SystemInfo,
            text,
        });
    }

    #[cfg(test)]
    pub(crate) fn seed_messages_for_test(
        &mut self,
        entries: impl IntoIterator<Item = (StyleClass, String)>,
        cx: &mut Context<Self>,
    ) {
        self.message_log = MessageLog::default();
        self.messages_line_lengths.clear();
        let mut rendered = String::new();
        let mut spans = Vec::new();
        for (class, text) in entries {
            let entry = MessageLogEntry {
                timestamp: chrono::Local::now().fixed_offset(),
                class,
                text,
            };
            let line = Self::render_message_entry(&entry);
            let start = rendered.len();
            rendered.push_str(&line);
            spans.push((class, start..rendered.len()));
            self.messages_line_lengths.push_back(line.len());
            let _ = self.message_log.push(entry);
        }
        self.messages_styles = self.messages_buffer.update(cx, |buffer, cx| {
            let old_len = buffer.len();
            buffer.edit([(0..old_len, rendered.as_str())], None, cx);
            spans
                .into_iter()
                .map(|(class, range)| {
                    (
                        class,
                        buffer.anchor_before(range.start)..buffer.anchor_before(range.end),
                    )
                })
                .collect()
        });
        self.apply_message_styles(cx);
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
    pub(crate) fn messages_buffer_id(&self) -> gpui::EntityId {
        self.messages_buffer.entity_id()
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
    pub(crate) fn echo_text_for_test(&self) -> Option<&str> {
        self.echo.as_ref().map(|echo| echo.text())
    }

    #[cfg(test)]
    pub(crate) fn desk_cells_snapshot_for_test(
        &self,
        host: HostId,
    ) -> Vec<crate::desk_view::DeskNode> {
        self.desk_cells
            .tree_source(host)
            .map(|(nodes, _)| nodes)
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn semantic_undo_count_for_test(&self) -> usize {
        self.desk_semantic_undo.len()
    }

    #[cfg(test)]
    pub(crate) fn messages_following(&self, cx: &App) -> bool {
        self.messages_editor.read(cx).has_active_autoscroll_pin()
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
        if self.connected() && !self.subscriptions.contains(agent_id) {
            self.subscribe_agent(agent_id, cx);
        }
        let view = self.materialize_model(&agent_id, window, cx);
        view.update(cx, |view, cx| view.tick_timers(now_ms(), cx));
        self.dashboard_preview = Some(agent_id);
        {
            self.dashboard_web_preview = None;
        }
        self.hosts
            .focus_agent(self.host_of(agent_id).map(|host| (host, agent_id)));
        self.ensure_duration_timer(cx);
        cx.notify();
    }

    fn preview_browser_page(
        &mut self,
        id: rho_browser::PageId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .dashboard_web_preview
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
        {
            return;
        }
        let Some(model) = rho_browser::open_page(id, cx) else {
            return;
        };
        self.scan_browser_pages_for_gc(cx);
        self.observe_browser_metadata(&model, cx);
        let view = cx.new(|cx| rho_browser::PageView::new(model, id, cx));
        self.dashboard_preview = None;
        self.hosts.focus_agent(None);
        self.dashboard_web_preview = Some((id, view));
        cx.notify();
    }

    fn current_iris_agent(&self) -> Option<AgentId> {
        let host = self.iris_host.or_else(|| self.hosts.primary())?;
        self.iris_agents.get(&host).copied()
    }

    fn select_agent_inner(
        &mut self,
        agent_id: Option<AgentId>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::journal::record(crate::journal::Event::AgentSelected {
            agent_id: agent_id.map(|id| id.encoded()),
        });
        // Any other route to the draft page composes at the root; only
        // `n a` sets an area, and it sets it after this.
        self.draft_area = None;
        if self.connected()
            && let Some(agent_id) = agent_id
        {
            // Selection is the strongest subscription signal. Touch it here
            // so keyboard cycling and every other direct selection path keep
            // the visible transcript at the protected end of the LRU.
            self.subscribe_agent(agent_id, cx);
        }
        if let Some(agent_id) = &agent_id {
            let view = self.materialize_model(agent_id, window, cx);
            view.update(cx, |view, cx| view.tick_timers(now_ms(), cx));
        }
        let (context, key) = match agent_id {
            Some(agent_id) => {
                self.registry.select_agent(agent_id);
                (
                    self.context_for_agent(agent_id),
                    SurfaceKey::Transcript(agent_id),
                )
            }
            None => {
                self.registry.enter_draft();
                (ContextId::Draft, SurfaceKey::Draft)
            }
        };
        self.active_context = context;
        let surface = self.make_surface(key, window, cx);
        self.display_surface(surface, cx);
        if focus {
            self.focus_active_surface(window, cx);
        }
        self.hosts
            .focus_agent(agent_id.and_then(|agent_id| Some((self.host_of(agent_id)?, agent_id))));
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
        let web_preview_empty = self.dashboard_web_preview.is_none();
        if self.dashboard_preview.is_none() && web_preview_empty {
            return;
        }
        self.dashboard_preview = None;
        {
            self.dashboard_web_preview = None;
        }
        self.hosts.focus_agent(None);
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

    /// Removes a surface from the context. The viewport falls back to its
    /// history, then to the list's most recent conversation
    /// surface. Dropping a terminal's last view detaches its wire client
    /// (the daemon keeps the pty; reopening the terminal reattaches).
    pub(crate) fn close_surface(
        &mut self,
        name: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = match name {
            Some(name) => match self.surface_named(name) {
                Some(surface) => surface.key.clone(),
                None => {
                    self.notice_on(
                        None,
                        &format!("no surface matching `{name}`"),
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
            },
            None => self.active_pane().surface.key.clone(),
        };
        let Some(list) = self.surfaces.get_mut(&self.active_context) else {
            return;
        };
        if list.iter().filter(|s| s.key != key).count() == 0 {
            self.notice_on(
                None,
                ":close: nothing else to show",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        list.retain(|surface| surface.key != key);
        if self.phone.enabled {
            self.phone.remove(self.active_context, &key);
        }
        let fallback = list
            .iter()
            .rev()
            .find(|surface| surface.key.is_conversation())
            .or_else(|| list.last())
            .cloned()
            .expect("list retains at least one surface");

        let pane = self.active_pane_mut();
        pane.purge_history(|surface| surface.key == key);
        if pane.surface.key == key && !pane.back() {
            pane.surface = fallback;
        }
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        if let SurfaceKey::Transcript(agent_id) = key
            && !self.subscriptions.contains(agent_id)
        {
            self.release_agent_view_cache(agent_id, cx);
        }
        cx.notify();
    }

    fn journal_surface(key: &SurfaceKey) -> crate::journal::SurfaceIdentity {
        use crate::journal::SurfaceIdentity;
        match key {
            SurfaceKey::Draft => SurfaceIdentity::Draft,
            SurfaceKey::Home => SurfaceIdentity::Home,
            SurfaceKey::Messages => SurfaceIdentity::Messages,
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
            if self.dashboard.deal_mode() && delta.y > px(12.) && delta.y.abs() > delta.x.abs() {
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
                crate::journal::SurfaceIdentity::Dashboard,
                editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64),
            )
        } else {
            let pane = self.active_pane();
            let position = match &pane.surface.view {
                SurfaceView::Home(view) => {
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
            (Self::journal_surface(&pane.surface.key), position)
        };
        self.scroll_journal_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;
            crate::journal::record(crate::journal::Event::Scroll {
                surface,
                rough_position,
            });
        }));
    }

    /// Emacs `display-buffer`: the one place surface display happens. The
    /// surface joins the context's surface list first, so it stays alive
    /// while hidden. The context's single viewport shows it, and is founded
    /// on the context's first visit.
    pub(crate) fn display_surface(&mut self, surface: Surface, cx: &mut Context<Self>) {
        let method = if self.overview_open {
            crate::journal::SurfaceShowMethod::Overview
        } else {
            crate::journal::SurfaceShowMethod::Open
        };
        self.display_surface_with_method(surface, method, cx);
    }

    pub(crate) fn display_surface_with_method(
        &mut self,
        surface: Surface,
        method: crate::journal::SurfaceShowMethod,
        cx: &mut Context<Self>,
    ) {
        use std::collections::hash_map::Entry;
        self.ensure_surface_subscription(&surface.key, cx);
        let list = self.surfaces.entry(self.active_context).or_default();
        match list.iter_mut().find(|s| **s == surface) {
            Some(existing) => *existing = surface.clone(),
            None => list.push(surface.clone()),
        }
        // Home is not a card on the phone: it is what the feed shows when
        // there is nothing to deal, so it never joins the stack.
        if self.phone.enabled && surface.key != SurfaceKey::Home {
            if method == crate::journal::SurfaceShowMethod::Deal {
                self.phone
                    .show_feed(self.active_context, surface.key.clone());
            } else {
                self.phone.show(self.active_context, surface.key.clone());
            }
        }
        let shown = match self.contexts.entry(self.active_context) {
            Entry::Vacant(entry) => {
                entry.insert(Pane::new(surface.clone()));
                surface
            }
            Entry::Occupied(entry) => {
                let pane = entry.into_mut();
                pane.show(surface);
                pane.surface.clone()
            }
        };
        self.overview_open = false;
        match method {
            crate::journal::SurfaceShowMethod::Deal => {
                self.append_history(shown.clone(), crate::journal::HistoryAppendMethod::Deal, cx)
            }
            crate::journal::SurfaceShowMethod::Overview => self.append_history(
                shown.clone(),
                crate::journal::HistoryAppendMethod::Overview,
                cx,
            ),
            crate::journal::SurfaceShowMethod::Command => self.append_history(
                shown.clone(),
                crate::journal::HistoryAppendMethod::Command,
                cx,
            ),
            crate::journal::SurfaceShowMethod::Open => {
                if let Some(index) = self
                    .surface_history
                    .iter()
                    .position(|warm| warm.surface.key == shown.key)
                {
                    self.history_cursor = index;
                }
            }
            crate::journal::SurfaceShowMethod::Mru => {}
        }
        crate::journal::record(crate::journal::Event::SurfaceShown {
            surface: Self::journal_surface(&shown.key),
            method,
        });
    }

    fn ensure_surface_subscription(&mut self, key: &SurfaceKey, cx: &mut Context<Self>) {
        let agent_id = match key {
            SurfaceKey::Transcript(agent_id) | SurfaceKey::File { agent_id, .. } => Some(*agent_id),
            _ => None,
        };
        if let Some(agent_id) = agent_id
            && self.agent_online(agent_id)
            && !self.subscriptions.contains(agent_id)
        {
            self.subscribe_agent(agent_id, cx);
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
            Some(crate::zed_remote::open_remote_project(
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
                    crate::zed_remote::open_file_buffer(&project, path, cx)
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
                        let model = cx.new(|cx| crate::shell_view::ShellModel::new(channel, cx));
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
            crate::zed_remote::open_remote_project(connection, workspace.clone(), cx)
        });
        let task = cx.spawn(async move |this, cx| {
            let result: anyhow::Result<(RemoteProject, crate::diff_view::PreparedDiff)> = async {
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
                let live_paths = cx.update(|cx| crate::diff_view::dirty_paths(&project, cx));
                let snapshot_task = cx.update(|cx| {
                    diff_client.snapshot(workspace.clone(), None, live_paths.clone(), cx)
                });
                let snapshot = snapshot_task
                    .await?
                    .context("initial diff snapshot unexpectedly unchanged")?;
                let prepared = crate::diff_view::PreparedDiff::load(
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
                            crate::diff_view::DiffModel::new(
                                project,
                                diff_client,
                                workspace,
                                prepared,
                                cx,
                            )
                        });
                        let view = cx.new(|cx| crate::diff_view::DiffView::new(model, window, cx));
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
                        let model =
                            cx.new(|cx| crate::terminal_view::TerminalModel::new(channel, cx));
                        let view = cx.new(|cx| crate::terminal_view::TerminalView::new(model, cx));
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
        let Some(agent_id) = self.registry.next_agent(delta) else {
            self.notice_on(
                None,
                "agent-switch: no visible agents available",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        if self.registry.selected_agent() == Some(&agent_id) {
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
        let (view, _) = self.ensure_agent_model(*agent_id, window, cx);
        if view.read(cx).initial_load_ready()
            && let (Some(summary), Some(state)) = (
                self.pending_syncs.remove(agent_id),
                self.store.get(agent_id),
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
        let context_used = self
            .store
            .get(agent_id)
            .and_then(|state| state.context_used);
        view.update(cx, |view, cx| {
            view.set_status("", None, None, None, context_used, cx)
        });
    }

    #[cfg(test)]
    pub(crate) fn agent_model(&self, agent_id: &AgentId) -> Option<Entity<AgentModel>> {
        self.models.get(agent_id).cloned()
    }

    #[cfg(test)]
    pub(crate) fn dashboard_editor(&self) -> Entity<editor::Editor> {
        self.dashboard.editor().clone()
    }

    #[cfg(test)]
    pub(crate) fn tree_cursor_for_test(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(HostId, rho_desk::cells::Id, usize)> {
        self.dashboard.tree_node_cursor_offset(cx)
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
            .tree_source(host)
            .into_iter()
            .flat_map(|(nodes, buffers)| {
                nodes.into_iter().filter_map(move |node| {
                    let text = buffers.get(&node.id)?.read(cx).text();
                    Some((node.id, node.parent, text))
                })
            })
            .collect()
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
    pub(crate) fn sync_dashboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_dashboard(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn dashboard_preview_agent(&self) -> Option<AgentId> {
        self.dashboard_preview
    }

    #[cfg(test)]
    pub(crate) fn dashboard_has_new_draft_for_test(&self) -> bool {
        self.dashboard.has_new_draft_for_test()
    }

    #[cfg(test)]
    pub(crate) fn draft_area_for_test(&self) -> Option<(HostId, rho_desk::cells::Id)> {
        self.draft_area.clone()
    }

    #[cfg(test)]
    pub(crate) fn has_transient_for_test(&self) -> bool {
        self.transient.is_some()
    }

    /// The reconnect loop marks test hosts disconnected (their sockets
    /// don't exist); verbs gated on connectivity need this to run.
    #[cfg(test)]
    pub(crate) fn force_host_online(&mut self, host: HostId) {
        self.hosts
            .set_status(host, crate::hosts::HostStatus::Online);
    }

    #[cfg(test)]
    pub(crate) fn take_host_messages_for_test(&self, host: HostId) -> Vec<ClientMessage> {
        self.hosts
            .connection(host)
            .map(Connection::take_sent_for_test)
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn dashboard_deal_mode_for_test(&self) -> bool {
        self.dashboard.deal_mode()
    }

    #[cfg(test)]
    pub(crate) fn merged_quota_summaries_for_test(&self) -> Vec<rho_ui_proto::QuotaSummary> {
        self.merged_quota_summaries()
    }

    #[cfg(test)]
    pub(crate) fn configure_surface_history_for_test(
        &mut self,
        names: &[&str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.active_editor(cx);
        // A surface is named after what it is about, so the test history is
        // built from named conversations.
        self.surface_history = names
            .iter()
            .rev()
            .map(|name| WarmSurface {
                context: self.active_context,
                surface: Self::wrap_surface(
                    SurfaceKey::ZulipNarrow {
                        label: (*name).to_owned(),
                    },
                    SurfaceView::DeskNode(editor.clone()),
                ),
            })
            .collect();
        self.history_cursor = self.surface_history.len().saturating_sub(1);
        if let Some(current) = self.surface_history.last().cloned() {
            self.active_pane_mut().show(current.surface.clone());
            self.overview_open = false;
            self.focus_active_surface(window, cx);
            cx.notify();
        }
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
        self.active_pane().surface.key.clone()
    }

    #[cfg(test)]
    pub(crate) fn current_surface_name_for_test(&self) -> String {
        self.surface_name(&self.active_pane().surface.key)
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

    #[cfg(test)]
    pub(crate) fn step_surface_forward_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.step_surface_forward(window, cx)
    }

    #[cfg(test)]
    pub(crate) fn surface_history_for_test(&self) -> (Vec<String>, usize) {
        (
            self.surface_history
                .iter()
                .map(|warm| self.surface_name(&warm.surface.key))
                .collect(),
            self.history_cursor,
        )
    }

    #[cfg(test)]
    pub(crate) fn history_contains_agent_for_test(&self, agent_id: AgentId) -> bool {
        self.surface_history
            .iter()
            .any(|warm| warm.surface.key == SurfaceKey::Transcript(agent_id))
    }

    #[cfg(test)]
    pub(crate) fn agent_surface_visible_for_test(&self, agent_id: AgentId) -> bool {
        !self.overview_open && self.active_pane().surface.key == SurfaceKey::Transcript(agent_id)
    }

    #[cfg(test)]
    pub(crate) fn reopen_agent_for_test(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_agent_inner(Some(agent_id), true, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn active_editor_in_deal_mode_for_test(&self, cx: &App) -> bool {
        vim::editor_in_deal_mode(&self.active_editor(cx), cx)
    }

    #[cfg(test)]
    pub(crate) fn append_newer_history_for_test(&mut self, name: &str, cx: &mut Context<Self>) {
        let surface = Self::wrap_surface(
            SurfaceKey::ZulipNarrow {
                label: name.to_owned(),
            },
            SurfaceView::DeskNode(self.active_editor(cx)),
        );
        self.append_history(surface, crate::journal::HistoryAppendMethod::Overview, cx);
        self.history_cursor = self.history_cursor.saturating_sub(1);
    }

    #[cfg(test)]
    pub(crate) fn show_current_history_for_test(
        &mut self,
        method: crate::journal::SurfaceShowMethod,
        cx: &mut Context<Self>,
    ) {
        let surface = self.active_pane().surface.clone();
        self.display_surface_with_method(surface, method, cx);
    }

    #[cfg(test)]
    pub(crate) fn open_history_index_for_test(&mut self, index: usize, cx: &mut Context<Self>) {
        let surface = self.surface_history[index].surface.clone();
        self.display_surface_with_method(surface, crate::journal::SurfaceShowMethod::Open, cx);
    }

    #[cfg(test)]
    pub(crate) fn deal_skip_exists_for_test(
        &self,
        identity: &crate::dashboard::DealCardId,
    ) -> bool {
        self.dashboard.has_skip_for_test(identity)
    }

    #[cfg(test)]
    pub(crate) fn configure_evicted_transcript_history_for_test(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dashboard.end_deal(cx);
        self.end_deal_session();
        self.deal_view = None;
        let transcript = self.make_surface(SurfaceKey::Transcript(agent_id), window, cx);
        self.display_surface_with_method(
            transcript,
            crate::journal::SurfaceShowMethod::Overview,
            cx,
        );
        let draft = self.make_surface(SurfaceKey::Draft, window, cx);
        self.display_surface_with_method(draft, crate::journal::SurfaceShowMethod::Overview, cx);
        self.subscriptions
            .mark_unloaded(agent_id, rho_ui_proto::AgentUnloadReason::Idle);
        self.release_agent_view_cache(agent_id, cx);
    }

    #[cfg(test)]
    pub(crate) fn agent_subscribed_for_test(&self, agent_id: AgentId) -> bool {
        self.subscriptions.contains(agent_id)
    }

    #[cfg(test)]
    pub(crate) fn forget_agent_subscription_for_test(&mut self, agent_id: AgentId) {
        self.subscriptions.forget(agent_id);
    }

    #[cfg(test)]
    pub(crate) fn current_deal_card_for_test(
        &self,
    ) -> Option<(crate::dashboard::DealCardId, crate::dashboard::DealCardKind)> {
        self.dashboard
            .current_deal_card()
            .map(|card| (card.identity.clone(), card.kind))
    }

    #[cfg(test)]
    pub(crate) fn seek_deal_card_for_test(
        &mut self,
        wanted: fn(crate::dashboard::DealCardKind) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        for _ in 0..8 {
            if self
                .dashboard
                .current_deal_card()
                .is_some_and(|card| wanted(card.kind))
            {
                return true;
            }
            self.deal_next(window, cx);
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn rendered_deal_card_for_test(
        &self,
    ) -> Option<(crate::dashboard::DealCardId, crate::dashboard::DealCardKind)> {
        let card = self.dashboard.current_deal_card()?;
        if let Some(view) = &self.deal_view {
            return Some((view.identity(), card.kind));
        }
        match card.kind {
            crate::dashboard::DealCardKind::Desk | crate::dashboard::DealCardKind::Thread => {}
            crate::dashboard::DealCardKind::Agent if card.agent_id.is_some() => {}
            _ => return None,
        }
        Some((card.identity.clone(), card.kind))
    }

    #[cfg(test)]
    pub(crate) fn current_deal_card_value_for_test(&self) -> Option<crate::dashboard::DealCard> {
        self.dashboard.current_deal_card().cloned()
    }

    #[cfg(test)]
    pub(crate) fn reopen_deal_for_test(&mut self, card: crate::dashboard::DealCard) {
        self.dashboard.reopen_deal(card);
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

    #[cfg(test)]
    pub(crate) fn focus_dealt_surface_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let card = self.dashboard.current_deal_card().cloned();
        if let Some(card) = &card {
            match card.kind {
                crate::dashboard::DealCardKind::Agent => {
                    if let Some(agent_id) = card.agent_id {
                        self.select_agent_inner(Some(agent_id), true, window, cx);
                        return;
                    }
                }
                crate::dashboard::DealCardKind::Desk | crate::dashboard::DealCardKind::Thread => {}
            }
        }
        let focus = match self.deal_view.as_ref() {
            Some(DealView::Desk { editor, .. }) => editor.focus_handle(cx),
            Some(DealView::Surface { surface, .. }) => match &surface.view {
                SurfaceView::DeskNode(editor) => editor.focus_handle(cx),
                SurfaceView::Transcript { editor, .. } => editor.focus_handle(cx),
                SurfaceView::Browser(view) => view.read(cx).focus_handle(cx),
                _ => return,
            },
            None => self.dashboard.editor().read(cx).focus_handle(cx),
        };
        window.focus(&focus, cx);
    }

    #[cfg(test)]
    pub(crate) fn dashboard_deal_highlight_for_test(&self, cx: &App) -> bool {
        self.dashboard.deal_highlight_active_for_test(cx)
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
        self.refresh_desk_sources(host, cx);
        if let Some((nodes, buffers)) = self.desk_cells.tree_source(host) {
            self.dashboard.set_tree_source(host, nodes, buffers, cx);
            self.refresh_dashboard(window, cx);
            self.sync_note_views(host, cx);
        }
    }

    /// What the sources say right now, handed to the store's views. None
    /// of it is written: an agent's spawner is the registry's fact and a
    /// thread's conversation is the mirror's, and a copy could only go
    /// stale.
    fn refresh_desk_sources(&mut self, host: HostId, cx: &Context<Self>) {
        let agents = self
            .registry
            .known_agents()
            .copied()
            .filter(|agent| self.registry.host_of_agent(*agent) == Some(host))
            .filter(|agent| !self.registry.agent_hidden(*agent))
            .map(|agent| crate::desk_view::AgentSource {
                agent,
                spawned_by: self.registry.agent_parent(agent),
                workdir: self.registry.working_directory(agent),
                open: self.registry.attention(agent) >= rho_ui_proto::UiAttention::Pending,
            })
            .collect::<Vec<_>>();
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
                })
                .collect()
        } else {
            Vec::new()
        };
        let sources = crate::desk_view::Sources {
            host: self.registry.host_machine_seed(host),
            agents,
            slack,
        };
        self.desk_cells.set_sources(host, sources);
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
        let Some((nodes, buffers)) = self.desk_cells.tree_source(host) else {
            return;
        };
        // A note's title is the first line of its body; a machine row's
        // buffer already holds the title the map derived for it.
        let titles = buffers
            .iter()
            .map(|(id, buffer)| {
                (
                    id.clone(),
                    crate::dashboard::note_title(&buffer.read(cx).text()).to_owned(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
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
        if !self.note_views.contains_key(&(host, node_id.clone())) {
            let view = crate::note_view::NoteView::new(host, node_id.clone(), body, window, cx);
            self.note_views.insert((host, node_id.clone()), view);
        }
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
            .tree_source(host)
            .into_iter()
            .flat_map(|(nodes, _)| nodes)
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
        let SurfaceKey::DeskNode { host, node_id } = self.active_pane().surface.key.clone() else {
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
    fn surface_node(&self) -> Option<(HostId, rho_desk::cells::Id)> {
        let card = match &self.active_pane().surface.key {
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
        let message = self.desk_cells.apply(host, writes, verdict)?;
        let ClientMessage::DeskMutationApply { mutation } = &message else {
            return None;
        };
        let stamp = mutation.stamp;
        // A created note needs its buffer before anything can be typed into
        // it, and the daemon's answer may be a frame away.
        self.desk_cells.reconcile_buffers(host, cx);
        self.sync_tree_dashboard(host, window, cx);
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
                .dashboard
                .current_deal_card()
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
        self.dashboard.sync(&self.registry, &threads, window, cx);
        {
            let pages = self.dashboard.page_ids();
            if pages != self.browser_pages {
                for page in &pages {
                    self.browser_page_gc.remove(page);
                }
                let removed = self
                    .browser_pages
                    .difference(&pages)
                    .copied()
                    .collect::<Vec<_>>();
                self.browser_pages = pages;
                for page in removed {
                    self.schedule_browser_page_gc(page, cx);
                }
                self.scan_browser_pages_for_gc(cx);
            }
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
                            this.browser_page_gc.remove(&page.id);
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
        const GRACE: Duration = Duration::from_secs(10 * 60);
        if self.browser_page_gc.contains_key(&page) || self.browser_page_retained(page) {
            return;
        }
        let gc = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(GRACE).await;
            let _ = this.update(cx, |this, cx| {
                this.browser_page_gc.remove(&page);
                if this.browser_page_retained(page) {
                    return;
                }
                tracing::info!(page_id = %page, "closing unreferenced browser page after grace period");
                if let Some(close) = rho_browser::close_page_if_running(page, cx) {
                    close.detach();
                }
            });
        });
        self.browser_page_gc.insert(page, gc);
    }

    fn browser_page_retained(&self, page: rho_browser::PageId) -> bool {
        self.dashboard.page_ids().contains(&page)
    }

    #[cfg(test)]
    pub(crate) fn is_dashboard_mode(&self, window: &Window, cx: &App) -> bool {
        self.dashboard_mode(window, cx)
    }

    #[cfg(test)]
    pub(crate) fn is_startup_pane(&self) -> bool {
        matches!(self.registry.active_pane(), ActivePane::Startup)
    }

    pub(crate) fn active_agent_model(&self) -> Option<Entity<AgentModel>> {
        self.registry
            .selected_agent()
            .and_then(|agent_id| self.models.get(agent_id))
            .cloned()
    }

    /// The editor the user is typing into. Terminal surfaces have no editor;
    /// the draft's stands in for text-style queries.
    pub(crate) fn active_editor(&self, cx: &gpui::App) -> Entity<editor::Editor> {
        match &self.active_pane().surface.view {
            SurfaceView::Draft { editor, .. } => editor.clone(),
            SurfaceView::Home(view) => view.read(cx).editor().clone(),
            SurfaceView::Messages(editor) => editor.clone(),
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
        match &self.active_pane().surface.view {
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
            && matches!(
                self.active_pane().surface.view,
                SurfaceView::Transcript { .. }
            )
        {
            return self.phone.dashboard_focus.clone();
        }
        match &self.active_pane().surface.view {
            SurfaceView::Draft { editor, .. } => editor.focus_handle(cx),
            SurfaceView::Home(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Messages(editor) => editor.focus_handle(cx),
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
        let agent_id = match &self.active_pane().surface.key {
            SurfaceKey::Transcript(agent_id)
            | SurfaceKey::Shell(agent_id)
            | SurfaceKey::File { agent_id, .. }
            | SurfaceKey::Diff { agent_id }
            | SurfaceKey::Terminal { agent_id, .. } => Some(*agent_id),
            SurfaceKey::Draft
            | SurfaceKey::Home
            | SurfaceKey::Messages
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
            SurfaceKey::Messages => SurfaceView::Messages(self.messages_editor.clone()),
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
        let selected = match self.active_pane().surface.key.clone() {
            SurfaceKey::Transcript(agent_id) | SurfaceKey::Shell(agent_id) => {
                self.registry.select_agent(agent_id);
                Some(agent_id)
            }
            SurfaceKey::Terminal { agent_id, .. } => {
                self.registry.select_agent(agent_id);
                Some(agent_id)
            }
            SurfaceKey::Browser(_) => None,
            SurfaceKey::Diff { agent_id } => {
                self.registry.select_agent(agent_id);
                Some(agent_id)
            }
            SurfaceKey::Draft => {
                self.registry.enter_draft();
                None
            }
            // Files and chat keep whatever agent context was current.
            SurfaceKey::Home
            | SurfaceKey::DeskNode { .. }
            | SurfaceKey::Messages
            | SurfaceKey::File { .. }
            | SurfaceKey::ZulipInbox
            | SurfaceKey::ZulipNarrow { .. } => None,
            SurfaceKey::SlackList | SurfaceKey::SlackConversation(_) => None,
            SurfaceKey::Image { .. } => None,
        };
        if self.connected()
            && let Some(agent_id) = selected
        {
            self.subscribe_agent(agent_id, cx);
        }
        cx.notify();
    }

    /// Recomputes candidates after an edit; subscribed by [`Minibuffer`].
    pub(crate) fn refresh_minibuffer(&mut self, cx: &mut Context<Self>) {
        let Some(mut minibuffer) = self.minibuffer.take() else {
            return;
        };
        minibuffer.refresh(self, cx);
        self.minibuffer = Some(minibuffer);
        cx.notify();
    }

    fn minibuffer_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut minibuffer) = self.minibuffer.take() else {
            return;
        };
        let prompt = minibuffer.prompt().to_owned();
        self.pending_filing_selected = None;
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
        crate::journal::record(crate::journal::Event::MinibufferSubmitted {
            prompt,
            input: input.clone(),
        });
        self.finish_overlay_focus(window, cx);
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
            crate::journal::record(crate::journal::Event::MinibufferCancelled {
                prompt: minibuffer.prompt().to_owned(),
                input: minibuffer.input(cx),
            });
            self.finish_overlay_focus(window, cx);
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
    /// transient items drop into for values.
    pub(crate) fn open_prompt(
        &mut self,
        prompt: impl Into<gpui::SharedString>,
        complete: crate::minibuffer::CandidateSource,
        on_submit: crate::minibuffer::SubmitHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = prompt.into();
        crate::journal::record(crate::journal::Event::MinibufferOpened {
            prompt: prompt.to_string(),
        });
        self.capture_overlay_focus(window, cx);
        let text_style = self
            .active_editor(cx)
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let mut minibuffer = Minibuffer::open(prompt, &text_style, complete, on_submit, window, cx);
        minibuffer.refresh(self, cx);
        self.minibuffer = Some(minibuffer);
        self.drop_transient();
        // The strip is single-occupancy; a stale message reappearing after
        // the prompt closes would be confusing.
        self.echo = None;
        cx.notify();
    }

    pub(crate) fn open_transient(
        &mut self,
        mut transient: crate::transient::Transient,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_overlay_focus(window, cx);
        let subject = self.subject(window, cx);
        transient.retain_applicable(&subject);
        self.transient = Some(transient);
        self.minibuffer = None;
        self.echo = None;
        window.focus(&self.transient_focus, cx);
        cx.notify();
    }

    fn has_modal_overlay(&self) -> bool {
        self.minibuffer.is_some() || self.transient.is_some() || self.pending_git_approval.is_some()
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

    /// Clears the menu without touching focus.
    fn drop_transient(&mut self) {
        self.transient = None;
        self.transient_stack.clear();
    }

    fn close_transient(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.transient.is_some() {
            self.drop_transient();
            self.finish_overlay_focus(window, cx);
            cx.notify();
        }
    }

    /// Keyboard dispatch while a transient is open: a bound key runs its
    /// action (toggles keep the menu up, submenus stack their parent),
    /// escape pops one level; unbound keys leave the menu open.
    fn transient_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // Bare modifiers arrive as key events too; holding shift for an
        // uppercase key must not dismiss the menu.
        if matches!(
            keystroke.key.as_str(),
            "shift" | "control" | "alt" | "platform" | "function"
        ) {
            return;
        }
        let Some(transient) = &self.transient else {
            return;
        };
        if keystroke.key == "escape" {
            match self.transient_stack.pop() {
                Some(parent) => {
                    self.transient = Some(parent);
                    cx.notify();
                }
                None => self.close_transient(window, cx),
            }
            cx.stop_propagation();
            return;
        }
        match transient.action_for(keystroke) {
            Some((run, stay)) if stay => {
                run(self, window, cx);
                cx.notify();
            }
            Some((run, _)) => {
                let parent = self.transient.take();
                // Restore focus to the chord's origin first so the action
                // sees normal focus (and a dashboard chord stays home);
                // submenus and prompts re-take the strip themselves.
                self.restore_overlay_focus(window, cx);
                run(self, window, cx);
                if self.transient.is_some() {
                    // The action opened a submenu: its parent waits under
                    // it for escape.
                    self.transient_stack.extend(parent);
                } else {
                    self.transient_stack.clear();
                    if !self.has_modal_overlay() {
                        self.overlay_return_focus = None;
                    }
                }
                cx.notify();
            }
            None => {}
        }
        cx.stop_propagation();
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
        self.open_prompt("project path [name]:", complete, on_submit, window, cx);
    }

    /// Prompt (completing over registered projects) for one to remove.
    pub(crate) fn prompt_project_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_lowercase();
            workspace
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

    /// `space r`: focus jumps directly to the dashboard.
    pub(crate) fn focus_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overview(window, cx);
    }

    pub(crate) fn cmd_surface_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step_surface_back(window, cx);
    }

    pub(crate) fn cmd_surface_forward_or_deal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.step_surface_forward(window, cx) {
            self.deal_next(window, cx);
        }
    }

    pub(crate) fn cmd_close_and_deal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_current_surface(window, cx);
        self.deal_next(window, cx);
    }

    pub(crate) fn cmd_toggle_raw_desk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dashboard.toggle_raw_mode(cx);
        crate::journal::record(crate::journal::Event::DeskRawModeToggled {
            enabled: self.dashboard.raw_mode(),
        });
        self.refresh_dashboard(window, cx);
    }

    pub(crate) fn open_deal_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.deal_next(window, cx);
        cx.notify();
    }

    fn end_deal_session(&mut self) {
        if std::mem::take(&mut self.deal_session_open) {
            crate::journal::record(crate::journal::Event::DealMode {
                action: crate::journal::DealModeAction::Exit,
                card: None,
            });
        }
    }

    fn mark_deal_interacted(&mut self) {
        if !self.dashboard.deal_mode() || self.deal_current_interacted {
            return;
        }
        self.deal_current_interacted = true;
        crate::journal::record(crate::journal::Event::DealMode {
            action: crate::journal::DealModeAction::Interacted,
            card: self
                .dashboard
                .current_deal_card()
                .map(|card| Self::journal_card_identity(&card.identity)),
        });
    }

    fn present_current_deal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(card) = self.dashboard.current_deal_card().cloned() else {
            return false;
        };
        self.deal_current_interacted = false;
        self.deal_view = None;
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
                crate::journal::record(crate::journal::Event::AgentOpened {
                    agent_id: agent_id.into(),
                });
                self.registry.select_agent(agent_id);
                self.active_context = self.context_for_agent(agent_id);
                self.hosts
                    .focus_agent(self.host_of(agent_id).map(|host| (host, agent_id)));
                self.make_surface(SurfaceKey::Transcript(agent_id), window, cx)
            }
            // A thread is a conversation: the deal view is the conversation
            // surface itself, opened the way `enter` opens it, with the
            // message that raised the card on screen.
            crate::dashboard::CardTarget::Thread(unit) => {
                if self.open_slack_deal(&unit, window, cx) {
                    self.deal_view = Some(DealView::Surface {
                        identity: card.identity,
                        surface: self.active_pane().surface.clone(),
                    });
                    self.finish_presenting_deal(window, cx);
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
                        self.observe_browser_metadata(&model, cx);
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
        self.deal_view = Some(DealView::Surface {
            identity: card.identity,
            surface: surface.clone(),
        });
        self.display_surface_with_method(surface, crate::journal::SurfaceShowMethod::Deal, cx);
        if self.phone.enabled {
            window.focus(&self.phone.dashboard_focus, cx);
        } else {
            self.focus_active_surface(window, cx);
        }
        self.finish_presenting_deal(window, cx);
        true
    }

    /// The editor the current deal is being read in, when the dealt surface
    /// has one. A browser page and a picture do not.
    fn deal_editor(&self, cx: &App) -> Option<Entity<editor::Editor>> {
        Self::surface_editor(&self.active_pane().surface.view, cx)
    }

    /// The editor a surface is read in, if it has one. Deal mode is entered
    /// and left on this editor directly, so every surface kind that can be
    /// dealt must be listed here or its keyboard never enters DEAL.
    fn surface_editor(view: &SurfaceView, cx: &App) -> Option<Entity<editor::Editor>> {
        match view {
            SurfaceView::DeskNode(editor) => Some(editor.clone()),
            SurfaceView::Transcript { editor, .. } => Some(editor.clone()),
            SurfaceView::SlackConversation(view) => Some(view.read(cx).editor().clone()),
            _ => None,
        }
    }

    /// The one way out of deal mode. Vim's Deal refuses ordinary mode
    /// switches and the dashboard keeps deal state of its own, so both are
    /// ended here: anywhere else they can disagree and the reader is left in
    /// a mode nothing will take them out of. The surface stays where it is.
    fn leave_deal_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.deal_view = None;
        self.deal_controls_visible = false;
        self.deal_current_interacted = false;
        self.end_deal_session();
        match self.deal_editor(cx) {
            // The dealt editor is not always what holds focus by now, and
            // Deal mode ignores anything but a direct ask.
            Some(editor) => {
                vim::exit_deal_mode(&editor, window, cx);
            }
            // The phone has no vim to leave; the dealer's own state is all
            // there is to end there.
            None if self.phone.enabled => {}
            None => {
                if let Ok(action) = cx.build_action("vim::ExitDealMode", None) {
                    window.dispatch_action(action, cx);
                }
            }
        }
        cx.notify();
    }

    /// `escape` on a dealt surface: the deal is over, the surface stays, and
    /// the keyboard is back to normal. Recorded as an open, which is what
    /// looking at something and moving on is.
    fn deal_leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dashboard.deal_mode() {
            cx.propagate();
            return;
        }
        // Escape out of insert inside a deal belongs to the editor, which
        // returns to DEAL. Only escape in DEAL itself leaves.
        if self
            .deal_editor(cx)
            .is_some_and(|editor| !vim::editor_in_deal_mode(&editor, cx))
        {
            cx.propagate();
            return;
        }
        self.dashboard.record_deal_verdict_as(
            crate::dashboard::DealerVerdict::Open,
            chrono::Local::now().fixed_offset(),
        );
        self.dashboard.end_deal(cx);
        self.leave_deal_mode(window, cx);
        self.refresh_dashboard(window, cx);
    }

    fn finish_presenting_deal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::journal::record(crate::journal::Event::DealMode {
            action: crate::journal::DealModeAction::Enter,
            card: self
                .dashboard
                .current_deal_card()
                .map(|card| Self::journal_card_identity(&card.identity)),
        });
        if self.phone.enabled {
            self.deal_focus_pending = false;
            cx.notify();
            return;
        }
        let editor = self.deal_editor(cx);
        if let Some(editor) = editor {
            // Surface promotion happens before the new editor is mounted in
            // GPUI's action dispatch tree, so enter the owned Vim instance
            // directly instead of dispatching EnterDealMode to the old focus.
            vim::enter_deal_mode(&editor, window, cx);
        } else if let Ok(action) = cx.build_action("vim::EnterDealMode", None) {
            window.dispatch_action(action, cx);
        }
        self.deal_focus_pending = true;
        cx.notify();
    }

    fn deal_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phone.enabled
            && (self.phone_snap_in_progress() || self.phone_current_deal_has_pending_tree_verdict())
        {
            return;
        }
        if self.dashboard.deal_mode() {
            self.skip_and_end_deal(window, cx);
        }
        let exclude = self.displayed_deal_identity();
        let now = chrono::Local::now().fixed_offset();
        let threads = self.slack_thread_facts(cx);
        if self
            .dashboard
            .pull_deal(
                &self.registry,
                &threads,
                now,
                exclude.as_ref(),
                &self.agent_last_interaction,
                cx,
            )
            .is_some()
        {
            self.deal_session_open = true;
            self.present_current_deal(window, cx);
            self.refresh_dashboard(window, cx);
        } else {
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
        }
    }

    fn displayed_deal_identity(&self) -> Option<crate::dashboard::DealCardId> {
        match &self.active_pane().surface.key {
            SurfaceKey::Transcript(agent_id) => self.dashboard.agent_card_id(*agent_id),
            _ => self
                .dashboard
                .current_deal_card()
                .map(|card| card.identity.clone()),
        }
    }

    fn skip_and_end_deal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(card) = self.dashboard.current_deal_card().cloned() {
            self.dashboard
                .skip_card(card.identity, chrono::Local::now().fixed_offset(), cx);
        }
        self.dashboard.end_deal(cx);
        self.leave_deal_mode(window, cx);
    }

    fn finish_dashboard_deal_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.deal_view = None;
        self.deal_controls_visible = false;
        if self.dashboard.deal_mode() {
            self.present_current_deal(window, cx);
        } else {
            self.leave_deal_mode(window, cx);
        }
        self.refresh_dashboard(window, cx);
    }

    /// `f` on a deal: choose the note the card's node moves under. Filing is
    /// a verdict like any other, so it goes through the tree's verdict log.
    pub(crate) fn prompt_file_deal_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard.current_deal_card().is_none() {
            return;
        }
        self.pending_filing_destinations = self.dashboard.heading_destination_candidates(cx);
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
            std::rc::Rc::new(|workspace, heading, window, cx| {
                let Some(parent) = workspace
                    .pending_filing_destinations
                    .iter()
                    .find(|(value, ..)| *value == heading)
                    .map(|(_, _, _, node_id)| node_id.clone())
                else {
                    workspace.notice_on(None, "file: choose a heading", StyleClass::SystemInfo, cx);
                    return;
                };
                if !workspace.submit_tree_verdict(
                    None,
                    crate::desk_view::DeskVerdict::File { parent },
                    crate::dashboard::DealerVerdict::File,
                    format!("file under {heading}"),
                    window,
                    cx,
                ) {
                    workspace.echo(
                        "file: the deal card disappeared",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    fn finish_deal_verdict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dashboard.end_deal(cx);
        // Exit the dealt editor while it is still focused. GPUI captures the
        // focus target when dispatching, so doing this after close would send
        // ExitDealMode to the replacement surface instead.
        self.finish_dashboard_deal_action(window, cx);
        self.close_current_surface(window, cx);
        self.cmd_surface_forward_or_deal(window, cx);
    }

    fn journal_dealer_verdict(
        verdict: crate::dashboard::DealerVerdict,
    ) -> crate::journal::DealerVerdict {
        match verdict {
            crate::dashboard::DealerVerdict::Skip => crate::journal::DealerVerdict::Skip,
            crate::dashboard::DealerVerdict::Done => crate::journal::DealerVerdict::Done,
            crate::dashboard::DealerVerdict::Mute => crate::journal::DealerVerdict::Mute,
            crate::dashboard::DealerVerdict::Defer => crate::journal::DealerVerdict::Defer,
            crate::dashboard::DealerVerdict::Open => crate::journal::DealerVerdict::Open,
            crate::dashboard::DealerVerdict::File => crate::journal::DealerVerdict::File,
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
        crate::journal::record(crate::journal::Event::VerdictUndone {
            card: Self::journal_card_identity(&card.identity),
            verdict: Self::journal_dealer_verdict(verdict),
        });
        self.echo(
            &format!("undid {verb}: {}", card.breadcrumb),
            StyleClass::SystemInfo,
            cx,
        );
        self.dashboard.reopen_deal(card);
        self.deal_session_open = true;
        self.present_current_deal(window, cx);
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
        nodes: Vec<rho_desk::cells::Id>,
        verb: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let mut applied = Vec::new();
        for node in nodes {
            let Some((writes, event)) =
                self.desk_cells
                    .verdict_writes(host, &node, crate::desk_view::DeskVerdict::Done)
            else {
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
        crate::journal::record(crate::journal::Event::SlackMarkReadBeforeUndone { cards: undone });
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
            && (self.phone_snap_in_progress() || self.phone_current_deal_has_pending_tree_verdict())
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
            && (self.phone_snap_in_progress() || self.phone_current_deal_has_pending_tree_verdict())
        {
            return false;
        }
        let Some(card) = self.dashboard.current_deal_card().cloned() else {
            return false;
        };
        let Some(event) = self
            .dashboard
            .prepare_deal_verdict(verdict, chrono::Local::now().fixed_offset())
        else {
            return false;
        };
        // The verdict lands on the card's own node: an agent card marks the
        // agent, a thread card marks the thread. `target_node` is the room
        // snooze, which deliberately files one level up.
        let node_id = target_node
            .clone()
            .unwrap_or_else(|| card.identity.node_id.clone());
        let phone_verdict = self.phone.enabled.then(|| match dealt {
            crate::desk_view::DeskVerdict::Done => crate::journal::PhoneVerdict::Done,
            crate::desk_view::DeskVerdict::Mute => crate::journal::PhoneVerdict::Mute,
            crate::desk_view::DeskVerdict::Defer { .. } => crate::journal::PhoneVerdict::Defer,
            crate::desk_view::DeskVerdict::Todo { .. } => crate::journal::PhoneVerdict::Todo,
            crate::desk_view::DeskVerdict::File { .. } => crate::journal::PhoneVerdict::File,
        });
        // `x` on a thread card is Slack's ignore thread: the same keystroke
        // that closes the card here stops Slack raising it anywhere else.
        if matches!(dealt, crate::desk_view::DeskVerdict::Mute)
            && target_node.is_none()
            && let Some(thread) = self.dashboard.card_thread(card.identity.clone())
        {
            self.slack_ignore_thread(&thread, cx);
        }
        let Some((writes, applied)) = self.desk_cells.verdict_writes(card.host, &node_id, dealt)
        else {
            return false;
        };
        let echo = format!("{verb}: {}", card.breadcrumb);
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
                card: card.clone(),
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

    fn defer_deal_edit(
        &mut self,
        action_name: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.defer_in(window, move |_, window, cx| {
            if let Ok(action) = cx.build_action("vim::EnterDealMode", None) {
                window.dispatch_action(action, cx);
            }
            cx.defer_in(window, move |_, window, cx| {
                if let Ok(action) = cx.build_action(action_name, None) {
                    window.dispatch_action(action, cx);
                }
            });
        });
    }

    fn focus_taken_agent_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let SurfaceView::Transcript { model, editor } = &self.active_pane().surface.view else {
            return;
        };
        let model = model.clone();
        let editor = editor.clone();
        model.update(cx, |model, cx| model.focus_prompt(&editor, window, cx));
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
            .map(|workdir| self.workdir_label(workdir))
            .unwrap_or_default();
        self.select_agent_inner(None, true, window, cx);
        self.draft_area = area;
        let editor = self.focused_draft_editor();
        self.draft_model.update(cx, |view, cx| {
            view.set_body_text("", cx);
            view.clear_attachments(cx);
            view.set_role_text(crate::draft_view::DEFAULT_ROLE, cx);
            view.set_start_text(crate::draft_view::DEFAULT_START, cx);
            view.seed(&label, true, editor.as_ref(), window, cx);
        });
    }

    /// The workdir to fall back on when nothing else names one: a single
    /// registered project is not a choice worth asking about.
    fn only_workdir(&self) -> Option<HostPath> {
        match self.workdirs.as_slice() {
            [workdir] => Some(HostPath {
                host: workdir.host,
                path: workdir.project.path.clone(),
            }),
            _ => None,
        }
    }

    /// The workdir a new thing under an area inherits: the area's own
    /// file, the nearest ancestor with one, then the agent that owns the
    /// area (or the area itself when it is an agent node). The caller
    /// falls back to the host's only workdir.
    fn area_workdir(&self, host: HostId, node_id: rho_desk::cells::Id) -> Option<HostPath> {
        if let Some(path) = self.desk_cells.inherited_file_path(host, &node_id) {
            return Some(HostPath { host, path });
        }
        let agent_id = self.desk_cells.nearest_agent(host, &node_id)?;
        self.agent_workdir(agent_id)
    }

    pub(crate) fn open_usage_transient(
        &mut self,
        days: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.quota_history_days = days;
        self.hosts.broadcast(|| ClientMessage::QuotaHistory);
        let history = self.merged_quota_history();
        self.open_transient(
            crate::transient::usage_menu(history, self.active_quota_namespaces(), days),
            window,
            cx,
        );
    }

    pub(crate) fn open_global_usage_transient(
        &mut self,
        days: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.global_usage_days = days;
        self.hosts.broadcast(|| ClientMessage::GlobalUsage {
            since_ms: now_ms().saturating_sub(days * 24 * 60 * 60 * 1_000),
        });
        self.open_transient(
            crate::transient::global_usage_menu(self.merged_global_usage(), days),
            window,
            cx,
        );
    }

    pub(crate) fn open_usage_share_transient(
        &mut self,
        days: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.global_usage_days = days;
        let ema_warmup_days = if days <= 7 { 4 } else { 14 };
        self.hosts.broadcast(|| ClientMessage::GlobalUsage {
            // Seed the EMA with seven half-lives before the visible range so
            // its left edge represents actual prior usage rather than a reset.
            since_ms: now_ms().saturating_sub((days + ema_warmup_days) * 24 * 60 * 60 * 1_000),
        });
        self.open_transient(
            crate::transient::usage_share_menu(self.merged_global_usage(), days),
            window,
            cx,
        );
    }

    pub(crate) fn open_agent_cost_transient(
        &mut self,
        days: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
        self.agent_cost_days = days;
        let ema_warmup_days = if days <= 7 { 4 } else { 14 };
        self.hosts
            .broadcast(|| ClientMessage::AgentCostDistribution {
                since_ms: now_ms().saturating_sub((days + ema_warmup_days) * DAY_MS),
            });
        self.open_transient(
            crate::transient::agent_cost_menu(self.merged_agent_cost_usage(), days),
            window,
            cx,
        );
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
        let role = parse_agent_role(crate::draft_view::DEFAULT_ROLE)?;
        Ok((
            workdir.host,
            rho_ui_proto::StartMode::NewOn {
                repo: workdir.path,
                revset: crate::draft_view::AUTO_BASE_REVSET.to_owned(),
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
        if let Ok(action) = cx.build_action("vim::InsertBefore", None) {
            window.dispatch_action(action, cx);
        }
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
    fn dashboard_insert_heading(
        &mut self,
        above: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  _window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let title = input.trim();
                if !title.is_empty() {
                    if let Some((host, relative)) = workspace.dashboard.tree_node_at_cursor(cx) {
                        workspace
                            .append_tree_heading(host, relative, false, above, title, _window, cx);
                    }
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
        _above: bool,
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
                crate::journal::record(crate::journal::Event::Find {
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

    fn prompt_dashboard_search(
        &mut self,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let query = input.trim();
                if query.is_empty() {
                    return;
                }
                let editor = workspace.dashboard.editor().clone();
                let text = editor.read(cx).text(cx);
                let found = if backwards {
                    text.rfind(query)
                } else {
                    text.find(query)
                };
                if let Some(start) = found {
                    editor.update(cx, |editor, cx| {
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_ranges([editor::MultiBufferOffset(start)
                                ..editor::MultiBufferOffset(start + query.len())]);
                        });
                    });
                    window.focus(&editor.read(cx).focus_handle(cx), cx);
                } else {
                    workspace.notice_on(None, "search: no match", StyleClass::SystemInfo, cx);
                }
            },
        );
        self.open_prompt(
            if backwards {
                "search backward:"
            } else {
                "search:"
            },
            std::rc::Rc::new(|_, _, _| Vec::new()),
            on_submit,
            window,
            cx,
        );
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

    /// The Iris document or selected agent preview editor.
    fn selected_preview_editor(
        &mut self,
        iris: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<editor::Editor>> {
        if iris {
            let Some(agent_id) = self.current_iris_agent() else {
                return Some(self.iris_preview.clone());
            };
            if self.agent_online(agent_id) && !self.subscriptions.contains(agent_id) {
                self.subscribe_agent(agent_id, cx);
            }
            let model = self.materialize_model(&agent_id, window, cx);
            self.hosts
                .focus_agent(self.host_of(agent_id).map(|host| (host, agent_id)));
            return Some(model.update(cx, |model, cx| model.preview_editor(window, cx)));
        }
        let agent_id = self.dashboard_preview?;
        let model = self.models.get(&agent_id)?.clone();
        Some(model.update(cx, |model, cx| model.preview_editor(window, cx)))
    }

    fn selected_preview(
        &mut self,
        iris: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if let Some((_, view)) = &self.dashboard_web_preview {
            return Some(
                div()
                    .size_full()
                    .overflow_hidden()
                    .child(view.clone())
                    .into_any_element(),
            );
        }
        self.selected_preview_editor(iris, window, cx)
            .map(|editor| {
                div()
                    .size_full()
                    .overflow_hidden()
                    .child(editor)
                    .into_any_element()
            })
    }

    fn deal_body(
        &mut self,
        card: &crate::dashboard::DealCard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let created = self
            .deal_view
            .as_ref()
            .is_none_or(|view| !view.matches(&card.identity));
        if created {
            let view = if matches!(card.kind, crate::dashboard::DealCardKind::Desk) {
                Some(DealView::Desk {
                    identity: card.identity.clone(),
                    editor: self.dashboard.editor().clone(),
                })
            } else if matches!(card.kind, crate::dashboard::DealCardKind::Agent) {
                card.agent_id.map(|agent_id| {
                    let surface = self.make_surface(SurfaceKey::Transcript(agent_id), window, cx);
                    DealView::Surface {
                        identity: card.identity.clone(),
                        surface,
                    }
                })
            } else {
                // A thread deals as its conversation surface, which the deal
                // presenter opens; nothing to build here.
                None
            };
            self.deal_view = view;
        }

        if created && !self.phone.enabled {
            let focus = match self.deal_view.as_ref() {
                Some(DealView::Desk { editor, .. }) => Some(editor.focus_handle(cx)),
                Some(DealView::Surface { surface, .. }) => {
                    Self::surface_editor(&surface.view, cx).map(|editor| editor.focus_handle(cx))
                }
                None => None,
            };
            if let Some(focus) = focus {
                let identity = card.identity.clone();
                cx.defer_in(window, move |this, window, cx| {
                    if !this.dashboard.deal_mode()
                        || this
                            .dashboard
                            .current_deal_card()
                            .is_none_or(|card| card.identity != identity)
                    {
                        return;
                    }
                    window.focus(&focus, cx);
                    cx.defer_in(window, |this, window, cx| {
                        if !this.dashboard.deal_mode() {
                            return;
                        }
                        if let Ok(action) = cx.build_action("vim::EnterDealMode", None) {
                            window.dispatch_action(action, cx);
                        }
                    });
                });
            }
        }

        if std::mem::take(&mut self.deal_focus_pending) && !self.phone.enabled {
            cx.defer_in(window, |this, window, cx| {
                if !this.dashboard.deal_mode() {
                    return;
                }
                this.focus_active_surface(window, cx);
                if let Some(editor) = this.deal_editor(cx) {
                    vim::enter_deal_mode(&editor, window, cx);
                } else if let Ok(action) = cx.build_action("vim::EnterDealMode", None) {
                    window.dispatch_action(action, cx);
                }
            });
        }

        match self.deal_view.as_ref() {
            Some(DealView::Desk { editor, .. }) => div()
                .size_full()
                .key_context("RhoDashboard")
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            Some(DealView::Surface { surface, .. }) => self.render_surface(surface),
            None => div().size_full().into_any_element(),
        }
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
                "deal" => "DEAL",
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
        let mode_color = if self.dashboard.deal_mode() {
            colors.terminal_ansi_bright_magenta
        } else if mode.contains("insert") {
            status.warning
        } else {
            colors.terminal_ansi_bright_cyan
        };
        let quota = self.merged_quota_summaries();
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
            .children(
                self.abnormal_connection_text()
                    .map(|connection| div().text_color(status.error).child(connection)),
            )
            .children(self.lamp_on.then(|| {
                div()
                    .flex_none()
                    .size(px(7.))
                    .rounded_full()
                    .bg(status.error)
            }))
            .child(
                div()
                    .text_color(mode_color)
                    .child(if self.dashboard.deal_mode() {
                        "DEAL".to_owned()
                    } else {
                        mode_word(&mode)
                    }),
            )
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
        if self.dashboard.deal_mode()
            && let Some(card) = self.dashboard.current_deal_card()
        {
            return self.render_deal_why(card, text_style, window, cx);
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
            match &self.active_pane().surface.key {
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
        let left = self.echo.as_ref().map_or_else(
            || self.render_status_path(&Self::truncate_outline_path(&path), cx),
            |echo| {
                div()
                    .text_color(cx.theme().status().info)
                    .child(echo.text().to_owned())
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
            div().child(left).children(unseen),
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
        if self.dashboard.deal_mode()
            && let Some(card) = self.dashboard.current_deal_card().cloned()
        {
            return div()
                .size_full()
                .key_context("RhoGuiDeal")
                .child(self.deal_body(&card, window, cx))
                .into_any_element();
        }
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
                self.active_pane().surface.view.telemetry_kind()
            };
            let visible_surfaces = focused_surface.bit();
            crate::telemetry::record_surfaces(focused_surface, visible_surfaces);
        }
        let iris = false;
        self.sync_diff_visibility(!home, cx);
        let web_preview_visible = self.dashboard_web_preview.is_some();
        let show_surface = !home || iris || self.dashboard_preview.is_some() || web_preview_visible;
        let rail = home.then(|| self.render_rail(show_surface, text_style, cx));
        // Same hairline the rail uses against the preview.
        let separator_color = cx.theme().colors().border_variant.opacity(0.6);
        let mut preview_text_style = text_style.clone();
        preview_text_style.font_size =
            (text_style.font_size.to_pixels(window.rem_size()) * 0.85).into();
        preview_text_style.line_height =
            (text_style.line_height_in_pixels(window.rem_size()) * 0.85).into();
        let preview = home
            .then(|| self.selected_preview(iris, window, cx))
            .flatten();
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
                    .child(self.render_surface(&self.active_pane().surface))
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
            .dashboard_web_preview
            .as_ref()
            .is_some_and(|(_, view)| view.read(cx).focus_handle(cx).is_focused(window));
        self.overview_open
            || self.overlay_return_focus.as_ref() == Some(&dashboard)
            || browser_preview_focused
    }

    /// Hidden surfaces stay alive as editor buffers, but they must not turn
    /// worktree events into jj manifest traffic. Only the visible diff may
    /// refresh.
    fn sync_diff_visibility(&self, surface_visible: bool, cx: &mut Context<Self>) {
        let visible = if surface_visible {
            match &self.active_pane().surface.view {
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
            SurfaceView::DeskNode(editor) => div()
                .id("rho-surface-note")
                .key_context("RhoNote")
                .size_full()
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            SurfaceView::Transcript { editor, .. } => div()
                .id("rho-surface-transcript")
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

fn parse_agent_role(text: &str) -> Result<AgentRole, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "" | "eng" => Ok(AgentRole::default()),
        "eng-mini" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        }),
        "eng-low" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
        }),
        "eng-cheap" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        }),
        "eng-high" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        }),
        "eng-ultra" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
        }),
        "eng-alt" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Alt,
        }),
        "eng-gemini" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Gemini,
        }),
        "pm" => Ok(AgentRole::pm()),
        other => Err(format!(
            "unknown role `{other}`; use eng, eng-mini, eng-low, eng-cheap, eng-high, eng-ultra, eng-alt, eng-gemini, or pm"
        )),
    }
}

fn cycle_agent_role_text(current: &str) -> &'static str {
    match parse_agent_role(current).unwrap_or_default() {
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Mini,
            ..
        } => "eng-low",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Low,
            ..
        } => "eng-cheap",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Cheap,
            ..
        } => "eng",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Medium,
            ..
        } => "eng-high",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::High,
            ..
        } => "eng-ultra",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Ultra,
            ..
        } => "eng-alt",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Alt,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Alt,
            ..
        } => "eng-gemini",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Gemini,
            ..
        }
        | AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Gemini,
            ..
        } => "pm",
        AgentRole::Advisor { .. } => "eng",
        AgentRole::PM | AgentRole::WorkflowPM { .. } => "eng-mini",
        AgentRole::Iris => "eng-mini",
    }
}

#[cfg(test)]
struct RoleLabel {
    text: String,
    family: RoleFamily,
}

#[cfg(test)]
fn agent_role_label(config: AgentRole) -> RoleLabel {
    match config {
        AgentRole::PM | AgentRole::WorkflowPM { .. } => RoleLabel {
            text: "pm".to_owned(),
            family: RoleFamily::Deep,
        },
        AgentRole::Iris => RoleLabel {
            text: "iris".to_owned(),
            family: RoleFamily::Deep,
        },
        AgentRole::Advisor { intelligence } => RoleLabel {
            text: match intelligence {
                AdvisorIntelligence::Medium => "advisor",
                AdvisorIntelligence::High => "advisor-high",
                AdvisorIntelligence::Cheap => "advisor-cheap",
            }
            .to_owned(),
            family: if intelligence == AdvisorIntelligence::High {
                RoleFamily::Fable
            } else {
                RoleFamily::Deep
            },
        },
        AgentRole::Engineer { intelligence } | AgentRole::WorkflowEngineer { intelligence, .. } => {
            RoleLabel {
                text: match intelligence {
                    EngineerIntelligence::Mini => "eng-mini",
                    EngineerIntelligence::Low => "eng-low",
                    EngineerIntelligence::Cheap => "eng-cheap",
                    EngineerIntelligence::Medium => "eng",
                    EngineerIntelligence::High => "eng-high",
                    EngineerIntelligence::Ultra => "eng-ultra",
                    EngineerIntelligence::Alt => "eng-alt",
                    EngineerIntelligence::Gemini => "eng-gemini",
                }
                .to_owned(),
                family: if matches!(
                    intelligence,
                    EngineerIntelligence::Ultra | EngineerIntelligence::Alt
                ) {
                    RoleFamily::Fable
                } else {
                    RoleFamily::Deep
                },
            }
        }
    }
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
            .on_scroll_wheel(cx.listener(Self::journal_scroll))
            .on_linux_pointer_axis(cx.listener(Self::journal_linux_scroll))
            .on_action(cx.listener(Self::submit_prompt))
            .on_action(cx.listener(Self::paste_prompt))
            .on_action(cx.listener(|this, _: &SurfaceBack, window, cx| {
                this.step_surface_back(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealOpen, window, cx| {
                if !this.step_surface_forward(window, cx) {
                    this.deal_next(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DealCloseAndNext, window, cx| {
                this.close_current_surface(window, cx);
                this.deal_next(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OverviewToggle, window, cx| {
                this.toggle_overview(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SurfaceClose, window, cx| {
                this.close_current_surface(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealLeave, window, cx| {
                this.deal_leave(window, cx);
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
                    this.forget_discarded_draft(cx);
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
                if this.dashboard.deal_mode() {
                    this.close_current_surface(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealNext, window, cx| {
                vim::take_count(cx);
                this.deal_next(window, cx);
            }))
            .on_action(cx.listener(|this, _: &UndoVerdict, window, cx| {
                vim::take_count(cx);
                this.undo_verdict(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealDone, window, cx| {
                vim::take_count(cx);
                if this.dashboard.current_deal_card().is_some() {
                    if !this.submit_tree_verdict(
                        None,
                        crate::desk_view::DeskVerdict::Done,
                        crate::dashboard::DealerVerdict::Done,
                        "done".to_owned(),
                        window,
                        cx,
                    ) {
                        this.echo("done: the note is unavailable", StyleClass::SystemInfo, cx);
                    }
                    return;
                }
                this.echo("done: nothing under the deal", StyleClass::SystemInfo, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealMute, window, cx| {
                vim::take_count(cx);
                if this.dashboard.current_deal_card().is_some() {
                    if !this.submit_tree_verdict(
                        None,
                        crate::desk_view::DeskVerdict::Mute,
                        crate::dashboard::DealerVerdict::Mute,
                        "mute".to_owned(),
                        window,
                        cx,
                    ) {
                        this.echo("mute: the note is unavailable", StyleClass::SystemInfo, cx);
                    }
                    return;
                }
                this.echo("mute: nothing under the deal", StyleClass::SystemInfo, cx);
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
                    let count = vim::take_count(cx).unwrap_or(1) as u32;
                    let today = chrono::Local::now().date_naive();
                    if let Some((_host, room_node)) = this.dashboard.current_tree_room_node() {
                        let days = count.max(1);
                        if !this.submit_tree_verdict(
                            Some(room_node),
                            crate::desk_view::DeskVerdict::Defer {
                                until: crate::desk_view::day_timestamp(
                                    today + chrono::Duration::days(i64::from(days)),
                                ),
                            },
                            crate::dashboard::DealerVerdict::Defer,
                            format!("snooze {days}d"),
                            window,
                            cx,
                        ) {
                            this.echo(
                                "room snooze: the room is unavailable",
                                StyleClass::SystemInfo,
                                cx,
                            );
                        }
                        return;
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &DashboardDealTodo, window, cx| {
                let count = vim::take_count(cx).unwrap_or(7) as u32;
                let today = chrono::Local::now().date_naive();
                if this.dashboard.current_deal_card().is_some() {
                    let days = count.max(1);
                    if !this.submit_tree_verdict(
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
                        this.echo("todo: the note is unavailable", StyleClass::SystemInfo, cx);
                    }
                    return;
                }
                let result: Result<(), &'static str> = Err("the dealt item has no node");
                let handled = result.is_ok();
                if handled {
                    this.dashboard.record_deal_verdict_as(
                        crate::dashboard::DealerVerdict::Done,
                        chrono::Local::now().fixed_offset(),
                    );
                    this.finish_deal_verdict(window, cx);
                }
                if let Err(reason) = result {
                    this.echo(&format!("todo: {reason}"), StyleClass::SystemInfo, cx);
                } else {
                    this.echo(
                        &format!("todo: {}d", count.max(1)),
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealRefresh, window, cx| {
                vim::take_count(cx);
                if !this.dashboard.deal_mode() {
                    cx.propagate();
                    return;
                }
                this.deal_next(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealInsert, window, cx| {
                vim::take_count(cx);
                let deal_agent = this
                    .dashboard
                    .current_deal_card()
                    .and_then(|card| card.agent_id);
                let edits_agent = deal_agent.is_some();
                this.mark_deal_interacted();
                if let Some(agent_id) = deal_agent {
                    this.dashboard.end_deal(cx);
                    this.end_deal_session();
                    this.deal_view = None;
                    this.overview_open = false;
                    this.select_agent_inner(Some(agent_id), true, window, cx);
                    // The transcript surface is still hidden behind the
                    // focused Desk preview until the next render. Blurring
                    // lets that render mount the selected surface; the
                    // deferred focus below can then enter its composer.
                    window.blur();
                    cx.defer_in(window, move |_this, window, cx| {
                        cx.defer_in(window, move |this, window, cx| {
                            let Some(surface) = this
                                .find_surface(|surface| {
                                    surface.key == SurfaceKey::Transcript(agent_id)
                                })
                                .cloned()
                            else {
                                return;
                            };
                            let SurfaceView::Transcript { model, editor } = surface.view else {
                                return;
                            };
                            model.update(cx, |model, cx| model.focus_prompt(&editor, window, cx));
                            vim::enter_deal_insert_mode(&editor, window, cx);
                        });
                    });
                }
                let edits_desk = matches!(
                    this.dashboard.current_deal_card().map(|card| card.kind),
                    Some(crate::dashboard::DealCardKind::Desk)
                );
                if edits_desk {
                    this.dashboard.prepare_taken_deal_edit(cx);
                    this.refresh_dashboard(window, cx);
                }
                if !edits_agent {
                    this.focus_taken_agent_prompt(window, cx);
                }
                if edits_desk {
                    if let Ok(action) = cx.build_action("vim::DealInsert", None) {
                        window.dispatch_action(action, cx);
                    }
                } else if !edits_agent {
                    this.defer_deal_edit("vim::DealInsert", window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealAppend, window, cx| {
                vim::take_count(cx);
                this.mark_deal_interacted();
                if matches!(
                    this.dashboard.current_deal_card().map(|card| card.kind),
                    Some(crate::dashboard::DealCardKind::Desk)
                ) {
                    this.dashboard.prepare_taken_deal_edit(cx);
                    this.refresh_dashboard(window, cx);
                }
                this.focus_taken_agent_prompt(window, cx);
                this.defer_deal_edit("vim::DealAppend", window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealOpenLine, window, cx| {
                vim::take_count(cx);
                this.mark_deal_interacted();
                if matches!(
                    this.dashboard.current_deal_card().map(|card| card.kind),
                    Some(crate::dashboard::DealCardKind::Desk)
                ) {
                    this.dashboard.prepare_taken_deal_edit(cx);
                    this.refresh_dashboard(window, cx);
                }
                this.focus_taken_agent_prompt(window, cx);
                this.defer_deal_edit("vim::DealOpenLine", window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealFile, window, cx| {
                vim::take_count(cx);
                this.prompt_file_deal_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealReply, window, cx| {
                vim::take_count(cx);
                let Some(card) = this.dashboard.current_deal_card().cloned() else {
                    return;
                };
                let target = this.dashboard.card_target(card.identity);
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
                this.dashboard.record_deal_verdict_as(
                    crate::dashboard::DealerVerdict::Open,
                    chrono::Local::now().fixed_offset(),
                );
                if !this.dashboard.end_deal(cx) {
                    return;
                }
                this.leave_deal_mode(window, cx);
                match target {
                    crate::dashboard::CardTarget::Page(page) => {
                        this.open_browser_page(page, window, cx);
                    }
                    crate::dashboard::CardTarget::Thread(unit) if this.phone.enabled => {
                        this.open_slack_source(crate::slack::unit_source(&unit), window, cx);
                        if let SurfaceView::SlackConversation(view) =
                            &this.active_pane().surface.view
                        {
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
                                &this.active_pane().surface.view
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
                this.open_transient(crate::transient::root_menu(), window, cx);
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
            .children(
                match (
                    &self.pending_git_approval,
                    &self.minibuffer,
                    &self.transient,
                    &self.echo,
                ) {
                    (Some(pending), _, _, _) => {
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
                    (None, Some(minibuffer), _, _) => Some(if phone {
                        minibuffer.render_phone(&text_style, cx)
                    } else {
                        minibuffer.render(&text_style, cx)
                    }),
                    (None, None, Some(transient), _) => {
                        if phone {
                            self.render_phone_transient_sheet(&text_style, cx)
                        } else {
                            Some(
                                div()
                                    .track_focus(&self.transient_focus)
                                    .on_key_down(cx.listener(Self::transient_key))
                                    .child(transient.render(&text_style, cx))
                                    .into_any_element(),
                            )
                        }
                    }
                    (None, None, None, Some(_)) => None,
                    (None, None, None, None) => None,
                },
            )
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
    fn parses_agent_role() {
        assert_eq!(
            parse_agent_role("eng-low").unwrap(),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Low,
            }
        );
        assert_eq!(parse_agent_role("pm").unwrap(), AgentRole::pm());
        assert_eq!(
            parse_agent_role("eng-gemini").unwrap(),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Gemini,
            }
        );
        assert!(parse_agent_role("pm ultra").is_err());
        assert!(parse_agent_role("eng-ultra-fast").is_err());
        assert!(parse_agent_role("advisor high").is_err());
    }

    #[test]
    fn labels_agent_role() {
        let label = agent_role_label(AgentRole::pm());
        assert_eq!(label.text, "pm");
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
