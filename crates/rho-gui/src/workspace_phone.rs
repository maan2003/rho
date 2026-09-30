//! Native narrow-screen projection for the canonical workspace.
//!
//! Phone mode keeps the existing surfaces alive in the workspace registry, but
//! replaces desktop presentation with one surface and a Desk-rooted stack.

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, FocusHandle, Focusable as _, MouseButton,
    MouseDownEvent, Pixels, Point, TouchEvent, TouchId, TouchPhase, Window, div, ease_out_quint,
    px,
};
use theme::ActiveTheme as _;

use super::{ContextId, Surface, SurfaceKey, Workspace};

const PHONE_MAX_WIDTH: Pixels = px(600.);
const TARGET_HEIGHT: Pixels = px(56.);
const TITLE_HEIGHT: Pixels = px(44.);
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

/// What sits beyond the card in view, peeking in while it is dragged: the
/// card a flick up deals, or what a flick down takes back.
enum PhonePeek {
    Card(Box<rho_dealer::Card>),
    Home,
    Pile,
    Undo,
}

/// A card being answered from the feed: the card, and the surface its reply
/// opened. Sending from that surface finishes the card.
#[derive(Clone)]
struct PhoneReply {
    card: rho_dealer::NodeId,
    surface: (ContextId, SurfaceKey),
}

