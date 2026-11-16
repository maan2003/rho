//! What plugs into the frame.
//!
//! A system owns a few kinds of address and everything about them: the
//! surfaces that open them, the rows a menu offers over them, what of
//! them wants the user, what Home and `find` list, and the streams it
//! keeps to each host. The frame holds systems as a list and asks each
//! in turn; a system never names another.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Context, Entity, Window};
use jiff::Timestamp;
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_dealer::facts::Seen;
use rho_dealer::rank::Trace;
use rho_dealer::{Address, Claim, Marks};

use crate::attention::Verdict;
use crate::frame::Point;
use crate::menu::Row;
use crate::surface::AnySurface;

/// One line of a Home section: something to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeRow {
    pub address: Address,
    pub title: String,
    /// The words beside the title: how long ago, how many.
    pub label: String,
    pub unread: bool,
}

/// A section of Home a system adds under the dealer's cards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeSection {
    pub title: String,
    pub rows: Vec<HomeRow>,
}

/// One channel in Home's list: a rho stream on a host, a Slack channel
/// the user follows. One list, each tagged with where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelRow {
    pub address: Address,
    pub name: String,
    /// Where it is: a host's name, a Slack workspace.
    pub tag: String,
    pub unread: bool,
}

/// Something `find` can open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub address: Address,
    pub title: String,
    pub description: String,
    /// When it last moved, for ordering; zero when nothing says.
    pub recency: u64,
}

/// What takes back the side of a verdict a system did outside the ledger:
/// Slack's own cursor moved back, a unit unmuted there.
pub type Reversal = Box<dyn FnOnce(&mut App)>;

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
    /// `name` is what the user calls the host.
    fn streams(
        &mut self,
        _host: HostId,
        _name: &str,
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

    /// What an address of this system is called and where it is, for a
    /// card no claim named.
    fn name(&self, _address: &Address, _cx: &App) -> Option<(String, String)> {
        None
    }

    /// How far the user has seen one of this system's addresses once they
    /// have read it: everything the system has of it, in its own terms.
    fn seen(&self, _address: &Address, _cx: &App) -> Seen {
        Seen::Whole
    }

    /// What this system lists on Home under the cards, given what the
    /// user has said about its things.
    fn home(&self, _marks: &Marks, _cx: &App) -> Vec<HomeSection> {
        Vec::new()
    }

    fn channels(&self, _marks: &Marks, _cx: &App) -> Vec<ChannelRow> {
        Vec::new()
    }

    /// Everything the system has that `find` lists by title, given what
    /// the user has said about its things; notes and labels live in the
    /// marks alone. Asked with the frame busy, so the marks are handed in.
    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        Vec::new()
    }

    /// What the user said about these addresses moved, on this device or
    /// from another. Asked with the frame busy: a system reads the marks
    /// afterwards, through [`crate::FrameLink::read`].
    fn marks_moved(&mut self, _touched: &BTreeSet<Address>, _cx: &mut Context<Self>) {}

    /// Whether this system holds a mute of the address itself, where the
    /// user can lift it too: the ledger then only settles it, so a mute
    /// lifted there is lifted here.
    fn mutes_itself(&self, _address: &Address) -> bool {
        false
    }

    /// The user said this about one of the system's addresses; what the
    /// system does about it outside the ledger, and how to take that back.
    fn said(
        &mut self,
        _address: &Address,
        _verdict: &Verdict,
        _cx: &mut Context<Self>,
    ) -> Option<Reversal> {
        None
    }
}

trait SystemHandle {
    fn kinds(&self, cx: &App) -> &'static [&'static str];
    fn open(&self, address: &Address, window: &mut Window, cx: &mut App) -> Option<AnySurface>;
    fn streams(
        &self,
        host: HostId,
        name: &str,
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
    fn name(&self, address: &Address, cx: &App) -> Option<(String, String)>;
    fn seen(&self, address: &Address, cx: &App) -> Seen;
    fn home(&self, marks: &Marks, cx: &App) -> Vec<HomeSection>;
    fn channels(&self, marks: &Marks, cx: &App) -> Vec<ChannelRow>;
    fn find(&self, marks: &Marks, cx: &App) -> Vec<Found>;
    fn marks_moved(&self, touched: &BTreeSet<Address>, cx: &mut App);
    fn mutes_itself(&self, address: &Address, cx: &App) -> bool;
    fn said(&self, address: &Address, verdict: &Verdict, cx: &mut App) -> Option<Reversal>;
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
        link: &Link,
        cx: &mut App,
    ) -> Vec<Arc<dyn HostStream>> {
        self.update(cx, |system, cx| system.streams(host, name, link, cx))
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

    fn name(&self, address: &Address, cx: &App) -> Option<(String, String)> {
        self.read(cx).name(address, cx)
    }

    fn seen(&self, address: &Address, cx: &App) -> Seen {
        self.read(cx).seen(address, cx)
    }

    fn home(&self, marks: &Marks, cx: &App) -> Vec<HomeSection> {
        self.read(cx).home(marks, cx)
    }

    fn channels(&self, marks: &Marks, cx: &App) -> Vec<ChannelRow> {
        self.read(cx).channels(marks, cx)
    }

    fn find(&self, marks: &Marks, cx: &App) -> Vec<Found> {
        self.read(cx).find(marks, cx)
    }

    fn marks_moved(&self, touched: &BTreeSet<Address>, cx: &mut App) {
        self.update(cx, |system, cx| system.marks_moved(touched, cx))
    }

    fn mutes_itself(&self, address: &Address, cx: &App) -> bool {
        self.read(cx).mutes_itself(address)
    }

    fn said(&self, address: &Address, verdict: &Verdict, cx: &mut App) -> Option<Reversal> {
        self.update(cx, |system, cx| system.said(address, verdict, cx))
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
        link: &Link,
        cx: &mut App,
    ) -> Vec<Arc<dyn HostStream>> {
        self.0.streams(host, name, link, cx)
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

    pub fn name(&self, address: &Address, cx: &App) -> Option<(String, String)> {
        self.0.name(address, cx)
    }

    pub fn seen(&self, address: &Address, cx: &App) -> Seen {
        self.0.seen(address, cx)
    }

    pub fn home(&self, marks: &Marks, cx: &App) -> Vec<HomeSection> {
        self.0.home(marks, cx)
    }

    pub fn channels(&self, marks: &Marks, cx: &App) -> Vec<ChannelRow> {
        self.0.channels(marks, cx)
    }

    pub fn find(&self, marks: &Marks, cx: &App) -> Vec<Found> {
        self.0.find(marks, cx)
    }

    pub fn marks_moved(&self, touched: &BTreeSet<Address>, cx: &mut App) {
        self.0.marks_moved(touched, cx)
    }

    pub fn mutes_itself(&self, address: &Address, cx: &App) -> bool {
        self.0.mutes_itself(address, cx)
    }

    pub fn said(&self, address: &Address, verdict: &Verdict, cx: &mut App) -> Option<Reversal> {
        self.0.said(address, verdict, cx)
    }
}
