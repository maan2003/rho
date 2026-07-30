//! Browser owner for the same dashboard, registry store, and transcript model
//! used by the native GPUI client. Transport remains direct iroh.

use std::collections::HashMap;

use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{Context, Entity, Render, Subscription, Task, Window, div, px};
use rho_registry::session::{
    AgentSubscriptions, INITIAL_AGENT_SUBSCRIPTIONS, recent_workstream_roots,
};
use rho_ui_proto::{AgentId, ClientMessage, ServerMessage};
use theme::ActiveTheme as _;

use crate::agent_view::AgentModel;
use crate::connection_web::{Connection, Event, Phase, daemon_id_from_page};
use crate::dashboard::{Dashboard, RowTarget};
use crate::registry::AgentRegistry;
use crate::store::{AgentStore, FrameSummary};

pub struct Workspace {
    connection: Connection,
    phase: Phase,
    dashboard: Dashboard,
    registry: AgentRegistry,
    subscriptions: AgentSubscriptions,
    store: AgentStore,
    models: HashMap<AgentId, Entity<AgentModel>>,
    pending_syncs: HashMap<AgentId, FrameSummary>,
    preview: Option<Entity<editor::Editor>>,
    /// On narrow (phone) viewports only one pane fits; this switches between
    /// the dashboard and the transcript preview.
    narrow_preview: bool,
    _event_task: Task<()>,
    _dashboard_subscription: Subscription,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (connection, mut events) = Connection::new();
        let phase = daemon_id_from_page().map_or_else(
            || Phase::Failed("Open this page with #daemon=<daemon-endpoint-id>".into()),
            Phase::Unlock,
        );
        let event_task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update_in(cx, |this, window, cx| this.handle_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let dashboard = Dashboard::new(window, cx);
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
        Self {
            connection,
            phase,
            dashboard,
            registry: AgentRegistry::default(),
            subscriptions: AgentSubscriptions::default(),
            store: AgentStore::default(),
            models: HashMap::new(),
            pending_syncs: HashMap::new(),
            preview: None,
            narrow_preview: false,
            _event_task: event_task,
            _dashboard_subscription: dashboard_subscription,
        }
    }

    pub fn registry_mut(&mut self) -> &mut AgentRegistry {
        &mut self.registry
    }
    pub fn dashboard(&self) -> &Dashboard {
        &self.dashboard
    }

    fn unlock(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Phase::Unlock(daemon) = self.phase.clone() {
            self.connection.connect(daemon);
            self.phase = Phase::Connecting;
            cx.notify();
        }
    }

