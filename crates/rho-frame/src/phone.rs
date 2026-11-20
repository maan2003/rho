//! The phone: the same frame, laid out for a thumb.
//!
//! A window at most [`PHONE_MAX_WIDTH`] wide is a phone's. It shows what
//! the desk shows, the one surface from the one history with the status
//! line, the minibuffer and the echo, and adds what a keyboard would have
//! given: a bar along the bottom edge, menus as a sheet of rows a thumb
//! can hit, and a flick on the surface for up and down through the
//! history. Nothing here is a second place for state; every button runs
//! what a key runs.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, MouseButton, MouseDownEvent, Pixels, Point,
    TouchEvent, TouchId, TouchPhase, Window, div, ease_out_quint, px,
};
use rho_journal::{PhoneFlickDirection, PhoneVerdict};
use theme::ActiveTheme as _;

use crate::attention::Verdict;
use crate::frame::{Frame, snooze_rows};
use crate::menu::{Pick, Row, Rows};
use crate::surface::{AnySurface, PhoneSend};

/// The widest window that is laid out as a phone.
pub(crate) const PHONE_MAX_WIDTH: Pixels = px(600.);
/// The least height of anything a thumb is meant to hit.
const TARGET_HEIGHT: Pixels = px(56.);
/// How far a finger travels before the touch is a flick and not a tap.
const FLICK_SLOP: f32 = 12.;
/// Pixels a second past which a short flick still commits.
const FLICK_COMMIT_VELOCITY: f32 = 900.;
const SNAP_DURATION: Duration = Duration::from_millis(180);
/// The status line's padding and the gap between a heading and its state.
const HEADER_GUTTER: Pixels = px(28.);

/// How the frame is laid out, from the window's width alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Desktop,
    Phone,
}

impl Layout {
    pub fn of(viewport_width: Pixels) -> Self {
        match viewport_width <= PHONE_MAX_WIDTH {
            true => Self::Phone,
            false => Self::Desktop,
        }
    }
}

/// Hit-test a phone editor against its painted layout. Capture this press
/// before the editor handles selection. The platform synthesizes a mouse
/// press for a recognized touch tap, not for a pan.
pub fn phone_editor_tap(
    editor: &editor::Editor,
    event: &gpui::MouseDownEvent,
    window: &Window,
) -> Option<multi_buffer::Anchor> {
    if Layout::of(window.viewport_size().width) != Layout::Phone
        || event.button != MouseButton::Left
        || event.click_count != 1
        || event.modifiers.modified()
    {
        return None;
    }
    editor
        .buffer_location_for_window_position(event.position, text::Bias::Left)
        .map(|(anchor, _, _)| anchor)
}

/// A heading and its state in `width`: the state is what the reader acts
/// on, so it is kept and the heading is cut at a word. A state wider than
/// the line, one that says why at length, takes the line and is cut at a
/// word itself.
pub(crate) fn fit_heading(
    path: &str,
    state: &str,
    width: Pixels,
    measure: impl Fn(&str) -> Pixels,
) -> (String, String) {
    let line = (width - HEADER_GUTTER).max(px(0.));
    if measure(state) > line {
        return (String::new(), cut_at_word(state, line, &measure));
    }
    let room = (line - measure(state)).max(px(0.));
    if measure(path) <= room {
        return (path.to_owned(), state.to_owned());
    }
    (cut_at_word(path, room, &measure), state.to_owned())
}

/// A long echo may use the state slot, but never the whole heading.
pub(crate) fn fit_echo_heading(
    path: &str,
    echo: &str,
    width: Pixels,
    measure: impl Fn(&str) -> Pixels,
) -> (String, String) {
    let line = (width - HEADER_GUTTER).max(px(0.));
    if measure(path) + measure(echo) <= line {
        return (path.to_owned(), echo.to_owned());
    }
    let heading = if measure(path) > line * 0.45 {
        cut_at_word(path, line * 0.45, &measure)
    } else {
        path.to_owned()
    };
    let room = (line - measure(&heading)).max(px(0.));
    let echo = if measure(echo) > room {
        cut_at_word(echo, room, &measure)
    } else {
        echo.to_owned()
    };
    (heading, echo)
}

