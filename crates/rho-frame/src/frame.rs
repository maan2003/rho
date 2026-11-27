//! The frame entity: the window's root view.

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::Editor;
use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Subscription, Task, Window, div, px,
};
use jiff::Timestamp;
use rho_agent_hosts::hosts::Hosts;
use rho_agent_hosts::{HostEvent, HostId, HostSpec};
use rho_dealer::facts::Seen;
use rho_dealer::{Address, Card};
use rho_ledger::Pairing;
use rho_window::layout::{Notch, PROMPT_GAP_PX, PROMPT_MAX_CH, READING_INSET_PX, TOP_ROW_PX};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::attention::Attention;
use crate::command::{Command, Shortcut};
use crate::deal::Signals;
use crate::desk::DeskView;
use crate::hosts::Reach;
use crate::minibuffer::{Answer, CancelHandler, CandidateSource, Echo, Minibuffer, SubmitHandler};
use crate::phone::{Layout, Phone};
use crate::prefix::{Prefix, Press, Row};
use crate::recent::Recent;
use crate::search::Search;
use crate::surface::{AnySurface, CardHere, Mode, Subject, Surface as _};
use crate::system::{AnySystem, Found, Opening, Project};
use crate::waiting::Waiting;
use crate::{commands, keys, place, places};

/// The kind of the frame's own surface: Desk.
pub const FRAME_KIND: &str = "frame";

/// Where the reader is: the surface in view, what the point in it is
/// on, which is the surface's own address when it is a page about one
/// thing and a row's when it lists things, and the card a verdict is
/// about, which only a surface that has one names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub surface: Address,
    pub subject: Subject,
    pub card: Option<Address>,
}

impl Point {
    /// The thing under point for commands that operate on an address.
    pub fn subject(&self) -> Option<&Address> {
        subject_address(&self.surface, &self.subject)
    }
}

fn subject_address<'a>(surface: &'a Address, subject: &'a Subject) -> Option<&'a Address> {
    match subject {
        Subject::Own | Subject::Draft { .. } => Some(surface),
        Subject::Address(address) => Some(address),
        Subject::None => None,
    }
}

/// The card as it stood when the reader stepped down into the work.
#[derive(Clone)]
pub struct CardInspection {
    address: Address,
    seen: Seen,
}

/// How a recent place was reached. A dealt screen and one opened by name
/// are the same: the screen shows its card live, so what the reader sees
/// of it is the hand's. Only a step down into the work keeps the card as
/// it stood, since the card's own screen is out of sight.
#[derive(Default)]
pub(crate) enum ScreenEntry {
    #[default]
    Opened,
    Inspection(CardInspection),
}

