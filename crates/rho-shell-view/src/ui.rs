//! Shells as the frame shows them.
//!
//! [`ShellSystem`] owns the `shell` kind: an agent has one host-owned
//! shell, and its [`ShellAddress`] opens a [`ShellSurface`] over it,
//! starting the shell when none runs. The palette offers the running
//! shells and closes the one in view; `find` lists the shells open here.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, KeyBinding, WeakEntity, Window, div};
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_dealer::Address;
use rho_frame::keys::Close;
use rho_frame::{
    AnySurface, Candidate, CandidateSource, Found, FrameLink, Marks, Point, Row, Surface, System,
};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::protocol::PagerAction;
use crate::{
    ShellEof, ShellInterrupt, ShellModel, ShellPagerAll, ShellPagerMore, ShellPagerQuit,
    ShellSubmit, channel,
};

/// An agent's shell wherever it is: the host by the name the user gave
/// it and the agent whose workset runs it. Its text form is
/// `shell:<host>/<agent>`
/// ([SPEC-shell-address](../specs/SPEC-shell-address.md)).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ShellAddress {
    host: String,
    agent: String,
}

impl ShellAddress {
    pub const KIND: &str = "shell";

    /// `None` unless there is a host name and an agent with no `/` in it.
    pub fn new(host: String, agent: String) -> Option<Self> {
        (!host.is_empty() && !agent.is_empty() && !agent.contains('/'))
            .then_some(Self { host, agent })
    }

    /// Reads the rest of a `shell:` address; the agent is read from the
    /// right, since a host name may hold `/`.
    pub fn parse(rest: &str) -> Option<Self> {
        let (host, agent) = rest.rsplit_once('/')?;
        Self::new(host.to_owned(), agent.to_owned())
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
}

impl fmt::Display for ShellAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.host, self.agent)
    }
}

/// One attached host: what the user calls it and how to reach it.
struct Slot {
    name: String,
    link: Link,
}

pub struct ShellSystem {
    hosts: HashMap<HostId, Slot>,
    /// The shell surfaces open in this window, for `find`; a closed one
    /// drops out by itself.
    surfaces: HashMap<ShellAddress, WeakEntity<ShellSurface>>,
    frame: FrameLink,
}

impl ShellSystem {
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

    /// `shell:` over every running shell; any agent can be typed, and its
    /// shell starts.
    fn prompt_open(&mut self, cx: &mut Context<Self>) {
        let asks: Vec<_> = self
            .hosts
            .values()
            .map(|slot| (slot.name.clone(), channel::list(&slot.link)))
            .collect();
        cx.spawn(async move |this, cx| {
            let mut candidates = Vec::new();
            let mut failed = Vec::new();
            for (host, ask) in asks {
                match ask.await {
                    Ok(shells) => candidates.extend(shells.into_iter().map(|shell| Candidate {
                        value: format!("{host}/{}", shell.agent),
                        description: match shell.clients {
                            0 => "detached".to_owned(),
                            n => format!("{n} attached"),
                        },
                    })),
                    Err(error) => failed.push(format!("shell: {host}: {error:#}")),
                }
            }
            candidates.sort_by(|a, b| a.value.cmp(&b.value));
            let _ = this.update(cx, |this, cx| {
                for line in failed {
                    this.frame.log(line, StyleClass::SystemInfo, cx);
                }
                let hosts = this.host_names();
                this.frame.with(cx, move |frame, window, cx| {
                    frame.open_prompt(
                        "shell:",
                        filtered(candidates),
                        Rc::new(move |frame, input, window, cx| {
                            match shell_for(input.trim(), &hosts) {
                                Some(shell) => frame.open(shell.address(), window, cx),
                                None => frame.echo(
                                    &format!("shell: say host/agent, not {:?}", input.trim()),
                                    StyleClass::SystemInfo,
                                    cx,
                                ),
                            }
                        }),
                        window,
                        cx,
                    );
                });
            });
        })
        .detach();
    }

