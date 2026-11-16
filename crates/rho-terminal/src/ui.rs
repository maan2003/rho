//! Terminals as the frame shows them.
//!
//! [`TerminalSystem`] owns the `term` kind: it knows the attached hosts,
//! opens a [`TerminalSurface`] for a [`TerminalAddress`], offers the root
//! menu's terminal rows, and lists the terminals open here for `find`.
//! Terminals keep no stream to a host: each surface dials its terminal
//! when it opens and drops the attachment when it closes, and the terminal
//! keeps running on the host.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, KeyBinding, NoAction, WeakEntity, Window, div,
};
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_dealer::Address;
use rho_frame::keys::{Close, Home, Palette, VerdictMenu};
use rho_frame::{
    AnySurface, Candidate, CandidateSource, Found, FrameLink, Marks, Point, Row, Surface, System,
};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::protocol::TerminalInfo;
use crate::{
    TerminalModel, TerminalNormalMode, TerminalPaste, TerminalRawMode, TerminalScrollBottom,
    TerminalScrollHalfPageDown, TerminalScrollHalfPageUp, TerminalScrollLineDown,
    TerminalScrollLineUp, TerminalScrollTop, TerminalView, channel,
};

/// One terminal wherever it is: the host by the name the user gave it, the
/// agent whose workset it runs in, and its id among that agent's
/// terminals. Its text form is `term:<host>/<agent>/<id>`
/// ([SPEC-terminal-address](../specs/SPEC-terminal-address.md)).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TerminalAddress {
    host: String,
    agent: String,
    terminal: u64,
}

impl TerminalAddress {
    pub const KIND: &str = "term";

    /// `None` unless every part is there: a host name, and an agent with
    /// no `/` in it.
    pub fn new(host: String, agent: String, terminal: u64) -> Option<Self> {
        (!host.is_empty() && !agent.is_empty() && !agent.contains('/')).then_some(Self {
            host,
            agent,
            terminal,
        })
    }

    /// Reads the rest of a `term:` address. A host name may hold `/`; the
    /// agent and the id never do, so they are read from the right.
    pub fn parse(rest: &str) -> Option<Self> {
        let (host_agent, terminal) = rest.rsplit_once('/')?;
        let (host, agent) = host_agent.rsplit_once('/')?;
        Self::new(host.to_owned(), agent.to_owned(), terminal.parse().ok()?)
    }

    pub fn of(address: &Address) -> Option<Self> {
        (address.kind() == Self::KIND)
            .then(|| Self::parse(address.rest()))
            .flatten()
    }

    pub fn address(&self) -> Address {
        Address::new(Self::KIND, self)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn agent(&self) -> &str {
        &self.agent
    }

    pub fn terminal(&self) -> u64 {
        self.terminal
    }
}

impl fmt::Display for TerminalAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.host, self.agent, self.terminal)
    }
}

/// One attached host: what the user calls it and how to reach it.
struct Slot {
    name: String,
    link: Link,
}

pub struct TerminalSystem {
    hosts: HashMap<HostId, Slot>,
    /// The terminal surfaces open in this window, for `find`; a closed one
    /// drops out by itself.
    surfaces: HashMap<TerminalAddress, WeakEntity<TerminalSurface>>,
    frame: FrameLink,
}

impl TerminalSystem {
    pub fn new(frame: FrameLink, cx: &mut Context<Self>) -> Self {
        bind_keys(cx);
        Self {
            hosts: HashMap::new(),
            surfaces: HashMap::new(),
            frame,
        }
    }

    fn link(&self, host: &str) -> Option<Link> {
        self.hosts
            .values()
            .find(|slot| slot.name == host)
            .map(|slot| slot.link.clone())
    }

