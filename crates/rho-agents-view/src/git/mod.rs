//! The user's word on an agent's Git (`product/agents.md`, `flow Approve a
//! push or a fetch`).
//!
//! When an agent pushes to `main` or `master`, or reaches a repository
//! through the user's ssh login, its host asks every window that holds the
//! login and waits for as long as it takes. Here each request that waits is
//! a card, `git:<request>`, dealt like any other: the lamp is its only
//! pull, Desk lists it, and dealing it opens a screen that says exactly
//! what it would do: the work that asks, the repository, each ref it
//! changes, and for a push the commits and diff it carries. The card is
//! answered with `approve` or `deny`, or snoozed, in place of done and
//! mute; nothing pops up over what the user is doing.
//!
//! The requests are the host's: this window keeps only what each host has
//! said waits, and forgets a host's requests when its stream goes, as the
//! host asks again when it is back.

mod surface;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Task, Window};
use jiff::Timestamp;
use rho_agent_hosts::git::{GitAsk, GitChange, GitEvent, GitStreams};
use rho_agent_hosts::protocol::GitDecision;
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_agent_types::{AgentId, EndpointId};
use rho_dealer::curve::{AGENT_BLOCKED_HEAD_START, Curve};
use rho_dealer::rank::{CardKind, Trace};
use rho_dealer::{Address, Addressed, Claim, Marks};
use rho_frame::{AnySurface, FrameLink, Opening, Row, System};
use rho_window::style::StyleClass;
pub use surface::GitAskSurface;

/// What the window knows of an agent, when it knows it.
pub type KnownAgents = Rc<dyn Fn(AgentId, &App) -> Option<KnownAgent>>;

pub struct KnownAgent {
    /// What the window calls it, such as `eng-b8os`.
    pub name: String,
    /// The work it does, as `<project> · <thread subject>`, such as `tide
    /// · Flaky clock test`; `None` when its work has no thread.
    pub work: Option<String>,
}

/// One request, by the id its host gave it. Hosts draw ids at random, so
/// one id names one request on every host and across restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitAskAddress(pub u64);

impl Addressed for GitAskAddress {
    const KIND: &'static str = "git";

    fn rest(&self) -> String {
        format!("{:016x}", self.0)
    }

    fn from_rest(rest: &str) -> Option<Self> {
        (rest.len() == 16)
            .then(|| u64::from_str_radix(rest, 16).ok())
            .flatten()
            .map(Self)
    }
}

/// A request that waits, and the host that asked.
struct Waiting {
    host: HostId,
    ask: GitAsk,
}

/// Every request that waits for the user, as cards.
pub struct GitApprovals {
    streams: GitStreams,
    waiting: BTreeMap<GitAskAddress, Waiting>,
    /// What the user calls each attached host.
    hosts: HashMap<HostId, String>,
    agents: KnownAgents,
    frame: FrameLink,
    _pump: Task<()>,
}