    /// Stops the agent's shell on its host, and closes its surface once
    /// the host has.
    fn close_shell(&mut self, shell: ShellAddress, cx: &mut Context<Self>) {
        let Some(link) = self.link(&shell.host) else {
            self.frame.echo(
                format!("shell: no host called {}", shell.host),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let closed = channel::close(&link, shell.agent.clone());
        cx.spawn(async move |this, cx| {
            let closed = closed.await;
            let _ = this.update(cx, |this, cx| match closed {
                Ok(()) => {
                    this.frame
                        .echo(format!("shell {shell} closed"), StyleClass::SystemInfo, cx);
                    this.frame.close(shell.address(), cx);
                }
                Err(error) => this.frame.echo(
                    format!("close shell: {error:#}"),
                    StyleClass::SystemInfo,
                    cx,
                ),
            });
        })
        .detach();
    }
}

/// `host/agent`, or a bare agent when one host is attached.
fn shell_for(input: &str, hosts: &[String]) -> Option<ShellAddress> {
    if let Some(shell) = ShellAddress::parse(input)
        && hosts.contains(&shell.host)
    {
        return Some(shell);
    }
    match hosts {
        [host] => ShellAddress::new(host.clone(), input.to_owned()),
        _ => None,
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

impl System for ShellSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[ShellAddress::KIND]
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let shell = ShellAddress::of(address)?;
        let link = self.link(&shell.host);
        let many_hosts = self.hosts.len() > 1;
        let surface = cx.new(|cx| ShellSurface::new(shell.clone(), link, many_hosts, window, cx));
        self.surfaces
            .retain(|_, surface| surface.upgrade().is_some());
        self.surfaces.insert(shell, surface.downgrade());
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
        let mut rows = vec![Row::run("x", "shell…", move |_, _, cx| {
            open.update(cx, |system, cx| system.prompt_open(cx));
        })];
        if let Some(here) = point.subject.as_ref().and_then(ShellAddress::of) {
            rows.push(Row::run("shift-x", "close shell", move |_, _, cx| {
                let here = here.clone();
                this.update(cx, |system, cx| system.close_shell(here, cx));
            }));
        }
        rows
    }

    fn find(&self, _marks: &Marks, cx: &App) -> Vec<Found> {
        self.surfaces
            .iter()
            .filter_map(|(shell, surface)| {
                let surface = surface.upgrade()?;
                Some(Found {
                    address: shell.address(),
                    title: format!("shell {shell}"),
                    description: surface.read(cx).status().to_owned(),
                    recency: 0,
                })
            })
            .collect()
    }
}

/// What a shell surface stands on.
enum State {
    /// Dropping the dial cancels it.
    Dialing {
        _dial: gpui::Task<()>,
    },
    Open {
        model: Entity<ShellModel>,
        editor: Entity<Editor>,
    },
    Failed(String),
}

/// A shell as a surface: its transcript and pending command once the host
/// has attached it, and until then what is happening.
pub struct ShellSurface {
    shell: ShellAddress,
    many_hosts: bool,
    state: State,
    focus: FocusHandle,
}

impl ShellSurface {
    fn new(
        shell: ShellAddress,
        link: Option<Link>,
        many_hosts: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = match link {
            None => State::Failed(format!("no host called {} is attached", shell.host)),
            Some(link) => {
                let dial = channel::open(&link, shell.agent.clone());
                State::Dialing {
                    _dial: cx.spawn_in(window, async move |this, cx| {
                        let dialed = dial.await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.state = match dialed {
                                Ok(channel) => {
                                    let model = cx.new(|cx| ShellModel::new(channel, cx));
                                    let editor = model
                                        .update(cx, |model, cx| model.build_editor(window, cx));
                                    // The keyboard was on the placeholder; it
                                    // moves to the shell it stood for.
                                    if this.focus.is_focused(window) {
                                        window.focus(&editor.focus_handle(cx), cx);
                                    }
                                    State::Open { model, editor }
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
            shell,
            many_hosts,
            state,
            focus: cx.focus_handle(),
        }
    }

    pub fn address(&self) -> &ShellAddress {
        &self.shell
    }

    fn status(&self) -> &str {
        match &self.state {
            State::Dialing { .. } => "connecting",
            State::Open { .. } => "shell",
            State::Failed(error) => error,
        }
    }

    fn model(&self) -> Option<&Entity<ShellModel>> {
        match &self.state {
            State::Open { model, .. } => Some(model),
            State::Dialing { .. } | State::Failed(_) => None,
        }
    }

    fn submit(&mut self, _: &ShellSubmit, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = self.model() {
            model.update(cx, |model, cx| model.submit(cx));
        }
    }

    fn interrupt(&mut self, _: &ShellInterrupt, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = self.model() {
            model.update(cx, |model, _| model.interrupt());
        }
    }

    fn eof(&mut self, _: &ShellEof, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = self.model() {
            model.update(cx, |model, cx| model.eof(cx));
        }
    }

    fn pager(&mut self, action: PagerAction, cx: &mut Context<Self>) {
        if let Some(model) = self.model() {
            model.read(cx).pager_action(action);
        }
    }
}

impl Surface for ShellSurface {
    fn title(&self, _cx: &App) -> String {
        match self.many_hosts {
            true => format!("{} › {} · shell", self.shell.host, self.shell.agent),
            false => format!("{} · shell", self.shell.agent),
        }
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        match &self.state {
            State::Open { editor, .. } => Some(editor.clone()),
            State::Dialing { .. } | State::Failed(_) => None,
        }
    }
}

impl Focusable for ShellSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            State::Open { editor, .. } => editor.focus_handle(cx),
            State::Dialing { .. } | State::Failed(_) => self.focus.clone(),
        }
    }
}

impl Render for ShellSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let root = div()
            .key_context("RhoShell")
            .size_full()
            .bg(colors.editor_background)
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::interrupt))
            .on_action(cx.listener(Self::eof))
            .on_action(
                cx.listener(|this, _: &ShellPagerMore, _, cx| {
                    this.pager(PagerAction::Continue, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ShellPagerAll, _, cx| this.pager(PagerAction::Drain, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShellPagerQuit, _, cx| this.pager(PagerAction::Quit, cx)),
            );
        let note = match &self.state {
            State::Open { editor, .. } => return root.child(editor.clone()),
            State::Dialing { .. } => format!("connecting to shell {}…", self.shell),
            State::Failed(error) => format!("shell {}: {error}", self.shell),
        };
        root.child(
            div()
                .key_context("RhoShellNote")
                .track_focus(&self.focus)
                .size_full()
                .p_2()
                .text_color(colors.text_muted)
                .child(note),
        )
    }
}

fn bind_keys(cx: &mut App) {
    // The shell is an editor with vim keys: insert mode types the command,
    // and enter in it sends it.
    let insert = "RhoShell > Editor && vim_mode == insert";
    cx.bind_keys([
        KeyBinding::new("enter", ShellSubmit, Some(insert)),
        KeyBinding::new("ctrl-c", ShellInterrupt, Some(insert)),
        KeyBinding::new("ctrl-d", ShellEof, Some(insert)),
        KeyBinding::new("alt-enter", ShellPagerMore, Some("RhoShell > Editor")),
        KeyBinding::new("alt-a", ShellPagerAll, Some("RhoShell > Editor")),
        KeyBinding::new("alt-q", ShellPagerQuit, Some("RhoShell > Editor")),
        // Before the shell is attached there is no editor: `q` still closes.
        KeyBinding::new("q", Close, Some("RhoShellNote")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_reads_back_with_a_slash_in_the_host() {
        let shell = ShellAddress::new("lab/east".into(), "eng-ht08".into()).unwrap();
        assert_eq!(shell.address().key(), "shell:lab/east/eng-ht08");
        let read = ShellAddress::of(&Address::parse("shell:lab/east/eng-ht08").unwrap()).unwrap();
        assert_eq!(read, shell);
        assert_eq!(read.host(), "lab/east");
        assert_eq!(read.agent(), "eng-ht08");
    }

    #[test]
    fn an_address_needs_a_host_and_an_agent() {
        for rest in ["eng-1", "/eng-1", "host/"] {
            assert_eq!(ShellAddress::parse(rest), None, "{rest}");
        }
        assert_eq!(ShellAddress::of(&Address::new("term", "host/eng-1")), None);
    }

    #[test]
    fn a_bare_agent_needs_one_host() {
        let one = ["local".to_owned()];
        let two = ["local".to_owned(), "lab/east".to_owned()];
        assert_eq!(
            shell_for("eng-1", &one),
            ShellAddress::new("local".into(), "eng-1".into())
        );
        assert_eq!(shell_for("eng-1", &two), None);
        assert_eq!(
            shell_for("lab/east/eng-1", &two),
            ShellAddress::new("lab/east".into(), "eng-1".into())
        );
        assert_eq!(shell_for("nowhere/eng-1", &two), None, "an unknown host");
    }
}
