//! Browser layout and gesture adapter for the canonical workspace.

use std::collections::HashMap;

use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{App, Context, Focusable as _, Pixels, Render, Window, div, px};
use rho_touch_keyboard::{ContextChip, KeyboardPlugin, KeyboardStyle, TouchKeyboard};
use theme::ActiveTheme as _;

use super::{ContextId, SurfaceKey, Workspace};
use crate::connection::daemon_targets_from_page;
use crate::hosts::Hosts;
use crate::registry::{AgentRegistry, HostId};
use crate::store::AgentStore;

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

#[derive(Clone)]
enum Authorization {
    Required,
    Connecting,
    Enrollment(String),
}

pub(super) struct WebUi {
    authorizations: HashMap<HostId, Authorization>,
    target_error: Option<String>,
    touch_keyboard: gpui::Entity<TouchKeyboard>,
    keyboard_visible: bool,
}

impl WebUi {
    pub(super) fn authorization_required(&mut self, host: HostId) {
        self.authorizations.insert(host, Authorization::Required);
    }

    pub(super) fn enrollment_required(&mut self, host: HostId, code: String) {
        self.authorizations
            .insert(host, Authorization::Enrollment(code));
    }

    pub(super) fn online(&mut self, host: HostId) {
        self.authorizations.remove(&host);
    }
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (mut hosts, events) = Hosts::new();
        let mut registry = AgentRegistry::default();
        let mut authorizations = HashMap::new();
        let target_error = match daemon_targets_from_page() {
            Ok(targets) if targets.is_empty() => {
                Some("Open this page with #daemon=<daemon-endpoint-id>".to_owned())
            }
            Ok(targets) => {
                for (name, target) in targets {
                    let host = hosts.attach(name.clone(), target, cx);
                    registry.attach_host(host, name);
                    authorizations.insert(host, Authorization::Required);
                }
                None
            }
            Err(error) => Some(format!("{error:#}")),
        };

