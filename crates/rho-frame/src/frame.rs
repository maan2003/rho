//! The frame entity: the window's root view.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::Editor;
use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Pixels, Subscription, Task, Window,
    div, px,
};
use jiff::Timestamp;
use rho_agent_hosts::hosts::{HostStatus, Hosts};
use rho_agent_hosts::{HostEvent, HostId, HostSpec};
use rho_dealer::facts::Seen;
use rho_dealer::{Address, Card};
use rho_ledger::Pairing;
use rho_window::history::History;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::attention::{Attention, Verdict};
use crate::deal::Signals;
use crate::desk::DeskView;
use crate::menu::{OpenMenu, Pick, Row, Rows, Shortcut};
use crate::minibuffer::{
    Answer, CancelHandler, Candidate, CandidateSource, Echo, Minibuffer, SubmitHandler,
    bottom_strip,
};
use crate::phone::{Layout, Phone};
use crate::search::Search;
use crate::surface::{AnySurface, CardHere, Mode, Subject, Surface as _};
use crate::system::{AnySystem, Found, Opening};
use crate::waiting::Waiting;
use crate::when::SnoozeUnit;
use crate::{keys, palette, place};

/// The kind of the frame's own surface: Desk.
pub const FRAME_KIND: &str = "frame";
pub(crate) const FRAME_PADDING: Pixels = px(2.);

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

/// How a screen in history was reached. A dealt screen and one opened by
/// name are the same: the screen shows its card live, so what the reader
/// sees of it is the hand's. Only a step down into the work keeps the card
/// as it stood, since the card's own screen is out of sight.
#[derive(Default)]
pub(crate) enum ScreenEntry {
    #[default]
    Opened,
    Inspection(CardInspection),
}

enum Open<'a> {
    Explicit,
    Deal(&'a Card),
    Inspect(CardInspection),
}

/// How long an echo stays on the line.
const ECHO_FOR: Duration = Duration::from_secs(4);

