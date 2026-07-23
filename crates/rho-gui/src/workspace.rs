//! Root entity: owns the daemon connection, the canonical agent states, the
//! registry, and one persistent [`AgentModel`] per opened agent.
//!
//! All protocol events flow through [`Workspace`]; queued frame runs are
//! merged per agent, and views receive summarized changes rather than the
//! protocol itself.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, Task, Window, div, px};
use rho_core::ContentPart;
use rho_ui_proto::{
    AdvisorIntelligence, AgentId, AgentRole, ClientMessage, EngineerIntelligence, MessageDelivery,
};
use theme::ActiveTheme as _;

use crate::agent_view::AgentModel;
use crate::chime::Chime;
use crate::connection::{ConnEvent, Connection, GitApprovalDecision};
use crate::draft_view::DraftModel;
use crate::minibuffer::{ECHO_DURATION, Echo, Minibuffer, bottom_strip};
use crate::pane::{PaneTree, SplitAxis, SurfaceKey};
use crate::registry::{ActivePane, AgentRegistry};
use crate::store::{AgentStore, FrameSummary};
use crate::style::{RoleFamily, StyleClass};
use crate::zed_remote::{FileView, RemoteProject};
use crate::{
    AgentDone, AgentHide, AgentJumpAttention, AgentNew, AgentNext, AgentPrevious,
    DashboardNewAgent, DashboardReply, DashboardToggleSubagents, GitApprovalAllow, GitApprovalDeny,
    MinibufferCancel, MinibufferComplete, MinibufferConfirm, MinibufferNext, MinibufferPrevious,
    PaneBack, PaneClose, PaneFocusNext, PaneSplitDown, PaneSplitRight, RailFocus, RailOpen,
    RoleCycle, RoleCycleGroup, ShellEof, ShellInterrupt, ShellPagerAll, ShellPagerMore,
    ShellPagerQuit, SubmitPrompt, TaskBoard,
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

pub struct Workspace {
    connection: Connection,
    store: AgentStore,
    registry: AgentRegistry,
    models: HashMap<AgentId, Entity<AgentModel>>,
    /// Weak project cache keyed by daemon-side workspace identity. Artifact
    /// surfaces hold the strong references; when the last file/diff closes,
    /// the remote channel and cache entry naturally expire.
    remote_projects:
        HashMap<rho_ui_proto::WorkspaceInfo, (gpui::WeakEntity<project::Project>, Utf8PathBuf)>,
    pending_diff_loads: HashMap<AgentId, Task<()>>,
    /// Accumulated change summaries for materialized but hidden views; they
    /// render once, with the merged summary, when next selected.
    pending_syncs: HashMap<AgentId, FrameSummary>,
    draft_model: Entity<DraftModel>,
    /// Registered workdirs from the daemon; selection vocabulary for new
    /// agents.
    workdirs: Vec<rho_ui_proto::UiProject>,
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
    awaiting_draft_agent: bool,
    connected: bool,
    chatgpt_usage: Option<(f64, i64)>,
    duration_timer: Option<Task<()>>,
    /// Attention chime output; lazily opened on the first play.
    chime: Chime,
    /// Per-context split trees of viewports over surfaces. The rail is
    /// ambient chrome beside the active tree, not a pane in it.
    contexts: HashMap<ContextId, PaneTree<Surface>>,
    /// Per-context surface list, the emacs buffer list: every surface
    /// opened in a context lives here for the context's lifetime,
    /// regardless of what its panes currently display. Panes are
    /// viewports over this list — covering or closing one never loses a
    /// file or terminal; the views (and any zed channel behind them)
    /// release when the context itself closes.
    surfaces: HashMap<ContextId, Vec<Surface>>,
    /// Always present in `contexts` (the draft context never closes).
    active_context: ContextId,
    /// The dashboard: the rail as a real editor buffer, ambient chrome
    /// beside the active tree.
    dashboard: crate::dashboard::Dashboard,
    /// The completing-read strip at the bottom of the window, when open.
    minibuffer: Option<Minibuffer>,
    /// An open transient menu in the bottom strip; captures the keyboard
    /// via `transient_focus` while shown.
    transient: Option<crate::transient::Transient>,
    /// Parent menus beneath the open one; escape pops one level (magit's
    /// quit-one) before a final escape closes the strip.
    transient_stack: Vec<crate::transient::Transient>,
    transient_focus: gpui::FocusHandle,
    git_approval_focus: gpui::FocusHandle,
    /// Focus beneath the single modal overlay. Transients, minibuffers, and
    /// Git approval hand this target between them so borrowing keyboard
    /// focus never changes dashboard/work mode.
    overlay_return_focus: Option<gpui::FocusHandle>,
    /// The last system notice, flashed in the bottom strip (emacs echo
    /// area). Cleared by its own timer or when the minibuffer opens.
    echo: Option<Echo>,
    pending_git_approval: Option<PendingGitApproval>,
    _event_task: Task<()>,
    _dashboard_subscription: gpui::Subscription,
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
    workdir: Option<Utf8PathBuf>,
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
    pub fn new(attach_target: AttachTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (connection, events) = crate::connection::spawn(attach_target, cx);
        let workspace = cx.entity().downgrade();
        let draft_model = cx.new(|cx| DraftModel::new(workspace, cx));
        let event_task = cx.spawn(async move |this, cx| {
            let mut events: UnboundedReceiver<ConnEvent> = events;
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
            connection,
            store: AgentStore::default(),
            registry: AgentRegistry::default(),
            models: HashMap::new(),
            remote_projects: HashMap::new(),
            pending_diff_loads: HashMap::new(),
            pending_syncs: HashMap::new(),
            draft_model,
            workdirs: Vec::new(),
            draft_workstream: None,
            new_agent_draft: None,
            awaiting_draft_agent: false,
            connected: false,
            chatgpt_usage: None,
            duration_timer: None,
            chime: Chime::default(),
            contexts: HashMap::new(),
            surfaces: HashMap::new(),
            active_context: ContextId::Draft,
            dashboard,
            minibuffer: None,
            transient: None,
            transient_stack: Vec::new(),
            transient_focus: cx.focus_handle(),
            git_approval_focus: cx.focus_handle(),
            overlay_return_focus: None,
            echo: None,
            pending_git_approval: None,
            _event_task: event_task,
            _dashboard_subscription: dashboard_subscription,
        };
        let draft = this.make_surface(SurfaceKey::Draft, window, cx);
        this.display_surface(draft);
        this.seed_draft(false, window, cx);
        // Startup lands in home mode: the dashboard is the front door.
        let dashboard_focus = this.dashboard.focus_handle(cx);
        window.focus(&dashboard_focus, cx);
        this
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
    /// zed channels behind them) release with them.
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
        };
        self.contexts.retain(|context, _| keep(context));
        self.surfaces.retain(|context, _| keep(context));
        if !self.contexts.contains_key(&self.active_context) {
            self.active_context = ContextId::Draft;
        }
    }

    pub(crate) fn handle_events(
        &mut self,
        events: Vec<ConnEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut frames = Vec::new();
        let mut allocations = Vec::new();
        for event in events {
            match event {
                ConnEvent::Frame {
                    agent_id,
                    frame,
                    allocation,
                } => {
                    frames.push((agent_id, frame));
                    allocations.push(allocation);
                }
                event => {
                    if !frames.is_empty() {
                        self.handle_frame_batch(std::mem::take(&mut frames), window, cx);
                        allocations.clear();
                    }
                    self.handle_event(event, window, cx);
                }
            }
        }
        if !frames.is_empty() {
            self.handle_frame_batch(frames, window, cx);
        }
        drop(allocations);
    }

    fn handle_frame_batch(
        &mut self,
        frames: Vec<(AgentId, rho_ui_proto::remote::AgentRemoteFrame)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let startup_agent =
            matches!(self.registry.active_pane(), ActivePane::Startup).then(|| frames[0].0);
        let mut order = Vec::new();
        let mut changes: HashMap<AgentId, (FrameSummary, Option<u64>)> = HashMap::new();
        let mut live_changed = false;

        for (agent_id, frame) in frames {
            let old_context = self
                .store
                .get(&agent_id)
                .and_then(|state| state.context_used);
            let summary = self.store.apply(agent_id, frame);
            live_changed |= self.registry.mark_live(agent_id);
            changes
                .entry(agent_id)
                .and_modify(|(pending, _)| *pending = pending.merge(summary))
                .or_insert_with(|| {
                    order.push(agent_id);
                    (summary, old_context)
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
        }

        if let Some(agent_id) = startup_agent {
            // Materializing after applying the whole batch renders the final
            // state directly, avoiding one editor sync per queued frame.
            self.select_agent(Some(agent_id), window, cx);
        }

        let selected = self.registry.selected_agent().copied();
        for agent_id in order {
            let summary = changes[&agent_id].0;
            if Some(agent_id) == startup_agent {
                continue;
            }
            if selected == Some(agent_id) {
                if let Some(view) = self.models.get(&agent_id).cloned()
                    && let Some(state) = self.store.get(&agent_id)
                {
                    view.update(cx, |view, cx| {
                        view.sync(
                            state,
                            summary,
                            now_ms(),
                            &|id| self.registry.agent_display_label(id),
                            cx,
                        )
                    });
                }
            } else if self.models.contains_key(&agent_id) {
                self.pending_syncs
                    .entry(agent_id)
                    .and_modify(|pending| *pending = pending.merge(summary))
                    .or_insert(summary);
            }
        }

        self.ensure_duration_timer(cx);
        // Selected views notify themselves when their editor changes. Only a
        // newly-live agent changes workspace chrome; background transcript
        // frames should not dirty the window.
        if live_changed && startup_agent.is_none() {
            cx.notify();
        }
    }

    pub(crate) fn handle_event(
        &mut self,
        event: ConnEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConnEvent::Ready {
                workstreams,
                agents,
                projects: workdirs,
                machine_seed,
                agent_counter,
            } => {
                let first_ready = !self.connected;
                self.registry.set_machine_seed(machine_seed);
                self.registry.set_agent_counter(agent_counter);
                self.registry.set_data(workstreams, agents);
                self.prune_contexts();
                self.workdirs = workdirs;
                self.connected = true;
                self.refresh_draft_agent_targets(cx);
                if first_ready && matches!(self.registry.active_pane(), ActivePane::Startup) {
                    // The startup scaffold guessed before daemon data existed;
                    // refresh it now that workdir names and topics are known.
                    self.seed_draft(false, window, cx);
                }
                self.update_statuses(cx);
                cx.notify();
            }
            ConnEvent::WorkstreamCreated(workstream) => {
                self.registry.add_workstream(workstream);
                self.refresh_draft_agent_targets(cx);
                cx.notify();
            }
            ConnEvent::AgentCreated {
                agent_id,
                workstream,
            } => {
                self.registry.note_agent_workstream(agent_id, workstream);
                self.registry.mark_known(agent_id);
                if self.awaiting_draft_agent {
                    self.awaiting_draft_agent = false;
                    // The draft became this agent: reset the compose surface
                    // and follow the new agent.
                    let label = self
                        .draft_default_workdir()
                        .map(|path| self.workdir_label(&path))
                        .unwrap_or_default();
                    self.draft_model.update(cx, |view, cx| {
                        view.set_body_text("", cx);
                        view.set_workdir_text(&label, cx);
                        view.set_role_text(crate::draft_view::DEFAULT_ROLE, cx);
                        view.set_start_text(crate::draft_view::DEFAULT_START, cx);
                    });
                    self.select_agent(Some(agent_id), window, cx);
                }
                cx.notify();
            }
            ConnEvent::AgentLoaded(agent_id) => {
                self.registry.mark_known(agent_id);
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
                // Chime on the rising edge into the user's court, like the
                // lamp turning on — but not for the agent already on screen,
                // whose turn end the user is watching anyway.
                let before = self.registry.attention(agent_id);
                self.registry.set_attention(agent_id, attention);
                if attention >= rho_ui_proto::UiAttention::Pending
                    && before < rho_ui_proto::UiAttention::Pending
                    && self.registry.selected_agent() != Some(&agent_id)
                {
                    self.chime.play();
                }
                cx.notify();
            }
            ConnEvent::ChatGptUsage {
                used_percent,
                reset_at_unix,
            } => {
                self.chatgpt_usage = Some((used_percent, reset_at_unix));
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
                self.awaiting_draft_agent = false;
                self.notice_on(
                    None,
                    &format!("[rho daemon error: {message}]"),
                    StyleClass::SystemImportant,
                    cx,
                );
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
                self.connected = false;
                self.awaiting_draft_agent = false;
                let notice = format!("[disconnected from rho daemon: {reason}]");
                for view in self.models.values() {
                    view.update(cx, |view, cx| {
                        view.system_notice(&notice, StyleClass::Disconnect, cx);
                    });
                }
                self.draft_model.update(cx, |view, cx| {
                    view.system_notice(&notice, StyleClass::Disconnect, cx);
                });
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
                    self.notice_on(
                        None,
                        "[SSH Git request denied: another prompt is active]",
                        StyleClass::SystemImportant,
                        cx,
                    );
                    return;
                }
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
    }

    fn submit_prompt(&mut self, _: &SubmitPrompt, window: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_tree().focused().surface.view {
            model.clone().update(cx, |model, cx| model.submit(cx));
            return;
        }
        match self.registry.selected_agent().copied() {
            Some(agent_id) => {
                let Some(view) = self.models.get(&agent_id).cloned() else {
                    return;
                };
                let Some(text) = view.update(cx, |view, cx| view.take_prompt(cx)) else {
                    return;
                };
                self.handle_submit(agent_id, text, cx);
            }
            None => self.submit_draft(window, cx),
        }
    }

    fn shell_interrupt(&mut self, _: &ShellInterrupt, _: &mut Window, cx: &mut Context<Self>) {
        if let SurfaceView::Shell { model, .. } = &self.active_tree().focused().surface.view {
            model.clone().update(cx, |model, _| model.interrupt());
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

    fn handle_submit(&mut self, agent_id: AgentId, text: String, cx: &mut Context<Self>) {
        if !self.connected {
            self.notice_on(
                Some(&agent_id),
                "not connected to rho-daemon",
                StyleClass::SystemImportant,
                cx,
            );
            return;
        }
        self.connection.send(ClientMessage::SendUserMessage {
            agent_id,
            content: vec![ContentPart::Text { text }],
            delivery: MessageDelivery::NextRequest,
        });
        // Engagement bump: keeps display-time staleness correct between
        // topic refreshes (the daemon persists the same timestamp).
        self.registry.touch_agent(agent_id);
    }

    /// Submitting the compose surface creates the agent: the workdir field
    /// picks the working directory, the topic is whatever the draft
    /// inherited. The buffers are not cleared here — they survive until the
    /// daemon confirms creation.
    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.draft_model.read(cx).body_text(cx).trim().to_owned();
        if body.is_empty() {
            // Enter in the workdir field with nothing to send: jump to the
            // body instead of submitting.
            if let Some(editor) = self.focused_draft_editor() {
                self.draft_model
                    .update(cx, |view, cx| view.focus_body(&editor, window, cx));
            }
            return;
        }
        if !self.connected {
            self.notice_on(
                None,
                "not connected to rho-daemon",
                StyleClass::SystemImportant,
                cx,
            );
            return;
        }
        let field = self.draft_model.read(cx).workdir_text(cx).trim().to_owned();
        let working_directory = (!field.is_empty())
            .then(|| self.resolve_workdir_path(Utf8PathBuf::from(field)))
            .or_else(|| self.draft_default_workdir());
        let start = {
            let draft = self.draft_model.read(cx);
            let mode = draft.start_mode();
            let target = draft.start_text(cx).trim().to_owned();
            match self.parse_start(mode, &target, working_directory) {
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
        self.awaiting_draft_agent = true;
        // Every top-level agent founds its own workstream; the daemon names
        // it from the agent's generated title.
        self.connection.send(ClientMessage::NewAgent {
            workstream: None,
            role,
            start,
            content: Some(vec![ContentPart::Text { text: body }]),
        });
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
        workdir: Option<Utf8PathBuf>,
    ) -> Result<rho_ui_proto::StartMode, String> {
        use rho_ui_proto::{JoinTarget, StartMode, WorkspaceInfo};

        use crate::draft_view::StartFieldMode;
        let require_workdir = || {
            workdir.clone().ok_or_else(|| {
                "no working directory for the new agent: type one in the \
                 Workdir field, or register one with :projects add <path>"
                    .to_owned()
            })
        };
        let workspace = self
            .registry
            .agent_by_label(target)
            .and_then(|agent_id| self.registry.agent_workspace(agent_id))
            .cloned();
        Ok(match (mode, target, workspace) {
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
                repo: require_workdir()?,
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
                    repo: require_workdir()?,
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
                        repo: require_workdir()?,
                    })
                } else {
                    return Err(format!(
                        "join target must be `user` or an agent label, not `{target}`"
                    ));
                }
            }
        })
    }

    /// Every command a transient can run goes through one of these
    /// `cmd_*` methods: no textual grammar, no dispatch enum — the menu
    /// item closure is the command.
    fn require_connected(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.connected {
            self.notice_on(
                None,
                "not connected to rho-daemon",
                StyleClass::SystemInfo,
                cx,
            );
        }
        self.connected
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
        if !self.require_connected(cx) {
            return;
        }
        if let Some(agent_id) = self.selected_or_notice("cancel", cx) {
            self.connection.send(ClientMessage::CancelTurn { agent_id });
        }
    }

    pub(crate) fn cmd_rewind(&mut self, turns: u32, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        if let Some(agent_id) = self.selected_or_notice("rewind", cx) {
            self.connection
                .send(ClientMessage::RewindAgent { agent_id, turns });
        }
    }

    pub(crate) fn cmd_continue_turn(&mut self, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        if let Some(agent_id) = self.selected_or_notice("continue", cx) {
            self.connection
                .send(ClientMessage::ContinueTurn { agent_id });
        }
    }

    pub(crate) fn cmd_compact(&mut self, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        if let Some(agent_id) = self.selected_or_notice("compact", cx) {
            self.connection.send(ClientMessage::CompactAgent {
                agent_id,
                delivery: rho_ui_proto::MessageDelivery::NextRequest,
            });
            self.notice_on(
                Some(&agent_id),
                "compacting context",
                StyleClass::SystemInfo,
                cx,
            );
        }
    }

    pub(crate) fn cmd_change_prompt_cache_key(&mut self, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        if let Some(agent_id) = self.selected_or_notice("change-prompt-cache-key", cx) {
            self.connection
                .send(ClientMessage::ChangePromptCacheKey { agent_id });
            self.notice_on(
                Some(&agent_id),
                "changed prompt cache key",
                StyleClass::SystemInfo,
                cx,
            );
        }
    }

    pub(crate) fn cmd_workstream_rename(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(workstream_id) = self.focused_workstream_or_notice(cx) {
            self.connection.send(ClientMessage::WorkstreamRename {
                workstream_id,
                name,
            });
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
        self.connection.send(ClientMessage::WorkstreamLabel {
            workstream_id,
            label,
            add,
        });
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
        let selected = self.registry.selected_agent().copied();
        let agent_id = self.set_agent_disposition(selected, "done", disposition, cx);
        // Hiding the open agent closes its tab, or it would stay
        // rail-visible through the selection exemption.
        if hide && agent_id.is_some() && agent_id.as_ref() == self.registry.selected_agent() {
            self.select_agent(None, window, cx);
        }
    }

    pub(crate) fn cmd_agent_snooze(&mut self, duration_ms: u64, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        let until = rho_core::UnixMs(now_ms().saturating_add(duration_ms));
        let selected = self.registry.selected_agent().copied();
        self.set_agent_disposition(
            selected,
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
            self.connection.send(ClientMessage::AgentMove {
                agent_id,
                target: rho_ui_proto::WorkstreamTarget::Existing(target),
            });
        }
    }

    pub(crate) fn cmd_project_add(
        &mut self,
        path: Utf8PathBuf,
        name: Option<String>,
        description: String,
        cx: &mut Context<Self>,
    ) {
        if !self.require_connected(cx) {
            return;
        }
        self.connection.send(ClientMessage::ProjectSet {
            path: self.resolve_workdir_path(path),
            name,
            description,
        });
    }

    pub(crate) fn cmd_project_remove(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        match resolve_workdir(&path, &self.workdir_table()) {
            Some(path) => {
                self.connection
                    .send(ClientMessage::ProjectRemove { path: path.into() });
            }
            None => {
                let message = format!("no registered project `{path}`");
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            }
        }
    }

    pub(crate) fn cmd_open(&mut self, path: Utf8PathBuf, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        let Some(agent_id) = self.selected_or_notice("open", cx) else {
            return;
        };
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
        if !self.require_connected(cx) {
            return;
        }
        if let Some(agent_id) = self.selected_or_notice("shell", cx) {
            self.open_shell_surface(agent_id, cx);
        }
    }

    pub(crate) fn cmd_shell_close(&mut self, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        let Some(agent_id) = self.selected_or_notice("close shell", cx) else {
            return;
        };
        let task = self.connection.close_shell(agent_id.encoded(), cx);
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!("shell close failed: {error}")),
            };
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
        if !self.require_connected(cx) {
            return;
        }
        if let Some(agent_id) = self.selected_or_notice("term", cx) {
            self.open_terminal_surface(agent_id, new, cx);
        }
    }

    pub(crate) fn cmd_diff(&mut self, cx: &mut Context<Self>) {
        if !self.require_connected(cx) {
            return;
        }
        let Some(agent_id) = self.selected_or_notice("diff", cx) else {
            return;
        };
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
            Some(path) => {
                let path = self.resolve_workdir_path(path);
                let label = self.workdir_label(&path);
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

    /// Selects an agent, asking the daemon to load it first when this
    /// connection has never seen frames for it.
    pub fn open_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        if self.connected && !self.registry.is_live(agent_id) {
            self.connection.send(ClientMessage::LoadAgent { agent_id });
        }
        self.select_agent(Some(agent_id), window, cx);
    }

    /// Clears (or snoozes, or files away) an agent's claim on the user's
    /// attention; returns the agent it acted on. The daemon echoes the
    /// resulting attention level back as a broadcast, so the rail updates
    /// through the normal event path.
    fn set_agent_disposition(
        &mut self,
        source_agent: Option<AgentId>,
        command: &str,
        disposition: rho_ui_proto::AgentDisposition,
        cx: &mut Context<Self>,
    ) -> Option<AgentId> {
        let Some(agent_id) = source_agent.or_else(|| self.registry.selected_agent().copied())
        else {
            let message = format!("{command}: no agent selected");
            self.notice_on(None, &message, StyleClass::SystemInfo, cx);
            return None;
        };
        self.connection.send(ClientMessage::SetAgentDisposition {
            agent_id,
            disposition,
        });
        Some(agent_id)
    }

    pub fn toggle_rail_tail(&mut self, cx: &mut Context<Self>) {
        self.registry.toggle_rail_tail();
        cx.notify();
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
    fn draft_default_workdir(&self) -> Option<Utf8PathBuf> {
        self.draft_workstream
            .and_then(|workstream_id| self.registry.last_working_directory(workstream_id))
            .or_else(|| self.workdirs.first().map(|workdir| workdir.path.clone()))
    }

    /// How a path reads in the draft header: its registered project name
    /// when it has one, else the full path.
    fn workdir_label(&self, path: &Utf8Path) -> String {
        self.workdirs
            .iter()
            .find(|workdir| workdir.path == path)
            .map(|workdir| workdir.name.clone())
            .unwrap_or_else(|| path.to_string())
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

    /// Registered workdirs as the `(name, path)` table the shared command
    /// layer expects.
    pub fn workdir_table(&self) -> Vec<(String, String)> {
        self.workdirs
            .iter()
            .map(|workdir| (workdir.name.clone(), workdir.path.to_string()))
            .collect()
    }

    /// A registered project name resolves to its path; anything else passes
    /// through untouched. Paths name directories on the daemon's machine,
    /// so the GUI never joins its own cwd or expands its own home — the
    /// daemon expands `~` and validates.
    fn resolve_workdir_path(&self, path: Utf8PathBuf) -> Utf8PathBuf {
        resolve_workdir(path.as_str(), &self.workdir_table())
            .map(Utf8PathBuf::from)
            .unwrap_or(path)
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

    /// Selects and displays an agent (or the draft) without moving keyboard
    /// focus: the dashboard's preview — the cursor stays home, the panes
    /// follow it.
    fn preview_agent(
        &mut self,
        agent_id: Option<AgentId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(agent_id) = agent_id
            && self.connected
            && !self.registry.is_live(agent_id)
        {
            self.connection.send(ClientMessage::LoadAgent { agent_id });
        }
        self.select_agent_inner(agent_id, false, window, cx);
    }

    fn select_agent_inner(
        &mut self,
        agent_id: Option<AgentId>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(agent_id) = &agent_id {
            let view = self.materialize_model(agent_id, cx);
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
        self.connection.focus_agent(agent_id);
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
            // A reply draft previews its addressee, same as the row above it.
            Some(
                RowTarget::Stream {
                    root: Some(agent_id),
                    ..
                }
                | RowTarget::Agent(agent_id)
                | RowTarget::Reply(agent_id),
            ) if self.registry.selected_agent() != Some(&agent_id) => {
                self.preview_agent(Some(agent_id), window, cx);
            }
            Some(
                RowTarget::Stream { root: Some(_), .. } | RowTarget::Agent(_) | RowTarget::Reply(_),
            ) => {}
            // Rows with no agent behind them (group headers, the fold
            // toggle, drafts-in-progress) preview nothing.
            _ => {
                if self.registry.selected_agent().is_some() {
                    self.preview_agent(None, window, cx);
                }
            }
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

    /// `:open`: reuses the agent workspace's remote Zed project and shows the
    /// file surface in the main pane.
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
        let cached = self.cached_remote_project(&workspace);
        let project_task = cached.is_none().then(|| {
            crate::zed_remote::open_remote_project(&self.connection, workspace.clone(), cx)
        });
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
                    let Ok(project) =
                        this.update(cx, |this, _| this.cache_remote_project(workspace, project))
                    else {
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
                        let view = cx.new(|cx| FileView::new(project.project, buffer, window, cx));
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
        let task = self.connection.open_shell(agent_id.encoded(), cx);
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(result) => result,
                Err(join_error) => Err(anyhow::anyhow!("shell dial failed: {join_error}")),
            };
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
        workspace: &rho_ui_proto::WorkspaceInfo,
    ) -> Option<RemoteProject> {
        let (project, root) = self.remote_projects.get(workspace)?.clone();
        match project.upgrade() {
            Some(project) => Some(RemoteProject { project, root }),
            _ => {
                self.remote_projects.remove(workspace);
                None
            }
        }
    }

    fn cache_remote_project(
        &mut self,
        workspace: rho_ui_proto::WorkspaceInfo,
        opened: RemoteProject,
    ) -> RemoteProject {
        if let Some(existing) = self.cached_remote_project(&workspace) {
            return existing;
        }
        self.remote_projects
            .insert(workspace, (opened.project.downgrade(), opened.root.clone()));
        opened
    }

    /// Persists the agent's jj working-copy snapshot, then projects its
    /// parent-side manifest over the workspace's shared live Zed buffers.
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

        let cached = self.cached_remote_project(&workspace);
        let project_task = cached.is_none().then(|| {
            crate::zed_remote::open_remote_project(&self.connection, workspace.clone(), cx)
        });
        let diff_client = self.connection.diff_client();
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
                        this.cache_remote_project(workspace.clone(), opened)
                    })
                    .map_err(|_| anyhow::anyhow!("GUI closed while loading diff"))?;
                let live_paths = cx.update(|cx| crate::diff_view::dirty_paths(&project, cx));
                let snapshot_task = cx.update(|cx| {
                    diff_client.snapshot(workspace.clone(), None, live_paths.clone(), cx)
                });
                let snapshot = snapshot_task
                    .await
                    .map_err(|error| anyhow::anyhow!("diff dial task failed: {error}"))??
                    .context("initial diff snapshot unexpectedly unchanged")?;
                let prepared =
                    crate::diff_view::PreparedDiff::load(&project, snapshot, live_paths, cx)
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
        let task = self
            .connection
            .open_terminal(agent_id.encoded(), new, 80, 24, cx);
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(result) => result,
                Err(join_error) => Err(anyhow::anyhow!("terminal dial failed: {join_error}")),
            };
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
        let Some(agent_id) = self.registry.next_live_agent(delta) else {
            self.notice_on(
                None,
                "agent-switch: no active agents available yet",
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
        cx: &mut Context<Self>,
    ) -> Entity<AgentModel> {
        let deferred = self.pending_syncs.remove(agent_id);
        if let Some(view) = self.models.get(agent_id).cloned() {
            if let (Some(summary), Some(state)) = (deferred, self.store.get(agent_id)) {
                view.update(cx, |view, cx| {
                    view.sync(
                        state,
                        summary,
                        now_ms(),
                        &|id| self.registry.agent_display_label(id),
                        cx,
                    )
                });
            }
            return view;
        }
        // A freshly created view renders the full state below, which
        // subsumes any deferred summary.
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| AgentModel::new(workspace, cx));
        if let Some(state) = self.store.get(agent_id) {
            view.update(cx, |view, cx| {
                view.sync(
                    state,
                    FrameSummary::everything(),
                    now_ms(),
                    &|id| self.registry.agent_display_label(id),
                    cx,
                );
            });
        }
        self.refresh_view_status(agent_id, &view, cx);
        self.models.insert(*agent_id, view.clone());
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
        let role_label = self.role_label(agent_id);
        let context_used = self
            .store
            .get(agent_id)
            .and_then(|state| state.context_used);
        view.update(cx, |view, cx| {
            view.set_status(
                &directory_label,
                workspace_label.as_deref(),
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
    }

    #[cfg(test)]
    pub(crate) fn dashboard_fold_count(&self) -> usize {
        self.dashboard.fold_count()
    }

    #[cfg(test)]
    pub(crate) fn is_dashboard_mode(&self, window: &Window, cx: &App) -> bool {
        self.dashboard_mode(window, cx)
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
                let model = self.materialize_model(&agent_id, cx);
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
        match self.active_tree().focused().surface.key.clone() {
            SurfaceKey::Transcript(agent_id) | SurfaceKey::Shell(agent_id) => {
                self.registry.select_agent(agent_id)
            }
            SurfaceKey::Terminal { agent_id, .. } => self.registry.select_agent(agent_id),
            SurfaceKey::Diff { agent_id } => self.registry.select_agent(agent_id),
            SurfaceKey::Draft => self.registry.enter_draft(),
            // Files keep whatever agent context was current.
            SurfaceKey::File { .. } => {}
        }
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
        self.active_tree_mut().close_focused();
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
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
                workspace.cmd_project_add(camino::Utf8PathBuf::from(path), name, description, cx);
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
                    | Some(RowTarget::Reply(agent_id)) => self.registry.working_directory(agent_id),
                    Some(RowTarget::Stream { workstream_id, .. }) => {
                        self.registry.last_working_directory(workstream_id)
                    }
                    _ => None,
                }
            } else {
                self.registry
                    .selected_agent()
                    .and_then(|agent_id| self.registry.working_directory(*agent_id))
            };
            let workdir = contextual.or_else(|| match self.workdirs.as_slice() {
                [workdir] => Some(workdir.path.clone()),
                _ => None,
            });
            self.new_agent_draft = Some(NewAgentDraft {
                workdir,
                workspace: DraftWorkspace::NewOn(DraftBase::Auto),
                role: crate::draft_view::DEFAULT_ROLE.to_owned(),
            });
        }
        let draft = self.new_agent_draft.as_ref().expect("draft initialized");
        let project = draft
            .workdir
            .as_deref()
            .map(|path| self.workdir_label(path))
            .unwrap_or_else(|| "<choose>".to_owned());
        self.open_transient(
            crate::transient::new_agent_menu(project, draft.workspace.label(), draft.role.clone()),
            window,
            cx,
        );
    }

    pub(crate) fn prompt_new_agent_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                let input = input.trim();
                if !input.is_empty() {
                    let path = workspace.resolve_workdir_path(Utf8PathBuf::from(input));
                    if let Some(draft) = &mut workspace.new_agent_draft {
                        draft.workdir = Some(path);
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
            .as_deref()
            .map(|path| self.workdir_label(path))
            .unwrap_or_else(|| "no project".to_owned());
        let summary = format!("{project} · {} · {}", draft.role, draft.workspace.label());
        self.dashboard.open_new_draft(summary, cx);
        let handle = self.dashboard.focus_handle(cx);
        window.focus(&handle, cx);
        self.dashboard_enter_insert(window, cx);
    }

    fn new_agent_launch(&self) -> Result<(rho_ui_proto::StartMode, AgentRole), String> {
        let draft = self
            .new_agent_draft
            .as_ref()
            .ok_or_else(|| "new agent has no launch configuration".to_owned())?;
        let (mode, target) = draft.workspace.mode_and_target();
        let start = self.parse_start(mode, target, draft.workdir.clone())?;
        let role = parse_agent_role(&draft.role)?;
        Ok((start, role))
    }

    /// `enter` in the dashboard: act on the row under the cursor.
    fn dashboard_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        match self.dashboard.cursor_target(cx) {
            Some(RowTarget::Stream {
                root: Some(agent_id),
                ..
            })
            | Some(RowTarget::Agent(agent_id)) => self.open_agent(agent_id, window, cx),
            Some(RowTarget::FoldToggle) => self.toggle_rail_tail(cx),
            // Enter sends the inline reply draft (and closes it); an empty
            // draft just closes. Disconnected, the draft stays parked
            // rather than being consumed into the void.
            Some(RowTarget::Reply(agent_id)) => {
                if !self.require_connected(cx) {
                    return;
                }
                if let Some(text) = self.dashboard.take_reply(agent_id, cx) {
                    self.handle_submit(agent_id, text, cx);
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
                let (start, role) = match self.new_agent_launch() {
                    Ok(launch) => launch,
                    Err(message) => {
                        self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                        return;
                    }
                };
                if let Some(body) = self.dashboard.take_new_draft(cx) {
                    self.create_inline_agent(body, start, role);
                }
                self.new_agent_draft = None;
                self.dashboard_exit_insert(window, cx);
            }
            Some(RowTarget::Stream { root: None, .. }) | Some(RowTarget::None) | None => {}
        }
    }

    fn create_inline_agent(
        &mut self,
        body: String,
        start: rho_ui_proto::StartMode,
        role: AgentRole,
    ) {
        self.connection.send(ClientMessage::NewAgent {
            workstream: None,
            role,
            start,
            content: Some(vec![ContentPart::Text { text: body }]),
        });
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
            // The boxed preview card carries the hierarchy, so it can take
            // the wider share; transcripts are the text-dense side.
            .w(if show_panes {
                gpui::relative(0.4)
            } else {
                gpui::relative(1.0)
            })
            .pl(px(24.))
            .pr(px(24.))
            .child(self.render_dashboard_header(text_style, cx));
        container
            .child(
                div()
                    .flex_grow(1.0)
                    .min_h_0()
                    .child(self.dashboard.editor().clone()),
            )
            .into_any_element()
    }

    /// The home-mode masthead: a centered title and a one-line attention
    /// summary, so the dashboard reads as a place rather than a sidebar.
    fn render_dashboard_header(
        &self,
        text_style: &gpui::TextStyle,
        _cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let mut waiting = 0usize;
        let mut working = 0usize;
        for workstream in self.registry.workstreams() {
            if workstream.hidden {
                continue;
            }
            for agent_id in workstream.agent_ids() {
                match self.registry.attention(agent_id) {
                    rho_ui_proto::UiAttention::NeedsInput | rho_ui_proto::UiAttention::Pending => {
                        waiting += 1;
                    }
                    rho_ui_proto::UiAttention::Working => working += 1,
                    rho_ui_proto::UiAttention::Quiet => {}
                }
            }
        }
        let summary = match (waiting, working) {
            (0, 0) => "all quiet".to_owned(),
            (0, working) => format!("{working} working"),
            (waiting, 0) => format!("{waiting} waiting on you"),
            (waiting, working) => format!("{waiting} waiting on you · {working} working"),
        };
        let usage = self.chatgpt_usage.map(|(used_percent, reset_at_unix)| {
            let used_percent = used_percent.clamp(0.0, 100.0);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs() as f64;
            let days = ((reset_at_unix as f64 - now).max(0.0)) / 86_400.0;
            format!("usage: {used_percent:.0}% · {days:.1} days")
        });
        let header = div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(10.))
            .pb(px(26.))
            .child(div().font_weight(gpui::FontWeight::BOLD).child("rho"))
            .child(
                div()
                    .text_color(text_style.color.opacity(0.55))
                    .child(summary),
            );
        if let Some(usage) = usage {
            header
                .child(
                    div()
                        .pt(px(5.))
                        .text_color(text_style.color.opacity(0.7))
                        .child(usage),
                )
                .into_any_element()
        } else {
            header.into_any_element()
        }
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
        let agent_id = self.registry.selected_agent().copied()?;
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

    /// The selected agent's document preview editor, built on first use.
    fn selected_preview_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<editor::Editor>> {
        let agent_id = self.registry.selected_agent().copied()?;
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
        self.sync_diff_visibility(!home, cx);
        let show_panes = !home || self.registry.selected_agent().is_some();
        let rail = home.then(|| self.render_rail(show_panes, text_style, cx));
        // Same hairline the rail uses against the panes.
        let separator_color = cx.theme().colors().border_variant.opacity(0.6);
        let preview_bar = home
            .then(|| self.render_preview_bar(text_style, cx))
            .flatten();
        let preview = home
            .then(|| self.selected_preview_editor(window, cx))
            .flatten()
            .map(|editor| div().size_full().overflow_hidden().child(editor));
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
            // Home mode boxes the preview — a sheet hanging from a top
            // inset, flush with the bottom and right window edges, visibly
            // a card showing what the cursor points at, not an equal half.
            // The sheet shows the agent's *document* editor: the same
            // transcript buffers composed without the prompt, ending where
            // the words end. Its bottom bar carries the context the prompt
            // row shows in work mode.
            if home {
                element
                    .mt(gpui::relative(0.02))
                    .border_1()
                    .border_color(separator_color)
                    .rounded_t_md()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .children(preview),
                    )
                    .children(preview_bar)
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
        for agent_id in self.registry.live_agents() {
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
        for agent_id in self.registry.live_agents() {
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
                cx.background_executor().timer(Duration::from_secs(1)).await;
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
        "eng-high" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        }),
        "eng-ultra" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
        }),
        "pm" => Ok(AgentRole::pm()),
        other => Err(format!(
            "unknown role `{other}`; use eng, eng-mini, eng-low, eng-high, eng-ultra, or pm"
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
        } => "pm",
        AgentRole::Advisor { .. } => "eng",
        AgentRole::PM | AgentRole::WorkflowPM { .. } => "eng-mini",
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
        AgentRole::Advisor { intelligence } => RoleLabel {
            text: match intelligence {
                AdvisorIntelligence::Medium => "advisor",
                AdvisorIntelligence::High => "advisor-high",
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
                    EngineerIntelligence::Medium => "eng",
                    EngineerIntelligence::High => "eng-high",
                    EngineerIntelligence::Ultra => "eng-ultra",
                }
                .to_owned(),
                family: if intelligence == EngineerIntelligence::Ultra {
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
        // Regenerate the dashboard listing whenever anything redraws; sync
        // is cheap and a near no-op when nothing changed.
        self.dashboard.sync(&self.registry, window, cx);
        div()
            .id("rho-gui")
            .size_full()
            .flex()
            .flex_col()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .key_context("RhoGui")
            .on_action(cx.listener(Self::submit_prompt))
            .on_action(cx.listener(Self::shell_interrupt))
            .on_action(cx.listener(Self::shell_eof))
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
                    (None, Some(minibuffer), _, _) => Some(minibuffer.render(&text_style, cx)),
                    (None, None, Some(transient), _) => Some(
                        div()
                            .track_focus(&self.transient_focus)
                            .on_key_down(cx.listener(Self::transient_key))
                            .child(transient.render(&text_style, cx))
                            .into_any_element(),
                    ),
                    (None, None, None, Some(echo)) => Some(echo.render(&text_style, cx)),
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

/// Resolves a workdir argument (registered name or path) to its path.
fn resolve_workdir<'a>(argument: &str, workdirs: &'a [(String, String)]) -> Option<&'a str> {
    workdirs
        .iter()
        .find(|(name, path)| name == argument || path == argument)
        .map(|(_, path)| path.as_str())
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