/// `text` cut at the last word that leaves room for an ellipsis.
pub(crate) fn cut_at_word(text: &str, room: Pixels, measure: &impl Fn(&str) -> Pixels) -> String {
    let ellipsis = "…";
    let mut cut = String::new();
    for (boundary, character) in text.char_indices() {
        if !character.is_whitespace() {
            continue;
        }
        let prefix = text[..boundary].trim_end_matches(|character: char| {
            character.is_whitespace() || character == '/' || character == '·'
        });
        let candidate = format!("{prefix}{ellipsis}");
        if measure(&candidate) > room {
            break;
        }
        cut = candidate;
    }
    if cut.is_empty() && measure(ellipsis) <= room {
        cut.push_str(ellipsis);
    }
    cut
}

/// How wide `text` is drawn in `style`.
pub(crate) fn measure<'a>(
    style: &'a gpui::TextStyle,
    window: &'a Window,
) -> impl Fn(&str) -> Pixels + 'a {
    let font_size = style.font_size.to_pixels(window.rem_size());
    let font = style.font();
    move |text: &str| {
        window
            .text_system()
            .shape_line(
                text.to_owned().into(),
                font_size,
                &[gpui::TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: style.color,
                    ..Default::default()
                }],
                None,
            )
            .width
    }
}

/// Which ends of its buffer the surface is scrolled to. A flick only moves
/// through the history from an end, so a flick in the middle scrolls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollEdge {
    Top,
    Bottom,
    Both,
    Middle,
}

impl ScrollEdge {
    fn permits(self, direction: PhoneFlickDirection) -> bool {
        matches!(
            (self, direction),
            (Self::Top | Self::Both, PhoneFlickDirection::Down)
                | (Self::Bottom | Self::Both, PhoneFlickDirection::Up)
        )
    }
}

/// One finger on the surface, from where it started.
struct Gesture {
    id: TouchId,
    start: Point<Pixels>,
    started_at: Duration,
    position: Point<Pixels>,
    timestamp: Duration,
    edge: ScrollEdge,
}

impl Gesture {
    fn new(event: &TouchEvent, edge: ScrollEdge) -> Self {
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

    /// Which way the finger is going, once it is plainly vertical.
    fn direction(&self) -> Option<PhoneFlickDirection> {
        let dy = (self.position.y - self.start.y).as_f32();
        let dx = (self.position.x - self.start.x).as_f32();
        if dy.abs() < FLICK_SLOP || dy.abs() < dx.abs() * 1.25 {
            return None;
        }
        Some(match dy < 0. {
            true => PhoneFlickDirection::Up,
            false => PhoneFlickDirection::Down,
        })
    }

    fn claims_touch(&self) -> bool {
        self.direction()
            .is_some_and(|direction| self.edge.permits(direction))
    }

    /// Where the flick goes on release: far enough or fast enough, from
    /// the end it started at.
    fn committed(&self, viewport_height: Pixels) -> Option<PhoneFlickDirection> {
        let direction = self.direction()?;
        if !self.edge.permits(direction) {
            return None;
        }
        let distance = (self.position.y - self.start.y).as_f32().abs();
        let elapsed = self.timestamp.saturating_sub(self.started_at).as_secs_f32();
        let velocity = match elapsed > 0. {
            true => distance / elapsed,
            false => 0.,
        };
        (distance >= viewport_height.as_f32() / 3. || velocity >= FLICK_COMMIT_VELOCITY)
            .then_some(direction)
    }
}

/// The surface sliding off after a flick, or back after one that did
/// not commit. The generation keys the animation and lets a later snap
/// supersede it.
#[derive(Clone, Copy)]
struct Snap {
    generation: u64,
    from: Pixels,
    to: Pixels,
}

/// What a phone's touches are doing now. Nothing here outlives a gesture.
#[derive(Default)]
pub(crate) struct Phone {
    contacts: BTreeSet<TouchId>,
    gesture: Option<Gesture>,
    /// How far the finger has dragged the surface.
    drag: Pixels,
    snap: Option<Snap>,
    generation: u64,
}

/// One button on the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Button {
    Back,
    Desk,
    Menu,
    /// Deal the top card.
    Open,
    Done,
    Snooze,
    More,
    Send,
    Undo,
    Cancel,
    Enter,
}

