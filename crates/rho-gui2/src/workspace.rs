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
use gpui::{Context, Entity, Focusable as _, MouseButton, Task, Window, div, px};
use rho_core::ContentPart;
use rho_ui_proto::{AgentId, ClientMessage, UiTopic};
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, IconSize};

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
                let summary = self.store.apply(agent_id.clone(), frame);
                self.registry.mark_live(agent_id.clone());
                if self.registry.selected().is_none() {
                    // First live agent: show it. Materialization renders the
                    // full state, so the per-frame sync below is unnecessary.
                    self.select_agent(Some(agent_id.clone()), window, cx);
                } else if let Some(view) = self.views.get(&agent_id).cloned() {
                    if let Some(state) = self.store.get(&agent_id) {
                        view.update(cx, |view, cx| view.sync(state, summary, now_ms(), cx));
                    }
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
                let views = self
                    .views
                    .values()
                    .cloned()
                    .chain([self.draft_view.clone()])
                    .collect::<Vec<_>>();
                for view in views {
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
                    .map(|topic| topic.topic_id.clone())
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
            ParsedCommand::Load(Ok(agent_id)) => {
                if !self.connected {
                    self.notice_on(
                        source_agent.as_ref(),
                        "/load is only available when connected to rho-daemon",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
                self.connection.send(ClientMessage::LoadAgent {
                    agent_id: agent_id.clone(),
                });
                self.registry.mark_known(agent_id.clone());
                self.select_agent(Some(agent_id), window, cx);
            }
            ParsedCommand::Load(Err(message)) => {
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
        if let Some(view) = self.views.get(agent_id) {
            return view.clone();
        }
        let workspace = cx.entity().downgrade();
        let id = Some(agent_id.clone());
        let view = cx.new(|cx| AgentView::new(id, workspace, window, cx));
        if let Some(state) = self.store.get(agent_id) {
            view.update(cx, |view, cx| {
                view.sync(state, FrameSummary::everything(), now_ms(), cx);
            });
        }
        let role = self.connected.then_some("rho");
        let project_label = self.project_label();
        view.update(cx, |view, cx| view.set_status(role, &project_label, cx));
        self.views.insert(agent_id.clone(), view.clone());
        view
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
        let views = self
            .views
            .values()
            .cloned()
            .chain([self.draft_view.clone()])
            .collect::<Vec<_>>();
        for view in views {
            view.update(cx, |view, cx| view.set_status(role, &project_label, cx));
        }
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

    fn render_topic_rail(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let active_view = self.active_view();
        let text_style = active_view.update(cx, |view, cx| {
            view.editor().update(cx, |editor, cx| editor.style(cx).text.clone())
        });
        let (selected_color, border_color) = {
            let colors = cx.theme().colors();
            (
                colors.terminal_ansi_magenta,
                colors.border_variant.opacity(0.6),
            )
        };
        let selected_agent = self.registry.selected().cloned();
        let live = self
            .registry
            .live_agents()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();

        let rows = self
            .registry
            .topics()
            .iter()
            .map(|topic| {
                render_topic_rows(
                    topic,
                    selected_agent.as_ref(),
                    &live,
                    &text_style,
                    selected_color,
                    cx,
                )
            })
            .collect::<Vec<_>>();

        div()
            .id("rho-gui2-topic-rail")
            .h_full()
            .w(px(224.))
            .flex_none()
            .border_r_1()
            .border_color(border_color)
            .pr(px(6.))
            .py(px(2.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .font_family(text_style.font_family.clone())
            .text_size(text_style.font_size)
            .line_height(text_style.line_height)
            .text_color(text_style.color)
            .child(
                div()
                    .id("rho-gui2-topic-list")
                    .w_full()
                    .flex_grow(1.0)
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

fn render_topic_rows(
    topic: &UiTopic,
    selected_agent: Option<&AgentId>,
    live: &std::collections::BTreeSet<AgentId>,
    text_style: &gpui::TextStyle,
    selected_color: gpui::Hsla,
    cx: &mut Context<Workspace>,
) -> gpui::Div {
    let name = topic
        .display_name
        .clone()
        .unwrap_or_else(|| topic.topic_id.to_string());
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(
            div()
                .w_full()
                .pt(px(5.))
                .pl(px(4.))
                .text_color(text_style.color.opacity(0.65))
                .child(name),
        )
        .children(topic.agent_ids.iter().map(|agent_id| {
            let selected = selected_agent == Some(agent_id);
            let is_live = live.contains(agent_id);
            let text_color = if selected {
                selected_color
            } else {
                text_style.color
            };
            let icon_color = if selected {
                selected_color
            } else if is_live {
                text_style.color.opacity(0.9)
            } else {
                text_style.color.opacity(0.5)
            };
            div()
                .relative()
                .w_full()
                .flex()
                .items_center()
                .gap_1()
                .pl(px(12.))
                .overflow_hidden()
                .whitespace_nowrap()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener({
                        let agent_id = agent_id.clone();
                        move |this, _, window, cx| {
                            this.select_agent(Some(agent_id.clone()), window, cx);
                        }
                    }),
                )
                .child(
                    Icon::new(if selected {
                        IconName::PlayFilled
                    } else {
                        IconName::Circle
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Custom(icon_color)),
                )
                .child(
                    div()
                        .flex_grow(1.0)
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(text_color)
                        .child(agent_id.to_string()),
                )
        }))
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.active_view().read(cx).editor().clone();
        let rail = self.render_topic_rail(cx);
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
