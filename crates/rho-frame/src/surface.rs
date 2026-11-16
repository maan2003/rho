//! What a screen is: a surface the frame shows, keeps in history and
//! asks about.

use std::rc::Rc;

use editor::Editor;
use gpui::{AnyView, App, Context, Entity, EntityId, FocusHandle, Focusable, Render, Window};
use rho_dealer::Address;

use crate::menu::Row;

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

/// A view the frame can show. Its address is the frame's to know; the
/// surface says what it is called, what it is a buffer of, and what the
/// point is on.
pub trait Surface: Render + Focusable + Sized + 'static {
    fn title(&self, cx: &App) -> String;

    /// The editor the surface is a buffer of, when it is one: what vim,
    /// search and the chrome's text style read.
    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        None
    }

    fn subject(&self, _cx: &App) -> Subject {
        Subject::Own
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
    fn editor(&self, cx: &App) -> Option<Entity<Editor>>;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn subject(&self, cx: &App) -> Subject;
    fn rows(&self, window: &mut Window, cx: &mut App) -> Vec<Row>;
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

    fn editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.read(cx).editor(cx)
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn subject(&self, cx: &App) -> Subject {
        self.read(cx).subject(cx)
    }

    fn rows(&self, window: &mut Window, cx: &mut App) -> Vec<Row> {
        self.update(cx, |surface, cx| surface.rows(window, cx))
    }
}

/// A surface of any type, as the frame holds it.
#[derive(Clone)]
pub struct AnySurface(Rc<dyn SurfaceHandle>);

impl AnySurface {
    pub fn new<S: Surface>(entity: Entity<S>) -> Self {
        Self(Rc::new(entity))
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

    pub fn editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.0.editor(cx)
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.0.focus_handle(cx)
    }

    pub fn subject(&self, cx: &App) -> Subject {
        self.0.subject(cx)
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
