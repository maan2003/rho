//! What plugs into the frame.
//!
//! A system owns a few kinds of address and everything about them: the
//! surfaces that open them, the rows a menu offers over them, what of
//! them wants the user, the status changes the desk lists, what `find` finds,
//! and the streams it keeps to each host. The frame holds systems as a list and
//! asks each in turn; a system never names another.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Context, Entity, Window};
use jiff::Timestamp;
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_agent_types::{AgentId, EndpointId};
use rho_dealer::rank::Trace;
use rho_dealer::{Address, Claim, Marks};
use url::Url;

use crate::frame::Point;
use crate::idea::{IdeaMail, IdeaProject, NewIdeaProject, ProjectCreated};
use crate::menu::Row;
use crate::surface::AnySurface;
use crate::text_search::{TextHit, TextSearch};

/// What a system has for an address it was asked to open.
pub enum Opening {
    /// The surface for it.
    Surface(AnySurface),
    /// Nothing yet: these hosts have not yet said whether they hold it.
    /// The frame keeps the address and shows that it waits, and asks
    /// again when the system changes.
    Waiting(Vec<HostId>),
    /// Nothing: no host that could hold it does.
    Gone,
}

impl From<Option<AnySurface>> for Opening {
    /// A surface, or gone: for a system whose addresses never wait on a host.
    fn from(surface: Option<AnySurface>) -> Self {
        surface.map_or(Self::Gone, Self::Surface)
    }
}

/// A place the palette lists, which it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub address: Address,
    pub title: String,
    pub description: String,
    /// A thread and its current status (if any), shown before its project
    /// address.
    pub thread: Option<FoundThread>,
    /// When it last moved, for ordering unvisited places; zero when nothing
    /// says.
    pub recency: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundThread {
    pub status: Option<String>,
}

/// An exact location in the owning surface, not a separate place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchPoint {
    Message { message: Address, line: u32 },
    Line(u32),
    Draft(u32),
}

/// The displayed words of a shortlisted text hit and its place.
pub struct FoundText {
    pub address: Address,
    pub title: String,
    pub line: String,
    pub point: SearchPoint,
}

pub type TextPresenter<'a> = Box<dyn FnMut(&TextHit) -> Option<FoundText> + 'a>;

