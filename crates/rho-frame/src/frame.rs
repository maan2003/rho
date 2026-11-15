//! The frame entity: the window's root view.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use editor::Editor;
use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Subscription, Task, Window, div, px,
};
use rho_agent_hosts::hosts::{HostStatus, Hosts};
use rho_agent_hosts::{HostEvent, HostSpec};
use rho_dealer::Address;
use rho_ledger::stream::LedgerEvent;
use rho_window::history::History;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::attention::{Attention, Verdict};
use crate::deal::Signals;
use crate::home::{HomeTarget, HomeView};
use crate::keys;
use crate::log::MessageLog;
use crate::menu::{OpenMenu, Pick, Row, Rows};
use crate::minibuffer::{
    CancelHandler, Candidate, CandidateSource, Echo, Minibuffer, SubmitHandler, bottom_strip,
};
use crate::surface::{AnySurface, Subject};
use crate::system::{AnySystem, Found};
use crate::when::SnoozeUnit;

/// The kind of the frame's own surfaces: Home and the message log.
pub const FRAME_KIND: &str = "frame";

/// Where the reader is: the surface in view, and what the point in it is
/// on, which is the surface's own address when it is a page about one
/// thing and a row's when it lists things.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub surface: Address,
    pub subject: Option<Address>,
}

/// How long an echo stays on the line.
const ECHO_FOR: Duration = Duration::from_secs(4);

