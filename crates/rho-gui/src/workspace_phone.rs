//! Native narrow-screen projection for the canonical workspace.
//!
//! Phone mode keeps the existing surfaces alive in the workspace registry, but
//! replaces desktop presentation with one surface and a Desk-rooted stack.

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Context, FocusHandle, Focusable as _,
    MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, TouchEvent, TouchId, TouchPhase,
    Window, div, ease_out_quint, px,
};
use theme::ActiveTheme as _;

use super::{ContextId, Surface, SurfaceKey, Workspace};

const PHONE_MAX_WIDTH: Pixels = px(600.);
const TARGET_HEIGHT: Pixels = px(56.);
const PHONE_HEADER_HEIGHT: Pixels = px(48.);
const PHONE_HEADER_BUTTON_WIDTH: Pixels = px(48.);
/// The two buttons on the right of the title bar and the left padding.
const PHONE_HEADER_BUTTONS_WIDTH: Pixels = px(108.);
const FLICK_SLOP: f32 = 12.;
const FLICK_COMMIT_VELOCITY: f32 = 900.;
const SNAP_DURATION: std::time::Duration = std::time::Duration::from_millis(180);
const PHONE_DEAL_HEADER_FIXED_GUTTER: Pixels = px(24.);
/// The share of the window a menu sheet may take.
const SHEET_MAX_HEIGHT: f32 = 0.82;