pub trait System: Sized + 'static {
    /// The address kinds this system opens.
    fn kinds(&self) -> &'static [&'static str];

    /// The surface for an address of one of its kinds, or why there is
    /// none. A system whose hosts have not yet said what they hold waits
    /// rather than calls the address gone, and says the frame changed
    /// (`FrameLink::changed`) once they have.
    fn open(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) -> Opening;

    /// Take a named jump whose destination resolves asynchronously. The
    /// window keeps its current surface until the system opens the target.
    fn visit(&mut self, _address: &Address, _cx: &mut Context<Self>) -> bool {
        false
    }

    /// The foreground surface, independent of where an overlay takes keys.
    /// `None` means that the window is inactive.
    fn active_surface(&mut self, _address: Option<&Address>, _cx: &mut Context<Self>) {}

    /// The streams this system keeps to a host, opened on every connect.
    /// `name` is what the user calls the host, `endpoint` its identity as
    /// far as the client knows before the host answers.
    fn streams(
        &mut self,
        _host: HostId,
        _name: &str,
        _endpoint: Option<EndpointId>,
        _link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        Vec::new()
    }

    fn detached(&mut self, _host: HostId, _cx: &mut Context<Self>) {}

    /// Rows for the palette, given what is in view.
    fn rows(&mut self, _point: &Point, _window: &mut Window, _cx: &mut Context<Self>) -> Vec<Row> {
        Vec::new()
    }

    /// What of this system's wants the user right now.
    fn claims(
        &self,
        _marks: &Marks,
        _now: Timestamp,
        _trace: Option<&mut Trace>,
        _cx: &App,
    ) -> Vec<Claim> {
        Vec::new()
    }

    /// Opens a web link the user chose in a message, when this system is
    /// the one that shows the web; `false` leaves it to the next.
    fn open_link(&mut self, _url: &Url, _cx: &mut Context<Self>) -> bool {
        false
    }

    /// What an address of this system is called and where it is, for a
    /// card no claim named. `marks` are the frame's, which a system cannot
    /// read for itself while the frame draws.
    fn name(&self, _address: &Address, _marks: &Marks, _cx: &App) -> Option<(String, String)> {
        None
    }

    /// What the window calls an agent, such as `eng-b8os`, where a host
    /// asks on its behalf; `None` from a system that does not hold it.
    fn agent_name(&self, _agent: AgentId, _cx: &App) -> Option<String> {
        None
    }

    /// The screen that shows `address` as one of its lines, when the
    /// system has one: a message, the thread it is read in.
    fn shown_in(&self, _address: &Address, _cx: &App) -> Option<Address> {
        None
    }

    /// A quiet, current line for one waiting row. The card itself is unchanged.
    fn desk_context(&self, _address: &Address, _cx: &App) -> Option<String> {
        None
    }

    fn idea_projects(&self, _cx: &App) -> Vec<IdeaProject> {
        Vec::new()
    }

    fn idea_project(&self, _address: &Address, _cx: &App) -> Option<IdeaProject> {
        None
    }

    fn last_idea_project(&self, _cx: &App) -> Option<IdeaProject> {
        None
    }

    fn idea_project_hosts(&self, _cx: &App) -> Vec<(EndpointId, String)> {
        Vec::new()
    }

    fn create_idea_project(
        &mut self,
        _project: NewIdeaProject,
        _created: ProjectCreated,
        _cx: &mut Context<Self>,
    ) {
    }

    fn send_idea(&mut self, _mail: IdeaMail, _cx: &mut Context<Self>) {}

    /// Every place of the system's that the palette lists by title. The
    /// frame joins them with the current hand and visit history.
    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        Vec::new()
    }

    fn text_sources(&self, _search: &mut TextSearch, _current: &Address, _cx: &App) {}

    fn text_presenter(&self, _cx: &App) -> Option<TextPresenter<'_>> {
        None
    }

    /// What the user said about these addresses moved, on this device or
    /// from another. Asked with the frame busy: a system reads the marks
    /// afterwards, through [`crate::FrameLink::read`].
    fn marks_moved(&mut self, _touched: &BTreeSet<Address>, _cx: &mut Context<Self>) {}
}

trait SystemHandle {
    fn kinds(&self, cx: &App) -> &'static [&'static str];
    fn open(&self, address: &Address, window: &mut Window, cx: &mut App) -> Opening;
    fn visit(&self, address: &Address, cx: &mut App) -> bool;

    fn active_surface(&self, address: Option<&Address>, cx: &mut App);
    fn streams(
        &self,
        host: HostId,
        name: &str,
        endpoint: Option<EndpointId>,
        link: &Link,
        cx: &mut App,
    ) -> Vec<Arc<dyn HostStream>>;
    fn detached(&self, host: HostId, cx: &mut App);
    fn rows(&self, point: &Point, window: &mut Window, cx: &mut App) -> Vec<Row>;
    fn claims(
        &self,
        marks: &Marks,
        now: Timestamp,
        trace: Option<&mut Trace>,
        cx: &App,
    ) -> Vec<Claim>;
    fn open_link(&self, url: &Url, cx: &mut App) -> bool;
    fn name(&self, address: &Address, marks: &Marks, cx: &App) -> Option<(String, String)>;
    fn agent_name(&self, agent: AgentId, cx: &App) -> Option<String>;
    fn shown_in(&self, address: &Address, cx: &App) -> Option<Address>;

    fn desk_context(&self, address: &Address, cx: &App) -> Option<String>;

    fn idea_projects(&self, cx: &App) -> Vec<IdeaProject>;

    fn idea_project(&self, address: &Address, cx: &App) -> Option<IdeaProject>;

    fn last_idea_project(&self, cx: &App) -> Option<IdeaProject>;

    fn idea_project_hosts(&self, cx: &App) -> Vec<(EndpointId, String)>;
    fn create_idea_project(&self, project: NewIdeaProject, created: ProjectCreated, cx: &mut App);

    fn send_idea(&self, mail: IdeaMail, cx: &mut App);

    fn find(&self, marks: &Marks, cx: &App) -> Vec<Found>;
    fn marks_moved(&self, touched: &BTreeSet<Address>, cx: &mut App);
    fn text_sources(&self, search: &mut TextSearch, current: &Address, cx: &App);
    fn text_presenter<'a>(&'a self, cx: &'a App) -> Option<TextPresenter<'a>>;
}