pub struct Frame {
    pub(crate) db: rho_db::RhoDb,
    pub(crate) hosts: Hosts,
    pub(crate) systems: Vec<AnySystem>,
    pub(crate) attention: Attention,
    /// The surfaces the reader has been on, by address. The newest entry
    /// is where down deals; Home is the oldest and never closes.
    history: History<Address, ()>,
    surfaces: HashMap<Address, AnySurface>,
    pub(crate) home: Entity<HomeView>,
    log: Entity<MessageLog>,
    pub(crate) minibuffer: Option<Minibuffer>,
    menu: Option<OpenMenu>,
    menu_focus: FocusHandle,
    /// Where the keyboard was before a menu or a prompt took it.
    overlay_return: Option<FocusHandle>,
    echo: Option<Echo>,
    /// The text style for a surface that is no buffer.
    chrome_editor: Entity<Editor>,
    mode_indicator: Entity<vim::ModeIndicator>,
    pub(crate) signals: Signals,
    pub(crate) chime: Rc<dyn Fn()>,
    /// What `find:` listed last, so the chosen title opens its address.
    found: Vec<Found>,
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
        let home = cx.new(|cx| HomeView::new(window, cx));
        let log = cx.new(|cx| MessageLog::new(window, cx));
        let chrome_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor
        });
        let mode_indicator = cx.new(|cx| vim::ModeIndicator::new(window, cx));
        let home_address = Self::home_address();
        let mut surfaces = HashMap::new();
        surfaces.insert(home_address.clone(), AnySurface::new(home.clone()));
        surfaces.insert(Self::log_address(), AnySurface::new(log.clone()));
        // A menu owns its keys before gpui resolves bindings: vim binds `g`
        // as a prefix, and an ordinary key handler would not see a menu's
        // one-stroke `g` until another key arrived.
        let menu_keys = cx.listener(|this, event: &gpui::KeystrokeEvent, window, cx| {
            if this.menu.is_some() && this.menu_focus.is_focused(window) {
                this.menu_keystroke(&event.keystroke, window, cx);
            }
        });
        let menu_keys = cx.intercept_keystrokes(menu_keys);
        let mut frame = Self {
            db,
            hosts: Hosts::new(Arc::new(host_sink)),
            systems,
            attention,
            history: History::new(home_address, ()),
            surfaces,
            home,
            log,
            minibuffer: None,
            menu: None,
            menu_focus: cx.focus_handle(),
            overlay_return: None,
            echo: None,
            chrome_editor,
            mode_indicator,
            signals: Signals::default(),
            chime,
            found: Vec::new(),
            _subscriptions: vec![menu_keys],
            _tasks: Vec::new(),
        };
        let ledger = cx.spawn(async move |this, cx| {
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
        frame.systems_changed(cx);
        frame.focus_surface(window, cx);
        frame
    }

    pub fn home_address() -> Address {
        Address::new(FRAME_KIND, "home")
    }

    pub fn log_address() -> Address {
        Address::new(FRAME_KIND, "log")
    }

    fn ledger_event(&mut self, event: LedgerEvent, cx: &mut Context<Self>) {
        let (touched, said) = self.attention.ledger_event(event);
        for line in said {
            self.log(line, StyleClass::SystemInfo, cx);
        }
        self.marks_moved(touched, cx);
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
        let subject = match self.current_surface().subject(cx) {
            Subject::Own if surface.kind() == FRAME_KIND => None,
            Subject::Own => Some(surface.clone()),
            Subject::Address(address) => Some(address),
            Subject::None => None,
        };
        Point { surface, subject }
    }

    /// The card a verdict lands on: what the point is on.
    pub fn card_target(&self, cx: &App) -> Option<Address> {
        self.point(cx).subject
    }

    /// The card the surface in view stands for, which is nothing on Home:
    /// Home is a list, so a pull from it opens the top card rather than
    /// passing over the row.
    pub fn card_in_view(&self, cx: &App) -> Option<Address> {
        match self.current_address().kind() == FRAME_KIND {
            true => None,
            false => self.card_target(cx),
        }
    }

    pub fn home_in_view(&self) -> bool {
        *self.current_address() == Self::home_address()
    }

    /// Opens `address`: the system that owns its kind makes the surface,
    /// or the one already made is shown again.
    pub fn open(&mut self, address: Address, window: &mut Window, cx: &mut Context<Self>) {
        self.open_with(address, rho_journal::SurfaceShowMethod::Open, window, cx);
    }

    pub(crate) fn open_with(
        &mut self,
        address: Address,
        method: rho_journal::SurfaceShowMethod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.surfaces.contains_key(&address) {
            let opened = self
                .systems
                .clone()
                .into_iter()
                .find(|system| system.opens(&address, cx))
                .and_then(|system| system.open(&address, window, cx));
            let Some(surface) = opened else {
                self.echo(
                    &format!("nothing opens {address}"),
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            };
            self.surfaces.insert(address.clone(), surface);
        }
        rho_journal::record(rho_journal::Event::SurfaceShown {
            surface: journal_surface(&address),
            method,
        });
        self.history.show(address, ());
        self.close_overlays(window, cx);
        self.focus_surface(window, cx);
        cx.notify();
    }

    pub fn open_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open(Self::home_address(), window, cx);
    }

    pub fn open_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open(Self::log_address(), window, cx);
    }

    /// Closes `address`: its surface goes, and no walk reaches it again.
    /// Home and the log only step out of view.
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
                self.history.show(Self::home_address(), ());
            }
            // A forgotten entry under the cursor is left on the first step
            // either way; a frame surface closes by stepping off it.
            if address.kind() == FRAME_KIND && self.current_address() == address {
                self.history.show(Self::home_address(), ());
            }
            self.close_overlays(window, cx);
            self.focus_surface(window, cx);
        }
        cx.notify();
    }

    pub fn close_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let address = self.current_address().clone();
        self.close(&address, window, cx);
    }

    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.back().is_none() {
            self.echo("at the oldest", StyleClass::SystemInfo, cx);
            return;
        }
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
        let handle = self.current_surface().focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// The surface for `address` while it is open, for a system that wants
    /// to tell its own view something.
    pub fn surface(&self, address: &Address) -> Option<&AnySurface> {
        self.surfaces.get(address)
    }

    // ---- echo and log ----

    /// One line on the status line for a few seconds, and in the log for
    /// good.
    pub fn echo(&mut self, text: &str, class: StyleClass, cx: &mut Context<Self>) {
        self.log(text.to_owned(), class, cx);
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

    pub fn log(&mut self, text: String, class: StyleClass, cx: &mut Context<Self>) {
        self.log.update(cx, |log, cx| log.append(text, class, cx));
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
    fn menu_dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.menu.as_mut() else {
            return;
        };
        match open.under.pop() {
            Some(parent) => {
                open.transient = parent.transient();
                open.rows = parent;
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
            rho_window::transient::Press::Count(_) => cx.notify(),
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
            action: format!("{} › {}", open.rows.title, row.description),
            count,
        });
        match row.pick {
            Pick::Menu(rows) => {
                let parent = std::mem::replace(&mut open.rows, rows);
                open.under.push(parent);
                open.transient = open.rows.transient();
                open.carried_count = count;
                cx.notify();
            }
            Pick::Run(run) => {
                if closes {
                    self.close_menu(window, cx);
                }
                run(self, count, window, cx);
                cx.notify();
            }
            Pick::Infix(run) => {
                run(self, count, window, cx);
                cx.notify();
            }
        }
    }

    /// `space`: the frame's rows, the surface's, and every system's for
    /// what is in view.
    fn root_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let point = self.point(cx);
        let mut rows = Rows::new("rho");
        rows.push(Row::run("h", "home", |frame, window, cx| {
            frame.open_home(window, cx)
        }));
        rows.push(Row::run("m", "messages", |frame, window, cx| {
            frame.open_log(window, cx)
        }));
        rows.push(Row::run("f", "find", |frame, window, cx| {
            frame.find(window, cx)
        }));
        rows.push(Row::run("u", "undo", |frame, window, cx| {
            frame.undo_verdict(window, cx)
        }));
        rows.push(Row::menu("H", "hosts", self.hosts_menu()));
        rows.push(Row::run("q", "quit", |_, _, cx| cx.quit()));
        rows.extend(self.current_surface().clone().rows(window, cx));
        for system in self.systems.clone() {
            rows.extend(system.rows(&point, window, cx));
        }
        self.open_menu(rows, window, cx);
    }

    fn hosts_menu(&self) -> Rows {
        let mut rows = Rows::new("hosts");
        rows.push(Row::run("a", "attach", |frame, window, cx| {
            frame.open_prompt(
                "attach host (name=unix:/path or name=iroh:...):",
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
                        .filter(|host| host.name.contains(needle))
                        .map(|host| Candidate {
                            value: host.name.clone(),
                            description: format!("{:?}", host.status),
                        })
                        .collect()
                }),
                Rc::new(|frame, input, _, cx| {
                    match frame.hosts.by_name(input.trim()).map(|host| host.id) {
                        Some(host) => frame.detach_host(host, cx),
                        None => frame.echo("detach: no such host", StyleClass::SystemInfo, cx),
                    }
                }),
                window,
                cx,
            );
        }));
        for (index, host) in self.hosts.iter().enumerate().take(9) {
            let id = host.id;
            rows.push(Row::run(
                (index + 1).to_string(),
                format!("{} · {}", host.name, status_words(&host.status)),
                move |frame, _, cx| {
                    let label = frame.hosts.host_label(id);
                    let status = frame
                        .hosts
                        .get(id)
                        .map(|host| status_words(&host.status))
                        .unwrap_or_default();
                    frame.echo(&format!("{label}: {status}"), StyleClass::SystemInfo, cx);
                },
            ));
        }
        rows
    }

    /// `tab`: the verdicts, over whatever card is in view. With no card
    /// it goes Home, and from Home it is the way back.
    fn verdict_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            self.close_menu(window, cx);
            self.toggle_home(window, cx);
            return;
        }
        let Some(target) = self.card_target(cx) else {
            self.toggle_home(window, cx);
            return;
        };
        let card = self.card_for(&target, cx);
        let mut rows = Rows::new(card.heading()).counted();
        let on = |run: fn(&mut Frame, &Address, Option<u32>, &mut Window, &mut Context<Frame>)| {
            let target = target.clone();
            move |frame: &mut Frame,
                  count: Option<u32>,
                  window: &mut Window,
                  cx: &mut Context<Frame>| { run(frame, &target, count, window, cx) }
        };
        rows.push(Row::counted(
            "d",
            "done",
            on(|frame, target, _, window, cx| {
                frame.verdict(target, Verdict::Done, "done".to_owned(), window, cx)
            }),
        ));
        rows.push(Row::counted(
            "m",
            "mute",
            on(|frame, target, _, window, cx| {
                frame.verdict(target, Verdict::Mute, "muted".to_owned(), window, cx)
            }),
        ));
        rows.push(Row::counted(
            "t",
            "todo (count: days from)",
            on(|frame, target, count, window, cx| frame.todo(target, count, window, cx)),
        ));
        let mut snooze = Rows::new("snooze for").counted();
        for (key, description, unit) in [
            ("m", "minutes", SnoozeUnit::Minutes),
            ("h", "hours", SnoozeUnit::Hours),
            ("d", "days", SnoozeUnit::Days),
            ("w", "weeks", SnoozeUnit::Weeks),
        ] {
            let target = target.clone();
            snooze.push(Row::counted(
                key,
                description,
                move |frame, count, window, cx| frame.snooze(&target, unit, count, window, cx),
            ));
        }
        rows.push(Row::menu("s", "snooze", snooze));
        rows.push(Row::counted(
            "p",
            "pile",
            on(|frame, target, _, window, cx| frame.prompt_pile(target.clone(), window, cx)),
        ));
        rows.push(Row::counted(
            "f",
            "file under a label",
            on(|frame, target, _, window, cx| frame.prompt_file(target.clone(), window, cx)),
        ));
        rows.push(Row::counted("u", "undo", |frame, _, window, cx| {
            frame.undo_verdict(window, cx)
        }));
        rows.push(Row::counted("n", "next card", |frame, _, window, cx| {
            frame.pull_card(window, cx)
        }));
        self.open_menu(rows, window, cx);
    }

    fn toggle_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.home_in_view() {
            true => self.back(window, cx),
            false => self.open_home(window, cx),
        }
    }

    /// `p`: the card goes on a pile the user names, a new one or one they
    /// already have, and stays there until they open it from Home.
    fn prompt_pile(&mut self, target: Address, window: &mut Window, cx: &mut Context<Self>) {
        let complete = Rc::new(|frame: &Frame, input: &str, _: &App| {
            let typed = input.trim();
            let needle = typed.to_lowercase();
            let mut candidates: Vec<Candidate> = frame
                .attention
                .marks
                .piles(jiff::Timestamp::now())
                .into_iter()
                .filter_map(|pile| {
                    let name = pile.name?;
                    name.to_lowercase().contains(&needle).then(|| Candidate {
                        value: name,
                        description: format!("{} cards", pile.nodes.len()),
                    })
                })
                .collect();
            if !typed.is_empty() && candidates.iter().all(|candidate| candidate.value != typed) {
                candidates.push(Candidate {
                    value: typed.to_owned(),
                    description: "new pile".to_owned(),
                });
            }
            candidates
        });
        self.open_prompt(
            "pile:",
            complete,
            Rc::new(move |frame, input, window, cx| {
                let name = input.trim().to_owned();
                if name.is_empty() {
                    frame.echo("pile: no name", StyleClass::SystemInfo, cx);
                    return;
                }
                frame.verdict(
                    &target,
                    Verdict::Pile(name.clone()),
                    format!("piled on {name}"),
                    window,
                    cx,
                );
            }),
            window,
            cx,
        );
    }

    /// `f`: the card under a label, by path; a path nobody made yet is made.
    fn prompt_file(&mut self, target: Address, window: &mut Window, cx: &mut Context<Self>) {
        let complete = Rc::new(|frame: &Frame, input: &str, _: &App| {
            let needle = input.trim().to_lowercase();
            frame
                .attention
                .marks
                .labels()
                .into_iter()
                .filter(|(_, path)| path.to_lowercase().contains(&needle))
                .map(|(_, path)| Candidate {
                    value: path,
                    description: "label".to_owned(),
                })
                .collect()
        });
        self.open_prompt(
            "file under:",
            complete,
            Rc::new(move |frame, input, _, cx| frame.toggle_label(&target, &input, cx)),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    /// `find:` everything every system has, by title.
    pub fn find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut found = Vec::new();
        for system in &self.systems {
            found.extend(system.find(cx));
        }
        found.sort_by_key(|found| std::cmp::Reverse(found.recency));
        self.found = found;
        self.open_prompt(
            "find:",
            Rc::new(|frame: &Frame, input: &str, _: &App| {
                let needle = input.trim().to_lowercase();
                frame
                    .found
                    .iter()
                    .filter(|found| {
                        found.title.to_lowercase().contains(&needle)
                            || found.description.to_lowercase().contains(&needle)
                    })
                    .take(40)
                    .map(|found| Candidate {
                        value: found.title.clone(),
                        description: found.description.clone(),
                    })
                    .collect()
            }),
            Rc::new(|frame, input, window, cx| {
                let input = input.trim();
                let address = frame
                    .found
                    .iter()
                    .find(|found| found.title == input)
                    .map(|found| found.address.clone())
                    .or_else(|| Address::parse(input));
                match address {
                    Some(address) => frame.open(address, window, cx),
                    None => frame.echo(
                        &format!("find: nothing called {input}"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
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
        let prompt = prompt.into();
        rho_journal::record(rho_journal::Event::MinibufferOpened {
            prompt: prompt.to_string(),
        });
        self.menu = None;
        self.take_keyboard(window, cx);
        let text_style = self
            .active_editor(cx)
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let mut minibuffer = Minibuffer::open(
            prompt,
            &text_style,
            complete,
            None,
            on_submit,
            on_cancel,
            window,
            cx,
        );
        minibuffer.refresh(self, cx);
        self.minibuffer = Some(minibuffer);
        // The strip is single-occupancy; a stale message after the prompt
        // closes would confuse.
        self.echo = None;
        cx.notify();
    }

    pub(crate) fn refresh_minibuffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut minibuffer) = self.minibuffer.take() else {
            return;
        };
        minibuffer.refresh(self, cx);
        let on_change = minibuffer.on_change();
        let input = minibuffer.input(cx);
        self.minibuffer = Some(minibuffer);
        if let Some(on_change) = on_change {
            on_change(self, &input, window, cx);
        }
        cx.notify();
    }

    fn minibuffer_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut minibuffer) = self.minibuffer.take() else {
            return;
        };
        let prompt = minibuffer.prompt().to_owned();
        minibuffer.accept_selected(window, cx);
        let secret = minibuffer.is_secret();
        let (input, on_submit) = minibuffer.into_submission(cx);
        rho_journal::record(rho_journal::Event::MinibufferSubmitted {
            prompt,
            input: match secret {
                true => String::new(),
                false => input.clone(),
            },
        });
        self.give_keyboard_back(window, cx);
        on_submit(self, input, window, cx);
        cx.notify();
    }

    fn minibuffer_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    /// Enter on Home: the row under the cursor.
    fn open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.home.read(cx).cursor_target(cx) {
            HomeTarget::Address(address) => self.open(address, window, cx),
            HomeTarget::Pile(name) => self.open_pile(name, window, cx),
            HomeTarget::None => {}
        }
    }

    // ---- render ----

    fn render_status_line(
        &mut self,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        // An echo is the answer to the key just pressed, so it takes the
        // line while it lasts. Otherwise the line is about what is in view:
        // a card says which card and why; anything else says its name.
        let left: AnyElement = match (&self.echo, self.card_in_view(cx)) {
            (Some(echo), _) => div()
                .text_color(status.info)
                .child(echo.text().to_owned())
                .into_any_element(),
            (None, Some(address)) => {
                let card = self.card_for(&address, cx);
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(truncate_path(&card.heading()))
                    .child(div().text_color(status.warning).child(card.label.clone()))
                    .into_any_element()
            }
            (None, None) => div()
                .child(truncate_path(&self.current_surface().title(cx)))
                .into_any_element(),
        };
        let mode = self
            .mode_indicator
            .read(cx)
            .plain_mode(cx)
            .unwrap_or_else(|| "normal".to_owned());
        let mode_color = match mode.contains("insert") {
            true => status.warning,
            false => colors.terminal_ansi_bright_cyan,
        };
        let lamp = self
            .signals
            .lamp_on
            .then(|| div().text_color(status.warning).child("●"));
        let unwell = self.unwell_hosts();
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .children(lamp)
            .children(unwell.map(|text| div().text_color(status.error).child(text)))
            .child(div().text_color(mode_color).child(mode_word(&mode)));
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
        let subject = match self.hosts.len() > 1 {
            true => format!("{name} "),
            false => String::new(),
        };
        match status {
            HostStatus::Online => None,
            status => Some(format!("{subject}{}", status_words(&status))),
        }
    }
}

fn status_words(status: &HostStatus) -> String {
    match status {
        HostStatus::Connecting => "connecting".to_owned(),
        HostStatus::Online => "online".to_owned(),
        HostStatus::Recovering(elapsed) => format!("reconnecting · {}s", elapsed.as_secs()),
        HostStatus::Disconnected(reason) => format!("disconnected · {reason}"),
    }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.active_editor(cx);
        let text_style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
        let surface = self.current_surface().clone();
        div()
            .id("rho-frame")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .text_color(cx.theme().colors().text)
            .key_context(crate::KEY_CONTEXT)
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
            .on_action(
                cx.listener(|this, _: &keys::RootMenu, window, cx| this.root_menu(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &keys::VerdictMenu, window, cx| {
                    this.verdict_menu(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &keys::Home, window, cx| this.toggle_home(window, cx)))
            .on_action(cx.listener(|this, _: &keys::OpenRow, window, cx| this.open_row(window, cx)))
            .on_action(cx.listener(|this, _: &keys::FindAddress, window, cx| this.find(window, cx)))
            .on_action(
                cx.listener(|this, _: &keys::UndoVerdict, window, cx| {
                    this.undo_verdict(window, cx)
                }),
            )
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
                cx.listener(|this, _: &keys::MinibufferComplete, window, cx| {
                    this.minibuffer_complete(window, cx)
                }),
            )
            .child(self.render_status_line(&text_style, cx))
            .child(div().flex_1().min_h_0().w_full().child(surface.view()))
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
                                        let closes = this.menu.as_ref().is_some_and(|open| {
                                            open.transient.items()[index].kind()
                                                == rho_window::transient::Kind::Suffix
                                        });
                                        let count =
                                            this.menu.as_ref().and_then(|open| open.carried_count);
                                        this.run_menu_row(index, count, closes, window, cx);
                                        cx.stop_propagation();
                                    }))
                                    .into_any_element()
                            },
                        )),
                    )
            }))
            .children(
                self.minibuffer
                    .as_ref()
                    .map(|minibuffer| minibuffer.render(&text_style, cx)),
            )
    }
}
