//! Desktops as the frame shows them.
//!
//! [`DesktopSystem`] owns the `desktop` kind. It keeps each host's
//! desktops stream, so it knows every desktop the host's worksets run
//! whether or not one is in view; `find` and the root menu list them. A
//! [`DesktopAddress`] opens a [`DesktopSurface`], which dials the live
//! view; only an iroh host carries one.

mod view;

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, KeyBinding, Task, Window, div};
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_dealer::Address;
use rho_frame::keys::{Close, RootMenu};
use rho_frame::{AnySurface, Candidate, Found, FrameLink, Marks, Point, Row, Surface, System};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;
pub use view::DesktopView;

use crate::protocol::DesktopSession;
use crate::stream::{DesktopStreams, DesktopsEvent};

/// One desktop wherever it is: the host by the name the user gave it, the
/// agent whose workset runs it, and its name there. Its text form is
/// `desktop:<host>/<agent>/<name>`
/// ([SPEC-desktop-address](../../specs/SPEC-desktop-address.md)).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DesktopAddress {
    host: String,
    agent: String,
    name: String,
}

impl DesktopAddress {
    pub const KIND: &str = "desktop";

    /// `None` unless there is a host name, and an agent and a name with no
    /// `/` in them.
    pub fn new(host: String, agent: String, name: String) -> Option<Self> {
        let word = |part: &str| !part.is_empty() && !part.contains('/');
        (!host.is_empty() && word(&agent) && word(&name)).then_some(Self { host, agent, name })
    }

    /// Reads the rest of a `desktop:` address. A host name may hold `/`;
    /// the agent and the name never do, so they are read from the right.
    pub fn parse(rest: &str) -> Option<Self> {
        let (host_agent, name) = rest.rsplit_once('/')?;
        let (host, agent) = host_agent.rsplit_once('/')?;
        Self::new(host.to_owned(), agent.to_owned(), name.to_owned())
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

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for DesktopAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.host, self.agent, self.name)
    }
}

/// One attached host: what the user calls it, how to reach it, and the
/// desktops it last said it runs.
struct Slot {
    name: String,
    link: Link,
    sessions: Vec<DesktopSession>,
}

pub struct DesktopSystem {
    hosts: HashMap<HostId, Slot>,
    streams: DesktopStreams,
    frame: FrameLink,
    _pump: Task<()>,
}

