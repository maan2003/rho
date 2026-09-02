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

#[cfg(feature = "native")]
#[path = "workspace_phone.rs"]
mod phone;
#[cfg(all(test, feature = "native"))]
pub(crate) use phone::set_touch_modal_editing;
#[cfg(all(target_family = "wasm", not(feature = "native")))]
#[path = "workspace_web.rs"]
mod web;

use std::collections::{HashMap, HashSet};
#[cfg(feature = "native")]
use std::path::PathBuf;
use std::time::Duration;
#[cfg(feature = "native")]
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use camino::Utf8PathBuf;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::prelude::*;
use gpui::{
    App, ClipboardEntry, Context, Entity, Focusable as _, Point, Task, TouchEvent, TouchId,
    TouchPhase, Window, div, px,
};
use rho_core::ContentPart;
#[cfg(test)]
use rho_ui_proto::AdvisorIntelligence;
use rho_ui_proto::{AgentId, AgentRole, ClientMessage, EngineerIntelligence, MessageDelivery};
use settings::Settings as _;
use theme::ActiveTheme as _;

use crate::agent_view::AgentModel;
#[cfg(feature = "native")]
use crate::chime::Chime;
#[cfg(feature = "native")]
use crate::connection::GitApprovalDecision;
use crate::connection::{AgentFrameAllocation, ConnEvent, Connection, HostEvent};
use crate::desk_view::DeskSync;
use crate::draft_view::DraftModel;
use crate::hosts::{HostStatus, Hosts};
use crate::inbox::{
    CapturedContext, InboxDraft, InboxId, InboxKind, InboxStore, SourceReference, Verdict,
};
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
    AgentDone, AgentHide, AgentJumpAttention, AgentNew, AgentNext, AgentPrevious, BrowserExit,
    DashboardArchive, DashboardBack, DashboardCycleGlobal, DashboardDealAppend,
    DashboardDealDiscard, DashboardDealDone, DashboardDealExit, DashboardDealFile,
    DashboardDealInsert, DashboardDealNext, DashboardDealOpenLine, DashboardDealPrevious,
    DashboardDealRefresh, DashboardDealReply, DashboardDealRoomSnooze, DashboardDealSnooze,
    DashboardDealTodo, DashboardDeleteEmpty, DashboardDemote, DashboardGoto, DashboardHeadingAbove,
    DashboardHeadingBelow, DashboardJump, DashboardNewAgent, DashboardNow, DashboardPromote,
    DashboardRenameTopic, DashboardReply, DashboardStaff, DashboardSubmit,
    DashboardToggleAgentTree, DashboardToggleSubagents, DashboardUndo, DealOpen, GitApprovalAllow,
    GitApprovalDeny, InboxCapture, MinibufferCancel, MinibufferComplete, MinibufferConfirm,
    MinibufferNext, MinibufferPrevious, OverviewToggle, PastePrompt, RailFocus, RailOpen,
    RoleCycle, RoleCycleGroup, RoomBack, RoomStripLeft, RoomStripRight, ShellEof, ShellInterrupt,
    ShellPagerAll, ShellPagerMore, ShellPagerQuit, StripRemove, SubmitPrompt, TaskBoard,
    UploadGuiTelemetry, VoiceToggle, ZulipLoadOlder, ZulipNextUnread, ZulipOpenRow, ZulipQuit,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RoomId {
    host: HostId,
    heading_offset: usize,
}

#[derive(Clone)]
struct WarmSurface {
    context: ContextId,
    surface: Surface,
}

struct MruOverlay {
    origin: RoomId,
    room_candidates: Vec<RoomId>,
    room_index: usize,
}

#[derive(Clone)]
struct DealOrigin {
    active_context: ContextId,
    surface: Surface,
    current_room: Option<RoomId>,
    overview_open: bool,
}

const SHELL_SWIPE_DISTANCE: gpui::Pixels = px(64.);

struct ShellTouchContact {
    start: Point<gpui::Pixels>,
    position: Point<gpui::Pixels>,
}

/// Stable surface identity plus its live view.
#[derive(Clone)]
pub struct Surface {
    key: SurfaceKey,
    view: SurfaceView,
}

enum DealView {
    Desk {
        identity: crate::dashboard::DealCardIdentity,
        editor: Entity<editor::Editor>,
    },
    Surface {
        identity: crate::dashboard::DealCardIdentity,
        kind: crate::dashboard::DealCardKind,
        surface: Surface,
    },
    Inbox {
        identity: crate::dashboard::DealCardIdentity,
        kind: crate::dashboard::DealCardKind,
        editor: Entity<editor::Editor>,
    },
}

impl DealView {
    fn matches(&self, identity: &crate::dashboard::DealCardIdentity) -> bool {
        match self {
            Self::Desk {
                identity: current, ..
            }
            | Self::Surface {
                identity: current, ..
            }
            | Self::Inbox {
                identity: current, ..
            } => current == identity,
        }
    }

    #[cfg(test)]
    fn card(
        &self,
    ) -> (
        &crate::dashboard::DealCardIdentity,
        crate::dashboard::DealCardKind,
    ) {
        match self {
            Self::Desk { identity, .. } => (identity, crate::dashboard::DealCardKind::Desk),
            Self::Surface { identity, kind, .. } | Self::Inbox { identity, kind, .. } => {
                (identity, *kind)
            }
        }
    }
}

#[cfg(feature = "native")]
struct PendingGitApproval {
    request_id: u64,
    prompt: String,
    response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
}

#[derive(Clone)]
enum SurfaceView {
    Draft {
        editor: Entity<editor::Editor>,
    },
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
    #[cfg(feature = "native")]
    Browser(Entity<rho_browser::PageView>),
    #[cfg(feature = "native")]
    ZulipInbox(Entity<rho_zulip::ui::InboxView>),
    #[cfg(feature = "native")]
    ZulipNarrow(Entity<rho_zulip::ui::NarrowView>),
}

