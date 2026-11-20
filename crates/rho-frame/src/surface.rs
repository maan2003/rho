//! What a screen is: a surface the frame shows, keeps in history and
//! asks about.

use std::rc::Rc;

use editor::Editor;
use gpui::{AnyView, App, Context, Entity, EntityId, FocusHandle, Focusable, Render, Window};
use rho_dealer::{Address, Card};

use crate::menu::Row;

/// The action under Send on the phone bar, when a surface has a draft to send.
pub type PhoneSend = Rc<dyn Fn(&mut Window, &mut App)>;

/// What a verdict, a note or `file` pressed over the surface lands on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// The thing the surface is of: its own address.
    Own,
    /// Something the surface lists: the row under the point.
    Address(Address),
    /// Nothing: a log, a picker, a page about nothing in particular.
    None,
}

/// Where the keys go on a surface that keeps a mode of its own rather
/// than being a buffer: what the status line's mode word says for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The keys move through the screen.
    Normal,
    /// The keys are typed into the screen.
    Insert,
}

/// One surface-specific key on the phone's keyboard bar.
#[derive(Clone)]
pub struct PhoneTool {
    pub icon: &'static str,
    pub label: &'static str,
    pub active: bool,
    pub(crate) replaces_desk: bool,
    run: Rc<dyn Fn(&mut Window, &mut App)>,
}

impl PhoneTool {
    pub fn new(
        icon: &'static str,
        label: &'static str,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            icon,
            label,
            active: false,
            replaces_desk: false,
            run: Rc::new(run),
        }
    }

    /// Use the Desk slot when the bar has one. The card bar keeps its
    /// own actions.
    pub fn in_desk_slot(mut self) -> Self {
        self.replaces_desk = true;
        self
    }

    pub(crate) fn press(&self, window: &mut Window, cx: &mut App) {
        (self.run)(window, cx);
    }
}