    fn host_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.hosts.values().map(|slot| slot.name.clone()).collect();
        names.sort();
        names
    }

    /// Opens an agent's terminal on `host`: with `new` a fresh one, else
    /// the first that runs, spawning one when none does. What the agents
    /// system calls to start a terminal from an agent's workset.
    pub fn open_agent_terminal(
        &mut self,
        host: String,
        agent: String,
        new: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(link) = self.link(&host) else {
            self.frame.echo(
                format!("terminal: no host called {host}"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let running = channel::list(&link, Some(agent.clone()));
        cx.spawn(async move |this, cx| {
            let running = running.await;
            let _ = this.update(cx, |this, cx| {
                let opened = running.map(|running| {
                    let (agent, terminal) = pick_terminal(agent, &running, new);
                    TerminalAddress::new(host, agent, terminal)
                });
                match opened {
                    Ok(Some(terminal)) => this.frame.open(terminal.address(), cx),
                    Ok(None) => {
                        this.frame
                            .echo("terminal: not an agent", StyleClass::SystemInfo, cx)
                    }
                    Err(error) => {
                        this.frame
                            .echo(format!("terminal: {error:#}"), StyleClass::SystemInfo, cx)
                    }
                }
            });
        })
        .detach();
    }

    /// Asks every attached host which terminals run there; `then` hears
    /// them all. A host that does not answer is left out, and the log
    /// says so.
    fn running(
        &self,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Vec<(String, TerminalInfo)>, &mut Context<Self>) + 'static,
    ) {
        let asks: Vec<_> = self
            .hosts
            .values()
            .map(|slot| (slot.name.clone(), channel::list(&slot.link, None)))
            .collect();
        cx.spawn(async move |this, cx| {
            let mut running = Vec::new();
            let mut failed = Vec::new();
            for (host, ask) in asks {
                match ask.await {
                    Ok(infos) => running.extend(infos.into_iter().map(|info| (host.clone(), info))),
                    Err(error) => failed.push(format!("terminal: {host}: {error:#}")),
                }
            }
            running.sort_by(|(host_a, a), (host_b, b)| {
                (host_a, &a.agent, a.terminal_id).cmp(&(host_b, &b.agent, b.terminal_id))
            });
            let _ = this.update(cx, |this, cx| {
                for line in failed {
                    this.frame.log(line, StyleClass::SystemInfo, cx);
                }
                then(this, running, cx);
            });
        })
        .detach();
    }

    /// `terminal:` over every running terminal. A terminal's own address
    /// opens it; an agent (`agent`, or `host/agent` with more than one
    /// host) opens its first, spawning one when none runs.
    fn prompt_open(&mut self, cx: &mut Context<Self>) {
        self.running(cx, |this, running, cx| {
            let candidates = running
                .iter()
                .map(|(host, info)| Candidate {
                    value: format!("{host}/{}/{}", info.agent, info.terminal_id),
                    description: describe(info),
                })
                .collect();
            this.prompt_agent("terminal:", candidates, false, cx);
        });
    }

    /// `new terminal for:` over the agents that run terminals now; any
    /// other agent can be typed.
    fn prompt_new(&mut self, cx: &mut Context<Self>) {
        self.running(cx, |this, running, cx| {
            let mut agents: Vec<(String, String)> = running
                .into_iter()
                .map(|(host, info)| (host, info.agent))
                .collect();
            agents.dedup();
            let candidates = agents
                .into_iter()
                .map(|(host, agent)| Candidate {
                    value: format!("{host}/{agent}"),
                    description: "agent".to_owned(),
                })
                .collect();
            this.prompt_agent("new terminal for:", candidates, true, cx);
        });
    }

    fn prompt_agent(
        &mut self,
        prompt: &'static str,
        candidates: Vec<Candidate>,
        new: bool,
        cx: &mut Context<Self>,
    ) {
        let system = cx.entity();
        let hosts = self.host_names();
        self.frame.with(cx, move |frame, window, cx| {
            frame.open_prompt(
                prompt,
                filtered(candidates),
                Rc::new(move |frame, input, window, cx| {
                    let input = input.trim();
                    if let Some(terminal) = TerminalAddress::parse(input)
                        && !new
                        && hosts.contains(&terminal.host)
                    {
                        frame.open(terminal.address(), window, cx);
                        return;
                    }
                    match host_and_agent(input, &hosts) {
                        Some((host, agent)) => system.update(cx, |system, cx| {
                            system.open_agent_terminal(host, agent, new, cx)
                        }),
                        None => frame.echo(
                            &format!("terminal: say host/agent, not {input:?}"),
                            StyleClass::SystemInfo,
                            cx,
                        ),
                    }
                }),
                window,
                cx,
            );
        });
    }
}

/// Which of an agent's terminals to open: with `new` one past every
/// running one, else the first running, else the first there will be.
/// The agent is named as the host names it when the host listed it, so a
/// prefix the user typed becomes the agent's whole id.
fn pick_terminal(agent: String, running: &[TerminalInfo], new: bool) -> (String, u64) {
    let agent = running.first().map_or(agent, |info| info.agent.clone());
    let terminal = match (new, running.first()) {
        (true, _) => channel::next_terminal_id(running),
        (false, Some(info)) => info.terminal_id,
        (false, None) => 0,
    };
    (agent, terminal)
}

/// `host/agent`, or a bare agent when one host is attached.
fn host_and_agent(input: &str, hosts: &[String]) -> Option<(String, String)> {
    if let Some((host, agent)) = input.rsplit_once('/')
        && hosts.iter().any(|name| name == host)
        && !agent.is_empty()
    {
        return Some((host.to_owned(), agent.to_owned()));
    }
    match hosts {
        [host] if !input.is_empty() && !input.contains('/') => {
            Some((host.clone(), input.to_owned()))
        }
        _ => None,
    }
}

fn describe(info: &TerminalInfo) -> String {
    let clients = match info.clients {
        0 => "detached".to_owned(),
        1 => "1 attached".to_owned(),
        n => format!("{n} attached"),
    };
    match info.title.is_empty() {
        true => format!("{}×{} · {clients}", info.cols, info.rows),
        false => format!("{} · {clients}", info.title),
    }
}

/// Completion over a fixed list, by substring of value or description.
fn filtered(candidates: Vec<Candidate>) -> CandidateSource {
    Rc::new(move |_, input, _| {
        let needle = input.trim().to_lowercase();
        candidates
            .iter()
            .filter(|candidate| {
                candidate.value.to_lowercase().contains(&needle)
                    || candidate.description.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect()
    })
}

impl System for TerminalSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[TerminalAddress::KIND]
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let terminal = TerminalAddress::of(address)?;
        let link = self.link(&terminal.host);
        let many_hosts = self.hosts.len() > 1;
        let surface =
            cx.new(|cx| TerminalSurface::new(terminal.clone(), link, many_hosts, window, cx));
        self.surfaces
            .retain(|_, surface| surface.upgrade().is_some());
        self.surfaces.insert(terminal, surface.downgrade());
        Some(AnySurface::new(surface))
    }

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<std::sync::Arc<dyn HostStream>> {
        self.hosts.insert(
            host,
            Slot {
                name: name.to_owned(),
                link: link.clone(),
            },
        );
        Vec::new()
    }

    fn detached(&mut self, host: HostId, _cx: &mut Context<Self>) {
        self.hosts.remove(&host);
    }

    fn rows(&mut self, point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        if self.hosts.is_empty() {
            return Vec::new();
        }
        let this = cx.entity();
        let open = this.clone();
        let mut rows = vec![Row::run("t", "terminal…", move |_, _, cx| {
            open.update(cx, |system, cx| system.prompt_open(cx));
        })];
        // On a terminal, a new one is for the same agent; elsewhere, ask.
        let here = point.subject.as_ref().and_then(TerminalAddress::of);
        rows.push(Row::run("shift-t", "new terminal", move |_, _, cx| {
            this.update(cx, |system, cx| match &here {
                Some(here) => {
                    system.open_agent_terminal(here.host.clone(), here.agent.clone(), true, cx)
                }
                None => system.prompt_new(cx),
            });
        }));
        rows
    }

    fn find(&self, _marks: &Marks, cx: &App) -> Vec<Found> {
        self.surfaces
            .iter()
            .filter_map(|(terminal, surface)| {
                let surface = surface.upgrade()?;
                Some(Found {
                    address: terminal.address(),
                    title: format!("term {terminal}"),
                    description: surface.read(cx).status(cx),
                    recency: 0,
                })
            })
            .collect()
    }
}

/// What a terminal surface stands on.
enum State {
    /// Dropping the dial cancels it.
    Dialing {
        _dial: gpui::Task<()>,
    },
    Open(Entity<TerminalView>),
    Failed(String),
}

/// A terminal as a surface: its view once the host has attached it, and
/// until then what is happening.
pub struct TerminalSurface {
    terminal: TerminalAddress,
    many_hosts: bool,
    state: State,
    focus: FocusHandle,
}

impl TerminalSurface {
    fn new(
        terminal: TerminalAddress,
        link: Option<Link>,
        many_hosts: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = match link {
            None => State::Failed(format!("no host called {} is attached", terminal.host)),
            // The pty takes its size from the view on its first paint.
            Some(link) => {
                let dial = channel::open(&link, terminal.agent.clone(), terminal.terminal, 80, 24);
                State::Dialing {
                    _dial: cx.spawn_in(window, async move |this, cx| {
                        let dialed = dial.await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.state = match dialed {
                                Ok(channel) => {
                                    let model = cx.new(|cx| TerminalModel::new(channel, cx));
                                    let view = cx.new(|cx| TerminalView::new(model.clone(), cx));
                                    cx.observe(&model, |_, _, cx| cx.notify()).detach();
                                    // The keyboard was on the placeholder; it
                                    // moves to the terminal it stood for.
                                    if this.focus.is_focused(window) {
                                        window.focus(&view.focus_handle(cx), cx);
                                    }
                                    State::Open(view)
                                }
                                Err(error) => State::Failed(format!("{error:#}")),
                            };
                            cx.notify();
                        });
                    }),
                }
            }
        };
        Self {
            terminal,
            many_hosts,
            state,
            focus: cx.focus_handle(),
        }
    }

    pub fn address(&self) -> &TerminalAddress {
        &self.terminal
    }

    /// What the terminal is doing, in a few words.
    fn status(&self, cx: &App) -> String {
        match &self.state {
            State::Dialing { .. } => "connecting".to_owned(),
            State::Failed(error) => error.clone(),
            State::Open(view) => {
                let title = view.read(cx).model().read(cx).title();
                match title.is_empty() {
                    true => "terminal".to_owned(),
                    false => title.to_owned(),
                }
            }
        }
    }
}

