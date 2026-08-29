//! Native narrow-screen projection for the canonical workspace.
//!
//! Phone mode keeps the existing surfaces alive in the workspace registry, but
//! replaces pane-tree presentation with one surface and a Desk-rooted stack.

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, FocusHandle, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point,
    Window, div, px,
};
use theme::ActiveTheme as _;

use super::{ContextId, Surface, SurfaceKey, Workspace};

const PHONE_MAX_WIDTH: Pixels = px(600.);
const TAP_SLOP: Pixels = px(8.);
const TARGET_HEIGHT: Pixels = px(48.);

pub(super) struct PhoneUi {
    pub(super) enabled: bool,
    forced: bool,
    stack: Vec<(ContextId, SurfaceKey)>,
    dashboard_press: Option<Point<Pixels>>,
    dashboard_focus: FocusHandle,
}

impl PhoneUi {
    pub(super) fn new(cx: &mut gpui::App) -> Self {
        let forced = std::env::var("RHO_PHONE").is_ok_and(|value| value == "1");
        Self {
            // The first render activates the projection. This keeps native
            // construction's seeded draft out of the phone history so the
            // Desk is always the permanent initial root, including with the
            // environment override.
            enabled: false,
            forced,
            stack: Vec::new(),
            dashboard_press: None,
            dashboard_focus: cx.focus_handle(),
        }
    }

    pub(super) fn update_mode(&mut self, window: &Window) -> PhoneModeChange {
        let was_enabled = self.enabled;
        self.enabled = self.forced || window.viewport_size().width <= PHONE_MAX_WIDTH;
        PhoneModeChange {
            enabled: self.enabled,
            entered: self.enabled && !was_enabled,
            exited: was_enabled && !self.enabled,
        }
    }

    pub(super) fn show(&mut self, context: ContextId, key: SurfaceKey) {
        self.stack
            .retain(|entry| entry.0 != context || entry.1 != key);
        self.stack.push((context, key));
    }

    pub(super) fn remove(&mut self, context: ContextId, key: &SurfaceKey) {
        self.stack
            .retain(|entry| entry.0 != context || &entry.1 != key);
    }

    pub(super) fn remove_key(&mut self, key: &SurfaceKey) {
        self.stack.retain(|entry| &entry.1 != key);
    }

    pub(super) fn retain_contexts(&mut self, mut keep: impl FnMut(&ContextId) -> bool) {
        self.stack.retain(|entry| keep(&entry.0));
    }
}

pub(super) struct PhoneModeChange {
    pub(super) enabled: bool,
    entered: bool,
    exited: bool,
}

const PHONE_FONT_SCALE: f32 = 1.4;

/// Touch is a plain-editor world: no Vim, no Helix, every editor accepts
/// text directly. Applied on phone-mode entry and reverted on exit so a
/// desktop window narrowed for a moment does not lose Helix. Live editors
/// pick the change up through the vim crate's SettingsStore observer.
pub(crate) fn set_touch_modal_editing(enabled: bool, cx: &mut gpui::App) {
    tracing::info!(helix = enabled, "touch modal editing toggle");
    let settings = cx.global_mut::<settings::SettingsStore>();
    settings.override_global(vim_mode_setting::VimModeSetting(false));
    settings.override_global(vim_mode_setting::HelixModeSetting(enabled));
}

