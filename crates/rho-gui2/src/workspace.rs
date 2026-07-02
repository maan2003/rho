//! Root entity: owns the daemon connection, the canonical agent states, the
//! registry, and one persistent [`AgentView`] per opened agent.
//!
//! All protocol events flow through [`Workspace::handle_event`]; views receive
//! already-summarized state changes and never see the protocol.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::prelude::*;
use gpui::{Context, Entity, Focusable as _, Task, Window, div, px};
use rho_core::ContentPart;
use rho_ui_proto::{AgentId, ClientMessage};
use theme::ActiveTheme as _;

use crate::agent_view::AgentView;
use crate::connection::{ConnEvent, Connection};
use crate::draft_view::DraftView;
use crate::registry::AgentRegistry;
use crate::store::{AgentStore, FrameSummary};
use crate::style::StyleClass;
use crate::{
    AgentNew, AgentNext, AgentPrevious, RoleCycle, RoleCycleGroup, SubmitPrompt, TaskBoard,
};

#[derive(Clone)]
pub struct AttachTarget {
    pub socket_path: PathBuf,
    pub project_root: PathBuf,
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
    project_root: PathBuf,
    /// Registered workdirs from the daemon; selection vocabulary for new
    /// agents.
    workdirs: Vec<rho_ui_proto::UiWorkdir>,
    /// Topic the draft inherits: whichever topic was focused when the draft
    /// was entered. Topics are ad-hoc tab groups — a new agent lands in the
    /// default topic unless created from inside one.
    draft_topic_id: Option<rho_ui_proto::TopicId>,
    /// A NewAgent request from the draft is in flight; the draft buffer is
    /// kept intact until the daemon confirms creation, so a rejected request
    /// (bad working directory, say) never loses the message.
    awaiting_draft_agent: bool,
    connected: bool,
    duration_timer: Option<Task<()>>,
    _event_task: Task<()>,
}

