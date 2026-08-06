//! Root entity: owns the attached daemons, the canonical agent states, the
//! registry, and one persistent [`AgentModel`] per opened agent.
//!
//! All protocol events flow through [`Workspace`]; queued frame runs are
//! merged per agent, and views receive summarized changes rather than the
//! protocol itself.
//!
//! Several daemons can be attached at once. Agent and workstream ids are
//! already unique across machines, so the client-side state stays keyed by
//! id alone; what the host is needed for is routing — which socket a command
//! travels down — and for the few places where a daemon-side *name* (a
//! repository path, a short agent label) is only unique within one machine.

#[cfg(all(target_family = "wasm", not(feature = "native")))]
#[path = "workspace_web.rs"]
mod web;

use std::collections::{HashMap, HashSet};
#[cfg(feature = "native")]
use std::path::PathBuf;
#[cfg(feature = "native")]
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use camino::Utf8PathBuf;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::prelude::*;
use gpui::{App, ClipboardEntry, Context, Entity, Focusable as _, Task, Window, div, px, svg};
use rho_core::ContentPart;
use rho_ui_proto::{
    AdvisorIntelligence, AgentId, AgentRole, ClientMessage, EngineerIntelligence, MessageDelivery,
};
use theme::ActiveTheme as _;

use crate::agent_view::AgentModel;
#[cfg(feature = "native")]
use crate::chime::Chime;
#[cfg(feature = "native")]
use crate::connection::GitApprovalDecision;
use crate::connection::{AgentFrameAllocation, ConnEvent, Connection, HostEvent};
use crate::draft_view::DraftModel;
use crate::hosts::{HostStatus, Hosts};
use crate::minibuffer::{ECHO_DURATION, Echo, Minibuffer, bottom_strip};
use crate::pane::{PaneTree, SplitAxis, SurfaceKey};
use crate::registry::session::{
    AgentSubscriptions, INITIAL_AGENT_SUBSCRIPTIONS, recent_workstream_roots,
};
use crate::registry::{ActivePane, AgentRegistry, HostId};
use crate::store::{AgentStore, FrameSummary};
use crate::style::{RoleFamily, StyleClass};
use crate::zed_remote::{FileView, RemoteProject};
use crate::{
    AgentDone, AgentHide, AgentJumpAttention, AgentNew, AgentNext, AgentPrevious,
    DashboardNewAgent, DashboardReply, DashboardToggleSubagents, GitApprovalAllow, GitApprovalDeny,
    MinibufferCancel, MinibufferComplete, MinibufferConfirm, MinibufferNext, MinibufferPrevious,
    PaneBack, PaneClose, PaneFocusNext, PaneSplitDown, PaneSplitRight, PastePrompt, RailFocus,
    RailOpen, RoleCycle, RoleCycleGroup, ShellEof, ShellInterrupt, ShellPagerAll, ShellPagerMore,
    ShellPagerQuit, SubmitPrompt, TaskBoard, VoiceToggle, ZulipLoadOlder, ZulipNextUnread,
    ZulipOpenRow, ZulipQuit,
};

/// What a pane shows: stable identity plus the live view. Surfaces live
/// in their context's surface list for the context's lifetime; panes hold
/// clones of the same view handles, so display is cheap and the view (and
/// any remote channel behind it) releases when the context closes.
#[derive(Clone)]
pub struct Surface {
    key: SurfaceKey,
    view: SurfaceView,
    /// The view's editor entity: the identity focus-follow reports, since
    /// two panes can show the same key through different editors.
    editor_id: Option<gpui::EntityId>,
    /// Focus-in observer: gpui focus arriving inside the surface's editor
    /// (mouse click, vim motion) pulls pane focus along. Shared by all
    /// pane clones of the surface, dropped with the last one.
    _focus_follow: Option<std::rc::Rc<gpui::Subscription>>,
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
        model: Entity<DraftModel>,
        /// This pane's own editor over the model's multibuffer.
        editor: Entity<editor::Editor>,
    },
    Transcript {
        model: Entity<AgentModel>,
        /// This pane's own editor over the model's multibuffer.
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
    ZulipInbox(Entity<rho_zulip::ui::InboxView>),
    #[cfg(feature = "native")]
    ZulipNarrow(Entity<rho_zulip::ui::NarrowView>),
}

impl PartialEq for Surface {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

/// Which task's window arrangement fills the window. Every workstream
/// keeps its own split tree, like emacs perspectives; the draft composer
/// is its own context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ContextId {
    Draft,
    Task(rho_ui_proto::WorkstreamId),
    /// Zulip's own window arrangement: entering it from the dashboard
    /// leaves the agent panes exactly as they were, and leaving it comes
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
    /// Workstream the draft inherits context from: whichever was focused
    /// when the draft was entered. Only used to derive the default workdir —
    /// a submitted draft always founds its own workstream.
    draft_workstream: Option<rho_ui_proto::WorkstreamId>,
    /// Launch arguments for the dashboard's parked new-root draft. The
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
    duration_timer: Option<Task<()>>,
    /// Attention chime output; lazily opened on the first play.
    #[cfg(feature = "native")]
    chime: Chime,
    /// Per-context split trees of viewports over surfaces. The rail is
    /// ambient chrome beside the active tree, not a pane in it.
    contexts: HashMap<ContextId, PaneTree<Surface>>,
    /// Per-context surface list, the emacs buffer list: every surface
    /// opened in a context lives here for the context's lifetime,
    /// regardless of what its panes currently display. Panes are
    /// viewports over this list — covering or closing one never loses a
    /// file or terminal; the views (and any workspace file channel behind them)
    /// release when the context itself closes.
    surfaces: HashMap<ContextId, Vec<Surface>>,
    /// Always present in `contexts` (the draft context never closes).
    active_context: ContextId,
    /// The dashboard: the rail as a real editor buffer, ambient chrome
    /// beside the active tree.
    dashboard: crate::dashboard::Dashboard,
    /// Agent shown beside the dashboard cursor. Kept separate from the
    /// focused task so cursor previews do not rebuild or reorder the rail.
    dashboard_preview: Option<AgentId>,
    /// The dashboard projection is regenerated only after its registry or
    /// local composition state changes, never merely because another view
    /// dirtied the window.
    dashboard_dirty: bool,
    /// Read-only document shown when the synthetic Iris row is targeted.
    iris_preview: Entity<editor::Editor>,
    /// The Zulip client, started the first time its dashboard row is
    /// opened. Chat costs nothing until asked for.
    #[cfg(feature = "native")]
    zulip: Option<Entity<rho_zulip::session::Session>>,
    /// The completing-read strip at the bottom of the window, when open.
    minibuffer: Option<Minibuffer>,
    /// An open transient menu in the bottom strip; captures the keyboard
    /// via `transient_focus` while shown.
    transient: Option<crate::transient::Transient>,
    /// Parent menus beneath the open one; escape pops one level (magit's
    /// quit-one) before a final escape closes the strip.
    transient_stack: Vec<crate::transient::Transient>,
    transient_focus: gpui::FocusHandle,
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
    iris_muted: bool,
    /// The daemon running the voice session. Iris delegates to an agent by
    /// id, and an id from another daemon means nothing there, so the session
    /// binds to one host; selecting an agent elsewhere leaves it without
    /// context rather than sending a foreign id.
    iris_host: Option<HostId>,
    _event_task: Task<()>,
    _dashboard_subscription: gpui::Subscription,
    #[cfg(all(target_family = "wasm", not(feature = "native")))]
    web: web::WebUi,
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

    fn apply_workstream_created(&mut self, host: HostId, workstream: rho_ui_proto::UiWorkstream) {
        self.registry.add_workstream(host, workstream);
    }

    fn apply_agent_subscribed(&mut self, agent_id: AgentId) {
        self.registry.mark_known(agent_id);
    }

    fn apply_attention(
        &mut self,
        agent_id: AgentId,
        attention: rho_ui_proto::UiAttention,
    ) -> rho_ui_proto::UiAttention {
        let before = self.registry.attention(agent_id);
        self.registry.set_attention(agent_id, attention);
        before
    }

    fn apply_turn_report(&mut self, agent_id: AgentId, report: rho_ui_proto::UiTurnReport) -> bool {
        let needs_you = report.needs_you;
        self.registry.set_turn_report(agent_id, report);
        needs_you
    }

    fn apply_ready(
        &mut self,
        host: HostId,
        machine_seed: u64,
        agent_counter: u64,
        workstreams: Vec<rho_ui_proto::UiWorkstream>,
        agents: Vec<rho_ui_proto::UiAgentSummary>,
    ) -> (bool, Option<Vec<AgentId>>) {
        let first_ready = self.ready_hosts.insert(host);
        let initial = first_ready.then(|| {
            recent_workstream_roots(
                &workstreams,
                &agents,
                self.registry.selected_agent().copied(),
                INITIAL_AGENT_SUBSCRIPTIONS,
            )
        });
        self.registry
            .set_host_data(host, machine_seed, agent_counter, workstreams, agents);
        (first_ready, initial)
    }