impl<S: System> SystemHandle for Entity<S> {
    fn kinds(&self, cx: &App) -> &'static [&'static str] {
        self.read(cx).kinds()
    }

    fn open(&self, address: &Address, window: &mut Window, cx: &mut App) -> Opening {
        self.update(cx, |system, cx| system.open(address, window, cx))
    }

    fn visit(&self, address: &Address, cx: &mut App) -> bool {
        self.update(cx, |system, cx| system.visit(address, cx))
    }

    fn active_surface(&self, address: Option<&Address>, cx: &mut App) {
        self.update(cx, |system, cx| system.active_surface(address, cx));
    }

    fn streams(
        &self,
        host: HostId,
        name: &str,
        endpoint: Option<EndpointId>,
        link: &Link,
        cx: &mut App,
    ) -> Vec<Arc<dyn HostStream>> {
        self.update(cx, |system, cx| {
            system.streams(host, name, endpoint, link, cx)
        })
    }

    fn detached(&self, host: HostId, cx: &mut App) {
        self.update(cx, |system, cx| system.detached(host, cx))
    }

    fn rows(&self, point: &Point, window: &mut Window, cx: &mut App) -> Vec<Row> {
        self.update(cx, |system, cx| system.rows(point, window, cx))
    }

    fn claims(
        &self,
        marks: &Marks,
        now: Timestamp,
        trace: Option<&mut Trace>,
        cx: &App,
    ) -> Vec<Claim> {
        self.read(cx).claims(marks, now, trace, cx)
    }

    fn open_link(&self, url: &Url, cx: &mut App) -> bool {
        self.update(cx, |system, cx| system.open_link(url, cx))
    }

    fn name(&self, address: &Address, marks: &Marks, cx: &App) -> Option<(String, String)> {
        self.read(cx).name(address, marks, cx)
    }

    fn agent_name(&self, agent: AgentId, cx: &App) -> Option<String> {
        self.read(cx).agent_name(agent, cx)
    }

    fn shown_in(&self, address: &Address, cx: &App) -> Option<Address> {
        self.read(cx).shown_in(address, cx)
    }

    fn desk_context(&self, address: &Address, cx: &App) -> Option<String> {
        self.read(cx).desk_context(address, cx)
    }

    fn idea_projects(&self, cx: &App) -> Vec<IdeaProject> {
        self.read(cx).idea_projects(cx)
    }

    fn idea_project(&self, address: &Address, cx: &App) -> Option<IdeaProject> {
        self.read(cx).idea_project(address, cx)
    }

    fn last_idea_project(&self, cx: &App) -> Option<IdeaProject> {
        self.read(cx).last_idea_project(cx)
    }

    fn idea_project_hosts(&self, cx: &App) -> Vec<(EndpointId, String)> {
        self.read(cx).idea_project_hosts(cx)
    }

    fn create_idea_project(&self, project: NewIdeaProject, created: ProjectCreated, cx: &mut App) {
        self.update(cx, |system, cx| {
            system.create_idea_project(project, created, cx)
        })
    }

    fn send_idea(&self, mail: IdeaMail, cx: &mut App) {
        self.update(cx, |system, cx| system.send_idea(mail, cx))
    }

    fn find(&self, marks: &Marks, cx: &App) -> Vec<Found> {
        self.read(cx).find(marks, cx)
    }

    fn text_sources(&self, search: &mut TextSearch, current: &Address, cx: &App) {
        self.read(cx).text_sources(search, current, cx);
    }

    fn text_presenter<'a>(&'a self, cx: &'a App) -> Option<TextPresenter<'a>> {
        self.read(cx).text_presenter(cx)
    }

    fn marks_moved(&self, touched: &BTreeSet<Address>, cx: &mut App) {
        self.update(cx, |system, cx| system.marks_moved(touched, cx))
    }
}