pub(super) struct PhoneUi {
    pub(super) enabled: bool,
    reply: Option<PhoneReply>,
    forced: bool,
    touch_debug: bool,
    last_gesture: Option<String>,
    flick: Option<PhoneFlickGesture>,
    /// The pages below and above the card in view, taken when a drag
    /// starts. None on a side is the end of the feed there.
    peek_next: Option<PhonePeek>,
    peek_back: Option<PhonePeek>,
    /// The next card's surface, built when a drag starts so that the page
    /// rising into view is the one that lands. A deal takes it.
    pub(super) peek_surface: Option<Surface>,
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
            reply: None,
            forced,
            touch_debug: std::env::var("RHO_PHONE_TOUCH_DEBUG").is_ok_and(|value| value == "1"),
            last_gesture: None,
            flick: None,
            peek_next: None,
            peek_back: None,
            peek_surface: None,
            drag_offset: Pixels::ZERO,
            snap: None,
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
            self.update_statuses(cx);
            cx.defer(|cx| {
                theme_settings::reset_buffer_font_size(cx);
                theme_settings::reset_ui_font_size(cx);
                set_touch_modal_editing(true, cx);
            });
        }
        change.enabled
    }

    /// A tap or a long press on a phone surface, once the editor under the
    /// finger has placed its cursor. A tap does what `enter` does on the row
    /// it landed on; a long press opens what can be done with it.
    fn phone_surface_pointer_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A sheet or the minibuffer over the surface owns the press.
        if self.menu_buffer.is_some() || self.minibuffer.is_some() {
            return;
        }
        let button = event.button;
        cx.defer_in(window, move |this, window, cx| match button {
            MouseButton::Left => this.phone_tap(window, cx),
            MouseButton::Right => this.phone_long_press(window, cx),
            _ => {}
        });
    }

    fn phone_tap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let feed = self.phone.stack.is_empty();
        match self.active_surface().view.clone() {
            // Text input is only on while the cursor is in the editable
            // prompt tail, so the keyboard does not cover a transcript that
            // is being read. On a card the prompt is the reply.
            super::SurfaceView::Transcript { model, editor } => {
                let in_prompt = model.read(cx).selection_in_prompt(&editor, cx);
                if feed && in_prompt {
                    self.phone_reply(window, cx);
                    return;
                }
                let focus = if in_prompt {
                    editor.focus_handle(cx)
                } else {
                    self.phone.feed_focus.clone()
                };
                window.focus(&focus, cx);
            }
            super::SurfaceView::SlackConversation(view) => {
                if !view.read(cx).cursor_in_compose(cx) {
                    self.slack_open_row(window, cx);
                } else if feed {
                    self.phone_reply(window, cx);
                }
            }
            super::SurfaceView::SlackList(_) => self.slack_open_row(window, cx),
            super::SurfaceView::SlackResults(_) => {
                self.slack_open_found(window, cx);
            }
            super::SurfaceView::Draft { editor, .. } => {
                self.phone_pick_draft_field(&editor, window, cx);
            }
            _ => {}
        }
    }

    /// A tap on one of the draft's header rows offers its values as a list,
    /// the completions a keyboard would have typed its way to.
    fn phone_pick_draft_field(
        &mut self,
        editor: &gpui::Entity<editor::Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[derive(Clone, Copy)]
        enum Field {
            Workdir,
            Role,
            Start,
        }
        let draft = self.draft_model.read(cx);
        let field = if draft.cursor_in_role_field(editor, cx) {
            Field::Role
        } else if draft.cursor_in_start_field(editor, cx) {
            Field::Start
        } else if draft.cursor_in_a_field(editor, cx) {
            Field::Workdir
        } else {
            return;
        };
        let prompt = match field {
            Field::Workdir => "workdir:",
            Field::Role => "role:",
            Field::Start => "on top of:",
        };
        let complete = std::rc::Rc::new(move |workspace: &Workspace, input: &str, _: &gpui::App| {
            match field {
                Field::Workdir => {
                    crate::commands::workdir_field_candidates(input, &workspace.hosts.workdir_table())
                }
                Field::Role => crate::commands::role_field_candidates(input),
                Field::Start => {
                    crate::commands::start_field_candidates(input, &workspace.live_agent_targets())
                }
            }
        });
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  _: &mut Window,
                  cx: &mut Context<Workspace>| {
                workspace.draft_model.update(cx, |draft, cx| match field {
                    Field::Workdir => draft.set_workdir_text(&input, cx),
                    Field::Role => draft.set_role_text(&input, cx),
                    Field::Start => draft.set_start_text(&input, cx),
                });
            },
        );
        self.open_prompt(prompt, complete, on_submit, window, cx);
    }

    /// The phone's right click: what can be done with the thing under the
    /// finger. A message has its actions and an agent its menu; anywhere
    /// else it is the whole menu.
    fn phone_long_press(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.active_surface().view {
            super::SurfaceView::SlackConversation(view) if !view.read(cx).cursor_in_compose(cx) => {
                self.run_command(crate::transient::Command::SlackMessageActions, window, cx);
            }
            super::SurfaceView::Transcript { .. } => {
                self.open_menu(crate::transient::agent_menu(), window, cx);
            }
            _ => self.open_phone_menu(window, cx),
        }
    }

    /// Answer the card in view: the reply verdict, remembering which card
    /// and which surface, so a send from there finishes the card.
    fn phone_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.open_card_in_view(cx).map(|card| card.node) else {
            return;
        };
        self.phone_verdict_with(
            rho_journal::PhoneVerdict::Reply,
            move |this, window, cx| {
                this.deal_reply(window, cx);
                this.phone.reply = this
                    .phone
                    .stack
                    .last()
                    .cloned()
                    .map(|surface| PhoneReply { card, surface });
                cx.notify();
            },
            window,
            cx,
        );
    }

    /// A reply went out from the card's surface: the card is answered. Back
    /// to the feed, the card done, and the next one dealt.
    fn phone_finish_reply(
        &mut self,
        reply: PhoneReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.phone.reply = None;
        self.phone.stack.retain(|surface| surface != &reply.surface);
        if !self.phone.stack.is_empty() {
            return;
        }
        self.restore_phone_feed(window, cx);
        if self
            .open_card_in_view(cx)
            .is_some_and(|card| card.node == reply.card)
        {
            self.phone_verdict_with(
                rho_journal::PhoneVerdict::Done,
                |this, window, cx| this.verdict_done(window, cx),
                window,
                cx,
            );
            self.echo(
                "sent · card done",
                rho_window::style::StyleClass::SystemInfo,
                cx,
            );
        }
        cx.notify();
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
    pub(crate) fn phone_peek_next_for_test(&self) -> Option<rho_dealer::NodeId> {
        match &self.phone.peek_next {
            Some(PhonePeek::Card(card)) => Some(card.node.clone()),
            _ => None,
        }
    }

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

        if let Some(popped) = self.phone.stack.pop()
            && self
                .phone
                .reply
                .as_ref()
                .is_some_and(|reply| reply.surface == popped)
        {
            self.phone.reply = None;
        }
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

    /// The pages beyond the card in view: below, the card a pull deals
    /// (Home when there is none); above, the card a flick down takes back.
    fn phone_peeks(
        &mut self,
        card: &rho_dealer::Card,
        cx: &mut Context<Self>,
    ) -> (Option<PhonePeek>, Option<PhonePeek>) {
        let next = if self.attention.open_pile.is_some() {
            PhonePeek::Pile
        } else {
            match self.hand(cx).top(Some(&card.node)) {
                Some(next) => PhonePeek::Card(Box::new(next.clone())),
                None => PhonePeek::Home,
            }
        };
        let back = match self.phone.transitions.last() {
            Some(PhoneTransition::Flick(card)) => Some(PhonePeek::Card(card.clone())),
            Some(PhoneTransition::Verdict(sequence))
                if self.attention.last_undo() == Some(*sequence) =>
            {
                Some(PhonePeek::Undo)
            }
            _ => None,
        };
        (Some(next), back)
    }

    /// The key a card's surface is filed under, for a card that opens as
    /// an ordinary surface; a Slack card opens through its conversation.
    fn phone_card_surface_key(node: &rho_dealer::NodeId) -> Option<SurfaceKey> {
        match node {
            rho_dealer::NodeId::Agent(agent_id) => Some(SurfaceKey::Transcript(*agent_id)),
            rho_dealer::NodeId::Slack(_) => None,
            node => Some(SurfaceKey::Note(node.clone())),
        }
    }

    fn phone_open_surface(&self, key: &SurfaceKey) -> Option<&Surface> {
        self.surfaces
            .values()
            .flatten()
            .chain(&self.phone.peek_surface)
            .find(|surface| &surface.key == key)
    }

    fn build_phone_peek_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PhonePeek::Card(card)) = &self.phone.peek_next else {
            return;
        };
        let Some(key) = Self::phone_card_surface_key(&card.node) else {
            return;
        };
        if self.phone_open_surface(&key).is_none() {
            self.phone.peek_surface = Some(self.make_surface(key, window, cx));
        }
    }

    /// The page a drag by `dy` pulls into view.
    fn phone_peek_toward(&self, dy: Pixels) -> Option<&PhonePeek> {
        match dy < Pixels::ZERO {
            true => self.phone.peek_next.as_ref(),
            false => self.phone.peek_back.as_ref(),
        }
    }

    /// A neighbouring page as it will look once it is in view: the card's
    /// own surface when it is already open somewhere, else its title and
    /// what it wants.
    fn render_phone_peek(&self, peek: &PhonePeek, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let note = |text: &str| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.text_muted)
                .child(text.to_owned())
                .into_any_element()
        };
        let (title, state, body) = match peek {
            PhonePeek::Card(card) => {
                let open = Self::phone_card_surface_key(&card.node)
                    .and_then(|key| self.phone_open_surface(&key));
                let body = match open {
                    Some(surface) => div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .child(self.render_surface(surface))
                        .into_any_element(),
                    None => div()
                        .flex_1()
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(18.))
                                .text_color(colors.text)
                                .child(card.title.clone()),
                        )
                        .child(
                            div()
                                .text_color(colors.text_muted)
                                .child(card.context.clone()),
                        )
                        .into_any_element(),
                };
                (Self::card_path(card), Some(card.label.clone()), body)
            }
            PhonePeek::Home => (
                "home".to_owned(),
                None,
                match self.home_view() {
                    Some(view) => div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .child(view)
                        .into_any_element(),
                    None => note("nothing needs attention"),
                },
            ),
            PhonePeek::Pile => ("pile".to_owned(), None, note("the next card on the pile")),
            PhonePeek::Undo => ("undo".to_owned(), None, note("let go to take the last verdict back")),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .child(self.render_phone_title(title, state, cx))
            .child(body)
            .into_any_element()
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
                {
                    let edge = if let Some(card) = self.open_card_in_view(cx) {
                        (self.phone.peek_next, self.phone.peek_back) =
                            self.phone_peeks(&card, cx);
                        self.build_phone_peek_surface(window, cx);
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
                    let dy = self
                        .phone
                        .flick
                        .as_ref()
                        .map_or(Pixels::ZERO, |flick| flick.position.y - flick.start.y);
                    // At the end of the feed the card follows the finger
                    // only a little, and springs back.
                    match self.phone_peek_toward(dy).is_some() {
                        true => dy,
                        false => dy * 0.25,
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
                self.phone.drag_offset = Pixels::ZERO;
                let card_in_view = self.open_card_in_view(cx).is_some();
                // A card with nothing beyond it in that direction stays.
                let direction = direction.filter(|_| !card_in_view || self.phone_peek_toward(from).is_some());
                if let Some(direction) = direction {
                    window.prevent_default();
                    cx.stop_propagation();
                    if card_in_view {
                        // A page is the screen above the bar: the neighbour
                        // lands exactly where the card was.
                        let page = window.viewport_size().height - TARGET_HEIGHT;
                        let to = match direction {
                            rho_journal::PhoneFlickDirection::Up => -page,
                            rho_journal::PhoneFlickDirection::Down => page,
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
                    && this
                        .card_in_view(cx)
                        .is_some_and(|card| Some(&card.node) == source.as_ref())
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

    pub(super) fn render_phone_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.phone.stack.is_empty()
            && let Some(card) = self.open_card_in_view(cx)
        {
            let header =
                self.render_phone_title(Self::card_path(&card), Some(card.label.clone()), cx);
            let body = self.render_surface(&self.active_surface().clone());
            let page = || div().absolute().left_0().size_full().flex().flex_col();
            // The card and its neighbours are one strip, a page apart, that
            // moves under the finger: the next card rises into view as the
            // one in view leaves.
            let mut strip = div()
                .id("phone-deal-strip")
                .absolute()
                .left_0()
                .size_full()
                .child(
                    page().top_0().child(header).child(
                        div()
                            .id("phone-deal-body")
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .overflow_hidden()
                            .child(body),
                    ),
                );
            let dragging = self.phone.drag_offset != Pixels::ZERO || self.phone.snap.is_some();
            if dragging {
                let colors = cx.theme().colors();
                for (peek, top) in [
                    (&self.phone.peek_next, gpui::relative(1.)),
                    (&self.phone.peek_back, gpui::relative(-1.)),
                ] {
                    if let Some(peek) = peek {
                        strip = strip.child(
                            page()
                                .top(top)
                                .border_t_1()
                                .border_color(colors.border_variant)
                                .child(self.render_phone_peek(peek, cx)),
                        );
                    }
                }
            }
            let strip = if let Some(snap) = self.phone.snap {
                strip
                    .with_animation(
                        ("phone-card-snap", snap.generation),
                        Animation::new(SNAP_DURATION).with_easing(ease_out_quint()),
                        move |strip, delta| {
                            let from = snap.from.as_f32();
                            let to = snap.to.as_f32();
                            strip.top(px(from + (to - from) * delta))
                        },
                    )
                    .into_any_element()
            } else {
                strip.top(self.phone.drag_offset).into_any_element()
            };
            return div()
                .id("phone-deal-card")
                .track_focus(&self.phone.feed_focus)
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("phone-deal-viewport")
                        .capture_touch(cx.listener(Self::phone_touch))
                        .capture_any_mouse_down(cx.listener(Self::phone_surface_pointer_down))
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .child(strip),
                )
                // A question in the minibuffer is answered with ok and dropped
                // with back, whatever it was asked over.
                .child(if self.minibuffer.is_some() {
                    self.render_phone_bar(cx)
                } else {
                    self.render_phone_verdict_bar(cx)
                })
                .into_any_element();
        }
        if self.phone.stack.is_empty() {
            let colors = cx.theme().colors();
            return div()
                .id("phone-feed-empty")
                .track_focus(&self.phone.feed_focus)
                .size_full()
                .flex()
                .flex_col()
                // The header names what the reader is looking at, and with
                // the queue empty that is Home, not the deal they have
                // already flicked past.
                .child(self.render_phone_title("home".to_owned(), None, cx))
                .child({
                    // Home is the card after the last deal: flick past the
                    // queue and the glance is what is left.
                    let body = div()
                        .id("phone-feed-empty-body")
                        .capture_touch(cx.listener(Self::phone_touch))
                        // A tap on a row of Home opens it, as `enter` does.
                        .capture_any_mouse_down(cx.listener(
                            |this, event: &MouseDownEvent, window, cx| {
                                if event.button == MouseButton::Left
                                    && this.menu_buffer.is_none()
                                    && this.minibuffer.is_none()
                                {
                                    cx.defer_in(window, |this, window, cx| {
                                        this.home_open_row(window, cx)
                                    });
                                }
                            },
                        ))
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
            div()
                .id("phone-surface")
                .size_full()
                .flex()
                .flex_col()
                .child(self.render_phone_title(
                    self.surface_path(cx),
                    self.phone_surface_state(cx),
                    cx,
                ))
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
        cx.defer_in(window, move |this, window, cx| {
            let after = this.open_card_in_view(cx).map(|card| card.node);
            if before.is_some() && after.is_some() && before != after && this.phone.stack.is_empty() {
                // The next card rises into place, as it does under a flick;
                // what was answered is gone, so nothing follows it down.
                this.phone.peek_next = None;
                this.phone.peek_back = None;
                let page = window.viewport_size().height - TARGET_HEIGHT;
                this.start_phone_snap(page, Pixels::ZERO, None, window, cx);
            }
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
                item("phone-verdict-todo", "○", "todo").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.dispatch_phone_verdict(
                            rho_journal::PhoneVerdict::Todo,
                            Box::new(crate::DealTodo),
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
                            rho_journal::PhoneVerdict::File,
                            Box::new(crate::DealFile),
                            window,
                            cx,
                        );
                    },
                )),
            )
            .child(
                item("phone-verdict-reply", "↩", "reply")
                    .on_click(cx.listener(|this, _, window, cx| this.phone_reply(window, cx))),
            )
            .child(
                item("phone-verdict-more", "☰", "more")
                    .on_click(cx.listener(|this, _, window, cx| this.open_phone_menu(window, cx))),
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
        // The third slot is always there, so back and menu never move: the
        // thumb learns a place, not a count.
        let primary = if self.minibuffer.is_some() {
            item("phone-send", "✓", "ok")
                .on_click(cx.listener(|this, _, window, cx| this.phone_send(window, cx)))
        } else if self.phone_surface().is_some_and(|surface| {
            matches!(
                surface.view,
                super::SurfaceView::Draft { .. }
                    | super::SurfaceView::Transcript { .. }
                    | super::SurfaceView::Shell { .. }
                    | super::SurfaceView::SlackConversation(_)
            )
        }) {
            // Answering a card: the send is the card's verdict too.
            let label = if self.phone_reply_in_view().is_some() {
                "send & done"
            } else {
                "send"
            };
            item("phone-send", "↑", label)
                .on_click(cx.listener(|this, _, window, cx| this.phone_send(window, cx)))
        } else {
            div().id("phone-primary-empty").flex_1()
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
                item("phone-menu", "☰", "menu")
                    .on_click(cx.listener(|this, _, window, cx| this.open_phone_menu(window, cx))),
            )
            .child(primary)
            .into_any_element()
    }

    /// The phone's whole menu: every leader item, and the card's verdicts
    /// while a card is dealt.
    fn open_phone_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let subject = self.subject(window, cx);
        let card = self.phone.stack.is_empty() && self.open_card_in_view(cx).is_some();
        self.open_menu(
            crate::transient::phone_root_menu(&subject, card),
            window,
            cx,
        );
    }

    /// The top of every phone screen: what is in view, and its state.
    fn render_phone_title(
        &self,
        title: String,
        state: Option<String>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        div()
            .id("phone-title")
            .flex_none()
            .h(TITLE_HEIGHT)
            .w_full()
            .px_3()
            .gap_2()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(colors.border_variant)
            .text_color(colors.text)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(16.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .children(state.map(|state| {
                div()
                    .flex_none()
                    .text_size(px(13.))
                    .text_color(colors.text_muted)
                    .child(state)
            }))
            .into_any_element()
    }

    /// What the status line says beside a surface's name: an agent's state,
    /// or what arrived in a Slack conversation below the fold.
    fn phone_surface_state(&self, cx: &Context<Self>) -> Option<String> {
        match &self.active_surface().key {
            SurfaceKey::Transcript(agent_id) | SurfaceKey::Activity(agent_id) => {
                crate::attention::agent_state_label(
                    &self.registry.agent_facts(*agent_id),
                    chrono::Local::now().fixed_offset(),
                )
            }
            _ => self.slack_unseen(cx).map(|unseen| format!("{unseen} new")),
        }
    }

    /// The echo line, which the phone has no status row for: a toast over
    /// the bottom bar for as long as the desk shows it. A tap opens the
    /// message log, where it stays.
    pub(super) fn render_phone_toast(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let echo = self.echo.as_ref()?;
        let colors = cx.theme().colors();
        Some(
            div()
                .id("phone-toast")
                .absolute()
                .occlude()
                .left_3()
                .right_3()
                .bottom(TARGET_HEIGHT + px(12.))
                .px_3()
                .py_2()
                .rounded_md()
                .bg(colors.elevated_surface_background)
                .border_1()
                .border_color(colors.border)
                .text_size(px(14.))
                .text_color(colors.text)
                .cursor_pointer()
                .child(echo.text().to_owned())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.echo = None;
                    this.cmd_messages(window, cx);
                    cx.stop_propagation();
                }))
                .into_any_element(),
        )
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
        let reply = self.phone_reply_in_view();
        match surface.view {
            super::SurfaceView::Transcript { model, .. } => {
                let sending = !model.read(cx).prompt_text(cx).trim().is_empty();
                self.submit_prompt(&crate::SubmitPrompt, window, cx);
                if sending && let Some(reply) = reply {
                    self.phone_finish_reply(reply, window, cx);
                }
            }
            super::SurfaceView::Draft { .. } | super::SurfaceView::Shell { .. } => {
                self.submit_prompt(&crate::SubmitPrompt, window, cx)
            }
            super::SurfaceView::SlackConversation(view) => {
                // The card is finished only once Slack has the message: a
                // refused write leaves the reader where the text still is.
                let submitting = view.update(cx, |view, cx| view.submit(cx));
                cx.spawn_in(window, async move |this, cx| {
                    let submitted = submitting.await;
                    let sent = matches!(
                        submitted,
                        rho_slack::ui::conversation::Submitted::Sent
                            | rho_slack::ui::conversation::Submitted::FileSent(_)
                    );
                    let _ = this.update_in(cx, |this, window, cx| {
                        if sent && let Some(reply) = reply {
                            this.phone_finish_reply(reply, window, cx);
                        }
                    });
                })
                .detach();
            }
            _ => {}
        }
    }

    /// The card reply the surface on top of the stack is, if it is one.
    fn phone_reply_in_view(&self) -> Option<PhoneReply> {
        self.phone
            .reply
            .clone()
            .filter(|reply| self.phone.stack.last() == Some(&reply.surface))
    }

    /// The open menu, drawn as a sheet: the same menu the desk draws as a
    /// block under the point, with its rows as targets a thumb can hit.
    pub(super) fn render_phone_menu_sheet(
        &self,
        text_style: &gpui::TextStyle,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let crate::workspace::MenuSheet {
            title,
            rows,
            has_back: has_parent,
        } = self.menu_sheet()?;
        let colors = cx.theme().colors();

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
                .occlude()
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
                        .max_h(gpui::relative(0.82))
                        .w_full()
                        .overflow_y_scroll()
                        .bg(background)
                        .text_color(text_style.color)
                        .text_size(px(16.))
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