impl Button {
    fn id(self) -> &'static str {
        match self {
            Self::Back => "rho-phone-back",
            Self::Desk => "rho-phone-desk",
            Self::Menu => "rho-phone-menu",
            Self::Open => "rho-phone-open",
            Self::Done => "rho-phone-done",
            Self::Snooze => "rho-phone-snooze",
            Self::More => "rho-phone-verdicts",
            Self::Send => "rho-phone-send",
            Self::Undo => "rho-phone-undo",
            Self::Cancel => "rho-phone-cancel",
            Self::Enter => "rho-phone-enter",
        }
    }

    fn face(self) -> (&'static str, &'static str) {
        match self {
            Self::Back => ("‹", "back"),
            Self::Desk => ("⌂", "desk"),
            Self::Menu => ("☰", "menu"),
            Self::Open => ("→", "open"),
            Self::Done => ("✓", "done"),
            Self::Snooze => ("◷", "snooze"),
            Self::More => ("⋯", "more"),
            Self::Send => ("↑", "send"),
            Self::Undo => ("↶", "undo"),
            Self::Cancel => ("×", "cancel"),
            Self::Enter => ("↵", "enter"),
        }
    }
}

/// Whether this is the card itself or a screen inspecting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenCard {
    Here,
    Related,
}

/// What the bar is about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BarState {
    prompting: bool,
    composing_idea: bool,
    desk: bool,
    /// A waiting card is open.
    card: Option<OpenCard>,
    /// The surface in view has a draft to send.
    send: bool,
    writing: bool,
}

/// The bar's buttons, left to right.
fn buttons(state: BarState) -> Vec<Button> {
    use Button::*;
    match state {
        BarState {
            prompting: true, ..
        } => vec![Cancel, Enter],
        BarState {
            composing_idea: true,
            ..
        } => vec![Cancel, Send],
        BarState { desk: true, .. } => vec![Menu, Open, More],
        BarState {
            card: Some(OpenCard::Here),
            writing: true,
            ..
        } => vec![Back, Menu, Undo, Send],
        BarState {
            card,
            send,
            writing,
            ..
        } => {
            let mut buttons = match card {
                Some(OpenCard::Here) => vec![Back, Menu, Done, Snooze, More],
                Some(OpenCard::Related) => vec![Back, Done, Snooze, More, Menu],
                None => vec![Back, Desk, Menu],
            };
            if writing {
                buttons.push(Undo);
                buttons.extend(send.then_some(Send));
            }
            buttons
        }
    }
}

/// `rows` with each pick also said to the journal as `verdict` by thumb.
fn journaled(mut rows: Rows, verdict: PhoneVerdict) -> Rows {
    for row in &mut rows.rows {
        if let Pick::Run { run, .. } = &mut row.pick {
            let action = run.clone();
            *run = Rc::new(move |frame, count, window, cx| {
                record_verdict(verdict);
                action(frame, count, window, cx);
            });
        }
    }
    rows
}

fn record_verdict(verdict: PhoneVerdict) {
    rho_journal::record(rho_journal::Event::PhoneVerdict { verdict });
}

