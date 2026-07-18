//! Root entity: owns the daemon connection, the canonical agent states, the
//! registry, and one persistent [`AgentModel`] per opened agent.
//!
//! All protocol events flow through [`Workspace`]; queued frame runs are
//! merged per agent, and views receive summarized changes rather than the
//! protocol itself.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::prelude::*;
use gpui::{Context, Entity, Focusable as _, Task, Window, div, px};
use rho_core::ContentPart;
use rho_ui_proto::{
    AdvisorIntelligence, AgentId, AgentRole, ClientMessage, EngineerIntelligence, MessageDelivery,
};
use theme::ActiveTheme as _;

use crate::agent_view::AgentModel;
use crate::chime::Chime;
use crate::connection::{ConnEvent, Connection};
use crate::draft_view::DraftModel;
use crate::minibuffer::{ECHO_DURATION, Echo, Minibuffer};
use crate::pane::{PaneTree, SplitAxis, SurfaceKey};
use crate::registry::{ActivePane, AgentRegistry};
use crate::store::{AgentStore, FrameSummary};
use crate::style::{RoleFamily, StyleClass};
use crate::zed_remote::FileView;
use crate::{
    AgentDone, AgentJumpAttention, AgentNew, AgentNext, AgentPrevious, MinibufferCancel,
    MinibufferCommand, MinibufferComplete, MinibufferConfirm, MinibufferNext, MinibufferPrevious,
    PaneBack, PaneClose, PaneFocusNext, PaneSplitDown, PaneSplitRight, RailFocus, RailOpen,
    RoleCycle, RoleCycleGroup, SubmitPrompt, TaskBoard,
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
    Task(rho_ui_proto::TagId),
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
    draft_workstream: Option<rho_ui_proto::TagId>,
    /// A NewAgent request from the draft is in flight; the draft buffer is
    /// kept intact until the daemon confirms creation, so a rejected request
    /// (bad working directory, say) never loses the message.
    awaiting_draft_agent: bool,
    connected: bool,
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
    /// Keyboard focus for the ambient rail (it has no editor).
    rail_focus: gpui::FocusHandle,
    /// The completing-read strip at the bottom of the window, when open.
    minibuffer: Option<Minibuffer>,
    /// The last system notice, flashed in the bottom strip (emacs echo
    /// area). Cleared by its own timer or when the minibuffer opens.
    echo: Option<Echo>,
    _event_task: Task<()>,
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

        let mut this = Self {
            connection,
            store: AgentStore::default(),
            registry: AgentRegistry::default(),
            models: HashMap::new(),
            pending_syncs: HashMap::new(),
            draft_model,
            workdirs: Vec::new(),
            draft_workstream: None,
            awaiting_draft_agent: false,
            connected: false,
            duration_timer: None,
            chime: Chime::default(),
            contexts: HashMap::new(),
            surfaces: HashMap::new(),
            active_context: ContextId::Draft,
            rail_focus: cx.focus_handle(),
            minibuffer: None,
            echo: None,
            _event_task: event_task,
        };
        let draft = this.make_surface(SurfaceKey::Draft, window, cx);
        this.display_surface(draft);
        this.seed_draft(false, window, cx);
        this.focus_active_surface(window, cx);
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
            .map(|workstream| workstream.tag_id)
            .collect::<HashSet<_>>();
        let keep = |context: &ContextId| match context {
            ContextId::Draft => true,
            ContextId::Task(tag_id) => live.contains(tag_id),
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
                tags,
                agents,
                projects: workdirs,
                machine_seed,
                agent_counter,
                workspace_counter,
            } => {
                let first_ready = !self.connected;
                self.registry.set_machine_seed(machine_seed);
                self.registry.set_agent_counter(agent_counter);
                self.registry.set_workspace_counter(workspace_counter);
                self.registry.set_data(tags, agents);
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
            ConnEvent::TagCreated(tag) => {
                self.registry.add_tag(tag);
                self.refresh_draft_agent_targets(cx);
                cx.notify();
            }
            ConnEvent::AgentCreated { agent_id, tags } => {
                self.registry.note_agent_tags(agent_id, tags);
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
        }
    }

    fn submit_prompt(&mut self, _: &SubmitPrompt, window: &mut Window, cx: &mut Context<Self>) {
        match self.registry.selected_agent().copied() {
            Some(agent_id) => {
                let Some(view) = self.models.get(&agent_id).cloned() else {
                    return;
                };
                let Some(text) = view.update(cx, |view, cx| view.take_prompt(cx)) else {
                    return;
                };
                self.handle_submit(agent_id, text, window, cx);
            }
            None => self.submit_draft(window, cx),
        }
    }

    fn handle_submit(
        &mut self,
        agent_id: AgentId,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(parsed) = rho_commands::parse(&text) {
            self.handle_command(Some(agent_id), parsed, window, cx);
            return;
        }
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
        if let Some(parsed) = rho_commands::parse(&body) {
            // A command typed into the draft body: run it and clear just the
            // body, keeping the chosen workdir.
            self.draft_model
                .update(cx, |view, cx| view.set_body_text("", cx));
            self.handle_command(None, parsed, window, cx);
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
            tags: Vec::new(),
            role,
            start,
            content: Some(vec![ContentPart::Text { text: body }]),
        });
    }

    /// Interprets the draft's start field (seeded with `@-`, the parents of
    /// your working copy). An agent label resolves to the agent's workspace
    /// — `<name>@` as a stacking base, or the workspace itself for Join;
    /// anything else is a revset (stacking only). `user` is only meaningful
    /// for Join — your own checkout. Agent targets carry their own repo;
    /// `workdir` is only needed (and only checked) for the other arms.
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
                revset: target.to_owned(),
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
                    revset: target.to_owned(),
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

    fn handle_command(
        &mut self,
        source_agent: Option<AgentId>,
        parsed: rho_commands::Parsed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use rho_commands::Command;
        let command = match parsed {
            rho_commands::Parsed::Command(command) => command,
            rho_commands::Parsed::Invalid(usage) => {
                let message = format!("usage: {usage}");
                self.notice_on(source_agent.as_ref(), &message, StyleClass::SystemInfo, cx);
                return;
            }
            rho_commands::Parsed::Unknown(command) => {
                let message = format!("unknown command `{command}`; try :help");
                self.notice_on(source_agent.as_ref(), &message, StyleClass::SystemInfo, cx);
                return;
            }
        };
        if !self.connected && !matches!(command, Command::Quit | Command::Help | Command::Version) {
            self.notice_on(
                source_agent.as_ref(),
                "not connected to rho-daemon",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        match command {
            Command::AgentNew { working_directory } => {
                self.enter_draft(working_directory, window, cx);
            }
            Command::AgentCancel => {
                let target = source_agent.or_else(|| self.registry.selected_agent().cloned());
                match target {
                    Some(agent_id) => {
                        self.connection.send(ClientMessage::CancelTurn { agent_id });
                    }
                    None => self.notice_on(
                        None,
                        ":cancel: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }
            Command::Rewind { turns } => {
                let target = source_agent.or_else(|| self.registry.selected_agent().cloned());
                match target {
                    Some(agent_id) => {
                        self.connection
                            .send(ClientMessage::RewindAgent { agent_id, turns });
                    }
                    None => self.notice_on(
                        None,
                        ":rewind: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }
            Command::Continue => {
                let target = source_agent.or_else(|| self.registry.selected_agent().cloned());
                match target {
                    Some(agent_id) => {
                        self.connection
                            .send(ClientMessage::ContinueTurn { agent_id });
                    }
                    None => self.notice_on(
                        None,
                        ":continue: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }
            Command::Compact => {
                let target = source_agent.or_else(|| self.registry.selected_agent().cloned());
                match target {
                    Some(agent_id) => {
                        self.connection.send(ClientMessage::CompactAgent {
                            agent_id,
                            delivery: rho_ui_proto::MessageDelivery::NextTurn,
                        });
                        self.notice_on(
                            source_agent.as_ref(),
                            "compacting context",
                            StyleClass::SystemInfo,
                            cx,
                        );
                    }
                    None => self.notice_on(
                        None,
                        ":compact: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }
            Command::AgentRename { name } => {
                let target = source_agent.or_else(|| self.registry.selected_agent().cloned());
                match target {
                    Some(agent_id) => {
                        self.connection
                            .send(ClientMessage::RenameAgent { agent_id, name });
                    }
                    None => self.notice_on(
                        None,
                        ":agent rename: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }
            Command::AgentChangePromptCacheKey => {
                let target = source_agent.or_else(|| self.registry.selected_agent().cloned());
                match target {
                    Some(agent_id) => {
                        self.connection
                            .send(ClientMessage::ChangePromptCacheKey { agent_id });
                        self.notice_on(
                            source_agent.as_ref(),
                            "changed prompt cache key",
                            StyleClass::SystemInfo,
                            cx,
                        );
                    }
                    None => self.notice_on(
                        None,
                        ":agent change-prompt-cache-key: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }
            Command::TagRename { name } => {
                let Some(tag_id) = self.focused_workstream(source_agent) else {
                    self.notice_on(None, "no workstream in focus", StyleClass::SystemInfo, cx);
                    return;
                };
                self.connection
                    .send(ClientMessage::RenameTag { tag_id, name });
            }
            Command::AgentDone { hide } => {
                let disposition = if hide {
                    rho_ui_proto::AgentDisposition::Hidden
                } else {
                    rho_ui_proto::AgentDisposition::Done
                };
                let agent_id = self.set_agent_disposition(source_agent, ":done", disposition, cx);
                // Hiding the open agent closes its tab, or it would stay
                // rail-visible through the selection exemption.
                if hide && agent_id.is_some() && agent_id.as_ref() == self.registry.selected_agent()
                {
                    self.select_agent(None, window, cx);
                }
            }
            Command::AgentSnooze { duration_ms } => {
                let until = rho_core::UnixMs(now_ms().saturating_add(duration_ms));
                self.set_agent_disposition(
                    source_agent,
                    ":snooze",
                    rho_ui_proto::AgentDisposition::Snoozed { until },
                    cx,
                );
            }
            Command::AgentPin => {
                self.toggle_agent_status(source_agent, rho_ui_proto::Status::Pinned, cx);
            }
            Command::TagPin { name } => {
                self.toggle_workstream_status(source_agent, name, rho_ui_proto::Status::Pinned, cx);
            }
            Command::TagMove { name } => {
                self.tag_by_name(source_agent, name, rho_ui_proto::TagKind::Workstream, cx);
            }
            Command::TagGroup { name } => {
                self.tag_by_name(
                    source_agent,
                    name,
                    rho_ui_proto::TagKind::WorkstreamGroup,
                    cx,
                );
            }
            Command::TagLabel { name } => {
                self.tag_by_name(source_agent, name, rho_ui_proto::TagKind::Label, cx);
            }
            Command::TagUnlabel { name } => {
                let target = source_agent.or_else(|| self.registry.selected_agent().copied());
                let Some(agent_id) = target else {
                    self.notice_on(
                        None,
                        ":tag unlabel: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                };
                match rho_commands::resolve_tag(
                    &name,
                    &self.tag_labels(rho_ui_proto::TagKind::Label),
                ) {
                    Some(tag_id) => {
                        self.connection
                            .send(ClientMessage::UntagAgent { agent_id, tag_id });
                    }
                    None => {
                        let message = format!("no label named `{name}`");
                        self.notice_on(source_agent.as_ref(), &message, StyleClass::SystemInfo, cx);
                    }
                }
            }
            Command::ProjectAdd {
                path,
                name,
                description,
            } => {
                // Unlike the CLI, the GUI may run on another machine, so
                // there is no local directory to default to.
                let Some(path) = path else {
                    self.notice_on(
                        source_agent.as_ref(),
                        "usage: :projects add <path> [name]",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                };
                self.connection.send(ClientMessage::ProjectSet {
                    path: self.resolve_workdir_path(path),
                    name,
                    description,
                });
            }
            Command::ProjectRemove { path } => {
                match rho_commands::resolve_workdir(&path, &self.workdir_table()) {
                    Some(path) => {
                        self.connection
                            .send(ClientMessage::ProjectRemove { path: path.into() });
                    }
                    None => {
                        let message = format!("no registered project `{path}`");
                        self.notice_on(source_agent.as_ref(), &message, StyleClass::SystemInfo, cx);
                    }
                }
            }
            Command::Open { path } => {
                let target = source_agent.or_else(|| self.registry.selected_agent().copied());
                let Some(agent_id) = target else {
                    self.notice_on(None, ":open: no agent selected", StyleClass::SystemInfo, cx);
                    return;
                };
                let Some(workspace) = self.registry.agent_workspace(agent_id).cloned() else {
                    self.notice_on(
                        None,
                        ":open: agent has no workspace",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                };
                self.open_file_surface(agent_id, workspace, path, cx);
            }
            Command::Term { new } => {
                let target = source_agent.or_else(|| self.registry.selected_agent().copied());
                let Some(agent_id) = target else {
                    self.notice_on(None, ":term: no agent selected", StyleClass::SystemInfo, cx);
                    return;
                };
                self.open_terminal_surface(agent_id, new, cx);
            }

            Command::Quit => cx.quit(),
            Command::Help => {
                let help = rho_commands::COMMANDS
                    .iter()
                    .map(|spec| format!("{}  —  {}", spec.usage, spec.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.notice_on(source_agent.as_ref(), &help, StyleClass::SystemInfo, cx);
            }
            Command::Version => {
                self.notice_on(
                    source_agent.as_ref(),
                    env!("CARGO_PKG_VERSION"),
                    StyleClass::SystemInfo,
                    cx,
                );
            }
            Command::Clear => {
                self.notice_on(
                    source_agent.as_ref(),
                    ":clear is not available in rho-gui",
                    StyleClass::SystemInfo,
                    cx,
                );
            }
        }
    }

    /// Opens the draft compose view. `working_directory` is an explicit
    /// choice (`:agent new <path>`, rewrites the header even mid-draft);
    /// otherwise the scaffold default is derived from the inherited topic.
    pub fn enter_draft(
        &mut self,
        working_directory: Option<Utf8PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(selected) = self.registry.selected_agent().copied()
            && let Some(tag_id) = self.registry.workstream_of(selected)
        {
            self.draft_workstream = Some(tag_id);
        }
        match working_directory {
            Some(path) => {
                let path = self.resolve_workdir_path(path);
                let label = self.workdir_label(&path);
                let editor = self.focused_draft_editor();
                self.draft_model
                    .update(cx, |view, cx| view.seed(&label, true, editor.as_ref(), window, cx));
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

    /// Pin toggle for the addressed (else selected) agent.
    fn toggle_agent_status(
        &mut self,
        source_agent: Option<AgentId>,
        target: rho_ui_proto::Status,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = source_agent.or_else(|| self.registry.selected_agent().copied())
        else {
            self.notice_on(None, "no agent selected", StyleClass::SystemInfo, cx);
            return;
        };
        let status = rho_commands::toggle_status(self.registry.agent_status(agent_id), target);
        self.connection
            .send(ClientMessage::SetAgentStatus { agent_id, status });
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

    /// Kind-aware tagging by name for the addressed (else selected) agent;
    /// the daemon creates unknown tags.
    fn tag_by_name(
        &mut self,
        source_agent: Option<AgentId>,
        name: String,
        kind: rho_ui_proto::TagKind,
        cx: &mut Context<Self>,
    ) {
        let target = source_agent.or_else(|| self.registry.selected_agent().copied());
        let Some(agent_id) = target else {
            self.notice_on(None, ":tag: no agent selected", StyleClass::SystemInfo, cx);
            return;
        };
        self.connection.send(ClientMessage::TagAgent {
            agent_id,
            target: rho_ui_proto::TagTarget::Named { name, kind },
        });
    }

    /// Pin toggle for a workstream named by argument, defaulting to the
    /// focused agent's workstream.
    fn toggle_workstream_status(
        &mut self,
        source_agent: Option<AgentId>,
        name: Option<String>,
        target: rho_ui_proto::Status,
        cx: &mut Context<Self>,
    ) {
        let Some(tag_id) = self.named_or_focused_workstream(source_agent, name, cx) else {
            return;
        };
        let current = self
            .registry
            .workstreams()
            .iter()
            .find(|workstream| workstream.tag_id == tag_id)
            .map(|workstream| workstream.status)
            .unwrap_or(rho_ui_proto::Status::Normal);
        let status = rho_commands::toggle_status(current, target);
        self.connection
            .send(ClientMessage::SetTagStatus { tag_id, status });
    }

    fn named_or_focused_workstream(
        &mut self,
        source_agent: Option<AgentId>,
        name: Option<String>,
        cx: &mut Context<Self>,
    ) -> Option<rho_ui_proto::TagId> {
        match &name {
            Some(name) => {
                let resolved = rho_commands::resolve_tag(
                    name,
                    &self.tag_labels(rho_ui_proto::TagKind::Workstream),
                );
                if resolved.is_none() {
                    let message = format!("no workstream named `{name}`");
                    self.notice_on(source_agent.as_ref(), &message, StyleClass::SystemInfo, cx);
                }
                resolved
            }
            None => {
                let focused = self.focused_workstream(source_agent);
                if focused.is_none() {
                    self.notice_on(None, "no workstream in focus", StyleClass::SystemInfo, cx);
                }
                focused
            }
        }
    }

    fn focused_workstream(&self, source_agent: Option<AgentId>) -> Option<rho_ui_proto::TagId> {
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
        self.draft_model
            .update(cx, |view, cx| view.seed(&label, force_header, editor.as_ref(), window, cx));
    }

    /// Where a new agent works when the draft doesn't say: the inherited
    /// workstream's newest agent sets the precedent, else the first
    /// registered workdir. All daemon-side data — the GUI may run on another
    /// machine, so its own cwd is meaningless here.
    fn draft_default_workdir(&self) -> Option<Utf8PathBuf> {
        self.draft_workstream
            .and_then(|tag_id| self.registry.last_working_directory(tag_id))
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

    /// Tags of one kind as the `(name, id)` pairs shared resolution expects.
    fn tag_labels(&self, kind: rho_ui_proto::TagKind) -> Vec<(String, rho_ui_proto::TagId)> {
        self.registry
            .tags()
            .iter()
            .filter(|tag| tag.kind == kind)
            .map(|tag| (tag.name.clone(), tag.tag_id))
            .collect()
    }

    pub fn tag_names(&self) -> crate::commands::TagNames {
        let names = |kind| {
            self.tag_labels(kind)
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        crate::commands::TagNames {
            workstreams: names(rho_ui_proto::TagKind::Workstream),
            groups: names(rho_ui_proto::TagKind::WorkstreamGroup),
            labels: names(rho_ui_proto::TagKind::Label),
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
        rho_commands::resolve_workdir(path.as_str(), &self.workdir_table())
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
        self.focus_active_surface(window, cx);
        self.connection.focus_agent(agent_id);
        self.ensure_duration_timer(cx);
        cx.notify();
    }

    /// The active context's surface with the given key, whether or not
    /// any pane currently displays it.
    fn find_surface(&self, pred: impl Fn(&Surface) -> bool) -> Option<&Surface> {
        self.surfaces
            .get(&self.active_context)?
            .iter()
            .find(|surface| pred(surface))
    }

    /// Emacs `display-buffer`: the one place pane choice happens. The
    /// surface joins the context's surface list first, so it stays alive
    /// however panes shuffle afterwards. Then, in order: a pane already
    /// showing it wins (the arrangement stays intact); a conversation
    /// surface arriving while an artifact pane (file, terminal) is
    /// focused lands in a pane already showing conversation, so
    /// switching agents never covers something opened deliberately;
    /// otherwise the focused pane. Founds the context's tree on its
    /// first visit.
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
        let target = tree.pane_showing(|s| s.key == surface.key).or_else(|| {
            (surface.key.is_conversation() && !tree.focused().surface.key.is_conversation())
                .then(|| tree.pane_showing(|s| s.key.is_conversation()))
                .flatten()
        });
        if let Some(pane) = target {
            tree.focus(pane);
        }
        tree.focused_mut().show(surface);
    }

    /// `:open`: dials a zed channel for the agent's workspace (once per
    /// file) and shows the file surface in the main pane.
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
        let task =
            crate::zed_remote::open_file_buffer(&self.connection, workspace, path.clone(), cx);
        cx.spawn(async move |this, cx| match task.await {
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
        })
        .detach();
    }

    /// `:term`: dials a dedicated terminal stream for the agent (attaching
    /// its first running terminal, spawning the default one when none run,
    /// or a fresh one with `new`) and shows the terminal surface.
    fn open_terminal_surface(&mut self, agent_id: AgentId, new: bool, cx: &mut Context<Self>) {
        if !new
            && let Some(surface) = self
                .find_surface(|s| {
                    matches!(s.key, SurfaceKey::Terminal { agent_id: id, .. } if id == agent_id)
                })
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
                        let view =
                            cx.new(|cx| crate::terminal_view::TerminalView::new(channel, cx));
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

    fn materialize_model(&mut self, agent_id: &AgentId, cx: &mut Context<Self>) -> Entity<AgentModel> {
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

    /// Moves gpui focus to the focused pane's surface.
    fn focus_active_surface(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.active_tree().focused().surface.view {
            SurfaceView::Draft { editor, .. } => window.focus(&editor.focus_handle(cx), cx),
            SurfaceView::Transcript { editor, .. } => window.focus(&editor.focus_handle(cx), cx),
            SurfaceView::File(view) => window.focus(&view.read(cx).editor().focus_handle(cx), cx),
            SurfaceView::Terminal(view) => window.focus(&view.read(cx).focus_handle(cx), cx),
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
            SurfaceKey::Terminal { .. } => {
                unreachable!("terminal surfaces are created by open_terminal_surface")
            }
        };
        Self::wrap_surface(key, view, window, cx)
    }

    /// A surface for a new pane over the same content as `surface`: file
    /// and transcript panes get a fresh editor (own cursor, scroll, folds)
    /// over the shared model; the draft still shares its editor until its
    /// own model/view split lands.
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
            // Terminals share their view between panes: splitting a pane
            // does not attach a second wire client.
            SurfaceView::Terminal(_) => surface,
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
            SurfaceKey::Transcript(agent_id) => self.registry.select_agent(agent_id),
            SurfaceKey::Terminal { agent_id, .. } => self.registry.select_agent(agent_id),
            SurfaceKey::Draft => self.registry.enter_draft(),
            // Files keep whatever agent context was current.
            SurfaceKey::File { .. } => {}
        }
        cx.notify();
    }

    fn split_pane(&mut self, axis: SplitAxis, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.active_tree().focused().surface.clone();
        let sibling = self.duplicate_surface(focused, window, cx);
        self.active_tree_mut().split(axis, sibling);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    fn close_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_tree_mut().close_focused();
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    fn focus_pane_by_delta(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.active_tree_mut().focus_by_delta(delta);
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    fn pane_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_tree_mut().focused_mut().back() {
            self.sync_selection_to_focus(cx);
            self.focus_active_surface(window, cx);
            cx.notify();
        }
    }

    /// `space :`, the emacs `M-x`: run any `:` command from anywhere, with
    /// the same completion grammar as the inline prompt.
    fn open_command_minibuffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, _cx: &gpui::App| {
            let text = format!(":{input}");
            crate::commands::completions_for(
                &text,
                &workspace.workdir_table(),
                &workspace.live_agent_targets(),
                &workspace.tag_names(),
            )
            .into_iter()
            .map(|mut candidate| {
                // The prompt already shows the `:`; keep the input bare.
                if let Some(bare) = candidate.value.strip_prefix(':') {
                    candidate.value = bare.to_owned();
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
                let input = input.trim().to_owned();
                if input.is_empty() {
                    return;
                }
                let text = format!(":{input}");
                match rho_commands::parse(&text) {
                    Some(parsed) => {
                        let agent_id = workspace.registry.selected_agent().copied();
                        workspace.handle_command(agent_id, parsed, window, cx);
                    }
                    None => workspace.notice_on(
                        None,
                        &format!("not a command: {text}"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            },
        );
        let text_style = self
            .active_editor(cx)
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let mut minibuffer = Minibuffer::open(":", &text_style, complete, on_submit, window, cx);
        minibuffer.refresh(self, cx);
        self.minibuffer = Some(minibuffer);
        // The minibuffer takes over the bottom strip; a stale message
        // reappearing after it closes would be confusing.
        self.echo = None;
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
        let Some(minibuffer) = self.minibuffer.take() else {
            return;
        };
        let (input, on_submit) = minibuffer.into_submission(cx);
        self.focus_active_surface(window, cx);
        on_submit(self, input, window, cx);
        cx.notify();
    }

    fn minibuffer_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.take().is_some() {
            self.focus_active_surface(window, cx);
            cx.notify();
        }
    }

    /// `space r`: the rail is ambient chrome, not a pane — focus jumps to
    /// it directly and never lands on it through the pane cycle.
    fn focus_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.rail_focus, cx);
        cx.notify();
    }

    /// Enter from the rail: return focus to the active context's tree.
    fn leave_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_selection_to_focus(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// The ambient rail beside the active context's tree: reachable by
    /// `space r` or the mouse, outside the pane cycle.
    fn render_rail(
        &self,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .track_focus(&self.rail_focus)
            .key_context("RhoRail")
            .child(crate::topic_rail::render_topic_rail(
                &self.registry,
                text_style,
                cx,
            ))
            .into_any_element()
    }

    fn render_panes(
        &self,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let rail = self.render_rail(text_style, cx);
        let mut leaf = |pane: &crate::pane::Pane<Surface>| -> gpui::AnyElement {
            let id = pane.id;
            let content = self.render_surface(&pane.surface);
            div()
                .h_full()
                .overflow_hidden()
                .flex_grow(1.0)
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
            let element = div().flex().size_full().flex_grow(1.0).min_h_0().min_w_0();
            let element = match axis {
                SplitAxis::Row => element.flex_row(),
                SplitAxis::Column => element.flex_col(),
            };
            element.children(children).into_any_element()
        };
        div()
            .flex()
            .flex_row()
            .w_full()
            .flex_grow(1.0)
            .min_h_0()
            .child(rail)
            .child(self.active_tree().layout(&mut leaf, &mut container))
            .into_any_element()
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.active_editor(cx);
        let text_style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
        div()
            .id("rho-gui")
            .size_full()
            .flex()
            .flex_col()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .key_context("RhoGui")
            .on_action(cx.listener(Self::submit_prompt))
            .on_action(cx.listener(|this, _: &AgentPrevious, window, cx| {
                this.switch_agent_by_delta(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentNext, window, cx| {
                this.switch_agent_by_delta(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentNew, window, cx| {
                this.enter_draft(None, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentJumpAttention, window, cx| {
                this.jump_to_attention(window, cx);
            }))
            .on_action(cx.listener(|this, _: &AgentDone, window, cx| {
                // Escalating: a first press acknowledges the turn; pressing
                // again on an already-quiet agent files it away.
                let selected = this.registry.selected_agent().copied();
                let quiet = selected.is_some_and(|agent_id| {
                    this.registry.attention(agent_id) == rho_ui_proto::UiAttention::Quiet
                });
                let disposition = if quiet {
                    rho_ui_proto::AgentDisposition::Hidden
                } else {
                    rho_ui_proto::AgentDisposition::Done
                };
                this.set_agent_disposition(None, ":done", disposition, cx);
                if quiet {
                    this.select_agent(None, window, cx);
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
                this.leave_rail(window, cx);
            }))
            .on_action(cx.listener(|this, _: &MinibufferCommand, window, cx| {
                this.open_command_minibuffer(window, cx);
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
            .child(self.render_panes(&text_style, cx))
            .children(match (&self.minibuffer, &self.echo) {
                (Some(minibuffer), _) => Some(minibuffer.render(&text_style, cx)),
                (None, Some(echo)) => Some(echo.render(&text_style, cx)),
                (None, None) => None,
            })
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
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
