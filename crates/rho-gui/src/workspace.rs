//! Root entity: owns the daemon connection, the canonical agent states, the
//! registry, and one persistent [`AgentView`] per opened agent.
//!
//! All protocol events flow through [`Workspace::handle_event`]; views receive
//! already-summarized state changes and never see the protocol.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::prelude::*;
use gpui::{Context, Entity, Focusable as _, Task, Window, div, px};
use rho_core::ContentPart;
use rho_ui_proto::{
    AgentId, AgentMode, ClientMessage, DeepConfig, DeepEffort, FableEffort, MessageDelivery,
};
use theme::ActiveTheme as _;

use crate::agent_view::AgentView;
use crate::connection::{ConnEvent, Connection};
use crate::draft_view::DraftView;
use crate::registry::{ActivePane, AgentRegistry};
use crate::store::{AgentStore, FrameSummary};
use crate::style::{ModeFamily, StyleClass};
use crate::{
    AgentNew, AgentNext, AgentPrevious, RoleCycle, RoleCycleGroup, SubmitPrompt, TaskBoard,
};

/// How to reach the daemon. Deliberately holds no client-local paths: the
/// socket may be forwarded from another machine, so the GUI's own cwd and
/// home mean nothing to the daemon and must never leak into agent working
/// directories.
#[derive(Clone)]
pub struct AttachTarget {
    pub socket_path: PathBuf,
}

pub struct Workspace {
    connection: Connection,
    store: AgentStore,
    registry: AgentRegistry,
    views: HashMap<AgentId, Entity<AgentView>>,
    /// Accumulated change summaries for materialized but hidden views; they
    /// render once, with the merged summary, when next selected.
    pending_syncs: HashMap<AgentId, FrameSummary>,
    draft_view: Entity<DraftView>,
    /// Registered workdirs from the daemon; selection vocabulary for new
    /// agents.
    workdirs: Vec<rho_ui_proto::UiWorkdir>,
    /// Topic the draft inherits: whichever topic was focused when the draft
    /// was entered. Topics are ad-hoc tab groups — a new agent lands in the
    /// default topic unless created from inside one.
    draft_topic_id: Option<rho_ui_proto::TopicId>,
    /// Announced by the daemon in `Ready`; where draft submissions land when
    /// no topic was inherited.
    default_topic_id: Option<rho_ui_proto::TopicId>,
    /// A NewAgent request from the draft is in flight; the draft buffer is
    /// kept intact until the daemon confirms creation, so a rejected request
    /// (bad working directory, say) never loses the message.
    awaiting_draft_agent: bool,
    /// Rail view mode: browsing archived topics/agents instead of active
    /// ones.
    show_archived: bool,
    connected: bool,
    duration_timer: Option<Task<()>>,
    _event_task: Task<()>,
}

