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
use crate::commands::{self, ParsedCommand};
use crate::connection::{ConnEvent, Connection};
use crate::registry::AgentRegistry;
use crate::store::{AgentStore, FrameSummary};
use crate::style::StyleClass;

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
    draft_view: Entity<AgentView>,
    project_root: PathBuf,
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
        let (connection, events) = crate::connection::spawn(attach_target.socket_path.clone());
        let workspace = cx.entity().downgrade();
        let draft_view = cx.new(|cx| AgentView::new(None, workspace, window, cx));
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

        let this = Self {
            connection,
            store: AgentStore::default(),
            registry: AgentRegistry::default(),
            views: HashMap::new(),
            pending_syncs: HashMap::new(),
            draft_view,
            project_root: attach_target.project_root,
            connected: false,
            duration_timer: None,
            _event_task: event_task,
        };
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
            ConnEvent::Ready { topics } => {
                self.registry.set_topics(topics);
                self.connected = true;
                self.update_statuses(cx);
                cx.notify();
            }
            ConnEvent::TopicCreated(topic) => {
                self.registry.add_topic(topic);
                cx.notify();
            }
            ConnEvent::AgentAnnounced(agent_id) => {
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
                self.active_view().update(cx, |view, cx| {
                    view.system_notice(
                        &format!("[rho daemon error: {message}]"),
                        StyleClass::SystemImportant,
                        cx,
                    );
                });
            }
            ConnEvent::Disconnected(reason) => {
                self.connected = false;
                for view in self.all_views() {
                    view.update(cx, |view, cx| {
                        view.system_notice(
                            &format!("[disconnected from rho daemon: {reason}]"),
                            StyleClass::Disconnect,
                            cx,
                        );
                    });
                }
                self.update_statuses(cx);
                cx.notify();
            }
        }
    }

    pub fn handle_submit(
        &mut self,
        source_agent: Option<AgentId>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = commands::parse(&text) {
            self.handle_command(source_agent, command, window, cx);
            return;
        }
        if !self.connected {
            self.notice_on(
                source_agent.as_ref(),
                "not connected to rho-daemon",
                StyleClass::SystemImportant,
                cx,
            );
            return;
        }
        match source_agent {
            Some(agent_id) => {
                self.connection.send(ClientMessage::SendUserMessage {
                    agent_id,
                    content: vec![ContentPart::Text { text }],
                });
            }
            None => {
                let Some(topic_id) = self
                    .registry
                    .topics()
                    .first()
                    .map(|topic| topic.topic_id)
                else {
                    self.notice_on(
                        None,
                        "no rho topic is available",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                };
                self.connection.send(ClientMessage::NewAgent {
                    topic_id,
                    content: Some(vec![ContentPart::Text { text }]),
                });
            }
        }
    }

    fn handle_command(
        &mut self,
        source_agent: Option<AgentId>,
        command: ParsedCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            ParsedCommand::New => self.select_agent(None, window, cx),
            ParsedCommand::Cancel => {
                let target = source_agent.or_else(|| self.registry.selected().cloned());
                match (target, self.connected) {
                    (Some(agent_id), true) => {
                        self.connection.send(ClientMessage::CancelTurn { agent_id });
                    }
                    (_, false) => self.notice_on(
                        None,
                        "/cancel is only available when connected to rho-daemon",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                    (None, _) => self.notice_on(
                        None,
                        "/cancel: no agent selected",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }
            ParsedCommand::Load(agent_id) => {
                if !self.connected {
                    self.notice_on(
                        source_agent.as_ref(),
                        "/load is only available when connected to rho-daemon",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
                self.connection.send(ClientMessage::LoadAgent { agent_id });
                self.registry.mark_known(agent_id);
                self.select_agent(Some(agent_id), window, cx);
            }
            ParsedCommand::Invalid(message) => {
                self.notice_on(source_agent.as_ref(), &message, StyleClass::SystemInfo, cx);
            }
            ParsedCommand::Unsupported => {
                self.notice_on(
                    source_agent.as_ref(),
                    "command is not available in rho-gui2 yet",
                    StyleClass::SystemInfo,
                    cx,
                );
            }
        }
    }

    fn notice_on(
        &self,
        agent_id: Option<&AgentId>,
        text: &str,
        class: StyleClass,
        cx: &mut Context<Self>,
    ) {
        let view = agent_id
            .and_then(|agent_id| self.views.get(agent_id))
            .cloned()
            .unwrap_or_else(|| self.active_view());
        view.update(cx, |view, cx| view.system_notice(text, class, cx));
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
        let id = Some(*agent_id);
        let view = cx.new(|cx| AgentView::new(id, workspace, window, cx));
        if let Some(state) = self.store.get(agent_id) {
            view.update(cx, |view, cx| {
                view.sync(state, FrameSummary::everything(), now_ms(), cx);
            });
        }
        let role = self.connected.then_some("rho");
        let project_label = self.project_label();
        view.update(cx, |view, cx| view.set_status(role, &project_label, cx));
        self.views.insert(*agent_id, view.clone());
        view
    }

    #[cfg(test)]
    pub(crate) fn agent_view(&self, agent_id: &AgentId) -> Option<Entity<AgentView>> {
        self.views.get(agent_id).cloned()
    }

    pub(crate) fn active_view(&self) -> Entity<AgentView> {
        self.registry
            .selected()
            .and_then(|agent_id| self.views.get(agent_id))
            .cloned()
            .unwrap_or_else(|| self.draft_view.clone())
    }

    fn focus_active_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.active_view().read(cx).editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
    }

    fn update_statuses(&self, cx: &mut Context<Self>) {
        let role = self.connected.then_some("rho");
        let project_label = self.project_label();
        for view in self.all_views() {
            view.update(cx, |view, cx| view.set_status(role, &project_label, cx));
        }
    }

    /// Every materialized view plus the draft view — the recipients of
    /// broadcasts like status updates and disconnect notices.
    fn all_views(&self) -> Vec<Entity<AgentView>> {
        self.views
            .values()
            .cloned()
            .chain([self.draft_view.clone()])
            .collect()
    }

    fn project_label(&self) -> String {
        self.project_root
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| self.project_root.display().to_string())
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
        if !self.active_view().read(cx).has_timers() {
            return;
        }
        self.duration_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(1))
                    .await;
                let keep_going = this.update(cx, |this, cx| {
                    let view = this.active_view();
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
        let editor = self.active_view().read(cx).editor().clone();
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