impl SurfaceView {
    #[cfg(feature = "native")]
    fn telemetry_kind(&self) -> crate::telemetry::SurfaceKind {
        use crate::telemetry::SurfaceKind;
        match self {
            Self::Draft { .. } => SurfaceKind::Draft,
            Self::Transcript { .. } => SurfaceKind::Transcript,
            Self::File(_) => SurfaceKind::File,
            Self::Shell { .. } => SurfaceKind::Shell,
            Self::Diff(_) => SurfaceKind::Diff,
            Self::Terminal(_) => SurfaceKind::Terminal,
            #[cfg(feature = "native")]
            Self::Browser(_) => SurfaceKind::Browser,
            #[cfg(feature = "native")]
            Self::ZulipInbox(_) => SurfaceKind::ZulipInbox,
            #[cfg(feature = "native")]
            Self::ZulipNarrow(_) => SurfaceKind::ZulipNarrow,
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
enum ContextId {
    Draft,
    Agent(AgentId),
    /// Zulip's own window arrangement: entering it from the dashboard
    /// leaves the agent surface exactly as it was, and leaving it comes
    /// back to them.
    Zulip,
}

/// How to reach the daemon. Deliberately holds no client-local paths: the
/// socket may be forwarded from another machine, so the GUI's own cwd and
/// home mean nothing to the daemon and must never leak into agent working
/// directories.
#[cfg(feature = "native")]
#[derive(Clone)]
pub enum AttachTarget {
    Unix(PathBuf),
    Iroh {
        endpoint_id: iroh::EndpointId,
        ssh_destination: String,
        remote_rho: String,
    },
}

#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
#[derive(Clone)]
pub struct HostSpec {
    pub name: String,
    pub target: AttachTarget,
}

#[cfg(feature = "native")]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeskProjectResolution {
    Use(usize),
    Choose,
    Missing,
}

fn resolve_desk_project(property: Option<&str>, projects: &[HostProject]) -> DeskProjectResolution {
    if let Some(property) = property {
        return projects
            .iter()
            .position(|candidate| {
                candidate.project.name == property || candidate.project.path == property
            })
            .map_or(DeskProjectResolution::Missing, DeskProjectResolution::Use);
    }
    match projects.len() {
        0 => DeskProjectResolution::Missing,
        1 => DeskProjectResolution::Use(0),
        _ => DeskProjectResolution::Choose,
    }
}

fn updated_desk_brief(brief: &str) -> String {
    format!("Updated brief from the Desk:\n\n{brief}")
}

pub struct Workspace {
    hosts: Hosts,
    subscriptions: AgentSubscriptions,
    store: AgentStore,
    registry: AgentRegistry,
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
    /// Registered workdirs from every attached daemon; selection vocabulary
    /// for new agents, and what decides which host a new agent lands on.
    workdirs: Vec<HostProject>,
    /// Launch arguments for a configured Desk staffing or quick-spawn. The
    /// transient edits these; the writable dashboard row owns the message.
    new_agent_draft: Option<NewAgentDraft>,
    /// A NewAgent request from the draft is in flight; the draft buffer is
    /// kept intact until the daemon confirms creation, so a rejected request
    /// (bad working directory, say) never loses the message.
    /// Which host the pending draft agent was sent to, so its confirmation
    /// can be recognized and the compose surface reset.
    awaiting_draft_agent: Option<HostId>,
    /// Hosts that have delivered at least one `Ready`. A host attaches
    /// blind; until it answers, its agents do not exist for this client.
    ready_hosts: HashSet<HostId>,
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
    #[cfg(feature = "native")]
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
    active_context: ContextId,
    /// Warm surfaces grouped by their Desk room. This is deliberately an
    /// ephemeral cache: the Desk and underlying resources remain canonical.
    room_strips: HashMap<RoomId, Vec<WarmSurface>>,
    room_names: HashMap<RoomId, String>,
    current_room: Option<RoomId>,
    room_mru: Vec<RoomId>,
    overview_open: bool,
    mru_overlay: Option<MruOverlay>,
    mru_overlay_task: Option<Task<()>>,
    room_focus: HashMap<RoomId, SurfaceKey>,
    last_shift_tap: Option<std::time::Instant>,
    shell_touches: HashMap<TouchId, ShellTouchContact>,
    shell_touch_was_multi: bool,
    shell_touch_committed: bool,
    horizontal_strip_gesture_active: bool,
    deal_session_open: bool,
    deal_origin: Option<DealOrigin>,
    deal_current_taken: bool,
    deal_view: Option<DealView>,
    deal_hints_visible: bool,
    deal_controls_visible: bool,
    agent_last_interaction: HashMap<AgentId, i64>,
    dealer_signal_eval_scheduled: bool,
    _dealer_signal_task: Task<()>,
    lamp_on: bool,
    dealer_signals_initialized: bool,
    chime_above_threshold: bool,
    /// The dashboard: the rail as a real editor buffer, ambient chrome
    /// beside the active tree.
    dashboard: crate::dashboard::Dashboard,
    /// The vendored modal engine's status item, kept visible in Rho's frame.
    #[cfg(feature = "native")]
    mode_indicator: Entity<vim::ModeIndicator>,
    /// Compact Helix-style key guide shown on deal entry and `?`.
    /// Canonical per-host CRDT Desk buffers shared by dashboard and source
    /// views.
    desk_sync: DeskSync,
    /// Reconciles the dashboard when a desk buffer changes, per host.
    desk_edit_subscriptions: HashMap<HostId, gpui::Subscription>,
    /// Agent shown beside the dashboard cursor. Kept separate from the
    /// focused task so cursor previews do not rebuild or reorder the rail.
    dashboard_preview: Option<AgentId>,
    /// Client-local web page shown in the same right-hand preview card.
    #[cfg(feature = "native")]
    dashboard_web_preview: Option<(rho_browser::PageId, Entity<rho_browser::PageView>)>,
    /// Browser resources referenced by the last reconciled Desk documents.
    #[cfg(feature = "native")]
    browser_pages: HashSet<rho_browser::PageId>,
    browser_metadata_subscription: Option<gpui::Subscription>,
    /// Unreferenced browser pages waiting out the Desk edit grace period.
    #[cfg(feature = "native")]
    browser_page_gc: HashMap<rho_browser::PageId, Task<()>>,
    /// Read-only document shown when the synthetic Iris row is targeted.
    iris_preview: Entity<editor::Editor>,
    /// Each daemon's hidden persisted Iris coordinator. These identities stay
    /// outside the ordinary registry so Iris never enters agent/workstream
    /// lists, but still route transcript subscriptions to the owning host.
    iris_agents: HashMap<HostId, AgentId>,
    /// The Zulip client, started the first time its dashboard row is
    /// opened. Chat costs nothing until asked for.
    #[cfg(feature = "native")]
    zulip: Option<Entity<rho_zulip::session::Session>>,
    /// Machine-owned arrivals. This store is client-local and never enters a
    /// Desk CRDT buffer until an explicit filing verdict.
    inbox: InboxStore,
    pending_inbox_item: Option<InboxId>,
    scroll_journal_task: Option<Task<()>>,
    /// The completing-read strip at the bottom of the window, when open.
    minibuffer: Option<Minibuffer>,
    /// An open transient menu in the bottom strip; captures the keyboard
    /// via `transient_focus` while shown.
    transient: Option<crate::transient::Transient>,
    /// Parent menus beneath the open one; escape pops one level (magit's
    /// quit-one) before a final escape closes the strip.
    transient_stack: Vec<crate::transient::Transient>,
    transient_focus: gpui::FocusHandle,
    /// Evil's one-shot `SPC u` prefix. The next supported Desk command
    /// consumes it; every other non-modifier key clears it.
    universal_argument: bool,
    #[cfg(feature = "native")]
    git_approval_focus: gpui::FocusHandle,
    /// Focus beneath the single modal overlay. Transients, minibuffers, and
    /// Git approval hand this target between them so borrowing keyboard
    /// focus never changes dashboard/work mode.
    overlay_return_focus: Option<gpui::FocusHandle>,
    /// The last system notice, flashed in the bottom strip (emacs echo
    /// area). Cleared by its own timer or when the minibuffer opens.
    echo: Option<Echo>,
    #[cfg(feature = "native")]
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
    _universal_argument_subscription: gpui::Subscription,
    _window_activation_subscription: gpui::Subscription,
    #[cfg(all(target_family = "wasm", not(feature = "native")))]
    web: web::WebUi,
    #[cfg(feature = "native")]
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
            #[cfg(feature = "native")]
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
        #[cfg(feature = "native")]
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

#[derive(Clone)]
struct NewAgentDraft {
    intent: NewAgentIntent,
    host: Option<HostId>,
    workdir: Option<HostPath>,
    workspace: DraftWorkspace,
    role: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NewAgentIntent {
    Staff((HostId, usize)),
    QuickSpawn,
}

impl NewAgentIntent {
    fn topic(self) -> Option<(HostId, usize)> {
        match self {
            Self::Staff(topic) => Some(topic),
            Self::QuickSpawn => None,
        }
    }

    fn compose_label(self) -> &'static str {
        match self {
            Self::Staff(_) => "compose · staff heading",
            Self::QuickSpawn => "compose · quick-spawn",
        }
    }
}

#[derive(Clone)]
enum DraftWorkspace {
    NewOn(DraftBase),
    Join(String),
    Sandbox(DraftBase),
}

#[derive(Clone)]
enum DraftBase {
    Auto,
    Explicit(String),
}

impl DraftBase {
    fn from_input(input: &str) -> Self {
        if input.eq_ignore_ascii_case(crate::draft_view::DEFAULT_START) {
            Self::Auto
        } else {
            Self::Explicit(input.to_owned())
        }
    }

    fn target(&self) -> &str {
        match self {
            Self::Auto => crate::draft_view::DEFAULT_START,
            Self::Explicit(target) => target,
        }
    }
}

impl DraftWorkspace {
    fn label(&self) -> String {
        match self {
            Self::NewOn(base) => format!("new on {}", base.target()),
            Self::Join(target) => format!("join {target}"),
            Self::Sandbox(base) => format!("sandbox on {}", base.target()),
        }
    }
}

impl Workspace {
    #[cfg(feature = "native")]
    pub fn new(specs: Vec<HostSpec>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (hosts, events) = Hosts::new();
        let workspace = cx.entity().downgrade();
        let mode_indicator = cx.new(|cx| vim::ModeIndicator::new(window, cx));
        let draft_model = cx.new(|cx| DraftModel::new(workspace, cx));
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
            |this, _, event: &editor::EditorEvent, window, cx| {
                if matches!(
                    event,
                    editor::EditorEvent::SelectionsChanged { local: true }
                ) {
                    this.refresh_dashboard(window, cx);
                    this.dashboard_cursor_moved(window, cx);
                }
            },
        );
        let universal_argument_subscription = cx.observe_keystrokes(|this, event, window, cx| {
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
                let stepping_room_back = event
                    .action
                    .as_ref()
                    .is_some_and(|action| action.as_any().is::<RoomBack>())
                    || cx
                        .all_bindings_for_input(std::slice::from_ref(&event.keystroke))
                        .iter()
                        .any(|binding| binding.action().as_any().is::<RoomBack>());
                if !stepping_room_back {
                    this.dismiss_mru_overlay(cx);
                }
            }
            if this.universal_argument
                && !matches!(
                    event.keystroke.key.as_str(),
                    "shift" | "control" | "alt" | "platform" | "function"
                )
            {
                // Supported actions consume the prefix before observers run;
                // anything still armed here was an unrelated command.
                this.universal_argument = false;
                cx.notify();
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
            workdirs: Vec::new(),
            new_agent_draft: None,
            awaiting_draft_agent: None,
            ready_hosts: HashSet::new(),
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
            room_strips: HashMap::new(),
            room_names: HashMap::new(),
            current_room: None,
            room_mru: Vec::new(),
            overview_open: true,
            mru_overlay: None,
            mru_overlay_task: None,
            room_focus: HashMap::new(),
            last_shift_tap: None,
            shell_touches: HashMap::new(),
            shell_touch_was_multi: false,
            shell_touch_committed: false,
            horizontal_strip_gesture_active: false,
            deal_session_open: false,
            deal_origin: None,
            deal_current_taken: false,
            deal_view: None,
            deal_hints_visible: false,
            deal_controls_visible: false,
            agent_last_interaction: HashMap::new(),
            dealer_signal_eval_scheduled: false,
            _dealer_signal_task: dealer_signal_task,
            lamp_on: false,
            dealer_signals_initialized: false,
            chime_above_threshold: false,
            dashboard,
            mode_indicator,
            desk_sync: DeskSync::default(),
            desk_edit_subscriptions: HashMap::new(),
            dashboard_preview: None,
            dashboard_web_preview: None,
            browser_pages: HashSet::new(),
            browser_metadata_subscription: None,
            browser_page_gc: HashMap::new(),
            iris_preview,
            iris_agents: HashMap::new(),
            zulip: None,
            // Tests build many concurrent workspaces; they must never open
            // (or pollute) the user's real inbox database.
            inbox: if cfg!(test) {
                InboxStore::memory()
            } else {
                InboxStore::open_default().unwrap_or_else(|error| {
                    tracing::warn!(%error, "opening client inbox; using memory-only store");
                    InboxStore::memory()
                })
            },
            pending_inbox_item: None,
            scroll_journal_task: None,
            minibuffer: None,
            transient: None,
            transient_stack: Vec::new(),
            transient_focus: cx.focus_handle(),
            universal_argument: false,
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
            _universal_argument_subscription: universal_argument_subscription,
            _window_activation_subscription: window_activation_subscription,
            phone: phone::PhoneUi::new(cx),
        };
        for spec in specs {
            this.attach_host(spec, cx);
        }
        let draft = this.make_surface(SurfaceKey::Draft, window, cx);
        this.display_surface(draft);
        this.seed_draft(false, window, cx);
        // Startup lands in home mode: the dashboard is the front door.
        let dashboard_focus = this.dashboard.focus_handle(cx);
        window.focus(&dashboard_focus, cx);
        // Seed the listing before any event arrives ("+ new agent").
        this.refresh_dashboard(window, cx);
        this
    }

    /// Attaches a daemon. The name is registered with the registry first so
    /// that labels and chrome can qualify by host from the moment the host
    /// exists, not only once it answers.
    #[cfg(feature = "native")]
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
        self.quota_summaries.remove(&host);
        self.quota_history.remove(&host);
        self.global_usage.remove(&host);
        self.agent_cost_usage.remove(&host);
        self.workdirs.retain(|workdir| workdir.host != host);
        self.remote_projects.retain(|(owner, _), _| *owner != host);
        self.registry.detach_host(host);
        self.desk_edit_subscriptions.remove(&host);
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
            self.display_surface(draft);
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

    pub(crate) fn mark_desk_text_local(
        &mut self,
        host: HostId,
        clock: rho_ui_proto::desk::DeskClock,
    ) {
        self.desk_sync.mark_local(host, clock);
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

    fn active_pane(&self) -> &Pane<Surface> {
        self.contexts
            .get(&self.active_context)
            .expect("active context has a pane")
    }

    fn active_pane_mut(&mut self) -> &mut Pane<Surface> {
        self.contexts
            .get_mut(&self.active_context)
            .expect("active context has a pane")
    }

    fn remember_room(
        &mut self,
        room: crate::dashboard::DeskRoom,
        method: crate::journal::RoomSwitchMethod,
        cx: &mut Context<Self>,
    ) {
        let id = RoomId {
            host: room.host,
            heading_offset: room.heading_offset,
        };
        self.room_names.insert(id.clone(), room.name.clone());
        if self.current_room.as_ref() != Some(&id) {
            self.room_mru.retain(|candidate| candidate != &id);
            if let Some(previous) = self.current_room.replace(id.clone()) {
                self.room_mru.retain(|candidate| candidate != &previous);
                self.room_mru.insert(0, previous);
            }
            crate::journal::record(crate::journal::Event::RoomSwitched {
                room: room.name,
                method,
            });
        }
        self.overview_open = false;
        self.invalidate_dealer_signals(cx);
    }

    fn room_for_agent(&self, agent_id: AgentId, cx: &App) -> Option<crate::dashboard::DeskRoom> {
        self.dashboard.room_for_agent(agent_id, cx)
    }

    fn touch_room_surface(&mut self, surface: &Surface) {
        let Some(room) = self.current_room.clone() else {
            return;
        };
        let strip = self.room_strips.entry(room.clone()).or_default();
        if let Some(warm) = strip
            .iter_mut()
            .find(|warm| warm.surface.key == surface.key)
        {
            warm.surface = surface.clone();
            warm.context = self.active_context;
        } else {
            strip.push(WarmSurface {
                context: self.active_context,
                surface: surface.clone(),
            });
        }
        self.room_focus.insert(room, surface.key.clone());
    }

    fn slide_room_strip(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.overview_open || self.dashboard.deal_mode() {
            return;
        }
        let Some(room) = self.current_room.clone() else {
            return;
        };
        let current_key = self.active_pane().surface.key.clone();
        let Some(strip) = self.room_strips.get_mut(&room) else {
            return;
        };
        if strip.len() < 2 {
            return;
        }
        let current = strip
            .iter()
            .position(|warm| warm.surface.key == current_key)
            .unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(strip.len() - 1);
        if next == current {
            return;
        }
        let warm = strip[next].clone();
        self.active_context = warm.context;
        self.display_surface(warm.surface.clone());
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        crate::journal::record(crate::journal::Event::StripSlid {
            room: self.room_names.get(&room).cloned().unwrap_or_default(),
            direction: if delta < 0 {
                crate::journal::StripDirection::Left
            } else {
                crate::journal::StripDirection::Right
            },
            surface: Self::journal_surface(&warm.surface.key),
        });
        cx.notify();
    }

    fn remove_from_room_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overview_open || self.dashboard.deal_mode() {
            return;
        }
        let Some(room) = self.current_room.clone() else {
            return;
        };
        let key = self.active_pane().surface.key.clone();
        let Some(strip) = self.room_strips.get_mut(&room) else {
            return;
        };
        let Some(index) = strip.iter().position(|warm| warm.surface.key == key) else {
            return;
        };
        strip.remove(index);
        crate::journal::record(crate::journal::Event::StripRemoved {
            room: self.room_names.get(&room).cloned().unwrap_or_default(),
            surface: Self::journal_surface(&key),
        });
        if let Some(warm) = strip.get(index.saturating_sub(1)).cloned() {
            self.active_context = warm.context;
            self.display_surface(warm.surface);
            self.focus_active_surface(window, cx);
        } else {
            self.room_focus.remove(&room);
        }
        cx.notify();
    }

    fn open_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(room) = self.current_room.as_ref() {
            self.dashboard
                .cursor_to_doc(room.host, room.heading_offset, cx);
        }
        self.overview_open = true;
        self.dismiss_mru_overlay(cx);
        self.refresh_dashboard(window, cx);
        window.focus(&self.dashboard.focus_handle(cx), cx);
        crate::journal::record(crate::journal::Event::OverviewOpened {
            room: self
                .current_room
                .as_ref()
                .and_then(|room| self.room_names.get(room))
                .cloned(),
        });
        cx.notify();
    }

    fn toggle_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overview_open {
            self.overview_open = false;
            self.focus_active_surface(window, cx);
            cx.notify();
        } else {
            self.open_overview(window, cx);
        }
    }

    fn shell_touch(&mut self, event: &TouchEvent, window: &mut Window, cx: &mut Context<Self>) {
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
                                if dy > 0. {
                                    Some(if self.dashboard.deal_mode() {
                                        Box::new(DashboardDealExit) as Box<dyn gpui::Action>
                                    } else {
                                        Box::new(DealOpen) as Box<dyn gpui::Action>
                                    })
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
                } else if !self.shell_touch_committed && !self.shell_touch_was_multi {
                    let contact = self.shell_touches.get(&event.id).expect("touch exists");
                    let dx = (contact.position.x - contact.start.x).as_f32();
                    let dy = (contact.position.y - contact.start.y).as_f32();
                    if !self.dashboard.deal_mode()
                        && dx.abs() >= SHELL_SWIPE_DISTANCE.as_f32()
                        && dx.abs() >= dy.abs() * 1.25
                    {
                        Some(if dx < 0. {
                            Box::new(RoomStripLeft) as Box<dyn gpui::Action>
                        } else {
                            Box::new(RoomStripRight) as Box<dyn gpui::Action>
                        })
                    } else {
                        None
                    }
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
    }

    fn dismiss_mru_overlay(&mut self, cx: &mut Context<Self>) {
        let Some(overlay) = self.mru_overlay.take() else {
            return;
        };
        self.mru_overlay_task = None;
        let Some(current) = self.current_room.clone() else {
            return;
        };
        let mut mru = overlay.room_candidates[..overlay.room_index]
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        mru.push(overlay.origin);
        mru.extend(
            overlay
                .room_candidates
                .into_iter()
                .skip(overlay.room_index + 1),
        );
        mru.retain(|room| room != &current);
        self.room_mru = mru;
        cx.notify();
    }

    fn step_room_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(origin) = self.current_room.clone() else {
            return;
        };
        if self.mru_overlay.is_none() {
            if self.room_mru.is_empty() {
                return;
            }
            self.mru_overlay = Some(MruOverlay {
                origin,
                room_candidates: self.room_mru.clone(),
                room_index: 0,
            });
        } else if let Some(overlay) = &mut self.mru_overlay {
            if overlay.room_index + 1 >= overlay.room_candidates.len() {
                return;
            }
            overlay.room_index += 1;
        }
        let overlay = self.mru_overlay.as_ref().expect("MRU overlay exists");
        let target = overlay.room_candidates[overlay.room_index].clone();
        let depth = overlay.room_index + 1;
        let from = self
            .current_room
            .replace(target.clone())
            .expect("room exists");
        self.overview_open = false;
        if let Some(key) = self.room_focus.get(&target)
            && let Some(warm) = self
                .room_strips
                .get(&target)
                .and_then(|strip| strip.iter().find(|warm| &warm.surface.key == key))
                .cloned()
        {
            self.active_context = warm.context;
            self.display_surface(warm.surface);
            self.focus_active_surface(window, cx);
        }
        crate::journal::record(crate::journal::Event::MruStepped {
            from_room: self.room_names.get(&from).cloned().unwrap_or_default(),
            to_room: self.room_names.get(&target).cloned().unwrap_or_default(),
            depth,
        });
        crate::journal::record(crate::journal::Event::RoomSwitched {
            room: self.room_names.get(&target).cloned().unwrap_or_default(),
            method: crate::journal::RoomSwitchMethod::Mru,
        });
        self.invalidate_dealer_signals(cx);
        self.mru_overlay_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1_200))
                .await;
            let _ = this.update(cx, |this, cx| this.dismiss_mru_overlay(cx));
        }));
        cx.notify();
    }

    fn journal_card_identity(
        identity: &crate::dashboard::DealCardIdentity,
    ) -> crate::journal::DealerCardIdentity {
        match identity {
            crate::dashboard::DealCardIdentity::Desk {
                host,
                heading_offset,
            } => crate::journal::DealerCardIdentity::Desk {
                host: host.to_string(),
                heading_offset: *heading_offset,
            },
            crate::dashboard::DealCardIdentity::Agent(agent_id) => {
                crate::journal::DealerCardIdentity::Agent {
                    agent_id: agent_id.encoded(),
                }
            }
            crate::dashboard::DealCardIdentity::Inbox(id) => {
                crate::journal::DealerCardIdentity::Inbox { id: id.clone() }
            }
        }
    }

    fn invalidate_dealer_signals(&mut self, cx: &mut Context<Self>) {
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
        let current_room = self
            .current_room
            .as_ref()
            .and_then(|room| Some((room.host, self.room_names.get(room)?.as_str())));
        let mut hand = self.dashboard.dealer_hand(
            &self.registry,
            &self.inbox,
            now,
            current_room,
            &self.agent_last_interaction,
            cx,
        );
        hand.cards.retain(|card| {
            if let Some(current) = self.dashboard.current_deal_card() {
                return card.identity != current.identity;
            }
            if self.overview_open {
                return !matches!(
                    (&card.identity, self.dashboard.cursor_topic(cx)),
                    (
                        crate::dashboard::DealCardIdentity::Desk { host, heading_offset },
                        Some((cursor_host, cursor_offset))
                    ) if *host == cursor_host && *heading_offset == cursor_offset
                );
            }
            match &self.active_pane().surface.key {
                SurfaceKey::Transcript(agent_id) => {
                    card.identity != crate::dashboard::DealCardIdentity::Agent(*agent_id)
                }
                #[cfg(feature = "native")]
                SurfaceKey::Browser(page) => !matches!(
                    card.inbox_source,
                    Some(crate::dashboard::DealerInboxSource::Page(id)) if id == *page
                ),
                _ => true,
            }
        });
        let top = hand.cards.first();
        let max_priority = top.map(|card| card.priority);
        let card = top.map(|card| Self::journal_card_identity(&card.identity));
        let lamp_on =
            max_priority.is_some_and(|priority| priority >= crate::dashboard::LAMP_THRESHOLD);
        if lamp_on != self.lamp_on {
            self.lamp_on = lamp_on;
            crate::journal::record(crate::journal::Event::LampTransition {
                state: if lamp_on {
                    crate::journal::SignalState::On
                } else {
                    crate::journal::SignalState::Off
                },
                hand_max_priority: max_priority,
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
                #[cfg(feature = "native")]
                if !cfg!(test) {
                    self.chime.play();
                }
                crate::journal::record(crate::journal::Event::ChimeRing {
                    hand_max_priority: priority,
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
            ContextId::Zulip => true,
        };
        self.contexts.retain(|context, _| keep(context));
        self.surfaces.retain(|context, _| keep(context));
        #[cfg(feature = "native")]
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
            ConnEvent::DeskSnapshot {
                snapshot,
                replica_id,
            } => {
                let buffer = self
                    .desk_sync
                    .apply_snapshot(host, snapshot, replica_id, cx);
                self.dashboard.set_source(host, buffer.downgrade(), cx);
                // Structure follows the text: any edit to the desk buffer
                // (vim in the excerpts, or a CRDT op from another client)
                // re-parses and reconciles.
                self.desk_edit_subscriptions.insert(
                    host,
                    cx.subscribe_in(
                        &buffer,
                        window,
                        |this, _, event: &language::BufferEvent, window, cx| {
                            if matches!(event, language::BufferEvent::Edited { .. }) {
                                this.refresh_dashboard(window, cx);
                            }
                        },
                    ),
                );
            }
            ConnEvent::DeskTextApplied(record) => {
                self.desk_sync.apply_text(host, record, cx);
            }
            ConnEvent::Ready {
                agents,
                iris_agent,
                projects: workdirs,
                auth,
                machine_seed,
                agent_counter,
            } => {
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
                #[cfg(all(target_family = "wasm", not(feature = "native")))]
                self.web.online(host);
                self.refresh_draft_agent_targets(cx);
                if first_ready && matches!(self.registry.active_pane(), ActivePane::Startup) {
                    // The startup scaffold guessed before daemon data existed;
                    // refresh it now that workdir names and topics are known.
                    self.seed_draft(false, window, cx);
                }
                if let Some(agent_ids) = initial_subscriptions
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
                self.hosts.set_status(host, HostStatus::Recovering(elapsed));
                cx.notify();
            }
            ConnEvent::Recovered => {
                self.hosts.set_status(host, HostStatus::Online);
                cx.notify();
            }
            ConnEvent::Disconnected(reason) => {
                #[cfg(feature = "native")]
                let had_git_approval = if let Some(pending) = self.pending_git_approval.take() {
                    let _ = pending.response.send(GitApprovalDecision::Done);
                    true
                } else {
                    false
                };
                #[cfg(feature = "native")]
                if had_git_approval {
                    self.finish_overlay_focus(window, cx);
                }
                // The host's agents stay in the rail with their retained
                // transcripts: losing a connection is not losing the work.
                // Only detaching (`space h d`) forgets a daemon.
                self.hosts
                    .set_status(host, HostStatus::Disconnected(reason.clone()));
                // A later reconnect is a fresh session for this host, and
                // earns the same warm start its first `Ready` did.
                self.ready_hosts.remove(&host);
                if self.awaiting_draft_agent == Some(host) {
                    self.awaiting_draft_agent = None;
                }
                if self.iris_host == Some(host) {
                    self.stop_iris();
                }
                self.update_statuses(cx);
                cx.notify();
            }
            #[cfg(feature = "native")]
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
            #[cfg(feature = "native")]
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
            #[cfg(all(target_family = "wasm", not(feature = "native")))]
            ConnEvent::AuthorizationRequired => {
                self.web.authorization_required(host);
                cx.notify();
            }
            #[cfg(all(target_family = "wasm", not(feature = "native")))]
            ConnEvent::EnrollmentRequired(code) => {
                self.web.enrollment_required(host, code);
                cx.notify();
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
        #[cfg(feature = "native")]
        if matches!(self.active_pane().surface.view, SurfaceView::ZulipNarrow(_)) {
            self.zulip_submit(cx);
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
        #[cfg(feature = "native")]
        let task = connection.start_native_realtime(stop_rx, input_muted_rx, cx);
        #[cfg(all(target_family = "wasm", not(feature = "native")))]
        let task = {
            let dialer = connection.realtime_dialer();
            cx.spawn(async move |_, cx| {
                crate::realtime_client::run(
                    move |offer_sdp| {
                        let dialer = dialer.clone();
                        async move { dialer.open(offer_sdp).await }
                    },
                    stop_rx,
                    input_muted_rx,
                )
                .await
            })
        };
        self.realtime_stop = Some(stop);
        self.realtime_input_muted = Some(input_muted);
        let starting = match self.hosts.len() > 1 {
            true => format!("starting Iris on {}…", self.host_label(host)),
            false => "starting Iris…".to_owned(),
        };
        self.notice_on(None, &starting, StyleClass::SystemInfo, cx);
        self.realtime_task = Some(cx.spawn(async move |this, cx| {
            #[cfg(feature = "native")]
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!("realtime task failed: {error}")),
            };
            #[cfg(all(target_family = "wasm", not(feature = "native")))]
            let result = task.await;
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
    #[cfg(feature = "native")]
    pub(crate) fn open_zulip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.zulip_session(cx);
        self.active_context = ContextId::Zulip;
        let surface = self.make_surface(SurfaceKey::ZulipInbox, window, cx);
        self.display_surface(surface);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    #[cfg(feature = "native")]
    fn zulip_session(&mut self, cx: &mut Context<Self>) -> Entity<rho_zulip::session::Session> {
        self.zulip
            .get_or_insert_with(|| cx.new(rho_zulip::session::Session::new))
            .clone()
    }

    /// The host services the Zulip surfaces borrow: editor chrome and the
    /// transcript's Markdown pipeline, so chat reads like every other
    /// buffer in the frame.
    #[cfg(feature = "native")]
    fn zulip_hooks() -> rho_zulip::ui::Hooks {
        rho_zulip::ui::Hooks {
            configure_editor: crate::editor_config::configure,
            configure_markdown: crate::render::markdown::configure_buffer,
        }
    }

    /// Shows one Zulip conversation, marking the conversation being left
    /// as read — a Gnus summary buffer's exit, which is what makes `n`
    /// walk unreads down to nothing.
    #[cfg(feature = "native")]
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
        self.display_surface(surface);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Marks the conversation on screen read, if one is.
    #[cfg(feature = "native")]
    fn leave_zulip_narrow(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::ZulipNarrow(view) = &self.active_pane().surface.view {
            view.clone().update(cx, |view, cx| view.mark_read(cx));
        }
    }

    /// `enter` inside the Zulip inbox: open the conversation under the
    /// cursor.
    #[cfg(feature = "native")]
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
    #[cfg(feature = "native")]
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

    /// `q`: leave Zulip for the agent world it was entered from, marking
    /// the conversation on screen read on the way out.
    #[cfg(feature = "native")]
    fn zulip_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_zulip_narrow(cx);
        let agent_id = self.registry.selected_agent().copied();
        self.select_agent(agent_id, window, cx);
    }

    /// `P`: page further back in the conversation on screen.
    #[cfg(feature = "native")]
    fn zulip_load_older(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::ZulipNarrow(view) = &self.active_pane().surface.view {
            view.clone().update(cx, |view, cx| view.load_older(cx));
        }
    }

    /// `enter` in a Zulip conversation: send the composed message.
    #[cfg(feature = "native")]
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
        // Start a top-level agent from the draft.
        self.hosts.send(
            host,
            ClientMessage::NewAgent {
                role,
                start,
                content: Some(content),
                desk_anchor: None,
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
            SurfaceView::Draft { .. } | SurfaceView::Transcript { .. }
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
        if desk_heading_without_agent(
            self.dashboard.is_focused(window, cx),
            self.dashboard.cursor_target(&self.registry, cx),
        ) {
            self.dashboard.set_cursor_heading_property(
                if hide {
                    rho_ui_proto::desk::TemporalMarkKind::Discarded
                } else {
                    rho_ui_proto::desk::TemporalMarkKind::Done
                },
                chrono::Local::now()
                    .date_naive()
                    .and_time(chrono::NaiveTime::MIN),
                None,
                cx,
            );
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

    #[cfg(feature = "native")]
    pub fn open_browser_page(
        &mut self,
        id: rho_browser::PageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(room) = self.dashboard.room_for_page(id, cx) {
            self.remember_room(
                room,
                if self.overview_open {
                    crate::journal::RoomSwitchMethod::Overview
                } else {
                    crate::journal::RoomSwitchMethod::Open
                },
                cx,
            );
        }
        let Some(model) = rho_browser::open_page(id, cx) else {
            let message = format!("browser page not found: {id}");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            return;
        };
        self.scan_browser_pages_for_gc(cx);
        self.observe_browser_metadata(&model, cx);
        let view = cx.new(|cx| rho_browser::PageView::new(model, id, cx));
        let surface = Self::wrap_surface(SurfaceKey::Browser(id), SurfaceView::Browser(view));
        self.display_surface(surface);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    #[cfg(feature = "native")]
    pub fn cmd_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let input = input.trim();
                if input.is_empty() {
                    return;
                }
                let url = if input.contains("://") {
                    input.to_owned()
                } else {
                    format!("https://{input}")
                };
                workspace.create_browser_page(url, window, cx);
            },
        );
        self.open_prompt(
            "new web:",
            std::rc::Rc::new(|_, _, _| Vec::new()),
            on_submit,
            window,
            cx,
        );
    }

    #[cfg(all(target_family = "wasm", not(feature = "native")))]
    pub fn cmd_browser(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.notice_on(
            None,
            "browser pages are available in the native client",
            StyleClass::SystemInfo,
            cx,
        );
    }

    #[cfg(feature = "native")]
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

    #[cfg(feature = "native")]
    fn create_browser_page(&mut self, url: String, window: &Window, cx: &mut Context<Self>) {
        let context = self.capture_context(window, cx);
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
                let inbox_id = match this.inbox.append(InboxDraft {
                    kind: InboxKind::Capture,
                    text: record.launch_url,
                    source: SourceReference::Page { id: id.to_string() },
                    context,
                    waiting_on: None,
                }) {
                    Ok(inbox_id) => inbox_id,
                    Err(error) => {
                        tracing::error!(%error, "persisting new browser page in inbox");
                        rho_browser::close_page(id, cx).detach();
                        this.notice_on(
                            None,
                            "new web: could not save inbox item",
                            StyleClass::SystemInfo,
                            cx,
                        );
                        return;
                    }
                };
                crate::journal::record(crate::journal::Event::Capture {
                    inbox_id: inbox_id.0,
                    method: crate::journal::CaptureMethod::TabBirth,
                });
                this.invalidate_dealer_signals(cx);
                this.preview_browser_page(id, window, cx);
                this.focus_rail(window, cx);
                this.echo("captured", StyleClass::SystemInfo, cx);
            });
        })
        .detach();
    }

    fn capture_context(&self, window: &Window, cx: &mut Context<Self>) -> CapturedContext {
        let position = self.dashboard.capture_position(cx);
        let host = position.as_ref().map(|(host, ..)| host.to_string());
        let room = position.map(|(_, _, room)| room);
        let focused_surface = if self.dashboard.is_focused(window, cx) {
            "Desk".to_owned()
        } else {
            self.surface_name(&self.active_pane().surface.key)
        };
        CapturedContext {
            host,
            room,
            focused_surface,
        }
    }

    pub(crate) fn cmd_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let context = self.capture_context(window, cx);
        let source = self.dashboard.capture_position(cx).map_or(
            SourceReference::None,
            |(host, offset, _)| SourceReference::DeskPosition {
                host: host.to_string(),
                offset,
            },
        );
        self.open_prompt(
            "capture:",
            std::rc::Rc::new(|_, _, _| Vec::new()),
            std::rc::Rc::new(move |workspace, input, _, cx| {
                let text = input.trim();
                if text.is_empty() {
                    return;
                }
                match workspace.inbox.append(InboxDraft {
                    kind: InboxKind::Capture,
                    text: text.to_owned(),
                    source: source.clone(),
                    context: context.clone(),
                    waiting_on: None,
                }) {
                    Ok(id) => {
                        crate::journal::record(crate::journal::Event::Capture {
                            inbox_id: id.0,
                            method: crate::journal::CaptureMethod::Keyboard,
                        });
                        workspace.invalidate_dealer_signals(cx);
                        workspace.echo("captured", StyleClass::SystemInfo, cx)
                    }
                    Err(error) => {
                        tracing::error!(%error, "persisting inbox capture");
                        workspace.notice_on(None, "capture failed", StyleClass::SystemInfo, cx);
                    }
                }
            }),
            window,
            cx,
        );
    }

    /// Opens the machine-owned inbox as a completing list. Selecting a row
    /// opens a two-key membrane: `f` files, `d` discards.
    pub(crate) fn open_inbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Err(error) = self.inbox.refresh_deferred(now_ms) {
            tracing::warn!(%error, "resurfacing deferred inbox items");
        }
        self.invalidate_dealer_signals(cx);
        if self.inbox.pending_items(now_ms).next().is_none() {
            self.notice_on(None, "inbox empty", StyleClass::SystemInfo, cx);
            self.drop_transient();
            return;
        }
        self.open_prompt(
            "inbox:",
            std::rc::Rc::new(|workspace, needle, _| {
                let needle = needle.to_lowercase();
                let now_ms = chrono::Utc::now().timestamp_millis();
                workspace
                    .inbox
                    .pending_items(now_ms)
                    .filter(|item| item.text.to_lowercase().contains(&needle))
                    .map(|item| crate::minibuffer::Candidate {
                        value: item.text.clone(),
                        description: format!(
                            "{:?} · {}",
                            item.kind,
                            item.context.room.as_deref().unwrap_or("no room")
                        ),
                    })
                    .collect()
            }),
            std::rc::Rc::new(|workspace, input, window, cx| {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let Some(item) = workspace
                    .inbox
                    .pending_items(now_ms)
                    .find(|item| item.text == input)
                    .cloned()
                else {
                    workspace.notice_on(None, "inbox: choose an item", StyleClass::SystemInfo, cx);
                    return;
                };
                workspace.pending_inbox_item = Some(item.id);
                workspace.open_transient(crate::transient::inbox_item_menu(), window, cx);
            }),
            window,
            cx,
        );
    }

    pub(crate) fn discard_inbox_item(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_inbox_item.take() else {
            return;
        };
        match self.inbox.verdict(&id, Verdict::Discarded) {
            Ok(_) => {
                crate::journal::record(crate::journal::Event::InboxVerdict {
                    inbox_id: id.0,
                    verdict: crate::journal::InboxVerdict::Discard,
                });
                self.echo("discarded", StyleClass::SystemInfo, cx);
                self.invalidate_dealer_signals(cx);
            }
            Err(error) => tracing::error!(%error, "discarding inbox item"),
        }
        self.drop_transient();
        #[cfg(feature = "native")]
        self.scan_browser_pages_for_gc(cx);
    }

    pub(crate) fn defer_inbox_item(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_inbox_item.take() else {
            return;
        };
        let until_ms = (chrono::Utc::now() + chrono::Duration::days(1)).timestamp_millis();
        match self.inbox.verdict(&id, Verdict::Deferred { until_ms }) {
            Ok(_) => {
                crate::journal::record(crate::journal::Event::InboxVerdict {
                    inbox_id: id.0,
                    verdict: crate::journal::InboxVerdict::Defer { until_ms },
                });
                self.echo("deferred 1d", StyleClass::SystemInfo, cx);
                self.invalidate_dealer_signals(cx);
            }
            Err(error) => tracing::error!(%error, "deferring inbox item"),
        }
        self.drop_transient();
    }

    pub(crate) fn prompt_file_inbox_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_inbox_item.is_none() {
            return;
        }
        self.open_prompt(
            "file under:",
            std::rc::Rc::new(|workspace, needle, cx| {
                workspace
                    .dashboard
                    .heading_candidates(&workspace.registry, needle, cx)
                    .into_iter()
                    .map(|(value, description)| crate::minibuffer::Candidate { value, description })
                    .collect()
            }),
            std::rc::Rc::new(|workspace, heading, window, cx| {
                workspace.file_inbox_item(&heading, window, cx)
            }),
            window,
            cx,
        );
    }

    /// Dealer-facing filing handoff. Destination completion and the eventual
    /// Desk edit remain owned by the inbox filing membrane.
    pub fn begin_inbox_filing(
        &mut self,
        id: &InboxId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.inbox.get(id).is_none() {
            return Err(format!("inbox item {} no longer exists", id.0));
        }
        self.pending_inbox_item = Some(id.clone());
        self.prompt_file_inbox_item(window, cx);
        Ok(())
    }

    fn file_inbox_item(&mut self, heading: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.pending_inbox_item.clone() else {
            return;
        };
        let Some(item) = self.inbox.get(&id).cloned() else {
            return;
        };
        let filed = match &item.source {
            SourceReference::Page { id } => self.file_inbox_page(heading, id, cx),
            _ => self.dashboard.append_child_heading(heading, &item.text, cx),
        };
        if !filed {
            self.notice_on(None, "file: heading not found", StyleClass::SystemInfo, cx);
            return;
        }
        if let Err(error) = self.inbox.verdict(&id, Verdict::Filed) {
            tracing::error!(%error, "retiring filed inbox item");
            self.notice_on(
                None,
                "filed, but inbox persistence failed",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        crate::journal::record(crate::journal::Event::InboxVerdict {
            inbox_id: id.0.clone(),
            verdict: crate::journal::InboxVerdict::File {
                heading: heading.to_owned(),
            },
        });
        self.pending_inbox_item = None;
        self.refresh_dashboard(window, cx);
        self.echo(
            &format!("filed under {heading}"),
            StyleClass::SystemInfo,
            cx,
        );
    }

    #[cfg(feature = "native")]
    fn file_inbox_page(&mut self, heading: &str, id: &str, cx: &mut Context<Self>) -> bool {
        self.dashboard.tag_heading_with_page(heading, id, cx)
    }

    #[cfg(not(feature = "native"))]
    fn file_inbox_page(&mut self, _heading: &str, _id: &str, _cx: &mut Context<Self>) -> bool {
        false
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

    #[cfg(feature = "native")]
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

    #[cfg(all(target_family = "wasm", not(feature = "native")))]
    pub(crate) fn cmd_upload_gui_telemetry(&mut self, cx: &mut Context<Self>) {
        self.notice_on(
            None,
            "performance snapshots are available in the native GUI",
            StyleClass::SystemInfo,
            cx,
        );
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
    #[cfg(feature = "native")]
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

    #[cfg(all(target_family = "wasm", not(feature = "native")))]
    pub(crate) fn cmd_host_attach(&mut self, _: &str, cx: &mut Context<Self>) {
        self.notice_on(
            None,
            "attach: browser hosts come from the page URL",
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
        #[cfg(feature = "native")]
        self.phone.remove_key(&SurfaceKey::Transcript(agent_id));
        self.pending_syncs.remove(&agent_id);
        self.models.remove(&agent_id);
    }

    pub fn open_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        crate::journal::record(crate::journal::Event::AgentOpened {
            agent_id: agent_id.encoded(),
        });
        if let Some(room) = self.room_for_agent(agent_id, cx) {
            self.remember_room(
                room,
                if self.overview_open {
                    crate::journal::RoomSwitchMethod::Overview
                } else {
                    crate::journal::RoomSwitchMethod::Open
                },
                cx,
            );
        }
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

        let row = if self.dashboard.focus_handle(cx).is_focused(window) {
            self.dashboard.cursor_target(&self.registry, cx)
        } else {
            None
        };
        let subject = match row {
            Some(RowTarget::Agent { agent_id, .. }) => Some(Subject {
                agent: Some(agent_id),
                agents: self.registry.agent_subtree(agent_id),
            }),
            // Anywhere in a staffed heading's subtree, chords act on its
            // top agent.
            Some(RowTarget::Topic {
                host,
                offset,
                first_attention,
                ..
            }) => first_attention
                .or_else(|| self.dashboard.first_agent_for_topic((host, offset)))
                .map(|agent_id| Subject {
                    agent: Some(agent_id),
                    agents: self.registry.agent_subtree(agent_id),
                }),
            _ => None,
        };
        subject
            .or_else(|| {
                let agent_id = self.registry.selected_agent().copied()?;
                Some(Subject {
                    agent: Some(agent_id),
                    agents: self.registry.agent_subtree(agent_id),
                })
            })
            .unwrap_or_default()
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

    /// Jumps to the rail's most urgent agent (excluding the current one), so
    /// working through a backlog is one keystroke per agent.
    pub fn jump_to_attention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.registry.next_attention_agent() else {
            self.notice_on(
                None,
                "attention-jump: nothing is waiting on you",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.open_agent(agent_id, window, cx);
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

    fn resolve_workdir_on_host(&self, argument: &str, host: HostId) -> Result<HostPath, String> {
        if let Some(registered) = self.registered_workdir(argument) {
            return (registered.host == host)
                .then_some(registered)
                .ok_or_else(|| "project belongs to a different host".to_owned());
        }
        if argument.contains(':') {
            let workdir = self.resolve_workdir(argument)?;
            return (workdir.host == host)
                .then_some(workdir)
                .ok_or_else(|| "project belongs to a different host".to_owned());
        }
        Ok(HostPath {
            host,
            path: Utf8PathBuf::from(argument),
        })
    }

    /// Emacs `message`: the notice lands in the transcript (the durable
    /// log) and flashes in the echo area at the bottom of the window.
    fn notice_on(
        &mut self,
        agent_id: Option<&AgentId>,
        text: &str,
        class: StyleClass,
        cx: &mut Context<Self>,
    ) {
        let view = agent_id
            .or_else(|| self.registry.selected_agent())
            .and_then(|agent_id| self.models.get(agent_id))
            .cloned();
        match view {
            Some(view) => view.update(cx, |view, cx| view.system_notice(text, class, cx)),
            None => self
                .draft_model
                .update(cx, |view, cx| view.system_notice(text, class, cx)),
        }
        self.echo(text, class, cx);
    }

    /// Shows a message in the echo area; replacing a message cancels its
    /// predecessor's dismiss timer.
    fn echo(&mut self, text: &str, class: StyleClass, cx: &mut Context<Self>) {
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

    pub fn select_agent(
        &mut self,
        agent_id: Option<AgentId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overview_open = false;
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
        #[cfg(feature = "native")]
        {
            self.dashboard_web_preview = None;
        }
        self.hosts
            .focus_agent(self.host_of(agent_id).map(|host| (host, agent_id)));
        self.ensure_duration_timer(cx);
        cx.notify();
    }

    #[cfg(feature = "native")]
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
        self.display_surface(surface);
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
        #[cfg(feature = "native")]
        if let Some(RowTarget::Page(id)) = &target {
            self.preview_browser_page(*id, window, cx);
            return;
        }
        // A reply draft sits inside its agent's heading region, so it keeps
        // that agent on screen while the reply is typed.
        let agent = match target {
            Some(RowTarget::Agent { agent_id, .. }) | Some(RowTarget::Reply(agent_id)) => {
                Some(agent_id)
            }
            // Anywhere else in a staffed heading's subtree previews its top
            // agent. Portal rows are handled by the arm above.
            Some(RowTarget::Topic {
                host,
                offset,
                first_attention,
                ..
            }) => first_attention.or_else(|| self.dashboard.first_agent_for_topic((host, offset))),
            Some(RowTarget::NewDraft(Some(topic))) => self.dashboard.first_agent_for_topic(topic),
            _ => None,
        };
        match agent {
            Some(agent_id) if self.dashboard_preview != Some(agent_id) => {
                self.preview_agent(agent_id, window, cx);
            }
            Some(_) => {}
            None => self.clear_dashboard_preview(cx),
        }
    }

    /// Hides the preview pane: the cursor is on a header, prose, or an
    /// unstaffed heading, so no agent claims the frame.
    fn clear_dashboard_preview(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native")]
        let web_preview_empty = self.dashboard_web_preview.is_none();
        #[cfg(not(feature = "native"))]
        let web_preview_empty = true;
        if self.dashboard_preview.is_none() && web_preview_empty {
            return;
        }
        self.dashboard_preview = None;
        #[cfg(feature = "native")]
        {
            self.dashboard_web_preview = None;
        }
        self.hosts.focus_agent(None);
        cx.notify();
    }

    /// The active context's surface with the given key, whether or not
    /// the viewport currently displays it.
    fn find_surface(&self, pred: impl Fn(&Surface) -> bool) -> Option<&Surface> {
        self.surfaces
            .get(&self.active_context)?
            .iter()
            .find(|surface| pred(surface))
    }

    /// Human name of a surface, as `:buffer`/`:close` address it.
    fn surface_name(&self, key: &SurfaceKey) -> String {
        match key {
            SurfaceKey::Draft => "draft".to_owned(),
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
            #[cfg(feature = "native")]
            SurfaceKey::Browser(browser) => browser.to_string(),
            SurfaceKey::ZulipInbox => "zulip".to_owned(),
            SurfaceKey::ZulipNarrow { label } => label.clone(),
        }
    }

    fn surface_kind(key: &SurfaceKey) -> &'static str {
        match key {
            SurfaceKey::Draft => "compose",
            SurfaceKey::Transcript(_) => "transcript",
            SurfaceKey::File { .. } => "file",
            SurfaceKey::Shell(_) => "shell",
            SurfaceKey::Diff { .. } => "diff",
            SurfaceKey::Terminal { .. } => "terminal",
            #[cfg(feature = "native")]
            SurfaceKey::Browser(_) => "browser",
            SurfaceKey::ZulipInbox => "zulip inbox",
            SurfaceKey::ZulipNarrow { .. } => "zulip",
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
        self.display_surface(surface);
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
        #[cfg(feature = "native")]
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
            SurfaceKey::Transcript(agent_id) => SurfaceIdentity::Transcript {
                agent_id: agent_id.encoded(),
            },
            SurfaceKey::File { agent_id, path } => SurfaceIdentity::File {
                agent_id: agent_id.encoded(),
                path: path.to_string(),
            },
            SurfaceKey::Shell(agent_id) => SurfaceIdentity::Shell {
                agent_id: agent_id.encoded(),
            },
            SurfaceKey::Diff { agent_id } => SurfaceIdentity::Diff {
                agent_id: agent_id.encoded(),
            },
            SurfaceKey::Terminal {
                agent_id,
                terminal_id,
            } => SurfaceIdentity::Terminal {
                agent_id: agent_id.encoded(),
                terminal_id: *terminal_id,
            },
            #[cfg(feature = "native")]
            SurfaceKey::Browser(page_id) => SurfaceIdentity::Browser {
                page_id: page_id.to_string(),
            },
            SurfaceKey::ZulipInbox => SurfaceIdentity::ZulipInbox,
            SurfaceKey::ZulipNarrow { label } => SurfaceIdentity::ZulipNarrow {
                label: label.clone(),
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
            self.horizontal_strip_gesture_active = false;
        } else if event.delta.precise() && !self.horizontal_strip_gesture_active {
            // Focused children consume horizontal motion while they can
            // scroll it. Only their edge spill bubbles to this strip seam.
            let delta = event.delta.pixel_delta(px(20.));
            if self.dashboard.deal_mode() && delta.y > px(12.) && delta.y.abs() > delta.x.abs() {
                self.horizontal_strip_gesture_active = true;
                window.dispatch_action(Box::new(DealOpen), cx);
            } else if delta.x.abs() > delta.y.abs() && delta.x.abs() >= px(12.) {
                self.horizontal_strip_gesture_active = true;
                self.slide_room_strip(if delta.x > px(0.) { -1 } else { 1 }, window, cx);
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
                SurfaceView::Draft { editor, .. }
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
                #[cfg(feature = "native")]
                SurfaceView::Browser(_) => 0,
                #[cfg(feature = "native")]
                SurfaceView::ZulipInbox(view) => {
                    let editor = view.read(cx).editor().clone();
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
                #[cfg(feature = "native")]
                SurfaceView::ZulipNarrow(view) => {
                    let editor = view.read(cx).editor().clone();
                    editor.update(cx, |editor, cx| editor.scroll_position(cx).y as i64)
                }
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
    fn display_surface(&mut self, surface: Surface) {
        use std::collections::hash_map::Entry;
        let list = self.surfaces.entry(self.active_context).or_default();
        match list.iter_mut().find(|s| **s == surface) {
            Some(existing) => *existing = surface.clone(),
            None => list.push(surface.clone()),
        }
        #[cfg(feature = "native")]
        if self.phone.enabled {
            self.phone.show(self.active_context, surface.key.clone());
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
        self.touch_room_surface(&shown);
        crate::journal::record(crate::journal::Event::SurfaceDisplayed {
            surface: Self::journal_surface(&shown.key),
        });
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
            self.display_surface(surface);
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
                        this.display_surface(surface);
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
            self.display_surface(surface);
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
                        this.display_surface(surface);
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
            self.display_surface(surface);
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
                        this.display_surface(surface);
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
            self.display_surface(surface);
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
                        this.display_surface(surface);
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
    pub(crate) fn sync_dashboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_dashboard(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn dashboard_preview_agent(&self) -> Option<AgentId> {
        self.dashboard_preview
    }

    #[cfg(test)]
    pub(crate) fn configured_draft_topic_for_test(&self) -> Option<(HostId, usize)> {
        self.dashboard.new_draft_topic()
    }

    #[cfg(test)]
    pub(crate) fn has_new_agent_configuration_for_test(&self) -> bool {
        self.new_agent_draft.is_some()
    }

    #[cfg(test)]
    pub(crate) fn has_universal_argument_for_test(&self) -> bool {
        self.universal_argument
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
    pub(crate) fn desk_buffer_for_test(&self, host: HostId) -> Option<Entity<language::Buffer>> {
        self.desk_sync.buffer(host)
    }

    /// One TAB-cycle step on the heading at `offset`, cursor included,
    /// with the fold state applied to the display.
    #[cfg(test)]
    pub(crate) fn dashboard_cycle_fold_for_test(
        &mut self,
        host: HostId,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dashboard.cursor_to_doc(host, offset, cx);
        self.dashboard.sync(&self.registry, &self.inbox, window, cx);
        self.dashboard.toggle_subagents(cx);
        self.dashboard.sync(&self.registry, &self.inbox, window, cx);
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
    pub(crate) fn configure_room_mru_for_test(&mut self, names: &[&str]) {
        let rooms = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let room = RoomId {
                    host: HostId::default(),
                    heading_offset: index,
                };
                self.room_names.insert(room.clone(), (*name).to_owned());
                room
            })
            .collect::<Vec<_>>();
        self.current_room = rooms.first().cloned();
        self.room_mru = rooms.into_iter().skip(1).collect();
    }

    #[cfg(test)]
    pub(crate) fn current_room_name_for_test(&self) -> Option<&str> {
        self.current_room
            .as_ref()
            .and_then(|room| self.room_names.get(room).map(String::as_str))
    }

    #[cfg(test)]
    pub(crate) fn has_room_mru_overlay_for_test(&self) -> bool {
        self.mru_overlay.is_some()
    }

    #[cfg(test)]
    pub(crate) fn step_room_back_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step_room_back(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn current_deal_card_for_test(
        &self,
    ) -> Option<(
        crate::dashboard::DealCardIdentity,
        crate::dashboard::DealCardKind,
    )> {
        self.dashboard
            .current_deal_card()
            .map(|card| (card.identity.clone(), card.kind))
    }

    #[cfg(test)]
    pub(crate) fn inject_agent_deal_card_for_test(&mut self, agent_id: AgentId) {
        self.dashboard.inject_agent_deal_card_for_test(agent_id);
    }

    #[cfg(test)]
    pub(crate) fn seek_deal_card_for_test(
        &mut self,
        wanted: fn(crate::dashboard::DealCardKind) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.dashboard.seek_deal_card_for_test(wanted) {
            return false;
        }
        self.deal_view = None;
        self.refresh_dashboard(window, cx);
        true
    }

    #[cfg(test)]
    pub(crate) fn next_deal_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .dashboard
            .skip_current_deal(chrono::Local::now().fixed_offset(), cx)
        {
            self.finish_dashboard_deal_action(window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn previous_deal_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard.previous_deal(cx) {
            self.deal_view = None;
            self.refresh_dashboard(window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn done_deal_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.dashboard.current_deal_card().cloned() else {
            return;
        };
        let handled = match card.identity {
            crate::dashboard::DealCardIdentity::Inbox(id) => {
                self.inbox.verdict(&InboxId(id), Verdict::Discarded).is_ok()
            }
            crate::dashboard::DealCardIdentity::Desk { .. } => self
                .dashboard
                .write_deal_done(chrono::Local::now().date_naive(), cx),
            crate::dashboard::DealCardIdentity::Agent(_) => false,
        };
        if handled {
            self.dashboard.advance_deal(cx);
            self.finish_dashboard_deal_action(window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn snooze_deal_for_test(
        &mut self,
        days: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .dashboard
            .write_deal_snooze(days, chrono::Local::now().date_naive(), cx)
        {
            self.dashboard.advance_deal(cx);
            self.finish_dashboard_deal_action(window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn rendered_deal_card_for_test(
        &self,
    ) -> Option<(
        crate::dashboard::DealCardIdentity,
        crate::dashboard::DealCardKind,
    )> {
        if let Some(view) = &self.deal_view {
            return Some((view.card().0.clone(), view.card().1));
        }
        let card = self.dashboard.current_deal_card()?;
        match card.kind {
            crate::dashboard::DealCardKind::Desk => {}
            crate::dashboard::DealCardKind::Agent if card.agent_id.is_some() => {}
            crate::dashboard::DealCardKind::Inbox(_) => {
                let crate::dashboard::DealCardIdentity::Inbox(id) = &card.identity else {
                    return None;
                };
                if self.inbox.get(&InboxId(id.clone())).is_none() {
                    return None;
                }
            }
            _ => return None,
        }
        Some((card.identity.clone(), card.kind))
    }

    #[cfg(test)]
    pub(crate) fn append_inbox_for_test(&mut self, draft: InboxDraft) -> InboxId {
        self.inbox.append(draft).expect("append test inbox item")
    }

    #[cfg(test)]
    pub(crate) fn age_inbox_for_test(&mut self, id: &InboxId, captured_at_ms: i64) {
        self.inbox.set_captured_at_for_test(id, captured_at_ms);
    }

    #[cfg(test)]
    pub(crate) fn inbox_contains_for_test(&self, id: &InboxId) -> bool {
        self.inbox.get(id).is_some()
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
                crate::dashboard::DealCardKind::Inbox(_) => {
                    self.deal_view = Some(DealView::Inbox {
                        identity: card.identity.clone(),
                        kind: card.kind,
                        editor: self.dashboard.editor().clone(),
                    });
                }
                crate::dashboard::DealCardKind::Desk => {}
            }
        }
        let focus = match self.deal_view.as_ref() {
            Some(DealView::Desk { editor, .. }) | Some(DealView::Inbox { editor, .. }) => {
                editor.focus_handle(cx)
            }
            Some(DealView::Surface { surface, .. }) => match &surface.view {
                SurfaceView::Transcript { editor, .. } => editor.focus_handle(cx),
                #[cfg(feature = "native")]
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

    #[cfg(test)]
    pub(crate) fn dashboard_deal_topic_for_test(&self) -> Option<(HostId, usize, &str)> {
        self.dashboard.current_deal_topic_for_test()
    }

    #[cfg(test)]
    pub(crate) fn dashboard_hint_for_test(&self, cx: &mut Context<Self>) -> String {
        self.dashboard.hint(cx)
    }

    #[cfg(test)]
    pub(crate) fn dashboard_reply_text_for_test(
        &self,
        agent_id: rho_ui_proto::AgentId,
        cx: &App,
    ) -> Option<String> {
        self.dashboard.reply_text_for_test(agent_id, cx)
    }

    #[cfg(test)]
    pub(crate) fn dashboard_cursor_topic_for_test(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(HostId, usize)> {
        self.dashboard.cursor_topic(cx)
    }

    /// The desk half of a quick spawn, without the daemon round-trip:
    /// appends the placeholder heading and writes the tag the daemon
    /// would, binding `agent_id` there. Returns the heading offset.
    #[cfg(test)]
    pub(crate) fn quick_spawn_heading_for_test(
        &mut self,
        host: HostId,
        agent_id: rho_ui_proto::AgentId,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.dashboard.append_placeholder_heading(host, cx)?;
        let buffer = self.desk_sync.buffer(host)?;
        buffer.update(cx, |buffer, cx| {
            let line_end = offset
                + buffer
                    .text_for_range(offset..buffer.len())
                    .collect::<String>()
                    .find('\n')
                    .unwrap_or(buffer.len() - offset);
            let tag = format!(" :eng-{}:", agent_id.encoded());
            buffer.edit([(line_end..line_end, tag)], None, cx);
        });
        Some(offset)
    }

    /// Reconciles the dashboard against the current world. Event-driven,
    /// with no flag to remember: the daemon funnel (`handle_event`),
    /// desk buffer edit subscriptions, draft edit subscriptions, the
    /// editor selection subscription, and the verbs each call this at
    /// their source. The reconcile is idempotent and cheap, so calling
    /// it from several funnels is fine.
    pub(crate) fn refresh_dashboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.inbox.refresh_deferred(now_ms() as i64) {
            tracing::warn!(%error, "waking deferred inbox items");
        }
        self.dashboard.autofill_titles(&self.registry, cx);
        self.dashboard.sync(&self.registry, &self.inbox, window, cx);
        #[cfg(feature = "native")]
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

    #[cfg(feature = "native")]
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

    #[cfg(feature = "native")]
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

    #[cfg(feature = "native")]
    fn browser_page_retained(&self, page: rho_browser::PageId) -> bool {
        self.dashboard.page_ids().contains(&page)
            || self.inbox.items().iter().any(|item| {
                matches!(&item.source, SourceReference::Page { id } if id == &page.to_string())
            })
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
            SurfaceView::Transcript { editor, .. } => editor.clone(),
            SurfaceView::File(view) => view.read(cx).editor().clone(),
            SurfaceView::Shell { editor, .. } => editor.clone(),
            SurfaceView::Diff(view) => view.read(cx).editor().clone(),
            SurfaceView::Terminal(_) => self
                .any_draft_editor()
                .expect("the draft context always holds a draft surface"),
            #[cfg(feature = "native")]
            SurfaceView::Browser(_) => self
                .any_draft_editor()
                .expect("the draft context always holds a draft surface"),
            #[cfg(feature = "native")]
            SurfaceView::ZulipInbox(view) => view.read(cx).editor().clone(),
            #[cfg(feature = "native")]
            SurfaceView::ZulipNarrow(view) => view.read(cx).editor().clone(),
        }
    }

    /// The draft editor, when the active viewport shows the draft.
    fn focused_draft_editor(&self) -> Option<Entity<editor::Editor>> {
        match &self.active_pane().surface.view {
            SurfaceView::Draft { editor, .. } => Some(editor.clone()),
            _ => None,
        }
    }

    /// Some draft editor, from the draft context's surface list (founded at
    /// startup, never pruned). Used only where any editor serves, e.g. text
    /// style for chrome while a terminal surface is focused.
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
        #[cfg(feature = "native")]
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
            SurfaceView::Transcript { editor, .. } => editor.focus_handle(cx),
            SurfaceView::File(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Shell { editor, .. } => editor.focus_handle(cx),
            SurfaceView::Diff(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Terminal(view) => view.read(cx).focus_handle(cx),
            #[cfg(feature = "native")]
            SurfaceView::Browser(view) => view.read(cx).focus_handle(cx),
            #[cfg(feature = "native")]
            SurfaceView::ZulipInbox(view) => view.read(cx).editor().focus_handle(cx),
            #[cfg(feature = "native")]
            SurfaceView::ZulipNarrow(view) => view.read(cx).editor().focus_handle(cx),
        }
    }

    /// Moves gpui focus to the active surface. If a modal overlay
    /// owns the keyboard, update where it will return instead of stealing
    /// focus from it.
    fn focus_active_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let agent_id = match &self.active_pane().surface.key {
            SurfaceKey::Transcript(agent_id)
            | SurfaceKey::Shell(agent_id)
            | SurfaceKey::File { agent_id, .. }
            | SurfaceKey::Diff { agent_id }
            | SurfaceKey::Terminal { agent_id, .. } => Some(*agent_id),
            SurfaceKey::Draft | SurfaceKey::ZulipInbox | SurfaceKey::ZulipNarrow { .. } => None,
            #[cfg(feature = "native")]
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
    fn make_surface(
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
            #[cfg(feature = "native")]
            SurfaceKey::Browser(_) => {
                unreachable!("browser surfaces are created by cmd_browser")
            }
            #[cfg(feature = "native")]
            SurfaceKey::ZulipInbox => {
                let session = self.zulip_session(cx);
                let hooks = Self::zulip_hooks();
                SurfaceView::ZulipInbox(
                    cx.new(|cx| rho_zulip::ui::InboxView::new(session, hooks, window, cx)),
                )
            }
            #[cfg(not(feature = "native"))]
            SurfaceKey::ZulipInbox => unreachable!("the Zulip client is native-only"),
            SurfaceKey::ZulipNarrow { .. } => {
                unreachable!("conversation surfaces are created by open_zulip_narrow")
            }
        };
        Self::wrap_surface(key, view)
    }

    fn wrap_surface(key: SurfaceKey, view: SurfaceView) -> Surface {
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
            #[cfg(feature = "native")]
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
            SurfaceKey::File { .. } | SurfaceKey::ZulipInbox | SurfaceKey::ZulipNarrow { .. } => {
                None
            }
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
        minibuffer.accept_selected(window, cx);
        let prompt = minibuffer.prompt().to_owned();
        let (input, on_submit) = minibuffer.into_submission(cx);
        crate::journal::record(crate::journal::Event::MinibufferSubmitted {
            prompt,
            input: input.clone(),
        });
        self.finish_overlay_focus(window, cx);
        on_submit(self, input, window, cx);
        cx.notify();
    }

    #[cfg(feature = "native")]
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

    #[cfg(feature = "native")]
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
        self.minibuffer.is_some() || self.transient.is_some() || {
            #[cfg(feature = "native")]
            {
                self.pending_git_approval.is_some()
            }
            #[cfg(not(feature = "native"))]
            {
                false
            }
        }
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

    pub(crate) fn prompt_snooze(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &gpui::App| Vec::new());
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let input = input.trim();
                if input.is_empty() {
                    return;
                }
                match parse_duration_ms(input) {
                    Some(duration_ms) => workspace.cmd_agent_snooze(duration_ms, window, cx),
                    None => workspace.notice_on(
                        None,
                        &format!("snooze: bad duration `{input}` (30m, 2h, 1d)"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            },
        );
        self.open_prompt("snooze (30m/2h/1d):", complete, on_submit, window, cx);
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

    pub(crate) fn cmd_toggle_raw_desk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dashboard.toggle_raw_mode(cx);
        crate::journal::record(crate::journal::Event::DeskRawModeToggled {
            enabled: self.dashboard.raw_mode(),
        });
        self.refresh_dashboard(window, cx);
    }

    pub(crate) fn open_deal_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard.deal_mode() {
            return;
        }
        self.deal_origin = Some(DealOrigin {
            active_context: self.active_context,
            surface: self.active_pane().surface.clone(),
            current_room: self.current_room.clone(),
            overview_open: self.overview_open,
        });
        self.deal_current_taken = false;
        self.dismiss_mru_overlay(cx);
        self.deal_view = None;
        self.deal_hints_visible = false;
        self.deal_controls_visible = false;
        window.focus(&self.dashboard.focus_handle(cx), cx);
        let now = chrono::Local::now().fixed_offset();
        if let Err(error) = self.inbox.refresh_deferred(now.timestamp_millis()) {
            tracing::warn!(%error, "waking deferred inbox items");
        }
        let current_room = self
            .current_room
            .as_ref()
            .and_then(|room| Some((room.host, self.room_names.get(room)?.as_str())));
        self.dashboard.enter_deal_mode_in_room(
            &self.registry,
            &self.inbox,
            now,
            current_room,
            &self.agent_last_interaction,
            cx,
        );
        if self.dashboard.deal_mode() {
            if let Ok(action) = cx.build_action("vim::EnterDealMode", None) {
                window.dispatch_action(action, cx);
            }
            self.journal_deal_card_presented();
            crate::journal::record(crate::journal::Event::DealMode {
                action: crate::journal::DealModeAction::Enter,
                card: self
                    .dashboard
                    .current_deal_card()
                    .map(|card| Self::journal_card_identity(&card.identity)),
            });
            self.deal_session_open = true;
        }
        self.refresh_dashboard(window, cx);
        cx.notify();
    }

    fn journal_deal_card_presented(&self) {
        let Some(card) = self.dashboard.current_deal_card() else {
            return;
        };
        crate::journal::record(crate::journal::Event::DealCardPresented {
            label: card.label.clone(),
            kind: match card.kind {
                crate::dashboard::DealCardKind::Desk => "heading",
                crate::dashboard::DealCardKind::Agent => "agent",
                crate::dashboard::DealCardKind::Inbox(_) => "inbox",
            }
            .to_owned(),
            score: card.priority,
            room: card.breadcrumb.split(" › ").next().unwrap_or("").to_owned(),
            host: card.host.to_string(),
            agent_id: card.agent_id.map(|id| id.encoded()),
        });
    }

    fn end_deal_session(&mut self) {
        if std::mem::take(&mut self.deal_session_open) {
            crate::journal::record(crate::journal::Event::DealMode {
                action: crate::journal::DealModeAction::Exit,
                card: None,
            });
        }
    }

    fn restore_deal_origin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(origin) = self.deal_origin.take() else {
            return;
        };
        self.active_context = origin.active_context;
        self.active_pane_mut().show(origin.surface);
        self.current_room = origin.current_room;
        self.overview_open = origin.overview_open;
        if self.overview_open {
            window.focus(&self.dashboard.focus_handle(cx), cx);
        } else {
            let focus = self.active_surface_focus(cx);
            window.focus(&focus, cx);
        }
    }

    /// Commits the provisional card without ending the visual deal. The
    /// ordinary open paths deliberately own strip, room, MRU, and journal
    /// side effects; merely rendering a card never calls this.
    fn take_current_deal(
        &mut self,
        edit_agent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.deal_current_taken {
            return self.dashboard.current_deal_card().is_some();
        }
        let Some(card) = self.dashboard.current_deal_card().cloned() else {
            return false;
        };
        self.dashboard.record_deal_verdict_as(
            crate::dashboard::DealerVerdict::Open,
            chrono::Local::now().fixed_offset(),
        );
        self.deal_current_taken = true;
        match card.identity {
            crate::dashboard::DealCardIdentity::Desk {
                host,
                heading_offset,
            } => {
                if let Some(room) = self.dashboard.room_for_heading(host, heading_offset, cx) {
                    self.remember_room(room, crate::journal::RoomSwitchMethod::Open, cx);
                }
                self.dashboard.cursor_to_doc(host, heading_offset, cx);
                self.overview_open = true;
                window.focus(&self.dashboard.focus_handle(cx), cx);
            }
            crate::dashboard::DealCardIdentity::Agent(agent_id) => {
                crate::journal::record(crate::journal::Event::AgentOpened {
                    agent_id: agent_id.encoded(),
                });
                if let Some(room) = self.room_for_agent(agent_id, cx) {
                    self.remember_room(room, crate::journal::RoomSwitchMethod::Open, cx);
                }
                if let Some(DealView::Surface { surface, .. }) = self.deal_view.as_ref() {
                    let surface = surface.clone();
                    if edit_agent && let SurfaceView::Transcript { model, editor } = &surface.view {
                        model.update(cx, |model, cx| model.focus_prompt(editor, window, cx));
                        vim::enter_deal_insert_mode(editor, window, cx);
                    }
                    self.registry.select_agent(agent_id);
                    self.active_context = self.context_for_agent(agent_id);
                    self.overview_open = false;
                    self.display_surface(surface);
                    self.focus_active_surface(window, cx);
                    self.hosts
                        .focus_agent(self.host_of(agent_id).map(|host| (host, agent_id)));
                }
            }
            crate::dashboard::DealCardIdentity::Inbox(_) => {
                #[cfg(feature = "native")]
                if let Some(crate::dashboard::DealerInboxSource::Page(page)) = card.inbox_source {
                    self.open_browser_page(page, window, cx);
                }
            }
        }
        true
    }

    fn present_next_deal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dashboard.relock_deal_sources(cx);
        if let Ok(action) = cx.build_action("vim::ExitDealMode", None) {
            window.dispatch_action(action, cx);
        }
        self.deal_view = None;
        self.deal_current_taken = false;
        self.deal_hints_visible = false;
        self.deal_controls_visible = false;
        self.journal_deal_card_presented();
        self.refresh_dashboard(window, cx);
    }

    fn stop_dealing_and_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dashboard.exit_deal_mode(cx);
        self.end_deal_session();
        if let Ok(action) = cx.build_action("vim::ExitDealMode", None) {
            window.dispatch_action(action, cx);
        }
        self.deal_view = None;
        self.deal_current_taken = false;
        self.restore_deal_origin(window, cx);
        self.refresh_dashboard(window, cx);
    }

    fn take_and_stop_dealing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dashboard.deal_mode() || !self.take_current_deal(false, window, cx) {
            return;
        }
        self.dashboard.exit_deal_mode(cx);
        self.end_deal_session();
        self.deal_origin = None;
        if let Ok(action) = cx.build_action("vim::ExitDealMode", None) {
            window.dispatch_action(action, cx);
        }
        self.deal_view = None;
    }

    fn finish_dashboard_deal_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.deal_view = None;
        self.deal_hints_visible = false;
        self.deal_controls_visible = false;
        if !self.dashboard.deal_mode()
            && let Ok(action) = cx.build_action("vim::ExitDealMode", None)
        {
            window.dispatch_action(action, cx);
        }
        if !self.dashboard.deal_mode() {
            self.end_deal_session();
            self.restore_deal_origin(window, cx);
        } else {
            self.deal_current_taken = false;
        }
        self.refresh_dashboard(window, cx);
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

    pub(crate) fn configure_dashboard_staff(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(topic) = self.dashboard.cursor_topic(cx) else {
            self.notice_on(
                None,
                "staff: choose a Desk heading",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.begin_new_agent_configuration(NewAgentIntent::Staff(topic), window, cx);
    }

    pub(crate) fn configure_dashboard_quick_spawn(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_new_agent_configuration(NewAgentIntent::QuickSpawn, window, cx);
    }

    fn begin_new_agent_configuration(
        &mut self,
        intent: NewAgentIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workdir = match intent {
            NewAgentIntent::Staff(topic) => self.workdir_for_desk_topic(topic, cx),
            NewAgentIntent::QuickSpawn => {
                let row_workdir = match self.dashboard.cursor_target(&self.registry, cx) {
                    Some(crate::dashboard::RowTarget::Agent { agent_id, .. }) => {
                        self.agent_workdir(agent_id)
                    }
                    _ => self
                        .dashboard
                        .cursor_topic(cx)
                        .and_then(|topic| self.workdir_for_desk_topic(topic, cx)),
                };
                row_workdir.or_else(|| {
                    self.registry
                        .selected_agent()
                        .copied()
                        .and_then(|agent_id| self.agent_workdir(agent_id))
                })
            }
        }
        .or_else(|| match self.workdirs.as_slice() {
            [workdir] => Some(HostPath {
                host: workdir.host,
                path: workdir.project.path.clone(),
            }),
            _ => None,
        });
        let host = match intent {
            NewAgentIntent::Staff((host, _)) => Some(host),
            NewAgentIntent::QuickSpawn => workdir
                .as_ref()
                .map(|workdir| workdir.host)
                .or_else(|| self.hosts.primary()),
        };
        self.new_agent_draft = Some(NewAgentDraft {
            intent,
            host,
            workdir,
            workspace: DraftWorkspace::NewOn(DraftBase::Auto),
            role: crate::draft_view::DEFAULT_ROLE.to_owned(),
        });
        self.open_new_agent_transient(window, cx);
    }

    fn workdir_for_desk_topic(
        &self,
        topic: (HostId, usize),
        cx: &mut Context<Self>,
    ) -> Option<HostPath> {
        let (host, _, _, project) = self.dashboard.staffing_target_for(topic, cx).ok()?;
        let candidates = self
            .workdirs
            .iter()
            .filter(|workdir| workdir.host == host)
            .collect::<Vec<_>>();
        let workdir = match project {
            Some(project) => candidates.iter().find(|workdir| {
                workdir.project.name == project || workdir.project.path.as_str() == project
            })?,
            None if candidates.len() == 1 => candidates[0],
            None => return None,
        };
        Some(HostPath {
            host,
            path: workdir.project.path.clone(),
        })
    }

    pub(crate) fn open_new_agent_transient(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_agent_draft.is_none() {
            // A new agent starts where the subject works.
            let subject = self.subject(window, cx);
            let contextual = subject
                .agent
                .and_then(|agent_id| self.agent_workdir(agent_id));
            let workdir = contextual.or_else(|| match self.workdirs.as_slice() {
                [workdir] => Some(HostPath {
                    host: workdir.host,
                    path: workdir.project.path.clone(),
                }),
                _ => None,
            });
            self.new_agent_draft = Some(NewAgentDraft {
                intent: NewAgentIntent::QuickSpawn,
                host: workdir
                    .as_ref()
                    .map(|workdir| workdir.host)
                    .or_else(|| self.hosts.primary()),
                workdir,
                workspace: DraftWorkspace::NewOn(DraftBase::Auto),
                role: crate::draft_view::DEFAULT_ROLE.to_owned(),
            });
        }
        let draft = self.new_agent_draft.as_ref().expect("draft initialized");
        let host = draft
            .host
            .map(|host| self.host_label(host))
            .unwrap_or_else(|| "<choose>".to_owned());
        let project = draft
            .workdir
            .as_ref()
            .map(|workdir| self.workdir_label(workdir))
            .unwrap_or_else(|| "<choose>".to_owned());
        self.open_transient(
            crate::transient::new_agent_menu(
                host,
                project,
                draft.workspace.label(),
                draft.role.clone(),
                draft.intent.compose_label(),
            ),
            window,
            cx,
        );
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

    pub(crate) fn prompt_new_agent_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_lowercase();
            let selected_host = workspace
                .new_agent_draft
                .as_ref()
                .and_then(|draft| draft.host);
            workspace
                .workdirs
                .iter()
                .filter(|workdir| selected_host.is_none_or(|host| workdir.host == host))
                .map(|workdir| {
                    (
                        workdir.project.name.clone(),
                        workdir.project.path.to_string(),
                    )
                })
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
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let input = input.trim();
                if !input.is_empty() {
                    let selected_host = workspace
                        .new_agent_draft
                        .as_ref()
                        .and_then(|draft| draft.host);
                    let resolved = match selected_host {
                        Some(host) => workspace.resolve_workdir_on_host(input, host),
                        None => workspace.resolve_workdir(input),
                    };
                    match resolved {
                        Ok(workdir) => {
                            if let Some(draft) = &mut workspace.new_agent_draft {
                                draft.host = Some(workdir.host);
                                draft.workdir = Some(workdir);
                            }
                        }
                        Err(message) => {
                            workspace.notice_on(None, &message, StyleClass::SystemInfo, cx)
                        }
                    }
                }
                workspace.open_new_agent_transient(window, cx);
            },
        );
        self.open_prompt("project:", complete, on_submit, window, cx);
    }

    pub(crate) fn open_new_agent_workspace_transient(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_transient(crate::transient::new_agent_workspace_menu(), window, cx);
    }

    pub(crate) fn prompt_new_agent_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    if let Some(draft) = &mut workspace.new_agent_draft
                        && draft.host != Some(host)
                    {
                        draft.host = Some(host);
                        draft.workdir = None;
                    }
                } else if !name.is_empty() {
                    workspace.notice_on(
                        None,
                        &format!("no attached host named `{name}`"),
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
                workspace.open_new_agent_transient(window, cx);
            },
        );
        self.open_prompt("host:", complete, on_submit, window, cx);
    }

    pub(crate) fn prompt_new_agent_workspace(
        &mut self,
        mode: crate::draft_view::StartFieldMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::draft_view::StartFieldMode;

        let complete =
            std::rc::Rc::new(move |workspace: &Workspace, input: &str, _: &gpui::App| {
                let needle = input.trim().to_lowercase();
                let mut candidates = workspace.live_agent_targets();
                candidates.insert(
                    0,
                    if mode == StartFieldMode::Join {
                        crate::commands::Candidate {
                            value: "user".to_owned(),
                            description: "your checkout".to_owned(),
                        }
                    } else {
                        crate::commands::Candidate {
                            value: crate::draft_view::DEFAULT_START.to_owned(),
                            description: "local main → local master → trunk".to_owned(),
                        }
                    },
                );
                candidates
                    .into_iter()
                    .filter(|candidate| {
                        candidate.value.to_lowercase().contains(&needle)
                            || candidate.description.to_lowercase().contains(&needle)
                    })
                    .collect()
            });
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let input = input.trim();
                if !input.is_empty()
                    && let Some(draft) = &mut workspace.new_agent_draft
                {
                    draft.workspace = match mode {
                        StartFieldMode::NewOn => {
                            DraftWorkspace::NewOn(DraftBase::from_input(input))
                        }
                        StartFieldMode::Join => DraftWorkspace::Join(input.to_owned()),
                        StartFieldMode::Sandbox => {
                            DraftWorkspace::Sandbox(DraftBase::from_input(input))
                        }
                    };
                }
                workspace.open_new_agent_transient(window, cx);
            },
        );
        let prompt = match mode {
            StartFieldMode::NewOn => "new workspace on:",
            StartFieldMode::Join => "join workspace:",
            StartFieldMode::Sandbox => "sandbox on:",
        };
        self.open_prompt(prompt, complete, on_submit, window, cx);
    }

    pub(crate) fn cycle_new_agent_role(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.new_agent_draft {
            draft.role = cycle_agent_role_text(&draft.role).to_owned();
        }
        self.open_new_agent_transient(window, cx);
    }

    pub(crate) fn compose_configured_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(intent) = self.new_agent_draft.as_ref().map(|draft| draft.intent) else {
            return;
        };
        self.dashboard.open_new_draft(intent.topic(), window, cx);
        self.dashboard_focus_draft(window, cx);
    }

    fn configured_agent_launch(
        &self,
    ) -> Result<(HostId, rho_ui_proto::StartMode, AgentRole), String> {
        use crate::draft_view::StartFieldMode;

        let draft = self
            .new_agent_draft
            .as_ref()
            .ok_or_else(|| "new agent has no launch configuration".to_owned())?;
        let (mode, target) = match &draft.workspace {
            DraftWorkspace::NewOn(base) => (StartFieldMode::NewOn, base.target()),
            DraftWorkspace::Join(target) => (StartFieldMode::Join, target.as_str()),
            DraftWorkspace::Sandbox(base) => (StartFieldMode::Sandbox, base.target()),
        };
        let selected_host = match draft.intent {
            NewAgentIntent::Staff((host, _)) => Some(host),
            NewAgentIntent::QuickSpawn => draft.host,
        };
        let (host, start) = self.parse_start(mode, target, draft.workdir.clone(), selected_host)?;
        let role = parse_agent_role(&draft.role)?;
        Ok((host, start, role))
    }

    fn submit_configured_agent(
        &mut self,
        topic: Option<(HostId, usize)>,
        body: String,
        host: HostId,
        start: rho_ui_proto::StartMode,
        role: AgentRole,
        cx: &mut Context<Self>,
    ) {
        let desk_anchor = match topic {
            Some((topic_host, offset)) => {
                debug_assert_eq!(host, topic_host);
                let repo = match &start {
                    rho_ui_proto::StartMode::NewOn { repo, .. }
                    | rho_ui_proto::StartMode::Sandbox { repo, .. }
                    | rho_ui_proto::StartMode::Join(rho_ui_proto::JoinTarget::User { repo }) => {
                        repo.as_path()
                    }
                    rho_ui_proto::StartMode::Join(rho_ui_proto::JoinTarget::Workspace(info)) => {
                        info.repo()
                    }
                };
                if let Some(project) = self.workdirs.iter().find(|candidate| {
                    candidate.host == host && candidate.project.path.as_path() == repo
                }) {
                    self.dashboard.set_heading_project(
                        topic_host,
                        offset,
                        &project.project.name,
                        cx,
                    );
                }
                self.dashboard.cursor_to_doc(topic_host, offset, cx);
                self.desk_sync.anchor_at(topic_host, offset, cx)
            }
            None => self
                .dashboard
                .append_placeholder_heading(host, cx)
                .and_then(|offset| {
                    self.dashboard.cursor_to_doc(host, offset, cx);
                    self.desk_sync.anchor_at(host, offset, cx)
                }),
        };
        self.send_to_host(
            host,
            ClientMessage::NewAgent {
                role,
                start,
                content: Some(vec![ContentPart::Text { text: body }]),
                desk_anchor,
            },
        );
        self.new_agent_draft = None;
    }

    /// `enter` on a bound Desk heading opens its agent.
    fn dashboard_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        if let Some(room) = self.dashboard.cursor_room(cx) {
            self.remember_room(room, crate::journal::RoomSwitchMethod::Overview, cx);
        }
        match self.dashboard.cursor_target(&self.registry, cx) {
            Some(RowTarget::Agent { agent_id, .. }) => self.open_agent(agent_id, window, cx),
            #[cfg(feature = "native")]
            Some(RowTarget::Page(id)) => self.open_browser_page(id, window, cx),
            // A staffed heading opens its top agent full-frame — loudest
            // first, quiet agents still reachable. An unstaffed heading opens
            // its first-message draft rather than a blank composer.
            Some(RowTarget::Topic {
                host,
                offset,
                first_attention,
                on_heading_line,
                ..
            }) => {
                if let Some(agent_id) =
                    first_attention.or_else(|| self.dashboard.first_agent_for_topic((host, offset)))
                {
                    self.open_agent(agent_id, window, cx);
                } else if on_heading_line {
                    self.dashboard
                        .open_new_draft(Some((host, offset)), window, cx);
                    self.dashboard_focus_draft(window, cx);
                } else {
                    cx.propagate();
                }
            }
            Some(RowTarget::Reply(agent_id)) => {
                if !self.require_connected(cx) {
                    return;
                }
                if let Some(text) = self.dashboard.take_reply(agent_id, cx) {
                    self.handle_submit(agent_id, vec![ContentPart::Text { text }], cx);
                }
                self.dashboard.cursor_to_agent(agent_id, cx);
                self.refresh_dashboard(window, cx);
            }
            Some(RowTarget::NewDraft(topic)) => {
                if !self.require_connected(cx) {
                    return;
                }
                let configured = self
                    .new_agent_draft
                    .as_ref()
                    .is_some_and(|draft| draft.intent.topic() == topic);
                if configured {
                    let (host, start, role) = match self.configured_agent_launch() {
                        Ok(launch) => launch,
                        Err(message) => {
                            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                            return;
                        }
                    };
                    if let Some(body) = self.dashboard.take_new_draft(cx) {
                        self.submit_configured_agent(topic, body, host, start, role, cx);
                    }
                    self.refresh_dashboard(window, cx);
                    return;
                }
                if let Some(body) = self.dashboard.take_new_draft(cx) {
                    if topic.is_some() {
                        self.staff_dashboard_node_with_brief(topic, Some(body), window, cx);
                    } else {
                        self.spawn_unfiled_dashboard_agent(body, window, cx);
                    }
                }
                self.refresh_dashboard(window, cx);
            }
            // Document text keeps vim's own enter.
            _ => cx.propagate(),
        }
    }

    pub(crate) fn set_universal_argument(&mut self, cx: &mut Context<Self>) {
        self.universal_argument = true;
        cx.notify();
    }

    fn take_universal_argument(&mut self) -> bool {
        std::mem::take(&mut self.universal_argument)
    }

    /// Insert-mode enter: send when the cursor is in a draft, and drop
    /// back to normal mode on the row the send leaves behind. In document
    /// text it is a newline — dispatched explicitly, because propagating
    /// would fall through to the transcript prompt's `RhoGui > Editor`
    /// SubmitPrompt binding, which swallows the key.
    fn dashboard_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        match self.dashboard.cursor_target(&self.registry, cx) {
            Some(RowTarget::Reply(_)) | Some(RowTarget::NewDraft(_)) => {
                self.dashboard_open(window, cx);
                if let Ok(action) = cx.build_action("vim::NormalBefore", None) {
                    window.dispatch_action(action, cx);
                }
            }
            _ => {
                if let Ok(action) = cx.build_action("editor::Newline", None) {
                    window.dispatch_action(action, cx);
                }
            }
        }
    }

    fn dashboard_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_agent_draft = None;
        match self.dashboard.cursor_target(&self.registry, cx) {
            Some(crate::dashboard::RowTarget::Agent { agent_id, .. })
            | Some(crate::dashboard::RowTarget::Reply(agent_id)) => {
                self.dashboard.open_reply(agent_id, window, cx);
                self.dashboard_focus_draft(window, cx);
            }
            // On a heading line, `r` is the one talking verb: a reply to
            // the heading's top agent when it's staffed, a first-message
            // draft (which spawns an agent on send) when it isn't.
            Some(crate::dashboard::RowTarget::Topic {
                host,
                offset,
                first_attention,
                on_heading_line: true,
                ..
            }) => {
                match first_attention
                    .or_else(|| self.dashboard.first_agent_for_topic((host, offset)))
                {
                    Some(agent_id) => self.dashboard.open_reply(agent_id, window, cx),
                    None => self
                        .dashboard
                        .open_new_draft(Some((host, offset)), window, cx),
                }
                self.dashboard_focus_draft(window, cx);
            }
            // Document text keeps vim's own `r`.
            _ => cx.propagate(),
        }
    }

    fn dashboard_enter_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(action) = cx.build_action("vim::InsertBefore", None) {
            window.dispatch_action(action, cx);
        }
    }

    /// A freshly opened draft row only exists on screen after a sync:
    /// splice it in now so the pending cursor lands on it, then enter
    /// insert there — never on the read-only row the cursor came from.
    fn dashboard_focus_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_dashboard(window, cx);
        self.dashboard_enter_insert(window, cx);
    }

    /// A quick spawn has no heading to inherit `:project:` from, so the
    /// project comes from a picker (skipped when only one is registered).
    fn spawn_unfiled_dashboard_agent(
        &mut self,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workdirs.is_empty() {
            self.notice_on(
                None,
                "new agent: no registered projects",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        if let [workdir] = self.workdirs.as_slice() {
            let workdir = workdir.clone();
            self.quick_spawn_on(workdir, body, cx);
            return;
        }
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_lowercase();
            let multiple_hosts = workspace
                .workdirs
                .iter()
                .map(|workdir| workdir.host)
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1;
            workspace
                .workdirs
                .iter()
                .filter(|candidate| {
                    candidate.project.name.to_lowercase().contains(&needle)
                        || candidate
                            .project
                            .path
                            .as_str()
                            .to_lowercase()
                            .contains(&needle)
                })
                .map(|candidate| crate::commands::Candidate {
                    value: candidate.project.name.clone(),
                    description: if multiple_hosts {
                        format!(
                            "{} · {}",
                            workspace.registry.host_name(candidate.host),
                            candidate.project.path
                        )
                    } else {
                        candidate.project.path.to_string()
                    },
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  _window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let input = input.trim();
                let Some(workdir) = workspace
                    .workdirs
                    .iter()
                    .find(|candidate| {
                        candidate.project.name == input || candidate.project.path == input
                    })
                    .cloned()
                else {
                    workspace.notice_on(
                        None,
                        "new agent: choose a listed project",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                };
                workspace.quick_spawn_on(workdir, body.clone(), cx);
            },
        );
        self.open_prompt("project:", complete, on_submit, window, cx);
    }

    /// Quick spawn (`shift-r`): the desk gets a `* …` placeholder heading,
    /// the agent spawns bound to it, and the title fills itself in from
    /// the agent's generated summary once one exists. Hosts without a
    /// desk fall back to an unfiled spawn.
    fn quick_spawn_on(&mut self, workdir: HostProject, body: String, cx: &mut Context<Self>) {
        let host = workdir.host;
        let Some(offset) = self.dashboard.append_placeholder_heading(host, cx) else {
            self.spawn_unfiled_agent_on(workdir, body, cx);
            return;
        };
        // The draft row the send came from is about to vanish; the new
        // heading is where the cursor belongs.
        self.dashboard.cursor_to_doc(host, offset, cx);
        self.spawn_dashboard_agent(host, offset, body, workdir, cx);
    }

    fn spawn_unfiled_agent_on(
        &mut self,
        workdir: HostProject,
        body: String,
        cx: &mut Context<Self>,
    ) {
        let role_text = self.draft_model.read(cx).role_text(cx).trim().to_owned();
        let role = match parse_agent_role(&role_text) {
            Ok(role) => role,
            Err(message) => {
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                return;
            }
        };
        self.send_to_host(
            workdir.host,
            ClientMessage::NewAgent {
                role,
                start: rho_ui_proto::StartMode::NewOn {
                    repo: workdir.project.path,
                    revset: crate::draft_view::AUTO_BASE_REVSET.to_owned(),
                },
                content: (!body.trim().is_empty())
                    .then_some(vec![ContentPart::Text { text: body }]),
                desk_anchor: None,
            },
        );
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
        let _ = above;
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                let title = input.trim();
                if !title.is_empty() {
                    workspace.dashboard.append_topic(title, cx);
                    workspace.refresh_dashboard(_window, cx);
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

    #[cfg(all(target_family = "wasm", not(feature = "native")))]
    fn dashboard_open_clicked_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dashboard_open(window, cx);
    }

    fn staff_dashboard_node(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_agent_draft = None;
        let Some(topic) = self.dashboard.cursor_topic(cx) else {
            self.notice_on(None, "staff: choose a topic", StyleClass::SystemInfo, cx);
            return;
        };
        self.dashboard.open_new_draft(Some(topic), window, cx);
        self.dashboard_focus_draft(window, cx);
    }

    fn staff_dashboard_node_with_brief(
        &mut self,
        topic: Option<(HostId, usize)>,
        brief: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(topic) = topic else {
            self.notice_on(None, "staff: choose a topic", StyleClass::SystemInfo, cx);
            return;
        };
        let (host, heading_offset, mut text, project) =
            match self.dashboard.staffing_target_for(topic, cx) {
                Ok(target) => target,
                Err(message) => {
                    self.notice_on(None, message, StyleClass::SystemInfo, cx);
                    return;
                }
            };
        if let Some(brief) = brief {
            text = brief;
        }
        if let Some(crate::dashboard::RowTarget::Agent { agent_id, .. }) =
            self.dashboard.cursor_target(&self.registry, cx)
            && matches!(
                self.registry.agent_disposition(agent_id),
                Some(
                    rho_ui_proto::AgentDisposition::Pending
                        | rho_ui_proto::AgentDisposition::Snoozed { .. }
                )
            )
        {
            self.send_to_host(
                host,
                ClientMessage::SendUserMessage {
                    agent_id,
                    content: vec![ContentPart::Text {
                        text: updated_desk_brief(&text),
                    }],
                    delivery: rho_ui_proto::MessageDelivery::NextRequest,
                },
            );
            self.registry.touch_agent(agent_id);
            self.mark_agent_prompt_sent(agent_id, cx);
            self.notice_on(
                Some(&agent_id),
                &format!(
                    "updated brief sent to {}",
                    self.registry.agent_id_label(agent_id)
                ),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }

        let projects = self
            .workdirs
            .iter()
            .filter(|workdir| workdir.host == host)
            .cloned()
            .collect::<Vec<_>>();
        match resolve_desk_project(project.as_deref(), &projects) {
            DeskProjectResolution::Use(index) => {
                self.spawn_dashboard_agent(host, heading_offset, text, projects[index].clone(), cx)
            }
            DeskProjectResolution::Missing => self.notice_on(
                None,
                project
                    .as_deref()
                    .map_or(
                        "staff: this host has no registered projects".to_owned(),
                        |project| format!("staff: no project `{project}` on this host"),
                    )
                    .as_str(),
                StyleClass::SystemInfo,
                cx,
            ),
            DeskProjectResolution::Choose => {
                let complete =
                    std::rc::Rc::new(move |workspace: &Workspace, input: &str, _: &gpui::App| {
                        let needle = input.trim().to_lowercase();
                        workspace
                            .workdirs
                            .iter()
                            .filter(|candidate| candidate.host == host)
                            .filter(|candidate| {
                                candidate.project.name.to_lowercase().contains(&needle)
                                    || candidate
                                        .project
                                        .path
                                        .as_str()
                                        .to_lowercase()
                                        .contains(&needle)
                            })
                            .map(|candidate| crate::commands::Candidate {
                                value: candidate.project.name.clone(),
                                description: candidate.project.path.to_string(),
                            })
                            .collect()
                    });
                let on_submit = std::rc::Rc::new(
                    move |workspace: &mut Workspace,
                          input: String,
                          _window: &mut Window,
                          cx: &mut Context<Workspace>| {
                        let input = input.trim();
                        let Some(workdir) = workspace
                            .workdirs
                            .iter()
                            .find(|candidate| {
                                candidate.host == host
                                    && (candidate.project.name == input
                                        || candidate.project.path == input)
                            })
                            .cloned()
                        else {
                            workspace.notice_on(
                                None,
                                "staff: choose a listed project",
                                StyleClass::SystemInfo,
                                cx,
                            );
                            return;
                        };
                        workspace.dashboard.set_heading_project(
                            host,
                            heading_offset,
                            &workdir.project.name,
                            cx,
                        );
                        workspace.spawn_dashboard_agent(
                            host,
                            heading_offset,
                            text.clone(),
                            workdir,
                            cx,
                        );
                    },
                );
                self.open_prompt("Desk project:", complete, on_submit, window, cx);
            }
        }
    }

    fn spawn_dashboard_agent(
        &mut self,
        host: HostId,
        heading_offset: usize,
        text: String,
        workdir: HostProject,
        cx: &mut Context<Self>,
    ) {
        let role_text = self.draft_model.read(cx).role_text(cx).trim().to_owned();
        let role = match parse_agent_role(&role_text) {
            Ok(role) => role,
            Err(message) => {
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                return;
            }
        };
        self.send_to_host(
            host,
            ClientMessage::NewAgent {
                role,
                start: rho_ui_proto::StartMode::NewOn {
                    repo: workdir.project.path,
                    revset: crate::draft_view::AUTO_BASE_REVSET.to_owned(),
                },
                content: (!text.trim().is_empty()).then_some(vec![ContentPart::Text { text }]),
                desk_anchor: self.desk_sync.anchor_at(host, heading_offset, cx),
            },
        );
    }

    fn dashboard_structure_move(
        &mut self,
        direction: crate::dashboard::StructureDirection,
        cx: &mut Context<Self>,
    ) {
        self.dashboard.structure_move(direction, cx);
    }

    fn dashboard_delete_empty(&mut self, cx: &mut Context<Self>) {
        if !self.dashboard.delete_empty(cx) {
            self.notice_on(
                None,
                "delete: heading is not empty",
                StyleClass::SystemInfo,
                cx,
            );
        }
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
        self.open_prompt("Desk heading:", complete, on_submit, window, cx);
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
        let container = container
            // The dashboard owns the preview card's reclaimed horizontal
            // space, rather than leaving a blank wrapper beside the card.
            .w(if show_preview {
                gpui::relative(0.55)
            } else {
                gpui::relative(1.0)
            })
            // The desktop gutter wastes too much of a phone's width.
            .pl(px(if self.phone.enabled { 6. } else { 24. }))
            .pr(px(if self.phone.enabled { 6. } else { 24. }));
        let dashboard = div()
            .id("dashboard-rail")
            .flex_grow(1.0)
            .min_h_0()
            .relative()
            .overflow_hidden()
            .child(self.dashboard.editor().clone());
        #[cfg(all(target_family = "wasm", not(feature = "native")))]
        let dashboard = dashboard
            // The editor consumes bubble-phase mouse events. Capture the
            // press/release around it, then open after its cursor has moved.
            .capture_any_mouse_down(cx.listener(Self::dashboard_pointer_down))
            .capture_any_mouse_up(cx.listener(Self::dashboard_pointer_up));
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
        #[cfg(feature = "native")]
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
                        kind: card.kind,
                        surface,
                    }
                })
            } else {
                let page = None;
                page.or_else(|| {
                    let crate::dashboard::DealCardIdentity::Inbox(id) = &card.identity else {
                        return None;
                    };
                    let item = self.inbox.get(&InboxId(id.clone()))?;
                    let mut text = item.text.clone();
                    let context = [
                        item.context
                            .room
                            .as_deref()
                            .map(|value| format!("room: {value}")),
                        item.context
                            .host
                            .as_deref()
                            .map(|value| format!("host: {value}")),
                        (!item.context.focused_surface.is_empty())
                            .then(|| format!("surface: {}", item.context.focused_surface)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                    if !context.is_empty() {
                        text.push_str(
                            "

",
                        );
                        text.push_str(&context.join(
                            "
",
                        ));
                    }
                    let buffer = cx.new(|cx| {
                        let mut buffer = language::Buffer::local(text, cx);
                        buffer.set_capability(language::Capability::Read, cx);
                        buffer
                    });
                    let editor = cx.new(|cx| {
                        let mut editor = editor::Editor::for_buffer(buffer, None, window, cx);
                        crate::editor_config::configure(&mut editor, window, cx);
                        editor.set_read_only(true);
                        editor
                    });
                    Some(DealView::Inbox {
                        identity: card.identity.clone(),
                        kind: card.kind,
                        editor,
                    })
                })
            };
            self.deal_view = view;
        }

        if created {
            let focus = match self.deal_view.as_ref() {
                Some(DealView::Desk { editor, .. }) | Some(DealView::Inbox { editor, .. }) => {
                    Some(editor.focus_handle(cx))
                }
                Some(DealView::Surface { surface, .. }) => match &surface.view {
                    SurfaceView::Transcript { editor, .. } => Some(editor.focus_handle(cx)),
                    _ => None,
                },
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

        match self.deal_view.as_ref() {
            Some(DealView::Desk { editor, .. }) => div()
                .size_full()
                .key_context("RhoDashboard")
                .overflow_hidden()
                .child(editor.clone())
                .into_any_element(),
            Some(DealView::Surface { kind, surface, .. }) => {
                debug_assert!(matches!(
                    kind,
                    crate::dashboard::DealCardKind::Agent
                        | crate::dashboard::DealCardKind::Inbox(_)
                ));
                self.render_surface(surface)
            }
            Some(DealView::Inbox { kind, editor, .. }) => {
                debug_assert!(matches!(kind, crate::dashboard::DealCardKind::Inbox(_)));
                div()
                    .size_full()
                    .overflow_hidden()
                    .child(editor.clone())
                    .into_any_element()
            }
            None => div().size_full().into_any_element(),
        }
    }

    fn render_deal_why(
        &self,
        card: &crate::dashboard::DealCard,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let path = match &card.inbox_source {
            Some(crate::dashboard::DealerInboxSource::Page(page)) => {
                let leaf = rho_browser::live_page_name(*page).unwrap_or_else(|| "page".to_owned());
                format!("{} / {leaf}", card.breadcrumb.replace(" › ", " / "))
            }
            _ => match card.kind {
                crate::dashboard::DealCardKind::Inbox(_) => card.room.as_ref().map_or_else(
                    || format!("inbox / {}", card.breadcrumb),
                    |room| format!("{room} / {}", card.breadcrumb),
                ),
                crate::dashboard::DealCardKind::Agent => card.breadcrumb.replace(" › ", " / "),
                crate::dashboard::DealCardKind::Desk => card.breadcrumb.replace(" › ", " / "),
            },
        };
        let path = Self::truncate_outline_path(&path);
        let line = div()
            .id("rho-status-line")
            .h(px(26.))
            .w_full()
            .px_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant.opacity(0.6))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .bg(cx.theme().colors().editor_background)
            .text_color(cx.theme().colors().text)
            .font_family(text_style.font_family.clone())
            .text_size(text_style.font_size)
            .line_height(text_style.line_height)
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
        if self.deal_hints_visible {
            return line
                .child("· Tab skip · d done · x dismiss · s defer · S defer room · t todo · f file · q skip + stop")
                .child(div().flex_1())
                .child(right)
                .into_any_element();
        }
        if self.deal_controls_visible {
            return line
                .child(
                    div()
                        .id("deal-touch-skip")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DashboardDealNext), cx)
                        })
                        .child("skip"),
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
                        .id("deal-touch-dismiss")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DashboardDealDiscard), cx)
                        })
                        .child("dismiss"),
                )
                .child(
                    div()
                        .id("deal-touch-close")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DashboardDealExit), cx)
                        })
                        .child("close"),
                )
                .child(div().flex_1())
                .child(right)
                .into_any_element();
        }
        line.child(div().flex_1()).child(right).into_any_element()
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
        let now = now_ms() as f64 / 1_000.0;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .children(quota.into_iter().enumerate().flat_map(|(index, summary)| {
                let color = match summary.remaining_percent {
                    0 => status.error,
                    1..=10 => status.warning,
                    _ if summary.model == "gpt" => colors.terminal_ansi_cyan,
                    _ if summary.model == "opus" || summary.model == "fable" => {
                        gpui::rgb(0xd97757).into()
                    }
                    _ => colors.text_muted,
                };
                let reset = summary
                    .reset_at_unix
                    .map(|reset| reset as f64 - now)
                    .filter(|seconds| *seconds > 0.0)
                    .map(|seconds| format!(" {:.1}d", seconds / 86_400.0))
                    .unwrap_or_default();
                let separator = (index > 0).then(|| {
                    div()
                        .text_color(colors.text_muted)
                        .child("·")
                        .into_any_element()
                });
                separator.into_iter().chain(std::iter::once(
                    div()
                        .text_color(color)
                        // Colour alone names the provider; no model text.
                        .child(format!("{}%{reset}", summary.remaining_percent))
                        .into_any_element(),
                ))
            }))
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
                        "deal".to_owned()
                    } else {
                        mode
                    }),
            )
            .into_any_element()
    }

    fn render_status_line(
        &mut self,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.dashboard.deal_mode()
            && let Some(card) = self.dashboard.current_deal_card()
        {
            return self.render_deal_why(card, text_style, cx);
        }
        let path = if self.overview_open {
            let path = self
                .dashboard
                .cursor_breadcrumb(cx)
                .unwrap_or_else(|| "desk".to_owned());
            if matches!(
                self.dashboard.cursor_target(&self.registry, cx),
                Some(crate::dashboard::RowTarget::NewDraft(_))
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
                #[cfg(feature = "native")]
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
        div()
            .id("rho-status-line")
            .h(px(26.))
            .w_full()
            .px_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant.opacity(0.6))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .bg(cx.theme().colors().editor_background)
            .text_color(cx.theme().colors().text)
            .font_family(text_style.font_family.clone())
            .text_size(text_style.font_size)
            .line_height(text_style.line_height)
            .child(left)
            .child(right)
            .into_any_element()
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
        #[cfg(feature = "native")]
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
        #[cfg(feature = "native")]
        let web_preview_visible = self.dashboard_web_preview.is_some();
        #[cfg(not(feature = "native"))]
        let web_preview_visible = false;
        let room_surface_visible = self.current_room.as_ref().is_none_or(|room| {
            self.room_strips
                .get(room)
                .is_some_and(|strip| !strip.is_empty())
        });
        let show_surface = (!home && room_surface_visible)
            || iris
            || self.dashboard_preview.is_some()
            || web_preview_visible;
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
        #[cfg(feature = "native")]
        let browser_preview_focused = self
            .dashboard_web_preview
            .as_ref()
            .is_some_and(|(_, view)| view.read(cx).focus_handle(cx).is_focused(window));
        #[cfg(not(feature = "native"))]
        let browser_preview_focused = false;
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
            #[cfg(feature = "native")]
            SurfaceView::ZulipInbox(view) => div()
                .id("rho-surface-zulip-inbox")
                .size_full()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            #[cfg(feature = "native")]
            SurfaceView::ZulipNarrow(view) => div()
                .id("rho-surface-zulip-narrow")
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
            #[cfg(feature = "native")]
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

fn desk_heading_without_agent(
    dashboard_focused: bool,
    target: Option<crate::dashboard::RowTarget>,
) -> bool {
    dashboard_focused && matches!(target, Some(crate::dashboard::RowTarget::Topic { .. }))
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

#[cfg(feature = "native")]
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
            .on_action(cx.listener(|this, _: &RoomStripLeft, window, cx| {
                this.take_and_stop_dealing(window, cx);
                this.slide_room_strip(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RoomStripRight, window, cx| {
                this.take_and_stop_dealing(window, cx);
                this.slide_room_strip(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RoomBack, window, cx| {
                this.take_and_stop_dealing(window, cx);
                this.step_room_back(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DealOpen, window, cx| {
                if this.dashboard.deal_mode() {
                    if !this.take_current_deal(false, window, cx) {
                        return;
                    }
                    this.dashboard.advance_deal(cx);
                    if this.dashboard.current_deal_card().is_some() {
                        this.present_next_deal(window, cx);
                    } else {
                        this.end_deal_session();
                        this.deal_origin = None;
                        this.deal_view = None;
                        if let Ok(action) = cx.build_action("vim::ExitDealMode", None) {
                            window.dispatch_action(action, cx);
                        }
                        this.notice_on(None, "nothing needs you", StyleClass::SystemInfo, cx);
                        this.refresh_dashboard(window, cx);
                    }
                } else {
                    this.open_deal_mode(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OverviewToggle, window, cx| {
                this.toggle_overview(window, cx);
            }))
            .on_action(cx.listener(|this, _: &StripRemove, window, cx| {
                this.remove_from_room_strip(window, cx);
            }))
            .on_action(cx.listener(|this, _: &BrowserExit, window, cx| {
                this.take_and_stop_dealing(window, cx);
                this.focus_rail(window, cx);
            }))
            .on_action(cx.listener(Self::shell_interrupt))
            .on_action(cx.listener(Self::toggle_voice))
            .on_action(cx.listener(|this, _: &InboxCapture, window, cx| {
                this.cmd_capture(window, cx);
            }))
            .on_action(cx.listener(Self::shell_eof))
            .on_action(cx.listener(|this, _: &ZulipOpenRow, window, cx| {
                #[cfg(feature = "native")]
                this.zulip_open_row(window, cx);
                #[cfg(not(feature = "native"))]
                let _ = window;
            }))
            .on_action(cx.listener(|this, _: &ZulipNextUnread, window, cx| {
                #[cfg(feature = "native")]
                this.zulip_next_unread(window, cx);
                #[cfg(not(feature = "native"))]
                let _ = window;
            }))
            .on_action(cx.listener(|this, _: &ZulipLoadOlder, _, cx| {
                #[cfg(feature = "native")]
                this.zulip_load_older(cx);
            }))
            .on_action(cx.listener(|this, _: &ZulipQuit, window, cx| {
                #[cfg(feature = "native")]
                this.zulip_quit(window, cx);
                #[cfg(not(feature = "native"))]
                let _ = window;
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
            .on_action(cx.listener(|this, _: &AgentJumpAttention, window, cx| {
                this.jump_to_attention(window, cx);
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
            .on_action(cx.listener(|this, _: &DashboardStaff, window, cx| {
                if !this.dashboard_verb_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.staff_dashboard_node(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardReply, window, cx| {
                if this.take_universal_argument() {
                    this.configure_dashboard_staff(window, cx);
                } else {
                    this.dashboard_reply(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardSubmit, window, cx| {
                this.dashboard_submit(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardNewAgent, window, cx| {
                if this.take_universal_argument() {
                    this.configure_dashboard_quick_spawn(window, cx);
                } else {
                    this.new_agent_draft = None;
                    this.dashboard.open_new_draft(None, window, cx);
                    this.dashboard_focus_draft(window, cx);
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
                    if !this.deal_current_taken {
                        this.dashboard
                            .skip_current_deal(chrono::Local::now().fixed_offset(), cx);
                    }
                    this.stop_dealing_and_restore(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealNext, window, cx| {
                vim::take_count(cx);
                if this
                    .dashboard
                    .skip_current_deal(chrono::Local::now().fixed_offset(), cx)
                {
                    if this.dashboard.current_deal_card().is_some() {
                        this.present_next_deal(window, cx);
                    } else {
                        this.stop_dealing_and_restore(window, cx);
                    }
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealPrevious, window, cx| {
                vim::take_count(cx);
                if this.dashboard.previous_deal(cx) {
                    this.deal_view = None;
                    this.deal_hints_visible = false;
                    this.deal_controls_visible = false;
                    this.journal_deal_card_presented();
                    this.refresh_dashboard(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealDone, window, cx| {
                vim::take_count(cx);
                let card = this.dashboard.current_deal_card().cloned();
                let today = chrono::Local::now().date_naive();
                let now = chrono::Local::now().fixed_offset();
                let handled = match card.as_ref().map(|card| card.kind) {
                    Some(crate::dashboard::DealCardKind::Desk) => {
                        this.dashboard.write_deal_done(today, cx)
                    }
                    Some(crate::dashboard::DealCardKind::Agent) => {
                        this.cmd_agent_done(false, window, cx);
                        true
                    }
                    Some(crate::dashboard::DealCardKind::Inbox(_)) => {
                        let Some(crate::dashboard::DealCardIdentity::Inbox(id)) =
                            card.as_ref().map(|card| &card.identity)
                        else {
                            return;
                        };
                        this.inbox
                            .verdict(
                                &crate::inbox::InboxId(id.clone()),
                                crate::inbox::Verdict::Discarded,
                            )
                            .is_ok()
                    }
                    None => false,
                };
                if handled {
                    this.dashboard
                        .record_deal_verdict_as(crate::dashboard::DealerVerdict::Done, now);
                    this.dashboard.advance_deal(cx);
                    this.journal_deal_card_presented();
                    this.finish_dashboard_deal_action(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealDiscard, window, cx| {
                vim::take_count(cx);
                let card = this.dashboard.current_deal_card().cloned();
                let today = chrono::Local::now().date_naive();
                let now = chrono::Local::now().fixed_offset();
                let handled = match card.as_ref().map(|card| card.kind) {
                    Some(crate::dashboard::DealCardKind::Desk) => {
                        this.dashboard.write_deal_discarded(today, cx)
                    }
                    Some(crate::dashboard::DealCardKind::Agent) => {
                        this.cmd_agent_done(true, window, cx);
                        true
                    }
                    Some(crate::dashboard::DealCardKind::Inbox(_)) => {
                        let Some(crate::dashboard::DealCardIdentity::Inbox(id)) =
                            card.as_ref().map(|card| &card.identity)
                        else {
                            return;
                        };
                        this.inbox
                            .verdict(
                                &crate::inbox::InboxId(id.clone()),
                                crate::inbox::Verdict::Discarded,
                            )
                            .is_ok()
                    }
                    None => false,
                };
                if handled {
                    this.dashboard
                        .record_deal_verdict_as(crate::dashboard::DealerVerdict::Dismiss, now);
                    this.dashboard.advance_deal(cx);
                    this.journal_deal_card_presented();
                    this.finish_dashboard_deal_action(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealSnooze, window, cx| {
                let count = vim::take_count(cx).unwrap_or(1) as u32;
                let today = chrono::Local::now().date_naive();
                let now = chrono::Local::now().fixed_offset();
                let card = this.dashboard.current_deal_card().cloned();
                let handled = match card.as_ref().map(|card| card.kind) {
                    Some(crate::dashboard::DealCardKind::Desk) => {
                        this.dashboard.write_deal_snooze(count, today, cx)
                    }
                    Some(crate::dashboard::DealCardKind::Inbox(_)) => {
                        let Some(crate::dashboard::DealCardIdentity::Inbox(id)) =
                            card.as_ref().map(|card| &card.identity)
                        else {
                            return;
                        };
                        this.inbox
                            .defer(
                                &crate::inbox::InboxId(id.clone()),
                                (now + chrono::Duration::days(i64::from(count))).timestamp_millis(),
                            )
                            .unwrap_or(false)
                    }
                    Some(crate::dashboard::DealCardKind::Agent) => {
                        let Some(agent_id) = card.as_ref().and_then(|card| card.agent_id) else {
                            return;
                        };
                        this.set_agent_disposition(
                            vec![agent_id],
                            "snooze",
                            rho_ui_proto::AgentDisposition::Snoozed {
                                until: rho_core::UnixMs(
                                    (now + chrono::Duration::days(i64::from(count)))
                                        .timestamp_millis()
                                        .max(0) as u64,
                                ),
                            },
                            cx,
                        )
                    }
                    _ => false,
                };
                if handled {
                    this.dashboard
                        .record_deal_verdict_as(crate::dashboard::DealerVerdict::Defer, now);
                    this.dashboard.advance_deal(cx);
                    this.journal_deal_card_presented();
                    this.finish_dashboard_deal_action(window, cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &DashboardDealRoomSnooze, window, cx| {
                    let count = vim::take_count(cx).unwrap_or(1) as u32;
                    let today = chrono::Local::now().date_naive();
                    let now = chrono::Local::now().fixed_offset();
                    if let Some((room, until, identity)) =
                        this.dashboard.write_deal_room_snooze(count, today, cx)
                    {
                        this.dashboard
                            .record_deal_verdict_as(crate::dashboard::DealerVerdict::Defer, now);
                        crate::journal::record(crate::journal::Event::RoomDeferred {
                            room,
                            until: until.format("%Y-%m-%dT%H:%M:%S").to_string(),
                            card: Self::journal_card_identity(&identity),
                        });
                        this.dashboard.advance_deal(cx);
                        this.journal_deal_card_presented();
                        this.finish_dashboard_deal_action(window, cx);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &DashboardDealTodo, window, cx| {
                vim::take_count(cx);
                let today = chrono::Local::now().date_naive();
                if this.dashboard.write_deal_todo(today, cx) {
                    this.dashboard.record_deal_verdict_as(
                        crate::dashboard::DealerVerdict::Done,
                        chrono::Local::now().fixed_offset(),
                    );
                    this.dashboard.advance_deal(cx);
                    this.journal_deal_card_presented();
                    this.finish_dashboard_deal_action(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealRefresh, window, cx| {
                vim::take_count(cx);
                if !this.dashboard.deal_mode() {
                    cx.propagate();
                    return;
                }
                this.dashboard.discard_deal_session(cx);
                let now = chrono::Local::now().fixed_offset();
                if let Err(error) = this.inbox.refresh_deferred(now.timestamp_millis()) {
                    tracing::warn!(%error, "waking deferred inbox items");
                }
                let current_room = this
                    .current_room
                    .as_ref()
                    .and_then(|room| Some((room.host, this.room_names.get(room)?.as_str())));
                this.deal_view = None;
                this.deal_hints_visible = false;
                this.deal_controls_visible = false;
                this.dashboard.enter_deal_mode_in_room(
                    &this.registry,
                    &this.inbox,
                    now,
                    current_room,
                    &this.agent_last_interaction,
                    cx,
                );
                this.journal_deal_card_presented();
                this.refresh_dashboard(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDealInsert, window, cx| {
                vim::take_count(cx);
                let deal_agent = this
                    .dashboard
                    .current_deal_card()
                    .and_then(|card| card.agent_id);
                let edits_agent = deal_agent.is_some();
                if !this.take_current_deal(false, window, cx) {
                    return;
                }
                if let Some(agent_id) = deal_agent {
                    this.dashboard.exit_deal_mode(cx);
                    this.end_deal_session();
                    this.deal_origin = None;
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
                if !this.take_current_deal(false, window, cx) {
                    return;
                }
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
                if !this.take_current_deal(false, window, cx) {
                    return;
                }
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
                if let Some(crate::dashboard::DealCardIdentity::Inbox(id)) = this
                    .dashboard
                    .current_deal_card()
                    .map(|card| card.identity.clone())
                {
                    let id = crate::inbox::InboxId(id);
                    if this.inbox.get(&id).is_none() {
                        return;
                    }
                    this.dashboard.record_deal_verdict_as(
                        crate::dashboard::DealerVerdict::File,
                        chrono::Local::now().fixed_offset(),
                    );
                    if let Err(error) = this.begin_inbox_filing(&id, window, cx) {
                        this.notice_on(None, &format!("file: {error}"), StyleClass::SystemInfo, cx);
                        return;
                    }
                    this.dashboard.exit_deal_mode(cx);
                    this.finish_dashboard_deal_action(window, cx);
                    return;
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDealReply, window, cx| {
                vim::take_count(cx);
                let Some(card) = this.dashboard.current_deal_card().cloned() else {
                    return;
                };
                #[cfg(feature = "native")]
                let opens_page = matches!(
                    &card.inbox_source,
                    Some(crate::dashboard::DealerInboxSource::Page(_))
                );
                #[cfg(not(feature = "native"))]
                let opens_page = false;
                if card.agent_id.is_none()
                    && !opens_page
                    && !matches!(card.kind, crate::dashboard::DealCardKind::Desk)
                {
                    return;
                }
                this.dashboard.record_deal_verdict_as(
                    crate::dashboard::DealerVerdict::Open,
                    chrono::Local::now().fixed_offset(),
                );
                if !this.dashboard.exit_deal_mode(cx) {
                    return;
                }
                this.end_deal_session();
                if let Ok(action) = cx.build_action("vim::ExitDealMode", None) {
                    window.dispatch_action(action, cx);
                }
                if let crate::dashboard::DealCardIdentity::Desk {
                    host,
                    heading_offset,
                } = card.identity
                {
                    this.dashboard
                        .open_new_draft(Some((host, heading_offset)), window, cx);
                    this.dashboard_focus_draft(window, cx);
                    return;
                }
                #[cfg(feature = "native")]
                if let Some(crate::dashboard::DealerInboxSource::Page(page)) = card.inbox_source {
                    this.open_browser_page(page, window, cx);
                    return;
                }
                if let Some(agent_id) = card.agent_id {
                    this.open_agent(agent_id, window, cx);
                    return;
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
                if this.dashboard.archive_cursor_heading(cx) {
                    this.refresh_dashboard(window, cx);
                } else {
                    this.notice_on(
                        None,
                        "archive: already archived",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            }))
            .on_action(cx.listener(|this, _: &DashboardDemote, window, cx| {
                if !this.dashboard_verb_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard_structure_move(crate::dashboard::StructureDirection::Demote, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardPromote, window, cx| {
                if !this.dashboard_verb_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard_structure_move(crate::dashboard::StructureDirection::Promote, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardDeleteEmpty, window, cx| {
                if !this.dashboard_verb_applies(window, cx) {
                    cx.propagate();
                    return;
                }
                this.dashboard_delete_empty(cx);
            }))
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
                this.take_and_stop_dealing(window, cx);
                this.focus_rail(window, cx);
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
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(if phone {
                        self.render_phone_body(&text_style, cx)
                    } else {
                        self.render_workspace(window, &text_style, cx)
                    }),
            )
            .child(self.render_status_line(&text_style, cx))
            .children(
                match (
                    &self.pending_git_approval,
                    &self.minibuffer,
                    &self.transient,
                    self.universal_argument,
                    &self.echo,
                ) {
                    (Some(pending), _, _, _, _) => {
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
                    (None, Some(minibuffer), _, _, _) => Some(if phone {
                        minibuffer.render_phone(&text_style, cx)
                    } else {
                        minibuffer.render(&text_style, cx)
                    }),
                    (None, None, Some(transient), _, _) => {
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
                    (None, None, None, true, _) => Some(
                        bottom_strip(&text_style, cx)
                            .child(
                                div()
                                    .px_2()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .child("SPC u"),
                            )
                            .child(div().px_2().child("universal argument"))
                            .into_any_element(),
                    ),
                    (None, None, None, false, Some(_)) => None,
                    (None, None, None, false, None) => None,
                },
            )
    }
}

#[cfg(feature = "native")]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(not(feature = "native"))]
pub fn now_ms() -> u64 {
    js_sys::Date::now().max(0.0) as u64
}

/// `30m`, `2h`, `1d`; a bare number means minutes.
fn parse_duration_ms(text: &str) -> Option<u64> {
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
    fn unbound_desk_heading_is_a_local_disposition_target() {
        use crate::dashboard::RowTarget;

        assert!(desk_heading_without_agent(
            true,
            Some(RowTarget::Topic {
                host: HostId::default(),
                offset: 0,
                first_attention: None,
                on_heading_line: true,
                on_bullet: false,
            })
        ));
        assert!(!desk_heading_without_agent(
            true,
            Some(RowTarget::Agent {
                agent_id: AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(0)).unwrap(),
                topic: None,
            })
        ));
        assert!(!desk_heading_without_agent(false, Some(RowTarget::None)));
        assert!(!desk_heading_without_agent(true, None));
    }

    fn project(name: &str, path: &str) -> HostProject {
        HostProject {
            host: HostId(1),
            project: rho_ui_proto::UiProject {
                name: name.into(),
                path: path.into(),
                description: String::new(),
            },
        }
    }

    #[test]
    fn desk_project_resolution_prefers_property_then_single_then_picker() {
        let projects = vec![project("rho", "/src/rho"), project("zed", "/src/zed")];
        assert_eq!(
            resolve_desk_project(Some("zed"), &projects),
            DeskProjectResolution::Use(1)
        );
        assert_eq!(
            resolve_desk_project(Some("/src/rho"), &projects),
            DeskProjectResolution::Use(0)
        );
        assert_eq!(
            resolve_desk_project(None, &projects),
            DeskProjectResolution::Choose
        );
        assert_eq!(
            resolve_desk_project(None, &projects[..1]),
            DeskProjectResolution::Use(0)
        );
        assert_eq!(
            resolve_desk_project(Some("missing"), &projects),
            DeskProjectResolution::Missing
        );
    }

    #[test]
    fn desk_reply_identifies_the_message_as_a_brief_update() {
        assert_eq!(
            updated_desk_brief("root\n\nchild"),
            "Updated brief from the Desk:\n\nroot\n\nchild"
        );
    }
}