fn phone_deal_header_text(
    path: &str,
    state: &str,
    header_width: Pixels,
    measure: impl Fn(&str) -> Pixels,
) -> (String, String) {
    let state_width = measure(state);
    let path_width = (header_width - PHONE_DEAL_HEADER_FIXED_GUTTER - state_width).max(px(0.));
    if measure(path) <= path_width {
        return (path.to_owned(), state.to_owned());
    }

    let ellipsis = "…";
    let mut truncated = String::new();
    for (boundary, character) in path.char_indices() {
        if !character.is_whitespace() {
            continue;
        }
        let prefix = path[..boundary]
            .trim_end_matches(|character: char| character.is_whitespace() || character == '/');
        let candidate = format!("{prefix}{ellipsis}");
        if measure(&candidate) > path_width {
            break;
        }
        truncated = candidate;
    }
    if truncated.is_empty() && measure(ellipsis) <= path_width {
        truncated.push_str(ellipsis);
    }
    (truncated, state.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhoneScrollEdge {
    Top,
    Bottom,
    Both,
    Middle,
}

impl PhoneScrollEdge {
    fn permits(self, direction: rho_journal::PhoneFlickDirection) -> bool {
        matches!(
            (self, direction),
            (
                Self::Top | Self::Both,
                rho_journal::PhoneFlickDirection::Down
            ) | (
                Self::Bottom | Self::Both,
                rho_journal::PhoneFlickDirection::Up
            )
        )
    }
}

enum PhoneTransition {
    /// Boxed: a card is far larger than a sequence number, and the stack
    /// holds mostly the latter.
    Flick(Box<rho_dealer::Card>),
    Verdict(u64),
}

#[derive(Clone, Copy)]
struct PhoneSnap {
    generation: u64,
    from: Pixels,
    to: Pixels,
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

    fn direction(&self) -> Option<rho_journal::PhoneFlickDirection> {
        let dy = (self.position.y - self.start.y).as_f32();
        let dx = (self.position.x - self.start.x).as_f32();
        if dy.abs() < FLICK_SLOP || dy.abs() < dx.abs() * 1.25 {
            return None;
        }
        Some(if dy < 0. {
            rho_journal::PhoneFlickDirection::Up
        } else {
            rho_journal::PhoneFlickDirection::Down
        })
    }

    fn claims_touch(&self) -> bool {
        self.direction()
            .is_some_and(|direction| self.edge.permits(direction))
    }

    fn committed_direction(
        &self,
        viewport_height: Pixels,
    ) -> Option<rho_journal::PhoneFlickDirection> {
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
    snap: Option<PhoneSnap>,
    next_snap_generation: u64,
    transitions: Vec<PhoneTransition>,
    feed_surface: Option<(ContextId, SurfaceKey)>,
    stack: Vec<(ContextId, SurfaceKey)>,
    /// A card arrived while the feed sat empty. The feed is the deal, so it
    /// has to be opened again; only a redraw has the window to do it.
    pub(super) feed_retry: bool,
    pub(super) feed_focus: FocusHandle,
    /// Where a tap on rows went down, until it comes up.
    pending_tap: Option<Point<Pixels>>,
    /// A long press on prose is selecting: the finger extends what it
    /// took, and lifting asks what to do with it.
    selecting: bool,
    /// Where the menu sheet's rows are scrolled to, so the foot can say
    /// whether more lie below.
    sheet_scroll: gpui::ScrollHandle,
    /// The height the body was last drawn at. The keyboard takes the
    /// bottom of the window, and the composer has to follow the cursor
    /// up when it does.
    viewport_height: Pixels,
    /// The cards a flick would reach, read when the finger goes down so
    /// the one being dragged toward can show under the feed card. `next`
    /// is the dealer's next; `previous` is where a flick down would land.
    peek: PhonePeek,
}

#[derive(Default)]
struct PhonePeek {
    next: Option<rho_dealer::Card>,
    previous: Option<rho_dealer::Card>,
}

impl PhoneUi {
    pub(super) fn new(cx: &mut gpui::App) -> Self {
        let forced = std::env::var("RHO_PHONE").is_ok_and(|value| value == "1");
        Self {
            // The first render activates the projection. This keeps native
            // construction's seeded draft out of the phone history so
            // Home is always the permanent initial root, including with the
            // environment override.
            enabled: false,
            forced,
            touch_debug: std::env::var("RHO_PHONE_TOUCH_DEBUG").is_ok_and(|value| value == "1"),
            last_gesture: None,
            flick: None,
            drag_offset: Pixels::ZERO,
            snap: None,
            next_snap_generation: 1,
            transitions: Vec::new(),
            feed_surface: None,
            stack: Vec::new(),
            feed_retry: false,
            feed_focus: cx.focus_handle(),
            pending_tap: None,
            selecting: false,
            sheet_scroll: gpui::ScrollHandle::new(),
            viewport_height: Pixels::ZERO,
            peek: PhonePeek::default(),
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

    pub(super) fn remove_key(&mut self, key: &SurfaceKey) {
        self.stack.retain(|entry| &entry.1 != key);
    }

    pub(super) fn retain_contexts(&mut self, mut keep: impl FnMut(&ContextId) -> bool) {
        self.stack.retain(|entry| keep(&entry.0));
    }

    pub(super) fn touch_debug_enabled(&self) -> bool {
        self.touch_debug
    }

    fn record_flick(&mut self, direction: rho_journal::PhoneFlickDirection, moved_card: bool) {
        let direction = match direction {
            rho_journal::PhoneFlickDirection::Up => "up",
            rho_journal::PhoneFlickDirection::Down => "down",
        };
        let outcome = if moved_card { "moved" } else { "stayed" };
        self.last_gesture = Some(format!("flick {direction} · {outcome}"));
    }

    fn record_verdict(&mut self, verdict: rho_journal::PhoneVerdict) {
        let verdict = match verdict {
            rho_journal::PhoneVerdict::Done => "done",
            rho_journal::PhoneVerdict::Mute => "mute",
            rho_journal::PhoneVerdict::Defer => "defer",
            rho_journal::PhoneVerdict::Todo => "todo",
            rho_journal::PhoneVerdict::File => "file",
            rho_journal::PhoneVerdict::Reply => "reply",
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

/// How far a finger may drift and still be a tap on the row it went down on.
const TAP_SLOP: Pixels = px(8.);
const PHONE_FONT_SCALE: f32 = 1.4;
/// How long a phone toast stays: long enough to read and tap its undo.
pub(super) const PHONE_TOAST_DURATION: std::time::Duration = std::time::Duration::from_secs(5);

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
            if self.open_card_in_view(cx).is_some() {
                self.phone
                    .show_feed(self.active_context, self.active_surface().key.clone());
            } else {
                self.phone.feed_surface = None;
                cx.defer_in(window, |this, window, cx| this.pull_card(window, cx));
            }
            self.update_statuses(cx);
            self.phone_prose_font_everywhere(cx);
            if let Some(home) = self.home_view() {
                home.update(cx, |home, cx| home.set_narrow(true, cx));
            }
            // Deferred: adjusting fonts and settings notifies observers,
            // which must not reenter the draw that detected the transition.
            cx.defer(|cx| {
                theme_settings::adjust_buffer_font_size(cx, |size| size * PHONE_FONT_SCALE);
                theme_settings::adjust_ui_font_size(cx, |size| size * PHONE_FONT_SCALE);
                set_touch_modal_editing(false, cx);
            });
        }
        // The queue can be empty when the phone first draws: a Slack thread
        // becomes a node only once the mirror has synced. Without this the
        // feed would stay on "nothing needs attention" for the rest of the
        // session.
        if self.phone.enabled
            && std::mem::take(&mut self.phone.feed_retry)
            && self.phone.stack.is_empty()
            && self.open_card_in_view(cx).is_none()
        {
            cx.defer_in(window, |this, window, cx| this.pull_card(window, cx));
        }
        if change.exited {
            self.phone.flick = None;
            self.phone.drag_offset = Pixels::ZERO;
            self.phone.snap = None;
            self.update_statuses(cx);
            self.phone_prose_font_everywhere(cx);
            if let Some(home) = self.home_view() {
                home.update(cx, |home, cx| home.set_narrow(false, cx));
            }
            cx.defer(|cx| {
                theme_settings::reset_buffer_font_size(cx);
                theme_settings::reset_ui_font_size(cx);
                set_touch_modal_editing(true, cx);
            });
        }
        change.enabled
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
            if !matches!(surface.key, super::SurfaceKey::Transcript(_)) {
                return;
            }
            let super::SurfaceView::Transcript { model, editor } = &surface.view else {
                return;
            };
            let focus = if model.read(cx).selection_in_prompt(editor, cx) {
                editor.focus_handle(cx)
            } else {
                this.phone.feed_focus.clone()
            };
            window.focus(&focus, cx);
        });
    }

    #[cfg(test)]
    pub(crate) fn phone_feed_for_test(&mut self, cx: &mut Context<Self>) -> bool {
        self.phone.enabled && self.phone.stack.is_empty() && self.open_card_in_view(cx).is_some()
    }

    #[cfg(test)]
    pub(crate) fn phone_feed_is_active_for_test(&self) -> bool {
        self.phone_feed_is_active()
    }

    #[cfg(test)]
    pub(crate) fn phone_composer_focused_for_test(&self, window: &Window, cx: &App) -> bool {
        self.phone_composer_focused(window, cx)
    }

    /// Whether what is on screen is the feed card itself. A verdict closes
    /// it, so the surface that shows through afterwards is not the feed
    /// and the next pull does not pass it over.
    pub(super) fn phone_feed_is_active(&self) -> bool {
        self.phone
            .feed_surface
            .as_ref()
            .is_some_and(|(context, key)| {
                *context == self.active_context && self.active_surface().key == *key
            })
    }

    #[cfg(test)]
    pub(crate) fn phone_flick_for_test(
        &mut self,
        direction: rho_journal::PhoneFlickDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_phone_flick(direction, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn phone_last_gesture_for_test(&self) -> Option<&str> {
        self.phone.last_gesture.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn phone_remember_last_verdict_for_test(&mut self) {
        let sequence = self.attention.last_undo().unwrap();
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
        self.show_history_surface(context, surface);
        self.sync_selection_to_focus(cx);
        self.focus_phone_feed(window, cx);
    }

    pub(crate) fn phone_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() {
            self.minibuffer_cancel(window, cx);
            return;
        }

        if self.phone.stack.is_empty() && self.open_card_in_view(cx).is_some() {
            self.restore_phone_feed(window, cx);
            cx.notify();
            return;
        }

        self.phone.stack.pop();
        let next = loop {
            let Some((context, key)) = self.phone.stack.last().cloned() else {
                break None;
            };
            let valid = self
                .surfaces
                .get(&context)
                .is_some_and(|surfaces| surfaces.iter().any(|surface| surface.key == key));
            if valid {
                break Some((context, key));
            }
            self.phone.stack.pop();
        };
        let Some((context, key)) = next else {
            if self.open_card_in_view(cx).is_some() {
                self.restore_phone_feed(window, cx);
                cx.notify();
            } else {
                self.pull_card(window, cx);
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
            self.show_history_surface(context, surface);
            self.sync_selection_to_focus(cx);
            self.focus_active_surface(window, cx);
        }
        cx.notify();
    }

    fn phone_deal_scroll_edge(&mut self, cx: &mut Context<Self>) -> PhoneScrollEdge {
        let editor = match &self.active_surface().view {
            super::SurfaceView::Note(editor) | super::SurfaceView::Transcript { editor, .. } => {
                Some(editor.clone())
            }
            super::SurfaceView::SlackConversation(view) => Some(view.read(cx).editor().clone()),
            _ => None,
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
                    && self.phone.snap.is_none()
                    && self.phone.stack.is_empty()
                    && (self.open_card_in_view(cx).is_some() || !self.phone.transitions.is_empty())
                    && self.minibuffer.is_none()
                    // A sheet is open: the finger scrolls it, not the deal
                    // under it.
                    && self.menu_buffer.is_none()
                {
                    let edge = if self.open_card_in_view(cx).is_some() {
                        self.phone_deal_scroll_edge(cx)
                    } else {
                        PhoneScrollEdge::Both
                    };
                    self.phone.flick = Some(PhoneFlickGesture::new(event, edge));
                    self.phone.peek = self.phone_peek(cx);
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
                    let offset = self
                        .phone
                        .flick
                        .as_ref()
                        .map_or(Pixels::ZERO, |flick| flick.position.y - flick.start.y);
                    // Home is the last screen: nothing lies past it to pull up.
                    if self.open_card_in_view(cx).is_none() {
                        offset.max(Pixels::ZERO)
                    } else {
                        offset
                    }
                } else {
                    Pixels::ZERO
                };
                if claims {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                let from = self.phone.drag_offset;
                let direction = self.phone.flick.take().and_then(|mut flick| {
                    (flick.id == event.id && event.phase == TouchPhase::Ended).then(|| {
                        flick.update(event);
                        flick.committed_direction(window.viewport_size().height)
                    })?
                });
                // Nothing lies past Home, so a flick up there is a drag let go.
                let direction = direction.filter(|direction| {
                    *direction == rho_journal::PhoneFlickDirection::Down
                        || self.open_card_in_view(cx).is_some()
                });
                self.phone.drag_offset = Pixels::ZERO;
                if let Some(direction) = direction {
                    window.prevent_default();
                    cx.stop_propagation();
                    let to = match direction {
                        rho_journal::PhoneFlickDirection::Up => -window.viewport_size().height,
                        rho_journal::PhoneFlickDirection::Down => window.viewport_size().height,
                    };
                    self.start_phone_snap(from, to, Some(direction), window, cx);
                } else if from != Pixels::ZERO {
                    self.start_phone_snap(from, Pixels::ZERO, None, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// What a flick from here would land on, read once per gesture: the
    /// dealer ranks the world to answer, and a drag asks every frame.
    fn phone_peek(&mut self, cx: &mut Context<Self>) -> PhonePeek {
        let current = self.open_card_in_view(cx);
        let next = self
            .hand(cx)
            .top(current.as_ref().map(|card| &card.node))
            .cloned();
        let previous = match self.phone.transitions.last() {
            Some(PhoneTransition::Flick(card)) => Some((**card).clone()),
            Some(PhoneTransition::Verdict(sequence))
                if self.attention.last_undo() == Some(*sequence) =>
            {
                self.attention
                    .last_undo_card()
                    .cloned()
                    .map(|node| self.card_for(&node, cx))
            }
            Some(PhoneTransition::Verdict(_)) | None => None,
        };
        PhonePeek { next, previous }
    }

    /// The card a drag is pulling in, drawn as its header and bar over an
    /// empty body: the real surface opens when the flick lands, and this
    /// is what makes the landing look like the card arriving rather than
    /// appearing. The Home glance stands in when the queue is empty.
    fn render_phone_peek(
        &mut self,
        card: Option<&rho_dealer::Card>,
        text_style: &gpui::TextStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors();
        let title = match card {
            Some(card) => self.render_phone_card_title(card, text_style, window, cx),
            None => div().child("home").into_any_element(),
        };
        let header = self.render_phone_header(false, title, cx);
        let bar = card.map(|_| self.render_phone_verdict_bar(cx));
        div()
            .id("phone-peek-card")
            .absolute()
            .left_0()
            .w_full()
            .h(window.viewport_size().height)
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .bg(colors.editor_background),
            )
            .children(bar)
    }

    /// Where the peeked card sits for a feed card at `offset`: just below
    /// the screen when the finger is pulling up, just above it when
    /// pulling down. `None` when nothing is there to pull.
    fn phone_peek_offset(&self, offset: Pixels, height: Pixels) -> Option<(Pixels, bool)> {
        if offset < Pixels::ZERO {
            Some((offset + height, true))
        } else if offset > Pixels::ZERO && self.phone.peek.previous.is_some() {
            Some((offset - height, false))
        } else {
            None
        }
    }

    fn start_phone_snap(
        &mut self,
        from: Pixels,
        to: Pixels,
        direction: Option<rho_journal::PhoneFlickDirection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = direction.and_then(|_| self.open_card_in_view(cx).map(|card| card.node));
        let generation = self.phone.next_snap_generation;
        self.phone.next_snap_generation = self.phone.next_snap_generation.wrapping_add(1);
        self.phone.snap = Some(PhoneSnap {
            generation,
            from,
            to,
        });
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SNAP_DURATION).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this
                    .phone
                    .snap
                    .is_none_or(|snap| snap.generation != generation)
                {
                    return;
                }
                this.phone.snap = None;
                if let Some(direction) = direction
                    && this.phone.enabled
                    && this.phone.stack.is_empty()
                    && this.card_in_view(cx).map(|card| card.node) == source
                {
                    this.commit_phone_flick(direction, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn commit_phone_flick(
        &mut self,
        direction: rho_journal::PhoneFlickDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.open_card_in_view(cx);
        match direction {
            rho_journal::PhoneFlickDirection::Up => self.pull_card(window, cx),
            rho_journal::PhoneFlickDirection::Down => match self.phone.transitions.pop() {
                Some(PhoneTransition::Flick(card)) => {
                    // Flicking back is taking the skip back: the card is the
                    // one to look at again, so it opens as it was.
                    self.attention.skips.clear(&card.node);
                    self.open_card(*card, window, cx);
                    self.invalidate_dealer_signals(cx);
                }
                Some(PhoneTransition::Verdict(sequence))
                    if self.attention.last_undo() == Some(sequence) =>
                {
                    self.undo_verdict(window, cx)
                }
                Some(PhoneTransition::Verdict(_)) | None => {}
            },
        }
        let after = self.open_card_in_view(cx);
        let moved_card =
            before.as_ref().map(|card| &card.node) != after.as_ref().map(|card| &card.node);
        if direction == rho_journal::PhoneFlickDirection::Up
            && moved_card
            && let Some(card) = before
        {
            self.phone
                .transitions
                .push(PhoneTransition::Flick(Box::new(card)));
        }
        self.record_phone_flick(direction, moved_card, cx);
    }

    pub(super) fn record_phone_flick(
        &mut self,
        direction: rho_journal::PhoneFlickDirection,
        moved_card: bool,
        cx: &mut Context<Self>,
    ) {
        self.phone.record_flick(direction, moved_card);
        rho_journal::record(rho_journal::Event::PhoneFlick {
            direction,
            moved_card,
        });
        cx.notify();
    }

    pub(super) fn record_phone_verdict(
        &mut self,
        verdict: rho_journal::PhoneVerdict,
        cx: &mut Context<Self>,
    ) {
        self.phone.record_verdict(verdict);
        rho_journal::record(rho_journal::Event::PhoneVerdict { verdict });
        cx.notify();
    }

    pub(super) fn render_phone_touch_debug(&self, contacts: usize) -> Option<AnyElement> {
        self.phone.touch_debug.then(|| {
            div()
                .id("phone-touch-debug")
                .absolute()
                .top(PHONE_HEADER_HEIGHT + px(4.))
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

    /// A card's two-line title: where it is, then what state it is in.
    fn render_phone_card_title(
        &self,
        card: &rho_dealer::Card,
        text_style: &gpui::TextStyle,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let label = card.label.clone();
        let (breadcrumb, _) = {
            let font_size = text_style.font_size.to_pixels(window.rem_size());
            let font = text_style.font();
            phone_deal_header_text(
                &Self::card_path(card),
                "",
                window.viewport_size().width - PHONE_HEADER_BUTTONS_WIDTH,
                |text| {
                    window
                        .text_system()
                        .shape_line(
                            text.into(),
                            font_size,
                            &[gpui::TextRun {
                                len: text.len(),
                                font: font.clone(),
                                color: text_style.color,
                                ..Default::default()
                            }],
                            None,
                        )
                        .width
                },
            )
        };
        let colors = cx.theme().colors();
        let title = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .justify_center()
            .child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(colors.text)
                    .child(breadcrumb),
            )
            .child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(13.))
                    .line_height(px(16.))
                    .text_color(cx.theme().status().warning)
                    .child(label),
            );
        title.into_any_element()
    }

    /// The feed screen under the finger: it follows a drag, and on release
    /// it snaps home or off the screen while the card it pulled in slides
    /// in behind it, so every flick is the same move a short video feed
    /// makes.
    fn render_phone_stage(
        &mut self,
        screen: gpui::Stateful<gpui::Div>,
        text_style: &gpui::TextStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let height = window.viewport_size().height;
        let stage = div().id("phone-feed-stage").size_full().relative();
        if let Some(snap) = self.phone.snap {
            let peek = self
                .phone_peek_offset(snap.from + snap.to, height)
                .map(|(_, forward)| {
                    let card = match forward {
                        true => self.phone.peek.next.clone(),
                        false => self.phone.peek.previous.clone(),
                    };
                    let shift = if forward { height } else { -height };
                    self.render_phone_peek(card.as_ref(), text_style, window, cx)
                        .with_animation(
                            ("phone-peek-snap", snap.generation),
                            Animation::new(SNAP_DURATION).with_easing(ease_out_quint()),
                            move |peek, delta| {
                                let from = snap.from.as_f32();
                                let to = snap.to.as_f32();
                                peek.top(px(from + (to - from) * delta) + shift)
                            },
                        )
                });
            stage
                .children(peek)
                .child(screen.with_animation(
                    ("phone-card-snap", snap.generation),
                    Animation::new(SNAP_DURATION).with_easing(ease_out_quint()),
                    move |screen, delta| {
                        let from = snap.from.as_f32();
                        let to = snap.to.as_f32();
                        screen.top(px(from + (to - from) * delta))
                    },
                ))
                .into_any_element()
        } else {
            let offset = self.phone.drag_offset;
            let peek = self
                .phone_peek_offset(offset, height)
                .map(|(top, forward)| {
                    let card = match forward {
                        true => self.phone.peek.next.clone(),
                        false => self.phone.peek.previous.clone(),
                    };
                    self.render_phone_peek(card.as_ref(), text_style, window, cx)
                        .top(top)
                });
            stage
                .children(peek)
                .child(screen.top(offset))
                .into_any_element()
        }
    }

    /// Every phone screen is the same frame: a title bar, the surface, and
    /// a bottom bar. The title bar carries the way around — back on a
    /// stacked surface, the menu and the surface's own actions everywhere
    /// — so the bottom bar never has to, and its slots never change what
    /// they mean between one screen and the next.
    pub(super) fn render_phone_body(
        &mut self,
        text_style: &gpui::TextStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let composing = self.phone_composer_focused(window, cx);
        let height = window.viewport_size().height;
        if self.phone.viewport_height != height {
            self.phone.viewport_height = height;
            // The keyboard came up under the composer: the line being typed
            // has to stay above it, wherever the window's new bottom is.
            if composing {
                self.active_editor(cx).update(cx, |editor, cx| {
                    editor.request_autoscroll(editor::scroll::Autoscroll::fit(), cx);
                });
            }
        }
        if self.phone.stack.is_empty()
            && let Some(card) = self.open_card_in_view(cx)
        {
            let title = self.render_phone_card_title(&card, text_style, window, cx);
            let colors = cx.theme().colors();
            let header = self.render_phone_header(false, title, cx);
            let body = self.render_surface(&self.active_surface().clone());
            let bar = if composing {
                self.render_phone_composer_bar(cx)
            } else {
                self.render_phone_verdict_bar(cx)
            };
            let card = div()
                .id("phone-deal-card")
                .track_focus(&self.phone.feed_focus)
                .size_full()
                .relative()
                .flex()
                .flex_col()
                .child(header)
                .child(
                    div()
                        .id("phone-deal-body")
                        .capture_touch(cx.listener(Self::phone_touch))
                        .capture_any_mouse_down(cx.listener(Self::phone_pointer_down))
                        .capture_any_mouse_up(cx.listener(Self::phone_pointer_up))
                        .on_mouse_move(cx.listener(Self::phone_pointer_move))
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .bg(colors.editor_background)
                        .child(body),
                )
                .child(bar);
            return self.render_phone_stage(card, text_style, window, cx);
        }
        if self.phone.stack.is_empty() {
            let colors = cx.theme().colors();
            // The header names what the reader is looking at, and with the
            // queue empty that is Home, not the deal they have already
            // flicked past.
            let header =
                self.render_phone_header(false, div().child("home").into_any_element(), cx);
            let home = div()
                .id("phone-feed-empty")
                .track_focus(&self.phone.feed_focus)
                .size_full()
                .relative()
                .flex()
                .flex_col()
                .child(header)
                .child({
                    // Home is the card after the last deal: flick past the
                    // queue and the glance is what is left.
                    let body = div()
                        .id("phone-feed-empty-body")
                        .capture_touch(cx.listener(Self::phone_touch))
                        .capture_any_mouse_down(cx.listener(Self::phone_pointer_down))
                        .capture_any_mouse_up(cx.listener(Self::phone_pointer_up))
                        .on_mouse_move(cx.listener(Self::phone_pointer_move))
                        .flex_1()
                        .min_h_0()
                        .w_full();
                    match self.home_view() {
                        Some(view) => body.child(view).into_any_element(),
                        None => body
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(colors.text_muted)
                            .child("nothing needs attention")
                            .into_any_element(),
                    }
                });
            return self.render_phone_stage(home, text_style, window, cx);
        }
        let Some(surface) = self.phone_surface() else {
            return div()
                .id("phone-dashboard")
                .size_full()
                .flex()
                .flex_col()
                .child(self.render_phone_header(true, div().into_any_element(), cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .track_focus(&self.phone.feed_focus),
                )
                .into_any_element();
        };
        let title = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(self.surface_title(cx))
            .into_any_element();
        let header = self.render_phone_header(true, title, cx);
        div()
            .id("phone-surface")
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("phone-surface-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .capture_any_mouse_down(cx.listener(Self::phone_pointer_down))
                    .capture_any_mouse_up(cx.listener(Self::phone_pointer_up))
                    .on_mouse_move(cx.listener(Self::phone_pointer_move))
                    .capture_any_mouse_down(cx.listener(Self::phone_surface_pointer_down))
                    .child(self.render_surface(&surface)),
            )
            .children(composing.then(|| self.render_phone_composer_bar(cx)))
            .into_any_element()
    }

    /// The title bar: back when there is something under this surface,
    /// the title, then the menu and the surface's own actions. The two
    /// buttons on the right are in the same place on every screen.
    fn render_phone_header(
        &self,
        stacked: bool,
        title: AnyElement,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let button = |id: &'static str, glyph: &'static str| {
            div()
                .id(id)
                .cursor_pointer()
                .flex_none()
                .h_full()
                .w(PHONE_HEADER_BUTTON_WIDTH)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(22.))
                .child(glyph)
        };
        div()
            .id("phone-header")
            .flex_none()
            .h(PHONE_HEADER_HEIGHT)
            .w_full()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(colors.border_variant)
            .text_color(colors.text_muted)
            .when(stacked, |header| {
                header.child(
                    button("phone-back", "‹")
                        .on_click(cx.listener(|this, _, window, cx| this.phone_back(window, cx))),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .when(!stacked, |title| title.pl_3())
                    .child(title),
            )
            .child(
                button("phone-menu", "☰").on_click(cx.listener(|this, _, window, cx| {
                    let subject = this.subject(window, cx);
                    this.open_menu(crate::transient::phone_root_menu(&subject), window, cx);
                })),
            )
            .child(
                button("phone-more", "⋯").on_click(cx.listener(|this, _, window, cx| {
                    this.open_phone_context_menu(window, cx);
                })),
            )
            .into_any_element()
    }

    /// Whether what is on screen is rows rather than text: a tap picks a
    /// row, the way `enter` opens the one the cursor is on.
    fn phone_surface_is_rows(&self) -> bool {
        matches!(
            self.active_surface().key,
            SurfaceKey::Home
                | SurfaceKey::SlackList
                | SurfaceKey::SlackResults { .. }
                | SurfaceKey::SlackInventory(_)
                | SurfaceKey::Note(_)
                | SurfaceKey::Draft
        )
    }

    /// Surfaces that are read: a long press on them selects the word under
    /// the finger rather than asking for a row's menu.
    fn phone_surface_is_prose(&self) -> bool {
        matches!(
            self.active_surface().key,
            SurfaceKey::Transcript(_)
                | SurfaceKey::Activity(_)
                | SurfaceKey::SlackConversation(_)
                | SurfaceKey::Note(_)
                | SurfaceKey::Messages
                | SurfaceKey::Draft
                | SurfaceKey::File { .. }
        )
    }

    /// The editor a surface reads in, if what it shows is prose: on the
    /// phone that is set in the proportional face, which fits more of a
    /// sentence on a narrow line. Home is the cards' titles, so it reads
    /// the same way. Files are code and keep the buffer face, as do the
    /// Slack lists, whose rows line up in columns; code inside prose keeps
    /// it through its highlight.
    pub(super) fn phone_prose_font(&self, surface: &Surface, cx: &mut App) {
        let editor = match &surface.view {
            super::SurfaceView::Note(editor) | super::SurfaceView::Messages(editor) => {
                editor.clone()
            }
            super::SurfaceView::Draft { editor }
            | super::SurfaceView::Transcript { editor, .. } => editor.clone(),
            super::SurfaceView::SlackConversation(view) => view.read(cx).editor().clone(),
            super::SurfaceView::Home(view) => view.read(cx).editor().clone(),
            _ => return,
        };
        let family = self
            .phone
            .enabled
            .then(|| rho_window::style::PROSE_FONT_FAMILY.into());
        editor.update(cx, |editor, _| {
            editor.set_text_style_refinement(gpui::TextStyleRefinement {
                font_family: family,
                ..Default::default()
            });
        });
    }

    fn phone_prose_font_everywhere(&self, cx: &mut App) {
        let surfaces: Vec<Surface> = self.surfaces.values().flatten().cloned().collect();
        for surface in &surfaces {
            self.phone_prose_font(surface, cx);
        }
    }

    /// A tap in the draft: a header field asks its question as a prompt
    /// with the same candidates the desktop completes from, since a phone
    /// has no completion popup worth the name; the body takes the keyboard.
    fn phone_draft_tap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.active_editor(cx);
        window.focus(&editor.focus_handle(cx), cx);
        let draft = self.draft_model.read(cx);
        use rho_agents_view::draft::DraftModel;
        type Apply = fn(&mut DraftModel, &str, &mut Context<DraftModel>);
        let (prompt, complete, apply): (&str, crate::minibuffer::CandidateSource, Apply) = if draft
            .cursor_in_start_field(&editor, cx)
        {
            (
                "on top of:",
                std::rc::Rc::new(|workspace: &Workspace, input: &str, _: &App| {
                    crate::commands::start_field_candidates(input, &workspace.live_agent_targets())
                }),
                |draft, text, cx| draft.set_start_text(text, cx),
            )
        } else if draft.cursor_in_role_field(&editor, cx) {
            (
                "role:",
                std::rc::Rc::new(|_: &Workspace, input: &str, _: &App| {
                    crate::commands::role_field_candidates(input)
                }),
                |draft, text, cx| draft.set_role_text(text, cx),
            )
        } else if draft.cursor_in_a_field(&editor, cx) {
            (
                "workdir:",
                std::rc::Rc::new(|workspace: &Workspace, input: &str, _: &App| {
                    crate::commands::workdir_field_candidates(
                        input,
                        &workspace.hosts.workdir_table(),
                    )
                }),
                |draft, text, cx| draft.set_workdir_text(text, cx),
            )
        } else {
            return;
        };
        // The prompt starts empty so every choice is on offer; an empty
        // answer keeps what the field already says.
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  _: &mut Window,
                  cx: &mut Context<Workspace>| {
                if input.trim().is_empty() {
                    return;
                }
                workspace
                    .draft_model
                    .update(cx, |draft, cx| apply(draft, input.trim(), cx));
            },
        );
        self.open_prompt(prompt, complete, on_submit, window, cx);
        self.set_prompt_complete_whole_input();
    }

    /// Puts the active editor's cursor under the finger. The editor would
    /// do this for a mouse, but rho keeps click selection off in most
    /// buffers, and a row list on the phone still has to know which row was
    /// touched.
    fn phone_place_cursor(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.active_editor(cx);
        editor.update(cx, |editor, cx| {
            if let Some((anchor, _, _)) =
                editor.buffer_location_for_window_position(position, text::Bias::Left)
            {
                editor.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| selections.select_anchor_ranges([anchor..anchor]),
                );
            }
        });
    }

    /// A touch arrives as the click gpui makes of it, and the editor
    /// underneath would take both kinds: a tap is swallowed where click
    /// selection is off, and a long press is answered with a desktop
    /// context menu. Both are caught on the way down instead. A long press
    /// is the row's own menu, with the cursor moved onto the row first so
    /// the menu is that row's; a tap on rows moves the cursor too and is
    /// finished on the way up.
    fn phone_pointer_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.phone.pending_tap = None;
        if self.minibuffer.is_some() || self.menu_buffer.is_some() {
            return;
        }
        match event.button {
            MouseButton::Right => {
                cx.stop_propagation();
                if self.phone_surface_is_prose() {
                    // The finger is selecting now, not flicking.
                    self.phone.flick = None;
                    self.phone.drag_offset = Pixels::ZERO;
                    let position = event.position;
                    let selecting = self.active_editor(cx).update(cx, |editor, cx| {
                        editor.begin_touch_selection(position, window, cx)
                    });
                    if selecting {
                        self.phone.selecting = true;
                        cx.notify();
                        return;
                    }
                }
                self.phone_place_cursor(event.position, window, cx);
                cx.defer_in(window, |this, window, cx| this.phone_row_menu(window, cx));
            }
            MouseButton::Left if self.phone_surface_is_rows() => {
                self.phone_place_cursor(event.position, window, cx);
                self.phone.pending_tap = Some(event.position);
            }
            _ => {}
        }
    }

    /// The finger that pressed long is still down and moving: the
    /// selection follows it, a word at a time, the way a drag after a
    /// double click does on the desk.
    fn phone_pointer_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.phone.selecting || event.pressed_button != Some(MouseButton::Right) {
            return;
        }
        let position = event.position;
        self.active_editor(cx).update(cx, |editor, cx| {
            editor.extend_touch_selection(position, window, cx);
        });
    }

    /// The tap that began on a row: `enter` on it, the same row the same
    /// key opens. A finger that moved away is not a tap. The finger that
    /// was selecting lifts: the selection is done and the sheet asks what
    /// to do with it.
    fn phone_pointer_up(
        &mut self,
        event: &gpui::MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Right && std::mem::take(&mut self.phone.selecting) {
            cx.stop_propagation();
            self.active_editor(cx).update(cx, |editor, cx| {
                editor.end_touch_selection(window, cx);
            });
            cx.defer_in(window, |this, window, cx| {
                this.open_phone_selection_menu(window, cx)
            });
            return;
        }
        let Some(down) = self.phone.pending_tap.take() else {
            return;
        };
        if event.button != MouseButton::Left
            || (event.position - down).magnitude() > TAP_SLOP.as_f32() as f64
        {
            return;
        }
        cx.defer_in(window, move |this, window, cx| {
            match &this.active_surface().key {
                SurfaceKey::Home => this.home_open_row(window, cx),
                SurfaceKey::SlackList => this.slack_open_row(window, cx),
                SurfaceKey::SlackResults { .. } | SurfaceKey::SlackInventory(_) => {
                    this.slack_open_found(window, cx);
                }
                SurfaceKey::Note(_) => {
                    this.note_open_row(window, cx);
                }
                SurfaceKey::Draft => this.phone_draft_tap(window, cx),
                _ => {}
            }
            cx.notify();
        });
    }

    /// A long press: the menu for the thing under the finger. A message
    /// has its own; everything else answers with what `⋯` would.
    fn phone_row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.active_surface().view,
            super::SurfaceView::SlackConversation(_)
        ) && self.prompt_slack_message_actions(window, cx)
        {
            return;
        }
        // A Home row is a card: holding it asks for its verdicts.
        if self.home_in_view()
            && self.card_in_view(cx).is_some()
            && self.open_verdict_transient(window, cx)
        {
            return;
        }
        self.open_phone_context_menu(window, cx);
    }

    /// What a selection can do, then what the message it is in can: a
    /// Slack message's own actions follow on the same sheet, so holding a
    /// message still reaches them.
    fn open_phone_selection_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut menu = crate::transient::selection_menu();
        if let super::SurfaceView::SlackConversation(view) = &self.active_surface().view
            && let Some(actions) = view.clone().update(cx, |view, cx| view.message_actions(cx))
        {
            menu = menu.append(crate::transient::slack_message_menu(&actions));
        }
        self.open_menu(menu, window, cx);
    }

    /// `⋯`: what can be done with the thing on screen. The card in the
    /// feed answers with the verdicts, an agent with its own menu, Slack
    /// with Slack's; anything else gets the root menu, which reads the
    /// subject and shows what applies.
    pub(super) fn open_phone_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phone.stack.is_empty()
            && self.open_card_in_view(cx).is_some()
            && self.open_verdict_transient(window, cx)
        {
            return;
        }
        // The desk reaches every command with `:` from any menu; the phone
        // has no key for that, so the subject's own menu ends with the row
        // the root menu has.
        let everything = |menu: crate::transient::Menu| {
            menu.item(
                ":",
                "all commands…",
                crate::transient::MenuAction::Command(crate::transient::Command::Palette),
            )
        };
        let menu = match &self.active_surface().key {
            SurfaceKey::Transcript(_) | SurfaceKey::Activity(_) => {
                everything(crate::transient::agent_menu())
            }
            SurfaceKey::SlackList
            | SurfaceKey::SlackResults { .. }
            | SurfaceKey::SlackInventory(_)
            | SurfaceKey::SlackConversation(_) => everything(crate::transient::slack_menu()),
            _ => {
                let subject = self.subject(window, cx);
                crate::transient::root_menu(&subject)
            }
        };
        self.open_menu(menu, window, cx);
    }

    /// Whether the reader is typing into the surface: its editor has the
    /// focus rather than the feed. The bottom bar becomes the composer's
    /// while they are, on every kind of surface.
    fn phone_composer_focused(&self, window: &Window, cx: &App) -> bool {
        if self.minibuffer.is_some() || self.menu_buffer.is_some() {
            return false;
        }
        let (editor, in_composer) = match &self.active_surface().view {
            super::SurfaceView::Draft { editor } => (editor.clone(), true),
            super::SurfaceView::Transcript { model, editor } => (
                editor.clone(),
                model.read(cx).selection_in_prompt(editor, cx),
            ),
            super::SurfaceView::SlackConversation(view) => {
                let view = view.read(cx);
                (view.editor().clone(), view.selection_in_compose(cx))
            }
            _ => return false,
        };
        let focused = editor.focus_handle(cx).is_focused(window);
        in_composer && focused
    }

    /// The feed takes the keyboard back, unless a prompt or a menu holds
    /// it: what they asked is still being answered.
    pub(super) fn focus_phone_feed(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() || self.menu_buffer.is_some() {
            return;
        }
        window.focus(&self.phone.feed_focus, cx);
    }

    /// The echo, as a toast above the bar rather than a line the phone
    /// has no room for. A verdict's toast carries its undo, so taking a
    /// verdict back is one tap on the thing that announced it.
    pub(super) fn render_phone_toast(
        &self,
        text_style: &gpui::TextStyle,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let echo = self.echo.as_ref()?;
        if self.minibuffer.is_some() || self.menu_buffer.is_some() {
            return None;
        }
        let colors = cx.theme().colors();
        let undo = match self.phone.transitions.last() {
            Some(PhoneTransition::Verdict(sequence))
                if self.attention.last_undo() == Some(*sequence) =>
            {
                Some(
                    div()
                        .id("phone-toast-undo")
                        .cursor_pointer()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(colors.element_background)
                        .text_color(colors.text_accent)
                        .child("undo")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.undo_verdict(window, cx);
                            cx.stop_propagation();
                        })),
                )
            }
            _ => None,
        };
        let text = echo.text().lines().next().unwrap_or_default().to_owned();
        Some(
            div()
                .id("phone-toast")
                .absolute()
                .left_3()
                .right_3()
                .bottom(TARGET_HEIGHT + px(12.))
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(colors.elevated_surface_background)
                .border_1()
                .border_color(colors.border)
                .text_color(text_style.color)
                .font_family(text_style.font_family.clone())
                .text_size(text_style.font_size)
                .child(div().flex_1().min_w_0().overflow_hidden().child(text))
                .children(undo)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.echo = None;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    pub(super) fn phone_completed_verdict(&mut self, sequence: u64) {
        self.phone
            .transitions
            .push(PhoneTransition::Verdict(sequence));
    }

    pub(crate) fn phone_snap_in_progress(&self) -> bool {
        self.phone.snap.is_some()
    }

    fn dispatch_phone_verdict(
        &mut self,
        verdict: rho_journal::PhoneVerdict,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.phone_verdict_with(
            verdict,
            move |_, window, cx| window.dispatch_action(action, cx),
            window,
            cx,
        );
    }

    /// The bookkeeping around a verdict taken by thumb, for the buttons
    /// whose verdict is a call rather than an action: the snooze chips,
    /// which each name their own time.
    pub(crate) fn phone_verdict_with(
        &mut self,
        verdict: rho_journal::PhoneVerdict,
        run: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.phone.snap.is_some() {
            return;
        }
        let before = self.open_card_in_view(cx).map(|card| card.node);
        let undo_before = self.attention.last_undo();
        run(self, window, cx);
        cx.defer_in(window, move |this, _window, cx| {
            let after = this.open_card_in_view(cx).map(|card| card.node);
            if before.is_some() && before != after {
                // A verdict that wrote a cell finished inside `run` and
                // told the phone itself; what is left here is the one that
                // only moved the card, which still owes the strip its
                // transition and its record.
                if let Some(sequence) = this.attention.last_undo()
                    && Some(sequence) != undo_before
                {
                    this.phone
                        .transitions
                        .push(PhoneTransition::Verdict(sequence));
                }
                this.record_phone_verdict(verdict, cx);
            }
        });
    }

    fn phone_bar_item(
        id: &'static str,
        icon: &'static str,
        label: &'static str,
        colors: &theme::ThemeColors,
    ) -> gpui::Stateful<gpui::Div> {
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
            .gap_0p5()
            .text_color(colors.text_muted)
            .child(div().text_size(px(22.)).line_height(px(24.)).child(icon))
            .child(div().text_size(px(12.)).line_height(px(14.)).child(label))
    }

    fn phone_bar(colors: &theme::ThemeColors) -> gpui::Stateful<gpui::Div> {
        div()
            .id("phone-bottom-bar")
            .flex_none()
            .h(TARGET_HEIGHT)
            .w_full()
            .flex()
            .items_stretch()
            .border_t_1()
            .border_color(colors.border_variant)
    }

    /// The verdicts, under the card in the feed and nowhere else. The rest
    /// of them — file, pile, name, wrong card, undo — are one tap further,
    /// under `⋯`.
    fn render_phone_verdict_bar(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        Self::phone_bar(colors)
            .child(
                Self::phone_bar_item("phone-verdict-done", "✓", "done", colors).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            rho_journal::PhoneVerdict::Done,
                            Box::new(crate::DealDone),
                            window,
                            cx,
                        );
                    }),
                ),
            )
            .child(
                Self::phone_bar_item("phone-verdict-mute", "×", "mute", colors).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            rho_journal::PhoneVerdict::Mute,
                            Box::new(crate::DealMute),
                            window,
                            cx,
                        );
                    }),
                ),
            )
            .child(
                // Defer asks how long: the same question the `s` operator's
                // unit answers on a keyboard.
                Self::phone_bar_item("phone-verdict-defer", "◷", "defer", colors).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.open_menu(crate::transient::snooze_sheet(), window, cx);
                    }),
                ),
            )
            .child(
                Self::phone_bar_item("phone-verdict-todo", "○", "todo", colors).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            rho_journal::PhoneVerdict::Todo,
                            Box::new(crate::DealTodo),
                            window,
                            cx,
                        );
                    }),
                ),
            )
            .child(
                Self::phone_bar_item("phone-verdict-more", "⋯", "more", colors).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.open_verdict_transient(window, cx);
                    }),
                ),
            )
            .into_any_element()
    }

    /// The bar while the reader is writing: put the keyboard away, or
    /// send. It replaces whatever bar the screen had, so a send is never
    /// where a verdict was a moment ago while the keyboard is up.
    fn render_phone_composer_bar(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        Self::phone_bar(colors)
            .child(
                Self::phone_bar_item("phone-compose-done", "⌄", "keyboard", colors).on_click(
                    cx.listener(|this, _, window, cx| {
                        window.focus(&this.phone.feed_focus, cx);
                        cx.notify();
                    }),
                ),
            )
            .child(
                Self::phone_bar_item("phone-send", "↑", "send", colors)
                    .text_color(colors.text_accent)
                    .on_click(cx.listener(|this, _, window, cx| this.phone_send(window, cx))),
            )
            .into_any_element()
    }

    fn phone_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() {
            self.minibuffer_confirm(window, cx);
            return;
        }
        match &self.active_surface().view {
            super::SurfaceView::Draft { .. } | super::SurfaceView::Transcript { .. } => {
                self.submit_prompt(&crate::SubmitPrompt, window, cx)
            }
            super::SurfaceView::SlackConversation(view) => {
                // The answer says what Slack made of it, which the journal
                // wants and the phone has nowhere to put. Dropping it drops
                // the answer, not the message: the write is detached inside
                // `submit`.
                let view = view.clone();
                drop(view.update(cx, |view, cx| view.submit(cx)));
            }
            _ => {}
        }
    }

    /// The open menu, drawn as a sheet: the same menu the desk draws as a
    /// block under the point, with its rows as targets a thumb can hit.
    pub(super) fn render_phone_menu_sheet(
        &self,
        text_style: &gpui::TextStyle,
        viewport_height: Pixels,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let crate::workspace::MenuSheet {
            title,
            rows,
            has_back: has_parent,
        } = self.menu_sheet()?;
        let colors = cx.theme().colors();
        // More rows than the sheet shows: say so at its foot until the
        // reader has scrolled to the last one, or it is never found. The
        // header is one row's height.
        let scroll = &self.phone.sheet_scroll;
        let overflows = TARGET_HEIGHT * (rows.len() + 1) as f32
            > viewport_height * SHEET_MAX_HEIGHT
            && -scroll.offset().y < scroll.max_offset().y - px(1.);

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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.menu_dismiss(window, cx);
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_menu(window, cx);
                        cx.stop_propagation();
                    })),
            );

        let rows = rows.into_iter().enumerate().map(
            |(index, crate::workspace::MenuRow { description, value })| {
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
                    this.run_menu_at(index, window, cx);
                    cx.stop_propagation();
                }))
            },
        );

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
                // The same press does the same thing whether the reader is
                // looking at a block on the desk or a sheet here, and a tap
                // outside dismisses it either way.
                .on_key_down(cx.listener(Workspace::menu_key))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.close_menu(window, cx);
                }))
                .child(
                    div()
                        .id("phone-sheet")
                        .max_h(gpui::relative(SHEET_MAX_HEIGHT))
                        .w_full()
                        .flex()
                        .flex_col()
                        .bg(background)
                        .text_color(text_style.color)
                        .font_family(text_style.font_family.clone())
                        .font_weight(text_style.font_weight)
                        .text_size(text_style.font_size)
                        .line_height(text_style.line_height)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .id("phone-sheet-rows")
                                .flex_1()
                                .min_h_0()
                                .w_full()
                                .overflow_y_scroll()
                                .track_scroll(scroll)
                                .child(header)
                                .children(rows),
                        )
                        .children(overflows.then(|| {
                            div()
                                .id("phone-sheet-more")
                                .w_full()
                                .py_1()
                                .text_center()
                                .text_color(colors.text_muted)
                                .child("⌄ more")
                        })),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_360px_deal_header_truncates_the_path_before_its_state() {
        let state = "needs reply · 1.9h";
        let measure = |text: &str| px(text.chars().count() as f32 * 8.);
        let (path, rendered_state) = phone_deal_header_text(
            "product strategy / deeply nested launch readiness review",
            state,
            px(360.),
            measure,
        );

        assert_eq!(path, "product strategy…");
        assert_eq!(rendered_state, state);
        assert!(
            measure(&path) + measure(&rendered_state) + PHONE_DEAL_HEADER_FIXED_GUTTER <= px(360.)
        );
    }

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

            phone.record_flick(rho_journal::PhoneFlickDirection::Up, false);
            assert_eq!(
                phone.touch_debug_label(1),
                "contacts 1 · last flick up · stayed"
            );

            phone.record_verdict(rho_journal::PhoneVerdict::Done);
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
            Some(rho_journal::PhoneFlickDirection::Up)
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
            Some(rho_journal::PhoneFlickDirection::Up)
        );

        let mut fast = PhoneFlickGesture::new(&start, PhoneScrollEdge::Bottom);
        fast.update(&touch(TouchPhase::Ended, 440., 50));
        assert_eq!(
            fast.committed_direction(px(600.)),
            Some(rho_journal::PhoneFlickDirection::Up)
        );
    }
}
