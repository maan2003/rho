//! Native narrow-screen projection for the canonical workspace.
//!
//! Phone mode keeps the existing surfaces alive in the workspace registry, but
//! replaces desktop presentation with one surface and a Desk-rooted stack.

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, FocusHandle, MouseButton, Pixels, Point,
    TouchEvent, TouchId, TouchPhase, Window, div, ease_out_quint, px,
};
use theme::ActiveTheme as _;

use super::{ContextId, Surface, SurfaceKey, Workspace};

const PHONE_MAX_WIDTH: Pixels = px(600.);
const TARGET_HEIGHT: Pixels = px(56.);
const FLICK_SLOP: f32 = 12.;
const FLICK_COMMIT_VELOCITY: f32 = 900.;
const SNAP_DURATION: std::time::Duration = std::time::Duration::from_millis(180);
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
    departing: Option<(Surface, String, String)>,
    next_snap_generation: u64,
    transitions: Vec<PhoneTransition>,
    feed_surface: Option<(ContextId, SurfaceKey)>,
    stack: Vec<(ContextId, SurfaceKey)>,
    /// A card arrived while the feed sat empty. The feed is the deal, so it
    /// has to be opened again; only a redraw has the window to do it.
    pub(super) feed_retry: bool,
    pub(super) feed_focus: FocusHandle,
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
            departing: None,
            next_snap_generation: 1,
            transitions: Vec::new(),
            feed_surface: None,
            stack: Vec::new(),
            feed_retry: false,
            feed_focus: cx.focus_handle(),
        }
    }

    pub(super) fn update_mode(&mut self, window: &Window) -> PhoneModeChange {
        let was_enabled = self.enabled;
        let size = window.viewport_size();
        // The same pocket-sized viewport stays a phone when rotated. A
        // keyboard can shrink its height without changing the interaction model.
        self.enabled = self.forced
            || size.width <= PHONE_MAX_WIDTH
            || (size.height <= PHONE_MAX_WIDTH && size.width <= px(1000.));
        PhoneModeChange {
            enabled: self.enabled,
            entered: self.enabled && !was_enabled,
            exited: was_enabled && !self.enabled,
        }
    }

    pub(super) fn show_feed(&mut self, context: ContextId, key: SurfaceKey) {
        self.stack.clear();
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

const PHONE_FONT_SCALE: f32 = 1.1;

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
    fn open_phone_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let subject = self.subject(window, cx);
        self.open_menu(crate::transient::phone_root_menu(&subject), window, cx);
    }

    pub(super) fn phone_surface_menu(&self) -> crate::transient::Menu {
        use gpui::Action;

        use crate::transient::{Command, Menu, MenuAction};
        let mut menu = Menu::new("On this screen");
        macro_rules! action {
            ($key:expr, $label:expr, $action:expr) => {
                menu = menu.item(
                    $key,
                    $label,
                    MenuAction::Command(Command::PhoneAction($action.name())),
                );
            };
        }
        match self.active_surface().key {
            SurfaceKey::Draft => {
                action!("s", "Start agent", crate::SubmitPrompt);
            }
            SurfaceKey::Activity(_) => {
                menu = menu.item(
                    "o",
                    "Open conversation",
                    MenuAction::Command(Command::AgentConversation),
                );
                action!("i", "Write a reply", crate::DealReply);
            }
            SurfaceKey::Transcript(_) => {
                action!("i", "Write a reply", crate::DealReply);
                action!("g", "Load and show all history", crate::TranscriptTop);
                menu = menu.item(
                    "/",
                    "Find in conversation…",
                    MenuAction::Command(Command::PhoneSearch),
                );
                action!("n", "Next match", crate::SearchRepeat);
                action!("p", "Previous match", crate::SearchRepeatReverse);
            }
            SurfaceKey::SlackConversation(_) => {
                action!("i", "Write a message", crate::SlackCompose);
                action!("e", "Edit last message", crate::SlackEditLast);
                action!("c", "Cancel message edit", crate::SlackCancelEdit);
                action!("o", "Open thread under selection", crate::SlackOpenRow);
                action!("n", "Next unread conversation", crate::SlackNextUnread);
            }
            SurfaceKey::SlackResults { .. } | SurfaceKey::SlackInventory(_) => {
                action!("o", "Open selected result", crate::SlackOpenFound);
                action!("n", "Next page", crate::SlackSearchNextPage);
                action!("p", "Previous page", crate::SlackSearchPreviousPage);
            }
            SurfaceKey::SlackList => {
                action!("n", "Next unread conversation", crate::SlackNextUnread);
                action!("s", "Filter conversations…", crate::SlackSearch);
                action!("d", "New direct message…", crate::SlackNewMessage);
            }
            SurfaceKey::Note(_) => {
                action!("o", "Open link under selection", crate::NoteOpenRow);
            }
            SurfaceKey::File { .. } => {
                action!("s", "Save file", crate::FileSave);
            }
            SurfaceKey::Shell(_) => {
                action!("s", "Run command", crate::SubmitPrompt);
                action!("c", "Interrupt · Ctrl-C", crate::ShellInterrupt);
                action!("d", "End input · Ctrl-D", crate::ShellEof);
                action!("m", "Pager · more", crate::ShellPagerMore);
                action!("a", "Pager · all", crate::ShellPagerAll);
                action!("q", "Pager · quit", crate::ShellPagerQuit);
            }
            SurfaceKey::Browser(_) => {
                menu = menu.item(
                    "count",
                    "Set count · scroll, history, input number…",
                    MenuAction::Command(Command::PhoneCount(
                        crate::transient::MenuId::PhoneSurface,
                    )),
                );
                for command in rho_browser::touch_commands() {
                    menu = menu.item(
                        command.keys,
                        command.label,
                        MenuAction::Command(Command::PhoneBrowserCommand(
                            command.keys,
                            command.needs_character,
                        )),
                    );
                }
            }
            SurfaceKey::Terminal { .. } => {
                menu = menu.item(
                    "s",
                    "Send any key chord…",
                    MenuAction::Command(Command::PhoneTerminalChord),
                );
                for (key, label, terminal_key, control) in [
                    ("enter", "Enter", "enter", false),
                    ("escape", "Escape", "escape", false),
                    ("tab", "Tab / completion", "tab", false),
                    ("c", "Interrupt · Ctrl-C", "c", true),
                    ("e", "End input · Ctrl-D", "d", true),
                    ("z", "Suspend · Ctrl-Z", "z", true),
                    ("a", "Line start · Ctrl-A", "a", true),
                    ("k", "Line end · Ctrl-E", "e", true),
                    ("up", "↑ Previous command", "up", false),
                    ("down", "↓ Next command", "down", false),
                    ("left", "← Left", "left", false),
                    ("right", "→ Right", "right", false),
                ] {
                    menu = menu.item(
                        key,
                        label,
                        MenuAction::Command(Command::PhoneTerminalKey(terminal_key, control)),
                    );
                }
                action!("v", "Paste", crate::TerminalPaste);
                action!("n", "Browse scrollback", crate::TerminalNormalMode);
                action!("i", "Type in terminal", crate::TerminalRawMode);
                action!("u", "Scroll up", crate::TerminalScrollHalfPageUp);
                action!("d", "Scroll down", crate::TerminalScrollHalfPageDown);
                action!("g", "Oldest output", crate::TerminalScrollTop);
                action!("b", "Latest output", crate::TerminalScrollBottom);
            }
            _ => {}
        }
        action!("x", "Close this screen", crate::SurfaceClose);
        menu
    }

    pub(super) fn run_phone_browser_command(
        &mut self,
        view: gpui::Entity<rho_browser::PageView>,
        keys: &'static str,
        count: Option<u32>,
        character: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let task = view.update(cx, |view, cx| {
            view.run_touch_command(keys.to_owned(), count.unwrap_or(0) as usize, character, cx)
        });
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                let _ = this.update(cx, |this, cx| {
                    this.echo(
                        &format!("Browser: {error:#}"),
                        rho_window::StyleClass::SystemImportant,
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    pub(crate) fn touch_mode(&self) -> bool {
        self.phone.enabled
    }

    pub(super) fn phone_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let change = self.phone.update_mode(window);
        if change.entered || change.exited {
            for surfaces in self.surfaces.values() {
                for surface in surfaces {
                    if let super::SurfaceView::Home(view) = &surface.view {
                        view.update(cx, |view, cx| {
                            view.set_touch_presentation(self.phone.enabled, cx)
                        });
                    }
                    if let super::SurfaceView::SlackConversation(view) = &surface.view {
                        view.update(cx, |view, cx| {
                            view.set_touch_presentation(self.phone.enabled, window, cx)
                        });
                    }
                }
            }
        }
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
            self.phone.departing = None;
            self.update_statuses(cx);
            cx.defer(|cx| {
                theme_settings::reset_buffer_font_size(cx);
                theme_settings::reset_ui_font_size(cx);
                set_touch_modal_editing(true, cx);
            });
        }
        change.enabled
    }

    #[cfg(test)]
    pub(crate) fn phone_feed_for_test(&mut self, cx: &mut Context<Self>) -> bool {
        self.phone.enabled && self.phone.stack.is_empty() && self.open_card_in_view(cx).is_some()
    }

    #[cfg(test)]
    pub(crate) fn phone_feed_is_active_for_test(&self) -> bool {
        self.phone
            .feed_surface
            .as_ref()
            .is_some_and(|(context, key)| {
                *context == self.active_context && self.active_surface().key == *key
            })
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
        window.focus(&self.phone.feed_focus, cx);
    }

    pub(super) fn phone_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    && !self.has_modal_overlay()
                {
                    let edge = if self.open_card_in_view(cx).is_some() {
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
                let from = self.phone.drag_offset;
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
                    if self.open_card_in_view(cx).is_some() {
                        let to = match direction {
                            rho_journal::PhoneFlickDirection::Up => -window.viewport_size().height,
                            rho_journal::PhoneFlickDirection::Down => window.viewport_size().height,
                        };
                        self.start_phone_snap(from, to, Some(direction), window, cx);
                    } else {
                        self.commit_phone_flick(direction, window, cx);
                    }
                } else if from != Pixels::ZERO {
                    self.start_phone_snap(from, Pixels::ZERO, None, window, cx);
                }
            }
        }
        cx.notify();
    }

    fn start_phone_snap(
        &mut self,
        from: Pixels,
        to: Pixels,
        direction: Option<rho_journal::PhoneFlickDirection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = self.open_card_in_view(cx);
        let departing = source.as_ref().map(|card| {
            (
                self.active_surface().clone(),
                Self::card_path(card),
                card.label.clone(),
            )
        });
        if let Some(direction) = direction {
            // Change the feed once on release, then move both pages together.
            // Waiting until the outgoing page disappeared left a blank flash.
            self.commit_phone_flick(direction, window, cx);
        }
        let moved = source.as_ref().map(|card| &card.node)
            != self.open_card_in_view(cx).as_ref().map(|card| &card.node);
        self.phone.departing = moved.then_some(departing).flatten();
        let generation = self.phone.next_snap_generation;
        self.phone.next_snap_generation = generation.wrapping_add(1);
        self.phone.snap = Some(PhoneSnap {
            generation,
            from,
            to: if moved { to } else { Pixels::ZERO },
        });
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SNAP_DURATION).await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this
                    .phone
                    .snap
                    .is_some_and(|snap| snap.generation == generation)
                {
                    this.phone.snap = None;
                    this.phone.departing = None;
                    cx.notify();
                }
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
        let incoming = self.render_phone_body_contents(text_style, window, cx);
        let Some(snap) = self.phone.snap else {
            return incoming;
        };
        let Some((surface, title, state)) = self.phone.departing.clone() else {
            return incoming;
        };
        let colors = cx.theme().colors();
        let outgoing = div()
            .absolute()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .child(
                div()
                    .flex_none()
                    .min_h(px(56.))
                    .max_h(px(112.))
                    .px_3()
                    .py_2()
                    .overflow_hidden()
                    .text_color(colors.text)
                    .child(title)
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(colors.text_muted)
                            .child(state),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_surface(&surface)),
            )
            .child(self.render_phone_verdict_bar(cx));
        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .child(outgoing.with_animation(
                ("phone-outgoing", snap.generation),
                Animation::new(SNAP_DURATION).with_easing(ease_out_quint()),
                move |page, delta| page.top(snap.from + (snap.to - snap.from) * delta),
            ))
            .child(div().absolute().size_full().child(incoming).with_animation(
                ("phone-incoming", snap.generation),
                Animation::new(SNAP_DURATION).with_easing(ease_out_quint()),
                move |page, delta| page.top((snap.from - snap.to) * (1. - delta)),
            ))
            .into_any_element()
    }

    fn render_phone_body_contents(
        &mut self,
        _text_style: &gpui::TextStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.phone.stack.is_empty()
            && let Some(card) = self.open_card_in_view(cx)
        {
            let colors = cx.theme().colors();
            let header = div()
                .id("phone-deal-header")
                .flex_none()
                .min_h(px(56.))
                .max_h(px(112.))
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .overflow_hidden()
                .border_b_1()
                .border_color(colors.border_variant)
                .text_color(colors.text)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| this.open_phone_menu(window, cx)))
                .child(Self::card_path(&card))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(colors.text_muted)
                        .child(card.label.clone()),
                );
            let body = self.render_surface(&self.active_surface().clone());
            let card = div()
                .id("phone-deal-card")
                .track_focus(&self.phone.feed_focus)
                .size_full()
                .relative()
                .flex()
                .flex_col()
                .children((window.viewport_size().height > px(300.)).then_some(header))
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
                .child(self.render_phone_verdict_bar(cx));
            return if let Some(snap) = self.phone.snap.filter(|_| self.phone.departing.is_none()) {
                card.with_animation(
                    ("phone-card-snap", snap.generation),
                    Animation::new(SNAP_DURATION).with_easing(ease_out_quint()),
                    move |card, delta| {
                        let from = snap.from.as_f32();
                        let to = snap.to.as_f32();
                        card.top(px(from + (to - from) * delta))
                    },
                )
                .into_any_element()
            } else {
                card.top(self.phone.drag_offset).into_any_element()
            };
        }
        if self.phone.stack.is_empty() {
            let colors = cx.theme().colors();
            return div()
                .id("phone-feed-empty")
                .track_focus(&self.phone.feed_focus)
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
                            this.open_phone_menu(window, cx);
                        }))
                        // The header names what the reader is looking at,
                        // and with the queue empty that is Home, not the
                        // deal they have already flicked past.
                        .child("home"),
                )
                .child({
                    // Home is the card after the last deal: flick past the
                    // queue and the glance is what is left.
                    let body = div()
                        .id("phone-feed-empty-body")
                        .capture_touch(cx.listener(Self::phone_touch))
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
                })
                .child(self.render_phone_bar(cx))
                .into_any_element();
        }
        if let Some(surface) = self.phone_surface() {
            let title = match &surface.key {
                SurfaceKey::Note(node) => self.node_title(node, cx),
                SurfaceKey::Transcript(id) | SurfaceKey::Activity(id) => {
                    self.registry.agent_display_label(*id)
                }
                SurfaceKey::SlackConversation(source) => self
                    .slack_labels
                    .get(source)
                    .cloned()
                    .unwrap_or_else(|| self.surface_name(&surface.key)),
                _ => self.surface_name(&surface.key),
            };
            let body = if surface.key == SurfaceKey::Draft {
                self.draft_model
                    .update(cx, |draft, cx| draft.render_phone(window, cx))
            } else {
                self.render_surface(&surface)
            };
            div()
                .id("phone-surface")
                .size_full()
                .flex()
                .flex_col()
                .children((window.viewport_size().height > px(300.)).then(|| {
                    div()
                        .id("phone-surface-title")
                        .flex_none()
                        .min_h(px(48.))
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .text_color(cx.theme().colors().text)
                        .child(title)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_phone_menu(window, cx)),
                        )
                }))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .child(body),
                )
                .child(self.render_phone_bar(cx))
                .into_any_element()
        } else {
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
                        .track_focus(&self.phone.feed_focus),
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
                            rho_journal::PhoneVerdict::Done,
                            Box::new(crate::DealDone),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(
                item("phone-verdict-mute", "×", "mute").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            rho_journal::PhoneVerdict::Mute,
                            Box::new(crate::DealMute),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(
                // Defer asks how long: the same question the `s` operator's
                // unit answers on a keyboard.
                item("phone-verdict-defer", "◷", "defer").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.open_menu(crate::transient::snooze_sheet(), window, cx);
                    },
                )),
            )
            .child(
                item("phone-verdict-reply", "↩", "reply").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            rho_journal::PhoneVerdict::Reply,
                            Box::new(crate::DealReply),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(item("phone-feed-menu", "☰", "menu").on_click(cx.listener(
                |this, _, window, cx| {
                    this.open_phone_menu(window, cx);
                },
            )))
            .children(self.attention.last_undo().map(|_| {
                item("phone-feed-undo", "↶", "undo").on_click(cx.listener(|this, _, window, cx| {
                    this.undo_verdict(window, cx);
                }))
            }))
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
        let primary = if self.phone_surface().is_some_and(|surface| {
            matches!(
                surface.key,
                super::SurfaceKey::Draft
                    | super::SurfaceKey::Transcript(_)
                    | super::SurfaceKey::SlackConversation(_)
                    | super::SurfaceKey::Shell(_)
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
                    this.open_phone_menu(window, cx);
                })),
            )
            .child(
                item("phone-edit", "✎", "edit").on_click(cx.listener(|this, _, window, cx| {
                    this.open_menu(crate::transient::phone_edit_menu(), window, cx);
                })),
            )
            .children(self.attention.last_undo().map(|_| {
                item("phone-action-undo", "↶", "undo").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.undo_verdict(window, cx);
                    },
                ))
            }))
            .children(primary)
            .into_any_element()
    }

    fn phone_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() {
            self.minibuffer_confirm(window, cx);
            return;
        }
        if self.phone.stack.is_empty() {
            return;
        }
        let Some(surface) = self.phone_surface() else {
            return;
        };
        match surface.key {
            super::SurfaceKey::Draft
            | super::SurfaceKey::Transcript(_)
            | super::SurfaceKey::Shell(_) => self.submit_prompt(&crate::SubmitPrompt, window, cx),
            super::SurfaceKey::SlackConversation(_) => {
                let super::SurfaceView::SlackConversation(view) = surface.view else {
                    return;
                };
                // The answer says what Slack made of it, which the journal
                // wants and the phone has nowhere to put. Dropping it drops
                // the answer, not the message: the write is detached inside
                // `submit`.
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
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let crate::workspace::MenuSheet {
            title,
            rows,
            has_back: has_parent,
        } = self.menu_sheet()?;
        let colors = cx.theme().colors();
        let compact = window.viewport_size().height <= px(300.);

        let mut header = div()
            .flex()
            .items_center()
            .min_h(if compact { px(48.) } else { TARGET_HEIGHT })
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
                .occlude()
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
                        .max_h(gpui::relative(if compact { 1. } else { 0.82 }))
                        .w_full()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .bg(background)
                        .text_color(text_style.color)
                        .font_family(text_style.font_family.clone())
                        .font_weight(text_style.font_weight)
                        .text_size(text_style.font_size)
                        .line_height(text_style.line_height)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(header.flex_none())
                        .child(
                            div()
                                .id("phone-sheet-rows")
                                .min_h_0()
                                .overflow_y_scroll()
                                .children(rows),
                        ),
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