impl Frame {
    /// Which ends the surface in view is scrolled to. A surface that is no
    /// buffer is never at an end: its touches are its own.
    fn scroll_edge(&self, cx: &mut Context<Self>) -> ScrollEdge {
        let Some(editor) = self.current_surface().editor(cx) else {
            return ScrollEdge::Middle;
        };
        editor.update(cx, |editor, cx| {
            let top = editor.scroll_position(cx).y;
            let Some(visible) = editor.visible_line_count() else {
                return ScrollEdge::Middle;
            };
            let rows = f64::from(editor.max_point(cx).row().0 + 1);
            let max_top = (rows - visible).max(0.);
            match (top <= 0.25, top >= max_top - 0.25) {
                (true, true) => ScrollEdge::Both,
                (true, false) => ScrollEdge::Top,
                (false, true) => ScrollEdge::Bottom,
                (false, false) => ScrollEdge::Middle,
            }
        })
    }

    fn phone_touch(&mut self, event: &TouchEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.phase {
            TouchPhase::Started => {
                self.note_input();
                self.phone.contacts.insert(event.id);
                // One finger flicks; a second makes it a pinch, and an
                // open prompt or menu is what the reader is answering.
                let free = self.phone.contacts.len() == 1
                    && self.phone.snap.is_none()
                    && self.minibuffer.is_none()
                    && self.menu.is_none();
                self.phone.gesture = match free {
                    true => Some(Gesture::new(event, self.scroll_edge(cx))),
                    false => None,
                };
                if !free {
                    self.phone.drag = px(0.);
                }
            }
            TouchPhase::Moved => {
                let Some(gesture) = self
                    .phone
                    .gesture
                    .as_mut()
                    .filter(|gesture| gesture.id == event.id)
                else {
                    return;
                };
                gesture.update(event);
                let claims = gesture.claims_touch();
                let drag = gesture.position.y - gesture.start.y;
                // Against the end it is not at, the finger is scrolling,
                // and stays scrolling until it lifts.
                if gesture
                    .direction()
                    .is_some_and(|direction| !gesture.edge.permits(direction))
                {
                    self.phone.gesture = None;
                }
                self.phone.drag = match claims {
                    true => drag,
                    false => px(0.),
                };
                if claims {
                    window.prevent_default();
                    cx.stop_propagation();
                }
                cx.notify();
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.phone.contacts.remove(&event.id);
                let from = std::mem::replace(&mut self.phone.drag, px(0.));
                let committed = self.phone.gesture.take().and_then(|mut gesture| {
                    if gesture.id != event.id || event.phase != TouchPhase::Ended {
                        return None;
                    }
                    gesture.update(event);
                    gesture.committed(window.viewport_size().height)
                });
                match committed {
                    Some(direction) => {
                        window.prevent_default();
                        cx.stop_propagation();
                        let height = window.viewport_size().height;
                        let to = match direction {
                            PhoneFlickDirection::Up => -height,
                            PhoneFlickDirection::Down => height,
                        };
                        self.snap(from, to, Some(direction), window, cx);
                    }
                    None if from != px(0.) => self.snap(from, px(0.), None, window, cx),
                    None => {}
                }
                cx.notify();
            }
        }
    }