/// A view the frame can show. Its address is the frame's to know; the
/// surface says what it is called, what it is a buffer of, and what the
/// point is on.
pub trait Surface: Render + Focusable + Sized + 'static {
    fn title(&self, cx: &App) -> String;

    fn phone_titles(&self, cx: &App) -> Vec<String> {
        vec![self.title(cx)]
    }

    fn status(&self, _cx: &App) -> Option<String> {
        None
    }

    fn phone_tools(&self, _typing: bool, _cx: &App) -> Vec<PhoneTool> {
        Vec::new()
    }

    /// The editor the surface is a buffer of, when it is one: what vim,
    /// search and the chrome's text style read.
    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        None
    }

    fn subject(&self, _cx: &App) -> Subject {
        Subject::Own
    }

    /// The mode the surface is in, for a surface that keeps its own;
    /// `None` leaves the mode word to the editor that has the keys.
    fn mode(&self, _cx: &App) -> Option<Mode> {
        None
    }

    /// The card a verdict pressed over the surface is about: what holds
    /// the subject as the dealer counts it. A thread is one card wherever
    /// the point is in it; a list's row is its own card.
    fn card(&self, cx: &App) -> Subject {
        self.subject(cx)
    }

    /// Whether the point is in the surface's writable text rather than on
    /// the reading side. Only there does the frame redo an editor edit.
    fn edits_at_point(&self, _cx: &App) -> bool {
        false
    }

    /// What the status line calls the surface while it is the card in
    /// view. By default the card's own heading; a surface that knows where
    /// the point is in it, such as the message under the cursor, says that
    /// instead.
    fn heading(&self, _cx: &App) -> Option<String> {
        None
    }

    /// An explicit open, distinct from returning through history.
    fn opened(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    /// Positions the screen on what the card deals, including when the
    /// frame already holds the screen in history.
    fn show_card(&mut self, _card: &Card, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn show_search_match(
        &mut self,
        point: &crate::SearchPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let crate::SearchPoint::Line(row) = *point else {
            return;
        };
        if let Some(editor) = self.editor(cx) {
            editor.update(cx, |editor, cx| {
                let point = language::Point::new(row, 0);
                editor.change_selections(
                    editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([point..point]),
                );
            });
        }
    }

    fn phone_send(&mut self, _cx: &mut Context<Self>) -> Option<PhoneSend> {
        None
    }

    /// The surface's own rows for the palette, for what is in view.
    fn rows(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Vec<Row> {
        Vec::new()
    }
}

trait SurfaceHandle {
    fn view(&self) -> AnyView;
    fn entity_id(&self) -> EntityId;
    fn title(&self, cx: &App) -> String;
    fn phone_titles(&self, cx: &App) -> Vec<String>;
    fn status(&self, cx: &App) -> Option<String>;
    fn phone_tools(&self, typing: bool, cx: &App) -> Vec<PhoneTool>;
    fn editor(&self, cx: &App) -> Option<Entity<Editor>>;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn subject(&self, cx: &App) -> Subject;
    fn mode(&self, cx: &App) -> Option<Mode>;
    fn card(&self, cx: &App) -> Subject;
    fn edits_at_point(&self, cx: &App) -> bool;
    fn heading(&self, cx: &App) -> Option<String>;
    fn opened(&self, window: &mut Window, cx: &mut App);
    fn show_card(&self, card: &Card, window: &mut Window, cx: &mut App);
    fn show_search_match(&self, point: &crate::SearchPoint, window: &mut Window, cx: &mut App);
    fn rows(&self, window: &mut Window, cx: &mut App) -> Vec<Row>;
    fn phone_send(&self, cx: &mut App) -> Option<PhoneSend>;
}

impl<S: Surface> SurfaceHandle for Entity<S> {
    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn entity_id(&self) -> EntityId {
        Entity::entity_id(self)
    }

    fn title(&self, cx: &App) -> String {
        self.read(cx).title(cx)
    }

    fn phone_titles(&self, cx: &App) -> Vec<String> {
        self.read(cx).phone_titles(cx)
    }

    fn status(&self, cx: &App) -> Option<String> {
        self.read(cx).status(cx)
    }

    fn phone_tools(&self, typing: bool, cx: &App) -> Vec<PhoneTool> {
        self.read(cx).phone_tools(typing, cx)
    }

    fn editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.read(cx).editor(cx)
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn subject(&self, cx: &App) -> Subject {
        self.read(cx).subject(cx)
    }

    fn mode(&self, cx: &App) -> Option<Mode> {
        self.read(cx).mode(cx)
    }

    fn card(&self, cx: &App) -> Subject {
        self.read(cx).card(cx)
    }

    fn edits_at_point(&self, cx: &App) -> bool {
        self.read(cx).edits_at_point(cx)
    }

    fn heading(&self, cx: &App) -> Option<String> {
        self.read(cx).heading(cx)
    }

    fn opened(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |surface, cx| surface.opened(window, cx));
    }

    fn show_card(&self, card: &Card, window: &mut Window, cx: &mut App) {
        self.update(cx, |surface, cx| surface.show_card(card, window, cx));
    }

    fn show_search_match(&self, point: &crate::SearchPoint, window: &mut Window, cx: &mut App) {
        self.update(cx, |surface, cx| {
            surface.show_search_match(point, window, cx)
        })
    }

    fn rows(&self, window: &mut Window, cx: &mut App) -> Vec<Row> {
        self.update(cx, |surface, cx| surface.rows(window, cx))
    }

    fn phone_send(&self, cx: &mut App) -> Option<PhoneSend> {
        self.update(cx, |surface, cx| surface.phone_send(cx))
    }
}

/// A surface of any type, as the frame holds it.
type RelatedCard = Rc<dyn Fn(&App) -> Option<Address>>;

#[derive(Clone)]
pub struct AnySurface(Rc<dyn SurfaceHandle>, Option<RelatedCard>);

impl AnySurface {
    pub fn new<S: Surface>(entity: Entity<S>) -> Self {
        Self(Rc::new(entity), None)
    }

    /// The screen is for inspecting another place's card, not for leaving
    /// the screen when that card is settled. `None` means there is no such
    /// place.
    pub fn with_related_card(mut self, find: impl Fn(&App) -> Option<Address> + 'static) -> Self {
        self.1 = Some(Rc::new(find));
        self
    }

    pub fn inspects_card(&self) -> bool {
        self.1.is_some()
    }

    pub fn view(&self) -> AnyView {
        self.0.view()
    }

    pub fn entity_id(&self) -> EntityId {
        self.0.entity_id()
    }

    pub fn title(&self, cx: &App) -> String {
        self.0.title(cx)
    }

    pub(crate) fn phone_titles(&self, cx: &App) -> Vec<String> {
        self.0.phone_titles(cx)
    }

    pub fn status(&self, cx: &App) -> Option<String> {
        self.0.status(cx)
    }

    pub(crate) fn phone_tools(&self, typing: bool, cx: &App) -> Vec<PhoneTool> {
        self.0.phone_tools(typing, cx)
    }

    pub fn editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.0.editor(cx)
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.0.focus_handle(cx)
    }

    pub fn subject(&self, cx: &App) -> Subject {
        self.0.subject(cx)
    }

    pub fn mode(&self, cx: &App) -> Option<Mode> {
        self.0.mode(cx)
    }

    pub fn card(&self, cx: &App) -> Subject {
        self.1.as_ref().map_or_else(
            || self.0.card(cx),
            |find| find(cx).map_or(Subject::None, Subject::Address),
        )
    }

    pub(crate) fn edits_at_point(&self, cx: &App) -> bool {
        self.0.edits_at_point(cx)
    }

    pub fn heading(&self, cx: &App) -> Option<String> {
        self.0.heading(cx)
    }

    pub(crate) fn opened(&self, window: &mut Window, cx: &mut App) {
        self.0.opened(window, cx);
    }

    pub(crate) fn show_card(&self, card: &Card, window: &mut Window, cx: &mut App) {
        self.0.show_card(card, window, cx);
    }

    pub(crate) fn show_search_match(
        &self,
        point: &crate::SearchPoint,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0.show_search_match(point, window, cx)
    }

    pub(crate) fn phone_send(&self, cx: &mut App) -> Option<PhoneSend> {
        self.0.phone_send(cx)
    }

    pub fn rows(&self, window: &mut Window, cx: &mut App) -> Vec<Row> {
        self.0.rows(window, cx)
    }
}

impl PartialEq for AnySurface {
    fn eq(&self, other: &Self) -> bool {
        self.entity_id() == other.entity_id()
    }
}