impl Workspace {
    pub fn new(attach_target: AttachTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (connection, events) = crate::connection::spawn(attach_target.socket_path.clone(), cx);
        let workspace = cx.entity().downgrade();
        let draft_view = cx.new(|cx| DraftView::new(workspace, window, cx));
        let event_task = cx.spawn(async move |this, cx| {
            let mut events: UnboundedReceiver<ConnEvent> = events;
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                let updated = this.update_in(cx, |this, window, cx| {
                    for event in batch {
                        this.handle_event(event, window, cx);
                    }
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
            views: HashMap::new(),
            pending_syncs: HashMap::new(),
            draft_view,
            workdirs: Vec::new(),
            draft_topic_id: None,
            default_topic_id: None,
            awaiting_draft_agent: false,
            show_archived: false,
            connected: false,
            duration_timer: None,
            _event_task: event_task,
        };
        this.seed_draft(false, window, cx);
        this.focus_active_editor(window, cx);
        this
    }

    pub(crate) fn handle_event(
        &mut self,
        event: ConnEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConnEvent::Ready {
                topics,
                workdirs,
                default_topic_id,
                machine_seed,
                agent_counter,
                workspace_counter,
            } => {
                let first_ready = !self.connected;
                self.registry.set_machine_seed(machine_seed);
                self.registry.set_agent_counter(agent_counter);
                self.registry.set_workspace_counter(workspace_counter);
                self.registry.set_topics(topics);
                self.workdirs = workdirs;
                self.default_topic_id = Some(default_topic_id);
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
            ConnEvent::TopicCreated(topic) => {
                self.registry.add_topic(topic);
                self.refresh_draft_agent_targets(cx);
                cx.notify();
            }
            ConnEvent::AgentCreated(agent_id) => {
                self.registry.mark_known(agent_id);
                if self.awaiting_draft_agent {
                    self.awaiting_draft_agent = false;
                    // The draft became this agent: reset the compose surface
                    // and follow the new agent.
                    let label = self
                        .draft_default_workdir()
                        .map(|path| self.workdir_label(&path))
                        .unwrap_or_default();
                    self.draft_view.update(cx, |view, cx| {
                        view.set_body_text("", cx);
                        view.set_workdir_text(&label, cx);
                        view.set_mode_text(crate::draft_view::DEFAULT_MODE, cx);
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
            ConnEvent::Frame { agent_id, frame } => {
                let old_context = self
                    .store
                    .get(&agent_id)
                    .and_then(|state| state.context_used);
                let summary = self.store.apply(agent_id, frame);
                self.registry.mark_live(agent_id);
                self.refresh_draft_agent_targets(cx);
                let new_context = self
                    .store
                    .get(&agent_id)
                    .and_then(|state| state.context_used);
                if old_context != new_context
                    && let Some(view) = self.views.get(&agent_id).cloned()
                {
                    self.refresh_view_status(&agent_id, &view, cx);
                }
                if matches!(self.registry.active_pane(), ActivePane::Startup) {
                    // First live agent: show it. Materialization renders the
                    // full state, so the per-frame sync below is unnecessary.
                    self.select_agent(Some(agent_id), window, cx);
                } else if self.registry.selected_agent() == Some(&agent_id) {
                    if let Some(view) = self.views.get(&agent_id).cloned()
                        && let Some(state) = self.store.get(&agent_id)
                    {
                        view.update(cx, |view, cx| view.sync(state, summary, now_ms(), cx));
                    }
                } else if self.views.contains_key(&agent_id) {
                    self.pending_syncs
                        .entry(agent_id)
                        .and_modify(|pending| *pending = pending.merge(summary))
                        .or_insert(summary);
                }
                self.ensure_duration_timer(cx);
                cx.notify();
            }
            ConnEvent::TurnCancelled(agent_id) => {
                if let Some(view) = self.views.get(&agent_id).cloned() {
                    view.update(cx, |view, cx| {
                        view.system_notice("[turn cancelled]", StyleClass::SystemInfo, cx);
                    });
                }
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
                for view in self.views.values() {
                    view.update(cx, |view, cx| {
                        view.system_notice(&notice, StyleClass::Disconnect, cx);
                    });
                }
                self.draft_view.update(cx, |view, cx| {
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
                let Some(view) = self.views.get(&agent_id).cloned() else {
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
    }

    /// Submitting the compose surface creates the agent: the workdir field
    /// picks the working directory, the topic is whatever the draft
    /// inherited. The buffers are not cleared here — they survive until the
    /// daemon confirms creation.
    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.draft_view.read(cx).body_text(cx).trim().to_owned();
        if body.is_empty() {
            // Enter in the workdir field with nothing to send: jump to the
            // body instead of submitting.
            self.draft_view
                .update(cx, |view, cx| view.focus_body(window, cx));
            return;
        }
        if let Some(parsed) = rho_commands::parse(&body) {
            // A command typed into the draft body: run it and clear just the
            // body, keeping the chosen workdir.
            self.draft_view
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
        let Some(topic_id) = self.draft_target_topic() else {
            self.notice_on(
                None,
                "no rho topic is available",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let field = self.draft_view.read(cx).workdir_text(cx).trim().to_owned();
        let working_directory = (!field.is_empty())
            .then(|| self.resolve_workdir_path(Utf8PathBuf::from(field)))
            .or_else(|| self.draft_default_workdir());
        let start = {
            let draft = self.draft_view.read(cx);
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
        let mode = match parse_agent_mode(self.draft_view.read(cx).mode_text(cx).trim()) {
            Ok(mode) => mode,
            Err(message) => {
                self.notice_on(None, &message, StyleClass::SystemInfo, cx);
                return;
            }
        };
        self.awaiting_draft_agent = true;
        self.connection.send(ClientMessage::NewAgent {
            topic_id,
            mode,
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
                 Workdir field, or register one with :workdirs add <path>"
                    .to_owned()
            })
        };
        let workspace = self
            .registry
            .agent_by_label(target)
            .and_then(|agent_id| self.registry.agent_workspace(agent_id))
            .cloned();
        Ok(match (mode, target, workspace) {
            (StartFieldMode::NewOn, "", _) => {
                return Err("pick a base: a revset like `@-` or an agent label".to_owned());
            }
            (StartFieldMode::NewOn, _, Some(WorkspaceInfo::Workspace { repo, id })) => {
                StartMode::NewOn {
                    repo,
                    revset: format!("{}@", id.encoded()),
                }
            }
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
            Command::TopicNew { name } => {
                self.connection.send(ClientMessage::NewTopic { name });
            }
            Command::TopicRename { name } => {
                let Some(topic_id) = self.focused_topic_id(source_agent) else {
                    self.notice_on(None, "no topic in focus", StyleClass::SystemInfo, cx);
                    return;
                };
                self.connection
                    .send(ClientMessage::RenameTopic { topic_id, name });
            }
            Command::AgentPin => {
                self.toggle_agent_status(source_agent, rho_ui_proto::Status::Pinned, window, cx);
            }
            Command::AgentArchive => {
                self.toggle_agent_status(source_agent, rho_ui_proto::Status::Archived, window, cx);
            }
            Command::AgentFast { enabled } => {
                self.update_deep_config(
                    source_agent,
                    ":agent fast: no agent selected",
                    |config| {
                        config.fast_mode = enabled.unwrap_or(!config.fast_mode);
                    },
                    cx,
                );
            }
            Command::AgentEffort { effort } => {
                self.update_agent_effort(source_agent, effort, cx);
            }
            Command::TopicPin { name } => {
                self.toggle_topic_status(source_agent, name, rho_ui_proto::Status::Pinned, cx);
            }
            Command::TopicArchive { name } => {
                self.toggle_topic_status(source_agent, name, rho_ui_proto::Status::Archived, cx);
            }
            Command::TopicMove { name } => {
                let target = source_agent.or_else(|| self.registry.selected_agent().copied());
                let Some(agent_id) = target else {
                    self.notice_on(
                        None,
                        ":topic move: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                };
                let topic = match rho_commands::resolve_topic(&name, &self.topic_labels()) {
                    Some(topic_id) => rho_ui_proto::TopicTarget::Existing(topic_id),
                    None => rho_ui_proto::TopicTarget::Named(name),
                };
                self.connection
                    .send(ClientMessage::MoveAgent { agent_id, topic });
            }
            Command::WorkdirAdd { path, name } => {
                // Unlike the CLI, the GUI may run on another machine, so
                // there is no local directory to default to.
                let Some(path) = path else {
                    self.notice_on(
                        source_agent.as_ref(),
                        "usage: :workdirs add <path> [name]",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                };
                self.connection.send(ClientMessage::WorkdirSet {
                    path: self.resolve_workdir_path(path),
                    name,
                });
            }
            Command::WorkdirRemove { path } => {
                match rho_commands::resolve_workdir(&path, &self.workdir_table()) {
                    Some(path) => {
                        self.connection
                            .send(ClientMessage::WorkdirRemove { path: path.into() });
                    }
                    None => {
                        let message = format!("no registered workdir `{path}`");
                        self.notice_on(source_agent.as_ref(), &message, StyleClass::SystemInfo, cx);
                    }
                }
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
            && let Some(topic_id) = self.registry.topic_of(selected)
        {
            self.draft_topic_id = Some(topic_id);
        }
        match working_directory {
            Some(path) => {
                let path = self.resolve_workdir_path(path);
                let label = self.workdir_label(&path);
                self.draft_view
                    .update(cx, |view, cx| view.seed(&label, true, window, cx));
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

    pub fn toggle_show_archived(&mut self, cx: &mut Context<Self>) {
        self.show_archived = !self.show_archived;
        cx.notify();
    }

    /// Selects an agent, asking the daemon to load it first when this
    /// connection has never seen frames for it.
    pub fn open_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        if self.connected && !self.registry.is_live(agent_id) {
            self.connection.send(ClientMessage::LoadAgent { agent_id });
        }
        self.select_agent(Some(agent_id), window, cx);
    }

    /// Pin/archive toggle for the addressed (else selected) agent. Archiving
    /// the selected agent closes its tab: the view returns to the draft.
    fn toggle_agent_status(
        &mut self,
        source_agent: Option<AgentId>,
        target: rho_ui_proto::Status,
        window: &mut Window,
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
        if status == rho_ui_proto::Status::Archived
            && self.registry.selected_agent() == Some(&agent_id)
        {
            self.select_agent(None, window, cx);
        }
    }

    /// Pin/archive toggle for a topic named by argument, defaulting to the
    /// focused agent's topic (else the default topic).
    fn toggle_topic_status(
        &mut self,
        source_agent: Option<AgentId>,
        name: Option<String>,
        target: rho_ui_proto::Status,
        cx: &mut Context<Self>,
    ) {
        let topic_id = match &name {
            Some(name) => {
                let Some(topic_id) = rho_commands::resolve_topic(name, &self.topic_labels()) else {
                    let message = format!("no topic named `{name}`");
                    self.notice_on(source_agent.as_ref(), &message, StyleClass::SystemInfo, cx);
                    return;
                };
                topic_id
            }
            None => match self.focused_topic_id(source_agent) {
                Some(topic_id) => topic_id,
                None => {
                    self.notice_on(None, "no topic in focus", StyleClass::SystemInfo, cx);
                    return;
                }
            },
        };
        let current = self
            .registry
            .topics()
            .iter()
            .find(|topic| topic.topic_id == topic_id)
            .map(|topic| topic.status)
            .unwrap_or(rho_ui_proto::Status::Normal);
        let status = rho_commands::toggle_status(current, target);
        self.connection
            .send(ClientMessage::SetTopicStatus { topic_id, status });
    }

    fn focused_topic_id(&self, source_agent: Option<AgentId>) -> Option<rho_ui_proto::TopicId> {
        source_agent
            .or_else(|| self.registry.selected_agent().copied())
            .and_then(|agent_id| self.registry.topic_of(agent_id))
            .or(self.draft_topic_id)
            .or(self.default_topic_id)
    }

    /// Tab in the draft cycles the `Workdir:` field, the start field, and
    /// the body. On agent views it does nothing.
    fn cycle_draft_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.registry.selected_agent().is_none() {
            self.draft_view
                .update(cx, |view, cx| view.toggle_field(window, cx));
        }
    }

    /// Shift-Tab in the draft: with the cursor in the start field, flip its
    /// mode (on top of ↔ join); anywhere else, cycle fields like Tab. On agent
    /// views it does nothing.
    fn cycle_draft_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.registry.selected_agent().is_none() {
            self.draft_view.update(cx, |view, cx| {
                if view.cursor_in_mode_field(cx) {
                    let next = cycle_agent_mode_text(&view.mode_text(cx));
                    view.set_mode_text(next, cx);
                } else if view.cursor_in_start_field(cx) {
                    view.cycle_start_mode(cx);
                } else {
                    view.toggle_field(window, cx);
                }
            });
        }
    }

    fn update_deep_config(
        &mut self,
        source_agent: Option<AgentId>,
        no_target_message: &str,
        update: impl FnOnce(&mut DeepConfig),
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = source_agent.or_else(|| self.registry.selected_agent().copied())
        else {
            self.notice_on(None, no_target_message, StyleClass::SystemInfo, cx);
            return;
        };
        let Some(AgentMode::Deep(mut config)) = self.registry.agent_mode(agent_id) else {
            self.notice_on(
                Some(&agent_id),
                "mode changes are only available for deep agents",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        update(&mut config);
        self.connection.send(ClientMessage::SetAgentMode {
            agent_id,
            mode: AgentMode::Deep(config),
        });
    }

    fn update_agent_effort(
        &mut self,
        source_agent: Option<AgentId>,
        effort: DeepEffort,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = source_agent.or_else(|| self.registry.selected_agent().copied())
        else {
            self.notice_on(
                None,
                ":agent effort: no agent selected",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let Some(mode) = self.registry.agent_mode(agent_id) else {
            return;
        };
        let mode = match mode {
            AgentMode::Deep(mut config) => {
                config.effort = effort;
                AgentMode::Deep(config)
            }
            AgentMode::Fable { .. } => {
                let effort = match effort {
                    DeepEffort::Low => {
                        self.notice_on(
                            Some(&agent_id),
                            "low effort is only available for deep agents",
                            StyleClass::SystemInfo,
                            cx,
                        );
                        return;
                    }
                    DeepEffort::Medium => FableEffort::Medium,
                    DeepEffort::Xhigh => FableEffort::Xhigh,
                };
                AgentMode::Fable { effort }
            }
        };
        self.connection
            .send(ClientMessage::SetAgentMode { agent_id, mode });
    }

    /// (Re)writes the draft scaffold with the derived default workdir; the
    /// field stays empty when nothing daemon-side suggests one.
    fn seed_draft(&mut self, force_header: bool, window: &mut Window, cx: &mut Context<Self>) {
        let label = self
            .draft_default_workdir()
            .map(|path| self.workdir_label(&path))
            .unwrap_or_default();
        self.draft_view
            .update(cx, |view, cx| view.seed(&label, force_header, window, cx));
    }

    /// The topic a draft submission lands in: the inherited topic unless it
    /// has since been archived (new agents must stay visible), else the
    /// default topic.
    fn draft_target_topic(&self) -> Option<rho_ui_proto::TopicId> {
        self.draft_topic_id
            .filter(|topic_id| !self.topic_archived(*topic_id))
            .or(self.default_topic_id)
    }

    fn topic_archived(&self, topic_id: rho_ui_proto::TopicId) -> bool {
        self.registry.topics().iter().any(|topic| {
            topic.topic_id == topic_id && topic.status == rho_ui_proto::Status::Archived
        })
    }

    /// Where a new agent works when the draft doesn't say: the target
    /// topic's newest agent sets the precedent, else the first registered
    /// workdir. All daemon-side data — the GUI may run on another machine,
    /// so its own cwd is meaningless here.
    fn draft_default_workdir(&self) -> Option<Utf8PathBuf> {
        self.draft_target_topic()
            .and_then(|topic_id| self.registry.last_working_directory(topic_id))
            .or_else(|| self.workdirs.first().map(|workdir| workdir.path.clone()))
    }

    /// How a path reads in the draft header: its registered workdir name
    /// when it has one, else the full path.
    fn workdir_label(&self, path: &Utf8Path) -> String {
        self.workdirs
            .iter()
            .find(|workdir| workdir.path == path)
            .map(|workdir| workdir.name.clone())
            .unwrap_or_else(|| path.to_string())
    }

    /// Topics as the `(name, id)` pairs shared resolution expects.
    fn topic_labels(&self) -> Vec<(String, rho_ui_proto::TopicId)> {
        self.registry
            .topics()
            .iter()
            .map(|topic| (topic.name.clone(), topic.topic_id))
            .collect()
    }

    pub fn topic_names(&self) -> Vec<String> {
        self.topic_labels()
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    /// Registered workdirs as the `(name, path)` table the shared command
    /// layer expects.
    pub fn workdir_table(&self) -> Vec<(String, String)> {
        self.workdirs
            .iter()
            .map(|workdir| (workdir.name.clone(), workdir.path.to_string()))
            .collect()
    }

    /// A registered workdir name resolves to its path; anything else passes
    /// through untouched. Paths name directories on the daemon's machine,
    /// so the GUI never joins its own cwd or expands its own home — the
    /// daemon expands `~` and validates.
    fn resolve_workdir_path(&self, path: Utf8PathBuf) -> Utf8PathBuf {
        rho_commands::resolve_workdir(path.as_str(), &self.workdir_table())
            .map(Utf8PathBuf::from)
            .unwrap_or(path)
    }

    fn notice_on(
        &self,
        agent_id: Option<&AgentId>,
        text: &str,
        class: StyleClass,
        cx: &mut Context<Self>,
    ) {
        let view = agent_id
            .or_else(|| self.registry.selected_agent())
            .and_then(|agent_id| self.views.get(agent_id))
            .cloned();
        match view {
            Some(view) => view.update(cx, |view, cx| view.system_notice(text, class, cx)),
            None => self
                .draft_view
                .update(cx, |view, cx| view.system_notice(text, class, cx)),
        }
    }

    pub fn select_agent(
        &mut self,
        agent_id: Option<AgentId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(agent_id) = &agent_id {
            let view = self.materialize_view(agent_id, window, cx);
            view.update(cx, |view, cx| view.tick_timers(now_ms(), cx));
        }
        match agent_id {
            Some(agent_id) => self.registry.select_agent(agent_id),
            None => self.registry.enter_draft(),
        }
        self.focus_active_editor(window, cx);
        self.ensure_duration_timer(cx);
        cx.notify();
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

    fn materialize_view(
        &mut self,
        agent_id: &AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<AgentView> {
        let deferred = self.pending_syncs.remove(agent_id);
        if let Some(view) = self.views.get(agent_id).cloned() {
            if let (Some(summary), Some(state)) = (deferred, self.store.get(agent_id)) {
                view.update(cx, |view, cx| view.sync(state, summary, now_ms(), cx));
            }
            return view;
        }
        // A freshly created view renders the full state below, which
        // subsumes any deferred summary.
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| AgentView::new(workspace, window, cx));
        if let Some(state) = self.store.get(agent_id) {
            view.update(cx, |view, cx| {
                view.sync(state, FrameSummary::everything(), now_ms(), cx);
            });
        }
        self.refresh_view_status(agent_id, &view, cx);
        self.views.insert(*agent_id, view.clone());
        view
    }

    /// Recomputes the right-prompt status chips for one agent's view.
    fn refresh_view_status(
        &self,
        agent_id: &AgentId,
        view: &Entity<AgentView>,
        cx: &mut Context<Self>,
    ) {
        let directory_label = self.working_directory_label(agent_id);
        let workspace_label = self.registry.workspace_id_label(*agent_id);
        let mode_label = self.mode_label(agent_id);
        let context_used = self
            .store
            .get(agent_id)
            .and_then(|state| state.context_used);
        view.update(cx, |view, cx| {
            view.set_status(
                &directory_label,
                workspace_label.as_deref(),
                mode_label
                    .as_ref()
                    .map(|label| (label.text.as_str(), label.family)),
                context_used,
                cx,
            )
        });
    }

    #[cfg(test)]
    pub(crate) fn agent_view(&self, agent_id: &AgentId) -> Option<Entity<AgentView>> {
        self.views.get(agent_id).cloned()
    }

    pub(crate) fn active_agent_view(&self) -> Option<Entity<AgentView>> {
        self.registry
            .selected_agent()
            .and_then(|agent_id| self.views.get(agent_id))
            .cloned()
    }

    /// The editor the user is typing into: the selected agent's, or the
    /// draft compose view's when nothing is selected.
    pub(crate) fn active_editor(&self, cx: &gpui::App) -> Entity<editor::Editor> {
        match self.active_agent_view() {
            Some(view) => view.read(cx).editor().clone(),
            None => self.draft_view.read(cx).editor().clone(),
        }
    }

    fn focus_active_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.active_editor(cx);
        window.focus(&editor.focus_handle(cx), cx);
    }

    fn update_statuses(&self, cx: &mut Context<Self>) {
        for (agent_id, view) in &self.views {
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

    fn mode_label(&self, agent_id: &AgentId) -> Option<ModeLabel> {
        self.registry.agent_mode(*agent_id).map(agent_mode_label)
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
        self.draft_view
            .update(cx, |view, cx| view.set_start_target_hints(hints, cx));
    }

    fn ensure_duration_timer(&mut self, cx: &mut Context<Self>) {
        if self.duration_timer.is_some() {
            return;
        }
        if !self
            .active_agent_view()
            .is_some_and(|view| view.read(cx).has_timers())
        {
            return;
        }
        self.duration_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let keep_going = this.update(cx, |this, cx| {
                    let Some(view) = this.active_agent_view() else {
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

fn parse_agent_mode(text: &str) -> Result<AgentMode, String> {
    let mut words = text.split_whitespace();
    let kind = words.next().unwrap_or("deep").to_ascii_lowercase();
    let effort = words.next().unwrap_or("medium").to_ascii_lowercase();
    if words.next().is_some() {
        return Err("mode must be `deep [low|medium|xhigh]` or `fable [medium|xhigh]`".to_owned());
    }
    match kind.as_str() {
        "deep" => Ok(AgentMode::Deep(DeepConfig {
            effort: match effort.as_str() {
                "low" => DeepEffort::Low,
                "medium" => DeepEffort::Medium,
                "xhigh" => DeepEffort::Xhigh,
                _ => {
                    return Err(format!(
                        "unknown deep effort `{effort}`; use low, medium, or xhigh"
                    ));
                }
            },
            fast_mode: true,
        })),
        "fable" => Ok(AgentMode::Fable {
            effort: match effort.as_str() {
                "medium" => FableEffort::Medium,
                "xhigh" => FableEffort::Xhigh,
                _ => {
                    return Err(format!(
                        "unknown fable effort `{effort}`; use medium or xhigh"
                    ));
                }
            },
        }),
        _ => Err(format!("unknown mode `{kind}`; use deep or fable")),
    }
}

fn cycle_agent_mode_text(current: &str) -> &'static str {
    match parse_agent_mode(current).unwrap_or_else(|_| AgentMode::deep_default()) {
        AgentMode::Deep(DeepConfig {
            effort: DeepEffort::Low,
            ..
        }) => "deep medium",
        AgentMode::Deep(DeepConfig {
            effort: DeepEffort::Medium,
            ..
        }) => "deep xhigh",
        AgentMode::Deep(DeepConfig {
            effort: DeepEffort::Xhigh,
            ..
        }) => "fable medium",
        AgentMode::Fable {
            effort: FableEffort::Medium,
        } => "fable xhigh",
        AgentMode::Fable {
            effort: FableEffort::Xhigh,
        } => "deep low",
    }
}

struct ModeLabel {
    text: String,
    family: ModeFamily,
}

fn agent_mode_label(mode: AgentMode) -> ModeLabel {
    match mode {
        AgentMode::Deep(config) => {
            let effort = config.effort;
            let fast_mode = config.fast_mode;
            let effort = match effort {
                DeepEffort::Low => "¹",
                DeepEffort::Medium => "²",
                DeepEffort::Xhigh => "³",
            };
            let fast = if fast_mode { "⚡" } else { "" };
            ModeLabel {
                text: format!("deep{effort}{fast}"),
                family: ModeFamily::Deep,
            }
        }
        AgentMode::Fable { effort } => {
            let suffix = match effort {
                FableEffort::Medium => "",
                FableEffort::Xhigh => "²",
            };
            ModeLabel {
                text: format!("fable{suffix}"),
                family: ModeFamily::Fable,
            }
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.active_editor(cx);
        let text_style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
        let rail = crate::topic_rail::render_topic_rail(
            &self.registry,
            self.show_archived,
            &text_style,
            cx,
        );
        div()
            .id("rho-gui")
            .size_full()
            .flex()
            .flex_row()
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
            .child(rail)
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
    fn parses_agent_mode_field() {
        assert_eq!(
            parse_agent_mode("deep low").unwrap(),
            AgentMode::Deep(DeepConfig {
                effort: DeepEffort::Low,
                fast_mode: true,
            })
        );
        assert_eq!(
            parse_agent_mode("fable xhigh").unwrap(),
            AgentMode::Fable {
                effort: FableEffort::Xhigh,
            }
        );
        assert!(parse_agent_mode("fable low").is_err());
        assert!(parse_agent_mode("sonnet medium").is_err());
    }

    #[test]
    fn renders_compact_agent_mode_labels() {
        let low = agent_mode_label(AgentMode::Deep(DeepConfig {
            effort: DeepEffort::Low,
            fast_mode: true,
        }));
        assert_eq!(low.text, "deep¹⚡");
        assert_eq!(low.family, ModeFamily::Deep);

        let medium = agent_mode_label(AgentMode::Deep(DeepConfig {
            effort: DeepEffort::Medium,
            fast_mode: true,
        }));
        assert_eq!(medium.text, "deep²⚡");
        assert_eq!(medium.family, ModeFamily::Deep);

        let xhigh = agent_mode_label(AgentMode::Deep(DeepConfig {
            effort: DeepEffort::Xhigh,
            fast_mode: true,
        }));
        assert_eq!(xhigh.text, "deep³⚡");
        assert_eq!(xhigh.family, ModeFamily::Deep);

        let slow = agent_mode_label(AgentMode::Deep(DeepConfig {
            effort: DeepEffort::Medium,
            fast_mode: false,
        }));
        assert_eq!(slow.text, "deep²");
        assert_eq!(slow.family, ModeFamily::Deep);

        let fable = agent_mode_label(AgentMode::Fable {
            effort: FableEffort::Medium,
        });
        assert_eq!(fable.text, "fable");
        assert_eq!(fable.family, ModeFamily::Fable);

        let fable_xhigh = agent_mode_label(AgentMode::Fable {
            effort: FableEffort::Xhigh,
        });
        assert_eq!(fable_xhigh.text, "fable²");
        assert_eq!(fable_xhigh.family, ModeFamily::Fable);
    }
}