/// How a screen comes into view. Opened by name, it starts where an open
/// starts (a thread at its draft); come back to through the recent places,
/// it is as the reader left it (`window.Screen`).
enum Open<'a> {
    Explicit,
    Recent,
    Deal(&'a Card),
    Inspect(CardInspection),
    /// From a remembered line in `Places`: on that line.
    Search(&'a crate::SearchPoint),
}

/// How many command names `Commands` remembers as recently run.
const RECENT_COMMANDS: usize = 20;

/// How long an echo stays on the line.
const ECHO_FOR: Duration = Duration::from_secs(4);

/// How many echoes `show messages` keeps.
const ECHOES_KEPT: usize = 100;

/// One echo as `show messages` lists it: what it said, and when.
pub(crate) struct Said {
    pub(crate) text: String,
    pub(crate) at: Timestamp,
}

pub struct Frame {
    pub(crate) db: rho_db::RhoDb,
    pub(crate) hosts: Hosts,
    pub(crate) systems: Vec<AnySystem>,
    pub(crate) attention: Attention,
    /// The places the reader has been, most recently visited first; the
    /// first is in view. Desk is always among them and never closes.
    recent: Recent<Address, ScreenEntry>,
    /// The names of the commands run last, most recent first.
    recent_commands: Vec<String>,
    surfaces: HashMap<Address, AnySurface>,
    pub(crate) desk: Entity<DeskView>,
    pub(crate) minibuffer: Option<Minibuffer>,
    /// `space` was pressed and the next key is the prefix's.
    prefix: Option<Prefix>,
    /// Where the keyboard was before a prompt took it.
    overlay_return: Option<FocusHandle>,
    echo: Option<Echo>,
    /// The echoes said lately, oldest first, for `show messages`; this
    /// window's own, gone when it closes.
    echoes: VecDeque<Said>,
    /// Whether the notch is masked in black. On by default; `toggle notch
    /// mask` turns it off for this window.
    pub(crate) notch_mask: bool,
    /// The text style for a surface that is no buffer.
    chrome_editor: Entity<Editor>,
    mode_indicator: Entity<vim::ModeIndicator>,
    pub(crate) signals: Signals,
    /// What the deal, the lamp and the chime answer to, kept on this device.
    pub(crate) dealer_focus: crate::focus::DealerFocus,
    pub(crate) chime: Rc<dyn Fn()>,
    /// The last `/` or `?`, which `n` and `N` repeat in any surface.
    pub(crate) search: Option<Search>,
    /// A flick or a snap in progress, while the window is a phone's.
    pub(crate) phone: Phone,
    /// Writes down where the user is after each move, for the next start.
    pub(crate) place: place::Writer,
    last_input: Option<Timestamp>,
    focus_lost: Option<Instant>,
    /// The question whether to pair this device waits to be asked
    /// ([`Frame::ask_pairing`]).
    pub(crate) pairing_due: bool,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl Frame {
    /// The frame over `systems`, attached to `hosts`. `chime` is what
    /// sounds when a card crosses the chime line.
    pub fn new(
        db: rho_db::RhoDb,
        systems: Vec<AnySystem>,
        hosts: Vec<HostSpec>,
        chime: Rc<dyn Fn()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        keys::bind(cx);
        let (attention, ledger_events) = Attention::open(db.clone());
        let (host_sink, host_events) = futures::channel::mpsc::unbounded::<HostEvent>();
        let frame = cx.weak_entity();
        let desk = cx.new(|cx| DeskView::new(frame, window, cx));
        let chrome_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor
        });
        let mode_indicator = cx.new(|cx| vim::ModeIndicator::new(window, cx));
        let desk_address = Self::desk_address();
        let mut surfaces = HashMap::new();
        surfaces.insert(desk_address.clone(), AnySurface::new(desk.clone()));
        // The key after `space` is the prefix's before gpui resolves
        // bindings: vim binds `g` as a prefix of its own, and an ordinary
        // key handler would not see it until another key arrived.
        let window_id = window.window_handle().window_id();
        let prefix_keys = cx.listener(move |this, event: &gpui::KeystrokeEvent, window, cx| {
            if window.window_handle().window_id() != window_id {
                return;
            }
            this.note_input();
            if this.prefix.is_some() {
                this.prefix_keystroke(&event.keystroke, window, cx);
            }
        });
        let prefix_keys = cx.intercept_keystrokes(prefix_keys);
        let saved_place = place::Saved::load(&db);
        let last_input = place::last_input(&db);
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                if this
                    .focus_lost
                    .take()
                    .is_some_and(|at| at.elapsed() >= place::COLD_RETURN)
                {
                    this.return_desk(window, cx);
                    return;
                }
            } else {
                this.focus_lost = Some(Instant::now());
            }
            this.report_active_surface(window, cx);
        });
        let mut frame = Self {
            db: db.clone(),
            hosts: Hosts::new(Arc::new(host_sink)),
            systems,
            attention,
            recent: Recent::new(desk_address, ScreenEntry::Opened),
            recent_commands: place::recent_commands(&db),
            surfaces,
            desk,
            minibuffer: None,
            prefix: None,
            overlay_return: None,
            echo: None,
            echoes: VecDeque::new(),
            notch_mask: true,
            chrome_editor,
            mode_indicator,
            signals: Signals::default(),
            dealer_focus: place::dealer_focus(&db),
            chime,
            search: None,
            phone: Phone::default(),
            place: place::Writer::spawn(db.clone()),
            last_input,
            focus_lost: None,
            pairing_due: false,
            _subscriptions: vec![prefix_keys, activation],
            _tasks: Vec::new(),
        };
        let ledger = cx.spawn_in(window, async move |this, cx| {
            let mut events = ledger_events;
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                let updated = this.update(cx, |this, cx| {
                    for event in batch {
                        this.ledger_event(event, cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let hosts_task = cx.spawn_in(window, async move |this, cx| {
            let mut events = host_events;
            while let Some(HostEvent { host, event }) = events.next().await {
                if this
                    .update(cx, |this, cx| this.host_event(host, event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        frame._tasks.push(ledger);
        frame._tasks.push(hosts_task);
        for spec in hosts {
            frame.attach_host(spec, cx);
        }
        if let Some(saved) = saved_place {
            frame.return_to(saved, window, cx);
            // In particular a warm restart after a cold one must restore
            // the desk, not the screen now behind it.
            frame.moved();
        }
        frame.systems_changed(cx);
        frame.focus_surface(window, cx);
        frame
    }

    /// Opens the window where the user was when it last ran on this
    /// device, with the places visited before it in the order they were.
    /// Those behind are made when the user reaches them; the one in view
    /// is made now, and one that no longer opens leaves the order.
    fn return_to(&mut self, saved: place::Saved, window: &mut Window, cx: &mut Context<Self>) {
        self.recent = place::return_recent(
            &saved,
            &Self::desk_address(),
            self.last_input,
            Timestamp::now(),
        );
        self.settle(window, cx);
    }

    /// What the system that owns `address`'s kind has for it.
    fn make_surface(
        &self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Opening {
        match self
            .systems
            .clone()
            .into_iter()
            .find(|system| system.opens(address, cx))
        {
            Some(system) => system.open(address, window, cx),
            None => Opening::Gone,
        }
    }

    /// Makes the surface for `address` and holds it: the real one, or a
    /// waiting screen while the hosts that would hold it have not said.
    /// `false` when nothing holds it.
    fn hold(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let surface = match self.make_surface(address, window, cx) {
            Opening::Surface(surface) => surface,
            Opening::Waiting(hosts) => AnySurface::new(self.waiting_surface(&hosts, cx)),
            Opening::Gone => return false,
        };
        self.surfaces.insert(address.clone(), surface);
        true
    }

    fn waiting_surface(&self, hosts: &[HostId], cx: &mut Context<Self>) -> Entity<Waiting> {
        let names = hosts
            .iter()
            .map(|host| self.hosts.host_label(*host))
            .collect::<Vec<_>>()
            .join(", ");
        cx.new(|cx| Waiting::new(names, cx))
    }

    /// Asks again for every address that waits on its hosts: one a host
    /// now holds gets its real surface in the waiting one's place, and
    /// one no host holds leaves the recent places, as a closed screen
    /// does. A prompt or a menu over the screen keeps the keyboard.
    pub(crate) fn reopen_waiting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let waiting = self
            .surfaces
            .iter()
            .filter(|(_, surface)| surface.view().downcast::<Waiting>().is_ok())
            .map(|(address, _)| address.clone())
            .collect::<Vec<_>>();
        if waiting.is_empty() {
            return;
        }
        let shown = self.current_surface().entity_id();
        for address in waiting {
            match self.make_surface(&address, window, cx) {
                Opening::Waiting(_) => {}
                Opening::Surface(surface) => {
                    self.surfaces.insert(address, surface);
                }
                Opening::Gone => {
                    self.surfaces.remove(&address);
                    self.recent.remove(&address);
                }
            }
        }
        self.settle(window, cx);
        if self.current_surface().entity_id() == shown {
            return;
        }
        let handle = self.current_surface().focus_handle(cx);
        match &mut self.overlay_return {
            Some(under) => *under = handle,
            None => self.focus_surface(window, cx),
        }
        self.moved();
        cx.notify();
    }

    /// Makes the screen in view when the recent places hold it by address
    /// alone, as one given back at start is. One that no longer opens
    /// leaves them, and the place visited before it is in view; Desk
    /// always opens.
    fn settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let address = self.current_address().clone();
            if self.surfaces.contains_key(&address) || self.hold(&address, window, cx) {
                return;
            }
            if !self.recent.remove(&address) {
                self.recent.show(Self::desk_address(), ScreenEntry::Opened);
            }
        }
    }

    /// Writes down where the user is now, for the next start.
    fn moved(&self) {
        self.place
            .write(place::Write::Place(place::Saved::of(&self.recent)));
    }

    /// A key or deliberate pointer/touch input is activity by the user;
    /// focuses and host/network activity are not.
    pub(crate) fn note_input(&mut self) {
        let at = Timestamp::now();
        self.last_input = Some(at);
        self.place.write(place::Write::Input(at));
    }

    pub fn desk_address() -> Address {
        Address::new(FRAME_KIND, "desk")
    }

    // ---- surfaces ----

    pub fn current_address(&self) -> &Address {
        self.recent.current_key()
    }

    pub fn current_surface(&self) -> &AnySurface {
        self.surfaces
            .get(self.recent.current_key())
            .expect("the current surface is held")
    }

    /// The surface in view's editor, or the chrome's for a surface that is
    /// no buffer.
    pub fn active_editor(&self, cx: &App) -> Entity<Editor> {
        self.current_surface()
            .editor(cx)
            .unwrap_or_else(|| self.chrome_editor.clone())
    }

    /// Where the reader is.
    pub fn point(&self, cx: &App) -> Point {
        let surface = self.current_address().clone();
        let current = self.current_surface();
        let subject = current.subject(self.card_seen(&surface, cx), cx);
        let card = match current.card(cx) {
            CardHere::Itself | CardHere::Reminder => Some(surface.clone()),
            CardHere::Inspected => match self.recent.current() {
                ScreenEntry::Inspection(inspection) if self.waits(&inspection.address) => {
                    Some(inspection.address.clone())
                }
                _ => None,
            },
            CardHere::None => None,
        };
        Point {
            surface,
            subject,
            card,
        }
    }

    /// The card a verdict lands on: the one that holds what the point is
    /// on.
    pub fn card_target(&self, cx: &App) -> Option<Address> {
        self.point(cx).card
    }

    /// The card the surface in view stands for, which is nothing on Desk:
    /// Desk is a list, so a pull from it opens the top card rather than
    /// passing over the row.
    pub fn card_in_view(&self, cx: &App) -> Option<Address> {
        match self.desk_in_view() {
            true => None,
            false => self.card_target(cx),
        }
    }

    /// The card the screen in view is, when it is one: the status line
    /// gives it the card's words, and a verdict on it puts the screen away.
    pub(crate) fn own_card(&self, cx: &App) -> Option<Address> {
        self.card_in_view(cx)
            .filter(|address| address == self.current_address())
    }

    pub(crate) fn waits(&self, address: &Address) -> bool {
        self.hand()
            .cards
            .iter()
            .any(|card| &card.address == address)
    }

    /// What the reader has seen of the card at `address`, while it waits.
    /// Looking into its work, the card as it stood when they stepped down,
    /// since mail that came after is out of sight. On its own screen, the
    /// card as the hand holds it, as far as the screen has had it in view:
    /// a thread shows its mail live, however it was reached, so a reply or
    /// a verdict settles what is on screen and nothing that came while the
    /// reader was elsewhere. Anywhere else, the card as the
    /// hand holds it.
    pub fn card_seen(&self, address: &Address, cx: &App) -> Option<Seen> {
        if let ScreenEntry::Inspection(inspection) = self.recent.current()
            && &inspection.address == address
        {
            return Some(inspection.seen);
        }
        let held = self
            .hand()
            .cards
            .iter()
            .find(|card| &card.address == address)
            .map(|card| card.seen)?;
        Some(match address == self.current_address() {
            true => self.current_surface().seen(held, cx),
            false => held,
        })
    }

    /// Capture the card under the reader, keeping its observation while
    /// moving between the transcript, desktop, shell and terminal.
    pub fn card_to_inspect(&self, cx: &App) -> Option<CardInspection> {
        if let ScreenEntry::Inspection(inspection) = self.recent.current() {
            return Some(inspection.clone());
        }
        let address = self.card_target(cx)?;
        let seen = self.card_seen(&address, cx)?;
        Some(CardInspection { address, seen })
    }

    pub fn desk_in_view(&self) -> bool {
        self.current_surface().entity_id() == self.desk.entity_id()
    }

    /// Opens `address` by name: the system that owns its kind makes the
    /// surface, or the one already made starts again where an open starts.
    pub fn open(&mut self, address: Address, window: &mut Window, cx: &mut Context<Self>) {
        self.open_by(address, Open::Explicit, window, cx);
    }

    /// Comes back to `address` as the reader left it, scroll and point
    /// included, as the recent places do.
    pub fn revisit(&mut self, address: Address, window: &mut Window, cx: &mut Context<Self>) {
        self.open_by(address, Open::Recent, window, cx);
    }

    /// A place not yet made whose system resolves it later (`visit`) is
    /// the system's to show. A note about a line another screen shows, a
    /// private reply under its message, is read there, at that line.
    /// Everything else shows now.
    fn open_by(
        &mut self,
        address: Address,
        open: Open<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(about) = self.attention.marks.get(&address).about.clone()
            && let Some(screen) = self.shown_in(&about, cx)
        {
            let at = crate::SearchPoint::Message {
                message: about,
                line: 0,
            };
            self.open_at(screen, &at, window, cx);
            return;
        }
        if !self.surfaces.contains_key(&address)
            && self
                .systems
                .iter()
                .find(|system| system.opens(&address, cx))
                .cloned()
                .is_some_and(|system| system.visit(&address, cx))
        {
            return;
        }
        self.open_with(address, open, window, cx);
    }

    pub fn open_inspection(
        &mut self,
        address: Address,
        inspection: Option<CardInspection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match inspection {
            Some(inspection) => self.open_with(address, Open::Inspect(inspection), window, cx),
            None => self.open(address, window, cx),
        }
    }

    pub(crate) fn show_dealt_card(
        &mut self,
        card: &Card,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_with(card.address.clone(), Open::Deal(card), window, cx);
    }

    fn open_with(
        &mut self,
        address: Address,
        open: Open<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fresh = !self.surfaces.contains_key(&address);
        if fresh && !self.hold(&address, window, cx) {
            // A recent place that no longer opens is no longer a place.
            if *self.current_address() != address {
                self.recent.remove(&address);
                self.moved();
            }
            self.echo(
                &format!("nothing opens {address}"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        let surface = self.surfaces[&address].clone();
        // How it was reached; a place come back to was reached as before,
        // so a step down into the work still inspects its card.
        let (entry, method) = match open {
            Open::Deal(card) => {
                surface.show_card(card, window, cx);
                (
                    Some(ScreenEntry::Opened),
                    rho_journal::SurfaceShowMethod::Deal,
                )
            }
            Open::Inspect(inspection) => {
                surface.opened(window, cx);
                (
                    Some(ScreenEntry::Inspection(inspection)),
                    rho_journal::SurfaceShowMethod::Open,
                )
            }
            Open::Explicit => {
                surface.opened(window, cx);
                (
                    Some(ScreenEntry::Opened),
                    rho_journal::SurfaceShowMethod::Open,
                )
            }
            Open::Recent => {
                // Nothing to come back to on a screen made just now.
                match fresh {
                    true => surface.opened(window, cx),
                    false => surface.revisited(window, cx),
                }
                (None, rho_journal::SurfaceShowMethod::Mru)
            }
            Open::Search(point) => {
                surface.show_search_match(point, window, cx);
                (
                    Some(ScreenEntry::Opened),
                    rho_journal::SurfaceShowMethod::Open,
                )
            }
        };
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: journal_surface(&address),
            method,
        });
        match entry {
            Some(entry) => self.recent.show(address, entry),
            None => {
                if !self.recent.bring(&address) {
                    self.recent.show(address, ScreenEntry::Opened);
                }
            }
        }
        if self.desk_in_view() {
            self.refresh_desk(&self.hand().clone(), false, cx);
        }
        self.moved();
        self.close_overlays(window, cx);
        self.focus_surface(window, cx);
        self.ask_pairing_when_free(window, cx);
        cx.notify();
    }

    /// `new` is `old` under another name, and takes its place among the
    /// recent places rather than adding one: a usage chart switching its
    /// window, or a queued idea becoming its thread. When `old` is in view,
    /// `new` is shown in its place; Desk is never replaced, and from it
    /// this opens `new` as usual.
    pub fn replace(
        &mut self,
        old: &Address,
        new: Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *old == new || *old == Self::desk_address() {
            if self.current_address() == old {
                self.open(new, window, cx);
            }
            return;
        }
        if self.current_address() != old {
            if self.recent.replace(old, new, ScreenEntry::Opened) {
                self.surfaces.remove(old);
                self.moved();
            }
            return;
        }
        if !self.surfaces.contains_key(&new) && !self.hold(&new, window, cx) {
            self.echo(&format!("nothing opens {new}"), StyleClass::SystemInfo, cx);
            return;
        }
        self.surfaces[&new].opened(window, cx);
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: journal_surface(&new),
            method: rho_journal::SurfaceShowMethod::Open,
        });
        self.recent.replace(old, new, ScreenEntry::Opened);
        self.surfaces.remove(old);
        self.moved();
        self.close_overlays(window, cx);
        self.focus_surface(window, cx);
        self.ask_pairing_when_free(window, cx);
        cx.notify();
    }

    /// `address` was visited just now, though the reader is still where
    /// they were, such as an idea sent from its own screen: it is the most
    /// recent place behind the one in view, and ages from there.
    pub fn remember(&mut self, address: Address) {
        self.recent.remember(address, ScreenEntry::Opened);
        self.moved();
    }

    /// Opens a web link the user chose: the system that shows the web
    /// takes it.
    pub fn open_link(&mut self, url: url::Url, cx: &mut Context<Self>) {
        let taken = self
            .systems
            .clone()
            .into_iter()
            .any(|system| system.open_link(&url, cx));
        if !taken {
            self.echo(&format!("nothing opens {url}"), StyleClass::SystemInfo, cx);
        }
    }

    pub fn open_desk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open(Self::desk_address(), window, cx);
    }

    /// `tab`: Desk, or from Desk the place visited before it.
    pub(crate) fn toggle_desk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.desk_in_view() {
            true => self.previous_place(window, cx),
            false => self.open_desk(window, cx),
        }
    }

    /// The place visited before the one in view, as it was left.
    pub fn previous_place(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.recent.previous().next().cloned();
        match previous {
            Some(previous) => self.revisit(previous, window, cx),
            None => self.echo("no other place yet", StyleClass::SystemInfo, cx),
        }
    }

    /// Closes `address`: its surface goes, and it leaves the recent
    /// places. When it was in view, the place visited before it is now.
    /// Desk never closes; closing it steps to the place before it.
    pub fn close(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_address() == address;
        if *address == Self::desk_address() {
            if current {
                self.previous_place(window, cx);
            }
            return;
        }
        rho_journal::record(rho_journal::Event::SurfaceClosed {
            surface: journal_surface(address),
        });
        self.recent.remove(address);
        self.surfaces.remove(address);
        if current {
            self.settle(window, cx);
            self.close_overlays(window, cx);
            self.focus_surface(window, cx);
            self.ask_pairing_when_free(window, cx);
        }
        self.moved();
        cx.notify();
    }

    /// Closes every place, made or only remembered, whose address `which`
    /// picks: what a detached host leaves behind.
    pub fn close_all(
        &mut self,
        which: impl Fn(&Address) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut picked: Vec<Address> = self.surfaces.keys().filter(|a| which(a)).cloned().collect();
        picked.extend(
            self.recent
                .keys()
                .filter(|address| which(address) && !self.surfaces.contains_key(*address))
                .cloned(),
        );
        for address in picked {
            self.close(&address, window, cx);
        }
    }

    pub fn close_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let address = self.current_address().clone();
        self.close(&address, window, cx);
    }

    fn focus_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.desk_in_view() {
            self.refresh_desk(&self.hand().clone(), false, cx);
        }
        let handle = self.current_surface().focus_handle(cx);
        window.focus(&handle, cx);
        self.report_active_surface(window, cx);
        self.systems_changed(cx);
    }

    fn return_desk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if *self.current_address() != Self::desk_address() {
            self.open_desk(window, cx);
        } else {
            self.report_active_surface(window, cx);
        }
    }

    /// The compositor observed device-wide idle even without a focus change.
    pub fn device_resumed(&mut self, after: Duration, window: &mut Window, cx: &mut Context<Self>) {
        if after >= place::COLD_RETURN && window.is_window_active() {
            self.return_desk(window, cx);
        }
    }

    fn report_active_surface(&self, window: &Window, cx: &mut Context<Self>) {
        let active = window.is_window_active().then(|| self.current_address());
        for system in &self.systems {
            system.active_surface(active, cx);
        }
    }

    /// The surface for `address` while it is open, for a system that wants
    /// to tell its own view something.
    pub fn surface(&self, address: &Address) -> Option<&AnySurface> {
        self.surfaces.get(address)
    }

    /// Whether a prompt has the keys.
    pub fn is_asking(&self) -> bool {
        self.minibuffer.is_some()
    }

    // ---- echo ----

    /// One line on the status line for a few seconds, and in the process
    /// log for good.
    pub fn echo(&mut self, text: &str, class: StyleClass, cx: &mut Context<Self>) {
        tracing::info!(%text, "echo");
        let dismiss = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ECHO_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.echo = None;
                cx.notify();
            });
        });
        let echo = Echo::new(text, class, dismiss);
        if self.echoes.len() == ECHOES_KEPT {
            self.echoes.pop_front();
        }
        self.echoes.push_back(Said {
            text: echo.text().to_owned(),
            at: Timestamp::now(),
        });
        self.echo = Some(echo);
        cx.notify();
    }

    /// The echoes said lately, oldest first.
    pub(crate) fn echoes(&self) -> &VecDeque<Said> {
        &self.echoes
    }

    /// Drops the echo when it still matches `predicate`, such as a stale
    /// away notice for a host that just came back online.
    pub(crate) fn clear_echo_if(&mut self, predicate: impl FnOnce(&str) -> bool) {
        if self
            .echo
            .as_ref()
            .is_some_and(|echo| predicate(echo.text()))
        {
            self.echo = None;
        }
    }

    /// `show messages`: the echoes said lately, newest first, each with
    /// its time; typing narrows them.
    pub(crate) fn show_messages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.echoes.is_empty() {
            return self.echo(
                "show messages: nothing said yet",
                StyleClass::SystemInfo,
                cx,
            );
        }
        let complete: CandidateSource = Rc::new(|frame: &Frame, typed: &str, _: &App| {
            let typed = typed.trim().to_lowercase();
            let zone = jiff::tz::TimeZone::system();
            frame
                .echoes
                .iter()
                .rev()
                .filter(|said| said.text.to_lowercase().contains(&typed))
                .map(|said| crate::minibuffer::Candidate {
                    value: said.text.clone(),
                    description: said
                        .at
                        .to_zoned(zone.clone())
                        .strftime("%H:%M:%S")
                        .to_string(),
                })
                .collect()
        });
        self.open_prompt("messages:", complete, Rc::new(|_, _, _, _| {}), window, cx);
    }

    /// `toggle notch mask`: the black over the notch, or the screen under
    /// it.
    pub(crate) fn toggle_notch_mask(&mut self, cx: &mut Context<Self>) {
        self.notch_mask = !self.notch_mask;
        let state = if self.notch_mask { "on" } else { "off" };
        self.echo(&format!("notch mask {state}"), StyleClass::SystemInfo, cx);
    }

    // ---- overlays: the `space` prefix and the minibuffer ----

    fn take_keyboard(&mut self, window: &mut Window, cx: &App) {
        if self.overlay_return.is_none() {
            self.overlay_return = window.focused(cx);
        }
    }

    fn give_keyboard_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.overlay_return.take() {
            Some(handle) => window.focus(&handle, cx),
            None => self.focus_surface(window, cx),
        }
        cx.notify();
    }

    fn close_overlays(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prefix = None;
        self.close_minibuffer(window, cx);
        self.overlay_return = None;
    }

    /// `space`: arms the silent prefix over the screen in view. Nothing is
    /// drawn; the next key runs what it is bound to, and `escape` or a key
    /// bound to nothing disarms it ([`crate::prefix`]).
    pub(crate) fn leader(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.prefix = Some(crate::own::prefix(self, cx));
    }

    /// The keys the armed `space` prefix binds, each with what it is
    /// called; nothing while it is not armed.
    pub fn menu_rows(&self) -> Vec<(String, String)> {
        self.prefix.as_ref().map_or_else(Vec::new, Prefix::keys)
    }

    fn prefix_keystroke(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prefix) = &self.prefix else {
            return;
        };
        let press = prefix.press(keystroke);
        if matches!(press, Press::Pending) {
            return;
        }
        self.prefix = None;
        cx.stop_propagation();
        match press {
            Press::Pending | Press::Cancelled => {}
            Press::Refused(reason) => self.echo(&reason, StyleClass::SystemInfo, cx),
            Press::Run(run) => {
                rho_journal::record(rho_journal::Event::MenuRan {
                    action: format!("space {}", keystroke.key),
                    count: None,
                });
                run(self, window, cx);
                cx.notify();
            }
        }
    }

    /// `space f`: `Places`, every place by name and the words remembered
    /// in messages and notes. The catalogue the systems list is joined to
    /// the places this window visited and the cards that wait; Desk is
    /// `tab`'s and is not listed.
    pub(crate) fn places(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_places(None, window, cx);
    }

    /// `Places`, or with `scope` only the places in that project.
    fn open_places(&mut self, scope: Option<Project>, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.place_rows(scope.as_ref(), cx);
        let mut listing = places::Listing::new(rows);
        listing.desk = self.desk_in_view();
        let prompt = match &scope {
            Some(project) => format!("places · {}:", project.name),
            None => "places:".to_owned(),
        };
        let scope = scope.map(|project| project.address);
        self.open_minibuffer(prompt, Answer::places(listing, scope), None, window, cx);
    }

    /// Every place `Places` lists, or with `scope` only those in that
    /// project: what the systems list, what the user visited, and what
    /// waits, each with its card and its states.
    fn place_rows(&self, scope: Option<&Project>, cx: &App) -> Vec<places::Place> {
        let desk = Self::desk_address();
        let in_scope = |frame: &Self, address: &Address| {
            *address != desk
                && scope.as_ref().is_none_or(|scope| {
                    *address != scope.address
                        && frame
                            .project_of(address, cx)
                            .is_some_and(|project| project.address == scope.address)
                })
        };
        let mut found = Vec::new();
        for system in &self.systems {
            found.extend(system.find(&self.attention.marks, cx));
        }
        found.retain(|place| in_scope(self, &place.address));
        let mut known: std::collections::HashSet<_> =
            found.iter().map(|place| place.address.clone()).collect();
        // Desk is `tab`'s, and found by name like any place.
        if scope.is_none() {
            found.push(Found {
                address: desk.clone(),
                title: self.desk.read(cx).title(cx),
                description: String::new(),
                thread: None,
                recency: 0,
                also: Vec::new(),
            });
        }
        // A visited place stays reachable by name, whether or not its
        // system lists it: a transcript, a terminal or a sent idea. Its
        // system's name for it comes first: a screen's own title can carry
        // what only the screen needs, such as a transcript's folder.
        for address in self.recent.keys() {
            if !in_scope(self, address) || !known.insert(address.clone()) {
                continue;
            }
            if let Some((title, description)) = self.name_of(address, cx).or_else(|| {
                self.surfaces
                    .get(address)
                    .map(|surface| (surface.title(cx), String::new()))
            }) {
                found.push(Found {
                    address: address.clone(),
                    title,
                    description,
                    thread: None,
                    recency: 0,
                    also: Vec::new(),
                });
            }
        }
        for card in &self.hand().cards {
            if in_scope(self, &card.address) && known.insert(card.address.clone()) {
                found.push(Found {
                    address: card.address.clone(),
                    title: card.title.clone(),
                    description: card.context.clone(),
                    thread: None,
                    recency: 0,
                    also: Vec::new(),
                });
            }
        }
        let cards: HashMap<_, _> = self
            .hand()
            .cards
            .iter()
            .enumerate()
            .map(|(rank, card)| (&card.address, (rank, card)))
            .collect();
        // With nothing typed, waiting places keep the deal's own order.
        found.sort_by_key(|place| {
            cards
                .get(&place.address)
                .map_or(usize::MAX, |(rank, _)| *rank)
        });
        let visits: HashMap<_, _> = self
            .recent
            .keys()
            .enumerate()
            .map(|(rank, address)| (address, rank))
            .collect();
        let now = Timestamp::now();
        found
            .into_iter()
            .map(|found| {
                let card = cards.get(&found.address).map(|(_, card)| (*card).clone());
                let away = self
                    .away_host(&found.address, cx)
                    .map(|host| format!("{} away", host.name));
                let states = self.place_states(&found, card.as_ref(), away.is_some(), now);
                places::Place {
                    key: (found.address == desk).then_some("tab"),
                    visit: visits.get(&found.address).copied(),
                    visited_ms: self.recent.visited_ms(&found.address),
                    current: self.current_address() == &found.address,
                    card,
                    found,
                    states,
                    away,
                }
            })
            .collect()
    }

    /// The states `Places` finds a place under (`working:`, `stopped:`,
    /// `muted:`, `snoozed:`): what its host last said its agents do, unless
    /// that host is `away`, what the deal holds of it, and what the user
    /// said about it.
    fn place_states(
        &self,
        found: &Found,
        card: Option<&Card>,
        away: bool,
        now: Timestamp,
    ) -> Vec<places::PlaceState> {
        use places::PlaceState;
        let work = found.thread.as_ref().and_then(|thread| thread.work);
        let facts = self.attention.marks.get(&found.address).facts();
        let stopped = work == Some(rho_agent_types::ThreadWork::StoppedByYou)
            || card.is_some_and(|card| matches!(card.kind, rho_dealer::rank::CardKind::Stop(_)));
        [
            (
                PlaceState::Working,
                work == Some(rho_agent_types::ThreadWork::Working) && !away,
            ),
            (PlaceState::Stopped, stopped),
            (PlaceState::Muted, facts.muted()),
            (
                PlaceState::Snoozed,
                facts.snooze().is_some_and(|snooze| now < snooze.until),
            ),
        ]
        .into_iter()
        .filter_map(|(state, holds)| holds.then_some(state))
        .collect()
    }

    /// What choosing a place in `Places` does. A project has no screen of
    /// its own: choosing one narrows `Places` to its threads. A place
    /// chosen from the recent ones with nothing typed is come back to as
    /// it was left; one chosen by name opens as an open does, and a
    /// waiting one shows its card.
    pub(crate) fn choose_place(
        &mut self,
        place: &places::Place,
        blank: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let address = &place.found.address;
        if let Some(project) = self
            .project_of(address, cx)
            .filter(|project| &project.address == address)
        {
            return self.open_places(Some(project), window, cx);
        }
        match (&place.card, blank, place.visit) {
            (Some(card), false, _) | (Some(card), true, None) => {
                self.open_card(card.clone(), window, cx)
            }
            (_, true, Some(_)) => self.revisit(address.clone(), window, cx),
            _ => self.open(address.clone(), window, cx),
        }
    }

    pub(crate) fn text_search(&self, query: String, limit: usize, cx: &App) -> crate::TextSearch {
        let mut search = crate::TextSearch::new(query, limit);
        if limit != 0 {
            for system in &self.systems {
                system.text_sources(&mut search, self.current_address(), cx);
            }
        }
        search
    }

    /// The rows for words found in messages and notes; within a project's
    /// scope, only those in its threads.
    pub(crate) fn describe_text(
        &self,
        hits: Vec<crate::TextHit>,
        scope: Option<&Address>,
        cx: &App,
    ) -> Vec<(crate::FoundText, Vec<std::ops::Range<usize>>)> {
        let mut presenters: Vec<_> = self
            .systems
            .iter()
            .filter_map(|system| system.text_presenter(cx))
            .collect();
        hits.into_iter()
            .filter_map(|hit| {
                let row = presenters.iter_mut().find_map(|describe| describe(&hit))?;
                let in_scope = scope.is_none_or(|scope| {
                    self.project_of(&row.address, cx)
                        .is_some_and(|project| project.address == *scope)
                });
                in_scope.then_some((row, hit.marks))
            })
            .collect()
    }

    pub(crate) fn finish_places_search(
        &mut self,
        input: &str,
        hits: Vec<crate::TextHit>,
        cx: &mut Context<Self>,
    ) {
        if let Some(mut prompt) = self.minibuffer.take() {
            prompt.finish_text(self, input, hits, cx);
            self.minibuffer = Some(prompt);
            cx.notify();
        }
    }

    /// Opens the place a remembered line is in, on that line: the message
    /// in a thread, the line in a note.
    pub fn open_text_match(
        &mut self,
        hit: &crate::FoundText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_at(hit.address.clone(), &hit.point, window, cx);
    }

    /// Opens `address` with the point on `point` in it: a message in its
    /// thread, a line in a note.
    pub fn open_at(
        &mut self,
        address: Address,
        point: &crate::SearchPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.current_address() == &address {
            true => self
                .current_surface()
                .clone()
                .show_search_match(point, window, cx),
            false => self.open_by(address, Open::Search(point), window, cx),
        }
    }

    fn screen_binding(
        &self,
        action: &dyn gpui::Action,
        window: &Window,
        cx: &App,
    ) -> Option<String> {
        let focus = window.focused(cx)?;
        window
            .bindings_for_action_in(action, &focus)
            .last()
            .map(|binding| {
                binding
                    .keystrokes()
                    .iter()
                    .map(|key| key.unparse(cx.platform_info().os))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
    }

    /// `:`: `Commands`, every command there is, each with what it acts on
    /// from where the user is.
    pub(crate) fn commands_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self
            .commands(window, cx)
            .into_iter()
            .map(|command| {
                let keys = match &command.shortcut {
                    Some(Shortcut::Action(action)) => {
                        self.screen_binding(action.as_ref(), window, cx)
                    }
                    Some(Shortcut::Keys(keys)) => Some((*keys).to_owned()),
                    None => None,
                };
                (command, keys.unwrap_or_default())
            })
            .collect();
        // Words after a command that acts on a place find the place as
        // `Places` would rank it first.
        let places = places::Listing::new(self.place_rows(None, cx));
        let find_place: commands::FindPlace = Rc::new(move |words| {
            let query = words.trim().to_lowercase();
            let query = crate::text_search::TextQuery::parse(&query);
            let row = places.rank_query(&query).into_iter().next()?;
            let places::Choice::Place(place) = &places.choices[row.index] else {
                return None;
            };
            let found = &place.found;
            Some(commands::Resolved {
                address: found.address.clone(),
                name: match found.description.is_empty() {
                    true => found.title.clone(),
                    false => format!("{} · {}", found.title, found.description),
                },
            })
        });
        let listing =
            commands::Listing::finding_places(entries, &self.recent_commands, Some(find_place));
        self.open_minibuffer(":", Answer::commands(listing), None, window, cx);
    }

    /// `Commands` with `line` already typed, as `space s` opens it at
    /// `snooze thread `.
    pub(crate) fn commands_prompt_at(
        &mut self,
        line: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commands_prompt(window, cx);
        if let Some(mut prompt) = self.minibuffer.take() {
            prompt.set_input(line.to_owned(), window, cx);
            prompt.refresh(self, cx);
            self.minibuffer = Some(prompt);
            cx.notify();
        }
    }

    /// The window's commands, the screen's, and each system's. A command
    /// is listed once whatever the screen: one about something the screen
    /// does not name asks for it (`window.commands are global`).
    fn commands(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<Command> {
        let point = self.point(cx);
        let mut commands = crate::own::commands(self, cx);
        commands.extend(crate::focus::commands(self, cx));
        commands.extend(crate::hosts::commands(self));
        commands.extend(crate::gccp::commands());
        commands.extend(crate::devices::commands());
        let surface = self.current_surface().clone();
        commands.extend(surface.commands(self.card_seen(self.current_address(), cx), window, cx));
        for system in self.systems.clone() {
            commands.extend(system.commands(&point, window, cx));
        }
        debug_assert!(
            {
                let mut names: Vec<_> = commands.iter().map(Command::name).collect();
                names.sort_unstable();
                names.windows(2).all(|pair| pair[0] != pair[1])
            },
            "two commands share a name"
        );
        commands
    }

    /// Runs `command` as if from `place`: the command of the same name
    /// that the systems offer there, so `stop agent clock` stops the agent
    /// of the `clock` thread wherever the user is (`window.Commands`).
    pub(crate) fn run_command_at(
        &mut self,
        command: &Command,
        place: commands::Resolved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let point = Point {
            surface: place.address.clone(),
            subject: Subject::Own,
            card: None,
        };
        let there = self
            .commands_at(&point, window, cx)
            .into_iter()
            .find(|there| there.name() == command.name() && there.takes_place());
        match there {
            Some(there) => self.run_command(&there, None, window, cx),
            None => self.echo(
                &format!("{}: nothing to act on at {}", command.name(), place.name),
                StyleClass::SystemInfo,
                cx,
            ),
        }
    }

    /// The commands every system offers at `point`, a place that need not
    /// be in view.
    fn commands_at(
        &mut self,
        point: &Point,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Command> {
        let mut commands = Vec::new();
        for system in self.systems.clone() {
            commands.extend(system.commands(point, window, cx));
        }
        commands
    }

    /// Runs `command` with what was typed after its name, and keeps it
    /// among the commands run last.
    pub(crate) fn run_command(
        &mut self,
        command: &Command,
        argument: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = command.name().to_owned();
        self.recent_commands.retain(|ran| *ran != name);
        self.recent_commands.insert(0, name);
        self.recent_commands.truncate(RECENT_COMMANDS);
        self.place
            .write(place::Write::Commands(place::SavedCommands {
                names: self.recent_commands.clone(),
            }));
        command.run(argument, self, window, cx);
    }

    /// The card in view while it waits: what a verdict lands on.
    pub(crate) fn waiting_card(&self, cx: &App) -> Option<Address> {
        self.card_in_view(cx).filter(|address| self.waits(address))
    }

    /// The reminder the screen in view is, due or not.
    pub(crate) fn reminder_in_view(&self, cx: &App) -> Option<Address> {
        (self.current_surface().card(cx) == CardHere::Reminder)
            .then(|| self.current_address().clone())
    }

    /// The answers the system that owns `address` gives its cards in place
    /// of done and mute, when it has its own.
    pub(crate) fn own_verdicts(&self, address: &Address, cx: &mut App) -> Option<Vec<Row>> {
        self.systems
            .iter()
            .find(|system| system.opens(address, cx))
            .cloned()
            .and_then(|system| system.verdicts(address, cx))
    }

    pub fn open_prompt(
        &mut self,
        prompt: impl Into<gpui::SharedString>,
        complete: CandidateSource,
        on_submit: SubmitHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_prompt_cancellable(prompt, complete, on_submit, None, window, cx);
    }

    pub fn open_prompt_cancellable(
        &mut self,
        prompt: impl Into<gpui::SharedString>,
        complete: CandidateSource,
        on_submit: SubmitHandler,
        on_cancel: Option<CancelHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_minibuffer(
            prompt,
            Answer::text(complete, on_submit),
            on_cancel,
            window,
            cx,
        );
    }

    fn open_minibuffer(
        &mut self,
        prompt: impl Into<gpui::SharedString>,
        answer: Answer,
        on_cancel: Option<CancelHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = prompt.into();
        rho_journal::record(rho_journal::Event::MinibufferOpened {
            prompt: prompt.to_string(),
        });
        self.prefix = None;
        self.take_keyboard(window, cx);
        let text_style = self
            .active_editor(cx)
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let mut minibuffer = Minibuffer::open(prompt, &text_style, answer, on_cancel, window, cx);
        minibuffer.refresh(self, cx);
        self.minibuffer = Some(minibuffer);
        // The strip is single-occupancy; a stale message after the prompt
        // closes would confuse.
        self.echo = None;
        cx.notify();
    }

    /// The prompt that is open, for its asker to say more about it: that
    /// what is typed is a secret, or that a candidate completes the whole
    /// input.
    pub fn minibuffer_mut(&mut self) -> Option<&mut Minibuffer> {
        self.minibuffer.as_mut()
    }

    pub(crate) fn refresh_minibuffer(&mut self, cx: &mut Context<Self>) {
        let Some(mut minibuffer) = self.minibuffer.take() else {
            return;
        };
        minibuffer.refresh(self, cx);
        self.minibuffer = Some(minibuffer);
        cx.notify();
    }

    pub(crate) fn minibuffer_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(minibuffer) = self.minibuffer.as_ref() else {
            return;
        };
        if !minibuffer.has_answer() {
            return;
        }
        if let Some(reason) = minibuffer.rejection(cx) {
            self.echo(reason, StyleClass::SystemInfo, cx);
            return;
        }
        let minibuffer = self.minibuffer.take().expect("the prompt is open");
        let prompt = minibuffer.prompt().to_owned();
        let secret = minibuffer.is_secret();
        let (input, submission) = minibuffer.into_submission(cx);
        rho_journal::record(rho_journal::Event::MinibufferSubmitted {
            prompt,
            input: match secret {
                true => String::new(),
                false => input.clone(),
            },
        });
        self.give_keyboard_back(window, cx);
        submission(self, window, cx);
        self.ask_pairing_when_free(window, cx);
        cx.notify();
    }

    /// Escape on a prompt: it closes, and its asker hears that the user
    /// said no ([`CancelHandler`]).
    pub(crate) fn minibuffer_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on_cancel = self.minibuffer.as_ref().and_then(Minibuffer::on_cancel);
        self.close_minibuffer(window, cx);
        if let Some(on_cancel) = on_cancel {
            on_cancel(self, window, cx);
        }
        self.ask_pairing_when_free(window, cx);
        cx.notify();
    }

    /// Closes the prompt without an answer, as a move away from it does:
    /// its asker hears nothing.
    fn close_minibuffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(minibuffer) = self.minibuffer.take() else {
            return;
        };
        rho_journal::record(rho_journal::Event::MinibufferCancelled {
            prompt: minibuffer.prompt().to_owned(),
            input: match minibuffer.is_secret() {
                true => String::new(),
                false => minibuffer.input(cx),
            },
        });
        self.give_keyboard_back(window, cx);
        cx.notify();
    }

    fn minibuffer_select(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.select_by_delta(delta);
            cx.notify();
        }
    }

    fn minibuffer_complete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.complete_selected(window, cx);
            cx.notify();
        }
    }

    fn minibuffer_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.clear_input(window, cx);
            cx.notify();
        }
    }

    /// A click on a Desk row: its card. On a phone, a press that lands
    /// while a prompt or a snap is open is not a tap on the Desk.
    pub(crate) fn open_row(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.minibuffer.is_some() || self.phone.snapping() {
            return;
        }
        if let Some(card) = self
            .hand()
            .cards
            .iter()
            .find(|card| &card.address == address)
            .cloned()
        {
            self.journal_deal(Some(address), cx);
            self.open_card(card, window, cx);
        }
    }

    // ---- render ----

    /// The phone's one status line: the heading and its words, then the
    /// lamp, the hosts and the mode. A phone has no notch to split it.
    fn render_phone_status_line(
        &mut self,
        text_style: &gpui::TextStyle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let mode = self.mode(window, cx);
        let mode = mode_word(&mode);
        let mode_color = mode_color(&mode, cx);
        // Signal evaluation is deferred. Desk must not paint the lamp
        // from the previous screen while that update is still pending.
        let lamp = (self.signals.lamp_on && !self.desk_in_view()).then_some("●");
        let (screen_host, notices) = self.host_notices(cx);
        let focus = self.focus_note(Timestamp::now(), cx);
        let surface = self.current_surface();
        let phone_titles = surface.phone_titles(cx);
        let surface_status = match &screen_host {
            Some(name) => format!("{name} away"),
            None => surface.status(cx).unwrap_or_default(),
        };
        let measured = crate::phone::measure(text_style, window);
        // The pairing hint is secondary to a surface that has already
        // shed everything but the name of the work and its condition.
        let show_phone_notice = phone_titles.len() < 2
            || phone_titles.last().is_none_or(|title| {
                let reserve = lamp
                    .into_iter()
                    .chain(focus.as_deref())
                    .chain(notices.iter().map(|notice| notice.words.as_str()))
                    .chain(["unpaired", mode.as_str()])
                    .map(|words| measured(words) + px(12.))
                    .fold(px(0.), |sum, width| sum + width);
                crate::phone::fit_heading(
                    title,
                    &surface_status,
                    window.viewport_size().width - reserve,
                    &measured,
                )
                .0 == *title
            });
        let unpaired = (self.echo.is_none() && self.attention.pairing() != Pairing::Paired)
            .then_some(show_phone_notice.then_some("unpaired"))
            .flatten();
        // A heading is cut to what the line has room for: a phone is too
        // narrow to guess at, and a cut by length there would push the
        // card's state off the edge.
        let right_words = lamp
            .into_iter()
            .chain(focus.as_deref())
            .chain(notices.iter().map(|notice| notice.words.as_str()))
            .chain(unpaired)
            .chain([mode.as_str()])
            .collect::<Vec<_>>();
        let fit = |path: &str, state: &str| {
            let measure = crate::phone::measure(text_style, window);
            let right = right_words
                .iter()
                .map(|words| measure(words) + px(12.))
                .fold(px(0.), |sum, width| sum + width);
            let fit = if self.echo.is_some() {
                crate::phone::fit_echo_heading
            } else {
                crate::phone::fit_heading
            };
            fit(path, state, window.viewport_size().width - right, measure)
        };
        // Echo replaces the state, not the heading, so a verdict does
        // not hide which screen it left us on.
        let own_card = self.own_card(cx);
        let (heading, state, state_color) = if let Some(address) = own_card {
            // The card's words while it waits; otherwise the surface says
            // what it is doing, as any other screen does.
            let heading = surface
                .heading(cx)
                .unwrap_or_else(|| self.card_for(&address, cx).heading());
            let waiting = self
                .hand()
                .cards
                .iter()
                .find(|card| card.address == address)
                .map(|card| card.label.clone());
            match waiting {
                _ if self.marks().get(&address).facts().muted() => {
                    (heading, "muted".to_owned(), colors.text_muted)
                }
                Some(label) => (heading, label, status.warning),
                None => (heading, surface_status, colors.text_muted),
            }
        } else {
            let title = phone_titles
                .iter()
                .find(|title| fit(title, &surface_status).0 == **title)
                .or_else(|| phone_titles.last())
                .cloned()
                .unwrap_or_default();
            (title, surface_status, colors.text_muted)
        };
        let (state, state_color) = self.echo.as_ref().map_or((state, state_color), |echo| {
            (echo.text().to_owned(), status.info)
        });
        let (heading, state) = fit(&heading, &state);
        let left = div()
            .flex()
            .gap_3()
            .min_w_0()
            .child(div().truncate().child(heading))
            .children(
                (!state.is_empty()).then(|| div().truncate().text_color(state_color).child(state)),
            )
            .into_any_element();
        // A host that is away outranks the notice, which is cut first when
        // the line is short. Only a host that needs the user, or one the
        // screen in view needs, is red; the rest and the focus are quiet.
        let right = div()
            .flex()
            .flex_row()
            .min_w_0()
            .items_center()
            .gap_3()
            .children(lamp.map(|lamp| div().flex_none().text_color(status.warning).child(lamp)))
            .children(
                focus.map(|focus| div().flex_none().text_color(colors.text_muted).child(focus)),
            )
            .children(notices.into_iter().map(|notice| {
                div()
                    .flex_none()
                    .text_color(match notice.alarm {
                        true => status.error,
                        false => colors.text_muted,
                    })
                    .child(notice.words)
            }))
            .children(unpaired.map(|text| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(colors.text_muted)
                    .child(text)
            }))
            .child(mode_element(mode, mode_color));
        div()
            .id("rho-status-line")
            .h(px(26.))
            .min_h_0()
            .w_full()
            .px_2()
            .border_b_1()
            .border_color(colors.border_variant.opacity(0.6))
            .flex()
            .flex_row()
            .items_center()
            .bg(colors.editor_background)
            .text_color(colors.text)
            .font_family(text_style.font_family.clone())
            .text_size(text_style.font_size)
            .line_height(text_style.line_height)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(left),
            )
            .child(right)
            .into_any_element()
    }

    /// What the top-left corner says of the screen in view, for a screen
    /// that does not draw its own (`window.Status line`): its name in
    /// `unread text`, where it is in `quiet`, and the page's own markers.
    /// A thread never repeats its card's age or its agent's model here;
    /// the newest message below already shows the age.
    fn left_ear(&self, cx: &App) -> LeftEar {
        let surface = self.current_surface();
        let colors = cx.theme().colors();
        let attention = rho_window::style::attention_color(cx);
        let own_card = self.own_card(cx);
        let heading = match &own_card {
            Some(address) => surface
                .heading(cx)
                .unwrap_or_else(|| self.card_for(address, cx).heading()),
            None => surface.title(cx),
        };
        let (context, title) = match heading.rsplit_once(" › ") {
            Some((context, title)) => (Some(context.to_owned()), title.to_owned()),
            None => (None, heading),
        };
        let mut markers = Vec::new();
        if let Some(address) = &own_card {
            if self.marks().get(address).facts().muted() {
                markers.push(("muted".to_owned(), colors.text_muted.into()));
            }
            let kind = self
                .hand()
                .cards
                .iter()
                .find(|card| &card.address == address)
                .map(|card| card.kind);
            match kind {
                Some(rho_dealer::rank::CardKind::Approve(what)) => {
                    markers.push((format!("approve {what}"), attention))
                }
                Some(rho_dealer::rank::CardKind::Stop(word)) => {
                    markers.push((word.to_owned(), attention))
                }
                _ => {}
            }
        }
        if let Some(status) = surface.status(cx).filter(|status| !status.is_empty())
            && markers.iter().all(|(marker, _)| *marker != status)
        {
            markers.push((status, colors.text_muted.into()));
        }
        LeftEar {
            title,
            context,
            markers,
        }
    }

    /// The default top-left corner, drawn in the screen's own layer so a
    /// prompt dims it with the screen.
    fn render_left_ear(&self, notch: Notch, cx: &App) -> AnyElement {
        let colors = cx.theme().colors();
        let LeftEar {
            title,
            context,
            markers,
        } = self.left_ear(cx);
        div()
            .id("rho-left-ear")
            .flex_none()
            .h(px(TOP_ROW_PX))
            .w(notch.start)
            .pl(px(READING_INSET_PX))
            .pr(px(EAR_GAP_PX))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(EAR_GAP_PX))
            .overflow_hidden()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rho_window::style::unread_text_color(cx))
                    .child(title),
            )
            .children(context.map(|context| {
                div()
                    .flex_none()
                    .max_w(notch.start * 0.3)
                    .truncate()
                    .text_color(colors.text_muted)
                    .child(context)
            }))
            .children(markers.into_iter().map(|(marker, color)| {
                div().flex_none().truncate().text_color(color).child(marker)
            }))
            .into_any_element()
    }

    /// The square black mask over the camera notch, unless `toggle notch
    /// mask` turned it off, and, right of it, the framed global status bar
    /// (`window.Status line`): the echo, the dealer focus, hosts that do
    /// not answer, the unpaired notice, the waiting lamp, and the mode.
    /// While an echo shows, it has the room of every quiet note; a host
    /// that needs the user stays. Drawn over the screen and over a
    /// prompt's dimming, so what rho says stays bright while a prompt is
    /// open.
    fn render_top_row(
        &mut self,
        text_style: &gpui::TextStyle,
        notch: Notch,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl Iterator<Item = AnyElement> + use<> {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let mode = mode_word(&self.mode(window, cx));
        let mode_color = mode_color(&mode, cx);
        // Signal evaluation is deferred. Desk must not paint the lamp
        // from the previous screen while that update is still pending.
        let lamp = (self.signals.lamp_on && !self.desk_in_view()).then_some("●");
        let StatusNotes {
            focus,
            notices,
            unpaired,
        } = self.status_notes(cx);
        let echo = self.echo.as_ref().map(|echo| {
            let color = match echo.class() {
                StyleClass::StatusError | StyleClass::Disconnect => {
                    rho_window::style::failed_color(cx)
                }
                StyleClass::StatusOk => status.success.into(),
                _ => status.info.into(),
            };
            div()
                .id("rho-echo")
                .min_w_0()
                .truncate()
                .text_color(color)
                .child(echo.text().to_owned())
        });
        let mask = self.notch_mask.then(|| {
            div()
                .id("rho-notch-mask")
                .absolute()
                .top_0()
                .left(notch.start)
                .w(notch.end - notch.start)
                .h(px(TOP_ROW_PX))
                .bg(rho_window::style::notch_mask_color())
                .into_any_element()
        });
        let bar = div()
            .id("rho-status-bar")
            .absolute()
            .top_0()
            .left(notch.end)
            .right_0()
            .h(px(TOP_ROW_PX))
            .px(px(EAR_GAP_PX))
            .border_l_1()
            .border_b_1()
            .border_color(colors.border)
            .bg(rho_window::style::global_status_surface(cx))
            .occlude()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(EAR_GAP_PX))
            .overflow_hidden()
            .font_family(text_style.font_family.clone())
            .text_size(text_style.font_size)
            .text_color(colors.text_muted)
            .child(div().flex_1().min_w_0().flex().children(echo))
            .children(focus.map(|focus| div().flex_none().child(focus)))
            // A host that needs the user, or one the screen in view needs,
            // is red; the rest are an everyday state and quiet.
            .children(notices.into_iter().map(|notice| {
                div()
                    .flex_none()
                    .text_color(match notice.alarm {
                        true => status.error,
                        false => colors.text_muted,
                    })
                    .child(notice.words)
            }))
            .children(unpaired.map(|text| div().min_w_0().truncate().child(text)))
            .children(lamp.map(|lamp| {
                div()
                    .flex_none()
                    .text_color(rho_window::style::attention_color(cx))
                    .child(lamp)
            }))
            .child(mode_element(mode, mode_color))
            .into_any_element();
        mask.into_iter().chain([bar])
    }

    /// The status bar's notes beside the echo. An echo takes the room of
    /// every quiet one for its few seconds; a host that needs the user
    /// stays.
    pub(crate) fn status_notes(&self, cx: &App) -> StatusNotes {
        let quiet = self.echo.is_none();
        let (_, notices) = self.host_notices(cx);
        StatusNotes {
            focus: self.focus_note(Timestamp::now(), cx).filter(|_| quiet),
            notices: notices
                .into_iter()
                .filter(|notice| quiet || notice.alarm)
                .collect(),
            unpaired: (quiet && self.attention.pairing() != Pairing::Paired)
                .then_some("verdicts stay on this device until it is paired"),
        }
    }

    /// The host the screen in view lives on when it is away, and the
    /// status line's notice of every host that does not answer. What an
    /// away host's screen says it is doing is only what it last said, so
    /// the host's name says why instead, and its notice is an alarm.
    pub(crate) fn host_notices(&self, cx: &App) -> (Option<String>, Vec<crate::hosts::Notice>) {
        let screen_host = self
            .away_host(self.current_address(), cx)
            .map(|host| host.name.clone());
        let notices = crate::hosts::notice(
            self.hosts
                .iter()
                .map(|host| (host.name.as_str(), Reach::of(&host.status))),
            screen_host.as_deref(),
        );
        (screen_host, notices)
    }

    /// The host `address` lives on, while it is away or needs the user.
    pub(crate) fn away_host(
        &self,
        address: &Address,
        cx: &App,
    ) -> Option<&rho_agent_hosts::hosts::Host> {
        self.host_of(address, cx)
            .and_then(|host| self.hosts.get(host))
            .filter(|host| matches!(Reach::of(&host.status), Reach::Away | Reach::NeedsYou))
    }

    /// The mode the keys are in: a surface's own while it has the keys,
    /// such as a raw place taking live input; else the minibuffer's or the
    /// surface editor's, as vim reads it.
    pub(crate) fn mode(&self, window: &Window, cx: &App) -> String {
        // A prompt over the surface is the editor's to say.
        if self.minibuffer.is_none() {
            match self.current_surface().mode(window, cx) {
                Some(Mode::Normal) => return "normal".to_owned(),
                Some(Mode::Insert) => return "insert".to_owned(),
                None => {}
            }
        }
        let typing = self
            .minibuffer
            .as_ref()
            .map(|minibuffer| minibuffer.editor().clone())
            .into_iter()
            .chain(self.current_surface().editor(cx))
            .any(|editor| {
                editor.focus_handle(cx).is_focused(window) && !editor.read(cx).use_modal_editing()
            });
        if typing {
            return "insert".to_owned();
        }
        self.mode_indicator
            .read(cx)
            .plain_mode(cx)
            .unwrap_or_else(|| "normal".to_owned())
    }
}