    /// Slides the surface from `from` to `to`, and then takes the flick
    /// when there is one, provided the surface it was made on is still the
    /// one in view.
    fn snap(
        &mut self,
        from: Pixels,
        to: Pixels,
        direction: Option<PhoneFlickDirection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.phone.generation = self.phone.generation.wrapping_add(1);
        let generation = self.phone.generation;
        self.phone.snap = Some(Snap {
            generation,
            from,
            to,
        });
        let source = self.current_address().clone();
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
                    && *this.current_address() == source
                {
                    this.flick(direction, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A committed flick is the history's up and down: up the screen is
    /// down the history, and off its newest entry the next card is dealt.
    fn flick(
        &mut self,
        direction: PhoneFlickDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.current_address().clone();
        match direction {
            PhoneFlickDirection::Up => self.forward_or_deal(window, cx),
            PhoneFlickDirection::Down => self.back(window, cx),
        }
        rho_journal::record(rho_journal::Event::PhoneFlick {
            direction,
            moved_card: *self.current_address() != before,
        });
    }

    /// The surface in view, where a finger can drag it.
    pub(crate) fn render_phone_surface(
        &self,
        surface: &AnySurface,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = div()
            .id("rho-phone-surface")
            .relative()
            .size_full()
            .child(surface.view());
        let body = match self.phone.snap {
            Some(snap) => body
                .with_animation(
                    ("rho-phone-snap", snap.generation),
                    Animation::new(SNAP_DURATION).with_easing(ease_out_quint()),
                    move |body, delta| {
                        let (from, to) = (snap.from.as_f32(), snap.to.as_f32());
                        body.top(px(from + (to - from) * delta))
                    },
                )
                .into_any_element(),
            None => body.top(self.phone.drag).into_any_element(),
        };
        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .capture_touch(cx.listener(Self::phone_touch))
            .capture_any_mouse_down(cx.listener(Self::phone_desk_row))
            .child(body)
            .into_any_element()
    }

    fn phone_desk_row(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left
            || event.modifiers.modified()
            || !self.desk_in_view()
            || self.minibuffer.is_some()
            || self.menu.is_some()
            || self.idea_editor().is_some()
            || self.phone.snap.is_some()
        {
            return;
        }
        // Claim only an actual desk row. The editor must still see clicks
        // in its draft and on its address separator.
        if self
            .desk
            .update(cx, |desk, cx| desk.tap(event.position, window, cx))
        {
            self.open_row(window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    /// The bar along the bottom edge.
    pub(crate) fn render_phone_bar(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let surface = self.current_surface().clone();
        let send = surface.phone_send(cx);
        let prompting = self.minibuffer.is_some();
        let typing = self.mode(window, cx).contains("insert");
        let composing_idea = self.idea_editor().is_some() || (typing && self.idea_here(cx));
        let mut tools = if prompting || composing_idea {
            Vec::new()
        } else {
            surface.phone_tools(typing, cx)
        };
        let state = BarState {
            prompting,
            composing_idea,
            desk: self.desk_in_view(),
            // Working in a terminal or shell keeps the navigation bar and
            // its Palette; that still offers the same related verdicts.
            card: (tools.iter().all(|tool| tool.replaces_desk) && self.phone_card(cx).is_some())
                .then(|| {
                    if surface.inspects_card() {
                        OpenCard::Related
                    } else {
                        OpenCard::Here
                    }
                }),
            send: send.is_some(),
            writing: typing,
        };
        let colors = cx.theme().colors();
        let render_tool = |tool: crate::surface::PhoneTool| {
            let color = if tool.active {
                colors.text_accent
            } else {
                colors.text_muted
            };
            div()
                .id(tool.label)
                .cursor_pointer()
                .h_full()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .text_color(color)
                .child(div().text_size(px(18.)).child(tool.icon))
                .child(div().text_size(px(11.)).child(tool.label))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.phone.snap.is_none() {
                        tool.press(window, cx);
                        cx.notify();
                    }
                    cx.stop_propagation();
                }))
                .into_any_element()
        };
        let mut items = Vec::new();
        for button in buttons(state) {
            if button == Button::Desk
                && let Some(index) = tools.iter().position(|tool| tool.replaces_desk)
            {
                items.push(render_tool(tools.remove(index)));
                continue;
            }
            let (icon, label) = button.face();
            let send = send.clone();
            items.push(
                div()
                    .id(button.id())
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.press(button, send.as_ref(), window, cx);
                        cx.stop_propagation();
                    }))
                    .into_any_element(),
            );
        }
        items.extend(tools.into_iter().map(render_tool));
        div()
            .id("rho-phone-bar")
            .flex_none()
            .h(TARGET_HEIGHT)
            .w_full()
            .flex()
            .items_stretch()
            .border_t_1()
            .border_color(colors.border_variant)
            .children(items)
            .into_any_element()
    }

