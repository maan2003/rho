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
use rho_agent_hosts::{HostEvent, HostSpec};
use rho_dealer::{Address, Card};
use rho_ledger::Pairing;
use rho_window::history::History;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::attention::{Attention, Verdict};
use crate::deal::Signals;
use crate::desk::{DeskTarget, DeskView};
use crate::idea::IdeaBox;
use crate::menu::{OpenMenu, Pick, Row, Rows, Shortcut};
use crate::minibuffer::{
    Answer, CancelHandler, Candidate, CandidateSource, Echo, Minibuffer, SubmitHandler,
    bottom_strip,
};
use crate::phone::{Layout, Phone};
use crate::search::Search;
use crate::surface::{AnySurface, Mode, Subject, Surface as _};
use crate::system::{AnySystem, Found};
use crate::when::SnoozeUnit;
use crate::{keys, palette, place};

/// The kind of the frame's own surface: Desk.
pub const FRAME_KIND: &str = "frame";
pub(crate) const FRAME_PADDING: Pixels = px(2.);

/// Where the reader is: the surface in view, what the point in it is
/// on, which is the surface's own address when it is a page about one
/// thing and a row's when it lists things, and the card that holds it,
/// which a verdict is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub surface: Address,
    pub subject: Option<Address>,
    pub card: Option<Address>,
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
    history: History<Address, ()>,
    surfaces: HashMap<Address, AnySurface>,
    pub(crate) desk: Entity<DeskView>,
    pub(crate) idea: IdeaBox,
    pub(crate) minibuffer: Option<Minibuffer>,
    last_palette_query: Option<String>,
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
        let idea = IdeaBox::new(cx, &db);
        let desk = cx.new(|cx| DeskView::new(window, cx));
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
            history: History::new(desk_address, ()),
            surfaces,
            desk,
            idea,
            minibuffer: None,
            last_palette_query: None,
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
                    .update_in(cx, |this, window, cx| {
                        this.host_event(host, event, window, cx)
                    })
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
        if self.surfaces.contains_key(&address) {
            return;
        }
        match self.make_surface(&address, window, cx) {
            Some(surface) => {
                self.surfaces.insert(address, surface);
            }
            None => {
                self.history.forget(&address);
                self.desk.update(cx, |desk, cx| desk.focus_first_row(cx));
                self.history.show(Self::desk_address(), ());
            }
        }
    }

    /// The surface for `address`, from the system that owns its kind.
    fn make_surface(
        &self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        if self
            .project_for(address, cx)
            .is_some_and(|(_, project)| &project.address == address)
        {
            return Some(AnySurface::new(self.desk.clone()));
        }
        self.systems
            .clone()
            .into_iter()
            .find(|system| system.opens(address, cx))
            .and_then(|system| system.open(address, window, cx))
    }

    /// Makes the screen the cursor came to, when the history holds it by
    /// address alone, as one it was given back at start does. One that no
    /// longer opens leaves the history and the walk goes on the way it
    /// went, or the other way from the end; Desk always opens.
    fn settle(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let address = self.current_address().clone();
            if self.surfaces.contains_key(&address) {
                return;
            }
            if let Some(surface) = self.make_surface(&address, window, cx) {
                self.surfaces.insert(address, surface);
                return;
            }
            self.history.forget(&address);
            let stepped = match forward {
                true => self.history.forward().is_some() || self.history.back().is_some(),
                false => self.history.back().is_some() || self.history.forward().is_some(),
            };
            if !stepped {
                self.history.show(Self::desk_address(), ());
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
        if let Some(editor) = self.idea_editor() {
            return editor;
        }
        self.current_surface()
            .editor(cx)
            .unwrap_or_else(|| self.chrome_editor.clone())
    }

    /// Where the reader is.
    pub fn point(&self, cx: &App) -> Point {
        let surface = self.current_address().clone();
        let resolve = |subject: Subject| match subject {
            Subject::Own if surface.kind() == FRAME_KIND => None,
            Subject::Own => Some(surface.clone()),
            Subject::Address(address) => Some(address),
            Subject::None => None,
        };
        let current = self.current_surface();
        let subject = resolve(current.subject(cx));
        let card = resolve(current.card(cx));
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
        match self.current_address().kind() == FRAME_KIND {
            true => None,
            false => self.card_target(cx),
        }
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
        self.open_with(address, None, window, cx);
    }

    pub(crate) fn open_with(
        &mut self,
        address: Address,
        card: Option<&Card>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.surfaces.contains_key(&address) {
            let Some(surface) = self.make_surface(&address, window, cx) else {
                self.echo(
                    &format!("nothing opens {address}"),
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            };
            self.surfaces.insert(address.clone(), surface);
        }
        let surface = self.surfaces[&address].clone();
        if let Some(card) = card {
            surface.show_card(card, window, cx);
        } else {
            surface.opened(window, cx);
        }
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: journal_surface(&address),
            method: if card.is_some() {
                rho_journal::SurfaceShowMethod::Deal
            } else {
                rho_journal::SurfaceShowMethod::Open
            },
        });
        self.history.show(address, ());
        if self.desk_in_view() {
            self.refresh_desk(&self.hand().clone(), false, cx);
            self.desk.update(cx, |desk, cx| desk.focus_first_row(cx));
        }
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
        if address.kind() != FRAME_KIND {
            rho_journal::record(rho_journal::Event::HistoryRemoved {
                identity: journal_surface(address),
                method: rho_journal::HistoryRemoveMethod::Close,
            });
            self.history.forget(address);
            self.surfaces.remove(address);
        }
        if current {
            if self.history.back().is_none() && self.history.forward().is_none() {
                self.history.show(Self::desk_address(), ());
            }
            // A forgotten entry under the cursor is left on the first step
            // either way; a frame surface closes by stepping off it.
            if address.kind() == FRAME_KIND && self.current_address() == address {
                self.history.show(Self::desk_address(), ());
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
        if self.idea_here(cx) {
            self.close_idea_overlay(window, cx);
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
        if *self.current_address() != Self::desk_address() || self.idea_here(cx) {
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
        self.close_idea_overlay(window, cx);
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

    pub(crate) fn find_text(
        &self,
        query: &crate::TextQuery<'_>,
        cx: &App,
    ) -> Vec<crate::FoundText> {
        self.systems
            .iter()
            .flat_map(|system| {
                system.find_text(query, self.current_address(), &self.attention.marks, cx)
            })
            .collect()
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
                    .map(|key| key.unparse())
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
            Row::run("", "write an idea", |frame, window, cx| {
                frame.prompt_idea(window, cx)
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
        rows.extend(surface.rows(window, cx));
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
        for (index, host) in self.hosts.iter().enumerate() {
            let id = host.id;
            rows.push(Row::run(
                (index + 1).to_string(),
                format!("{} · {}", host.name, host.status.label()),
                move |frame, _, cx| {
                    let label = frame.hosts.host_label(id);
                    let status = frame
                        .hosts
                        .get(id)
                        .map(|host| host.status.label())
                        .unwrap_or_default();
                    frame.echo(&format!("{label}: {status}"), StyleClass::SystemInfo, cx);
                },
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
        let inspecting = self.current_surface().inspects_card();
        let target = self.card_target(cx);
        if inspecting
            && !target
                .as_ref()
                .is_some_and(|target| self.hand().cards.iter().any(|card| &card.address == target))
        {
            self.echo("nothing waits here", StyleClass::SystemInfo, cx);
            return;
        }
        self.open_menu(self.verdict_rows(cx), window, cx);
    }

    fn verdict_rows(&self, cx: &App) -> Rows {
        let inspecting = self.current_surface().inspects_card();
        let Some(target) = self.card_target(cx).filter(|target| {
            !inspecting || self.hand().cards.iter().any(|card| &card.address == target)
        }) else {
            // With no card here there is nothing to judge, but the deal
            // still goes on: `n` is how the user asks what waits.
            let mut rows = Rows::new("no card here").counted();
            push_deal_rows(&mut rows);
            return rows;
        };
        let card = self.card_for(&target, cx);
        let mut rows = Rows::new(if inspecting {
            card.title.clone()
        } else {
            card.heading()
        })
        .counted();
        rows.extend(verdict_rows(target.clone()));
        rows.push(Row::menu("s", snooze_rows(&target)));
        push_deal_rows(&mut rows);
        rows
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
        let is_palette = minibuffer.is_palette();
        let prompt = minibuffer.prompt().to_owned();
        let secret = minibuffer.is_secret();
        let (input, submission) = minibuffer.into_submission(cx);
        if is_palette && !input.trim().is_empty() {
            self.last_palette_query = Some(input.clone());
        }
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
        if minibuffer.is_palette() {
            let input = minibuffer.input(cx);
            if !input.trim().is_empty() {
                self.last_palette_query = Some(input);
            }
        }
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

    fn minibuffer_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.minibuffer.as_mut() else {
            return;
        };
        if prompt.is_palette() {
            if let Some(query) = self.last_palette_query.clone() {
                prompt.set_input(query, window, cx);
            }
        } else {
            prompt.select_by_delta(-1);
        }
        cx.notify();
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
        match self.desk.read(cx).cursor_target(cx) {
            Some(DeskTarget::Card(address)) => {
                if let Some(card) = self
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
            Some(DeskTarget::Thread(address)) => self.open(address, window, cx),
            None => {}
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
        let composing_idea = self.idea_here(cx);
        let surface_status = if composing_idea {
            String::new()
        } else {
            surface.status(cx).unwrap_or_default()
        };
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
        let own_card = self
            .card_in_view(cx)
            .filter(|address| address == self.current_address());
        let (heading, state, state_color) = if composing_idea {
            ("idea".to_owned(), String::new(), colors.text_muted)
        } else if let Some(address) = own_card {
            let card = self.card_for(&address, cx);
            (
                surface.heading(cx).unwrap_or_else(|| card.heading()),
                card.label,
                status.warning,
            )
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

    /// The mode the keys are in. The minibuffer and global idea strip
    /// always type; the mode indicator follows the active surface editor.
    pub(crate) fn mode(&self, window: &Window, cx: &App) -> String {
        let idea = self.idea_editor();
        if idea
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
        {
            return "insert".to_owned();
        }
        // A surface with a mode of its own says it while it has the keys;
        // a menu or a prompt over it is the editor's to say.
        if self.menu.is_none() && self.minibuffer.is_none() {
            match self.current_surface().mode(cx) {
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

/// The verdict menu's rows that need no card: take back the last verdict,
/// and deal the next card.
fn push_deal_rows(rows: &mut Rows) {
    rows.push(Row::counted("u", "undo", |frame, _, window, cx| {
        frame.undo_verdict(window, cx)
    }));
    rows.push(Row::counted("n", "next card", |frame, _, window, cx| {
        frame.pull_card(window, cx)
    }));
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
        let editor = self.idea_editor().unwrap_or_else(|| self.active_editor(cx));
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
            .on_action(
                cx.listener(|this, _: &keys::PromptIdea, window, cx| this.prompt_idea(window, cx)),
            )
            .on_action(cx.listener(|this, _: &keys::FocusIdeaAddress, window, cx| {
                this.focus_idea_address(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &keys::SendIdea, window, cx| this.send_idea(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &keys::CancelIdea, window, cx| this.cancel_idea(window, cx)),
            )
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
            .on_action(cx.listener(|this, _: &keys::MinibufferUp, window, cx| {
                this.minibuffer_up(window, cx)
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
        let idea = self.render_idea(cx);
        if layout == Layout::Phone {
            let body =
                idea.unwrap_or_else(|| self.render_phone_surface(&surface, cx).into_any_element());
            let bar = self.render_phone_bar(window, cx);
            let sheet = self.render_menu_sheet(&text_style, cx);
            return frame
                .child(body)
                .children(minibuffer)
                .child(bar)
                .children(sheet);
        }
        frame
            .child(idea.unwrap_or_else(|| {
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(surface.view())
                    .into_any_element()
            }))
            // The menu, pinned to the bottom edge and drawn over the buffer
            // rather than in it: nothing reflows, and the point stays where
            // the reader left it.
            .children(self.menu.as_ref().map(|open| {
                div()
                    .track_focus(&self.menu_focus)
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(
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
