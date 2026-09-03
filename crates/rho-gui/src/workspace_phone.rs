//! Native narrow-screen projection for the canonical workspace.
//!
//! Phone mode keeps the existing surfaces alive in the workspace registry, but
//! replaces desktop presentation with one surface and a Desk-rooted stack.

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, FocusHandle, Focusable as _, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, TouchEvent, TouchId, TouchPhase, Window, div, px,
};
use theme::ActiveTheme as _;

use super::{ContextId, Surface, SurfaceKey, Workspace};

const PHONE_MAX_WIDTH: Pixels = px(600.);
const TAP_SLOP: Pixels = px(8.);
const TARGET_HEIGHT: Pixels = px(56.);
const FLICK_SLOP: f32 = 12.;
const FLICK_COMMIT_VELOCITY: f32 = 900.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhoneScrollEdge {
    Top,
    Bottom,
    Both,
    Middle,
}

impl PhoneScrollEdge {
    fn permits(self, direction: crate::journal::PhoneFlickDirection) -> bool {
        matches!(
            (self, direction),
            (
                Self::Top | Self::Both,
                crate::journal::PhoneFlickDirection::Down
            ) | (
                Self::Bottom | Self::Both,
                crate::journal::PhoneFlickDirection::Up
            )
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhoneRoot {
    Feed,
    Desk,
}

enum PhoneTransition {
    Flick(crate::dashboard::DealCard),
    Verdict(u64),
}

struct PhoneFlickGesture {
    id: TouchId,
    start: Point<Pixels>,
    started_at: std::time::Duration,
    position: Point<Pixels>,
    timestamp: std::time::Duration,
    edge: PhoneScrollEdge,
}

impl PhoneFlickGesture {
    fn new(event: &TouchEvent, edge: PhoneScrollEdge) -> Self {
        Self {
            id: event.id,
            start: event.position,
            started_at: event.timestamp,
            position: event.position,
            timestamp: event.timestamp,
            edge,
        }
    }

    fn update(&mut self, event: &TouchEvent) {
        self.position = event.position;
        self.timestamp = event.timestamp;
    }

    fn direction(&self) -> Option<crate::journal::PhoneFlickDirection> {
        let dy = (self.position.y - self.start.y).as_f32();
        let dx = (self.position.x - self.start.x).as_f32();
        if dy.abs() < FLICK_SLOP || dy.abs() < dx.abs() * 1.25 {
            return None;
        }
        Some(if dy < 0. {
            crate::journal::PhoneFlickDirection::Up
        } else {
            crate::journal::PhoneFlickDirection::Down
        })
    }

    fn claims_touch(&self) -> bool {
        self.direction()
            .is_some_and(|direction| self.edge.permits(direction))
    }

    fn committed_direction(
        &self,
        viewport_height: Pixels,
    ) -> Option<crate::journal::PhoneFlickDirection> {
        let direction = self.direction()?;
        if !self.edge.permits(direction) {
            return None;
        }
        let distance = (self.position.y - self.start.y).as_f32().abs();
        let elapsed = self.timestamp.saturating_sub(self.started_at).as_secs_f32();
        let velocity = if elapsed > 0. { distance / elapsed } else { 0. };
        (distance >= viewport_height.as_f32() / 3. || velocity >= FLICK_COMMIT_VELOCITY)
            .then_some(direction)
    }
}

pub(super) struct PhoneUi {
    pub(super) enabled: bool,
    forced: bool,
    touch_debug: bool,
    last_gesture: Option<String>,
    flick: Option<PhoneFlickGesture>,
    drag_offset: Pixels,
    transitions: Vec<PhoneTransition>,
    root: PhoneRoot,
    feed_surface: Option<(ContextId, SurfaceKey)>,
    stack: Vec<(ContextId, SurfaceKey)>,
    dashboard_press: Option<(Point<Pixels>, Option<crate::dashboard::RowTarget>)>,
    pub(super) dashboard_focus: FocusHandle,
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
            touch_debug: std::env::var("RHO_PHONE_TOUCH_DEBUG").is_ok_and(|value| value == "1"),
            last_gesture: None,
            flick: None,
            drag_offset: Pixels::ZERO,
            transitions: Vec::new(),
            root: PhoneRoot::Feed,
            feed_surface: None,
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

    pub(super) fn show_feed(&mut self, context: ContextId, key: SurfaceKey) {
        self.feed_surface = Some((context, key));
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

    pub(super) fn touch_debug_enabled(&self) -> bool {
        self.touch_debug
    }

    fn record_flick(&mut self, direction: crate::journal::PhoneFlickDirection, moved_card: bool) {
        let direction = match direction {
            crate::journal::PhoneFlickDirection::Up => "up",
            crate::journal::PhoneFlickDirection::Down => "down",
        };
        let outcome = if moved_card { "moved" } else { "stayed" };
        self.last_gesture = Some(format!("flick {direction} · {outcome}"));
    }

    fn record_verdict(&mut self, verdict: crate::journal::PhoneVerdict) {
        let verdict = match verdict {
            crate::journal::PhoneVerdict::Done => "done",
            crate::journal::PhoneVerdict::Dismiss => "dismiss",
            crate::journal::PhoneVerdict::Defer => "defer",
            crate::journal::PhoneVerdict::Todo => "todo",
            crate::journal::PhoneVerdict::File => "file",
            crate::journal::PhoneVerdict::Reply => "reply",
        };
        self.last_gesture = Some(format!("verdict {verdict}"));
    }

    fn touch_debug_label(&self, contacts: usize) -> String {
        format!(
            "contacts {contacts} · last {}",
            self.last_gesture.as_deref().unwrap_or("none")
        )
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
            self.phone.transitions.clear();
            self.phone.root = PhoneRoot::Feed;
            if self.dashboard.deal_mode() {
                self.phone
                    .show_feed(self.active_context, self.active_pane().surface.key.clone());
            } else {
                self.phone.feed_surface = None;
                cx.defer_in(window, |this, window, cx| this.open_deal_mode(window, cx));
            }
            self.update_statuses(cx);
            // Deferred: adjusting fonts and settings notifies observers,
            // which must not reenter the draw that detected the transition.
            cx.defer(|cx| {
                theme_settings::adjust_buffer_font_size(cx, |size| size * PHONE_FONT_SCALE);
                theme_settings::adjust_ui_font_size(cx, |size| size * PHONE_FONT_SCALE);
                set_touch_modal_editing(false, cx);
            });
        }
        if change.exited {
            self.phone.flick = None;
            self.phone.drag_offset = Pixels::ZERO;
            self.deal_focus_pending = self.dashboard.deal_mode();
            self.update_statuses(cx);
            if self.dashboard.set_phone_browse_mode(false) {
                cx.defer_in(window, |this, window, cx| {
                    this.refresh_dashboard(window, cx)
                });
            }
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
        self.phone.dashboard_press = (event.button == MouseButton::Left).then(|| {
            (
                event.position,
                self.dashboard
                    .target_at_window_position(event.position, &self.registry, cx),
            )
        });
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
        let Some((start, target)) = self.phone.dashboard_press.take() else {
            return;
        };
        if event.button != MouseButton::Left
            || (event.position.x - start.x).abs() > TAP_SLOP
            || (event.position.y - start.y).abs() > TAP_SLOP
        {
            return;
        }
        cx.defer_in(window, move |this, window, cx| {
            this.phone_dashboard_activate_tapped_row(target, window, cx);
        });
    }

    fn phone_surface_pointer_down(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Let the editor place its cursor first, then only enable text input
        // when that cursor landed in the editable prompt tail.
        cx.defer_in(window, |this, window, cx| {
            let Some(surface) = this.phone_surface() else {
                return;
            };
            let super::SurfaceView::Transcript { model, editor } = &surface.view else {
                return;
            };
            let focus = if model.read(cx).selection_in_prompt(editor, cx) {
                editor.focus_handle(cx)
            } else {
                this.phone.dashboard_focus.clone()
            };
            window.focus(&focus, cx);
        });
    }

    fn phone_dashboard_activate_tapped_row(
        &mut self,
        target: Option<crate::dashboard::RowTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::dashboard::RowTarget;
        match target {
            Some(RowTarget::TreeAgent { agent_id, .. }) => self.open_agent(agent_id, window, cx),
            Some(RowTarget::TreeTopic {
                host,
                node_id,
                first_attention,
                ..
            }) => match first_attention
                .or_else(|| self.dashboard.first_tree_agent_for_topic((host, node_id)))
            {
                Some(agent_id) => self.open_agent(agent_id, window, cx),
                None => {
                    self.dashboard.move_to_tree_node_when_ready(host, node_id);
                    self.dashboard.toggle_subagents(cx);
                    self.refresh_dashboard(window, cx);
                }
            },
            Some(RowTarget::TreePage { page_id, .. }) => {
                self.open_browser_page(page_id, window, cx)
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn phone_feed_for_test(&self) -> bool {
        self.phone.enabled && self.phone.stack.is_empty() && self.dashboard.deal_mode()
    }

    #[cfg(test)]
    pub(crate) fn phone_feed_is_active_for_test(&self) -> bool {
        self.phone
            .feed_surface
            .as_ref()
            .is_some_and(|(context, key)| {
                *context == self.active_context && self.active_pane().surface.key == *key
            })
    }

    #[cfg(test)]
    pub(crate) fn phone_last_gesture_for_test(&self) -> Option<&str> {
        self.phone.last_gesture.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn phone_remember_last_verdict_for_test(&mut self) {
        let sequence = self.verdict_undo.last().unwrap().sequence;
        self.phone
            .transitions
            .push(PhoneTransition::Verdict(sequence));
    }

    #[cfg(test)]
    pub(crate) fn phone_has_surface_for_test(&self, key: &SurfaceKey) -> bool {
        self.phone
            .stack
            .iter()
            .any(|(_, candidate)| candidate == key)
    }

    #[cfg(test)]
    pub(crate) fn phone_back_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.phone_back(window, cx);
    }

    fn phone_surface(&self) -> Option<Surface> {
        let (context, key) = self.phone.stack.last()?;
        self.surfaces
            .get(context)?
            .iter()
            .find(|surface| &surface.key == key)
            .cloned()
    }

    pub(super) fn restore_phone_feed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((context, key)) = self.phone.feed_surface.clone() else {
            return;
        };
        let Some(surface) = self
            .surfaces
            .get(&context)
            .and_then(|surfaces| surfaces.iter().find(|surface| surface.key == key))
            .cloned()
        else {
            self.phone.feed_surface = None;
            return;
        };
        self.active_context = context;
        if let Some(pane) = self.contexts.get_mut(&context) {
            pane.show(surface);
        }
        self.sync_selection_to_focus(cx);
        window.focus(&self.phone.dashboard_focus, cx);
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
            self.phone.root = PhoneRoot::Feed;
            self.restore_phone_feed(window, cx);
            cx.notify();
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
            self.phone.root = PhoneRoot::Feed;
            if self.dashboard.deal_mode() {
                self.restore_phone_feed(window, cx);
                cx.notify();
            } else {
                self.open_deal_mode(window, cx);
            }
            return;
        };
        self.active_context = context;
        if let Some(surface) = self
            .surfaces
            .get(&context)
            .and_then(|surfaces| surfaces.iter().find(|surface| surface.key == key))
            .cloned()
        {
            if let Some(pane) = self.contexts.get_mut(&context) {
                pane.show(surface);
            }
            self.sync_selection_to_focus(cx);
            self.focus_active_surface(window, cx);
        }
        cx.notify();
    }

    fn phone_deal_scroll_edge(&mut self, cx: &mut Context<Self>) -> PhoneScrollEdge {
        let editor = match self.deal_view.as_ref() {
            Some(super::DealView::Desk { editor, .. })
            | Some(super::DealView::Inbox { editor, .. }) => Some(editor.clone()),
            Some(super::DealView::Surface { surface, .. }) => match &surface.view {
                super::SurfaceView::DeskNode(editor)
                | super::SurfaceView::Inbox(editor)
                | super::SurfaceView::Transcript { editor, .. } => Some(editor.clone()),
                _ => None,
            },
            None => None,
        };
        let Some(editor) = editor else {
            return PhoneScrollEdge::Middle;
        };
        editor.update(cx, |editor, cx| {
            let top = editor.scroll_position(cx).y;
            let Some(visible) = editor.visible_line_count() else {
                return PhoneScrollEdge::Middle;
            };
            let rows = f64::from(editor.max_point(cx).row().0 + 1);
            let max_top = (rows - visible).max(0.);
            let at_top = top <= 0.25;
            let at_bottom = top >= max_top - 0.25;
            match (at_top, at_bottom) {
                (true, true) => PhoneScrollEdge::Both,
                (true, false) => PhoneScrollEdge::Top,
                (false, true) => PhoneScrollEdge::Bottom,
                (false, false) => PhoneScrollEdge::Middle,
            }
        })
    }

    pub(super) fn phone_debug_touch(&mut self, event: &TouchEvent, cx: &mut Context<Self>) {
        match event.phase {
            TouchPhase::Started => {
                self.shell_touches.insert(
                    event.id,
                    super::ShellTouchContact {
                        start: event.position,
                        position: event.position,
                    },
                );
                if self.shell_touches.len() > 1 {
                    self.phone.flick = None;
                    self.phone.drag_offset = Pixels::ZERO;
                }
            }
            TouchPhase::Moved => {
                if let Some(contact) = self.shell_touches.get_mut(&event.id) {
                    contact.position = event.position;
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.shell_touches.remove(&event.id);
            }
        }
        if self.phone.touch_debug_enabled() {
            cx.notify();
        }
    }

    pub(super) fn phone_touch(
        &mut self,
        event: &TouchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.phase {
            TouchPhase::Started => {
                if self.shell_touches.len() == 1
                    && self.phone.root == PhoneRoot::Feed
                    && self.phone.stack.is_empty()
                    && !self.phone_current_deal_has_pending_tree_verdict()
                    && (self.dashboard.deal_mode() || !self.phone.transitions.is_empty())
                    && self.transient.is_none()
                    && self.minibuffer.is_none()
                {
                    let edge = if self.dashboard.deal_mode() {
                        self.phone_deal_scroll_edge(cx)
                    } else {
                        PhoneScrollEdge::Both
                    };
                    self.phone.flick = Some(PhoneFlickGesture::new(event, edge));
                } else {
                    self.phone.flick = None;
                }
                if self.shell_touches.len() > 1 {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }
            TouchPhase::Moved => {
                let (claims, yielded) = self.phone.flick.as_mut().map_or((false, false), |flick| {
                    if flick.id != event.id {
                        return (false, false);
                    }
                    flick.update(event);
                    let direction = flick.direction();
                    (
                        flick.claims_touch(),
                        direction.is_some_and(|direction| !flick.edge.permits(direction)),
                    )
                });
                if yielded {
                    self.phone.flick = None;
                }
                self.phone.drag_offset = if claims {
                    self.phone
                        .flick
                        .as_ref()
                        .map_or(Pixels::ZERO, |flick| flick.position.y - flick.start.y)
                } else {
                    Pixels::ZERO
                };
                if claims {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                let direction = self.phone.flick.take().and_then(|mut flick| {
                    (flick.id == event.id && event.phase == TouchPhase::Ended).then(|| {
                        flick.update(event);
                        flick.committed_direction(window.viewport_size().height)
                    })?
                });
                self.phone.drag_offset = Pixels::ZERO;
                if let Some(direction) = direction {
                    window.prevent_default();
                    cx.stop_propagation();
                    self.commit_phone_flick(direction, window, cx);
                }
            }
        }
        cx.notify();
    }

    fn commit_phone_flick(
        &mut self,
        direction: crate::journal::PhoneFlickDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.dashboard.current_deal_card().cloned();
        match direction {
            crate::journal::PhoneFlickDirection::Up => self.deal_next(window, cx),
            crate::journal::PhoneFlickDirection::Down => match self.phone.transitions.pop() {
                Some(PhoneTransition::Flick(card)) => {
                    self.dashboard.clear_skip(&card.identity);
                    self.dashboard.reopen_deal(card);
                    self.deal_session_open = true;
                    self.present_current_deal(window, cx);
                    self.refresh_dashboard(window, cx);
                }
                Some(PhoneTransition::Verdict(sequence))
                    if self.verdict_undo.last().map(|entry| entry.sequence) == Some(sequence) =>
                {
                    self.undo_verdict(window, cx)
                }
                Some(PhoneTransition::Verdict(_)) | None => {}
            },
        }
        let after = self.dashboard.current_deal_card().cloned();
        let moved_card =
            before.as_ref().map(|card| &card.identity) != after.as_ref().map(|card| &card.identity);
        if direction == crate::journal::PhoneFlickDirection::Up
            && moved_card
            && let Some(card) = before
        {
            self.phone.transitions.push(PhoneTransition::Flick(card));
        }
        self.record_phone_flick(direction, moved_card, cx);
    }

    pub(super) fn record_phone_flick(
        &mut self,
        direction: crate::journal::PhoneFlickDirection,
        moved_card: bool,
        cx: &mut Context<Self>,
    ) {
        self.phone.record_flick(direction, moved_card);
        crate::journal::record(crate::journal::Event::PhoneFlick {
            direction,
            moved_card,
        });
        cx.notify();
    }

    pub(super) fn record_phone_verdict(
        &mut self,
        verdict: crate::journal::PhoneVerdict,
        cx: &mut Context<Self>,
    ) {
        self.phone.record_verdict(verdict);
        crate::journal::record(crate::journal::Event::PhoneVerdict { verdict });
        cx.notify();
    }

    pub(super) fn render_phone_touch_debug(&self, contacts: usize) -> Option<AnyElement> {
        self.phone.touch_debug.then(|| {
            div()
                .id("phone-touch-debug")
                .absolute()
                .top_2()
                .right_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(gpui::black().opacity(0.75))
                .text_color(gpui::white())
                .child(self.phone.touch_debug_label(contacts))
                .into_any_element()
        })
    }

    pub(super) fn render_phone_body(
        &mut self,
        text_style: &gpui::TextStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.phone.root == PhoneRoot::Feed
            && self.phone.stack.is_empty()
            && let Some(card) = self.dashboard.current_deal_card().cloned()
        {
            let colors = cx.theme().colors();
            let header = div()
                .id("phone-deal-header")
                .flex_none()
                .h(px(32.))
                .w_full()
                .px_2()
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(colors.border_variant)
                .text_color(colors.text_muted)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_transient(crate::transient::phone_root_menu(), window, cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(card.breadcrumb.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .ml_2()
                        .whitespace_nowrap()
                        .child(card.label.clone()),
                );
            let body = self.deal_body(&card, window, cx);
            return div()
                .id("phone-deal-card")
                .track_focus(&self.phone.dashboard_focus)
                .size_full()
                .relative()
                .top(self.phone.drag_offset)
                .flex()
                .flex_col()
                .child(header)
                .child(
                    div()
                        .id("phone-deal-body")
                        .capture_touch(cx.listener(Self::phone_touch))
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .child(body),
                )
                .child(self.render_phone_verdict_bar(cx))
                .into_any_element();
        }
        if self.phone.root == PhoneRoot::Feed && self.phone.stack.is_empty() {
            let colors = cx.theme().colors();
            return div()
                .id("phone-feed-empty")
                .track_focus(&self.phone.dashboard_focus)
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("phone-feed-empty-header")
                        .h(px(32.))
                        .w_full()
                        .px_2()
                        .flex()
                        .items_center()
                        .text_color(colors.text_muted)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_transient(crate::transient::phone_root_menu(), window, cx);
                        }))
                        .child("deal"),
                )
                .child(
                    div()
                        .id("phone-feed-empty-body")
                        .capture_touch(cx.listener(Self::phone_touch))
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(colors.text_muted)
                        .child("nothing needs you"),
                )
                .child(self.render_phone_bar(cx))
                .into_any_element();
        }
        if let Some(surface) = self.phone_surface() {
            div()
                .id("phone-surface")
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .capture_any_mouse_down(cx.listener(Self::phone_surface_pointer_down))
                        .child(self.render_surface(&surface)),
                )
                .child(self.render_phone_bar(cx))
                .into_any_element()
        } else {
            let browsing = !self.dashboard.raw_mode();
            div()
                .id("phone-dashboard")
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .track_focus(&self.phone.dashboard_focus)
                        // The editor consumes bubble-phase pointer events. Capture the
                        // gesture around it, then inspect its updated cursor.
                        .when(browsing, |dashboard| {
                            dashboard
                                .capture_any_mouse_down(
                                    cx.listener(Self::phone_dashboard_pointer_down),
                                )
                                .capture_any_mouse_up(cx.listener(Self::phone_dashboard_pointer_up))
                        })
                        .child(self.render_rail(false, text_style, cx)),
                )
                .child(self.render_phone_bar(cx))
                .into_any_element()
        }
    }

    pub(super) fn phone_completed_verdict(&mut self, sequence: u64) {
        self.phone
            .transitions
            .push(PhoneTransition::Verdict(sequence));
    }

    pub(super) fn phone_current_deal_has_pending_tree_verdict(&self) -> bool {
        let Some(card) = self.dashboard.current_deal_card() else {
            return false;
        };
        self.pending_tree_verdicts
            .values()
            .any(|pending| pending.event.card == card.identity)
    }

    fn dispatch_phone_verdict(
        &mut self,
        verdict: crate::journal::PhoneVerdict,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.phone_current_deal_has_pending_tree_verdict() {
            return;
        }
        let before = self
            .dashboard
            .current_deal_card()
            .map(|card| card.identity.clone());
        let pending_before = self.pending_tree_verdicts.len();
        let undo_before = self.verdict_undo.last().map(|entry| entry.sequence);
        window.dispatch_action(action, cx);
        cx.defer_in(window, move |this, _window, cx| {
            let after = this
                .dashboard
                .current_deal_card()
                .map(|card| card.identity.clone());
            let submitted = this.pending_tree_verdicts.len() > pending_before;
            if before.is_some() && (before != after || submitted) {
                if !submitted
                    && let Some(sequence) = this.verdict_undo.last().map(|entry| entry.sequence)
                    && Some(sequence) != undo_before
                {
                    this.phone
                        .transitions
                        .push(PhoneTransition::Verdict(sequence));
                }
                if !submitted {
                    this.record_phone_verdict(verdict, cx);
                }
            }
        });
    }

    fn render_phone_verdict_bar(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let item = |id: &'static str, icon: &'static str, label: &'static str| {
            div()
                .id(id)
                .cursor_pointer()
                .h_full()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .text_color(colors.text_muted)
                .child(div().text_size(px(18.)).child(icon))
                .child(div().text_size(px(11.)).child(label))
        };
        div()
            .id("phone-verdict-bar")
            .flex_none()
            .h(TARGET_HEIGHT)
            .w_full()
            .flex()
            .items_stretch()
            .border_t_1()
            .border_color(colors.border_variant)
            .child(
                item("phone-verdict-done", "✓", "done").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            crate::journal::PhoneVerdict::Done,
                            Box::new(crate::DashboardDealDone),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(
                item("phone-verdict-dismiss", "×", "dismiss").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            crate::journal::PhoneVerdict::Dismiss,
                            Box::new(crate::DashboardDealDiscard),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(
                item("phone-verdict-defer", "◷", "defer").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            crate::journal::PhoneVerdict::Defer,
                            Box::new(crate::DashboardDealSnooze),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(
                item("phone-verdict-todo", "○", "todo").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            crate::journal::PhoneVerdict::Todo,
                            Box::new(crate::DashboardDealTodo),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(
                item("phone-verdict-file", "⌂", "file").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            crate::journal::PhoneVerdict::File,
                            Box::new(crate::DashboardDealFile),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(
                item("phone-verdict-reply", "↩", "reply").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            crate::journal::PhoneVerdict::Reply,
                            Box::new(crate::DashboardDealReply),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .into_any_element()
    }

    pub(super) fn render_phone_bar(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let item = |id: &'static str, icon: &'static str, label: &'static str| {
            div()
                .id(id)
                .cursor_pointer()
                .h_full()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .text_color(colors.text_muted)
                .child(div().text_size(px(18.)).child(icon))
                .child(div().text_size(px(11.)).child(label))
        };
        let primary = if self.phone.stack.is_empty() && self.phone.root == PhoneRoot::Desk {
            Some(item("phone-edit", "✎", "edit").on_click(
                cx.listener(|this, _, window, cx| this.phone_toggle_dashboard_editing(window, cx)),
            ))
        } else if self.phone_surface().is_some_and(|surface| {
            matches!(
                surface.view,
                super::SurfaceView::Draft { .. }
                    | super::SurfaceView::Transcript { .. }
                    | super::SurfaceView::SlackConversation(_)
                    | super::SurfaceView::ZulipNarrow(_)
            )
        }) {
            Some(
                item("phone-send", "↑", "send")
                    .on_click(cx.listener(|this, _, window, cx| this.phone_send(window, cx))),
            )
        } else {
            None
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
                item("phone-back", "‹", "back")
                    .on_click(cx.listener(|this, _, window, cx| this.phone_back(window, cx))),
            )
            .child(
                item("phone-menu", "☰", "menu").on_click(cx.listener(|this, _, window, cx| {
                    this.open_transient(crate::transient::phone_root_menu(), window, cx);
                })),
            )
            .children(primary)
            .into_any_element()
    }

    pub(crate) fn phone_open_desk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard.deal_mode() {
            self.dashboard.end_deal(cx);
            self.end_deal_session();
            self.deal_view = None;
            self.deal_current_interacted = false;
        }
        self.phone.stack.clear();
        self.phone.root = PhoneRoot::Desk;
        self.phone.feed_surface = None;
        self.phone_set_dashboard_browsing(true, window, cx);
    }

    fn phone_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() {
            self.minibuffer_confirm(window, cx);
            return;
        }
        if self.phone.stack.is_empty() {
            if self.dashboard.raw_mode() {
                self.dashboard_submit(window, cx);
            }
            return;
        }
        let Some(surface) = self.phone_surface() else {
            return;
        };
        match surface.view {
            super::SurfaceView::Draft { .. } | super::SurfaceView::Transcript { .. } => {
                self.submit_prompt(&crate::SubmitPrompt, window, cx)
            }
            super::SurfaceView::SlackConversation(view) => {
                view.update(cx, |view, cx| view.submit(cx));
            }
            super::SurfaceView::ZulipNarrow(view) => {
                view.update(cx, |view, cx| view.submit(cx));
            }
            _ => {}
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
        let presentation_changed = self.dashboard.set_phone_browse_mode(browsing);
        if self.dashboard.raw_mode() == browsing {
            self.dashboard.toggle_raw_mode(cx);
            self.refresh_dashboard(window, cx);
        } else if presentation_changed {
            cx.defer_in(window, |this, window, cx| {
                this.refresh_dashboard(window, cx)
            });
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
            .map(|(index, (_key, description, value))| {
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

    #[gpui::test]
    fn touch_debug_label_reports_contacts_and_last_gesture(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut phone = PhoneUi::new(cx);
            assert_eq!(phone.touch_debug_label(2), "contacts 2 · last none");

            phone.record_flick(crate::journal::PhoneFlickDirection::Up, false);
            assert_eq!(
                phone.touch_debug_label(1),
                "contacts 1 · last flick up · stayed"
            );

            phone.record_verdict(crate::journal::PhoneVerdict::Done);
            assert_eq!(phone.touch_debug_label(0), "contacts 0 · last verdict done");
        });
    }

    fn touch(phase: TouchPhase, y: f32, milliseconds: u64) -> TouchEvent {
        TouchEvent {
            id: TouchId(1),
            phase,
            position: gpui::point(px(100.), px(y)),
            timestamp: std::time::Duration::from_millis(milliseconds),
            ..Default::default()
        }
    }

    #[test]
    fn flick_requires_the_matching_scroll_edge() {
        let start = touch(TouchPhase::Started, 500., 0);
        let end = touch(TouchPhase::Ended, 350., 100);
        let mut at_bottom = PhoneFlickGesture::new(&start, PhoneScrollEdge::Bottom);
        at_bottom.update(&end);
        assert_eq!(
            at_bottom.committed_direction(px(600.)),
            Some(crate::journal::PhoneFlickDirection::Up)
        );

        let mut in_middle = PhoneFlickGesture::new(&start, PhoneScrollEdge::Middle);
        in_middle.update(&end);
        assert_eq!(in_middle.committed_direction(px(600.)), None);
    }

    #[test]
    fn flick_requires_distance_or_velocity_at_the_scroll_end() {
        let start = touch(TouchPhase::Started, 500., 0);
        let mut slow = PhoneFlickGesture::new(&start, PhoneScrollEdge::Bottom);
        slow.update(&touch(TouchPhase::Ended, 350., 1000));
        assert_eq!(slow.committed_direction(px(600.)), None);

        let mut long_drag = PhoneFlickGesture::new(&start, PhoneScrollEdge::Bottom);
        long_drag.update(&touch(TouchPhase::Ended, 250., 2000));
        assert_eq!(
            long_drag.committed_direction(px(600.)),
            Some(crate::journal::PhoneFlickDirection::Up)
        );

        let mut fast = PhoneFlickGesture::new(&start, PhoneScrollEdge::Bottom);
        fast.update(&touch(TouchPhase::Ended, 440., 50));
        assert_eq!(
            fast.committed_direction(px(600.)),
            Some(crate::journal::PhoneFlickDirection::Up)
        );
    }
}