    fn phone_card(&self, cx: &gpui::App) -> Option<rho_dealer::Address> {
        self.card_in_view(cx).filter(|address| {
            self.hand()
                .cards
                .iter()
                .any(|card| &card.address == address)
        })
    }

    fn press(
        &mut self,
        button: Button,
        send: Option<&PhoneSend>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A verdict mid-snap would land on the card sliding off.
        if self.phone.snap.is_some() {
            return;
        }
        let target = self.phone_card(cx);
        match button {
            Button::Back => self.back(window, cx),
            Button::Desk => self.toggle_desk(window, cx),
            Button::Menu => self.palette(window, cx),
            Button::Open => self.pull_card(window, cx),
            Button::Done => {
                if let Some(target) = &target {
                    record_verdict(PhoneVerdict::Done);
                    self.verdict(target, Verdict::Done, "done".to_owned(), window, cx);
                }
            }
            // How long is a question, the one the snooze menu asks.
            Button::Snooze => {
                if let Some(target) = &target {
                    let rows = journaled(snooze_rows(target), PhoneVerdict::Defer);
                    self.open_menu(rows, window, cx);
                }
            }
            Button::More if self.desk_in_view() => {
                let rows = Rows::new("desk")
                    .row(Row::run("b", "back", |frame, window, cx| {
                        frame.back(window, cx)
                    }))
                    .row(Row::run("i", "write an idea", |frame, window, cx| {
                        frame.prompt_idea(window, cx)
                    }))
                    .row(Row::run("u", "undo", |frame, window, cx| {
                        frame.undo_verdict(window, cx)
                    }));
                self.open_menu(rows, window, cx);
            }
            Button::More => self.verdict_menu(window, cx),
            Button::Undo => {
                if let Some(editor) = self
                    .idea_editor()
                    .or_else(|| self.current_surface().editor(cx))
                {
                    editor.update(cx, |editor, cx| {
                        editor.undo(&editor::actions::Undo, window, cx)
                    });
                }
            }
            Button::Send if self.idea_here(cx) => self.send_idea(window, cx),
            Button::Send => {
                if let Some(send) = send {
                    send(window, cx);
                }
            }
            Button::Cancel if self.idea_here(cx) && self.minibuffer.is_none() => {
                self.cancel_idea(window, cx)
            }
            Button::Cancel => self.minibuffer_cancel(window, cx),
            Button::Enter => self.minibuffer_confirm(window, cx),
        }
        cx.notify();
    }