/// What the status bar says besides the echo, the lamp and the mode.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StatusNotes {
    /// The dealer focus: `focus tide`, `quiet 32m`.
    pub(crate) focus: Option<String>,
    /// Hosts that do not answer.
    pub(crate) notices: Vec<crate::hosts::Notice>,
    pub(crate) unpaired: Option<&'static str>,
}

/// What the top-left corner says of a screen that does not draw its own.
struct LeftEar {
    title: String,
    context: Option<String>,
    /// Page markers, each in its color: `approve push`, `working`, `muted`.
    markers: Vec<(String, gpui::Hsla)>,
}

/// `same band gap`: the air between items in each ear.
const EAR_GAP_PX: f32 = 12.;

/// `NOR` is quiet; every other mode is keys in motion.
fn mode_color(mode: &str, cx: &App) -> gpui::Hsla {
    match mode {
        "NOR" => cx.theme().colors().text_muted.into(),
        _ => rho_window::style::keys_in_motion_color(cx),
    }
}

/// The mode word in its color: bold while keys are in motion, and never a
/// fill behind it.
fn mode_element(mode: String, color: gpui::Hsla) -> gpui::Div {
    div()
        .flex_none()
        .text_color(color)
        .when(mode != "NOR", |word| {
            word.font_weight(gpui::FontWeight::BOLD)
        })
        .child(mode)
}

