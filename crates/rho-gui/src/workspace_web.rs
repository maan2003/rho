//! Browser owner for the same dashboard, registry store, and transcript model
//! used by the native GPUI client. Transport remains direct iroh.

use std::collections::HashMap;

use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, Focusable as _, Pixels, Render, Subscription, Task, Window, div, px,
};
use rho_registry::session::{
    AgentSubscriptions, INITIAL_AGENT_SUBSCRIPTIONS, recent_workstream_roots,
};
use rho_touch_keyboard::{ContextChip, KeyboardPlugin, KeyboardStyle, TouchKeyboard};
use rho_ui_proto::{AgentId, ClientMessage, MessageDelivery, ServerMessage};
use theme::ActiveTheme as _;

use crate::agent_view::AgentModel;
use crate::connection_web::{Connection, Event, Phase, daemon_id_from_page};
use crate::dashboard::{Dashboard, RowTarget};
use crate::registry::AgentRegistry;
use crate::store::{AgentStore, FrameSummary};

struct RhoKeyboardPlugin;

impl KeyboardPlugin for RhoKeyboardPlugin {
    fn context_chips(&self) -> Vec<ContextChip> {
        Vec::new()
    }

    fn style(&self, cx: &App) -> KeyboardStyle {
        let colors = cx.theme().colors();
        KeyboardStyle {
            background: colors.editor_background.into(),
            key_background: colors.element_background.into(),
            key_pressed: colors.element_selected.into(),
            border: colors.border.into(),
            key_border: colors.border_variant.into(),
            text: colors.text.into(),
            text_muted: colors.text_muted.into(),
            text_disabled: colors.text_disabled.into(),
        }
    }
}

fn coarse_pointer() -> bool {
    web_sys::window()
        .and_then(|window| window.match_media("(pointer: coarse)").ok().flatten())
        .is_some_and(|query| query.matches())
}

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
    /// The full transcript editor (prompt included) for the phone layout,
    /// kept per selected agent so reopening does not rebuild it.
    transcript: Option<(AgentId, Entity<editor::Editor>)>,
    /// On narrow (phone) viewports only one pane fits; this switches between
    /// the dashboard and the full agent transcript.
    agent_screen: bool,
    touch_keyboard: Entity<TouchKeyboard>,
    /// The keyboard appears only on explicit intent to type: a tap that
    /// lands in the transcript's editable prompt, or the action bar's
    /// keyboard toggle. It hides on a tap in the read-only transcript, the
    /// toggle again, and after sending a message.
    keyboard_visible: bool,
    _event_task: Task<()>,
    _dashboard_subscription: Subscription,
    _transcript_subscription: Option<Subscription>,
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
        let touch_keyboard = cx.new(|_| TouchKeyboard::new(RhoKeyboardPlugin));
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
            transcript: None,
            agent_screen: false,
            touch_keyboard,
            keyboard_visible: false,
            _event_task: event_task,
            _dashboard_subscription: dashboard_subscription,
            _transcript_subscription: None,
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
            ServerMessage::AgentActivity { agent_id, activity } => {
                self.registry.set_activity(agent_id, activity);
                self.dashboard_dirty = true;
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
        if window.viewport_size().width < px(700.) {
            let editor = match &self.transcript {
                Some((id, editor)) if *id == agent_id => editor.clone(),
                _ => {
                    let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
                    // Chat editors disable click selection for the desktop's
                    // keyboard-first navigation; on a phone the tap is the
                    // only cursor, and it also drives keyboard visibility.
                    editor.update(cx, |editor, cx| {
                        editor.set_mouse_click_selection_enabled(true, cx)
                    });
                    self.transcript = Some((agent_id, editor.clone()));
                    // A pointer tap decides keyboard visibility by where it
                    // lands: the editable prompt shows it, the read-only
                    // transcript hides it. Keystroke-driven selection moves
                    // (typing, submit clearing the prompt) leave it alone.
                    self._transcript_subscription = Some(cx.subscribe_in(
                        &editor,
                        window,
                        move |this, editor, event: &editor::EditorEvent, window, cx| {
                            if matches!(
                                event,
                                editor::EditorEvent::SelectionsChanged { local: true }
                            ) && !window.last_input_was_keyboard()
                            {
                                let in_prompt = this.models.get(&agent_id).is_some_and(|model| {
                                    model.read(cx).selection_in_prompt(editor, cx)
                                });
                                if this.keyboard_visible != in_prompt {
                                    this.keyboard_visible = in_prompt;
                                    cx.notify();
                                }
                            }
                        },
                    ));
                    editor
                }
            };
            window.focus(&editor.read(cx).focus_handle(cx), cx);
        } else {
            self.preview = Some(model.update(cx, |model, cx| model.preview_editor(window, cx)));
        }
        self.agent_screen = true;
        cx.notify();
    }

    fn submit_prompt(
        &mut self,
        _: &crate::SubmitPrompt,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Online {
            return;
        }
        let Some(agent_id) = self.registry.selected_agent().copied() else {
            return;
        };
        let Some(model) = self.models.get(&agent_id).cloned() else {
            return;
        };
        let Some(content) = model.update(cx, |model, cx| model.take_prompt(cx)) else {
            return;
        };
        self.connection.send(ClientMessage::SendUserMessage {
            agent_id,
            content,
            delivery: MessageDelivery::NextRequest,
        });
        self.registry.touch_agent(agent_id);
        self.keyboard_visible = false;
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
        // The phone gets a persistent action bar along the very bottom: it
        // hosts quick actions and doubles as clearance for the iPhone's
        // rounded corners and home indicator in PWA mode.
        let phone = coarse_pointer() && narrow;
        let (inset_top, inset_right, inset_bottom, inset_left) = safe_area();
        // The bar's button row stays 40px; the bottom safe-area inset extends
        // it so the buttons clear the iPhone's home indicator and rounded
        // corners while the bar background fills the screen edge.
        let bar_height = px(40.) + inset_bottom;
        let show_keyboard = phone && self.agent_screen && self.keyboard_visible;
        window.set_direct_touch_region(
            show_keyboard.then(|| TouchKeyboard::region(window.viewport_size(), bar_height)),
        );
        set_haptic_region(if show_keyboard {
            f32::from(window.viewport_size().height - bar_height - TouchKeyboard::height())
        } else {
            -1.
        });
        // Percent heights do not survive the flex_1 wrapper here (the agent
        // screen collapsed to its header), so the phone transcript gets
        // explicit pixel heights derived from the viewport.
        let header_height = px(34.);
        let content_height = window.viewport_size().height
            - px(4.)
            - inset_top
            - if phone { bar_height } else { px(0.) }
            - if show_keyboard {
                TouchKeyboard::height()
            } else {
                px(0.)
            };
        let body = if narrow {
            if self.agent_screen
                && let Some((agent_id, transcript)) = self.transcript.clone()
            {
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .h(content_height)
                    .key_context("RhoTranscript")
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .h(header_height)
                            .px_1()
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
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.agent_screen = false;
                                        this.keyboard_visible = false;
                                        window.focus(
                                            &this.dashboard.editor().read(cx).focus_handle(cx),
                                            cx,
                                        );
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_color(text_muted)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(self.registry.agent_display_label(agent_id)),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(content_height - header_height)
                            .overflow_hidden()
                            .child(transcript),
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
            .flex()
            .flex_col()
            .p(px(2.))
            .pt(px(2.) + inset_top)
            .pl(px(2.) + inset_left)
            .pr(px(2.) + inset_right)
            .bg(cx.theme().colors().editor_background)
            .key_context("RhoGui")
            .on_action(cx.listener(Self::submit_prompt))
            .child(div().flex_1().overflow_hidden().child(body))
            .children(show_keyboard.then(|| self.touch_keyboard.clone().into_any_element()))
            .children(phone.then(|| {
                let mut bar = div()
                    .flex_none()
                    .h(bar_height)
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_1()
                    .border_t_1()
                    .border_color(card_border);
                if self.agent_screen {
                    bar = bar
                        .child(
                            div()
                                .id("kbd-toggle")
                                .cursor_pointer()
                                .h_full()
                                .flex()
                                .items_center()
                                .px_4()
                                .text_color(text_muted)
                                .child(
                                    gpui::svg()
                                        .path(if self.keyboard_visible {
                                            "icons/chevron_down.svg"
                                        } else {
                                            "icons/keyboard.svg"
                                        })
                                        .size(px(20.))
                                        .text_color(text_muted),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.keyboard_visible = !this.keyboard_visible;
                                    if this.keyboard_visible
                                        && let Some((_, editor)) = this.transcript.clone()
                                    {
                                        window.focus(&editor.read(cx).focus_handle(cx), cx);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("send-prompt")
                                .cursor_pointer()
                                .h_full()
                                .flex()
                                .items_center()
                                .px_4()
                                .text_color(text_muted)
                                .child("send")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_prompt(&crate::SubmitPrompt, window, cx);
                                })),
                        );
                }
                bar
            }))
            .children(overlay)
    }
}