impl Surface for TerminalSurface {
    fn title(&self, cx: &App) -> String {
        let mut title = format!("{} · term {}", self.terminal.agent, self.terminal.terminal);
        if self.many_hosts {
            title = format!("{} › {title}", self.terminal.host);
        }
        if let State::Open(view) = &self.state {
            let osc = view.read(cx).model().read(cx).title();
            if !osc.is_empty() {
                title = format!("{title} · {osc}");
            }
        }
        title
    }
}

impl Focusable for TerminalSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            State::Open(view) => view.focus_handle(cx),
            State::Dialing { .. } | State::Failed(_) => self.focus.clone(),
        }
    }
}

impl Render for TerminalSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let note = match &self.state {
            State::Open(view) => return div().size_full().child(view.clone()),
            State::Dialing { .. } => format!("connecting to term {}…", self.terminal),
            State::Failed(error) => format!("term {}: {error}", self.terminal),
        };
        // The placeholder takes the keys of a terminal in normal mode, so
        // `q` closes it and `:` is the palette.
        div()
            .size_full()
            .key_context("RhoTerminalNormal")
            .track_focus(&self.focus)
            .bg(colors.terminal_background)
            .text_color(colors.text_muted)
            .p_2()
            .child(note)
    }
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // Raw mode: every unbound key goes to the pty, so the terminal's
        // own chords are ones shells do not see. `ctrl-\ ctrl-n` is vim's
        // terminal escape; `ctrl-shift-n` the discoverable one.
        KeyBinding::new("ctrl-shift-v", TerminalPaste, Some("RhoTerminal")),
        KeyBinding::new("ctrl-shift-;", Palette, Some("RhoTerminal")),
        KeyBinding::new("ctrl-\\ ctrl-n", TerminalNormalMode, Some("RhoTerminal")),
        KeyBinding::new("ctrl-shift-n", TerminalNormalMode, Some("RhoTerminal")),
        // The frame walks history on ctrl-j and ctrl-k; in a shell they are
        // the shell's.
        KeyBinding::new("ctrl-j", NoAction, Some("RhoTerminal")),
        KeyBinding::new("ctrl-k", NoAction, Some("RhoTerminal")),
        // Normal mode: the keyboard is rho's again; vim keys browse
        // scrollback and insert returns to raw.
        KeyBinding::new("i", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("a", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("enter", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("j", TerminalScrollLineDown, Some("RhoTerminalNormal")),
        KeyBinding::new("k", TerminalScrollLineUp, Some("RhoTerminalNormal")),
        KeyBinding::new("down", TerminalScrollLineDown, Some("RhoTerminalNormal")),
        KeyBinding::new("up", TerminalScrollLineUp, Some("RhoTerminalNormal")),
        KeyBinding::new(
            "ctrl-d",
            TerminalScrollHalfPageDown,
            Some("RhoTerminalNormal"),
        ),
        KeyBinding::new(
            "ctrl-u",
            TerminalScrollHalfPageUp,
            Some("RhoTerminalNormal"),
        ),
        KeyBinding::new("g g", TerminalScrollTop, Some("RhoTerminalNormal")),
        KeyBinding::new("shift-g", TerminalScrollBottom, Some("RhoTerminalNormal")),
        KeyBinding::new(":", Palette, Some("RhoTerminalNormal")),
        KeyBinding::new("space", VerdictMenu, Some("RhoTerminalNormal")),
        KeyBinding::new("tab", Home, Some("RhoTerminalNormal")),
        KeyBinding::new("q", Close, Some("RhoTerminalNormal")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(agent: &str, terminal_id: u64) -> TerminalInfo {
        TerminalInfo {
            agent: agent.to_owned(),
            terminal_id,
            title: String::new(),
            cols: 80,
            rows: 24,
            clients: 0,
        }
    }

    #[test]
    fn an_address_reads_back_with_a_slash_in_the_host() {
        let terminal = TerminalAddress::new("lab/east".into(), "eng-ht08".into(), 3).unwrap();
        let address = terminal.address();
        assert_eq!(address.key(), "term:lab/east/eng-ht08/3");
        let read = TerminalAddress::of(&Address::parse(address.key()).unwrap()).unwrap();
        assert_eq!(read, terminal);
        assert_eq!(read.host(), "lab/east");
    }

    #[test]
    fn an_address_needs_every_part() {
        for rest in [
            "host/eng-1",
            "/eng-1/0",
            "host//0",
            "host/eng-1/x",
            "eng-1/0",
        ] {
            assert_eq!(TerminalAddress::parse(rest), None, "{rest}");
        }
        assert_eq!(
            TerminalAddress::of(&Address::new("shell", "host/eng-1/0")),
            None,
            "another kind"
        );
    }

    #[test]
    fn an_agent_opens_its_first_terminal_or_a_fresh_one() {
        let running = [info("eng-ht08", 2), info("eng-ht08", 5)];
        assert_eq!(
            pick_terminal("eng-ht".into(), &running, false),
            ("eng-ht08".to_owned(), 2),
            "the first running, under the agent's whole id"
        );
        assert_eq!(
            pick_terminal("eng-ht08".into(), &running, true),
            ("eng-ht08".to_owned(), 6),
            "one past the highest, not past the first"
        );
        assert_eq!(
            pick_terminal("eng-x".into(), &[], false),
            ("eng-x".to_owned(), 0)
        );
        assert_eq!(
            pick_terminal("eng-x".into(), &[], true),
            ("eng-x".to_owned(), 0)
        );
    }

    #[test]
    fn a_bare_agent_needs_one_host() {
        let one = ["local".to_owned()];
        let two = ["local".to_owned(), "lab/east".to_owned()];
        assert_eq!(
            host_and_agent("eng-1", &one),
            Some(("local".to_owned(), "eng-1".to_owned()))
        );
        assert_eq!(host_and_agent("eng-1", &two), None);
        assert_eq!(
            host_and_agent("lab/east/eng-1", &two),
            Some(("lab/east".to_owned(), "eng-1".to_owned()))
        );
        assert_eq!(host_and_agent("nowhere/eng-1", &two), None);
    }
}