    /// The open menu as a sheet over the window: the same rows the desk
    /// draws under the point, each a row a thumb can hit. The scrim and
    /// bottom edge dismiss it; neither is part of the row hit area.
    pub(crate) fn render_menu_sheet(
        &self,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let open = self.menu.as_ref()?;
        let colors = cx.theme().colors();
        let target = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .cursor_pointer()
                .h(TARGET_HEIGHT)
                .min_w(TARGET_HEIGHT)
                .flex()
                .items_center()
                .text_color(colors.text_muted)
                .child(label)
        };
        let dismiss = || cx.listener(|this, _, window, cx| this.close_menu(window, cx));
        let header = div()
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .min_h(TARGET_HEIGHT)
            .px_3()
            .gap_3()
            .border_b_1()
            .border_color(colors.border_variant)
            .children((!open.under.is_empty()).then(|| {
                target("rho-sheet-back", "‹ back")
                    .on_click(cx.listener(|this, _, window, cx| this.menu_dismiss(window, cx)))
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(open.rows.title.clone()),
            )
            .child(
                target("rho-sheet-close", "close")
                    .justify_end()
                    .on_click(dismiss()),
            );
        let rows = open.rows.rows.iter().enumerate().map(|(index, row)| {
            div()
                .id(("rho-menu-row", index))
                .cursor_pointer()
                .flex()
                .flex_row()
                .items_center()
                .min_h(TARGET_HEIGHT)
                .w_full()
                .px_3()
                .gap_3()
                .border_b_1()
                .border_color(colors.border_variant)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(open.transient.items()[index].description().to_owned()),
                )
                .children(
                    row.pick
                        .value()
                        .map(|value| div().text_color(colors.text_muted).child(value.to_owned())),
                )
                .children(
                    matches!(row.pick, Pick::Menu(_))
                        .then(|| div().text_color(colors.text_muted).child("›")),
                )
                // The key is still the key: a phone with a keyboard
                // presses it the way the desk does.
                .child(
                    div()
                        .text_color(colors.terminal_ansi_bright_cyan)
                        .child(row.key.clone()),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.click_menu_row(index, window, cx);
                }))
        });
        let sheet = crate::minibuffer::bottom_strip(text_style, cx)
            .occlude()
            .max_h(gpui::relative(0.82))
            .child(header)
            .child(
                div()
                    .id("rho-sheet-rows")
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            );
        let cancel = crate::minibuffer::bottom_strip(text_style, cx)
            .absolute()
            .bottom_0()
            .py_0()
            .occlude()
            .child(
                target("rho-sheet-cancel", "cancel")
                    .justify_center()
                    .on_click(dismiss()),
            );
        Some(
            div()
                .track_focus(&self.menu_focus)
                .absolute()
                .inset_0()
                // Keep the scrim a sibling, not a click listener on the
                // sheet's parent. A press outside can't arm a menu row.
                .child(
                    div()
                        .id("rho-sheet-backdrop")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .bg(gpui::black().opacity(0.35))
                        .on_click(dismiss()),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .bottom(TARGET_HEIGHT)
                        .flex()
                        .flex_col()
                        .justify_end()
                        .child(sheet),
                )
                // Cover the work's bar with a dismiss target, not a command row.
                .child(cancel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_a_phone_up_to_600px_wide() {
        assert_eq!(Layout::of(px(360.)), Layout::Phone);
        assert_eq!(Layout::of(px(600.)), Layout::Phone);
        assert_eq!(Layout::of(px(600.5)), Layout::Desktop);
        assert_eq!(Layout::of(px(1280.)), Layout::Desktop);
    }

    #[test]
    fn a_360px_heading_is_cut_at_a_word_before_its_state() {
        let state = "needs reply · 1.9h";
        let measure = |text: &str| px(text.chars().count() as f32 * 8.);
        let (path, kept) = fit_heading(
            "product strategy / deeply nested launch readiness review",
            state,
            px(360.),
            measure,
        );
        assert_eq!(path, "product strategy…");
        assert_eq!(kept, state);
        assert!(measure(&path) + measure(&kept) + HEADER_GUTTER <= px(360.));
        // Room enough, nothing is cut.
        let (whole, _) = fit_heading("short", state, px(360.), measure);
        assert_eq!(whole, "short");
    }

    /// A stop that says why can be wider than the phone: it takes the line,
    /// cut at a word, rather than running under the mode word.
    #[test]
    fn a_state_wider_than_the_line_is_cut_and_the_heading_gives_way() {
        let measure = |text: &str| px(text.chars().count() as f32 * 8.);
        let state = "failed · now · The model refused the request: the conte…";
        let (path, kept) = fit_heading("rho-web / FAILME native try", state, px(360.), measure);
        assert_eq!(path, "");
        assert_eq!(kept, "failed · now · The model refused the…");
        assert!(measure(&kept) + HEADER_GUTTER <= px(360.));
    }

    fn touch(phase: TouchPhase, y: f32, milliseconds: u64) -> TouchEvent {
        TouchEvent {
            id: TouchId(1),
            phase,
            position: gpui::point(px(100.), px(y)),
            timestamp: Duration::from_millis(milliseconds),
            ..Default::default()
        }
    }

    fn released(edge: ScrollEdge, to_y: f32, milliseconds: u64) -> Option<PhoneFlickDirection> {
        let mut gesture = Gesture::new(&touch(TouchPhase::Started, 500., 0), edge);
        gesture.update(&touch(TouchPhase::Ended, to_y, milliseconds));
        gesture.committed(px(600.))
    }

    #[test]
    fn a_flick_moves_only_from_the_end_it_is_flicked_away_from() {
        // 150px up in 100ms: fast enough, from the bottom or a buffer that
        // fits, and not from the top or the middle.
        assert_eq!(
            released(ScrollEdge::Bottom, 350., 100),
            Some(PhoneFlickDirection::Up)
        );
        assert_eq!(
            released(ScrollEdge::Both, 350., 100),
            Some(PhoneFlickDirection::Up)
        );
        assert_eq!(released(ScrollEdge::Top, 350., 100), None);
        assert_eq!(released(ScrollEdge::Middle, 350., 100), None);
        // And down, from the top only.
        assert_eq!(
            released(ScrollEdge::Top, 650., 100),
            Some(PhoneFlickDirection::Down)
        );
        assert_eq!(released(ScrollEdge::Bottom, 650., 100), None);
    }

    #[test]
    fn a_flick_commits_on_a_third_of_the_screen_or_900px_a_second() {
        // 150px in a second: neither far nor fast.
        assert_eq!(released(ScrollEdge::Bottom, 350., 1000), None);
        // 250px of 600 is past a third, however slow.
        assert_eq!(
            released(ScrollEdge::Bottom, 250., 2000),
            Some(PhoneFlickDirection::Up)
        );
        // 60px in 50ms is 1200px a second.
        assert_eq!(
            released(ScrollEdge::Bottom, 440., 50),
            Some(PhoneFlickDirection::Up)
        );
        // 60px in 100ms is 600px a second: short and slow.
        assert_eq!(released(ScrollEdge::Bottom, 440., 100), None);
        // Inside the slop it is a tap, however fast.
        assert_eq!(released(ScrollEdge::Both, 490., 1), None);
    }

    #[test]
    fn a_sideways_drag_is_no_flick() {
        let mut gesture = Gesture::new(&touch(TouchPhase::Started, 500., 0), ScrollEdge::Both);
        let mut moved = touch(TouchPhase::Ended, 300., 100);
        moved.position.x = px(400.);
        gesture.update(&moved);
        assert_eq!(gesture.committed(px(600.)), None);
    }

    #[test]
    fn the_bar_offers_what_the_state_can_take() {
        use Button::*;
        let desk = BarState {
            desk: true,
            ..BarState::default()
        };
        assert_eq!(buttons(desk), [Menu, Open, More]);
        assert_eq!(
            buttons(BarState {
                card: Some(OpenCard::Here),
                ..desk
            }),
            [Menu, Open, More]
        );
        let card = BarState {
            card: Some(OpenCard::Here),
            ..BarState::default()
        };
        assert_eq!(buttons(card), [Back, Menu, Done, Snooze, More]);
        let thread = BarState { send: true, ..card };
        assert_eq!(buttons(thread), [Back, Menu, Done, Snooze, More]);
        assert_eq!(
            buttons(BarState {
                writing: true,
                ..thread
            }),
            [Back, Menu, Undo, Send]
        );
        assert_eq!(
            buttons(BarState {
                card: Some(OpenCard::Related),
                ..BarState::default()
            }),
            [Back, Done, Snooze, More, Menu]
        );
        assert_eq!(buttons(BarState::default()), [Back, Desk, Menu]);
        assert_eq!(
            buttons(BarState {
                composing_idea: true,
                ..thread
            }),
            [Cancel, Send]
        );
        assert_eq!(
            buttons(BarState {
                prompting: true,
                ..thread
            }),
            [Cancel, Enter]
        );
    }
}