pub fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// Safe-area insets (top, right, bottom, left) of the display cutouts —
/// status bar, rounded corners, home indicator — read from index.html's CSS
/// env() probe. The canvas spans the full screen; the app pads around these.
fn safe_area() -> (Pixels, Pixels, Pixels, Pixels) {
    use wasm_bindgen::JsCast as _;
    let zero = (px(0.), px(0.), px(0.), px(0.));
    let Some(window) = web_sys::window() else {
        return zero;
    };
    let window: &wasm_bindgen::JsValue = window.as_ref();
    let Ok(hook) = js_sys::Reflect::get(window, &"__rhoSafeArea".into()) else {
        return zero;
    };
    let Some(hook) = hook.dyn_ref::<js_sys::Function>() else {
        return zero;
    };
    let Ok(values) = hook.call0(&wasm_bindgen::JsValue::NULL) else {
        return zero;
    };
    let values = js_sys::Array::from(&values);
    let inset = |index| px(values.get(index).as_f64().unwrap_or(0.) as f32);
    (inset(0), inset(1), inset(2), inset(3))
}

/// Moves the index.html haptic switch overlay over the keyboard (negative
/// hides it). iOS 26.5 blocks scripted switch clicks, so key haptics come
/// from the finger's genuine tap toggling an invisible switch there; the
/// overlay forwards cloned pointer events to the canvas.
fn set_haptic_region(top: f32) {
    use wasm_bindgen::JsCast as _;
    let Some(window) = web_sys::window() else {
        return;
    };
    let window: &wasm_bindgen::JsValue = window.as_ref();
    if let Ok(hook) = js_sys::Reflect::get(window, &"__rhoHapticRegion".into())
        && let Some(hook) = hook.dyn_ref::<js_sys::Function>()
    {
        let _ = hook.call1(&wasm_bindgen::JsValue::NULL, &top.into());
    }
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