    fn handle_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::Phase(phase) => {
                self.phase = phase;
                cx.notify();
            }
            Event::Message(message) => self.handle_message(message, window, cx),
        }
    }

    fn handle_message(
        &mut self,
        message: ServerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match message {
            ServerMessage::Ready {
                workstreams,
                agents,
                machine_seed,
                agent_counter,
                ..
            } => {
                let first_ready = self.phase != Phase::Online;
                let initial = first_ready.then(|| {
                    recent_workstream_roots(
                        &workstreams,
                        &agents,
                        self.registry.selected_agent().copied(),
                        INITIAL_AGENT_SUBSCRIPTIONS,
                    )
                });
                self.registry.set_machine_seed(machine_seed);
                self.registry.set_agent_counter(agent_counter);
                self.registry.set_data(workstreams, agents);
                if let Some(agent_ids) = initial.filter(|ids| !ids.is_empty()) {
                    self.subscriptions.reset(&agent_ids);
                    self.connection
                        .send(ClientMessage::SubscribeAgents { agent_ids });
                }
                self.phase = Phase::Online;
                cx.notify();
            }
            ServerMessage::Agent { agent_id, frame } => {
                if !self.subscriptions.accepts_frames(agent_id) {
                    return;
                }
                let summary = self.store.apply(agent_id, frame);
                self.registry.mark_live(agent_id);
                let (model, started) = self.ensure_agent_model(agent_id, window, cx);
                if started || !model.read(cx).initial_load_ready() {
                    if !started {
                        self.pending_syncs
                            .entry(agent_id)
                            .and_modify(|pending| *pending = pending.merge(summary))
                            .or_insert(summary);
                    }
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
                cx.notify();
            }
            ServerMessage::AgentSubscribed { agent_id } => {
                self.registry.mark_known(agent_id);
                cx.notify();
            }
            ServerMessage::AgentCreated {
                agent_id,
                workstream,
            } => {
                self.registry.note_agent_workstream(agent_id, workstream);
                self.registry.mark_known(agent_id);
                cx.notify();
            }
            ServerMessage::WorkstreamCreated { workstream } => {
                self.registry.add_workstream(workstream);
                cx.notify();
            }
            ServerMessage::AgentAttention {
                agent_id,
                attention,
            } => {
                self.registry.set_attention(agent_id, attention);
                cx.notify();
            }
            ServerMessage::AgentUnloaded { agent_id, reason } => {
                if self.subscriptions.mark_unloaded(agent_id, reason) {
                    self.registry.mark_not_live(agent_id);
                    let summary = self.store.mark_unloaded(agent_id);
                    if let (Some(model), Some(state)) =
                        (self.models.get(&agent_id), self.store.get(&agent_id))
                        && model.read(cx).initial_load_ready()
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
                    cx.notify();
                }
            }
            ServerMessage::Error { message } => {
                self.phase = Phase::Failed(message);
                cx.notify();
            }
            _ => {}
        }
    }

    fn dashboard_cursor_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.dashboard.cursor_target(cx);
        let agent_id = match target {
            Some(RowTarget::Stream { root: Some(id), .. } | RowTarget::Agent(id)) => id,
            _ => return,
        };
        self.open_agent(agent_id, window, cx);
    }

    pub fn open_agent(&mut self, agent_id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        if !self.registry.known_agents().any(|id| *id == agent_id) {
            return;
        }
        self.registry.select_agent(agent_id);
        let (subscribe, evicted) = self.subscriptions.touch(agent_id);
        if let Some(agent_id) = evicted {
            self.connection.send(ClientMessage::UnsubscribeAgents {
                agent_ids: vec![agent_id],
            });
        }
        if subscribe {
            self.connection.send(ClientMessage::SubscribeAgents {
                agent_ids: vec![agent_id],
            });
        }
        self.connection.send(ClientMessage::AgentStreamFocus {
            agent_id: Some(agent_id),
        });
        let (model, _) = self.ensure_agent_model(agent_id, window, cx);
        self.preview = Some(model.update(cx, |model, cx| model.preview_editor(window, cx)));
        self.narrow_preview = true;
        cx.notify();
    }

    fn ensure_agent_model(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<AgentModel>, bool) {
        let model = self
            .models
            .entry(agent_id)
            .or_insert_with(|| {
                let workspace = cx.entity().downgrade();
                cx.new(|cx| AgentModel::new(workspace, cx))
            })
            .clone();
        let mut started = false;
        if !model.read(cx).initial_load_started()
            && let Some(state) = self.store.get(&agent_id).cloned()
        {
            let labels = self
                .registry
                .known_agents()
                .copied()
                .map(|id| (id, self.registry.agent_display_label(id)))
                .collect();
            model.update(cx, |model, cx| {
                model.start_initial_load(agent_id, state, labels, now_ms(), cx)
            });
            started = true;
        }
        model.update(cx, |model, cx| {
            model.preview_editor(window, cx);
        });
        (model, started)
    }

    pub(crate) fn finish_initial_agent_load(&mut self, agent_id: AgentId, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    pub(crate) fn mark_draft_active_from_edit(&mut self, _cx: &mut Context<Self>) {}
    pub(crate) fn refresh_minibuffer(&mut self, _cx: &mut Context<Self>) {}
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.dashboard.sync(&self.registry, window, cx);
        let colors = cx.theme().colors();
        let (overlay_bg, card_bg, card_border, text, text_muted) = (
            colors.editor_background,
            colors.element_background,
            colors.border_variant,
            colors.text,
            colors.text_muted,
        );
        let narrow = window.viewport_size().width < px(700.);
        let body = if narrow {
            if self.narrow_preview && self.preview.is_some() {
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .px_1()
                            .py_1()
                            .border_b_1()
                            .border_color(card_border)
                            .child(
                                div()
                                    .id("back-to-agents")
                                    .cursor_pointer()
                                    .px_3()
                                    .py_1()
                                    .rounded_sm()
                                    .text_color(text_muted)
                                    .child("‹ agents")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.narrow_preview = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .children(self.preview.clone()),
                    )
            } else {
                div()
                    .size_full()
                    .overflow_hidden()
                    .child(self.dashboard.editor().clone())
            }
        } else {
            div()
                .flex()
                .size_full()
                .gap_2()
                .child(
                    div()
                        .w(px(430.))
                        .h_full()
                        .overflow_hidden()
                        .child(self.dashboard.editor().clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .overflow_hidden()
                        .children(self.preview.clone()),
                )
        };
        let card_style = move |content: gpui::Div| {
            content
                .flex()
                .flex_col()
                .gap_2()
                .max_w(px(420.))
                .m_4()
                .p_4()
                .rounded_md()
                .border_1()
                .border_color(card_border)
                .bg(card_bg)
                .text_color(text)
                .text_sm()
        };
        let card = move |content: gpui::Div| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(overlay_bg)
                .child(card_style(content))
                .into_any_element()
        };
        let muted = move |line: String| div().text_color(text_muted).child(line);
        let overlay = match &self.phase {
            Phase::Online => None,
            Phase::Unlock(daemon) => Some(
                div()
                    .id("unlock")
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(overlay_bg)
                    .cursor_pointer()
                    .on_click(cx.listener(Self::unlock))
                    .child(card_style(
                        div()
                            .child("Connect to rho daemon")
                            .child(muted(format!("daemon {}", shorten_id(daemon))))
                            .child(muted("Tap to unlock with your browser identity".into())),
                    ))
                    .into_any_element(),
            ),
            Phase::Connecting => Some(card(div().child("Connecting to rho daemon…"))),
            Phase::Enroll(code) => {
                Some(card(div().child("This browser is not enrolled yet").child(
                    muted(format!(
                        "Run `rho iroh approve {code}`, then reload this page"
                    )),
                )))
            }
            Phase::Failed(error) => Some(card(
                div().child("Connection failed").child(muted(error.clone())),
            )),
        };
        div()
            .id("rho-gui")
            .relative()
            .size_full()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .key_context("RhoGui")
            .child(body)
            .children(overlay)
    }
}

pub fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// Endpoint ids are 64 hex chars; unbroken they defeat text wrapping, so the
/// overlays show a recognizable abbreviation instead.
fn shorten_id(id: &str) -> String {
    if id.len() <= 12 {
        id.to_owned()
    } else {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    }
}