/// The status line's mode word: three letters, the way helix does it, so
/// the segment never moves when the mode changes.
fn mode_word(mode: &str) -> String {
    match mode {
        "normal" => "NOR",
        "insert" => "INS",
        "replace" => "REP",
        "visual" => "VIS",
        "visual line" => "V-LINE",
        "visual block" => "V-BLOCK",
        "select" => "SEL",
        other => return other.to_uppercase(),
    }
    .to_owned()
}

fn journal_surface(address: &Address) -> rho_journal::SurfaceIdentity {
    rho_journal::SurfaceIdentity::DeskNode {
        host: 0,
        node_id: address.clone().into(),
    }
}

impl Focusable for Frame {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.current_surface().focus_handle(cx)
    }
}

impl Render for Frame {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.active_editor(cx);
        let text_style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
        let surface = self.current_surface().clone();
        let layout = Layout::of(window.viewport_size().width);
        let frame = div()
            .id("rho-frame")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().colors().editor_background)
            .text_color(cx.theme().colors().text)
            .key_context(crate::KEY_CONTEXT)
            .capture_any_mouse_down(cx.listener(|this, _: &gpui::MouseDownEvent, _, _| {
                this.note_input();
                // A press anywhere disarms `space`, as any other key does.
                this.prefix = None;
            }))
            .on_action(cx.listener(|this, _: &keys::Deal, window, cx| this.pull_card(window, cx)))
            .on_action(
                cx.listener(|this, _: &keys::Close, window, cx| this.close_current(window, cx)),
            )
            .on_action(cx.listener(|this, _: &keys::CloseAndDeal, window, cx| {
                this.close_current(window, cx);
                this.pull_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keys::Places, window, cx| this.places(window, cx)))
            .on_action(
                cx.listener(|this, _: &keys::Commands, window, cx| {
                    this.commands_prompt(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &keys::Leader, window, cx| this.leader(window, cx)))
            .on_action(cx.listener(|this, _: &keys::Desk, window, cx| this.toggle_desk(window, cx)))
            .on_action(cx.listener(|this, _: &keys::RedoEdit, window, cx| {
                let surface = this.current_surface();
                if surface.edits_at_point(cx)
                    && let Some(editor) = surface.editor(cx)
                {
                    editor.update(cx, |editor, cx| {
                        editor.redo(&editor::actions::Redo, window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &keys::SearchForward, window, cx| {
                this.search(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &keys::SearchBackward, window, cx| {
                this.search(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &keys::SearchNext, window, cx| {
                this.search_again(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &keys::SearchPrevious, window, cx| {
                this.search_again(true, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &keys::MinibufferConfirm, window, cx| {
                    this.minibuffer_confirm(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &keys::MinibufferCancel, window, cx| {
                this.minibuffer_cancel(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &keys::MinibufferNext, _, cx| this.minibuffer_select(1, cx)),
            )
            .on_action(cx.listener(|this, _: &keys::MinibufferPrevious, _, cx| {
                this.minibuffer_select(-1, cx)
            }))
            .on_action(
                cx.listener(|this, _: &keys::MinibufferDeleteWord, window, cx| {
                    if let Some(prompt) = &mut this.minibuffer {
                        prompt.delete_word(window, cx);
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &keys::MinibufferComplete, window, cx| {
                    this.minibuffer_complete(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &keys::MinibufferClear, window, cx| {
                this.minibuffer_clear(window, cx);
            }));
        let width = window.viewport_size().width;
        if layout == Layout::Phone {
            let status = self.render_phone_status_line(&text_style, window, cx);
            let minibuffer = self
                .minibuffer
                .as_ref()
                .map(|minibuffer| minibuffer.render(&text_style, layout, width, window, cx));
            let body = self.render_phone_surface(&surface, cx);
            let bar = self.render_phone_bar(window, cx);
            return frame
                .child(status)
                .child(body)
                .children(minibuffer)
                .child(bar);
        }
        // The screen fills the window from its top edge; only the notch
        // mask and the status bar right of it lie over it. A screen that
        // does not draw its own top-left corner gets one here, in its own
        // layer, so a prompt dims it with the screen.
        let notch = Notch::of(width);
        let content = div()
            .size_full()
            .font(text_style.font())
            .text_size(text_style.font_size)
            .line_height(text_style.line_height);
        let content = match surface.draws_top_left(cx) {
            true => content.child(surface.view()),
            false => content
                .flex()
                .flex_col()
                .child(self.render_left_ear(notch, cx))
                .child(div().flex_1().min_h_0().w_full().child(surface.view())),
        };
        let prompt = self.minibuffer.as_ref().map(|minibuffer| {
            let ch = rho_window::layout::ch(&text_style, window);
            let panel = (ch * PROMPT_MAX_CH).min(width - px(READING_INSET_PX) * 2.);
            let panel = minibuffer.render(&text_style, layout, panel, window, cx);
            // The work stays where it is, dimmed behind the panel; a press
            // on it closes the prompt.
            [
                div()
                    .id("rho-prompt-dim")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .bg(gpui::black().opacity(0.35))
                    .on_click(cx.listener(|this, _, window, cx| this.minibuffer_cancel(window, cx)))
                    .into_any_element(),
                div()
                    .absolute()
                    .top(px(TOP_ROW_PX + PROMPT_GAP_PX))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(panel)
                    .into_any_element(),
            ]
        });
        let top_row = self.render_top_row(&text_style, notch, window, cx);
        frame
            .child(content)
            .children(prompt.into_iter().flatten())
            .children(top_row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::Verdict;
    use crate::desk::desk_rows;

    mod focus_and_hosts;

    struct OpenCard(gpui::FocusHandle);

    impl gpui::Focusable for OpenCard {
        fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
            self.0.clone()
        }
    }

    impl gpui::Render for OpenCard {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl crate::Surface for OpenCard {
        fn title(&self, _: &App) -> String {
            "The meter".into()
        }

        fn card(&self, _: &App) -> CardHere {
            CardHere::Itself
        }
    }

    /// A screen that looks into a card's work, as a transcript does.
    struct Transcript(gpui::FocusHandle);

    impl gpui::Focusable for Transcript {
        fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
            self.0.clone()
        }
    }

    impl gpui::Render for Transcript {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl crate::Surface for Transcript {
        fn title(&self, _: &App) -> String {
            "standing · lab".into()
        }

        fn card(&self, _: &App) -> CardHere {
            CardHere::Inspected
        }
    }

    /// A screen that says nothing of cards, as a page, a chart or a note.
    struct Page(gpui::FocusHandle);

    impl gpui::Focusable for Page {
        fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
            self.0.clone()
        }
    }

    impl gpui::Render for Page {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl crate::Surface for Page {
        fn title(&self, _: &App) -> String {
            "web › Rust".into()
        }
    }

    /// A screen called `title` that counts the opens that tell it to
    /// start over, as a thread's open moves its point to the draft.
    struct Named {
        focus: gpui::FocusHandle,
        title: &'static str,
        opened: Rc<std::cell::Cell<u32>>,
    }

    impl gpui::Focusable for Named {
        fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
            self.focus.clone()
        }
    }

    impl gpui::Render for Named {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl crate::Surface for Named {
        fn title(&self, _: &App) -> String {
            self.title.into()
        }

        fn opened(&mut self, _: &mut Window, _: &mut Context<Self>) {
            self.opened.set(self.opened.get() + 1);
        }
    }

    /// Holds a [`Named`] screen at `page/<title>`, and returns its address
    /// and how many times it was opened.
    fn hold_named(
        frame: &mut Frame,
        title: &'static str,
        cx: &mut Context<Frame>,
    ) -> (Address, Rc<std::cell::Cell<u32>>) {
        let address = Address::new("page", title);
        let opened = Rc::new(std::cell::Cell::new(0));
        let view = cx.new(|cx| Named {
            focus: cx.focus_handle(),
            title,
            opened: opened.clone(),
        });
        frame
            .surfaces
            .insert(address.clone(), AnySurface::new(view));
        (address, opened)
    }

    /// `space` over the screen in view: what each key it binds is called,
    /// in order; then `space` is disarmed again.
    fn space(frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) -> Vec<String> {
        frame.leader(window, cx);
        let words = frame
            .menu_rows()
            .into_iter()
            .map(|(_, word)| word)
            .collect();
        frame.prefix = None;
        words
    }

    fn test_frame(cx: &mut gpui::TestAppContext, db: rho_db::RhoDb) -> gpui::WindowHandle<Frame> {
        test_frame_with(cx, db, |_| Vec::new())
    }

    fn test_frame_with(
        cx: &mut gpui::TestAppContext,
        db: rho_db::RhoDb,
        systems: impl FnOnce(&mut App) -> Vec<AnySystem>,
    ) -> gpui::WindowHandle<Frame> {
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
        let systems = cx.update(systems);
        cx.add_window(|window, cx| Frame::new(db, systems, Vec::new(), Rc::new(|| {}), window, cx))
    }

    /// What a host has said of the agent its screens are about.
    #[derive(Clone, Copy)]
    enum Holds {
        NotYetSaid,
        Here,
        Gone,
    }

    /// A system whose one address lives on a host, which has said `holds`.
    struct HostBacked(Rc<std::cell::Cell<Holds>>);

    impl crate::System for HostBacked {
        fn kinds(&self) -> &'static [&'static str] {
            &["agent"]
        }

        fn open(&mut self, _: &Address, _: &mut Window, cx: &mut Context<Self>) -> Opening {
            match self.0.get() {
                Holds::NotYetSaid => Opening::Waiting(vec![HostId(0)]),
                Holds::Here => {
                    Opening::Surface(AnySurface::new(cx.new(|cx| OpenCard(cx.focus_handle()))))
                }
                Holds::Gone => Opening::Gone,
            }
        }

        fn agent_name(&self, agent: rho_agent_types::AgentId, _: &App) -> Option<String> {
            (agent == Self::asker()).then(|| "eng-b8os".to_owned())
        }
    }

    impl HostBacked {
        fn asker() -> rho_agent_types::AgentId {
            rho_agent_types::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap()
        }
    }

    fn host_backed_frame(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::WindowHandle<Frame>, Rc<std::cell::Cell<Holds>>) {
        let holds = Rc::new(std::cell::Cell::new(Holds::NotYetSaid));
        let system = holds.clone();
        let frame = test_frame_with(cx, rho_db::RhoDb::in_memory(), move |cx| {
            vec![AnySystem::new(cx.new(|_| HostBacked(system)))]
        });
        (frame, holds)
    }

    /// The places as the last run left them: `screen` in view, Desk
    /// before it.
    fn saved_on(screen: &Address) -> place::Saved {
        let mut recent = Recent::new(Frame::desk_address(), ScreenEntry::Opened);
        recent.show(screen.clone(), ScreenEntry::Opened);
        place::Saved::of(&recent)
    }

    fn places_in_order(frame: &Frame) -> Vec<Address> {
        frame.recent.keys().cloned().collect()
    }

    fn is_waiting(frame: &Frame) -> bool {
        frame.current_surface().view().downcast::<Waiting>().is_ok()
    }

    #[gpui::test]
    fn a_warm_restart_keeps_a_screen_whose_host_has_not_answered(cx: &mut gpui::TestAppContext) {
        let (frame, holds) = host_backed_frame(cx);
        let agent = Address::new("agent", "standing");
        frame
            .update(cx, |frame, window, cx| {
                frame.last_input = Some(Timestamp::now());
                frame.return_to(saved_on(&agent), window, cx);
                assert_eq!(frame.current_address(), &agent);
                assert!(is_waiting(frame), "the screen waits for its host");
                // To Desk and back again before the host answers.
                frame.toggle_desk(window, cx);
                assert_eq!(frame.current_address(), &Frame::desk_address());
                frame.toggle_desk(window, cx);
                assert_eq!(frame.current_address(), &agent, "the recent places kept it");
                assert!(is_waiting(frame));

                holds.set(Holds::Here);
                frame.reopen_waiting(window, cx);
                assert_eq!(frame.current_address(), &agent);
                assert_eq!(frame.current_surface().title(cx), "The meter");
                assert!(
                    frame.current_surface().focus_handle(cx).is_focused(window),
                    "the keyboard moves to the real screen"
                );
                assert_eq!(
                    places_in_order(frame),
                    [agent.clone(), Frame::desk_address()]
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn an_early_tab_reaches_a_screen_left_behind_the_desk(cx: &mut gpui::TestAppContext) {
        let (frame, holds) = host_backed_frame(cx);
        let agent = Address::new("agent", "standing");
        frame
            .update(cx, |frame, window, cx| {
                // A cold return: Desk in view, the agent just behind it.
                frame.last_input = None;
                frame.return_to(saved_on(&agent), window, cx);
                assert_eq!(frame.current_address(), &Frame::desk_address());
                frame.toggle_desk(window, cx);
                assert_eq!(frame.current_address(), &agent);
                assert!(is_waiting(frame));
                // The host answers while the user is in a prompt: the prompt
                // keeps the keyboard, and gives it to the real screen.
                frame.open_prompt(
                    "name:",
                    Rc::new(|_, _, _| Vec::new()),
                    Rc::new(|_, _, _, _| {}),
                    window,
                    cx,
                );
                holds.set(Holds::Here);
                frame.reopen_waiting(window, cx);
                assert!(frame.minibuffer.is_some(), "the prompt stays open");
                let real = frame.current_surface().focus_handle(cx);
                assert!(!real.is_focused(window));
                frame.minibuffer_cancel(window, cx);
                assert!(real.is_focused(window));
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_screen_its_host_no_longer_holds_is_passed_over(cx: &mut gpui::TestAppContext) {
        let (frame, holds) = host_backed_frame(cx);
        let agent = Address::new("agent", "standing");
        frame
            .update(cx, |frame, window, cx| {
                frame.last_input = Some(Timestamp::now());
                frame.return_to(saved_on(&agent), window, cx);
                assert!(is_waiting(frame));
                holds.set(Holds::Gone);
                frame.reopen_waiting(window, cx);
                assert_eq!(frame.current_address(), &Frame::desk_address());
                assert!(frame.desk.focus_handle(cx).contains_focused(window, cx));
                assert_eq!(
                    places_in_order(frame),
                    [Frame::desk_address()],
                    "it is no longer a place"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_row_on_desk_is_no_card_in_view(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, _, cx| {
                let project = crate::Project {
                    address: Address::new("thread", "lab/1"),
                    name: "tide@lab".into(),
                };
                let subject = Address::new("thread", "lab/2");
                let mut card = frame.card_for(&subject, cx);
                card.kind = rho_dealer::rank::CardKind::Waits;
                let hand = rho_dealer::rank::Hand {
                    cards: vec![card],
                    next_change: None,
                };
                let rows = desk_rows(&hand, |_| Some(project.clone()), |_| None, |_| false);
                frame
                    .desk
                    .update(cx, |desk, cx| desk.set_rows(rows, false, cx));
                assert!(frame.desk_in_view());
                assert_eq!(
                    frame.card_target(cx),
                    None,
                    "the Desk is a glance: no row is under a cursor"
                );
                assert_eq!(frame.card_in_view(cx), None, "a pull must not skip the row");

                let card = cx.new(|cx| OpenCard(cx.focus_handle()));
                frame
                    .surfaces
                    .insert(subject.clone(), AnySurface::new(card));
                frame.recent.show(subject.clone(), ScreenEntry::Opened);
                assert_eq!(
                    frame.card_in_view(cx),
                    Some(subject),
                    "a pull from the actual card can skip it"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn opening_an_agent_by_name_has_no_verdict_target(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let agent = Address::new("agent", "standing");
                let transcript = cx.new(|cx| Transcript(cx.focus_handle()));
                frame
                    .surfaces
                    .insert(agent.clone(), AnySurface::new(transcript));
                frame.open(agent, window, cx);
                assert_eq!(frame.card_target(cx), None);
                assert_eq!(
                    space(frame, window, cx),
                    ["places"],
                    "nothing to undo, no card to judge"
                );
            })
            .unwrap();
    }

    /// A system with one project, `tide@lab`, and a thread in it and one
    /// outside it, all of which `find` lists.
    struct Projects;

    impl Projects {
        fn tide() -> Address {
            Address::new("project", "tide")
        }

        fn thread(&self, key: &str, title: &str) -> Found {
            Found {
                address: Address::new("project", key),
                title: title.into(),
                description: String::new(),
                thread: None,
                recency: 0,
                also: Vec::new(),
            }
        }
    }

    impl crate::System for Projects {
        fn kinds(&self) -> &'static [&'static str] {
            &["project"]
        }

        fn open(&mut self, _: &Address, _: &mut Window, _: &mut Context<Self>) -> Opening {
            Opening::Gone
        }

        fn project(&self, address: &Address, _: &App) -> Option<crate::Project> {
            matches!(address.rest(), "tide" | "tide/1").then(|| crate::Project {
                address: Self::tide(),
                name: "tide@lab".into(),
            })
        }

        fn find(&self, _: &rho_dealer::Marks, _: &App) -> Vec<Found> {
            vec![
                Found {
                    description: "on /srv/tide · one agent · opus-high".into(),
                    ..self.thread("tide", "tide@lab")
                },
                self.thread("tide/1", "Flaky clock test"),
                self.thread("reef/1", "Clock drift on reef"),
            ]
        }
    }

    /// A project has no screen: choosing it in `Places` narrows `Places`
    /// to its threads, and the prompt says so.
    #[gpui::test]
    fn choosing_a_project_in_places_narrows_it_to_its_threads(cx: &mut gpui::TestAppContext) {
        let window = test_frame_with(cx, rho_db::RhoDb::in_memory(), |cx| {
            vec![AnySystem::new(cx.new(|_| Projects))]
        });
        let typed = |text: &'static str, cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| {
                    frame
                        .minibuffer_mut()
                        .unwrap()
                        .set_input(text.into(), window, cx);
                })
                .unwrap();
            cx.run_until_parked();
        };
        let rows = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, _, _| {
                    frame.minibuffer.as_ref().unwrap().row_names()
                })
                .unwrap()
        };
        window
            .update(cx, |frame, window, cx| frame.places(window, cx))
            .unwrap();
        typed("clock", cx);
        assert_eq!(rows(cx), ["Clock drift on reef", "Flaky clock test"]);
        typed("tide@lab", cx);
        assert_eq!(rows(cx)[0], "tide@lab");
        cx.dispatch_action(window.into(), keys::MinibufferConfirm);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, _| {
                assert_eq!(frame.current_address(), &Frame::desk_address());
                let places = frame.minibuffer.as_ref().expect("Places stays open");
                assert_eq!(places.prompt(), "places · tide@lab:");
            })
            .unwrap();
        typed("clock", cx);
        assert_eq!(
            rows(cx),
            ["Flaky clock test"],
            "the reef thread is out of scope"
        );
        typed("tide", cx);
        assert_eq!(
            rows(cx),
            Vec::<String>::new(),
            "the project is not one of its threads"
        );
    }

    #[gpui::test]
    fn a_page_is_no_card_even_where_the_hand_names_it(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let page = Address::new("page", "7");
                let view = cx.new(|cx| Page(cx.focus_handle()));
                frame.surfaces.insert(page.clone(), AnySurface::new(view));
                let mut card = frame.card_for(&page, cx);
                card.kind = rho_dealer::rank::CardKind::Waits;
                frame.signals.hand.cards = vec![card];
                frame.open(page.clone(), window, cx);
                assert_eq!(frame.card_target(cx), None);
                assert_eq!(frame.own_card(cx), None, "the status line keeps its title");
                assert_eq!(
                    space(frame, window, cx),
                    ["places"],
                    "no verdicts on a page"
                );
                // Commands are global, but say that nothing here waits.
                let commands = frame.commands(window, cx);
                let command = |name: &str| {
                    commands
                        .iter()
                        .find(|command| command.name() == name)
                        .unwrap_or_else(|| panic!("{name} is a command everywhere"))
                        .clone()
                };
                let snooze = command("snooze thread");
                assert_eq!(snooze.target_text(), Some("nothing to snooze"));
                assert_eq!(command("done").target_text(), Some("not waiting here"));
                assert_eq!(command("mute").target_text(), Some("no thread here"));
                frame.run_command(&snooze, Some("3h".into()), window, cx);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("snooze thread: nothing to snooze here")
                );
                assert!(!frame.marks().get(&page).facts().muted());
                assert_eq!(frame.marks().get(&page).facts().seen_mail().next(), None);
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_transcript_settles_the_card_as_it_was_when_the_reader_stepped_down(
        cx: &mut gpui::TestAppContext,
    ) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let earlier = Address::new("thread", "lab/2");
                let inspected = Address::new("thread", "lab/7");
                let agent = Address::new("agent", "standing");
                let thread = AnySurface::new(cx.new(|cx| OpenCard(cx.focus_handle())));
                frame.surfaces.insert(inspected.clone(), thread);
                let transcript = AnySurface::new(cx.new(|cx| Transcript(cx.focus_handle())));
                frame.surfaces.insert(agent.clone(), transcript);
                let first = frame.card_for(&earlier, cx);
                let mut card = frame.card_for(&inspected, cx);
                card.seen = Seen::Mail {
                    message: 10,
                    version: 11,
                };
                frame.signals.hand.cards = vec![first.clone(), card.clone()];
                frame.open_card(card.clone(), window, cx);
                // A revision came while the thread was on screen, so the
                // reader saw it before stepping down.
                card.seen = Seen::Mail {
                    message: 10,
                    version: 12,
                };
                frame.signals.hand.cards = vec![first.clone(), card.clone()];
                let origin = frame.card_to_inspect(cx).expect("the open card waits");
                frame.open_inspection(agent.clone(), Some(origin), window, cx);
                // Mail that comes while the reader is in the transcript is
                // out of sight. Neither it nor the same agent's earlier
                // thread is the one the reader chose to inspect.
                card.seen = Seen::Mail {
                    message: 13,
                    version: 13,
                };
                frame.signals.hand.cards = vec![first, card];
                assert_eq!(frame.card_target(cx), Some(inspected.clone()));
                frame.verdict(&inspected, Verdict::Done, "done".to_owned(), window, cx);
                let settled = |frame: &Frame| {
                    frame
                        .marks()
                        .get(&inspected)
                        .facts()
                        .seen_mail()
                        .collect::<Vec<_>>()
                };
                assert_eq!(settled(frame), [(10, 12)]);
                assert_eq!(frame.marks().get(&earlier).facts().seen_mail().next(), None);
                assert_eq!(frame.current_address(), &agent, "stay on the transcript");

                // The card has left the hand; `space` still offers undo,
                // and undo stays on the transcript.
                frame
                    .signals
                    .hand
                    .cards
                    .retain(|card| card.address != inspected);
                assert_eq!(frame.card_target(cx), None);
                assert_eq!(space(frame, window, cx), ["places", "undo"]);
                frame.undo_verdict(window, cx);
                assert_eq!(settled(frame), []);
                assert_eq!(
                    frame.current_address(),
                    &agent,
                    "undo stays on the transcript"
                );

                // Back in the hand, the card is the transcript's again
                // when the reader comes back to it through Desk.
                let card = frame.card_for(&inspected, cx);
                frame.signals.hand.cards.push(card);
                frame.toggle_desk(window, cx);
                frame.toggle_desk(window, cx);
                assert_eq!(frame.current_address(), &agent);
                assert_eq!(frame.card_target(cx), Some(inspected.clone()));
                frame.open(agent, window, cx);
                assert_eq!(
                    frame.card_target(cx),
                    None,
                    "an open by name has no stale origin"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_thread_on_screen_is_its_card_as_the_hand_holds_it(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let thread = Address::new("thread", "lab/7");
                let view = AnySurface::new(cx.new(|cx| OpenCard(cx.focus_handle())));
                frame.surfaces.insert(thread.clone(), view);
                let mut card = frame.card_for(&thread, cx);
                card.seen = Seen::Mail {
                    message: 10,
                    version: 10,
                };
                frame.signals.hand.cards = vec![card.clone()];
                frame.open_card(card.clone(), window, cx);
                // Answered elsewhere, then asked again while on screen.
                card.seen = Seen::Mail {
                    message: 14,
                    version: 14,
                };
                frame.signals.hand.cards = vec![card.clone()];
                assert_eq!(frame.card_seen(&thread, cx), Some(card.seen));
                frame.verdict(&thread, Verdict::Done, "done".to_owned(), window, cx);
                assert_eq!(
                    frame
                        .marks()
                        .get(&thread)
                        .facts()
                        .seen_mail()
                        .collect::<Vec<_>>(),
                    [(14, 14)]
                );

                // Reached by name, after the deal or a restart, the same.
                frame.signals.hand.cards = vec![card.clone()];
                let view = AnySurface::new(cx.new(|cx| OpenCard(cx.focus_handle())));
                frame.surfaces.insert(thread.clone(), view);
                frame.recent.show(thread.clone(), ScreenEntry::Opened);
                assert_eq!(frame.card_seen(&thread, cx), Some(card.seen));
                frame.signals.hand.cards.clear();
                assert_eq!(
                    frame.card_seen(&thread, cx),
                    None,
                    "nothing waits to settle"
                );
                assert_eq!(
                    frame.card_target(cx),
                    Some(thread),
                    "a thread that does not wait still takes a mute"
                );
            })
            .unwrap();
    }

    /// A system whose wants the test sets.
    struct Wants(Rc<std::cell::RefCell<Vec<rho_dealer::Claim>>>);

    impl crate::System for Wants {
        fn kinds(&self) -> &'static [&'static str] {
            &["thread"]
        }

        fn open(&mut self, _: &Address, _: &mut Window, cx: &mut Context<Self>) -> Opening {
            Opening::Surface(AnySurface::new(cx.new(|cx| OpenCard(cx.focus_handle()))))
        }

        fn claims(
            &self,
            _: &rho_dealer::Marks,
            _: Timestamp,
            _: Option<&mut rho_dealer::rank::Trace>,
            _: &App,
        ) -> Vec<rho_dealer::Claim> {
            self.0.borrow().clone()
        }
    }

    /// A thread that asked `ago`, and that the user wrote to just before
    /// when `just_wrote`.
    fn asks(thread: &Address, ago: jiff::SignedDuration, just_wrote: bool) -> rho_dealer::Claim {
        let since = Timestamp::now() - ago;
        let mut claim = rho_dealer::Claim::new(
            thread.clone(),
            rho_dealer::curve::Curve::Waiting {
                head_start: rho_dealer::curve::AGENT_BLOCKED_HEAD_START,
                since,
            },
            rho_dealer::rank::CardKind::Waits,
            thread.key(),
        );
        if just_wrote {
            claim.bonus = rho_dealer::curve::recency_bonus(since, Timestamp::now());
        }
        claim
    }

    /// A frame over [`Wants`], with the claims the test sets.
    fn wanting_frame(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::WindowHandle<Frame>,
        Rc<std::cell::RefCell<Vec<rho_dealer::Claim>>>,
    ) {
        let wants = Rc::new(std::cell::RefCell::new(Vec::new()));
        let system = wants.clone();
        let frame = test_frame_with(cx, rho_db::RhoDb::in_memory(), move |cx| {
            vec![AnySystem::new(cx.new(|_| Wants(system)))]
        });
        (frame, wants)
    }

    fn titled(mut claim: rho_dealer::Claim, title: &str) -> rho_dealer::Claim {
        claim.title = title.into();
        claim
    }

    #[gpui::test]
    fn a_pull_from_the_sole_waiting_card_stays_and_skips_nothing(cx: &mut gpui::TestAppContext) {
        let (frame, wants) = wanting_frame(cx);
        let sole = Address::new("thread", "lab/1");
        let other = Address::new("thread", "lab/2");
        let hours = jiff::SignedDuration::from_hours;
        wants
            .borrow_mut()
            .push(titled(asks(&sole, hours(2), false), "Clock drift"));
        frame
            .update(cx, |frame, window, cx| {
                frame.systems_changed(cx);
                frame.pull_card(window, cx);
                assert_eq!(frame.current_address(), &sole);
                frame.pull_card(window, cx);
                assert_eq!(frame.current_address(), &sole, "nothing else waits");
                assert!(
                    !frame.attention.skips.contains(&sole),
                    "no skip for a card that would deal again at once"
                );
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("only waiting card: Clock drift")
                );
            })
            .unwrap();

        // With a second card waiting, the pull passes over the first and
        // says so.
        wants
            .borrow_mut()
            .push(titled(asks(&other, hours(1), false), "Retry window"));
        frame
            .update(cx, |frame, window, cx| {
                frame.systems_changed(cx);
                frame.pull_card(window, cx);
                assert_eq!(frame.current_address(), &other);
                assert!(frame.attention.skips.contains(&sole));
                assert_eq!(echo_of(frame).as_deref(), Some("skipped: Clock drift"));
            })
            .unwrap();
    }

    /// Closing falls back along the order of visits, not of opens; `tab`
    /// flips between Desk and the place before it; nothing piles up.
    #[gpui::test]
    fn navigation_is_by_recency(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let desk = Frame::desk_address();
                let (a, _) = hold_named(frame, "a", cx);
                let (b, _) = hold_named(frame, "b", cx);
                let (c, _) = hold_named(frame, "c", cx);
                frame.open(a.clone(), window, cx);
                frame.open(b.clone(), window, cx);
                frame.open(c.clone(), window, cx);
                frame.open(a.clone(), window, cx);
                for _ in 0..5 {
                    frame.toggle_desk(window, cx);
                }
                assert_eq!(frame.current_address(), &desk);
                frame.toggle_desk(window, cx);
                assert_eq!(
                    places_in_order(frame),
                    [a.clone(), desk.clone(), c.clone(), b.clone()],
                    "each place once, newest first"
                );
                frame.close_current(window, cx);
                assert_eq!(frame.current_address(), &desk, "the place visited before a");
                assert!(frame.surface(&a).is_none());
                frame.close_current(window, cx);
                assert_eq!(
                    frame.current_address(),
                    &c,
                    "q on Desk flips to the place before"
                );
                assert_eq!(places_in_order(frame), [c.clone(), desk.clone(), b.clone()]);
                frame.close_current(window, cx);
                frame.close_current(window, cx);
                frame.close_current(window, cx);
                assert_eq!(places_in_order(frame), [desk.clone()]);
                frame.close_current(window, cx);
                assert_eq!(frame.current_address(), &desk, "Desk never closes");
                assert_eq!(echo_of(frame).as_deref(), Some("no other place yet"));
            })
            .unwrap();
    }

    /// A place come back to is as it was left: only an open by name tells
    /// the screen to start over.
    #[gpui::test]
    fn a_place_come_back_to_is_as_it_was_left(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let (thread, opened) = hold_named(frame, "thread", cx);
                let (page, _) = hold_named(frame, "page", cx);
                frame.open(thread.clone(), window, cx);
                assert_eq!(opened.get(), 1);
                frame.toggle_desk(window, cx);
                frame.toggle_desk(window, cx);
                assert_eq!(frame.current_address(), &thread);
                frame.open(page.clone(), window, cx);
                frame.previous_place(window, cx);
                assert_eq!(frame.current_address(), &thread);
                assert_eq!(opened.get(), 1, "tab and the recent places revisit");
                frame.open(thread.clone(), window, cx);
                assert_eq!(opened.get(), 2, "an open by name starts over");
            })
            .unwrap();
    }

    /// With nothing typed, `Places` lists where the reader has been, newest
    /// first, without the screen in view or Desk, and `enter` flips back.
    #[gpui::test]
    fn blank_places_flip_back_to_the_previous_place(cx: &mut gpui::TestAppContext) {
        let window = test_frame(cx, rho_db::RhoDb::in_memory());
        let (thread, opened) = window
            .update(cx, |frame, window, cx| {
                let (thread, opened) = hold_named(frame, "Flaky clock test", cx);
                let (page, _) = hold_named(frame, "web › Rust", cx);
                let (term, _) = hold_named(frame, "tide · terminal 1", cx);
                frame.open(term, window, cx);
                frame.open(thread.clone(), window, cx);
                frame.open_desk(window, cx);
                frame.open(page, window, cx);
                frame.places(window, cx);
                assert_eq!(
                    frame.minibuffer.as_ref().unwrap().row_names(),
                    ["Flaky clock test", "tide · terminal 1"]
                );
                (thread, opened)
            })
            .unwrap();
        cx.dispatch_action(window.into(), keys::MinibufferConfirm);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, _| {
                assert_eq!(frame.current_address(), &thread);
                assert_eq!(opened.get(), 1, "the thread is as it was left");
            })
            .unwrap();
    }

    /// `space` is the same leader everywhere: `f` places, and `u` only
    /// while there is a verdict to take back. A waiting card adds its
    /// verdicts; nothing else ever does, and `n` is gone.
    #[gpui::test]
    fn the_leader_offers_verdicts_only_on_a_waiting_card(cx: &mut gpui::TestAppContext) {
        let (frame, wants) = wanting_frame(cx);
        let thread = Address::new("thread", "lab/1");
        wants.borrow_mut().push(titled(
            asks(&thread, jiff::SignedDuration::from_hours(2), false),
            "Clock drift",
        ));
        frame
            .update(cx, |frame, window, cx| {
                frame.systems_changed(cx);
                frame.pull_card(window, cx);
                assert_eq!(frame.current_address(), &thread);
                assert_eq!(
                    space(frame, window, cx),
                    ["places", "done", "mute", "snooze"]
                );
                wants.borrow_mut().clear();
                frame.systems_changed(cx);
                frame.verdict(&thread, Verdict::Done, "done".to_owned(), window, cx);
                assert!(frame.desk_in_view(), "nothing else waits");
                assert!(frame.attention.can_undo());
                assert_eq!(
                    space(frame, window, cx),
                    ["places", "undo"],
                    "no verdicts and no deal on Desk"
                );
            })
            .unwrap();
    }

    /// A screen the user schedules, as a note is; the point it was last
    /// put on, as a thread shows a message.
    struct Scheduled {
        focus: gpui::FocusHandle,
        card: CardHere,
        shown: Rc<std::cell::RefCell<Option<crate::SearchPoint>>>,
    }

    impl gpui::Focusable for Scheduled {
        fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
            self.focus.clone()
        }
    }

    impl gpui::Render for Scheduled {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl crate::Surface for Scheduled {
        fn title(&self, _: &App) -> String {
            "Call the dentist".into()
        }

        fn card(&self, _: &App) -> CardHere {
            self.card.clone()
        }

        fn show_search_match(
            &mut self,
            point: &crate::SearchPoint,
            _: &mut Window,
            _: &mut Context<Self>,
        ) {
            *self.shown.borrow_mut() = Some(point.clone());
        }
    }

    /// Notes, which are reminders, and threads, which show messages, with
    /// the claims the test sets.
    struct Reminders {
        wants: Rc<std::cell::RefCell<Vec<rho_dealer::Claim>>>,
        shown: Rc<std::cell::RefCell<Option<crate::SearchPoint>>>,
    }

    impl crate::System for Reminders {
        fn kinds(&self) -> &'static [&'static str] {
            &["note", "thread"]
        }

        fn open(&mut self, address: &Address, _: &mut Window, cx: &mut Context<Self>) -> Opening {
            let card = match address.kind() {
                "note" => CardHere::Reminder,
                _ => CardHere::Itself,
            };
            let shown = self.shown.clone();
            Opening::Surface(AnySurface::new(cx.new(|cx| Scheduled {
                focus: cx.focus_handle(),
                card,
                shown,
            })))
        }

        fn claims(
            &self,
            _: &rho_dealer::Marks,
            _: Timestamp,
            _: Option<&mut rho_dealer::rank::Trace>,
            _: &App,
        ) -> Vec<rho_dealer::Claim> {
            self.wants.borrow().clone()
        }

        /// A message `thread/lab/7.3` is read in `thread/lab/7`.
        fn shown_in(&self, address: &Address, _: &App) -> Option<Address> {
            let (thread, _) = address.rest().split_once('.')?;
            Some(Address::new("thread", thread))
        }
    }

    fn reminders_frame(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::WindowHandle<Frame>,
        Rc<std::cell::RefCell<Vec<rho_dealer::Claim>>>,
        Rc<std::cell::RefCell<Option<crate::SearchPoint>>>,
    ) {
        let wants = Rc::new(std::cell::RefCell::new(Vec::new()));
        let shown = Rc::new(std::cell::RefCell::new(None));
        let system = Reminders {
            wants: wants.clone(),
            shown: shown.clone(),
        };
        let frame = test_frame_with(cx, rho_db::RhoDb::in_memory(), move |cx| {
            vec![AnySystem::new(cx.new(|_| system))]
        });
        (frame, wants, shown)
    }

    /// A note takes `space s` before it waits, which sets its reminder
    /// and leaves the note on screen. Once the reminder is due it is a
    /// card that takes done too, never mute, and done puts it away and
    /// deals the next, as any card's does.
    #[gpui::test]
    fn a_note_takes_a_reminder_before_it_waits_and_done_once_it_is_due(
        cx: &mut gpui::TestAppContext,
    ) {
        let (frame, wants, _) = reminders_frame(cx);
        let note = Address::note(uuid::Uuid::new_v4());
        frame
            .update(cx, |frame, window, cx| {
                frame.open(note.clone(), window, cx);
                assert_eq!(space(frame, window, cx), ["places", "snooze"]);
                assert_eq!(
                    frame.reminder_in_view(cx),
                    Some(note.clone()),
                    ":snooze note too"
                );
                let presets = |frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>| {
                    frame
                        .commands(window, cx)
                        .into_iter()
                        .find(|command| command.name() == "snooze note")
                        .map(|command| command.presets.to_vec())
                        .expect("snooze note")
                };
                assert_eq!(presets(frame, window, cx)[0], "1h");
                frame.snooze(&note, crate::when::SnoozeUnit::Hours, Some(3), window, cx);
                assert_eq!(frame.current_address(), &note, "the note stays on screen");
                assert!(!frame.desk_in_view());
                let snooze = frame.marks().get(&note).facts().snooze().expect("set");
                assert_eq!(
                    snooze.set.timestamp().duration_until(snooze.until),
                    jiff::SignedDuration::from_hours(3)
                );
                assert_eq!(
                    space(frame, window, cx),
                    ["places", "snooze", "undo"],
                    "no done before it is due"
                );
                assert_eq!(
                    presets(frame, window, cx)[..2],
                    ["now", "clear"],
                    "a reminder set can come due now or go"
                );
                // It came due: as if the time passed, a snooze that ended.
                frame.write_marks(
                    vec![crate::attention::Write::Fact(
                        rho_dealer::facts::Fact::Snooze {
                            node: note.clone(),
                            until: rho_dealer::Until::In(-jiff::SignedDuration::from_secs(1)),
                        },
                    )],
                    cx,
                );
                let mut due = rho_dealer::Claim::new(
                    note.clone(),
                    rho_dealer::curve::Curve::Waiting {
                        head_start: rho_dealer::curve::REMINDER_HEAD_START,
                        since: Timestamp::now(),
                    },
                    rho_dealer::rank::CardKind::Due,
                    "set",
                );
                due.title = "Call the dentist".into();
                *wants.borrow_mut() = vec![due];
                frame.systems_changed(cx);
            })
            .unwrap();
        cx.run_until_parked();
        frame
            .update(cx, |frame, window, cx| {
                assert_eq!(
                    space(frame, window, cx),
                    ["places", "done", "snooze", "undo"]
                );
                wants.borrow_mut().clear();
                frame.verdict(&note, Verdict::Done, "done".to_owned(), window, cx);
                assert!(frame.desk_in_view(), "put away, and nothing else waits");
                assert!(frame.marks().get(&note).facts().snooze().is_none());
            })
            .unwrap();
    }

    /// A private reply is read under its message: opening the note opens
    /// the thread the message is in, at the message.
    #[gpui::test]
    fn a_private_reply_opens_its_thread_at_its_message(cx: &mut gpui::TestAppContext) {
        let (frame, _, shown) = reminders_frame(cx);
        let note = Address::note(uuid::Uuid::new_v4());
        let alone = Address::note(uuid::Uuid::new_v4());
        let message = Address::new("thread", "lab/7.3");
        use crate::attention::Write;
        frame
            .update(cx, |frame, window, cx| {
                frame.write_marks(
                    vec![Write::Fact(rho_dealer::facts::Fact::About {
                        node: note.clone(),
                        about: Some(message.clone()),
                    })],
                    cx,
                );
                frame.open(note.clone(), window, cx);
                assert_eq!(frame.current_address(), &Address::new("thread", "lab/7"));
                assert_eq!(
                    *shown.borrow(),
                    Some(crate::SearchPoint::Message {
                        message: message.clone(),
                        line: 0
                    })
                );
                assert!(!frame.surfaces.contains_key(&note), "no screen of its own");
                frame.open(alone.clone(), window, cx);
                assert_eq!(
                    frame.current_address(),
                    &alone,
                    "a note about nothing opens"
                );
            })
            .unwrap();
    }

    /// `:later 3h` snoozes the thread in view in one line; a time it cannot
    /// read is echoed, changes nothing and comes back on the line to fix;
    /// and what ran last leads the blank `Commands`.
    #[gpui::test]
    fn a_command_takes_its_argument_on_the_line(cx: &mut gpui::TestAppContext) {
        let (frame, wants) = wanting_frame(cx);
        let thread = Address::new("thread", "lab/1");
        wants.borrow_mut().push(titled(
            asks(&thread, jiff::SignedDuration::from_hours(2), false),
            "Clock drift",
        ));
        frame
            .update(cx, |frame, window, cx| {
                frame.systems_changed(cx);
                frame.pull_card(window, cx);
                assert_eq!(frame.current_address(), &thread);
                frame.commands_prompt(window, cx);
                answer(frame, "snooze 3x", window, cx);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some(
                        "snooze thread: say how long, such as 45m, 3h, 2d, evening, tomorrow or mon 9am"
                    )
                );
                let prompt = frame.minibuffer.as_ref().expect("the line comes back to fix");
                assert_eq!(prompt.input(cx), "snooze thread 3x");
                frame.minibuffer_cancel(window, cx);
                assert_eq!(frame.current_address(), &thread, "nothing changed");
                assert!(!frame.attention.can_undo());
                run_line(frame, "later 3h", window, cx);
                assert!(frame.attention.can_undo(), "the snooze is a verdict");
                assert!(frame.desk_in_view(), "the card is put away");
                assert!(
                    echo_of(frame).is_some_and(|echo| echo.starts_with("snooze until")),
                    "{:?}",
                    echo_of(frame)
                );
                run_line(frame, "quit", window, cx);
            })
            .unwrap();
        frame
            .update(cx, |frame, window, cx| {
                frame.commands_prompt(window, cx);
                let rows = frame.minibuffer.as_ref().unwrap().row_names();
                assert_eq!(
                    rows[..2],
                    ["quit", "snooze thread"],
                    "the commands run last lead"
                );
            })
            .unwrap();
    }

    /// The rows `Commands` shows for `line`: each command's name, the
    /// words after it, and its target or where those words land.
    fn rows_for(
        frame: &mut Frame,
        line: &str,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) -> Vec<(String, Option<String>, Option<String>)> {
        frame.commands_prompt_at(line, window, cx);
        frame.refresh_minibuffer(cx);
        let rows = frame.minibuffer.as_ref().expect("Commands").command_rows();
        frame.minibuffer_cancel(window, cx);
        rows
    }

    /// `space` then `key`, as the user presses them.
    fn press_space(
        frame: &mut Frame,
        key: &'static str,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        frame.leader(window, cx);
        frame.prefix_keystroke(&gpui::Keystroke::parse(key).unwrap(), window, cx);
    }

    /// `space s` opens `Commands` at `snooze thread `, which offers each
    /// preset with where it lands; a time typed lands too. Snoozed, the
    /// thread offers `now` and `clear` first, and `unsnooze thread` wakes
    /// it once.
    #[gpui::test]
    fn snooze_offers_where_each_time_lands_and_a_snooze_can_be_taken_back(
        cx: &mut gpui::TestAppContext,
    ) {
        let (frame, wants) = wanting_frame(cx);
        let thread = Address::new("thread", "lab/1");
        wants.borrow_mut().push(titled(
            asks(&thread, jiff::SignedDuration::from_hours(2), false),
            "Clock drift",
        ));
        frame
            .update(cx, |frame, window, cx| {
                frame.systems_changed(cx);
                frame.pull_card(window, cx);
                press_space(frame, "s", window, cx);
                let prompt = frame.minibuffer.as_ref().expect("space s opens Commands");
                assert_eq!(prompt.input(cx), "snooze thread ");
                frame.minibuffer_cancel(window, cx);

                let rows = rows_for(frame, "snooze thread ", window, cx);
                let values = rows
                    .iter()
                    .map(|(name, value, _)| (name.as_str(), value.as_deref()))
                    .collect::<Vec<_>>();
                assert_eq!(
                    values[..5],
                    crate::when::SNOOZE_PRESETS.map(|preset| ("snooze thread", Some(preset))),
                    "every preset first"
                );
                assert_eq!(
                    values[5..],
                    [("unsnooze thread", None)],
                    "then words inside a name"
                );
                assert!(
                    rows[..5].iter().all(|(.., lands)| lands
                        .as_deref()
                        .is_some_and(|lands| lands.starts_with("→ "))),
                    "{rows:?}"
                );
                // A typed time is a row of its own, ahead of the presets it
                // does not begin.
                let typed = rows_for(frame, "snooze thread 2d", window, cx);
                assert_eq!(typed.len(), 1, "{typed:?}");
                assert_eq!(typed[0].1.as_deref(), Some("2d"));
                assert!(
                    typed[0]
                        .2
                        .as_deref()
                        .is_some_and(|lands| lands.ends_with(" 09:00"))
                );
                let begun = rows_for(frame, "snooze thread ev", window, cx);
                assert_eq!(begun.len(), 1);
                assert_eq!(begun[0].1.as_deref(), Some("evening"));

                run_line(frame, "later 3h", window, cx);
                assert!(frame.desk_in_view(), "the card is put away");
                frame.open(thread.clone(), window, cx);
                assert_eq!(
                    space(frame, window, cx),
                    ["places", "mute", "snooze", "undo"],
                    "asleep, it waits for nothing done, and can still snooze"
                );
                let rows = rows_for(frame, "snooze thread ", window, cx);
                assert_eq!(
                    rows[..2]
                        .iter()
                        .map(|(_, value, lands)| (value.as_deref(), lands.as_deref()))
                        .collect::<Vec<_>>(),
                    [
                        (Some("now"), Some("→ now")),
                        (Some("clear"), Some("→ no snooze"))
                    ]
                );
                run_line(frame, "wake", window, cx);
                let snooze = frame
                    .marks()
                    .get(&thread)
                    .facts()
                    .snooze()
                    .expect("a snooze");
                assert!(snooze.until <= Timestamp::now(), "the snooze is over");
                assert!(echo_of(frame).is_some_and(|echo| echo.starts_with("unsnoozed: ")));
                run_line(frame, "unsnooze thread", window, cx);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("unsnooze thread: nothing snoozed here")
                );
            })
            .unwrap();
    }

    /// `snooze` names the snooze of what is on screen first: the note's
    /// on a note, though it means a thread's more surely elsewhere.
    #[gpui::test]
    fn snooze_on_a_note_is_snooze_note(cx: &mut gpui::TestAppContext) {
        let (frame, _, _) = reminders_frame(cx);
        let note = Address::note(uuid::Uuid::new_v4());
        frame
            .update(cx, |frame, window, cx| {
                let names = |frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>| {
                    rows_for(frame, "snooze", window, cx)
                        .into_iter()
                        .map(|(name, ..)| name)
                        .take(2)
                        .collect::<Vec<_>>()
                };
                assert_eq!(names(frame, window, cx), ["snooze thread", "snooze note"]);
                frame.open(note.clone(), window, cx);
                assert_eq!(names(frame, window, cx), ["snooze note", "snooze thread"]);
                press_space(frame, "s", window, cx);
                let prompt = frame.minibuffer.as_ref().expect("space s opens Commands");
                assert_eq!(prompt.input(cx), "snooze note ");
            })
            .unwrap();
    }

    /// A key `space` knows says why it does nothing here; any other key,
    /// and `escape`, cancels without a word.
    #[gpui::test]
    fn a_refused_space_key_says_why_and_another_cancels_silently(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                assert!(frame.desk_in_view());
                for (key, reason) in [
                    ("d", "not waiting here"),
                    ("m", "no thread here"),
                    ("s", "nothing to snooze here"),
                    ("a", "no push here"),
                    ("x", "no push here"),
                    ("u", "nothing to undo"),
                ] {
                    press_space(frame, key, window, cx);
                    assert_eq!(echo_of(frame).as_deref(), Some(reason), "space {key}");
                    assert!(frame.prefix.is_none());
                    assert!(frame.minibuffer.is_none());
                }
                frame.echo = None;
                for key in ["z", "escape", "ctrl-d"] {
                    press_space(frame, key, window, cx);
                    assert_eq!(echo_of(frame), None, "space {key}");
                    assert!(frame.prefix.is_none(), "space {key}");
                }
            })
            .unwrap();
    }

    /// `mute` works on any open thread, waiting or not; `done` only on a
    /// card that waits.
    #[gpui::test]
    fn mute_takes_any_open_thread_and_done_only_a_waiting_one(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let thread = Address::new("thread", "lab/9");
                let view = cx.new(|cx| OpenCard(cx.focus_handle()));
                frame.surfaces.insert(thread.clone(), AnySurface::new(view));
                frame.open(thread.clone(), window, cx);
                assert_eq!(space(frame, window, cx), ["places", "mute"]);
                press_space(frame, "d", window, cx);
                assert_eq!(echo_of(frame).as_deref(), Some("not waiting here"));
                assert!(frame.marks().get(&thread).facts.is_empty());
                press_space(frame, "m", window, cx);
                assert!(frame.marks().get(&thread).facts().muted());
                assert_eq!(frame.current_address(), &thread, "nothing to put away");
                run_line(frame, "mute", window, cx);
                assert_eq!(echo_of(frame).as_deref(), Some("mute: muted already"));
            })
            .unwrap();
    }

    /// `approve push` and `deny push` answer only the push on screen; from
    /// elsewhere they open its thread to read first. `space x` puts `deny
    /// push` on the line for `enter`, and words that are not the draft's
    /// are refused rather than lost.
    #[gpui::test]
    fn a_push_is_answered_only_where_it_is_read(cx: &mut gpui::TestAppContext) {
        let (frame, waiting) = answers_frame(cx);
        let push = Address::new("git", "1");
        waiting
            .borrow_mut()
            .push(asks(&push, jiff::SignedDuration::from_mins(3), false));
        frame
            .update(cx, |frame, _, cx| frame.systems_changed(cx))
            .unwrap();
        cx.run_until_parked();
        frame
            .update(cx, |frame, window, cx| {
                assert!(frame.desk_in_view());
                run_line(frame, "approve push", window, cx);
                assert_eq!(frame.current_address(), &push, "the push opens to be read");
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("approve push: the push is on screen · read it, then answer")
                );
                assert_eq!(frame.hand().cards.len(), 1, "nothing answered yet");
                assert_eq!(
                    rows_for(frame, "approve", window, cx)[0].0,
                    "approve push",
                    "on the push, before any other approval"
                );

                press_space(frame, "x", window, cx);
                let prompt = frame.minibuffer.as_ref().expect("deny push asks first");
                assert_eq!(prompt.input(cx), "deny push ");
                frame.minibuffer_cancel(window, cx);
                assert_eq!(waiting.borrow().len(), 1, "nothing denied before enter");
                run_line(frame, "deny push not on main", window, cx);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("deny push: only the draft's words go with it · write them in the draft")
                );
                assert_eq!(waiting.borrow().len(), 1, "still waiting");
                run_line(frame, "deny push", window, cx);
                assert!(waiting.borrow().is_empty(), "denied");
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("denied: push · nothing waits")
                );
            })
            .unwrap();
    }

    /// `show messages` lists what the echo said lately, newest first, and
    /// keeps a bounded number.
    #[gpui::test]
    fn show_messages_lists_recent_echoes_newest_first(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                run_line(frame, "show messages", window, cx);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("show messages: nothing said yet")
                );
                frame.echo("lab away", StyleClass::SystemInfo, cx);
                frame.echo("done: Flaky clock\nsecond line", StyleClass::SystemInfo, cx);
                run_line(frame, "messages", window, cx);
                let listed = candidates_of(frame)
                    .into_iter()
                    .map(|(text, at)| {
                        assert_eq!(at.len(), "12:34:56".len(), "{at}");
                        text
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    listed,
                    [
                        "done: Flaky clock",
                        "lab away",
                        "show messages: nothing said yet"
                    ]
                );
                frame.minibuffer_cancel(window, cx);
                for n in 0..150 {
                    frame.echo(&format!("echo {n}"), StyleClass::SystemInfo, cx);
                }
                assert_eq!(frame.echoes().len(), 100);
                assert_eq!(
                    frame.echoes().front().map(|said| said.text.as_str()),
                    Some("echo 50")
                );
                assert_eq!(
                    frame.echoes().back().map(|said| said.text.as_str()),
                    Some("echo 149")
                );
            })
            .unwrap();
    }

    /// `toggle notch mask` turns the black over the notch off and on.
    #[gpui::test]
    fn the_notch_mask_toggles(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                assert!(frame.notch_mask, "on by default");
                run_line(frame, "toggle notch mask", window, cx);
                assert!(!frame.notch_mask);
                assert_eq!(echo_of(frame).as_deref(), Some("notch mask off"));
                run_line(frame, "notch", window, cx);
                assert!(frame.notch_mask);
            })
            .unwrap();
    }

    #[gpui::test]
    fn the_chime_rings_once_for_each_card_that_crosses_its_line(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
        let wants = Rc::new(std::cell::RefCell::new(Vec::new()));
        let rings = Rc::new(std::cell::Cell::new(0));
        let frame = cx.add_window(|window, cx| {
            let system = AnySystem::new(cx.new(|_| Wants(wants.clone())));
            let rings = rings.clone();
            let chime = Rc::new(move || rings.set(rings.get() + 1));
            Frame::new(
                rho_db::RhoDb::in_memory(),
                vec![system],
                Vec::new(),
                chime,
                window,
                cx,
            )
        });
        let hours = jiff::SignedDuration::from_hours;
        let mins = jiff::SignedDuration::from_mins;
        let page = Address::new("page", "7");
        let settle =
            |cx: &mut gpui::TestAppContext,
             f: &dyn Fn(&mut Frame, &mut Window, &mut Context<Frame>)| {
                frame
                    .update(cx, |frame, window, cx| {
                        f(frame, window, cx);
                        frame.systems_changed(cx);
                    })
                    .unwrap();
                cx.run_until_parked();
            };

        // An ask long above the line, from before the window opened.
        let old = Address::new("thread", "lab/1");
        wants.borrow_mut().push(asks(&old, hours(2), false));
        settle(cx, &|frame, window, cx| {
            let view = AnySurface::new(cx.new(|cx| Page(cx.focus_handle())));
            frame.surfaces.insert(page.clone(), view);
            frame.open(page.clone(), window, cx);
        });
        assert_eq!(rings.get(), 0, "what waited at the start does not ring");

        // The agent the user just wrote to finishes: it crosses at once,
        // and the old ask above the line does not hold it back.
        let replied = Address::new("thread", "lab/2");
        wants.borrow_mut().push(asks(&replied, mins(0), true));
        settle(cx, &|_, _, _| {});
        assert_eq!(rings.get(), 1);
        settle(cx, &|_, _, _| {});
        assert_eq!(rings.get(), 1, "one crossing, one ring");

        // On Desk nothing rings, and leaving it does not ring for what
        // crossed there.
        let on_desk = Address::new("thread", "lab/3");
        settle(cx, &|frame, window, cx| frame.open_desk(window, cx));
        wants.borrow_mut().push(asks(&on_desk, mins(0), true));
        settle(cx, &|_, _, _| {});
        settle(cx, &|frame, window, cx| {
            frame.open(page.clone(), window, cx)
        });
        assert_eq!(rings.get(), 1, "Desk shows the hand; it does not ring");

        // The card in view crosses without a ring; another can still ring.
        let in_view = Address::new("thread", "lab/4");
        wants.borrow_mut().push(asks(&in_view, mins(20), false));
        settle(cx, &|frame, window, cx| {
            frame.open(in_view.clone(), window, cx)
        });
        wants.borrow_mut().retain(|claim| claim.address != in_view);
        wants.borrow_mut().push(asks(&in_view, mins(30), false));
        settle(cx, &|_, _, _| {});
        assert_eq!(rings.get(), 1, "the card in view does not ring");
        let elsewhere = Address::new("thread", "lab/5");
        wants.borrow_mut().push(asks(&elsewhere, mins(0), true));
        settle(cx, &|_, _, _| {});
        assert_eq!(rings.get(), 2);
    }

    #[gpui::test]
    fn a_persisted_mute_can_be_lifted_without_undoing_older_verdicts(
        cx: &mut gpui::TestAppContext,
    ) {
        let thread = Address::new("thread", "lab/2");
        let other = Address::new("thread", "lab/12");
        let db = rho_db::RhoDb::in_memory();
        let (mut attention, _) = Attention::open(db.clone());
        attention.say(vec![
            crate::attention::Write::Fact(rho_dealer::facts::Fact::Mute {
                node: thread.clone(),
            }),
            crate::attention::Write::Fact(rho_dealer::facts::Fact::Mute {
                node: other.clone(),
            }),
        ]);
        drop(attention);

        let frame = test_frame(cx, db);
        frame
            .update(cx, |frame, window, cx| {
                let card = cx.new(|cx| OpenCard(cx.focus_handle()));
                frame.surfaces.insert(thread.clone(), AnySurface::new(card));
                frame.recent.show(thread.clone(), ScreenEntry::Opened);
                assert!(!frame.attention.can_undo(), "the window just restarted");
                assert!(frame.marks().get(&thread).facts().muted());
                let unmute = frame
                    .commands(window, cx)
                    .into_iter()
                    .find(|command| command.name() == "unmute")
                    .expect("unmute is a command");
                assert_eq!(
                    unmute.target_text(),
                    Some("thread:lab/2"),
                    "the muted thread"
                );
                frame.run_command(&unmute, None, window, cx);
                assert!(!frame.marks().get(&thread).facts().muted());
                assert!(
                    frame.marks().get(&other).facts().muted(),
                    "leave other verdicts alone"
                );
                assert_eq!(
                    frame.current_address(),
                    &thread,
                    "stay in the reopened thread"
                );
            })
            .unwrap();
    }
    fn prompt_of(frame: &Frame) -> Option<String> {
        frame
            .minibuffer
            .as_ref()
            .map(|prompt| prompt.prompt().to_owned())
    }

    fn answer(frame: &mut Frame, text: &str, window: &mut Window, cx: &mut Context<Frame>) {
        frame
            .minibuffer_mut()
            .expect("a prompt is open")
            .set_input(text.to_owned(), window, cx);
        // As typing does: the answers offered follow the line.
        frame.refresh_minibuffer(cx);
        frame.minibuffer_confirm(window, cx);
    }

    /// A system whose cards are answered in its own words, as a push the
    /// user approves or denies: the test sets what waits, and each answer
    /// takes its card away, as the host's would.
    struct Answers(Rc<std::cell::RefCell<Vec<rho_dealer::Claim>>>);

    impl crate::System for Answers {
        fn kinds(&self) -> &'static [&'static str] {
            &["git"]
        }

        fn open(&mut self, _: &Address, _: &mut Window, cx: &mut Context<Self>) -> Opening {
            Opening::Surface(AnySurface::new(cx.new(|cx| OpenCard(cx.focus_handle()))))
        }

        fn claims(
            &self,
            _: &rho_dealer::Marks,
            _: Timestamp,
            _: Option<&mut rho_dealer::rank::Trace>,
            _: &App,
        ) -> Vec<rho_dealer::Claim> {
            self.0.borrow().clone()
        }

        fn verdicts(&mut self, address: &Address, _: &mut Context<Self>) -> Option<Vec<Row>> {
            let waiting = self.0.clone();
            let address = address.clone();
            let answer = |word: &'static str| {
                let waiting = waiting.clone();
                let address = address.clone();
                move |frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>| {
                    waiting
                        .borrow_mut()
                        .retain(|claim| claim.address != address);
                    frame.answered(&address, format!("{word}: push"), window, cx);
                }
            };
            Some(vec![
                Row::run("a", "approve", answer("approved")),
                Row::run("x", "deny", answer("denied")),
            ])
        }
    }

    fn answers_frame(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::WindowHandle<Frame>,
        Rc<std::cell::RefCell<Vec<rho_dealer::Claim>>>,
    ) {
        let waiting = Rc::new(std::cell::RefCell::new(Vec::new()));
        let system = waiting.clone();
        let frame = test_frame_with(cx, rho_db::RhoDb::in_memory(), move |cx| {
            vec![AnySystem::new(cx.new(|_| Answers(system)))]
        });
        (frame, waiting)
    }

    /// A push that waits is answered with approve, never done or mute;
    /// answering it puts it away and deals the next push, and the last one
    /// leaves the deal empty.
    #[gpui::test]
    fn a_card_its_system_answers_is_answered_in_its_words(cx: &mut gpui::TestAppContext) {
        let (frame, waiting) = answers_frame(cx);
        let first = Address::new("git", "1");
        let second = Address::new("git", "2");
        waiting.borrow_mut().extend([
            asks(&first, jiff::SignedDuration::from_mins(5), false),
            asks(&second, jiff::SignedDuration::from_mins(1), false),
        ]);
        let approve = |frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>| {
            assert_eq!(
                space(frame, window, cx),
                ["places", "approve", "deny", "snooze"],
                "its own words, and a snooze, in place of done and mute"
            );
            // `space d` decides nothing on a push, and says why.
            frame.leader(window, cx);
            frame.prefix_keystroke(&gpui::Keystroke::parse("d").unwrap(), window, cx);
            assert_eq!(
                echo_of(frame).as_deref(),
                Some("a push waits · space a approves, space x denies")
            );
            assert!(frame.prefix.is_none(), "the refusal spends the prefix");
            frame.leader(window, cx);
            frame.prefix_keystroke(&gpui::Keystroke::parse("a").unwrap(), window, cx);
        };
        frame
            .update(cx, |frame, window, cx| {
                frame.pull_card(window, cx);
                assert_eq!(frame.current_address(), &first, "the older push first");
                approve(frame, window, cx);
                assert_eq!(frame.current_address(), &second, "the next push is dealt");
                approve(frame, window, cx);
                assert!(frame.desk_in_view(), "nothing waits");
                assert!(
                    frame.marks().get(&first).facts.is_empty(),
                    "an answer is the host's, not a verdict in the ledger"
                );
            })
            .unwrap();
    }

    /// A push that comes while the user types in a prompt lights the lamp
    /// and takes nothing: the prompt, its words and the screen under it
    /// stay as they were.
    #[gpui::test]
    fn a_push_that_comes_while_the_user_types_only_lights_the_lamp(cx: &mut gpui::TestAppContext) {
        let (frame, waiting) = answers_frame(cx);
        let page = Address::new("page", "7");
        frame
            .update(cx, |frame, window, cx| {
                let view = AnySurface::new(cx.new(|cx| Page(cx.focus_handle())));
                frame.surfaces.insert(page.clone(), view);
                frame.open(page.clone(), window, cx);
                frame.open_prompt(
                    "attach host:",
                    Rc::new(|_, _, _| Vec::new()),
                    Rc::new(|_, _, _, _| {}),
                    window,
                    cx,
                );
                frame
                    .minibuffer_mut()
                    .unwrap()
                    .set_input("lab=iroh:".to_owned(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        let push = Address::new("git", "1");
        waiting
            .borrow_mut()
            .push(asks(&push, jiff::SignedDuration::from_mins(0), false));
        frame
            .update(cx, |frame, _, cx| frame.systems_changed(cx))
            .unwrap();
        cx.run_until_parked();
        frame
            .update(cx, |frame, _, cx| {
                assert_eq!(frame.current_address(), &page);
                let prompt = frame.minibuffer.as_ref().expect("the prompt stays open");
                assert_eq!(prompt.prompt(), "attach host:");
                assert_eq!(prompt.input(cx), "lab=iroh:");
                assert!(frame.signals.lamp_on, "the lamp is the only pull");
                assert_eq!(frame.hand().cards[0].address, push);
            })
            .unwrap();
    }

    /// On the phone, `done` on a push opens `Commands` at its answers
    /// rather than settling it.
    #[gpui::test]
    fn done_on_the_phone_asks_for_the_cards_own_words(cx: &mut gpui::TestAppContext) {
        let (frame, waiting) = answers_frame(cx);
        let push = Address::new("git", "1");
        waiting
            .borrow_mut()
            .push(asks(&push, jiff::SignedDuration::from_mins(3), false));
        frame
            .update(cx, |frame, window, cx| {
                frame.pull_card(window, cx);
                assert_eq!(frame.current_address(), &push);
                frame.press_phone_button(crate::phone::Button::Done, window, cx);
                frame.refresh_minibuffer(cx);
                let prompt = frame.minibuffer.as_ref().expect("the answers are offered");
                assert_eq!(prompt.input(cx), "push");
                assert_eq!(prompt.row_names()[..2], ["approve push", "deny push"]);
                assert!(frame.marks().get(&push).facts.is_empty(), "nothing settled");
            })
            .unwrap();
    }

    /// Switching a screen in place three times adds no place: the screen
    /// keeps its rank, and what is written down for the next start names
    /// the screen as it is now. One out of view is renamed where it stands.
    #[gpui::test]
    fn switching_a_screen_in_place_adds_no_place(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let thread = Address::new("thread", "lab/2");
                let charts = [
                    "rate-limit/7d",
                    "rate-limit/30d",
                    "model-cost/30d",
                    "model-cost/7d",
                ]
                .map(|chart| Address::new("usage", chart));
                for address in std::iter::once(&thread).chain(&charts) {
                    let view = cx.new(|cx| OpenCard(cx.focus_handle()));
                    frame
                        .surfaces
                        .insert(address.clone(), AnySurface::new(view));
                }
                frame.open(thread.clone(), window, cx);
                frame.open(charts[0].clone(), window, cx);
                for pair in charts.windows(2) {
                    frame.replace(&pair[0], pair[1].clone(), window, cx);
                }
                assert_eq!(
                    places_in_order(frame),
                    [charts[3].clone(), thread.clone(), Frame::desk_address()]
                );
                for gone in &charts[..3] {
                    assert!(frame.surface(gone).is_none(), "{gone} went");
                }
                let saved = place::Saved::of(&frame.recent);
                assert_eq!(
                    saved.recent(&Frame::desk_address()).current_key(),
                    &charts[3]
                );
                // A sent idea becomes its thread where it stands, out of view.
                let idea = Address::new("idea", "9");
                let posted = Address::new("thread", "lab/9");
                frame.remember(idea.clone());
                assert_eq!(places_in_order(frame)[1], idea);
                frame.replace(&idea, posted.clone(), window, cx);
                assert_eq!(
                    places_in_order(frame),
                    [
                        charts[3].clone(),
                        posted,
                        thread.clone(),
                        Frame::desk_address()
                    ]
                );
            })
            .unwrap();
    }

    // ---- commands that act on a host ----

    use rho_agent_hosts::protocol::{
        GccpApprove, GccpContact, GccpHost, GccpList, GccpRemove, GccpState, IrohDevice,
        IrohDevices, IrohRevoke, Open as HostOpen, Request as HostRequest,
    };
    use rho_rpc::protocol::{Answer as HostAnswer, read_frame, write_frame};

    const HARNESS: &str = "9kpjn6yxeu4jfy5o3xjnsq3qfqe9zd5ab6dn7ommq3tjsbzzkzby";
    const ADDRESS: &str = "9kpjn6yxeu4jfy5o3xjnsq3qfqe9zd5ab6dn7ommq3tjsbzzkzby/4";

    /// What a host says of GCCP: one approved harness, and one address on
    /// it the test system's agent accepted.
    fn gccp_state() -> GccpState {
        GccpState {
            endpoint: Some("this-host-endpoint".to_owned()),
            hosts: vec![GccpHost {
                endpoint: HARNESS.to_owned(),
                name: "fable lab".to_owned(),
            }],
            contacts: vec![GccpContact {
                agent: HostBacked::asker().encoded(),
                address: ADDRESS.to_owned(),
                name: "reviewer".to_owned(),
            }],
        }
    }

    /// What a host in this process was asked, in order.
    type Asked = Rc<std::cell::RefCell<Vec<HostRequest>>>;

    /// Attaches a host called `name` and puts it in this process: it
    /// answers `GccpList` with `listed` (or refuses with it), and does
    /// whatever else it is asked.
    fn attach_in_process(
        frame: &gpui::WindowHandle<Frame>,
        name: &str,
        listed: Result<GccpState, &'static str>,
        cx: &mut gpui::TestAppContext,
    ) -> Asked {
        let streams = frame
            .update(cx, |frame, _, cx| {
                let spec = HostSpec {
                    name: name.to_owned(),
                    target: rho_agent_hosts::AttachTarget::Local {
                        runtime_dir: format!("/nonexistent/{name}").into(),
                    },
                };
                frame.attach_host_in_process(spec, cx).1
            })
            .unwrap();
        let asked = Asked::default();
        let record = asked.clone();
        cx.foreground_executor()
            .spawn(async move {
                let mut streams = streams;
                while let Some(mut stream) = streams.recv().await {
                    let open: rho_rpc::protocol::Open = read_frame(&mut stream).await.unwrap();
                    let HostOpen::Request(request) = open.unpack::<HostOpen>().unwrap() else {
                        panic!("only calls here");
                    };
                    record.borrow_mut().push(request.clone());
                    match request {
                        HostRequest::GccpList(GccpList) => {
                            let answer = listed.clone().map_err(|reason| anyhow::anyhow!(reason));
                            write_frame(&mut stream, &HostAnswer::from(answer)).await
                        }
                        HostRequest::GccpApprove(_) | HostRequest::GccpRemove(_) => {
                            write_frame(&mut stream, &HostAnswer::from(Ok(()))).await
                        }
                        HostRequest::IrohDevices(IrohDevices) => {
                            write_frame(&mut stream, &HostAnswer::from(Ok(trusted_devices()))).await
                        }
                        HostRequest::IrohRevoke(IrohRevoke { endpoint_id }) => {
                            write_frame(&mut stream, &HostAnswer::from(Ok(endpoint_id))).await
                        }
                        other => panic!("not asked of a host here: {other:?}"),
                    }
                    .unwrap();
                }
            })
            .detach();
        asked
    }

    fn gccp_frame(cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<Frame> {
        cx.update(gpui_tokio::init);
        host_backed_frame(cx).0
    }

    /// Types `line` into `Commands` and presses enter, as the user does.
    fn run_line(frame: &mut Frame, line: &str, window: &mut Window, cx: &mut Context<Frame>) {
        frame.commands_prompt(window, cx);
        answer(frame, line, window, cx);
        assert!(
            frame
                .minibuffer
                .as_ref()
                .is_none_or(|prompt| prompt.prompt() != ":"),
            "{line:?} names no command"
        );
    }

    fn echo_of(frame: &Frame) -> Option<String> {
        frame.echo.as_ref().map(|echo| echo.text().to_owned())
    }

    fn candidates_of(frame: &Frame) -> Vec<(String, String)> {
        frame
            .minibuffer
            .as_ref()
            .expect("a prompt is open")
            .candidates()
            .iter()
            .map(|row| (row.value.clone(), row.description.clone()))
            .collect()
    }

    const GCCP: [&str; 3] = [
        "list gccp agents",
        "approve gccp agent",
        "remove gccp agent",
    ];

    /// The GCCP commands are the window's own: on the desk, on a page that
    /// says nothing of hosts, and with no host attached at all.
    #[gpui::test]
    fn gccp_commands_are_on_every_screen(cx: &mut gpui::TestAppContext) {
        let frame = gccp_frame(cx);
        frame
            .update(cx, |frame, window, cx| {
                let page = Address::new("page", "7");
                let view = cx.new(|cx| Page(cx.focus_handle()));
                frame.surfaces.insert(page.clone(), AnySurface::new(view));
                for screen in [None, Some(page)] {
                    if let Some(screen) = screen {
                        frame.open(screen, window, cx);
                    }
                    let commands = frame.commands(window, cx);
                    for label in GCCP {
                        assert!(
                            commands.iter().any(|command| command.name() == label),
                            "{label} on {}",
                            frame.current_address()
                        );
                    }
                }
                run_line(frame, GCCP[0], window, cx);
                assert_eq!(prompt_of(frame), None, "nothing to ask about");
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("list gccp agents: no agent host is attached")
                );
            })
            .unwrap();
    }

    /// With one host attached, `gccp list` asks it without asking which,
    /// and lists its endpoint, its harnesses and what its agents accepted,
    /// each agent by the name the window calls it; enter copies an id.
    #[gpui::test]
    fn one_host_is_asked_without_a_host_prompt(cx: &mut gpui::TestAppContext) {
        let frame = gccp_frame(cx);
        let asked = attach_in_process(&frame, "lab", Ok(gccp_state()), cx);
        frame
            .update(cx, |frame, window, cx| run_line(frame, GCCP[0], window, cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(*asked.borrow(), [HostRequest::GccpList(GccpList)]);
        frame
            .update(cx, |frame, window, cx| {
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("gccp on lab · enter copies:")
                );
                assert_eq!(
                    candidates_of(frame),
                    [
                        (
                            "this-host-endpoint".to_owned(),
                            "lab · give it to another harness's owner to approve".to_owned()
                        ),
                        (
                            HARNESS.to_owned(),
                            "approved harness \"fable lab\"".to_owned()
                        ),
                        (
                            ADDRESS.to_owned(),
                            "eng-b8os accepted it as \"reviewer\"".to_owned()
                        ),
                    ]
                );
                answer(frame, ADDRESS, window, cx);
                assert_eq!(echo_of(frame), Some(format!("copied: {ADDRESS}")));
            })
            .unwrap();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(ADDRESS.to_owned())
        );
        assert_eq!(asked.borrow().len(), 1, "listing changes nothing");
    }

    /// With two hosts attached, the command asks `host:` first, and only
    /// the chosen host is asked and told.
    #[gpui::test]
    fn two_hosts_ask_which_and_remove_goes_to_the_chosen_one(cx: &mut gpui::TestAppContext) {
        let frame = gccp_frame(cx);
        let lab = attach_in_process(&frame, "lab", Ok(gccp_state()), cx);
        let sea = attach_in_process(&frame, "sea", Ok(gccp_state()), cx);
        frame
            .update(cx, |frame, window, cx| {
                run_line(frame, GCCP[2], window, cx);
                assert_eq!(prompt_of(frame).as_deref(), Some("host:"));
                let hosts: Vec<_> = candidates_of(frame)
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect();
                assert_eq!(hosts, ["lab", "sea"]);
                answer(frame, "sea", window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(lab.borrow().is_empty(), "the other host is not asked");
        let row = format!("eng-b8os {ADDRESS}");
        frame
            .update(cx, |frame, window, cx| {
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("remove gccp address on sea:")
                );
                assert_eq!(
                    candidates_of(frame),
                    [(row.clone(), "\"reviewer\"".to_owned())]
                );
                answer(frame, &row, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(lab.borrow().is_empty());
        assert_eq!(
            *sea.borrow(),
            [
                HostRequest::GccpList(GccpList),
                HostRequest::GccpRemove(GccpRemove {
                    agent: HostBacked::asker().encoded(),
                    address: ADDRESS.to_owned(),
                }),
            ],
            "the host is told the agent by its id, not the window's name for it"
        );
        frame
            .update(cx, |frame, _, _| {
                assert_eq!(
                    echo_of(frame),
                    Some(format!("sea: eng-b8os no longer accepts {ADDRESS}"))
                );
            })
            .unwrap();
    }

    /// `gccp approve` takes the endpoint and the rest of the line as the
    /// harness's name; a line without a name tells the host nothing.
    #[gpui::test]
    fn approve_sends_the_endpoint_and_the_whole_name(cx: &mut gpui::TestAppContext) {
        let frame = gccp_frame(cx);
        let asked = attach_in_process(&frame, "lab", Ok(gccp_state()), cx);
        frame
            .update(cx, |frame, window, cx| {
                run_line(frame, GCCP[1], window, cx);
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("approve a gccp harness on lab · <endpoint id> <name>:")
                );
                answer(frame, HARNESS, window, cx);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("approve gccp agent: give the harness's endpoint id, then a name for it")
                );
                run_line(frame, GCCP[1], window, cx);
                answer(frame, &format!("  {HARNESS}   fable lab  "), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            *asked.borrow(),
            [HostRequest::GccpApprove(GccpApprove {
                endpoint: HARNESS.to_owned(),
                name: "fable lab".to_owned(),
            })]
        );
        frame
            .update(cx, |frame, _, _| {
                assert_eq!(
                    echo_of(frame),
                    Some(format!(
                        "lab: approved gccp harness {HARNESS} as \"fable lab\""
                    ))
                );
            })
            .unwrap();
    }

    /// A host that refuses is named on the echo line with its reason, and
    /// no prompt opens.
    #[gpui::test]
    fn a_refusal_opens_nothing(cx: &mut gpui::TestAppContext) {
        let frame = gccp_frame(cx);
        attach_in_process(&frame, "lab", Err("gccp needs an iroh endpoint"), cx);
        frame
            .update(cx, |frame, window, cx| run_line(frame, GCCP[2], window, cx))
            .unwrap();
        cx.run_until_parked();
        frame
            .update(cx, |frame, _, _| {
                assert_eq!(prompt_of(frame), None);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("remove gccp agent: lab: gccp needs an iroh endpoint")
                );
            })
            .unwrap();
    }

    /// An answer that comes while the user has another prompt open does
    /// not take it from them.
    #[gpui::test]
    fn a_late_answer_leaves_the_users_prompt_alone(cx: &mut gpui::TestAppContext) {
        let frame = gccp_frame(cx);
        attach_in_process(&frame, "lab", Ok(gccp_state()), cx);
        frame
            .update(cx, |frame, window, cx| {
                run_line(frame, GCCP[0], window, cx);
                frame.open_prompt(
                    "attach host:",
                    Rc::new(|_, _, _| Vec::new()),
                    Rc::new(|_, _, _, _| {}),
                    window,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();
        frame
            .update(cx, |frame, _, _| {
                assert_eq!(prompt_of(frame).as_deref(), Some("attach host:"));
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("list gccp agents: lab answered while another prompt was open · run it again")
                );
            })
            .unwrap();
    }

    // ---- this device: pairing, and the devices a host trusts ----

    const PAIR_QUESTION: &str = "secret phrase · enter alone makes a new one:";

    /// A screen where an idea is being written.
    struct Composer(gpui::FocusHandle);

    impl gpui::Focusable for Composer {
        fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
            self.0.clone()
        }
    }

    impl gpui::Render for Composer {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl crate::Surface for Composer {
        fn title(&self, _: &App) -> String {
            "new idea".into()
        }

        fn composes(&self, _: &App) -> bool {
            true
        }
    }

    /// The window asks a new device once, when it starts: escape keeps the
    /// device to itself, and neither a move nor the next start asks again.
    #[gpui::test]
    fn a_new_device_is_asked_once(cx: &mut gpui::TestAppContext) {
        let db = rho_db::RhoDb::in_memory();
        let frame = test_frame(cx, db.clone());
        frame
            .update(cx, |frame, window, cx| {
                assert_eq!(frame.attention.pairing(), Pairing::Unasked);
                assert_eq!(prompt_of(frame), None, "only the window's start asks");
                frame.ask_pairing(window, cx);
                assert_eq!(prompt_of(frame).as_deref(), Some(PAIR_QUESTION));
                frame.minibuffer_cancel(window, cx);
                assert_eq!(prompt_of(frame), None);
                assert_eq!(frame.attention.pairing(), Pairing::Unpaired);
                let page = Address::new("page", "7");
                let view = cx.new(|cx| Page(cx.focus_handle()));
                frame.surfaces.insert(page.clone(), AnySurface::new(view));
                frame.open(page, window, cx);
                assert_eq!(prompt_of(frame), None, "a move does not ask again");
            })
            .unwrap();
        let restarted = test_frame(cx, db);
        restarted
            .update(cx, |frame, window, cx| {
                frame.ask_pairing(window, cx);
                assert_eq!(prompt_of(frame), None, "nor does the next start");
                assert_eq!(frame.attention.pairing(), Pairing::Unpaired);
                // `pair device` still asks when the user wants it.
                run_line(frame, "pair device", window, cx);
                assert_eq!(prompt_of(frame).as_deref(), Some(PAIR_QUESTION));
            })
            .unwrap();
    }

    /// The question never takes keys from an idea being written or from
    /// another prompt: it waits until they are done.
    #[gpui::test]
    fn the_question_waits_for_an_idea_and_a_prompt(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                let idea = Address::new("idea", "1");
                let view = cx.new(|cx| Composer(cx.focus_handle()));
                frame.surfaces.insert(idea.clone(), AnySurface::new(view));
                frame.open(idea.clone(), window, cx);
                frame.ask_pairing(window, cx);
                assert_eq!(prompt_of(frame), None, "the idea keeps its keys");
                frame.open_prompt(
                    "name this host:",
                    Rc::new(|_, _, _| Vec::new()),
                    Rc::new(|_, _, _, _| {}),
                    window,
                    cx,
                );
                answer(frame, "lab", window, cx);
                assert_eq!(prompt_of(frame), None, "the idea is still being written");
                frame.close(&idea, window, cx);
                assert_eq!(prompt_of(frame).as_deref(), Some(PAIR_QUESTION));
            })
            .unwrap();
        // A prompt open when the window starts keeps its keys too.
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        frame
            .update(cx, |frame, window, cx| {
                frame.open_prompt(
                    "name this host:",
                    Rc::new(|_, _, _| Vec::new()),
                    Rc::new(|_, _, _, _| {}),
                    window,
                    cx,
                );
                frame.ask_pairing(window, cx);
                assert_eq!(prompt_of(frame).as_deref(), Some("name this host:"));
                answer(frame, "lab", window, cx);
                assert_eq!(prompt_of(frame).as_deref(), Some(PAIR_QUESTION));
                assert_eq!(frame.attention.pairing(), Pairing::Unasked);
            })
            .unwrap();
    }

    /// Enter alone makes a new user's phrase, shows it to write down, and
    /// seals with it what the device said before: a device that takes the
    /// phrase reads the verdict and the note from the sealed bytes.
    #[gpui::test]
    fn a_new_user_seals_what_was_said_before(cx: &mut gpui::TestAppContext) {
        use rho_dealer::facts::{Entry, Fact};

        use crate::attention::Write;

        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        let card = Address::new("thread", "lab/1");
        let note = uuid::Uuid::from_u128(7);
        let (secret, sealed) = frame
            .update(cx, |frame, window, cx| {
                frame.attention.say(vec![
                    Write::Fact(Fact::Mute { node: card.clone() }),
                    Write::Note {
                        note,
                        body: Some("buy oat milk".to_owned()),
                        deleted: None,
                    },
                ]);
                let ledger = frame.attention.streams().ledger().clone();
                assert!(ledger.lengths().is_empty(), "nothing leaves it unpaired");
                frame.ask_pairing(window, cx);
                answer(frame, "", window, cx);
                let secret = frame.attention.secret().expect("a phrase is made");
                assert_eq!(frame.attention.pairing(), Pairing::Paired);
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("write down your secret phrase · it pairs your other devices · enter:")
                );
                assert_eq!(
                    candidates_of(frame),
                    [(secret.to_words(), "your secret phrase".to_owned())]
                );
                let sealed: Vec<_> = ledger
                    .lengths()
                    .into_keys()
                    .map(|log| (log, ledger.bytes_after(log, 0, usize::MAX)))
                    .collect();
                (secret, sealed)
            })
            .unwrap();
        assert!(!sealed.is_empty(), "a host has something to carry");
        assert!(
            sealed
                .iter()
                .all(|(_, bytes)| !bytes.windows(8).any(|w| w == b"oat milk")),
            "sealed, not plain"
        );
        let other = futures::executor::block_on(async {
            let other = rho_ledger::Ledger::open(rho_db::RhoDb::in_memory()).await;
            other.set_secret(secret).await.unwrap();
            for (log, bytes) in sealed {
                other.receive(log, 0, bytes).await;
            }
            other
        });
        let mut marks = rho_dealer::Marks::default();
        marks.apply(
            other
                .items(rho_ledger::Channel::Facts)
                .iter()
                .filter_map(|item| Entry::decode(&item.bytes))
                .collect::<Vec<_>>(),
        );
        assert!(marks.get(&card).facts().muted(), "the verdict");
        assert_eq!(
            marks.get(&Address::note(note)).body.as_deref(),
            Some("buy oat milk"),
            "the note"
        );
    }

    /// A phrase with a word that is no phrase word, or too few words, asks
    /// again with the reason and keeps what was typed; neither answers the
    /// question, so only escape keeps the device to itself.
    #[gpui::test]
    fn a_wrong_phrase_asks_again_with_the_reason(cx: &mut gpui::TestAppContext) {
        let frame = test_frame(cx, rho_db::RhoDb::in_memory());
        let typed = |frame: &Frame, cx: &App| frame.minibuffer.as_ref().unwrap().input(cx);
        frame
            .update(cx, |frame, window, cx| {
                frame.ask_pairing(window, cx);
                let words =
                    "abandon ability able about above absent absorb abstract absurd abuse access";
                let wrong = format!("{words} quokkaz");
                answer(frame, &wrong, window, cx);
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("secret phrase (`quokkaz` is not a phrase word):")
                );
                assert_eq!(typed(frame, cx), wrong);
                answer(frame, words, window, cx);
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("secret phrase (12 words, not 11):")
                );
                assert_eq!(typed(frame, cx), words);
                assert_eq!(frame.attention.pairing(), Pairing::Unasked);
                assert!(frame.attention.secret().is_none());
                frame.minibuffer_cancel(window, cx);
                assert_eq!(frame.attention.pairing(), Pairing::Unpaired);
            })
            .unwrap();
    }

    const THIS_DEVICE: &str = "u3pdkbb7qgxxsfwnj4c9euyyk8ynwkha6yikonub8qcxxfthrqwy";
    const LOST: &str = "9kpjn6yxeu4jfy5o3xjnsq3qfqe9zd5ab6dn7ommq3tjsbzzkzby";

    /// What a host trusts: this device, which asks, and a lost one.
    fn trusted_devices() -> Vec<IrohDevice> {
        vec![
            IrohDevice {
                endpoint_id: THIS_DEVICE.to_owned(),
                trusted_at_unix_secs: 1_780_000_000,
                asking: true,
            },
            IrohDevice {
                endpoint_id: LOST.to_owned(),
                trusted_at_unix_secs: 1_790_000_000,
                asking: false,
            },
        ]
    }

    /// `list devices` lists each key the host trusts, this device's marked.
    #[gpui::test]
    fn list_devices_marks_this_one(cx: &mut gpui::TestAppContext) {
        let frame = gccp_frame(cx);
        let asked = attach_in_process(&frame, "lab", Ok(gccp_state()), cx);
        frame
            .update(cx, |frame, window, cx| {
                run_line(frame, "list devices", window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(*asked.borrow(), [HostRequest::IrohDevices(IrohDevices)]);
        frame
            .update(cx, |frame, _, _| {
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("devices lab trusts · enter copies:")
                );
                assert_eq!(
                    candidates_of(frame),
                    [
                        (
                            THIS_DEVICE.to_owned(),
                            "this device · trusted since 2026-05-28".to_owned()
                        ),
                        (LOST.to_owned(), "trusted since 2026-09-21".to_owned()),
                    ]
                );
            })
            .unwrap();
    }

    /// `revoke device` asks which host, offers only the other devices,
    /// refuses this one, and tells only the chosen host to revoke the
    /// lost one.
    #[gpui::test]
    fn revoke_device_takes_back_a_lost_one_and_never_this_one(cx: &mut gpui::TestAppContext) {
        let frame = gccp_frame(cx);
        let lab = attach_in_process(&frame, "lab", Ok(gccp_state()), cx);
        let sea = attach_in_process(&frame, "sea", Ok(gccp_state()), cx);
        let choose_sea = |frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>| {
            run_line(frame, "revoke device", window, cx);
            assert_eq!(prompt_of(frame).as_deref(), Some("host:"));
            answer(frame, "sea", window, cx);
        };
        frame
            .update(cx, |frame, window, cx| choose_sea(frame, window, cx))
            .unwrap();
        cx.run_until_parked();
        frame
            .update(cx, |frame, window, cx| {
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("revoke a device sea trusts:")
                );
                assert_eq!(
                    candidates_of(frame),
                    [(LOST.to_owned(), "trusted since 2026-09-21".to_owned())]
                );
                answer(frame, THIS_DEVICE, window, cx);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("revoke device: that is this device · revoke it from another")
                );
                choose_sea(frame, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        frame
            .update(cx, |frame, window, cx| answer(frame, LOST, window, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(lab.borrow().is_empty(), "the other host is not asked");
        assert_eq!(
            *sea.borrow(),
            [
                HostRequest::IrohDevices(IrohDevices),
                HostRequest::IrohDevices(IrohDevices),
                HostRequest::IrohRevoke(IrohRevoke {
                    endpoint_id: LOST.to_owned(),
                }),
            ],
            "this device is never revoked"
        );
        frame
            .update(cx, |frame, _, _| {
                assert_eq!(
                    echo_of(frame),
                    Some(format!("revoked: sea no longer trusts {LOST}"))
                );
            })
            .unwrap();
    }
}