    fn note_agent_created(
        &mut self,
        host: HostId,
        agent_id: AgentId,
        workstream: rho_ui_proto::WorkstreamId,
    ) {
        self.registry
            .note_agent_workstream(host, agent_id, workstream);
        self.registry.mark_known(agent_id);
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

/// Which workstream operation a transient prompt collects a name for.
#[derive(Clone, Copy)]
pub enum WorkstreamPrompt {
    Group,
    Label,
    Unlabel,
    Rename,
    Merge,
}

#[derive(Clone)]
struct NewAgentDraft {
    host: Option<HostId>,
    workdir: Option<HostPath>,
    workspace: DraftWorkspace,
    role: String,
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

    fn mode_and_target(&self) -> (crate::draft_view::StartFieldMode, &str) {
        match self {
            Self::NewOn(base) => (crate::draft_view::StartFieldMode::NewOn, base.target()),
            Self::Join(target) => (crate::draft_view::StartFieldMode::Join, target),
            Self::Sandbox(base) => (crate::draft_view::StartFieldMode::Sandbox, base.target()),
        }
    }
}

impl WorkstreamPrompt {
    fn prompt(self) -> &'static str {
        match self {
            WorkstreamPrompt::Group => "group:",
            WorkstreamPrompt::Label => "label:",
            WorkstreamPrompt::Unlabel => "unlabel:",
            WorkstreamPrompt::Rename => "rename workstream:",
            WorkstreamPrompt::Merge => "merge into:",
        }
    }
}

impl Workspace {
    #[cfg(feature = "native")]
    pub fn new(specs: Vec<HostSpec>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (hosts, events) = Hosts::new();
        let workspace = cx.entity().downgrade();
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
        // change while the dashboard is focused re-aims the panes.
        let dashboard_subscription = cx.subscribe_in(
            dashboard.editor(),
            window,
            |this, _, event: &editor::EditorEvent, window, cx| {
                if matches!(
                    event,
                    editor::EditorEvent::SelectionsChanged { local: true }
                ) {
                    this.dashboard_cursor_moved(window, cx);
                }
            },
        );
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
            draft_workstream: None,
            new_agent_draft: None,
            awaiting_draft_agent: None,
            ready_hosts: HashSet::new(),
            quota_summaries: HashMap::new(),
            quota_history: HashMap::new(),
            quota_history_days: 7,
            global_usage: HashMap::new(),
            global_usage_days: 7,
            duration_timer: None,
            chime: Chime::default(),
            contexts: HashMap::new(),
            surfaces: HashMap::new(),
            active_context: ContextId::Draft,
            dashboard,
            dashboard_preview: None,
            dashboard_dirty: true,
            iris_preview,
            zulip: None,
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
            iris_muted: false,
            iris_host: None,
            _event_task: event_task,
            _dashboard_subscription: dashboard_subscription,
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
        let departed = self
            .registry
            .known_agents()
            .copied()
            .filter(|agent_id| self.registry.host_of_agent(*agent_id) == Some(host))
            .collect::<Vec<_>>();
        let contexts = self
            .registry
            .workstreams()
            .iter()
            .filter(|workstream| workstream.host == host)
            .map(|workstream| ContextId::Task(workstream.workstream_id))
            .collect::<HashSet<_>>();
        if self.iris_host == Some(host) {
            self.stop_iris();
        }
        self.hosts.detach(host);
        self.ready_hosts.remove(&host);
        self.quota_summaries.remove(&host);
        self.quota_history.remove(&host);
        self.global_usage.remove(&host);
        self.workdirs.retain(|workdir| workdir.host != host);
        self.remote_projects.retain(|(owner, _), _| *owner != host);
        self.registry.detach_host(host);
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
        self.dashboard_dirty = true;
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

    fn send_to_workstream(
        &self,
        workstream_id: rho_ui_proto::WorkstreamId,
        message: ClientMessage,
    ) {
        if let Some(connection) = self
            .registry
            .host_of_workstream(workstream_id)
            .and_then(|host| self.hosts.connection(host))
        {
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

    fn active_tree(&self) -> &PaneTree<Surface> {
        self.contexts
            .get(&self.active_context)
            .expect("active context has a tree")
    }

    fn active_tree_mut(&mut self) -> &mut PaneTree<Surface> {
        self.contexts
            .get_mut(&self.active_context)
            .expect("active context has a tree")
    }

    /// The context an agent's transcript lives in: its workstream. An
    /// agent the registry can't place yet shows in the draft context.
    fn context_for_agent(&self, agent_id: AgentId) -> ContextId {
        self.registry
            .workstream_of(agent_id)
            .map(ContextId::Task)
            .unwrap_or(ContextId::Draft)
    }

    /// Drops trees for tasks that no longer exist; their views (and any
    /// workspace file channels behind them) release with them.
    fn prune_contexts(&mut self) {
        let live = self
            .registry
            .workstreams()
            .iter()
            .map(|workstream| workstream.workstream_id)
            .collect::<HashSet<_>>();
        let keep = |context: &ContextId| match context {
            ContextId::Draft => true,
            ContextId::Task(workstream_id) => live.contains(workstream_id),
            // Zulip belongs to no daemon, so no workstream's death closes it.
            ContextId::Zulip => true,
        };
        self.contexts.retain(|context, _| keep(context));
        self.surfaces.retain(|context, _| keep(context));
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
        cx.on_next_frame(window, |this, window, cx| {
            this.frame_flush_scheduled = false;
            this.flush_pending_frames(window, cx);
        });
        // `on_next_frame` attaches to a draw; make sure an otherwise idle
        // window gets one to consume the queued transport state.
        cx.notify();
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
        // Transcript frames are delivered independently of rail state and
        // can arrive for every streamed token. Rebuilding the dashboard's
        // editor excerpts and inlays for them makes the rail visibly flicker.
        // Only events that can change its projection invalidate it.
        if matches!(
            &event,
            ConnEvent::Ready { .. }
                | ConnEvent::WorkstreamCreated(_)
                | ConnEvent::AgentCreated { .. }
                | ConnEvent::AgentAttention { .. }
                | ConnEvent::AgentTurnReport { .. }
        ) {
            self.dashboard_dirty = true;
        }
        match event {
            ConnEvent::Ready {
                workstreams,
                agents,
                projects: workdirs,
                auth,
                machine_seed,
                agent_counter,
            } => {
                let (first_ready, initial_subscriptions) =
                    self.apply_ready(host, machine_seed, agent_counter, workstreams, agents);
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
                cx.notify();
            }
            ConnEvent::AuthState(auth) => {
                if let Some(entry) = self.hosts.get_mut(host) {
                    entry.auth = Some(auth);
                }
                cx.notify();
            }
            ConnEvent::WorkstreamCreated(workstream) => {
                self.apply_workstream_created(host, workstream);
                self.refresh_draft_agent_targets(cx);
                cx.notify();
            }
            ConnEvent::AgentCreated {
                agent_id,
                workstream,
            } => {
                self.note_agent_created(host, agent_id, workstream);
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
            } => {
                // Chime on the rising edge into the user's court only when
                // the agent is blocked or a needs-you report is already in
                // hand (snooze expiry resurfacing a classified turn); a
                // plain turn end waits for its report, which decides between
                // a needs-you chime and a silent FYI. Never for the agent
                // already on screen, whose turn end the user is watching.
                let before = self.apply_attention(agent_id, attention);
                let needs_you = attention >= rho_ui_proto::UiAttention::NeedsInput
                    || self
                        .registry
                        .agent_turn_report(agent_id)
                        .is_some_and(|report| report.needs_you);
                if attention >= rho_ui_proto::UiAttention::Pending
                    && before < rho_ui_proto::UiAttention::Pending
                    && needs_you
                    && self.registry.selected_agent() != Some(&agent_id)
                {
                    #[cfg(feature = "native")]
                    self.chime.play();
                }
                cx.notify();
            }
            ConnEvent::AgentTurnReport { agent_id, report } => {
                // The attention gate keeps snoozed agents silent: their
                // reports arrive while attention is Quiet and surface only
                // at snooze expiry.
                let needs_you = self.apply_turn_report(agent_id, report);
                if needs_you
                    && self.registry.attention(agent_id) >= rho_ui_proto::UiAttention::Pending
                    && self.registry.selected_agent() != Some(&agent_id)
                {
                    #[cfg(feature = "native")]
                    self.chime.play();
                }
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

    fn submit_prompt(&mut self, _: &SubmitPrompt, window: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_tree().focused().surface.view {
            model.clone().update(cx, |model, cx| model.submit(cx));
            return;
        }
        #[cfg(feature = "native")]
        if matches!(
            self.active_tree().focused().surface.view,
            SurfaceView::ZulipNarrow(_)
        ) {
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
        if let SurfaceView::Shell { model, .. } = &self.active_tree().focused().surface.view {
            model.clone().update(cx, |model, _| model.interrupt());
        }
    }

    pub(crate) fn cmd_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_voice(&VoiceToggle, window, cx);
    }

    fn toggle_voice(&mut self, _: &VoiceToggle, _: &mut Window, cx: &mut Context<Self>) {
        if self.realtime_task.is_some() {
            self.iris_muted = true;
            self.stop_iris();
            self.notice_on(None, "muting Iris…", StyleClass::SystemInfo, cx);
            return;
        }
        self.iris_muted = false;
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
        self.iris_muted = false;
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
        let Some(connection) = self.hosts.connection(host) else {
            return;
        };
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        #[cfg(feature = "native")]
        let task = connection.start_native_realtime(stop_rx, cx);
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
                )
                .await
            })
        };
        self.realtime_stop = Some(stop);
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
                let message = match result {
                    Ok(()) => "Iris stopped listening".to_owned(),
                    Err(error) => format!("Iris failed: {error:#}"),
                };
                this.notice_on(None, &message, StyleClass::SystemInfo, cx);
                let host = this.iris_host.filter(|host| this.hosts.is_online(*host));
                if host.is_some() && !this.iris_muted {
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
                let view = cx.new(|cx| {
                    rho_zulip::ui::NarrowView::new(session, narrow, hooks, window, cx)
                });
                Self::wrap_surface(key, SurfaceView::ZulipNarrow(view), window, cx)
            }
        };
        self.display_surface(surface);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Marks the conversation on screen read, if one is.
    #[cfg(feature = "native")]
    fn leave_zulip_narrow(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::ZulipNarrow(view) = &self.active_tree().focused().surface.view {
            view.clone().update(cx, |view, cx| view.mark_read(cx));
        }
    }

    /// `enter` inside the Zulip inbox: open the conversation under the
    /// cursor.
    #[cfg(feature = "native")]
    fn zulip_open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let SurfaceView::ZulipInbox(view) = &self.active_tree().focused().surface.view else {
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
        let current = match &self.active_tree().focused().surface.view {
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
        if let SurfaceView::ZulipNarrow(view) = &self.active_tree().focused().surface.view {
            view.clone().update(cx, |view, cx| view.load_older(cx));
        }
    }

    /// `enter` in a Zulip conversation: send the composed message.
    #[cfg(feature = "native")]
    fn zulip_submit(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::ZulipNarrow(view) = &self.active_tree().focused().surface.view {
            view.clone().update(cx, |view, cx| view.submit(cx));
        }
    }

    fn shell_eof(&mut self, _: &ShellEof, _: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_tree().focused().surface.view {
            model.clone().update(cx, |model, cx| model.eof(cx));
        }
    }

    fn shell_pager_action(
        &mut self,
        action: rho_ui_proto::shell::PagerAction,
        cx: &mut Context<Self>,
    ) {
        if let SurfaceView::Shell { model, .. } = &self.active_tree().focused().surface.view {
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
        self.dashboard_dirty = true;
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
        // Every top-level agent founds its own workstream; the daemon names
        // it from the agent's generated title.
        self.hosts.send(
            host,
            ClientMessage::NewAgent {
                workstream: None,
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
        let dashboard_prompt = dashboard_mode && self.dashboard.accepts_attachments(cx);
        let pane_prompt = matches!(
            self.active_tree().focused().surface.view,
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
        if (!dashboard_prompt && !pane_prompt) || images.is_empty() {
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
            let added = if dashboard_prompt {
                self.dashboard
                    .add_image(media_type.to_owned(), image.bytes.clone(), cx)
            } else {
                match &self.active_tree().focused().surface.view {
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
                }
            };
            if dashboard_prompt && added {
                self.dashboard_dirty = true;
            }
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
        let dashboard_mode = self.dashboard_mode(window, cx);
        let cleared = if dashboard_mode {
            self.dashboard.clear_attachments(cx)
        } else {
            match &self.active_tree().focused().surface.view {
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
            if dashboard_mode {
                self.dashboard_dirty = true;
            }
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

    /// The selected agent, or a `{verb}: no agent selected` notice.
    fn selected_or_notice(&mut self, verb: &str, cx: &mut Context<Self>) -> Option<AgentId> {
        let selected = self.registry.selected_agent().copied();
        if selected.is_none() {
            let message = format!("{verb}: no agent selected");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
        }
        selected
    }

    pub(crate) fn cmd_agent_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_or_notice("cancel", cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(agent_id, ClientMessage::CancelTurn { agent_id });
        }
    }

    pub(crate) fn cmd_rewind(&mut self, turns: u32, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_or_notice("rewind", cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(agent_id, ClientMessage::RewindAgent { agent_id, turns });
        }
    }

    pub(crate) fn cmd_continue_turn(&mut self, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_or_notice("continue", cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.send_to_agent(agent_id, ClientMessage::ContinueTurn { agent_id });
        }
    }

    pub(crate) fn cmd_compact(&mut self, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_or_notice("compact", cx) {
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

    pub(crate) fn cmd_change_prompt_cache_key(&mut self, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_or_notice("change-prompt-cache-key", cx) {
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
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = self.selected_or_notice("change-role", cx) else {
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
        let Some(agent_id) = self.selected_or_notice("change-role", cx) else {
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
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                let intelligence = match input.trim().to_ascii_lowercase().as_str() {
                    "eng-low" => Some(EngineerIntelligence::Low),
                    "eng-cheap" => Some(EngineerIntelligence::Cheap),
                    "eng" => Some(EngineerIntelligence::Medium),
                    "eng-high" => Some(EngineerIntelligence::High),
                    "eng-ultra" => Some(EngineerIntelligence::Ultra),
                    "eng-alt" => Some(EngineerIntelligence::Alt),
                    _ => None,
                };
                match intelligence {
                    Some(intelligence) => workspace.cmd_change_agent_role(intelligence, cx),
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

    pub(crate) fn cmd_workstream_rename(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(workstream_id) = self.focused_workstream_or_notice(cx) {
            self.send_to_workstream(
                workstream_id,
                ClientMessage::WorkstreamRename {
                    workstream_id,
                    name,
                },
            );
        }
    }

    /// The workstream the menus act on, or an echo-area notice saying why
    /// there is none (not connected, or nothing selected).
    fn focused_workstream_or_notice(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<rho_ui_proto::WorkstreamId> {
        if !self.require_connected(cx) {
            return None;
        }
        let selected = self.registry.selected_agent().copied();
        let focused = self.focused_workstream(selected);
        if focused.is_none() {
            self.notice_on(None, "no workstream in focus", StyleClass::SystemInfo, cx);
        }
        focused
    }

    /// Adds or removes one label on the focused workstream; the toggles
    /// (pin, hide) and the free-form label prompt all come through here.
    fn send_workstream_label(
        &mut self,
        workstream_id: rho_ui_proto::WorkstreamId,
        label: String,
        add: bool,
    ) {
        self.send_to_workstream(
            workstream_id,
            ClientMessage::WorkstreamLabel {
                workstream_id,
                label,
                add,
            },
        );
    }

    pub(crate) fn cmd_agent_done(
        &mut self,
        hide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require_connected(cx) {
            return;
        }
        let disposition = if hide {
            rho_ui_proto::AgentDisposition::Hidden
        } else {
            rho_ui_proto::AgentDisposition::Done
        };
        let targets = self.disposition_targets(cx);
        let sent = self.set_agent_disposition(targets, "done", disposition, cx);
        // Hiding the open agent closes its tab, or it would stay
        // rail-visible through the selection exemption.
        if hide && sent && self.registry.selected_agent().is_some() {
            self.select_agent(None, window, cx);
        }
    }

    pub(crate) fn cmd_agent_snooze(&mut self, duration_ms: u64, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        let until = rho_core::UnixMs(now_ms().saturating_add(duration_ms));
        let targets = self.disposition_targets(cx);
        self.set_agent_disposition(
            targets,
            "snooze",
            rho_ui_proto::AgentDisposition::Snoozed { until },
            cx,
        );
    }

    pub(crate) fn cmd_workstream_pin(&mut self, cx: &mut Context<Self>) {
        if let Some(workstream_id) = self.focused_workstream_or_notice(cx) {
            let pinned = self
                .registry
                .workstream_labels(workstream_id)
                .iter()
                .any(|label| label == crate::registry::PIN_LABEL);
            self.send_workstream_label(
                workstream_id,
                crate::registry::PIN_LABEL.to_owned(),
                !pinned,
            );
        }
    }

    pub(crate) fn cmd_workstream_hide(&mut self, cx: &mut Context<Self>) {
        if let Some(workstream_id) = self.focused_workstream_or_notice(cx) {
            let hidden = self
                .registry
                .workstream_labels(workstream_id)
                .iter()
                .any(|label| label == crate::registry::HIDE_LABEL);
            self.send_workstream_label(
                workstream_id,
                crate::registry::HIDE_LABEL.to_owned(),
                !hidden,
            );
        }
    }

    pub(crate) fn cmd_workstream_group(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(workstream_id) = self.focused_workstream_or_notice(cx) {
            self.send_workstream_label(
                workstream_id,
                format!("{}{name}", crate::registry::GROUP_LABEL_PREFIX),
                true,
            );
        }
    }

    pub(crate) fn cmd_workstream_label(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(workstream_id) = self.focused_workstream_or_notice(cx) {
            self.send_workstream_label(workstream_id, name, true);
        }
    }

    pub(crate) fn cmd_workstream_unlabel(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(workstream_id) = self.focused_workstream_or_notice(cx) {
            if !self
                .registry
                .workstream_labels(workstream_id)
                .contains(&name)
            {
                let message = format!("no label named `{name}` on this workstream");
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                return;
            }
            self.send_workstream_label(workstream_id, name, false);
        }
    }

    /// Moves every agent of the focused workstream into the named one; the
    /// daemon deletes the emptied source. Merging targets existing streams
    /// only — a typo should not found a stream.
    pub(crate) fn cmd_workstream_merge(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(source_id) = self.focused_workstream_or_notice(cx) else {
            return;
        };
        let Some(target) = self
            .registry
            .workstreams()
            .iter()
            .find(|workstream| workstream.name == name)
            .map(|workstream| workstream.workstream_id)
        else {
            let message = format!("no workstream named `{name}`");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            return;
        };
        if target == source_id {
            self.notice_on(None, "already that workstream", StyleClass::SystemInfo, cx);
            return;
        }
        let Some(source) = self
            .registry
            .workstreams()
            .iter()
            .find(|workstream| workstream.workstream_id == source_id)
        else {
            return;
        };
        // Roots only: a moved agent brings its spawned subtree along.
        let members = source.agent_ids().collect::<Vec<_>>();
        let roots = source
            .agents
            .iter()
            .filter(|agent| {
                agent
                    .parent_agent
                    .is_none_or(|parent| !members.contains(&parent))
            })
            .map(|agent| agent.agent_id)
            .collect::<Vec<_>>();
        for agent_id in roots {
            self.send_to_agent(
                agent_id,
                ClientMessage::AgentMove {
                    agent_id,
                    target: rho_ui_proto::WorkstreamTarget::Existing(target),
                },
            );
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

    pub(crate) fn cmd_open(&mut self, path: Utf8PathBuf, cx: &mut Context<Self>) {
        let Some(agent_id) = self.selected_or_notice("open", cx) else {
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

    pub(crate) fn cmd_shell(&mut self, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_or_notice("shell", cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.open_shell_surface(agent_id, cx);
        }
    }

    pub(crate) fn cmd_shell_close(&mut self, cx: &mut Context<Self>) {
        let Some(agent_id) = self.selected_or_notice("close shell", cx) else {
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

    pub(crate) fn cmd_term(&mut self, new: bool, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_or_notice("term", cx) {
            if !self.require_agent_online(agent_id, cx) {
                return;
            }
            self.open_terminal_surface(agent_id, new, cx);
        }
    }

    pub(crate) fn cmd_diff(&mut self, cx: &mut Context<Self>) {
        let Some(agent_id) = self.selected_or_notice("diff", cx) else {
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
                    .map(|name| crate::commands::Candidate {
                        value: name.clone(),
                        description: "auth namespace".to_owned(),
                    })
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
                workspace.hosts.send(
                    host,
                    ClientMessage::SetAuthNamespace {
                        name: name.to_owned(),
                    },
                );
                workspace.notice_on(
                    None,
                    &format!("setting auth on {} to {name}", workspace.host_label(host)),
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
        if let Some(selected) = self.registry.selected_agent().copied()
            && let Some(workstream_id) = self.registry.workstream_of(selected)
        {
            self.draft_workstream = Some(workstream_id);
        }
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
            self.dashboard_dirty = true;
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
    /// Pane-visible transcripts stay pinned; dashboard previews, pane history,
    /// and hidden transcript surfaces are cache and can be rebuilt lazily.
    fn release_agent_view_cache(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        if let Some(model) = self.models.get(&agent_id).cloned() {
            model.update(cx, |model, _| model.clear_preview_editor());
        }

        for tree in self.contexts.values_mut() {
            tree.for_each_pane_mut(&mut |pane| {
                pane.purge_history(|surface| surface.key == SurfaceKey::Transcript(agent_id));
            });
        }
        let shown = self.contexts.values().any(|tree| {
            tree.panes()
                .iter()
                .any(|pane| pane.surface.key == SurfaceKey::Transcript(agent_id))
        });
        if shown {
            return;
        }

        for surfaces in self.surfaces.values_mut() {
            surfaces.retain(|surface| surface.key != SurfaceKey::Transcript(agent_id));
        }
        self.pending_syncs.remove(&agent_id);
        self.models.remove(&agent_id);
    }

    pub fn open_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_online(agent_id) && !self.subscriptions.contains(agent_id) {
            self.subscribe_agent(agent_id, cx);
        }
        self.select_agent(Some(agent_id), window, cx);
    }

    /// Clears (or snoozes, or files away) an agent's claim on the user's
    /// attention; returns the agent it acted on. The daemon echoes the
    /// resulting attention level back as a broadcast, so the rail updates
    /// through the normal event path.
    /// Whom a triage verdict speaks for: the open agent, or — with none
    /// open, as when `d` comes from the dashboard — the row under the
    /// cursor. Either way it covers everything the row's lamp aggregates,
    /// which is the whole workstream for a stream row and the agent's
    /// subtree for an agent row; acking only the root would leave the row
    /// active on a child's lamp, and the row would never reach settled.
    fn disposition_targets(&mut self, cx: &mut Context<Self>) -> Vec<AgentId> {
        use crate::dashboard::RowTarget;
        if let Some(agent_id) = self.registry.selected_agent().copied() {
            return self.registry.agent_subtree(agent_id);
        }
        match self.dashboard.cursor_target(cx) {
            Some(RowTarget::Stream { workstream_id, .. }) => {
                self.registry.workstream_agents(workstream_id)
            }
            Some(RowTarget::Agent(agent_id)) | Some(RowTarget::Reply(agent_id)) => {
                self.registry.agent_subtree(agent_id)
            }
            _ => Vec::new(),
        }
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
        for agent_id in targets {
            self.send_to_agent(
                agent_id,
                ClientMessage::SetAgentDisposition {
                    agent_id,
                    disposition,
                },
            );
        }
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

    fn focused_workstream(
        &self,
        source_agent: Option<AgentId>,
    ) -> Option<rho_ui_proto::WorkstreamId> {
        source_agent
            .or_else(|| self.registry.selected_agent().copied())
            .and_then(|agent_id| self.registry.workstream_of(agent_id))
            .or(self.draft_workstream)
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

    /// Where a new agent works when the draft doesn't say: the inherited
    /// workstream's newest agent sets the precedent, else the first
    /// registered workdir. All daemon-side data — the GUI may run on another
    /// machine, so its own cwd is meaningless here.
    fn draft_default_workdir(&self) -> Option<HostPath> {
        self.draft_workstream
            .and_then(|workstream_id| {
                Some(HostPath {
                    host: self.registry.host_of_workstream(workstream_id)?,
                    path: self.registry.last_working_directory(workstream_id)?,
                })
            })
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

    pub fn prompt_names(&self) -> crate::commands::PromptNames {
        let workstreams = self.registry.workstreams();
        let mut groups = workstreams
            .iter()
            .filter_map(|workstream| workstream.group.clone())
            .collect::<Vec<_>>();
        groups.sort();
        groups.dedup();
        crate::commands::PromptNames {
            workstreams: workstreams
                .iter()
                .map(|workstream| workstream.name.clone())
                .collect(),
            groups,
            labels: self.registry.workstream_label_names(),
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
        self.hosts
            .focus_agent(self.host_of(agent_id).map(|host| (host, agent_id)));
        self.ensure_duration_timer(cx);
        cx.notify();
    }

    fn select_agent_inner(
        &mut self,
        agent_id: Option<AgentId>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.dashboard_dirty = true;
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

    /// The dashboard cursor moved: preview the row it landed on. Only
    /// while the dashboard owns the keyboard — programmatic cursor
    /// restoration and unfocused syncs never drive the panes.
    fn dashboard_cursor_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        if !self.dashboard.focus_handle(cx).is_focused(window) {
            return;
        }
        match self.dashboard.cursor_target(cx) {
            Some(RowTarget::Iris) => cx.notify(),
            // A reply draft previews its addressee, same as the row above it.
            Some(
                RowTarget::Stream {
                    root: Some(agent_id),
                    ..
                }
                | RowTarget::Agent(agent_id)
                | RowTarget::Reply(agent_id),
            ) if self.dashboard_preview != Some(agent_id) => {
                self.preview_agent(agent_id, window, cx);
            }
            Some(
                RowTarget::Stream { root: Some(_), .. } | RowTarget::Agent(_) | RowTarget::Reply(_),
            ) => {}
            // Headers and the folded tail retain the last preview. Clearing
            // it here would remove the preview pane, resize the dashboard,
            // and visibly rewrap its rows on ordinary cursor motion.
            _ => {}
        }
    }

    /// The active context's surface with the given key, whether or not
    /// any pane currently displays it.
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

    /// Shows the named surface in the focused pane (or focuses a pane
    /// already showing it).
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

    /// Removes a surface from the context. Panes showing it fall back to
    /// their own history, then to the list's most recent conversation
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
            None => self.active_tree().focused().surface.key.clone(),
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
        let fallback = list
            .iter()
            .rev()
            .find(|surface| surface.key.is_conversation())
            .or_else(|| list.last())
            .cloned()
            .expect("list retains at least one surface");

        // Replace the closed surface everywhere it is shown, preferring
        // each pane's own history; only the first history-less pane may
        // take the list's surface directly (a view renders in one pane).
        let mut orphaned = Vec::new();
        self.active_tree_mut().for_each_pane_mut(&mut |pane| {
            pane.purge_history(|surface| surface.key == key);
            if pane.surface.key == key {
                orphaned.push(pane.id);
            }
        });
        // A view renders in one pane: the list's surface may go to one
        // orphan (and only when no pane shows it already), the rest get
        // fresh views.
        let mut fallback_used = self
            .active_tree()
            .pane_showing(|s| s.key == fallback.key)
            .is_some();
        for pane_id in orphaned {
            let went_back = self
                .active_tree_mut()
                .pane_mut(pane_id)
                .is_some_and(|pane| pane.back());
            if went_back {
                continue;
            }
            let replacement = if fallback_used {
                self.duplicate_surface(fallback.clone(), window, cx)
            } else {
                fallback_used = true;
                fallback.clone()
            };
            if let Some(pane) = self.active_tree_mut().pane_mut(pane_id) {
                pane.surface = replacement;
            }
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

    /// Emacs `display-buffer`: the one place pane choice happens. The
    /// surface joins the context's surface list first, so it stays alive
    /// however panes shuffle afterwards. A pane already showing it wins
    /// (the arrangement stays intact and no view is shown twice);
    /// otherwise the focused pane shows it — never any other split, so
    /// switching agents only ever changes the pane you're in. Founds the
    /// context's tree on its first visit.
    fn display_surface(&mut self, surface: Surface) {
        use std::collections::hash_map::Entry;
        let list = self.surfaces.entry(self.active_context).or_default();
        match list.iter_mut().find(|s| **s == surface) {
            Some(existing) => *existing = surface.clone(),
            None => list.push(surface.clone()),
        }
        let tree = match self.contexts.entry(self.active_context) {
            Entry::Vacant(entry) => {
                entry.insert(PaneTree::new(surface));
                return;
            }
            Entry::Occupied(entry) => entry.into_mut(),
        };
        if let Some(pane) = tree.pane_showing(|s| s.key == surface.key) {
            tree.focus(pane);
        }
        tree.focused_mut().show(surface);
    }

    /// `:open`: reuses the agent workspace's remote buffer registry and shows
    /// the file surface in the main pane.
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
                        let surface = Self::wrap_surface(key, SurfaceView::File(view), window, cx);
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
                        let surface = Self::wrap_surface(
                            key,
                            SurfaceView::Shell { model, editor },
                            window,
                            cx,
                        );
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
                        let surface = Self::wrap_surface(key, SurfaceView::Diff(view), window, cx);
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
                        let surface =
                            Self::wrap_surface(key, SurfaceView::Terminal(view), window, cx);
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
        let directory_label = self.working_directory_label(agent_id);
        let workspace_label = self.registry.workspace_id_label(*agent_id);
        let usage_label = self.store.get(agent_id).map(|state| {
            let usage = &state.usage.total;
            format!(
                "${:.2}",
                crate::transient::bucket_cost_usd(usage, &state.usage.provider)
            )
        });
        let role_label = self.role_label(agent_id);
        let context_used = self
            .store
            .get(agent_id)
            .and_then(|state| state.context_used);
        view.update(cx, |view, cx| {
            view.set_status(
                &directory_label,
                workspace_label.as_deref(),
                usage_label.as_deref(),
                role_label
                    .as_ref()
                    .map(|label| (label.text.as_str(), label.family)),
                context_used,
                cx,
            )
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
        self.dashboard.sync(&self.registry, window, cx);
        self.dashboard_dirty = false;
    }

    #[cfg(test)]
    pub(crate) fn dashboard_is_dirty(&self) -> bool {
        self.dashboard_dirty
    }

    #[cfg(test)]
    pub(crate) fn preview_dashboard_agent(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preview_agent(agent_id, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn dashboard_preview_agent(&self) -> Option<AgentId> {
        self.dashboard_preview
    }

    fn sync_dashboard_if_dirty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard_dirty {
            self.dashboard.sync(&self.registry, window, cx);
            self.dashboard_dirty = false;
        }
    }

    #[cfg(test)]
    pub(crate) fn dashboard_fold_count(&self) -> usize {
        self.dashboard.fold_count()
    }

    #[cfg(test)]
    pub(crate) fn dashboard_rail_tail_id(&self) -> Option<editor::DisplayElisionId> {
        self.dashboard.rail_tail_id()
    }

    #[cfg(test)]
    pub(crate) fn dashboard_open_reply(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
        self.dashboard.open_reply(agent_id, cx);
        self.dashboard_dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn dashboard_rail_tail_ends_in_reply(&self, agent_id: AgentId) -> bool {
        self.dashboard.rail_tail_ends_in_reply(agent_id)
    }

    #[cfg(test)]
    pub(crate) fn dashboard_cursor_target(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<crate::dashboard::RowTarget> {
        self.dashboard.cursor_target(cx)
    }

    #[cfg(test)]
    pub(crate) fn triage_targets(&mut self, cx: &mut Context<Self>) -> Vec<AgentId> {
        self.disposition_targets(cx)
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

    /// The editor the user is typing into: the focused pane's own editor
    /// (each transcript pane has one). Terminal panes have no editor; the
    /// draft's stands in for text-style queries.
    pub(crate) fn active_editor(&self, cx: &gpui::App) -> Entity<editor::Editor> {
        match &self.active_tree().focused().surface.view {
            SurfaceView::Draft { editor, .. } => editor.clone(),
            SurfaceView::Transcript { editor, .. } => editor.clone(),
            SurfaceView::File(view) => view.read(cx).editor().clone(),
            SurfaceView::Shell { editor, .. } => editor.clone(),
            SurfaceView::Diff(view) => view.read(cx).editor().clone(),
            SurfaceView::Terminal(_) => self
                .any_draft_editor()
                .expect("the draft context always holds a draft surface"),
            #[cfg(feature = "native")]
            SurfaceView::ZulipInbox(view) => view.read(cx).editor().clone(),
            #[cfg(feature = "native")]
            SurfaceView::ZulipNarrow(view) => view.read(cx).editor().clone(),
        }
    }

    /// The focused pane's draft editor, when the focused pane shows the
    /// draft — cursor-dependent draft operations act on it.
    fn focused_draft_editor(&self) -> Option<Entity<editor::Editor>> {
        match &self.active_tree().focused().surface.view {
            SurfaceView::Draft { editor, .. } => Some(editor.clone()),
            _ => None,
        }
    }

    /// Some draft editor, from the draft context's surface list (founded at
    /// startup, never pruned). Used only where any editor serves, e.g. text
    /// style for chrome while a terminal pane is focused.
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
        match &self.active_tree().focused().surface.view {
            SurfaceView::Draft { editor, .. } => editor.focus_handle(cx),
            SurfaceView::Transcript { editor, .. } => editor.focus_handle(cx),
            SurfaceView::File(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Shell { editor, .. } => editor.focus_handle(cx),
            SurfaceView::Diff(view) => view.read(cx).editor().focus_handle(cx),
            SurfaceView::Terminal(view) => view.read(cx).focus_handle(cx),
            #[cfg(feature = "native")]
            SurfaceView::ZulipInbox(view) => view.read(cx).editor().focus_handle(cx),
            #[cfg(feature = "native")]
            SurfaceView::ZulipNarrow(view) => view.read(cx).editor().focus_handle(cx),
        }
    }

    /// Moves gpui focus to the focused pane's surface. If a modal overlay
    /// owns the keyboard, update where it will return instead of stealing
    /// focus from it.
    fn focus_active_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.active_surface_focus(cx);
        if self.has_modal_overlay() {
            self.overlay_return_focus = Some(handle);
        } else {
            window.focus(&handle, cx);
        }
    }

    /// The surface for `key`, reusing the live one (and its focus observer)
    /// when some pane in the active context already shows or remembers it.
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
                SurfaceView::Draft { model, editor }
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
        Self::wrap_surface(key, view, window, cx)
    }

    /// A surface for a new pane over the same content as `surface`: every
    /// pane gets its own view (own cursor, scroll, folds — or for
    /// terminals, own focus and mode) over the shared model.
    fn duplicate_surface(
        &mut self,
        surface: Surface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Surface {
        match &surface.view {
            SurfaceView::File(view) => {
                let (project, buffer) = view.read(cx).shared_content();
                let view = cx.new(|cx| FileView::new(project, buffer, window, cx));
                Self::wrap_surface(surface.key.clone(), SurfaceView::File(view), window, cx)
            }
            SurfaceView::Diff(view) => {
                let model = view.read(cx).model();
                let view = cx.new(|cx| crate::diff_view::DiffView::new(model, window, cx));
                Self::wrap_surface(surface.key.clone(), SurfaceView::Diff(view), window, cx)
            }
            SurfaceView::Transcript { model, .. } => {
                let model = model.clone();
                let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
                Self::wrap_surface(
                    surface.key.clone(),
                    SurfaceView::Transcript { model, editor },
                    window,
                    cx,
                )
            }
            SurfaceView::Draft { model, .. } => {
                let model = model.clone();
                let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
                Self::wrap_surface(
                    surface.key.clone(),
                    SurfaceView::Draft { model, editor },
                    window,
                    cx,
                )
            }
            SurfaceView::Shell { model, .. } => {
                let model = model.clone();
                let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
                Self::wrap_surface(
                    surface.key.clone(),
                    SurfaceView::Shell { model, editor },
                    window,
                    cx,
                )
            }
            // Terminals share one model (one wire client) but each pane
            // gets its own view: own focus, scroll offset, and mode. Only
            // the focused view sizes the pty, so splits don't fight.
            SurfaceView::Terminal(view) => {
                let model = view.read(cx).model().clone();
                let view = cx.new(|cx| crate::terminal_view::TerminalView::new(model, cx));
                Self::wrap_surface(surface.key.clone(), SurfaceView::Terminal(view), window, cx)
            }
            // Chat surfaces hold one editor over one conversation: a split
            // shows the same view rather than a second cursor over the
            // same messages, which no one has ever wanted.
            #[cfg(feature = "native")]
            SurfaceView::ZulipInbox(_) | SurfaceView::ZulipNarrow(_) => surface.clone(),
        }
    }

    /// Wraps a view as a surface with a focus-follow observer: gpui focus
    /// arriving inside its editor (mouse click, vim motion) moves pane
    /// focus and the agent context along.
    fn wrap_surface(
        key: SurfaceKey,
        view: SurfaceView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Surface {
        let (handle, editor_id) = match &view {
            SurfaceView::Draft { editor, .. } => (editor.focus_handle(cx), editor.entity_id()),
            SurfaceView::Transcript { editor, .. } => (editor.focus_handle(cx), editor.entity_id()),
            SurfaceView::File(view) => {
                let editor = view.read(cx).editor();
                (editor.focus_handle(cx), editor.entity_id())
            }
            SurfaceView::Shell { editor, .. } => (editor.focus_handle(cx), editor.entity_id()),
            SurfaceView::Diff(view) => {
                let editor = view.read(cx).editor();
                (editor.focus_handle(cx), editor.entity_id())
            }
            // Terminals have no editor; the view itself carries focus.
            SurfaceView::Terminal(view) => (view.read(cx).focus_handle(cx), view.entity_id()),
            #[cfg(feature = "native")]
            SurfaceView::ZulipInbox(view) => {
                let editor = view.read(cx).editor().clone();
                (editor.focus_handle(cx), editor.entity_id())
            }
            #[cfg(feature = "native")]
            SurfaceView::ZulipNarrow(view) => {
                let editor = view.read(cx).editor().clone();
                (editor.focus_handle(cx), editor.entity_id())
            }
        };
        let focus_follow = cx.on_focus_in(&handle, window, move |this, _window, cx| {
            this.surface_focused(editor_id, cx);
        });
        Surface {
            key,
            view,
            editor_id: Some(editor_id),
            _focus_follow: Some(std::rc::Rc::new(focus_follow)),
        }
    }

    fn surface_focused(&mut self, editor_id: gpui::EntityId, cx: &mut Context<Self>) {
        let tree = self.active_tree();
        if tree.focused().surface.editor_id == Some(editor_id) {
            return;
        }
        if let Some(id) = tree.pane_showing(|s| s.editor_id == Some(editor_id)) {
            self.active_tree_mut().focus(id);
            self.sync_selection_to_focus(cx);
        }
    }

    /// Keeps the registry's notion of "current agent" in step with the
    /// focused pane, so `:` commands resolve against what the user sees.
    fn sync_selection_to_focus(&mut self, cx: &mut Context<Self>) {
        let selected = match self.active_tree().focused().surface.key.clone() {
            SurfaceKey::Transcript(agent_id) | SurfaceKey::Shell(agent_id) => {
                self.registry.select_agent(agent_id);
                Some(agent_id)
            }
            SurfaceKey::Terminal { agent_id, .. } => {
                self.registry.select_agent(agent_id);
                Some(agent_id)
            }
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
        self.dashboard_dirty = true;
        cx.notify();
    }

    pub(crate) fn split_pane(
        &mut self,
        axis: SplitAxis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = self.active_tree().focused().surface.clone();
        let sibling = self.duplicate_surface(focused, window, cx);
        self.active_tree_mut().split(axis, sibling);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    pub(crate) fn close_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let closed = match self.active_tree().focused().surface.key {
            SurfaceKey::Transcript(agent_id) => Some(agent_id),
            _ => None,
        };
        self.active_tree_mut().close_focused();
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        if let Some(agent_id) = closed
            && !self.subscriptions.contains(agent_id)
        {
            self.release_agent_view_cache(agent_id, cx);
        }
        cx.notify();
    }

    pub(crate) fn focus_pane_by_delta(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_tree_mut().focus_by_delta(delta);
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    pub(crate) fn pane_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_tree_mut().focused_mut().back() {
            self.sync_selection_to_focus(cx);
            self.focus_active_surface(window, cx);
            cx.notify();
        }
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
        let (input, on_submit) = minibuffer.into_submission(cx);
        self.finish_overlay_focus(window, cx);
        on_submit(self, input, window, cx);
        cx.notify();
    }

    fn minibuffer_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.take().is_some() {
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
        transient.retain_applicable(self);
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

    pub(crate) fn has_selected_agent(&self) -> bool {
        self.registry.selected_agent().is_some()
    }

    pub(crate) fn has_focused_workstream(&self) -> bool {
        let selected = self.registry.selected_agent().copied();
        self.focused_workstream(selected).is_some()
    }

    pub(crate) fn prompt_snooze(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &gpui::App| Vec::new());
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                let input = input.trim();
                if input.is_empty() {
                    return;
                }
                match parse_duration_ms(input) {
                    Some(duration_ms) => workspace.cmd_agent_snooze(duration_ms, cx),
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

    pub(crate) fn prompt_workstream(
        &mut self,
        kind: WorkstreamPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let names = {
            let names = self.prompt_names();
            match kind {
                WorkstreamPrompt::Merge => names.workstreams,
                WorkstreamPrompt::Group => names.groups,
                WorkstreamPrompt::Label | WorkstreamPrompt::Unlabel => names.labels,
                WorkstreamPrompt::Rename => Vec::new(),
            }
        };
        let complete = std::rc::Rc::new(move |_: &Workspace, input: &str, _: &gpui::App| {
            let needle = input.trim().to_lowercase();
            names
                .iter()
                .filter(|name| name.to_lowercase().contains(&needle))
                .map(|name| crate::commands::Candidate {
                    value: name.clone(),
                    description: String::new(),
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  _window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let name = input.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                match kind {
                    WorkstreamPrompt::Merge => workspace.cmd_workstream_merge(name, cx),
                    WorkstreamPrompt::Group => workspace.cmd_workstream_group(name, cx),
                    WorkstreamPrompt::Label => workspace.cmd_workstream_label(name, cx),
                    WorkstreamPrompt::Unlabel => workspace.cmd_workstream_unlabel(name, cx),
                    WorkstreamPrompt::Rename => workspace.cmd_workstream_rename(name, cx),
                }
            },
        );
        self.open_prompt(kind.prompt(), complete, on_submit, window, cx);
    }

    /// Prompt for a path to open from the current agent's workspace.
    pub(crate) fn prompt_open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &gpui::App| Vec::new());
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                let path = input.trim().to_owned();
                if !path.is_empty() {
                    workspace.cmd_open(camino::Utf8PathBuf::from(path), cx);
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
             _window: &mut Window,
             cx: &mut Context<Workspace>| {
                let input = input.trim();
                let turns = if input.is_empty() {
                    Some(1)
                } else {
                    input.parse::<u32>().ok().filter(|turns| *turns > 0)
                };
                match turns {
                    Some(turns) => workspace.cmd_rewind(turns, cx),
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

    /// `space r`: the dashboard is ambient chrome, not a pane — focus jumps
    /// to it directly and never lands on it through the pane cycle.
    pub(crate) fn focus_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.dashboard.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    pub(crate) fn open_new_agent_transient(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_agent_draft.is_none() {
            use crate::dashboard::RowTarget;

            let contextual = if self.dashboard.focus_handle(cx).is_focused(window) {
                match self.dashboard.cursor_target(cx) {
                    Some(RowTarget::Stream {
                        root: Some(agent_id),
                        ..
                    })
                    | Some(RowTarget::Agent(agent_id))
                    | Some(RowTarget::Reply(agent_id)) => self.agent_workdir(agent_id),
                    Some(RowTarget::Stream { workstream_id, .. }) => self
                        .registry
                        .host_of_workstream(workstream_id)
                        .zip(self.registry.last_working_directory(workstream_id))
                        .map(|(host, path)| HostPath { host, path }),
                    _ => None,
                }
            } else {
                self.registry
                    .selected_agent()
                    .copied()
                    .and_then(|agent_id| self.agent_workdir(agent_id))
            };
            let workdir = contextual.or_else(|| match self.workdirs.as_slice() {
                [workdir] => Some(HostPath {
                    host: workdir.host,
                    path: workdir.project.path.clone(),
                }),
                _ => None,
            });
            self.new_agent_draft = Some(NewAgentDraft {
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
        self.open_transient(crate::transient::usage_menu(history, days), window, cx);
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

    pub(crate) fn compose_new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.new_agent_draft else {
            return;
        };
        let project = draft
            .workdir
            .as_ref()
            .map(|workdir| self.workdir_label(workdir))
            .unwrap_or_else(|| "no project".to_owned());
        let host = draft
            .host
            .map(|host| self.host_label(host))
            .unwrap_or_else(|| "no host".to_owned());
        let summary = format!(
            "{host} · {project} · {} · {}",
            draft.role,
            draft.workspace.label()
        );
        self.dashboard.open_new_draft(summary, cx);
        self.dashboard_dirty = true;
        let handle = self.dashboard.focus_handle(cx);
        window.focus(&handle, cx);
        self.dashboard_enter_insert(window, cx);
    }

    fn new_agent_launch(&self) -> Result<(HostId, rho_ui_proto::StartMode, AgentRole), String> {
        let draft = self
            .new_agent_draft
            .as_ref()
            .ok_or_else(|| "new agent has no launch configuration".to_owned())?;
        let (mode, target) = draft.workspace.mode_and_target();
        let (host, start) = self.parse_start(mode, target, draft.workdir.clone(), draft.host)?;
        let role = parse_agent_role(&draft.role)?;
        Ok((host, start, role))
    }

    /// `enter` in the dashboard: act on the row under the cursor.
    fn dashboard_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        match self.dashboard.cursor_target(cx) {
            Some(RowTarget::Iris) => self.cmd_voice(window, cx),
            #[cfg(feature = "native")]
            Some(RowTarget::Zulip) => self.open_zulip(window, cx),
            #[cfg(not(feature = "native"))]
            Some(RowTarget::Zulip) => {}
            Some(RowTarget::Stream {
                root: Some(agent_id),
                ..
            })
            | Some(RowTarget::Agent(agent_id)) => self.open_agent(agent_id, window, cx),
            // Enter sends the inline reply draft (and closes it); an empty
            // draft just closes. Disconnected, the draft stays parked
            // rather than being consumed into the void.
            Some(RowTarget::Reply(agent_id)) => {
                if !self.require_connected(cx) {
                    return;
                }
                let content = self.dashboard.take_reply(agent_id, cx);
                self.dashboard_dirty = true;
                if let Some(content) = content {
                    self.handle_submit(agent_id, content, cx);
                }
                // Removing the draft's excerpt would drop the cursor onto
                // whatever text slid into the gap; park it back on the row
                // the reply belonged to.
                if let Some(workstream_id) = self.registry.workstream_of(agent_id) {
                    self.dashboard.cursor_to_agent(agent_id, workstream_id, cx);
                }
                self.dashboard_exit_insert(window, cx);
            }
            Some(RowTarget::NewDraft) => {
                if !self.require_connected(cx) {
                    return;
                }
                let (host, start, role) = match self.new_agent_launch() {
                    Ok(launch) => launch,
                    Err(message) => {
                        self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                        return;
                    }
                };
                let content = self.dashboard.take_new_draft(cx);
                self.dashboard_dirty = true;
                if let Some(content) = content {
                    self.create_inline_agent(host, content, start, role);
                }
                self.new_agent_draft = None;
                self.dashboard_exit_insert(window, cx);
            }
            Some(RowTarget::Stream { root: None, .. }) | Some(RowTarget::None) | None => {}
        }
    }

    #[cfg(all(target_family = "wasm", not(feature = "native")))]
    fn dashboard_open_clicked_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        match self.dashboard.cursor_target(cx) {
            Some(RowTarget::Stream {
                root: Some(agent_id),
                ..
            })
            | Some(RowTarget::Agent(agent_id)) => self.open_agent(agent_id, window, cx),
            _ => {}
        }
    }

    fn create_inline_agent(
        &mut self,
        host: HostId,
        content: Vec<ContentPart>,
        start: rho_ui_proto::StartMode,
        role: AgentRole,
    ) {
        self.hosts.send(
            host,
            ClientMessage::NewAgent {
                workstream: None,
                role,
                start,
                content: Some(content),
            },
        );
    }

    /// `r` in the dashboard: splice an inline reply draft under the row —
    /// the cursor moves into it, in insert mode, but never leaves the
    /// dashboard. Drafts park where they are: wander off mid-thought and
    /// come back later.
    fn dashboard_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        match self.dashboard.cursor_target(cx) {
            Some(RowTarget::Stream {
                root: Some(agent_id),
                ..
            })
            | Some(RowTarget::Agent(agent_id))
            | Some(RowTarget::Reply(agent_id)) => {
                self.dashboard.open_reply(agent_id, cx);
                self.dashboard_dirty = true;
                self.dashboard_enter_insert(window, cx);
            }
            _ => {
                self.notice_on(
                    None,
                    "reply: no agent under the cursor",
                    StyleClass::SystemInfo,
                    cx,
                );
            }
        }
    }

    /// A draft was just opened under the cursor: drop the editor into
    /// insert mode so typing starts immediately (writing is the only
    /// reason these drafts exist).
    fn dashboard_enter_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(action) = cx.build_action("vim::InsertBefore", None) {
            window.dispatch_action(action, cx);
        }
    }

    /// Sending a draft ends the writing; the cursor goes back to being a
    /// dashboard cursor, not an insertion point.
    fn dashboard_exit_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(action) = cx.build_action("vim::SwitchToNormalMode", None) {
            window.dispatch_action(action, cx);
        }
    }

    /// The home-mode dashboard beside the active context's preview.
    fn render_rail(
        &mut self,
        show_panes: bool,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
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
            .w(if show_panes {
                gpui::relative(0.55)
            } else {
                gpui::relative(1.0)
            })
            .pl(px(24.))
            .pr(px(24.))
            .child(self.render_dashboard_header(text_style, cx));
        let dashboard = div()
            .id("dashboard-rail")
            .flex_grow(1.0)
            .min_h_0()
            .child(self.dashboard.editor().clone());
        #[cfg(all(target_family = "wasm", not(feature = "native")))]
        let dashboard = dashboard
            // The editor consumes bubble-phase mouse events. Capture the
            // press/release around it, then open after its cursor has moved.
            .capture_any_mouse_down(cx.listener(Self::dashboard_pointer_down))
            .capture_any_mouse_up(cx.listener(Self::dashboard_pointer_up));
        container.child(dashboard).into_any_element()
    }

    /// The dashboard-only two-line masthead.
    fn render_dashboard_header(
        &self,
        text_style: &gpui::TextStyle,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let now = now_ms() as f64 / 1_000.0;
        let mut stats = div().flex().items_center().gap(px(6.));
        let format_reset = |reset_at_unix: Option<i64>| {
            reset_at_unix
                .map(|reset| reset as f64 - now)
                .filter(|seconds| *seconds > 0.0)
                .map(|seconds| format!(" {:.1}d", seconds / 86_400.0))
                .unwrap_or_default()
        };
        let summaries = self.merged_quota_summaries();
        let gpt = summaries
            .iter()
            .filter(|summary| summary.model == "gpt")
            .collect::<Vec<_>>();
        for summary in &gpt {
            stats = stats.child(
                div()
                    .text_color(colors.terminal_ansi_cyan)
                    .child(format!("{}%", summary.remaining_percent)),
            );
        }
        let opus = summaries.iter().find(|summary| summary.model == "opus");
        let fable = summaries.iter().find(|summary| summary.model == "fable");
        if opus.is_some() || fable.is_some() {
            if !gpt.is_empty() {
                stats = stats.child(div().text_color(text_style.color.opacity(0.55)).child("·"));
            }
            let mut claude = String::new();
            if let Some(opus) = opus {
                claude.push_str(&format!("{}%", opus.remaining_percent));
            }
            if let Some(fable) = fable {
                if !claude.is_empty() {
                    claude.push(' ');
                }
                claude.push_str(&format!("{}%", fable.remaining_percent));
            }
            let reset = opus
                .and_then(|summary| summary.reset_at_unix)
                .or_else(|| fable.and_then(|summary| summary.reset_at_unix));
            claude.push_str(&format_reset(reset));
            stats = stats.child(div().text_color(gpui::rgb(0xd97757)).child(claude));
        }
        div()
            .w_full()
            .h(px(60.))
            .pb(px(20.))
            .flex()
            .items_center()
            .gap(px(10.))
            .child(
                div().w(px(40.)).h_full().p(px(4.)).pt(px(6.)).child(
                    svg()
                        .path("icons/rho.svg")
                        .size_full()
                        .text_color(colors.text_accent),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().font_weight(gpui::FontWeight::BOLD).child("rho"))
                    .child(
                        div()
                            .text_color(text_style.color.opacity(0.75))
                            .child(stats),
                    ),
            )
            .into_any_element()
    }

    /// The preview sheet's bottom bar: the previewed agent's name and the
    /// status chips (working directory, workspace, role, context used) —
    /// left-aligned, real chrome on the sheet rather than a prompt row in
    /// the transcript. A quiet modeline, not a header.
    fn render_preview_bar(
        &self,
        text_style: &gpui::TextStyle,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let agent_id = self.dashboard_preview?;
        let spans = self
            .models
            .get(&agent_id)
            .map(|model| model.read(cx).status_spans().to_vec())
            .unwrap_or_default();
        if spans.is_empty() {
            return None;
        }
        Some(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .justify_end()
                .items_baseline()
                .gap(px(12.))
                .px(px(12.))
                .py(px(5.))
                .font_family(text_style.font_family.clone())
                .text_size(text_style.font_size)
                .line_height(text_style.line_height)
                .text_color(text_style.color)
                .children(
                    spans
                        .into_iter()
                        .filter(|(text, _)| !text.trim().is_empty())
                        .map(|(text, style)| {
                            let mut chip =
                                div().text_color(style.color.unwrap_or(text_style.color));
                            if let Some(weight) = style.font_weight {
                                chip = chip.font_weight(weight);
                            }
                            chip.child(text)
                        }),
                )
                .into_any_element(),
        )
    }

    /// The Iris document or selected agent preview editor.
    fn selected_preview_editor(
        &mut self,
        iris: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<editor::Editor>> {
        if iris {
            return Some(self.iris_preview.clone());
        }
        let agent_id = self.dashboard_preview?;
        let model = self.models.get(&agent_id)?.clone();
        Some(model.update(cx, |model, cx| model.preview_editor(window, cx)))
    }

    fn render_panes(
        &mut self,
        window: &mut Window,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Home mode: the dashboard owns the keyboard, so it owns the frame;
        // the panes are its preview. With nothing selected there is
        // nothing to preview — the dashboard takes the whole frame.
        // Modal overlays borrow keyboard focus; the frame stays in the mode
        // recorded beneath the overlay for its whole replacement chain.
        let home = self.dashboard_mode(window, cx);
        let iris = home
            && matches!(
                self.dashboard.cursor_target(cx),
                Some(crate::dashboard::RowTarget::Iris)
            );
        self.sync_diff_visibility(!home, cx);
        let show_panes = !home || iris || self.dashboard_preview.is_some();
        let rail = home.then(|| self.render_rail(show_panes, text_style, cx));
        // Same hairline the rail uses against the panes.
        let separator_color = cx.theme().colors().border_variant.opacity(0.6);
        let mut preview_text_style = text_style.clone();
        preview_text_style.font_size =
            (text_style.font_size.to_pixels(window.rem_size()) * 0.85).into();
        preview_text_style.line_height =
            (text_style.line_height_in_pixels(window.rem_size()) * 0.85).into();
        let preview_bar = (home && !iris)
            .then(|| self.render_preview_bar(&preview_text_style, cx))
            .flatten();
        let preview = home
            .then(|| self.selected_preview_editor(iris, window, cx))
            .flatten();
        let mut leaf = |pane: &crate::pane::Pane<Surface>| -> gpui::AnyElement {
            let id = pane.id;
            let content = self.render_surface(&pane.surface);
            // `flex: 1 1 0` — basis zero, so splits share space by pane
            // count alone and content (a terminal's widest row) can never
            // move a split edge.
            div()
                .h_full()
                .overflow_hidden()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        if this.active_tree().focused_id() != id {
                            this.active_tree_mut().focus(id);
                            this.sync_selection_to_focus(cx);
                            this.focus_active_surface(window, cx);
                        }
                    }),
                )
                .child(content)
                .into_any_element()
        };
        let mut container = |axis: SplitAxis, children: Vec<gpui::AnyElement>| {
            let element = div().flex().size_full().flex_1().min_h_0().min_w_0();
            let element = match axis {
                SplitAxis::Row => element.flex_row(),
                SplitAxis::Column => element.flex_col(),
            };
            let mut separated = Vec::with_capacity(children.len() * 2);
            for (index, child) in children.into_iter().enumerate() {
                if index > 0 {
                    let separator = match axis {
                        SplitAxis::Row => div().w(px(1.)).h_full(),
                        SplitAxis::Column => div().h(px(1.)).w_full(),
                    };
                    separated.push(separator.flex_none().bg(separator_color).into_any_element());
                }
                separated.push(child);
            }
            element.children(separated).into_any_element()
        };
        let panes = show_panes.then(|| {
            let element = div().flex_1().min_w_0().min_h_0();
            // Home mode uses a narrow preview card with the original top
            // inset, anchored to the bottom-right of the pane area rather
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
                        )
                        .children(preview_bar),
                )
            } else {
                element
                    .h_full()
                    .child(self.active_tree().layout(&mut leaf, &mut container))
            }
        });
        div()
            .flex()
            .flex_row()
            .w_full()
            .flex_grow(1.0)
            .min_h_0()
            .children(rail)
            .children(panes)
            .into_any_element()
    }

    fn dashboard_mode(&self, window: &Window, cx: &App) -> bool {
        let dashboard = self.dashboard.focus_handle(cx);
        dashboard.is_focused(window) || self.overlay_return_focus.as_ref() == Some(&dashboard)
    }

    /// Hidden surfaces stay alive as editor buffers, but they must not turn
    /// worktree events into jj manifest traffic. Only models currently shown
    /// in an active pane are allowed to refresh.
    fn sync_diff_visibility(&self, panes_visible: bool, cx: &mut Context<Self>) {
        let visible = if panes_visible {
            self.active_tree()
                .panes()
                .into_iter()
                .filter_map(|pane| match &pane.surface.view {
                    SurfaceView::Diff(view) => Some(view.read(cx).model().entity_id()),
                    _ => None,
                })
                .collect::<HashSet<_>>()
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
        }
    }

    fn update_statuses(&self, cx: &mut Context<Self>) {
        for (agent_id, view) in &self.models {
            self.refresh_view_status(agent_id, view, cx);
        }
    }

    /// The bottom strip's connection notice. With several hosts attached the
    /// unhealthiest one speaks, named — a daemon being down says nothing
    /// about the others, and a nameless "disconnected" would imply it did.
    fn render_connection_status(
        &self,
        text_style: &gpui::TextStyle,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let (name, status) = self.hosts.worst_status()?;
        let subject = match self.hosts.len() > 1 {
            true => format!("{name} · "),
            false => String::new(),
        };
        let (text, class) = match status {
            HostStatus::Connecting => (
                format!("{subject}connecting to rho daemon…"),
                StyleClass::SystemInfo,
            ),
            HostStatus::Recovering(elapsed) => {
                let seconds = elapsed.as_secs();
                let elapsed = if seconds < 60 {
                    format!("{seconds}s")
                } else {
                    format!("{}m {:02}s", seconds / 60, seconds % 60)
                };
                (
                    format!("{subject}connection interrupted · recovering · {elapsed}"),
                    StyleClass::SystemImportant,
                )
            }
            HostStatus::Disconnected(reason) => (
                format!("{subject}disconnected from rho daemon · {reason}"),
                StyleClass::Disconnect,
            ),
            HostStatus::Online => return None,
        };
        let mut strip = bottom_strip(text_style, cx);
        if let Some(color) = class.resolve(cx).color {
            strip = strip.text_color(color);
        }
        Some(strip.child(div().px_2().child(text)).into_any_element())
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

    /// Chip label: the agent's own working directory, when its summary has
    /// arrived.
    fn working_directory_label(&self, agent_id: &AgentId) -> String {
        let Some(directory) = self.registry.working_directory(*agent_id) else {
            return String::new();
        };
        directory
            .file_name()
            .map(str::to_owned)
            .unwrap_or_else(|| directory.to_string())
    }

    fn role_label(&self, agent_id: &AgentId) -> Option<RoleLabel> {
        self.registry.agent_role(*agent_id).map(agent_role_label)
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
        "pm" => Ok(AgentRole::pm()),
        other => Err(format!(
            "unknown role `{other}`; use eng, eng-mini, eng-low, eng-cheap, eng-high, eng-ultra, eng-alt, or pm"
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
        } => "pm",
        AgentRole::Advisor { .. } => "eng",
        AgentRole::PM | AgentRole::WorkflowPM { .. } => "eng-mini",
        AgentRole::Iris => "eng-mini",
    }
}

struct RoleLabel {
    text: String,
    family: RoleFamily,
}

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
        let connection_status = self.render_connection_status(&text_style, cx);
        self.sync_dashboard_if_dirty(window, cx);
        div()
            .id("rho-gui")
            .size_full()
            .flex()
            .flex_col()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .key_context("RhoGui")
            .on_action(cx.listener(Self::submit_prompt))
            .on_action(cx.listener(Self::paste_prompt))
            .on_action(cx.listener(Self::shell_interrupt))
            .on_action(cx.listener(Self::toggle_voice))
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
                this.open_new_agent_transient(window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentJumpAttention, window, cx| {
                this.jump_to_attention(window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentDone, window, cx| {
                this.cmd_agent_done(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentHide, window, cx| {
                this.cmd_agent_done(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardReply, window, cx| {
                this.dashboard_reply(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardNewAgent, window, cx| {
                this.open_new_agent_transient(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DashboardToggleSubagents, _, cx| {
                this.dashboard.toggle_subagents(cx);
                this.dashboard_dirty = true;
            }))
            .on_action(cx.listener(|this, _: &TaskBoard, _window, cx| {
                this.notice_on(
                    None,
                    "task board is not available yet",
                    StyleClass::SystemInfo,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &RoleCycle, window, cx| {
                this.cycle_draft_field(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RoleCycleGroup, window, cx| {
                this.cycle_draft_group(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneSplitRight, window, cx| {
                this.split_pane(SplitAxis::Row, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneSplitDown, window, cx| {
                this.split_pane(SplitAxis::Column, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneClose, window, cx| {
                this.close_pane(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneFocusNext, window, cx| {
                this.focus_pane_by_delta(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneBack, window, cx| {
                this.pane_back(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RailFocus, window, cx| {
                this.focus_rail(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RailOpen, window, cx| {
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
            .child(self.render_panes(window, &text_style, cx))
            .children(
                match (
                    &self.pending_git_approval,
                    &self.minibuffer,
                    &self.transient,
                    connection_status,
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
                    (None, Some(minibuffer), _, _, _) => Some(minibuffer.render(&text_style, cx)),
                    (None, None, Some(transient), _, _) => Some(
                        div()
                            .track_focus(&self.transient_focus)
                            .on_key_down(cx.listener(Self::transient_key))
                            .child(transient.render(&text_style, cx))
                            .into_any_element(),
                    ),
                    (None, None, None, Some(status), _) => Some(status),
                    (None, None, None, None, Some(echo)) => Some(echo.render(&text_style, cx)),
                    (None, None, None, None, None) => None,
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
        assert!(parse_agent_role("pm ultra").is_err());
        assert!(parse_agent_role("eng-ultra-fast").is_err());
        assert!(parse_agent_role("advisor high").is_err());
    }

    #[test]
    fn labels_agent_role() {
        let label = agent_role_label(AgentRole::pm());
        assert_eq!(label.text, "pm");
    }
}