impl DesktopSystem {
    pub fn new(frame: FrameLink, cx: &mut Context<Self>) -> Self {
        let (streams, mut events) = DesktopStreams::new();
        let pump = cx.spawn(async move |this, cx| {
            while let Some(DesktopsEvent { host, sessions }) = events.next().await {
                if this
                    .update(cx, |this, cx| {
                        if let Some(slot) = this.hosts.get_mut(&host) {
                            slot.sessions = sessions;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        bind_keys(cx);
        Self {
            hosts: HashMap::new(),
            streams,
            frame,
            _pump: pump,
        }
    }

    /// Every desktop every attached host runs, in order.
    pub fn desktops(&self) -> Vec<DesktopAddress> {
        let mut desktops: Vec<DesktopAddress> = self
            .hosts
            .values()
            .flat_map(|slot| {
                slot.sessions.iter().filter_map(|session| {
                    DesktopAddress::new(
                        slot.name.clone(),
                        session.agent.clone(),
                        session.name.clone(),
                    )
                })
            })
            .collect();
        desktops.sort();
        desktops
    }

    fn link(&self, host: &str) -> Option<Link> {
        self.hosts
            .values()
            .find(|slot| slot.name == host)
            .map(|slot| slot.link.clone())
    }

    /// `desktop:` over every desktop the hosts run.
    fn prompt_open(&mut self, cx: &mut Context<Self>) {
        let desktops = self.desktops();
        if desktops.is_empty() {
            self.frame
                .echo("no desktops are running", StyleClass::SystemInfo, cx);
            return;
        }
        let candidates: Vec<Candidate> = desktops
            .iter()
            .map(|desktop| Candidate {
                value: desktop.to_string(),
                description: "desktop".to_owned(),
            })
            .collect();
        self.frame.with(cx, move |frame, window, cx| {
            frame.open_prompt(
                "desktop:",
                Rc::new(move |_, input, _| {
                    let needle = input.trim().to_lowercase();
                    candidates
                        .iter()
                        .filter(|candidate| candidate.value.to_lowercase().contains(&needle))
                        .cloned()
                        .collect()
                }),
                Rc::new(move |frame, input, window, cx| {
                    let input = input.trim();
                    match desktops.iter().find(|desktop| desktop.to_string() == input) {
                        Some(desktop) => frame.open(desktop.address(), window, cx),
                        None => frame.echo(
                            &format!("desktop: none called {input:?}"),
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

impl System for DesktopSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[DesktopAddress::KIND]
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let desktop = DesktopAddress::of(address)?;
        let link = self.link(&desktop.host);
        let many_hosts = self.hosts.len() > 1;
        let surface = cx.new(|cx| DesktopSurface::new(desktop, link, many_hosts, window, cx));
        Some(AnySurface::new(surface))
    }

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        self.hosts.insert(
            host,
            Slot {
                name: name.to_owned(),
                link: link.clone(),
                sessions: Vec::new(),
            },
        );
        vec![self.streams.stream(host)]
    }

    fn detached(&mut self, host: HostId, cx: &mut Context<Self>) {
        self.hosts.remove(&host);
        cx.notify();
    }

    fn rows(&mut self, _point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        if self.hosts.is_empty() {
            return Vec::new();
        }
        let this = cx.entity();
        vec![Row::run("w", "desktop…", move |_, _, cx| {
            this.update(cx, |system, cx| system.prompt_open(cx));
        })]
    }

    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        self.desktops()
            .into_iter()
            .map(|desktop| Found {
                title: format!("desktop {desktop}"),
                description: format!("{}'s desktop on {}", desktop.agent, desktop.host),
                address: desktop.address(),
                recency: 0,
            })
            .collect()
    }
}

/// What a desktop surface stands on.
enum State {
    /// Dropping the dial cancels it.
    Dialing {
        _dial: Task<()>,
    },
    Open(Entity<DesktopView>),
    Failed(String),
}

/// A desktop as a surface: its live view once the host has sent it, and
/// until then what is happening.
pub struct DesktopSurface {
    desktop: DesktopAddress,
    many_hosts: bool,
    state: State,
    focus: FocusHandle,
}

impl DesktopSurface {
    fn new(
        desktop: DesktopAddress,
        link: Option<Link>,
        many_hosts: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = match link {
            None => State::Failed(format!("no host called {} is attached", desktop.host)),
            Some(link) => {
                let dial = crate::viewer::open(&link, desktop.agent.clone(), desktop.name.clone());
                State::Dialing {
                    _dial: cx.spawn_in(window, async move |this, cx| {
                        let dialed = dial.await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.state = match dialed {
                                Ok(viewer) => {
                                    let view = cx.new(|cx| DesktopView::new(viewer, window, cx));
                                    // The keyboard was on the placeholder; it
                                    // moves to the desktop it stood for.
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
            desktop,
            many_hosts,
            state,
            focus: cx.focus_handle(),
        }
    }

    pub fn address(&self) -> &DesktopAddress {
        &self.desktop
    }
}

impl Surface for DesktopSurface {
    fn title(&self, _cx: &App) -> String {
        let title = format!("{} · desktop {}", self.desktop.agent, self.desktop.name);
        match self.many_hosts {
            true => format!("{} › {title}", self.desktop.host),
            false => title,
        }
    }

    fn rows(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let State::Open(view) = &self.state else {
            return Vec::new();
        };
        let description = match view.read(cx).annotating() {
            true => "back to live",
            false => "draw on this frame",
        };
        let view = view.clone();
        vec![Row::run("a", description, move |_, window, cx| {
            view.update(cx, |view, cx| view.toggle_annotation(window, cx));
        })]
    }
}

impl Focusable for DesktopSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            State::Open(view) => view.focus_handle(cx),
            State::Dialing { .. } | State::Failed(_) => self.focus.clone(),
        }
    }
}

impl Render for DesktopSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let note = match &self.state {
            State::Open(view) => return div().size_full().child(view.clone()),
            State::Dialing { .. } => format!("connecting to desktop {}…", self.desktop),
            State::Failed(error) => format!("desktop {}: {error}", self.desktop),
        };
        div()
            .key_context("RhoDesktopNote")
            .track_focus(&self.focus)
            .size_full()
            .p_2()
            .bg(colors.editor_background)
            .text_color(colors.text_muted)
            .child(note)
    }
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // Every key on a live desktop goes to it; the menu is on a chord
        // its applications do not see.
        KeyBinding::new("ctrl-shift-;", RootMenu, Some("RhoDesktop")),
        KeyBinding::new("space", RootMenu, Some("RhoDesktopNote")),
        KeyBinding::new("q", Close, Some("RhoDesktopNote")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_reads_back_with_a_slash_in_the_host() {
        let desktop =
            DesktopAddress::new("lab/east".into(), "eng-ht08".into(), "browser".into()).unwrap();
        assert_eq!(desktop.address().key(), "desktop:lab/east/eng-ht08/browser");
        let read = DesktopAddress::of(&Address::parse(desktop.address().key()).unwrap()).unwrap();
        assert_eq!(read, desktop);
        assert_eq!(
            (read.host(), read.agent(), read.name()),
            ("lab/east", "eng-ht08", "browser")
        );
    }

    #[test]
    fn an_address_needs_every_part() {
        for rest in ["host/eng-1", "/eng-1/desk", "host//desk", "host/eng-1/"] {
            assert_eq!(DesktopAddress::parse(rest), None, "{rest}");
        }
        assert_eq!(
            DesktopAddress::of(&Address::new("term", "host/eng-1/desk")),
            None
        );
    }
}