impl Workspace {
    pub fn new(
        attach_target: AttachTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (connection, events) =
            crate::connection::spawn(attach_target.socket_path.clone(), cx);
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
            project_root: attach_target.project_root,
            workdirs: Vec::new(),
            draft_topic_id: None,
            awaiting_draft_agent: false,
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
            ConnEvent::Ready { topics, workdirs } => {
                let first_ready = !self.connected;
                self.registry.set_topics(topics);
                self.workdirs = workdirs;
                self.connected = true;
                if first_ready && self.registry.selected().is_none() {
                    // The startup scaffold guessed before daemon data existed;
                    // refresh it now that workdir names and topics are known.
                    self.seed_draft(false, window, cx);
                }
                self.update_statuses(cx);
                cx.notify();
            }
            ConnEvent::TopicCreated(topic) => {
                self.registry.add_topic(topic);
                cx.notify();
            }
            ConnEvent::AgentCreated(agent_id) => {
                self.registry.mark_known(agent_id);
                if self.awaiting_draft_agent {
                    self.awaiting_draft_agent = false;
                    // The draft became this agent: reset the compose surface
                    // and follow the new agent.
                    let label = self.workdir_label(&self.draft_default_workdir());
                    self.draft_view.update(cx, |view, cx| {
                        view.set_body_text("", cx);
                        view.set_workdir_text(&label, cx);
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
                let summary = self.store.apply(agent_id, frame);
                self.registry.mark_live(agent_id);
                if self.registry.selected().is_none() {
                    // First live agent: show it. Materialization renders the
                    // full state, so the per-frame sync below is unnecessary.
                    self.select_agent(Some(agent_id), window, cx);
                } else if self.registry.selected() == Some(&agent_id) {
                    if let Some(view) = self.views.get(&agent_id).cloned()
                        && let Some(state) = self.store.get(&agent_id) {
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
        match self.registry.selected().copied() {
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
            self.notice_on(None, "not connected to rho-daemon", StyleClass::SystemImportant, cx);
            return;
        }
        let Some(topic_id) = self
            .draft_topic_id
            .or_else(|| self.registry.topics().first().map(|topic| topic.topic_id))
        else {
            self.notice_on(None, "no rho topic is available", StyleClass::SystemInfo, cx);
            return;
        };
        let field = self.draft_view.read(cx).workdir_text(cx).trim().to_owned();
        let working_directory = (!field.is_empty())
            .then(|| self.resolve_workdir_path(PathBuf::from(field)))
            .or_else(|| self.registry.last_working_directory(topic_id))
            .unwrap_or_else(|| self.project_root.clone());
        self.awaiting_draft_agent = true;
        self.connection.send(ClientMessage::NewAgent {
            topic_id,
            working_directory,
            content: Some(vec![ContentPart::Text { text: body }]),
        });
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
        if !self.connected
            && !matches!(command, Command::Quit | Command::Help | Command::Version)
        {
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
                let target = source_agent.or_else(|| self.registry.selected().cloned());
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
            Command::AgentLoad { agent_id } => {
                self.registry.mark_known(agent_id);
                self.open_agent(agent_id, window, cx);
            }
            Command::TopicNew { name } => {
                self.connection.send(ClientMessage::NewTopic {
                    display_name: name,
                });
            }
            Command::TopicMove { name } => {
                let target = source_agent.or_else(|| self.registry.selected().copied());
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
                let path = path.map_or_else(
                    || self.project_root.clone(),
                    |path| self.resolve_workdir_path(path),
                );
                self.connection.send(ClientMessage::WorkdirSet { path, name });
            }
            Command::WorkdirRemove { path } => {
                match rho_commands::resolve_workdir(&path, &self.workdir_table()) {
                    Some(path) => {
                        self.connection
                            .send(ClientMessage::WorkdirRemove { path: path.into() });
                    }
                    None => {
                        let message = format!("no registered workdir `{path}`");
                        self.notice_on(
                            source_agent.as_ref(),
                            &message,
                            StyleClass::SystemInfo,
                            cx,
                        );
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
                    ":clear is not available in rho-gui2",
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
        working_directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(selected) = self.registry.selected().copied()
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

    /// Selects an agent, asking the daemon to load it first when this
    /// connection has never seen frames for it.
    pub fn open_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        if self.connected && !self.registry.is_live(agent_id) {
            self.connection.send(ClientMessage::LoadAgent { agent_id });
        }
        self.select_agent(Some(agent_id), window, cx);
    }

    /// Tab in the draft jumps between the `Workdir:` header and the body;
    /// on agent views it does nothing (yet).
    fn cycle_draft_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.registry.selected().is_none() {
            self.draft_view
                .update(cx, |view, cx| view.toggle_field(window, cx));
        }
    }

    /// (Re)writes the draft scaffold with the derived default workdir.
    fn seed_draft(&mut self, force_header: bool, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.workdir_label(&self.draft_default_workdir());
        self.draft_view
            .update(cx, |view, cx| view.seed(&label, force_header, window, cx));
    }

    /// Where a new agent works when the draft doesn't say: the inherited
    /// topic's newest agent sets the precedent, else where the GUI was
    /// launched.
    fn draft_default_workdir(&self) -> PathBuf {
        self.draft_topic_id
            .or_else(|| self.registry.topics().first().map(|topic| topic.topic_id))
            .and_then(|topic_id| self.registry.last_working_directory(topic_id))
            .unwrap_or_else(|| self.project_root.clone())
    }

    /// How a path reads in the draft header: its registered workdir name
    /// when it has one, else the full path.
    fn workdir_label(&self, path: &std::path::Path) -> String {
        self.workdirs
            .iter()
            .find(|workdir| workdir.path == path)
            .map(|workdir| workdir.name.clone())
            .unwrap_or_else(|| path.display().to_string())
    }

    /// Topics as the `(label, id)` pairs shared resolution expects: display
    /// name, or the id string for unnamed topics.
    fn topic_labels(&self) -> Vec<(String, rho_ui_proto::TopicId)> {
        self.registry
            .topics()
            .iter()
            .map(|topic| {
                (
                    topic
                        .display_name
                        .clone()
                        .unwrap_or_else(|| topic.topic_id.to_string()),
                    topic.topic_id,
                )
            })
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
            .map(|workdir| (workdir.name.clone(), workdir.path.display().to_string()))
            .collect()
    }

    /// A workdir argument may be a registered name, `~`-prefixed, or
    /// relative to where the GUI was launched.
    fn resolve_workdir_path(&self, path: PathBuf) -> PathBuf {
        let argument = path.display().to_string();
        if let Some(resolved) = rho_commands::resolve_workdir(&argument, &self.workdir_table()) {
            return resolved.into();
        }
        if let Some(rest) = argument.strip_prefix("~/")
            && let Some(home) = std::env::home_dir()
        {
            return home.join(rest);
        }
        self.project_root.join(path)
    }

    fn notice_on(
        &self,
        agent_id: Option<&AgentId>,
        text: &str,
        class: StyleClass,
        cx: &mut Context<Self>,
    ) {
        let view = agent_id
            .or_else(|| self.registry.selected())
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
        self.registry.select(agent_id);
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
        if self.registry.selected() == Some(&agent_id) {
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
        let role = self.connected.then_some("rho");
        let label = self.working_directory_label(agent_id);
        view.update(cx, |view, cx| view.set_status(role, &label, cx));
        self.views.insert(*agent_id, view.clone());
        view
    }

    #[cfg(test)]
    pub(crate) fn agent_view(&self, agent_id: &AgentId) -> Option<Entity<AgentView>> {
        self.views.get(agent_id).cloned()
    }

    pub(crate) fn active_agent_view(&self) -> Option<Entity<AgentView>> {
        self.registry
            .selected()
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
        let role = self.connected.then_some("rho");
        for (agent_id, view) in &self.views {
            let label = self.working_directory_label(agent_id);
            view.update(cx, |view, cx| view.set_status(role, &label, cx));
        }
    }

    /// Chip label: the agent's own working directory.
    fn working_directory_label(&self, agent_id: &AgentId) -> String {
        let directory = self
            .registry
            .working_directory(*agent_id)
            .cloned()
            .unwrap_or_else(|| self.project_root.clone());
        directory
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| directory.display().to_string())
    }

    pub fn known_agent_names(&self) -> Vec<String> {
        self.registry
            .known_agents()
            .map(|agent_id| agent_id.to_string())
            .collect()
    }

    pub fn live_agent_names(&self) -> Vec<String> {
        self.registry
            .live_agents()
            .map(|agent_id| agent_id.to_string())
            .collect()
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
                cx.background_executor()
                    .timer(Duration::from_secs(1))
                    .await;
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

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.active_editor(cx);
        let text_style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
        let rail = crate::topic_rail::render_topic_rail(&self.registry, &text_style, cx);
        div()
            .id("rho-gui2")
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
                this.cycle_draft_field(window, cx);
            }))
            .child(rail)
            .child(
                div()
                    .id("rho-gui2-editor")
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