impl GitApprovals {
    pub fn new(
        (streams, mut events): (
            GitStreams,
            futures::channel::mpsc::UnboundedReceiver<GitEvent>,
        ),
        agents: KnownAgents,
        frame: FrameLink,
        cx: &mut Context<Self>,
    ) -> Self {
        let pump = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this.update(cx, |this, cx| this.heard(event, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            streams,
            waiting: BTreeMap::new(),
            hosts: HashMap::new(),
            agents,
            frame,
            _pump: pump,
        }
    }

    /// What a host said about its requests.
    pub(crate) fn heard(&mut self, GitEvent { host, change }: GitEvent, cx: &mut Context<Self>) {
        let name = self.host_name(host);
        match change {
            GitChange::Asked(ask) => {
                self.waiting
                    .insert(GitAskAddress(ask.request_id), Waiting { host, ask });
            }
            GitChange::Ended { request_id } => {
                self.forget(
                    |address, waiting| {
                        waiting.host == host && *address == GitAskAddress(request_id)
                    },
                    cx,
                );
            }
            GitChange::Lost => self.forget(|_, waiting| waiting.host == host, cx),
            GitChange::Refused { reason, .. } => {
                self.frame.echo(
                    format!("{name}: refused an agent's git without asking · {reason}"),
                    StyleClass::SystemImportant,
                    cx,
                );
                return;
            }
            GitChange::Failed { what, error } => {
                self.frame.echo(
                    format!("{name}: {what} failed · {error}"),
                    StyleClass::StatusError,
                    cx,
                );
                return;
            }
        }
        self.frame.changed(cx);
        cx.notify();
    }

    /// Drops the requests `which` picks, and closes their screens.
    fn forget(&mut self, which: impl Fn(&GitAskAddress, &Waiting) -> bool, cx: &mut Context<Self>) {
        let gone: Vec<GitAskAddress> = self
            .waiting
            .iter()
            .filter(|(address, waiting)| which(address, waiting))
            .map(|(address, _)| *address)
            .collect();
        for address in gone {
            self.waiting.remove(&address);
            self.frame.close(address.address(), cx);
        }
    }

    /// The user's word on a request: handed to its host, and the card goes.
    /// Says what the echo line reads.
    pub(crate) fn answer(&mut self, address: GitAskAddress, decision: GitDecision) -> String {
        let verb = match decision {
            GitDecision::Approve => "approved",
            GitDecision::Deny => "denied",
        };
        let Some(Waiting { host, ask }) = self.waiting.remove(&address) else {
            return format!("{verb}: it no longer waits");
        };
        let title = self.title(&ask);
        match ask.answer(decision) {
            Ok(()) => format!("{verb}: {title}"),
            Err(_) => format!(
                "{}: not connected · it asks again when it is back",
                self.host_name(host)
            ),
        }
    }

    fn host_name(&self, host: HostId) -> String {
        self.hosts
            .get(&host)
            .cloned()
            .unwrap_or_else(|| host.to_string())
    }

    /// Who asks: the agent, by what the window calls it, or the host when
    /// the request came from outside any agent.
    fn asker(&self, waiting: &Waiting, cx: &App) -> String {
        let host = self.host_name(waiting.host);
        match waiting.ask.request.agent {
            Some(agent) => {
                let name =
                    (self.agents)(agent, cx).map_or_else(|| agent.encoded(), |known| known.name);
                format!("{name}@{host}")
            }
            None => host,
        }
    }

    /// The work that asks, `tide · Flaky clock test`, when the asker is an
    /// agent whose work has a thread.
    fn work(&self, waiting: &Waiting, cx: &App) -> Option<String> {
        (self.agents)(waiting.ask.request.agent?, cx)?.work
    }

    /// Where the card is: the work that asks, or who asks when the work
    /// has no thread.
    fn context(&self, waiting: &Waiting, cx: &App) -> String {
        self.work(waiting, cx)
            .unwrap_or_else(|| self.asker(waiting, cx))
    }

    /// The card's title: what it does, where, in short.
    fn title(&self, ask: &GitAsk) -> String {
        let repository = format!("{}/{}", ask.request.host, ask.request.repository);
        match ask.push() {
            Some(_) => format!("push {} to {repository}", ask.refs().join(", ")),
            None => format!("fetch from {repository}"),
        }
    }

    fn since(ask: &GitAsk) -> Timestamp {
        Timestamp::from_millisecond(ask.requested_at_ms as i64).unwrap_or_else(|_| Timestamp::now())
    }

    fn verdict(
        &self,
        address: GitAskAddress,
        decision: GitDecision,
        cx: &mut Context<Self>,
    ) -> Row {
        let system = cx.entity();
        let (key, word) = match decision {
            GitDecision::Approve => ("a", "approve"),
            GitDecision::Deny => ("d", "deny"),
        };
        Row::run(key, word, move |frame, window, cx| {
            let said = system.update(cx, |system, _| system.answer(address, decision));
            frame.answered(&address.address(), said, window, cx);
        })
    }
}

impl System for GitApprovals {
    fn kinds(&self) -> &'static [&'static str] {
        &[GitAskAddress::KIND]
    }

    fn open(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) -> Opening {
        let Some(address) = GitAskAddress::from_address(address) else {
            return Opening::Gone;
        };
        let Some(waiting) = self.waiting.get(&address) else {
            return Opening::Gone;
        };
        let screen = surface::Shown {
            title: self.title(&waiting.ask),
            work: self.work(waiting, cx),
            asker: self.asker(waiting, cx),
            destination: waiting.ask.destination(),
            push: waiting.ask.push().cloned(),
            asked: Self::since(&waiting.ask),
        };
        Opening::Surface(AnySurface::new(
            cx.new(|cx| GitAskSurface::new(screen, window, cx)),
        ))
    }

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        _endpoint: Option<EndpointId>,
        _link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        self.hosts.insert(host, name.to_owned());
        vec![self.streams.stream(host)]
    }

    fn detached(&mut self, host: HostId, cx: &mut Context<Self>) {
        self.forget(|_, waiting| waiting.host == host, cx);
        self.hosts.remove(&host);
    }

    fn verdicts(&mut self, address: &Address, cx: &mut Context<Self>) -> Option<Vec<Row>> {
        let address = GitAskAddress::from_address(address)?;
        self.waiting.contains_key(&address).then(|| {
            vec![
                self.verdict(address, GitDecision::Approve, cx),
                self.verdict(address, GitDecision::Deny, cx),
            ]
        })
    }

    /// Every request waits on the user like an agent's ask: from when the
    /// agent's git asked, lit at once.
    fn claims(
        &self,
        _marks: &Marks,
        _now: Timestamp,
        _trace: Option<&mut Trace>,
        cx: &App,
    ) -> Vec<Claim> {
        self.waiting
            .iter()
            .map(|(address, waiting)| {
                let mut claim = Claim::new(
                    address.address(),
                    Curve::Waiting {
                        head_start: AGENT_BLOCKED_HEAD_START,
                        since: Self::since(&waiting.ask),
                    },
                    CardKind::Waits,
                    address.rest(),
                );
                claim.title = self.title(&waiting.ask);
                claim.context = self.context(waiting, cx);
                claim
            })
            .collect()
    }

    fn name(&self, address: &Address, _marks: &Marks, cx: &App) -> Option<(String, String)> {
        let waiting = self.waiting.get(&GitAskAddress::from_address(address)?)?;
        Some((self.title(&waiting.ask), self.context(waiting, cx)))
    }
}