        let workspace = cx.entity().downgrade();
        let draft_model = cx.new(|cx| crate::draft_view::DraftModel::new(workspace, cx));
        let event_task = cx.spawn(async move |this, cx| {
            let mut events = events;
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                if this
                    .update_in(cx, |this, window, cx| this.handle_events(batch, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let dashboard = crate::dashboard::Dashboard::new(window, cx);
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
        let iris_buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local("iris\n\nlistening", cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let iris_multi_buffer = cx.new(|cx| multi_buffer::MultiBuffer::singleton(iris_buffer, cx));
        let iris_preview = cx.new(|cx| {
            let mut editor = editor::Editor::new(
                editor::EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: editor::SizingBehavior::ExcludeOverscrollMargin,
                },
                iris_multi_buffer,
                window,
                cx,
            );
            crate::editor_config::configure_preview(&mut editor, window, cx);
            editor.set_read_only(true);
            editor
        });

        let mut this = Self {
            hosts,
            subscriptions: Default::default(),
            store: AgentStore::default(),
            registry,
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
            ready_hosts: Default::default(),
            quota_summaries: HashMap::new(),
            quota_history: HashMap::new(),
            quota_history_days: 7,
            global_usage: HashMap::new(),
            global_usage_days: 7,
            duration_timer: None,
            contexts: HashMap::new(),
            surfaces: HashMap::new(),
            active_context: ContextId::Draft,
            dashboard,
            dashboard_preview: None,
            dashboard_dirty: true,
            iris_preview,
            minibuffer: None,
            transient: None,
            transient_stack: Vec::new(),
            transient_focus: cx.focus_handle(),
            overlay_return_focus: None,
            echo: None,
            realtime_task: None,
            realtime_stop: None,
            iris_muted: false,
            iris_host: None,
            _event_task: event_task,
            _dashboard_subscription: dashboard_subscription,
            web: WebUi {
                authorizations,
                target_error,
                touch_keyboard: cx.new(|_| TouchKeyboard::new(RhoKeyboardPlugin)),
                keyboard_visible: false,
            },
        };
        let draft = this.make_surface(SurfaceKey::Draft, window, cx);
        this.display_surface(draft);
        this.seed_draft(false, window, cx);
        window.focus(&this.dashboard.focus_handle(cx), cx);
        this
    }

    fn authorize_host(
        &mut self,
        host: HostId,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hosts.authorize(host);
        self.web
            .authorizations
            .insert(host, Authorization::Connecting);
        cx.notify();
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.active_editor(cx);
        let text_style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
        let connection_status = self.render_connection_status(&text_style, cx);
        self.sync_dashboard_if_dirty(window, cx);

        let narrow = window.viewport_size().width < px(700.);
        let phone = coarse_pointer() && narrow;
        let home = self.dashboard_mode(window, cx);
        let (inset_top, inset_right, inset_bottom, inset_left) = safe_area();
        let bar_height = px(40.) + inset_bottom;
        let show_keyboard = phone && !home && self.web.keyboard_visible;
        window.set_direct_touch_region(
            show_keyboard.then(|| TouchKeyboard::region(window.viewport_size(), bar_height)),
        );
        set_haptic_region(if show_keyboard {
            f32::from(window.viewport_size().height - bar_height - TouchKeyboard::height())
        } else {
            -1.
        });

        let body = if narrow {
            if home {
                div()
                    .id("dashboard-narrow")
                    .size_full()
                    .overflow_hidden()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dashboard_open_clicked_agent(window, cx)
                    }))
                    .child(self.dashboard.editor().clone())
                    .into_any_element()
            } else {
                let surface = self.active_tree().focused().surface.clone();
                div()
                    .size_full()
                    .overflow_hidden()
                    .child(self.render_surface(&surface))
                    .into_any_element()
            }
        } else {
            self.render_panes(window, &text_style, cx)
        };

        let auth_overlay = self.render_web_authorizations(cx);
        let bottom = match (
            &self.minibuffer,
            &self.transient,
            connection_status,
            &self.echo,
        ) {
            (Some(minibuffer), _, _, _) => Some(minibuffer.render(&text_style, cx)),
            (None, Some(transient), _, _) => Some(
                div()
                    .track_focus(&self.transient_focus)
                    .on_key_down(cx.listener(Self::transient_key))
                    .child(transient.render(&text_style, cx))
                    .into_any_element(),
            ),
            (None, None, Some(status), _) => Some(status),
            (None, None, None, Some(echo)) => Some(echo.render(&text_style, cx)),
            (None, None, None, None) => None,
        };

        let mut root = div()
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
            .on_action(cx.listener(Self::paste_prompt))
            .on_action(cx.listener(Self::toggle_voice))
            .on_action(cx.listener(|this, _: &crate::RootTransient, window, cx| {
                this.open_transient(crate::transient::root_menu(), window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::MinibufferConfirm, window, cx| {
                    this.minibuffer_confirm(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::MinibufferCancel, window, cx| {
                    this.minibuffer_cancel(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::MinibufferNext, _, cx| {
                if let Some(minibuffer) = &mut this.minibuffer {
                    minibuffer.select_by_delta(1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &crate::MinibufferPrevious, _, cx| {
                if let Some(minibuffer) = &mut this.minibuffer {
                    minibuffer.select_by_delta(-1);
                    cx.notify();
                }
            }))
            .on_action(
                cx.listener(|this, _: &crate::MinibufferComplete, window, cx| {
                    if let Some(mut minibuffer) = this.minibuffer.take() {
                        minibuffer.complete_selected(window, cx);
                        this.minibuffer = Some(minibuffer);
                    }
                }),
            )
            .child(div().flex_1().overflow_hidden().child(body))
            .children(show_keyboard.then(|| self.web.touch_keyboard.clone().into_any_element()));

        if phone {
            let text_muted = cx.theme().colors().text_muted;
            root = root.child(
                div()
                    .flex_none()
                    .h(bar_height)
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        div()
                            .id("home-toggle")
                            .cursor_pointer()
                            .h_full()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_color(text_muted)
                            .child(if home { "work" } else { "agents" })
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.dashboard_mode(window, cx) {
                                    this.focus_active_surface(window, cx);
                                } else {
                                    window.focus(&this.dashboard.focus_handle(cx), cx);
                                    this.web.keyboard_visible = false;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("kbd-toggle")
                            .cursor_pointer()
                            .h_full()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_color(text_muted)
                            .child(if show_keyboard {
                                "hide keyboard"
                            } else {
                                "keyboard"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.web.keyboard_visible = !this.web.keyboard_visible;
                                if this.web.keyboard_visible {
                                    this.focus_active_surface(window, cx);
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
                                this.web.keyboard_visible = false;
                            })),
                    ),
            );
        }
        root.children(bottom).children(auth_overlay)
    }
}

impl Workspace {
    fn render_web_authorizations(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        if self.web.authorizations.is_empty() && self.web.target_error.is_none() {
            return None;
        }
        let colors = cx.theme().colors();
        let mut cards = div().flex().flex_col().gap_3().max_w(px(460.));
        if let Some(error) = &self.web.target_error {
            cards = cards.child(div().child("No rho daemon").child(error.clone()));
        }
        for (host, authorization) in &self.web.authorizations {
            let name = self.host_label(*host);
            let host_id = *host;
            let card = match authorization {
                Authorization::Required => div()
                    .id(("authorize-host", host.0 as usize))
                    .cursor_pointer()
                    .child(format!("Connect to {name}"))
                    .child("Tap to unlock with your browser identity")
                    .on_click(cx.listener(move |this, event, window, cx| {
                        this.authorize_host(host_id, event, window, cx)
                    }))
                    .into_any_element(),
                Authorization::Connecting => div()
                    .child(format!("Connecting to {name}…"))
                    .into_any_element(),
                Authorization::Enrollment(code) => div()
                    .child(format!("{name} is not enrolled yet"))
                    .child(format!(
                        "Run `rho iroh approve {code}`, then reload this page"
                    ))
                    .into_any_element(),
            };
            cards = cards.child(
                div()
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.element_background)
                    .child(card),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(colors.editor_background.opacity(0.92))
                .text_color(colors.text)
                .child(cards)
                .into_any_element(),
        )
    }
}

/// Safe-area insets (top, right, bottom, left) of the display cutouts.
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