/// A system of any type, as the frame holds it.
#[derive(Clone)]
pub struct AnySystem(Rc<dyn SystemHandle>);

impl AnySystem {
    pub fn new<S: System>(entity: Entity<S>) -> Self {
        Self(Rc::new(entity))
    }

    pub fn opens(&self, address: &Address, cx: &App) -> bool {
        self.0.kinds(cx).contains(&address.kind())
    }

    pub fn open(&self, address: &Address, window: &mut Window, cx: &mut App) -> Opening {
        self.0.open(address, window, cx)
    }

    pub(crate) fn active_surface(&self, address: Option<&Address>, cx: &mut App) {
        self.0.active_surface(address, cx);
    }

    pub fn streams(
        &self,
        host: HostId,
        name: &str,
        endpoint: Option<EndpointId>,
        link: &Link,
        cx: &mut App,
    ) -> Vec<Arc<dyn HostStream>> {
        self.0.streams(host, name, endpoint, link, cx)
    }

    pub fn detached(&self, host: HostId, cx: &mut App) {
        self.0.detached(host, cx)
    }

    pub fn rows(&self, point: &Point, window: &mut Window, cx: &mut App) -> Vec<Row> {
        self.0.rows(point, window, cx)
    }

    pub fn claims(
        &self,
        marks: &Marks,
        now: Timestamp,
        trace: Option<&mut Trace>,
        cx: &App,
    ) -> Vec<Claim> {
        self.0.claims(marks, now, trace, cx)
    }

    pub(crate) fn visit(&self, address: &Address, cx: &mut App) -> bool {
        self.0.visit(address, cx)
    }

    pub fn open_link(&self, url: &Url, cx: &mut App) -> bool {
        self.0.open_link(url, cx)
    }

    pub fn name(&self, address: &Address, marks: &Marks, cx: &App) -> Option<(String, String)> {
        self.0.name(address, marks, cx)
    }

    pub fn agent_name(&self, agent: AgentId, cx: &App) -> Option<String> {
        self.0.agent_name(agent, cx)
    }

    pub fn shown_in(&self, address: &Address, cx: &App) -> Option<Address> {
        self.0.shown_in(address, cx)
    }

    pub(crate) fn desk_context(&self, address: &Address, cx: &App) -> Option<String> {
        self.0.desk_context(address, cx)
    }

    pub(crate) fn idea_projects(&self, cx: &App) -> Vec<IdeaProject> {
        self.0.idea_projects(cx)
    }

    pub(crate) fn idea_project(&self, address: &Address, cx: &App) -> Option<IdeaProject> {
        self.0.idea_project(address, cx)
    }

    pub(crate) fn last_idea_project(&self, cx: &App) -> Option<IdeaProject> {
        self.0.last_idea_project(cx)
    }

    pub(crate) fn idea_project_hosts(&self, cx: &App) -> Vec<(EndpointId, String)> {
        self.0.idea_project_hosts(cx)
    }

    pub(crate) fn create_idea_project(
        &self,
        project: NewIdeaProject,
        created: ProjectCreated,
        cx: &mut App,
    ) {
        self.0.create_idea_project(project, created, cx)
    }

    pub(crate) fn send_idea(&self, mail: IdeaMail, cx: &mut App) {
        self.0.send_idea(mail, cx)
    }

    pub fn find(&self, marks: &Marks, cx: &App) -> Vec<Found> {
        self.0.find(marks, cx)
    }

    pub(crate) fn text_sources(&self, search: &mut TextSearch, current: &Address, cx: &App) {
        self.0.text_sources(search, current, cx);
    }

    pub(crate) fn text_presenter<'a>(&'a self, cx: &'a App) -> Option<TextPresenter<'a>> {
        self.0.text_presenter(cx)
    }

    pub fn marks_moved(&self, touched: &BTreeSet<Address>, cx: &mut App) {
        self.0.marks_moved(touched, cx)
    }
}