impl Workspace {
    pub(super) fn phone_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let change = self.phone.update_mode(window);
        if change.entered {
            self.phone.stack.clear();
            self.phone_set_dashboard_browsing(true, window, cx);
            // Deferred: adjusting fonts and settings notifies observers,
            // which must not reenter the draw that detected the transition.
            cx.defer(|cx| {
                theme_settings::adjust_buffer_font_size(cx, |size| size * PHONE_FONT_SCALE);
                theme_settings::adjust_ui_font_size(cx, |size| size * PHONE_FONT_SCALE);
                set_touch_modal_editing(false, cx);
            });
        }
        if change.exited {
            self.dashboard
                .editor()
                .update(cx, |editor, _| editor.set_read_only(false));
            window.focus(&self.dashboard.focus_handle(cx), cx);
            cx.defer(|cx| {
                theme_settings::reset_buffer_font_size(cx);
                theme_settings::reset_ui_font_size(cx);
                set_touch_modal_editing(true, cx);
            });
        }
        change.enabled
    }

    pub(super) fn phone_dashboard_pointer_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.phone.dashboard_press = (event.button == MouseButton::Left).then_some(event.position);
        if !self.dashboard.raw_mode() {
            let focus = self.phone.dashboard_focus.clone();
            cx.defer_in(window, move |_, window, cx| window.focus(&focus, cx));
        }
    }

    pub(super) fn phone_dashboard_pointer_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(start) = self.phone.dashboard_press.take() else {
            return;
        };
        if event.button != MouseButton::Left
            || (event.position.x - start.x).abs() > TAP_SLOP
            || (event.position.y - start.y).abs() > TAP_SLOP
        {
            return;
        }
        cx.defer_in(window, |this, window, cx| {
            this.phone_dashboard_activate_tapped_row(window, cx);
        });
    }

    fn phone_dashboard_activate_tapped_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::dashboard::RowTarget;
        match self.dashboard.cursor_target(&self.registry, cx) {
            Some(RowTarget::Agent { agent_id, .. }) | Some(RowTarget::Reply(agent_id)) => {
                self.open_agent(agent_id, window, cx);
            }
            Some(RowTarget::Topic {
                on_heading_line: true,
                ..
            }) => {
                self.dashboard.toggle_subagents(cx);
                self.refresh_dashboard(window, cx);
            }
            Some(RowTarget::Page(id)) => self.open_browser_page(id, window, cx),
            _ => {}
        }
    }

    fn phone_surface(&self) -> Option<Surface> {
        let (context, key) = self.phone.stack.last()?;
        self.surfaces
            .get(context)?
            .iter()
            .find(|surface| &surface.key == key)
            .cloned()
    }

    pub(super) fn phone_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.transient.is_some() {
            if let Some(parent) = self.transient_stack.pop() {
                self.transient = Some(parent);
                cx.notify();
            } else {
                self.close_transient(window, cx);
            }
            return;
        }
        if self.minibuffer.is_some() {
            self.minibuffer_cancel(window, cx);
            return;
        }

        if self.phone.stack.is_empty() && self.dashboard.deal_mode() {
            self.toggle_dashboard_deal(window, cx);
            // The toggle focuses the desk editor; browse keeps the OSK down.
            self.phone_set_dashboard_browsing(true, window, cx);
            return;
        }
        if self.phone.stack.is_empty() && self.dashboard.raw_mode() {
            self.phone_set_dashboard_browsing(true, window, cx);
            return;
        }

        self.phone.stack.pop();
        let next = loop {
            let Some((context, key)) = self.phone.stack.last().cloned() else {
                break None;
            };
            let valid = self.contexts.contains_key(&context)
                && self
                    .surfaces
                    .get(&context)
                    .is_some_and(|surfaces| surfaces.iter().any(|surface| surface.key == key));
            if valid {
                break Some((context, key));
            }
            self.phone.stack.pop();
        };
        let Some((context, key)) = next else {
            self.phone_set_dashboard_browsing(true, window, cx);
            cx.notify();
            return;
        };
        self.active_context = context;
        if let Some(surface) = self
            .surfaces
            .get(&context)
            .and_then(|surfaces| surfaces.iter().find(|surface| surface.key == key))
            .cloned()
        {
            if let Some(tree) = self.contexts.get_mut(&context) {
                tree.focused_mut().show(surface);
            }
            self.sync_selection_to_focus(cx);
            self.focus_active_surface(window, cx);
        }
        cx.notify();
    }

    pub(super) fn render_phone_body(
        &mut self,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(surface) = self.phone_surface() {
            div()
                .id("phone-surface")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_hidden()
                .child(self.render_surface(&surface))
                .into_any_element()
        } else {
            let browsing = !self.dashboard.raw_mode();
            div()
                .id("phone-dashboard")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_hidden()
                .track_focus(&self.phone.dashboard_focus)
                // The editor consumes bubble-phase pointer events. Capture the
                // gesture around it, then inspect its updated cursor.
                .when(browsing, |dashboard| {
                    dashboard
                        .capture_any_mouse_down(cx.listener(Self::phone_dashboard_pointer_down))
                        .capture_any_mouse_up(cx.listener(Self::phone_dashboard_pointer_up))
                })
                .child(self.render_rail(false, text_style, cx))
                .into_any_element()
        }
    }

    pub(super) fn render_phone_bar(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let item = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .cursor_pointer()
                .h_full()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.text_muted)
                .child(label)
        };
        div()
            .id("phone-bottom-bar")
            .flex_none()
            .h(TARGET_HEIGHT)
            .w_full()
            .flex()
            .items_stretch()
            .border_t_1()
            .border_color(colors.border_variant)
            .child(
                item("phone-back", "back").on_click(cx.listener(|this, _, window, cx| {
                    this.phone_back(window, cx);
                })),
            )
            .child(
                item("phone-menu", "menu").on_click(cx.listener(|this, _, window, cx| {
                    let menu = if this.phone.stack.is_empty() {
                        crate::transient::phone_desk_menu(this.dashboard.raw_mode())
                    } else {
                        crate::transient::root_menu()
                    };
                    this.open_transient(menu, window, cx);
                })),
            )
            .child(
                item("phone-send", "send").on_click(cx.listener(|this, _, window, cx| {
                    this.phone_send(window, cx);
                })),
            )
            .into_any_element()
    }

    fn phone_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() {
            self.minibuffer_confirm(window, cx);
        } else if self.phone.stack.is_empty() && self.dashboard.raw_mode() {
            self.dashboard_submit(window, cx);
        } else if !self.phone.stack.is_empty() {
            self.submit_prompt(&crate::SubmitPrompt, window, cx);
        }
    }

    pub(crate) fn phone_cycle_dashboard_folds(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dashboard.cycle_global_folds(cx);
        self.refresh_dashboard(window, cx);
    }

    pub(crate) fn phone_toggle_dashboard_editing(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let browsing = self.dashboard.raw_mode();
        self.phone_set_dashboard_browsing(browsing, window, cx);
    }

    fn phone_set_dashboard_browsing(
        &mut self,
        browsing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dashboard.raw_mode() == browsing {
            self.dashboard.toggle_raw_mode(cx);
            self.refresh_dashboard(window, cx);
        }
        self.dashboard
            .editor()
            .update(cx, |editor, _| editor.set_read_only(browsing));
        let focus = if browsing {
            self.phone.dashboard_focus.clone()
        } else {
            self.dashboard.focus_handle(cx)
        };
        window.focus(&focus, cx);
        cx.notify();
    }

    fn phone_transient_action(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((run, stay)) = self
            .transient
            .as_ref()
            .and_then(|transient| transient.action_at(index))
        else {
            return;
        };
        if stay {
            run(self, window, cx);
            cx.notify();
            return;
        }

        let parent = self.transient.take();
        self.restore_overlay_focus(window, cx);
        run(self, window, cx);
        if self.transient.is_some() {
            self.transient_stack.extend(parent);
        } else {
            self.transient_stack.clear();
            if !self.has_modal_overlay() {
                self.overlay_return_focus = None;
            }
        }
        cx.notify();
    }

    pub(super) fn render_phone_transient_sheet(
        &self,
        text_style: &gpui::TextStyle,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let transient = self.transient.as_ref()?;
        let colors = cx.theme().colors();
        let title = transient.title();
        let rows = transient.phone_rows();
        let has_parent = !self.transient_stack.is_empty();

        let mut header = div()
            .flex()
            .items_center()
            .min_h(TARGET_HEIGHT)
            .px_3()
            .gap_3()
            .border_b_1()
            .border_color(colors.border_variant);
        if has_parent {
            header = header.child(
                div()
                    .id("phone-sheet-back")
                    .cursor_pointer()
                    .h_full()
                    .min_w(TARGET_HEIGHT)
                    .flex()
                    .items_center()
                    .child("back")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if let Some(parent) = this.transient_stack.pop() {
                            this.transient = Some(parent);
                            cx.notify();
                        }
                        cx.stop_propagation();
                    })),
            );
        }
        header = header
            .child(
                div()
                    .flex_1()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(title),
            )
            .child(
                div()
                    .id("phone-sheet-close")
                    .cursor_pointer()
                    .h_full()
                    .min_w(TARGET_HEIGHT)
                    .flex()
                    .items_center()
                    .justify_end()
                    .child("close")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_transient(window, cx);
                        cx.stop_propagation();
                    })),
            );

        let rows = rows
            .into_iter()
            .enumerate()
            .map(|(index, (key, description, value))| {
                let mut row = div()
                    .id(("phone-sheet-row", index))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .min_h(TARGET_HEIGHT)
                    .w_full()
                    .px_3()
                    .gap_3()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        div()
                            .min_w(px(36.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(colors.text_accent)
                            .child(key),
                    )
                    .child(div().flex_1().child(description));
                if let Some(value) = value {
                    row = row.child(div().text_color(colors.text_muted).child(value));
                }
                row.on_click(cx.listener(move |this, _, window, cx| {
                    this.phone_transient_action(index, window, cx);
                    cx.stop_propagation();
                }))
            });

        let mut background: gpui::Hsla = colors.editor_background.into();
        if background.l < 0.5 {
            background.l += 0.04;
        } else {
            background.l -= 0.04;
        }
        Some(
            div()
                .id("phone-sheet-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .justify_end()
                .bg(gpui::black().opacity(0.35))
                .track_focus(&self.transient_focus)
                .on_key_down(cx.listener(Self::transient_key))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_transient(window, cx);
                }))
                .child(
                    div()
                        .id("phone-sheet")
                        .max_h(gpui::relative(0.82))
                        .w_full()
                        .overflow_y_scroll()
                        .bg(background)
                        .text_color(text_style.color)
                        .font_family(text_style.font_family.clone())
                        .font_weight(text_style.font_weight)
                        .text_size(text_style.font_size)
                        .line_height(text_style.line_height)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(header)
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn showing_an_existing_surface_brings_it_to_top_without_duplicates(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let mut phone = PhoneUi::new(cx);
            phone.show(ContextId::Draft, SurfaceKey::Draft);
            phone.show(ContextId::Draft, SurfaceKey::Draft);

            assert_eq!(phone.stack.len(), 1);
            assert_eq!(
                phone.stack.last(),
                Some(&(ContextId::Draft, SurfaceKey::Draft))
            );
        });
    }
}