pub struct Frame {
    pub(crate) db: rho_db::RhoDb,
    pub(crate) hosts: Hosts,
    pub(crate) systems: Vec<AnySystem>,
    pub(crate) attention: Attention,
    /// The surfaces the reader has been on, by address. The newest entry
    /// is where down deals; Desk is the oldest and never closes.
    history: History<Address, ScreenEntry>,
    surfaces: HashMap<Address, AnySurface>,
    pub(crate) desk: Entity<DeskView>,
    pub(crate) minibuffer: Option<Minibuffer>,
    pub(crate) menu: Option<OpenMenu>,
    pub(crate) menu_focus: FocusHandle,
    /// Where the keyboard was before a menu or a prompt took it.
    overlay_return: Option<FocusHandle>,
    echo: Option<Echo>,
    /// The text style for a surface that is no buffer.
    chrome_editor: Entity<Editor>,
    mode_indicator: Entity<vim::ModeIndicator>,
    pub(crate) signals: Signals,
    pub(crate) chime: Rc<dyn Fn()>,
    /// The last `/` or `?`, which `n` and `N` repeat in any surface.
    pub(crate) search: Option<Search>,
    /// A flick or a snap in progress, while the window is a phone's.
    pub(crate) phone: Phone,
    /// Writes down where the user is after each move, for the next start.
    pub(crate) place: place::Writer,
    last_input: Option<Timestamp>,
    focus_lost: Option<Instant>,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

// The palette and the verdict menu answer the same card with the same words.
fn verdict_rows(target: Address) -> [Row; 2] {
    let done = target.clone();
    [
        Row::run("d", "done", move |frame, window, cx| {
            frame.verdict(&done, Verdict::Done, "done".to_owned(), window, cx);
        }),
        Row::run("m", "mute", move |frame, window, cx| {
            frame.verdict(&target, Verdict::Mute, "mute".to_owned(), window, cx);
        }),
    ]
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
        let desk_editor = desk.read(cx).editor(cx).expect("Desk is an editor");
        let desk_selection = cx.subscribe(&desk_editor, |frame, _, event, cx| {
            if frame.desk_in_view() {
                if matches!(event, editor::EditorEvent::SelectionsChanged { .. }) {
                    cx.notify();
                }
            }
        });
        let desk_address = Self::desk_address();
        let mut surfaces = HashMap::new();
        surfaces.insert(desk_address.clone(), AnySurface::new(desk.clone()));
        // A menu owns its keys before gpui resolves bindings: vim binds `g`
        // as a prefix, and an ordinary key handler would not see a menu's
        // one-stroke `g` until another key arrived.
        let window_id = window.window_handle().window_id();
        let menu_keys = cx.listener(move |this, event: &gpui::KeystrokeEvent, window, cx| {
            if window.window_handle().window_id() != window_id {
                return;
            }
            this.note_input();
            if this.menu.is_some() && this.menu_focus.is_focused(window) {
                this.menu_keystroke(&event.keystroke, window, cx);
            }
        });
        let menu_keys = cx.intercept_keystrokes(menu_keys);
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
            history: History::new(desk_address, ScreenEntry::Opened),
            surfaces,
            desk,
            minibuffer: None,
            menu: None,
            menu_focus: cx.focus_handle(),
            overlay_return: None,
            echo: None,
            chrome_editor,
            mode_indicator,
            signals: Signals::default(),
            chime,
            search: None,
            phone: Phone::default(),
            place: place::Writer::spawn(db.clone()),
            last_input,
            focus_lost: None,
            _subscriptions: vec![menu_keys, activation, desk_selection],
            _tasks: Vec::new(),
        };
        let ledger = cx.spawn_in(window, async move |this, cx| {
            let mut events = ledger_events;
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                let updated = this.update_in(cx, |this, window, cx| {
                    for event in batch {
                        this.ledger_event(event, window, cx);
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
    /// device, with the way back and forward as it was. The screens behind
    /// and ahead are made when the user reaches them; the one they were on
    /// is made now, and when it no longer opens the window opens on Desk.
    fn return_to(&mut self, saved: place::Saved, window: &mut Window, cx: &mut Context<Self>) {
        self.history = place::return_history(
            &saved,
            &Self::desk_address(),
            self.last_input,
            Timestamp::now(),
        );
        let address = self.current_address().clone();
        if self.surfaces.contains_key(&address) || self.hold(&address, window, cx) {
            return;
        }
        self.history.forget(&address);
        self.desk.update(cx, |desk, cx| desk.focus_first_row(cx));
        self.history.show(Self::desk_address(), ScreenEntry::Opened);
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
    /// one no host holds leaves the history, as a closed screen does. A
    /// prompt or a menu over the screen keeps the keyboard.
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
                    self.history.forget(&address);
                }
            }
        }
        self.settle(false, window, cx);
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

    /// Makes the screen the cursor came to, when the history holds it by
    /// address alone, as one it was given back at start does. One that no
    /// longer opens leaves the history and the walk goes on the way it
    /// went, or the other way from the end; Desk always opens.
    fn settle(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let address = self.current_address().clone();
            if self.surfaces.contains_key(&address) || self.hold(&address, window, cx) {
                return;
            }
            self.history.forget(&address);
            let stepped = match forward {
                true => self.history.forward().is_some() || self.history.back().is_some(),
                false => self.history.back().is_some() || self.history.forward().is_some(),
            };
            if !stepped {
                self.history.show(Self::desk_address(), ScreenEntry::Opened);
            }
        }
    }

    /// Writes down where the user is now, for the next start.
    fn moved(&self) {
        self.place
            .write(place::Write::Place(place::Saved::of(&self.history)));
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
        self.history.current_key()
    }

    pub fn current_surface(&self) -> &AnySurface {
        self.surfaces
            .get(self.history.current_key())
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
            CardHere::Itself => Some(surface.clone()),
            CardHere::Row(row) => row,
            CardHere::Inspected => match self.history.current() {
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

    fn waits(&self, address: &Address) -> bool {
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
    /// reader was in the history below. Anywhere else, the card as the
    /// hand holds it.
    pub(crate) fn card_seen(&self, address: &Address, cx: &App) -> Option<Seen> {
        if let ScreenEntry::Inspection(inspection) = self.history.current()
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
        if let ScreenEntry::Inspection(inspection) = self.history.current() {
            return Some(inspection.clone());
        }
        let address = self.card_target(cx)?;
        let seen = self.card_seen(&address, cx)?;
        Some(CardInspection { address, seen })
    }

    pub fn desk_in_view(&self) -> bool {
        self.current_surface().entity_id() == self.desk.entity_id()
    }

    /// Opens `address`: the system that owns its kind makes the surface,
    /// or the one already made is shown again.
    pub fn open(&mut self, address: Address, window: &mut Window, cx: &mut Context<Self>) {
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
        self.open_with(address, Open::Explicit, window, cx);
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
        if !self.surfaces.contains_key(&address) && !self.hold(&address, window, cx) {
            self.echo(
                &format!("nothing opens {address}"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        let surface = self.surfaces[&address].clone();
        let (entry, method) = match open {
            Open::Deal(card) => {
                surface.show_card(card, window, cx);
                (ScreenEntry::Opened, rho_journal::SurfaceShowMethod::Deal)
            }
            Open::Inspect(inspection) => {
                surface.opened(window, cx);
                (
                    ScreenEntry::Inspection(inspection),
                    rho_journal::SurfaceShowMethod::Open,
                )
            }
            Open::Explicit => {
                surface.opened(window, cx);
                (ScreenEntry::Opened, rho_journal::SurfaceShowMethod::Open)
            }
        };
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: journal_surface(&address),
            method,
        });
        self.history.show(address, entry);
        if self.desk_in_view() {
            self.refresh_desk(&self.hand().clone(), false, cx);
            self.desk.update(cx, |desk, cx| desk.focus_first_row(cx));
        }
        self.moved();
        self.close_overlays(window, cx);
        self.focus_surface(window, cx);
        cx.notify();
    }

    /// Shows `address` in place of the screen in view, for a screen that
    /// switches what it shows (a usage chart's window or picture): the
    /// history entry becomes `address` where it stands, so switching adds
    /// nothing to walk back through. Desk is never replaced; from it this
    /// opens `address` as usual.
    pub fn replace_current(
        &mut self,
        address: Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old = self.current_address().clone();
        if old == address || old.kind() == FRAME_KIND {
            return self.open(address, window, cx);
        }
        if !self.surfaces.contains_key(&address) && !self.hold(&address, window, cx) {
            self.echo(
                &format!("nothing opens {address}"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        self.surfaces[&address].opened(window, cx);
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: journal_surface(&address),
            method: rho_journal::SurfaceShowMethod::Open,
        });
        self.history.replace_current(address, ScreenEntry::Opened);
        self.surfaces.remove(&old);
        self.moved();
        self.close_overlays(window, cx);
        self.focus_surface(window, cx);
        cx.notify();
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

    /// Closes `address`: its surface goes, and no walk reaches it again.
    /// Desk only steps out of view.
    pub fn close(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_address() == address;
        let desk = *address == Self::desk_address();
        if !desk {
            rho_journal::record(rho_journal::Event::HistoryRemoved {
                identity: journal_surface(address),
                method: rho_journal::HistoryRemoveMethod::Close,
            });
            self.history.forget(address);
            self.surfaces.remove(address);
        }
        if current {
            if self.history.back().is_none() && self.history.forward().is_none() {
                self.history.show(Self::desk_address(), ScreenEntry::Opened);
            }
            // A forgotten entry under the cursor is left on the first step
            // either way; Desk closes by stepping off it.
            if desk && self.current_address() == address {
                self.history.show(Self::desk_address(), ScreenEntry::Opened);
            }
            self.settle(false, window, cx);
            self.close_overlays(window, cx);
            self.focus_surface(window, cx);
        }
        self.moved();
        cx.notify();
    }

    /// Closes every open surface whose address `which` picks: what a
    /// detached host leaves behind.
    pub fn close_all(
        &mut self,
        which: impl Fn(&Address) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picked: Vec<Address> = self.surfaces.keys().filter(|a| which(a)).cloned().collect();
        for address in picked {
            self.close(&address, window, cx);
        }
    }

    pub fn close_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let address = self.current_address().clone();
        self.close(&address, window, cx);
    }

    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() {
            return;
        }
        if self.history.back().is_none() {
            self.echo("at the oldest", StyleClass::SystemInfo, cx);
            return;
        }
        self.settle(false, window, cx);
        self.moved();
        rho_journal::record(rho_journal::Event::HistoryStepped {
            direction: rho_journal::HistoryDirection::Back,
            position: self.history.behind(),
            len: self.history.len(),
        });
        self.close_overlays(window, cx);
        self.focus_surface(window, cx);
        cx.notify();
    }

    /// Down: forward through history, and off the newest entry the next
    /// card is dealt.
    pub fn forward_or_deal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.forward().is_none() {
            self.pull_card(window, cx);
            return;
        }
        self.settle(true, window, cx);
        self.moved();
        rho_journal::record(rho_journal::Event::HistoryStepped {
            direction: rho_journal::HistoryDirection::Forward,
            position: self.history.behind(),
            len: self.history.len(),
        });
        self.close_overlays(window, cx);
        self.focus_surface(window, cx);
        cx.notify();
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

    /// Whether a prompt or a menu has the keys.
    pub fn is_asking(&self) -> bool {
        self.minibuffer.is_some() || self.menu.is_some()
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
        self.echo = Some(Echo::new(text, class, dismiss));
        cx.notify();
    }

    // ---- overlays: menus and the minibuffer ----

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
        self.menu = None;
        if self.minibuffer.is_some() {
            self.minibuffer_cancel(window, cx);
        }
        self.overlay_return = None;
    }

    /// Draws `rows` under the point and gives them the keyboard.
    pub fn open_menu(&mut self, rows: Rows, window: &mut Window, cx: &mut Context<Self>) {
        if rows.is_empty() {
            self.echo(
                &format!("{}: nothing here", rows.title),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        if self.minibuffer.is_some() {
            self.minibuffer_cancel(window, cx);
        }
        self.take_keyboard(window, cx);
        self.menu = Some(OpenMenu {
            transient: rows.transient(),
            rows,
            carried_count: None,
            under: Vec::new(),
        });
        self.menu.as_mut().unwrap().resolve_times();
        window.focus(&self.menu_focus, cx);
        cx.notify();
    }

    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    pub fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            self.give_keyboard_back(window, cx);
        }
    }

    /// One step back: to the menu this one is standing on, or out.
    pub(crate) fn menu_dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.menu.as_mut() else {
            return;
        };
        match open.under.pop() {
            Some(parent) => {
                open.transient = parent.transient();
                open.rows = parent;
                open.carried_count = None;
                open.resolve_times();
                cx.notify();
            }
            None => self.close_menu(window, cx),
        }
    }

    fn menu_keystroke(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Bare modifiers arrive as key events too; holding shift for an
        // uppercase key must not dismiss the menu.
        if matches!(
            keystroke.key.as_str(),
            "shift" | "control" | "alt" | "platform" | "function"
        ) {
            return;
        }
        let Some(open) = self.menu.as_mut() else {
            return;
        };
        let carried = open.carried_count;
        match open.transient.press(keystroke) {
            rho_window::transient::Press::Count(_) => {
                open.resolve_times();
                cx.notify();
            }
            rho_window::transient::Press::Dismiss => self.menu_dismiss(window, cx),
            rho_window::transient::Press::Unbound => {}
            rho_window::transient::Press::Run {
                item,
                count,
                closes,
            } => {
                let count = count.or(carried);
                self.run_menu_row(item, count, closes, window, cx);
            }
        }
        cx.stop_propagation();
    }

    /// Row `index` of the open menu, done. A submenu replaces the menu over
    /// the same point; everything else gives the keyboard back first, so
    /// the command sees normal focus.
    fn run_menu_row(
        &mut self,
        index: usize,
        count: Option<u32>,
        closes: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.menu.as_mut() else {
            return;
        };
        let Some(row) = open.rows.rows.get(index).cloned() else {
            return;
        };
        rho_journal::record(rho_journal::Event::MenuRan {
            action: format!("{} › {}", open.rows.title, row.pick.label()),
            count,
        });
        match row.pick {
            Pick::Menu(rows) => {
                let parent = std::mem::replace(&mut open.rows, rows);
                open.under.push(parent);
                open.transient = open.rows.transient();
                open.carried_count = count;
                open.resolve_times();
                cx.notify();
            }
            Pick::Run { run, .. } => {
                if closes {
                    self.close_menu(window, cx);
                }
                run(self, count, window, cx);
                cx.notify();
            }
            Pick::Infix { run, .. } => {
                run(self, count, window, cx);
                cx.notify();
            }
        }
    }

    /// Row `index` of the open menu, picked by pointer: a suffix closes
    /// the menu, as its key would, and a count typed before carries.
    pub(crate) fn click_menu_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let closes = self.menu.as_ref().is_some_and(|open| {
            open.transient.items()[index].kind() == rho_window::transient::Kind::Suffix
        });
        let count = self
            .menu
            .as_ref()
            .and_then(|open| open.transient.count().or(open.carried_count));
        self.run_menu_row(index, count, closes, window, cx);
    }

    /// The Palette joins the catalogue to the actual dealt cards and the
    /// places this window has visited. It does not infer a card from mail.
    pub(crate) fn palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.commands(window, cx);
        let desk_address = Self::desk_address();
        let desk_key = self.screen_binding(&keys::Desk, window, cx);
        let commands = palette::commands(&rows, |action| self.screen_binding(action, window, cx));
        let mut found = Vec::new();
        for system in &self.systems {
            found.extend(system.find(&self.attention.marks, cx));
        }

        let mut known: std::collections::HashSet<_> =
            found.iter().map(|place| place.address.clone()).collect();
        let recent = self.history.recent();
        let visits: HashMap<_, _> = recent
            .iter()
            .copied()
            .enumerate()
            .map(|(rank, key)| (key, rank))
            .collect();
        for address in recent {
            if !known.insert(address.clone()) {
                continue;
            }
            if let Some((title, description)) = self
                .surfaces
                .get(address)
                .map(|surface| (surface.title(cx), String::new()))
                .or_else(|| self.name_of(address, cx))
            {
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
            if known.insert(card.address.clone()) {
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
        // Preserve the deal's own order among waiting places on blank input.
        found.sort_by_key(|place| {
            cards
                .get(&place.address)
                .map_or(usize::MAX, |(rank, _)| *rank)
        });
        let places = found
            .into_iter()
            .map(|found| palette::Place {
                visit: visits.get(&found.address).copied(),
                visited_ms: self
                    .history
                    .visit(&found.address)
                    .and_then(|visit| visit.wall_ms),
                current: self.current_address() == &found.address,
                shortcut: if found.address == desk_address {
                    desk_key.clone()
                } else {
                    None
                },
                card: cards.get(&found.address).map(|(_, card)| (*card).clone()),
                found,
            })
            .collect();
        let mut listing = palette::Listing::new(commands, places);
        listing.desk = self.desk_in_view();
        self.open_minibuffer(":", Answer::palette(listing), None, window, cx);
    }

    /// What choosing a place in the palette does. A project has no screen
    /// of its own: choosing one narrows the palette to its threads, which
    /// carry its address.
    pub(crate) fn choose_place(
        &mut self,
        place: &palette::Place,
        blank: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let address = &place.found.address;
        if let Some(project) = self
            .project_of(address, cx)
            .filter(|project| &project.address == address)
        {
            self.palette(window, cx);
            if let Some(prompt) = self.minibuffer.as_mut() {
                prompt.set_input(format!("{} ", project.name), window, cx);
            }
            return;
        }
        match &place.card {
            Some(card) if !blank || place.visit.is_none() => {
                self.open_card(card.clone(), window, cx)
            }
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

    pub(crate) fn describe_palette_text(
        &self,
        hits: Vec<crate::TextHit>,
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
                Some((row, hit.marks))
            })
            .collect()
    }

    pub(crate) fn finish_palette_search(
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

    pub(crate) fn open_text_match(
        &mut self,
        hit: &crate::FoundText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current_address() != &hit.address {
            self.open(hit.address.clone(), window, cx);
        }
        if self.current_address() == &hit.address {
            self.current_surface()
                .clone()
                .show_search_match(&hit.point, window, cx);
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

    /// The frame's rows, the surface's, and each system's for the point.
    fn commands(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let point = self.point(cx);
        let undo = Row::run("", "undo", |frame, window, cx| {
            frame.undo_verdict(window, cx);
        })
        .window_wide();
        let mut rows = vec![
            Row::run("", "write an idea", |_, window, cx| {
                window.dispatch_action(Box::new(keys::PromptIdea), cx)
            })
            .bound_to(keys::PromptIdea)
            .window_wide(),
            undo,
        ];
        rows.extend(
            self.hosts_menu()
                .into_commands()
                .into_iter()
                .map(Row::window_wide),
        );
        let phrase = match self.attention.secret() {
            Some(secret) => Row::run("", "show my secret phrase", move |frame, window, cx| {
                frame.show_secret(secret, "your secret phrase · enter:", window, cx)
            }),
            None => Row::run("", "pair this device", |frame, window, cx| {
                frame.prompt_pair(String::new(), None, window, cx)
            }),
        };
        rows.push(phrase.window_wide());
        rows.push(Row::run("", "quit", |_, _, cx| cx.quit()).window_wide());
        let surface = self.current_surface().clone();
        rows.extend(surface.rows(self.card_seen(self.current_address(), cx), window, cx));
        for system in self.systems.clone() {
            rows.extend(system.rows(&point, window, cx));
        }
        if !self.desk_in_view() {
            let opener = self.screen_binding(&keys::VerdictMenu, window, cx);
            rows.extend(
                self.verdict_rows(cx)
                    .rows
                    .into_iter()
                    .filter_map(|mut row| {
                        // The window-wide undo is the same verdict and has its
                        // own key.
                        if row.key == "u" {
                            return None;
                        }
                        row.shortcut = opener
                            .as_ref()
                            .map(|key| Shortcut::Menu(format!("{key} {}", row.key)));
                        Some(row)
                    }),
            );
        }
        rows
    }

    fn hosts_menu(&self) -> Rows {
        let mut rows = Rows::new("hosts");
        rows.push(Row::run("a", "attach", |frame, window, cx| {
            frame.open_prompt(
                "attach host · name=local (this machine), name=iroh:<id>@<ssh host> (ssh), \
                 or name=iroh:<id> (security key):",
                Rc::new(|_, _, _| Vec::new()),
                Rc::new(
                    |frame, input, _, cx| match HostSpec::parse(input.trim(), "rho") {
                        Ok(spec) => {
                            frame.attach_host(spec, cx);
                        }
                        Err(error) => {
                            frame.echo(&format!("attach: {error}"), StyleClass::SystemInfo, cx)
                        }
                    },
                ),
                window,
                cx,
            );
        }));
        rows.push(Row::run("d", "detach", |frame, window, cx| {
            frame.open_prompt(
                "detach host:",
                Rc::new(|frame, needle, _| {
                    frame
                        .hosts
                        .iter()
                        .filter(|host| host.name.contains(needle.trim()))
                        .map(|host| Candidate {
                            value: host.name.clone(),
                            description: host.status.label(),
                        })
                        .collect()
                }),
                Rc::new(|frame, input, _, cx| {
                    let name = input.trim();
                    match frame.hosts.by_name(name).map(|host| host.id) {
                        Some(host) => {
                            frame.detach_host(host, cx);
                            frame.echo(&format!("detached: {name}"), StyleClass::SystemInfo, cx);
                        }
                        None => frame.echo(
                            &format!("detach: no host called {name}"),
                            StyleClass::SystemInfo,
                            cx,
                        ),
                    }
                }),
                window,
                cx,
            );
        }));
        rows.push(Row::menu("g", crate::gccp::rows()));
        for (index, host) in self.hosts.iter().enumerate() {
            let id = host.id;
            rows.push(Row::run(
                (index + 1).to_string(),
                format!("{} · {}", host.name, host.status.label()),
                move |frame, _, cx| frame.pick_host(id, cx),
            ));
        }
        rows
    }

    /// The verdicts over the card under the cursor.
    pub(crate) fn verdict_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            self.close_menu(window, cx);
            self.toggle_desk(window, cx);
            return;
        }
        self.open_menu(self.verdict_rows(cx), window, cx);
    }

    fn verdict_rows(&self, cx: &mut App) -> Rows {
        let Some(target) = self.card_target(cx) else {
            // With no card here there is nothing to judge, but the deal
            // still goes on: `n` is how the user asks what waits.
            let mut rows = Rows::new("no card here").counted();
            self.push_deal_rows(&mut rows);
            return rows;
        };
        let card = self.card_for(&target, cx);
        let mut rows = Rows::new(if self.current_surface().inspects_card(cx) {
            card.title.clone()
        } else {
            card.heading()
        })
        .counted();
        if let Some(own) = self.own_verdicts(&target, cx) {
            // A card its system answers is never muted: it ends when it is
            // answered, and until then it can only wait.
            rows.extend(own);
            rows.push(Row::menu("s", snooze_rows(&target)));
        } else if self.marks().get(&target).facts().muted() {
            rows.push(Row::run("m", "unmute", move |frame, window, cx| {
                frame.verdict(&target, Verdict::Unmute, "unmuted".to_owned(), window, cx);
            }));
        } else {
            rows.extend(verdict_rows(target.clone()));
            rows.push(Row::menu("s", snooze_rows(&target)));
        }
        self.push_deal_rows(&mut rows);
        rows
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

    /// The verdict menu's rows that need no card: take back the last
    /// verdict, and deal the next card. On a Desk that reads `0 wait` a
    /// pull would deal nothing and stay, so it is not offered there.
    fn push_deal_rows(&self, rows: &mut Rows) {
        rows.push(Row::counted("u", "undo", |frame, _, window, cx| {
            frame.undo_verdict(window, cx)
        }));
        if self.desk_in_view() && self.hand().cards.is_empty() {
            return;
        }
        rows.push(Row::counted("n", "next card", |frame, _, window, cx| {
            frame.pull_card(window, cx)
        }));
    }
    pub(crate) fn toggle_desk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.desk_in_view() {
            true => self.back(window, cx),
            false => self.open_desk(window, cx),
        }
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
        self.menu = None;
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
        cx.notify();
    }

    pub(crate) fn minibuffer_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let on_cancel = minibuffer.on_cancel();
        self.give_keyboard_back(window, cx);
        if let Some(on_cancel) = on_cancel {
            on_cancel(self, window, cx);
        }
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

    /// Enter on Desk: the row under the cursor.
    pub(crate) fn open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(address) = self.desk.read(cx).cursor_card(cx)
            && let Some(card) = self
                .hand()
                .cards
                .iter()
                .find(|card| card.address == address)
                .cloned()
        {
            self.journal_deal(Some(&address), cx);
            self.open_card(card, window, cx);
        }
    }

    // ---- render ----

    fn render_status_line(
        &mut self,
        text_style: &gpui::TextStyle,
        layout: Layout,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let mode = self.mode(window, cx);
        let mode_color = colors.terminal_ansi_bright_cyan;
        let mode = mode_word(&mode);
        // Signal evaluation is deferred. Desk must not paint the lamp
        // from the previous screen while that update is still pending.
        let lamp = (self.signals.lamp_on && !self.desk_in_view()).then_some("●");
        let unwell = self.unwell_hosts();
        let surface = self.current_surface();
        let phone_titles = match layout {
            Layout::Phone => Some(surface.phone_titles(cx)),
            Layout::Desktop => None,
        };
        let surface_status = surface.status(cx).unwrap_or_default();
        let measured = crate::phone::measure(text_style, window);
        // The pairing hint is secondary to a surface that has already
        // shed everything but the name of the work and its condition.
        let show_phone_notice = phone_titles.as_ref().is_none_or(|titles| {
            titles.len() < 2
                || titles.last().is_none_or(|title| {
                    let reserve = lamp
                        .into_iter()
                        .chain(unwell.as_deref())
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
                })
        });
        let unpaired = (self.echo.is_none() && self.attention.pairing() == Pairing::Unpaired)
            .then_some(match layout {
                Layout::Desktop => Some("verdicts stay on this device until it is paired"),
                Layout::Phone => show_phone_notice.then_some("unpaired"),
            })
            .flatten();
        // A heading is cut to what the line has room for: on the desk a
        // fixed length does, but a phone is too narrow to guess at, and a
        // cut by length there would push the card's state off the edge.
        let right_words = lamp
            .into_iter()
            .chain(unwell.as_deref())
            .chain(unpaired)
            .chain([mode.as_str()])
            .collect::<Vec<_>>();
        let fit = |path: &str, state: &str| match layout {
            Layout::Desktop => (truncate_path(path), state.to_owned()),
            Layout::Phone => {
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
            }
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
            let title = if let Some(titles) = phone_titles {
                titles
                    .iter()
                    .find(|title| fit(title, &surface_status).0 == **title)
                    .or_else(|| titles.last())
                    .cloned()
                    .unwrap_or_default()
            } else {
                surface.title(cx)
            };
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
        // the line is short.
        let right = div()
            .flex()
            .flex_row()
            .min_w_0()
            .items_center()
            .gap_3()
            .children(lamp.map(|lamp| div().flex_none().text_color(status.warning).child(lamp)))
            .children(unwell.map(|text| div().flex_none().text_color(status.error).child(text)))
            .children(unpaired.map(|text| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(colors.text_muted)
                    .child(text)
            }))
            .child(div().flex_none().text_color(mode_color).child(mode));
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

    /// The mode the keys are in: a surface's own while it has the keys,
    /// such as a raw place taking live input; else the minibuffer's or the
    /// surface editor's, as vim reads it.
    pub(crate) fn mode(&self, window: &Window, cx: &App) -> String {
        // A menu or a prompt over the surface is the editor's to say.
        if self.menu.is_none() && self.minibuffer.is_none() {
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

    fn unwell_hosts(&self) -> Option<String> {
        let (name, status) = self.hosts.worst_status()?;
        let unwell = self
            .hosts
            .iter()
            .filter(|host| host.status != HostStatus::Online)
            .count();
        if unwell > 1 {
            return Some(format!(
                "{unwell} of {} hosts not connected",
                self.hosts.len()
            ));
        }
        // Named even when it is the only host: `reconnecting` alone does
        // not say what is away.
        match status {
            HostStatus::Online => None,
            status => Some(format!("{name} · {}", status.label())),
        }
    }
}

/// How long `target` sleeps: a unit, and the count typed before it.
pub(crate) fn snooze_rows(target: &Address) -> Rows {
    let mut snooze = Rows::new("snooze").counted();
    for (key, label, unit) in [
        ("h", "hour", SnoozeUnit::Hours),
        ("d", "tomorrow", SnoozeUnit::Days),
        ("w", "next week", SnoozeUnit::Weeks),
    ] {
        let target = target.clone();
        let mut row = Row::counted(key, label, move |frame, count, window, cx| {
            frame.snooze(&target, unit, count, window, cx)
        });
        row.snooze = Some(unit);
        snooze.push(row);
    }
    snooze
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

fn truncate_path(path: &str) -> String {
    const MAX_CHARS: usize = 80;
    if path.chars().count() <= MAX_CHARS {
        return path.to_owned();
    }
    let parts = path.split(" › ").collect::<Vec<_>>();
    if parts.len() < 3 {
        return path.chars().take(MAX_CHARS - 1).collect::<String>() + "…";
    }
    format!("{} › … › {}", parts[0], parts[parts.len() - 1])
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
        let inset = FRAME_PADDING;
        let frame = div()
            .id("rho-frame")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .p(inset)
            .bg(cx.theme().colors().editor_background)
            .text_color(cx.theme().colors().text)
            .key_context(crate::KEY_CONTEXT)
            .capture_any_mouse_down(
                cx.listener(|this, _: &gpui::MouseDownEvent, _, _| this.note_input()),
            )
            .on_action(cx.listener(|this, _: &keys::Back, window, cx| this.back(window, cx)))
            .on_action(
                cx.listener(|this, _: &keys::Forward, window, cx| this.forward_or_deal(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &keys::Close, window, cx| this.close_current(window, cx)),
            )
            .on_action(cx.listener(|this, _: &keys::CloseAndDeal, window, cx| {
                this.close_current(window, cx);
                this.pull_card(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keys::Palette, window, cx| this.palette(window, cx)))
            .on_action(cx.listener(|this, _: &keys::VerdictMenu, window, cx| {
                if this.desk_in_view() {
                    this.desk_space(window, cx);
                } else {
                    this.verdict_menu(window, cx);
                }
            }))
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
            .on_action(cx.listener(|this, _: &keys::OpenRow, window, cx| this.open_row(window, cx)))
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
            }))
            .child(self.render_status_line(&text_style, layout, window, cx));
        let minibuffer = self.minibuffer.as_ref().map(|minibuffer| {
            minibuffer.render(
                &text_style,
                layout,
                window.viewport_size().width - inset * 2.,
                window,
                cx,
            )
        });
        if layout == Layout::Phone {
            let body = self.render_phone_surface(&surface, cx);
            let bar = self.render_phone_bar(window, cx);
            let sheet = self.render_menu_sheet(&text_style, cx);
            return frame
                .child(body)
                .children(minibuffer)
                .child(bar)
                .children(sheet);
        }
        let keep_bottom = surface
            .editor(cx)
            .is_some_and(|editor| editor.read(cx).keeps_bottom_visible());
        frame
            .child(div().flex_1().min_h_0().w_full().child(surface.view()))
            .children(self.menu.as_ref().map(|open| {
                let menu = div().track_focus(&self.menu_focus);
                // A bottom-aligned document and a stuck excerpt need space
                // above the choices. Other buffers keep their usual overlay.
                let menu = if keep_bottom {
                    menu.flex_none()
                } else {
                    menu.absolute().bottom_0().left_0().right_0()
                };
                menu.child(
                    bottom_strip(&text_style, cx).child(open.transient.render_rows(
                        &text_style,
                        cx,
                        |index, row| {
                            row.id(("rho-menu-row", index))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.click_menu_row(index, window, cx);
                                    cx.stop_propagation();
                                }))
                                .into_any_element()
                        },
                    )),
                )
            }))
            .children(minibuffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::desk_rows;

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

    fn labels(rows: &Rows) -> Vec<String> {
        rows.rows
            .iter()
            .map(|row| row.pick.label().to_owned())
            .collect()
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

    /// The way back as the last run left it: Desk, then `screen` in view.
    fn saved_on(screen: &Address) -> place::Saved {
        let mut history = History::new(Frame::desk_address(), ScreenEntry::Opened);
        history.show(screen.clone(), ScreenEntry::Opened);
        place::Saved::of(&history)
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
                // Back and forward again before the host answers.
                frame.back(window, cx);
                assert_eq!(frame.current_address(), &Frame::desk_address());
                frame.forward_or_deal(window, cx);
                assert_eq!(frame.current_address(), &agent, "the way back kept it");
                assert!(is_waiting(frame));

                holds.set(Holds::Here);
                frame.reopen_waiting(window, cx);
                assert_eq!(frame.current_address(), &agent);
                assert_eq!(frame.current_surface().title(cx), "The meter");
                assert!(
                    frame.current_surface().focus_handle(cx).is_focused(window),
                    "the keyboard moves to the real screen"
                );
                assert_eq!(frame.history.len(), 2);
            })
            .unwrap();
    }

    #[gpui::test]
    fn an_early_back_reaches_a_screen_left_behind_the_desk(cx: &mut gpui::TestAppContext) {
        let (frame, holds) = host_backed_frame(cx);
        let agent = Address::new("agent", "standing");
        frame
            .update(cx, |frame, window, cx| {
                // A cold return: Desk in view, the agent just behind it.
                frame.last_input = None;
                frame.return_to(saved_on(&agent), window, cx);
                assert_eq!(frame.current_address(), &Frame::desk_address());
                frame.back(window, cx);
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
                assert_eq!(frame.history.len(), 1, "no walk reaches it again");
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_row_on_desk_is_not_an_open_card(cx: &mut gpui::TestAppContext) {
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
                let rows = desk_rows(&hand, |_| Some(project.clone()), |_| None);
                frame
                    .desk
                    .update(cx, |desk, cx| desk.set_rows(rows, false, cx));
                assert!(frame.desk_in_view());
                assert_eq!(
                    frame.card_target(cx),
                    Some(subject.clone()),
                    "a verdict can target the row"
                );
                assert_eq!(frame.card_in_view(cx), None, "a pull must not skip the row");

                let card = cx.new(|cx| OpenCard(cx.focus_handle()));
                frame
                    .surfaces
                    .insert(subject.clone(), AnySurface::new(card));
                frame.history.show(subject.clone(), ScreenEntry::Opened);
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
                frame.verdict_menu(window, cx);
                let menu = &frame.menu.as_ref().expect("space still opens a menu").rows;
                assert_eq!(menu.title, "no card here");
                assert_eq!(labels(menu), ["undo", "next card"]);
            })
            .unwrap();
    }

    /// A system with one project, `tide@lab`, that `find` lists.
    struct Projects;

    impl crate::System for Projects {
        fn kinds(&self) -> &'static [&'static str] {
            &["project"]
        }

        fn open(&mut self, _: &Address, _: &mut Window, _: &mut Context<Self>) -> Opening {
            Opening::Gone
        }

        fn project(&self, address: &Address, _: &App) -> Option<crate::Project> {
            (address == &Address::new("project", "tide")).then(|| crate::Project {
                address: address.clone(),
                name: "tide@lab".into(),
            })
        }

        fn find(&self, _: &rho_dealer::Marks, _: &App) -> Vec<Found> {
            vec![Found {
                address: Address::new("project", "tide"),
                title: "tide@lab".into(),
                description: "on /srv/tide · one agent · opus-high".into(),
                thread: None,
                recency: 0,
                also: Vec::new(),
            }]
        }
    }

    /// A project has no screen: choosing it in the palette filters the
    /// palette to its threads.
    #[gpui::test]
    fn choosing_a_project_in_the_palette_filters_to_its_threads(cx: &mut gpui::TestAppContext) {
        let window = test_frame_with(cx, rho_db::RhoDb::in_memory(), |cx| {
            vec![AnySystem::new(cx.new(|_| Projects))]
        });
        window
            .update(cx, |frame, window, cx| {
                frame.palette(window, cx);
                frame
                    .minibuffer_mut()
                    .unwrap()
                    .set_input("tide@lab".into(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), keys::MinibufferConfirm);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, cx| {
                assert_eq!(frame.current_address(), &Frame::desk_address());
                let palette = frame.minibuffer_mut().expect("the palette stays open");
                assert_eq!(palette.input(cx), "tide@lab ");
            })
            .unwrap();
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
                let menu = frame.verdict_rows(cx);
                assert_eq!(menu.title, "no card here");
                assert_eq!(labels(&menu), ["undo", "next card"]);
                let commands = frame.commands(window, cx);
                let commands = commands
                    .iter()
                    .map(|row| row.pick.label())
                    .collect::<Vec<_>>();
                assert!(commands.contains(&"next card"), "{commands:?}");
                for verdict in ["done", "mute", "unmute", "snooze"] {
                    assert!(!commands.contains(&verdict), "{verdict} in {commands:?}");
                }
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

                // The card has left the hand; `space` still offers undo
                // and the next card, and undo stays on the transcript.
                frame
                    .signals
                    .hand
                    .cards
                    .retain(|card| card.address != inspected);
                assert_eq!(frame.card_target(cx), None);
                frame.verdict_menu(window, cx);
                let menu = &frame.menu.as_ref().expect("space opens a menu").rows;
                assert_eq!(menu.title, "no card here");
                assert_eq!(labels(menu), ["undo", "next card"]);
                frame.close_menu(window, cx);
                frame.undo_verdict(window, cx);
                assert_eq!(settled(frame), []);
                assert_eq!(
                    frame.current_address(),
                    &agent,
                    "undo stays on the transcript"
                );

                frame.open(agent, window, cx);
                assert_eq!(
                    frame.card_target(cx),
                    None,
                    "a new visit has no stale origin"
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
                frame.history.show(thread.clone(), ScreenEntry::Opened);
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

    #[gpui::test]
    fn an_empty_desk_offers_undo_but_no_next_card(cx: &mut gpui::TestAppContext) {
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
                // Away from Desk, nothing waiting still deals: it opens Desk.
                wants.borrow_mut().clear();
                frame.systems_changed(cx);
                assert_eq!(
                    labels(&frame.verdict_rows(cx)),
                    ["done", "mute", "snooze", "undo", "next card"]
                );
                frame.verdict(&thread, Verdict::Done, "done".to_owned(), window, cx);
                assert!(frame.desk_in_view());
                assert!(frame.attention.can_undo());
                frame.desk_space(window, cx);
                let menu = &frame.menu.as_ref().expect("space offers undo").rows;
                assert_eq!(labels(menu), ["undo"], "0 wait: no next card on Desk");
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
                frame.history.show(thread.clone(), ScreenEntry::Opened);
                assert!(!frame.attention.can_undo(), "the window just restarted");
                assert!(frame.marks().get(&thread).facts().muted());
                let mut rows = frame.verdict_rows(cx).rows;
                let unmute = rows
                    .iter()
                    .position(|row| row.pick.label() == "unmute")
                    .expect("the muted thread offers unmute");
                let Pick::Run { run, .. } = rows.swap_remove(unmute).pick else {
                    panic!("not an operation")
                };
                run(frame, None, window, cx);
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
            Some(vec![Row::run("a", "approve", move |frame, window, cx| {
                waiting
                    .borrow_mut()
                    .retain(|claim| claim.address != address);
                frame.answered(&address, "approved: push".to_owned(), window, cx);
            })])
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
            let rows = frame.verdict_rows(cx);
            assert_eq!(
                labels(&rows)[..2],
                ["approve".to_owned(), "snooze".to_owned()],
                "its own words, and a snooze, in place of done and mute"
            );
            assert!(
                !labels(&rows)
                    .iter()
                    .any(|label| label == "done" || label == "mute")
            );
            let Pick::Run { run, .. } = rows.rows[0].pick.clone() else {
                panic!("approve runs");
            };
            run(frame, None, window, cx);
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

    /// On the phone, `done` on a push opens its answers rather than
    /// settling it.
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
                let menu = frame.menu.as_ref().expect("the answers are offered");
                assert_eq!(labels(&menu.rows)[0], "approve");
                assert!(frame.marks().get(&push).facts.is_empty(), "nothing settled");
            })
            .unwrap();
    }

    /// Switching a screen in place three times leaves history as long as
    /// it was, with the screen where it stood, and what is written down
    /// for the next start names the screen as it is now.
    #[gpui::test]
    fn switching_a_screen_in_place_adds_no_history(cx: &mut gpui::TestAppContext) {
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
                let len = frame.history.len();
                for chart in &charts[1..] {
                    frame.replace_current(chart.clone(), window, cx);
                }
                assert_eq!(frame.history.len(), len);
                assert_eq!(frame.current_address(), &charts[3]);
                assert_eq!(
                    frame.history.keys_back().cloned().collect::<Vec<_>>(),
                    [thread.clone(), Frame::desk_address()]
                );
                for gone in &charts[..3] {
                    assert!(frame.surface(gone).is_none(), "{gone} went");
                }
                let saved = place::Saved::of(&frame.history);
                assert_eq!(
                    saved.history(&Frame::desk_address()).current_key(),
                    &charts[3]
                );
            })
            .unwrap();
    }

    // ---- commands that act on a host ----

    use rho_agent_hosts::protocol::{
        GccpApprove, GccpContact, GccpHost, GccpList, GccpRemove, GccpState, Open as HostOpen,
        Request as HostRequest,
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
                let host = frame.attach_host(spec, cx);
                frame
                    .hosts()
                    .connection(host)
                    .unwrap()
                    .link()
                    .connect_in_process()
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

    /// Runs the palette's command called `label`, as `enter` on its row.
    fn run_command(frame: &mut Frame, label: &str, window: &mut Window, cx: &mut Context<Frame>) {
        let commands = frame.commands(window, cx);
        let Some(row) = commands.iter().find(|row| row.pick.label() == label) else {
            let labels: Vec<_> = commands.iter().map(|row| row.pick.label()).collect();
            panic!("no {label:?} in {labels:?}");
        };
        let Pick::Run { run, .. } = &row.pick else {
            panic!("{label} is no command");
        };
        run.clone()(frame, None, window, cx);
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
        "hosts › gccp › list",
        "hosts › gccp › approve…",
        "hosts › gccp › remove…",
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
                        let row = commands
                            .iter()
                            .find(|row| row.pick.label() == label)
                            .unwrap_or_else(|| panic!("{label} on {}", frame.current_address()));
                        assert_eq!(row.scope, crate::menu::Scope::Window, "{label}");
                    }
                }
                run_command(frame, GCCP[0], window, cx);
                assert_eq!(prompt_of(frame), None, "nothing to ask about");
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("gccp list: no agent host is attached")
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
            .update(cx, |frame, window, cx| {
                run_command(frame, GCCP[0], window, cx)
            })
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
                run_command(frame, GCCP[2], window, cx);
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
                run_command(frame, GCCP[1], window, cx);
                assert_eq!(
                    prompt_of(frame).as_deref(),
                    Some("approve a gccp harness on lab · <endpoint id> <name>:")
                );
                answer(frame, HARNESS, window, cx);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("gccp approve: give the harness's endpoint id, then a name for it")
                );
                run_command(frame, GCCP[1], window, cx);
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
            .update(cx, |frame, window, cx| {
                run_command(frame, GCCP[2], window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        frame
            .update(cx, |frame, _, _| {
                assert_eq!(prompt_of(frame), None);
                assert_eq!(
                    echo_of(frame).as_deref(),
                    Some("gccp remove: lab: gccp needs an iroh endpoint")
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
                run_command(frame, GCCP[0], window, cx);
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
                    Some("gccp list: lab answered while another prompt was open · run it again")
                );
            })
            .unwrap();
    }
}
