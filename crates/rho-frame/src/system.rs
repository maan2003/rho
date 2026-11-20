//! What plugs into the frame.
//!
//! A system owns a few kinds of address and everything about them: the
//! surfaces that open them, the rows a menu offers over them, what of
//! them wants the user, the rooms Home lists, what `find` finds, and the
//! streams it keeps to each host. The frame holds systems as a list and asks
//! each in turn; a system never names another.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Context, Entity, Window};
use jiff::Timestamp;
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_agent_types::EndpointId;
use rho_dealer::facts::Seen;
use rho_dealer::rank::Trace;
use rho_dealer::{Address, Claim, Marks};
use url::Url;

use crate::frame::Point;
use crate::menu::Row;
use crate::surface::AnySurface;

/// One room: a place the user works in, as the system that holds it lists
/// it on Home. Home orders the rooms by attention: a room with a card in
/// the hand goes by that card, the rest by what waits in them and when
/// they last moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomRow {
    pub address: Address,
    pub standing: Standing,
    pub name: String,
    /// Where it is: the host's name.
    pub tag: String,
    /// What waits in it, in the system's words (`3 unread · 2h`); empty
    /// when nothing does.
    pub waiting: String,
    /// When it last moved, in unix milliseconds; zero when nothing says.
    pub at: u64,
}

/// Why a root is on Home. Whether the deal holds a card in it is the
/// hand's to say, which Home reads; the rest is the system's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// The user works in it: they made it or wrote in it, or
    /// something in it asks them or mentions them unread.
    Worked,
    /// Only a place a card can be in: a room while the hand holds one in
    /// it, and otherwise reached by name alone.
    WhileCard,
}

/// A place the palette lists, which it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub address: Address,
    pub title: String,
    pub description: String,
    /// When it last moved, for ordering; zero when nothing says.
    pub recency: u64,
}

pub trait System: Sized + 'static {
    /// The address kinds this system opens.
    fn kinds(&self) -> &'static [&'static str];

    /// A surface for an address of one of its kinds; `None` for an
    /// address it has nothing for.
    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface>;

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

    /// The screen that shows `address` as one of its lines, when the
    /// system has one: a message, the thread it is read in.
    fn shown_in(&self, _address: &Address, _cx: &App) -> Option<Address> {
        None
    }

    /// How far the user has seen one of this system's addresses once they
    /// have read it: everything the system has of it, in its own terms.
    fn seen(&self, _address: &Address, _cx: &App) -> Seen {
        Seen::Whole
    }

    /// The rooms this system has, given what the user has said about its
    /// things: every root they work in, and every root a card could be in
    /// as [`Standing::WhileCard`].
    fn rooms(&self, _marks: &Marks, _cx: &App) -> Vec<RoomRow> {
        Vec::new()
    }

    /// Every place of the system's that the palette lists by title, given what
    /// the user has said about its things. Asked with the frame busy, so
    /// the marks are handed in.
    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        Vec::new()
    }

    /// What the user said about these addresses moved, on this device or
    /// from another. Asked with the frame busy: a system reads the marks
    /// afterwards, through [`crate::FrameLink::read`].
    fn marks_moved(&mut self, _touched: &BTreeSet<Address>, _cx: &mut Context<Self>) {}
}

trait SystemHandle {
    fn kinds(&self, cx: &App) -> &'static [&'static str];
    fn open(&self, address: &Address, window: &mut Window, cx: &mut App) -> Option<AnySurface>;
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
    fn shown_in(&self, address: &Address, cx: &App) -> Option<Address>;
    fn seen(&self, address: &Address, cx: &App) -> Seen;
    fn rooms(&self, marks: &Marks, cx: &App) -> Vec<RoomRow>;
    fn find(&self, marks: &Marks, cx: &App) -> Vec<Found>;
    fn marks_moved(&self, touched: &BTreeSet<Address>, cx: &mut App);
}

impl<S: System> SystemHandle for Entity<S> {
    fn kinds(&self, cx: &App) -> &'static [&'static str] {
        self.read(cx).kinds()
    }

    fn open(&self, address: &Address, window: &mut Window, cx: &mut App) -> Option<AnySurface> {
        self.update(cx, |system, cx| system.open(address, window, cx))
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

    fn shown_in(&self, address: &Address, cx: &App) -> Option<Address> {
        self.read(cx).shown_in(address, cx)
    }

    fn seen(&self, address: &Address, cx: &App) -> Seen {
        self.read(cx).seen(address, cx)
    }

    fn rooms(&self, marks: &Marks, cx: &App) -> Vec<RoomRow> {
        self.read(cx).rooms(marks, cx)
    }

    fn find(&self, marks: &Marks, cx: &App) -> Vec<Found> {
        self.read(cx).find(marks, cx)
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

    pub fn open(&self, address: &Address, window: &mut Window, cx: &mut App) -> Option<AnySurface> {
        self.0.open(address, window, cx)
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

    pub fn open_link(&self, url: &Url, cx: &mut App) -> bool {
        self.0.open_link(url, cx)
    }

    pub fn name(&self, address: &Address, marks: &Marks, cx: &App) -> Option<(String, String)> {
        self.0.name(address, marks, cx)
    }

    pub fn shown_in(&self, address: &Address, cx: &App) -> Option<Address> {
        self.0.shown_in(address, cx)
    }

    pub fn seen(&self, address: &Address, cx: &App) -> Seen {
        self.0.seen(address, cx)
    }

    pub fn rooms(&self, marks: &Marks, cx: &App) -> Vec<RoomRow> {
        self.0.rooms(marks, cx)
    }

    pub fn find(&self, marks: &Marks, cx: &App) -> Vec<Found> {
        self.0.find(marks, cx)
    }

    pub fn marks_moved(&self, touched: &BTreeSet<Address>, cx: &mut App) {
        self.0.marks_moved(touched, cx)
    }
}
