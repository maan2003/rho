//! The agents, plugged into the frame.
//!
//! An agent has no list and no place of its own: its transcript opens from
//! an `agent:` address, which a thread's participants and an agent's posts
//! carry. The system holds the model thread's client, the directory, the
//! transcripts it has open, and one [`AgentModel`] per opened agent; a
//! surface observes its model. Starting an agent is a row on a thread
//! surface: the message under the point is the agent's origin.
//!
//! An agent's terminals, its shell and its desktops hang under it
//! ([`AgentAddress`]): the same system opens them, with the agent's host
//! as their link, and offers the rows that start one from any surface of
//! the agent.
//! The system also opens the usage charts ([`crate::usage`]): it asks the
//! hosts it reaches and keeps their answers, and a chart observes it.

mod address;
mod places;
mod start;
mod surface;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

pub use address::{AgentAddress, Part};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task, WeakEntity, Window,
};
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_agent_types::{AgentId, EndpointId, MessageId};
use rho_agents_client::model::{AgentsClient, ModelEvent, ModelMsg};
use rho_agents_client::protocol::worksets::Eval;
use rho_agents_client::protocol::{AgentCostDistribution, GlobalUsage, QuotaHistory, QuotaSeries};
use rho_agents_client::quota::Quotas;
use rho_agents_client::remote::AgentsLink;
use rho_agents_client::store::FrameSummary;
use rho_agents_client::{AgentMap, Heard, now_ms};
use rho_desktop_client::protocol::DesktopSession;
use rho_desktop_client::stream::{DesktopStreams, DesktopsEvent};
use rho_desktop_client::ui::DesktopSurface;
use rho_frame::{
    Address, Addressed as _, AnySurface, Candidate, Found, FrameLink, Marks, Point, Row, System,
};
use rho_mail::ui::{Event as MailEvent, MailSystem, Participant};
use rho_mail::{AgentRef, ThreadId};
use rho_rpc::protocol::Call;
use rho_shell_view::ui::ShellSurface;
use rho_terminal::ui::TerminalSurface;
use rho_window::style::StyleClass;
use surface::AgentSurface;

use crate::agent_view::{AgentModel, AgentModelEvent};
use crate::transcript::{TranscriptFrame, Transcripts};
use crate::usage::{Request, Usage, UsageChart, UsageMoved, UsageSurface};

/// Every kind the system opens: agents, and the usage charts
/// (`SPEC-usage-addresses`).
const KINDS: &[&str] = &[AgentRef::KIND, UsageChart::KIND];

pub struct AgentsSystem {
    client: AgentsClient,
    registry: AgentMap,
    transcripts: Transcripts,
    models: HashMap<AgentId, Entity<AgentModel>>,
    /// The surfaces open on agents, weakly: one the frame dropped is one
    /// less transcript to follow.
    surfaces: HashMap<AgentId, WeakEntity<AgentSurface>>,
    links: HashMap<HostId, Link>,
    /// Each host's desktops stream, and the desktops it last said it runs.
    desktops: DesktopStreams,
    sessions: HashMap<HostId, Vec<DesktopSession>>,
    quotas: Quotas,
    /// What the hosts answered for the spend charts.
    usage: Usage,
    mail: Entity<MailSystem>,
    frame: FrameLink,
    /// Agents asked for with the user's message, by the id the chat gave
    /// the message: started once the host has logged it.
    starting: HashMap<(EndpointId, u64), start::Plan>,
    /// Where each channel's agents work, as the user answered it, until an
    /// agent of the channel says so itself.
    places: places::ProjectPlaces,
    /// Agents this window started whose first turn has not ended, and
    /// whether it got to work: a failure before then is the start failing,
    /// and the echo line says so.
    first_turns: HashMap<AgentId, bool>,
    timer: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentsSystem {
    pub fn new(
        db: rho_db::RhoDb,
        mail: Entity<MailSystem>,
        frame: FrameLink,
        cx: &mut Context<Self>,
    ) -> Self {
        let (client, changes) = AgentsClient::spawn();
        let pump = cx.spawn(async move |this, cx| {
            let mut changes = changes;
            while let Some(change) = changes.next().await {
                let mut batch = vec![change];
                while let Ok(change) = changes.try_recv() {
                    batch.push(change);
                }
                let updated = this.update(cx, |this, cx| {
                    for ModelEvent { host, msg } in batch {
                        this.model_event(host, msg, cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let (desktops, mut desktop_events) = DesktopStreams::new();
        let desktops_pump = cx.spawn(async move |this, cx| {
            while let Some(DesktopsEvent { host, sessions }) = desktop_events.next().await {
                let updated = this.update(cx, |this, cx| {
                    this.sessions.insert(host, sessions);
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        AgentSurface::bind_keys(cx);
        rho_terminal::ui::bind_keys(cx);
        rho_shell_view::ui::bind_keys(cx);
        rho_desktop_client::ui::bind_keys(cx);
        let delivered = cx.subscribe(&mail, |this, _, event: &MailEvent, cx| match event {
            MailEvent::Delivered {
                endpoint,
                client_id,
                message,
            } => this.delivered(*endpoint, *client_id, *message, cx),
            MailEvent::Idea {
                endpoint,
                parent,
                source,
            } => start::idea(
                this,
                ThreadId {
                    host: *endpoint,
                    message: *parent,
                },
                source.clone(),
                cx,
            ),
            MailEvent::Inspect { agent, mail } => {
                let agent = *agent;
                let mail = mail.clone();
                let system = cx.weak_entity();
                this.frame.with(cx, move |frame, window, cx| {
                    let Some(system) = system.upgrade() else {
                        return;
                    };
                    if system.read(cx).registry.mirrored(agent).is_none() {
                        frame.echo(
                            "the agent's transcript is not available",
                            StyleClass::SystemInfo,
                            cx,
                        );
                        return;
                    }
                    frame.open(AgentRef(agent).address(), window, cx);
                    if let Some(surface) = system
                        .read(cx)
                        .surfaces
                        .get(&agent)
                        .and_then(WeakEntity::upgrade)
                    {
                        surface.update(cx, |surface, cx| surface.inspect(mail, window, cx));
                    }
                });
            }
            MailEvent::Changed { .. } | MailEvent::DraftSent(_) => {}
        });
        Self {
            client,
            registry: AgentMap::default(),
            transcripts: Transcripts::default(),
            models: HashMap::new(),
            surfaces: HashMap::new(),
            links: HashMap::new(),
            desktops,
            sessions: HashMap::new(),
            quotas: Quotas::default(),
            usage: Usage::default(),
            mail,
            frame,
            starting: HashMap::new(),
            places: places::ProjectPlaces::open(db),
            first_turns: HashMap::new(),
            timer: None,
            _tasks: vec![pump, desktops_pump],
            _subscriptions: vec![delivered],
        }
    }

    pub fn registry(&self) -> &AgentMap {
        &self.registry
    }

    /// What the chat calls an agent of the host with this identity and
    /// what it is doing, and `None` once the agent is gone. A host not
    /// attached keeps every agent of its threads, by id, saying nothing
    /// of what they do.
    pub fn participant(&self, endpoint: EndpointId, agent: AgentId) -> Option<Participant> {
        match self.host_of(endpoint) {
            Some(host) => self
                .registry
                .mirrored(agent)
                .filter(|entry| entry.host == host)
                .map(|entry| {
                    let heard = self.registry.agent_facts(agent).runtime;
                    Participant {
                        label: self.registry.agent_display_label(agent),
                        runtime: Some(format!(
                            "{}-{}",
                            entry.identity.entry.spec.model, entry.identity.entry.spec.effort
                        )),
                        created_at: Some(entry.identity.entry.created_at),
                        doing: heard.as_ref().map(|heard| heard.doing()),
                        stopped_because: heard
                            .as_ref()
                            .and_then(|heard| heard.stopped_because())
                            .map(str::to_owned),
                    }
                }),
            None => Some(Participant {
                label: agent.encoded(),
                runtime: None,
                created_at: None,
                doing: None,
                stopped_because: None,
            }),
        }
    }

    /// The attached host with this identity.
    fn host_of(&self, endpoint: EndpointId) -> Option<HostId> {
        self.registry
            .hosts()
            .map(|(host, _)| host)
            .find(|host| self.registry.host_endpoint(*host) == Some(endpoint))
    }

    fn agents_on(&self, host: HostId) -> Option<AgentsLink> {
        self.links.get(&host).cloned().map(AgentsLink::new)
    }

    fn agents_for(&self, agent: AgentId) -> Option<AgentsLink> {
        self.agents_on(self.registry.host_of_agent(agent)?)
    }

    /// The agent's label with its host, for the status line and `find`.
    fn label(&self, agent: AgentId) -> (String, String) {
        let host = self
            .registry
            .host_of_agent(agent)
            .map(|host| self.registry.host_name(host).to_owned())
            .unwrap_or_default();
        (self.registry.agent_display_label(agent), host)
    }

    fn model_event(&mut self, host: HostId, msg: ModelMsg, cx: &mut Context<Self>) {
        match msg {
            ModelMsg::Loaded { agents, .. } => {
                for agent in self.registry.host_agents(host) {
                    self.transcripts.forget(agent);
                }
                self.registry.reset_host(host);
                self.registry.restore(agents);
                self.note_open(cx);
                self.changed(cx);
            }
            ModelMsg::Changed { agents } => {
                let changed = self.registry.told(agents);
                if changed.is_empty() {
                    return;
                }
                self.changed(cx);
            }
            ModelMsg::Deleted { agents } => self.deleted(agents, cx),
            ModelMsg::Rows { agent_id, rows } => {
                if let Some(delta) = self.transcripts.refold(agent_id, &rows) {
                    self.land(agent_id, TranscriptFrame::Folded(delta), cx);
                }
            }
            ModelMsg::Read {
                agent_id,
                inspection,
            } => {
                if !self.transcripts.is_open(&agent_id) {
                    return;
                }
                let rows = self.client.read_transcript(agent_id);
                if let Some(state) = self.transcripts.reread(agent_id, &rows) {
                    self.land(agent_id, TranscriptFrame::Fold(state), cx);
                }
                if let Some(mail) = inspection
                    && let Some(surface) = self.surfaces.get(&agent_id).cloned()
                {
                    self.frame.with(cx, move |_, window, cx| {
                        if let Some(surface) = surface.upgrade() {
                            surface.update(cx, |surface, cx| {
                                surface.read_inspection(mail, window, cx);
                            });
                        }
                    });
                }
            }
            ModelMsg::Runtime {
                agent_id,
                state,
                since,
            } => {
                if self.transcripts.state(&agent_id).is_some() {
                    self.land(agent_id, TranscriptFrame::Runtime(state.clone()), cx);
                }
                if self.registry.set_runtime(agent_id, Heard { state, since }) {
                    self.first_turn_moved(agent_id, cx);
                    self.changed(cx);
                }
            }
            ModelMsg::Live { agent_id, live } => {
                self.land(agent_id, TranscriptFrame::Live(live), cx);
            }
            ModelMsg::Host {
                machine_seed,
                endpoint,
                agent_counter,
            } => {
                self.registry
                    .set_host_data(host, machine_seed, endpoint, agent_counter);
                cx.emit(UsageMoved::Connected);
                cx.notify();
            }
            ModelMsg::Auth { auth } => {
                self.quotas.set_auth(host, auth);
                cx.emit(UsageMoved::Quota);
                cx.notify();
            }
            ModelMsg::QuotaUsage { summaries } => {
                self.quotas.set_summaries(host, summaries);
                cx.notify();
            }
        }
    }

    /// Lands a frame on an agent's transcript and shows what moved.
    fn land(&mut self, agent: AgentId, frame: TranscriptFrame, cx: &mut Context<Self>) {
        let change = self.transcripts.apply(agent, frame);
        if change.was_live && self.registry.mark_live(agent) {
            cx.notify();
        }
        self.sync(agent, change.summary, cx);
        self.ensure_timer(cx);
    }

    fn sync(&mut self, agent: AgentId, summary: FrameSummary, cx: &mut Context<Self>) {
        let (Some(model), Some(state)) = (self.models.get(&agent), self.transcripts.state(&agent))
        else {
            return;
        };
        let registry = &self.registry;
        model.update(cx, |model, cx| {
            model.sync(
                state,
                summary,
                now_ms(),
                &|id| registry.agent_display_label(id),
                cx,
            )
        });
    }

    /// A durable deletion removes host-owned history: the surfaces go too,
    /// so history cannot show what the host no longer has.
    fn deleted(&mut self, agents: BTreeSet<AgentId>, cx: &mut Context<Self>) {
        for agent in &agents {
            self.transcripts.forget(*agent);
            self.models.remove(agent);
            self.surfaces.remove(agent);
        }
        // Every screen of theirs: the transcript, terminals, shells, and
        // desktops.
        let gone = agents.clone();
        self.frame.close_all(
            move |address| {
                AgentAddress::from_address(address).is_some_and(|at| gone.contains(&at.agent))
            },
            cx,
        );
        self.registry.forget_agents(&agents);
        self.note_open(cx);
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        self.frame.changed(cx);
    }

    /// Tells the model which transcripts to follow and every host which
    /// agents to stream live: the ones with a surface open.
    fn note_open(&mut self, _cx: &mut Context<Self>) {
        self.surfaces
            .retain(|_, surface| surface.upgrade().is_some());
        let open: BTreeSet<AgentId> = self.surfaces.keys().copied().collect();
        self.client.follow(open.clone());
        let hosts: Vec<HostId> = self.registry.hosts().map(|(host, _)| host).collect();
        for host in hosts {
            let agents = open
                .iter()
                .copied()
                .filter(|agent| self.registry.host_of_agent(*agent) == Some(host))
                .collect();
            self.client.focus(host, agents);
        }
        self.transcripts
            .forget_live_except(|agent| open.contains(&agent));
        for agent in self.registry.known_agents().copied().collect::<Vec<_>>() {
            if !open.contains(&agent) {
                self.registry.mark_not_live(agent);
            }
        }
    }

    /// Opens an agent's transcript from the disk copy, with the runtime
    /// state already heard. Answers whether it was opened now.
    pub(super) fn read_for_inspection(&self, agent: AgentId, mail: MessageId) -> bool {
        let Some(host) = self.registry.host_of_agent(agent) else {
            return false;
        };
        self.client.inspect_mail(host, agent, mail);
        true
    }

    fn seed(&mut self, agent: AgentId) -> bool {
        if self.transcripts.is_open(&agent) || self.registry.agent_workset(agent).is_none() {
            return false;
        }
        let rows = self.client.read_transcript(agent);
        if !self.transcripts.seed(agent, &rows) {
            return false;
        }
        if let Some(heard) = self.registry.agent_facts(agent).runtime {
            self.transcripts
                .apply(agent, TranscriptFrame::Runtime(heard.state));
        }
        true
    }

    fn model(&mut self, agent: AgentId, cx: &mut Context<Self>) -> Entity<AgentModel> {
        if let Some(model) = self.models.get(&agent) {
            return model.clone();
        }
        let link = self.agents_for(agent).unwrap_or_else(AgentsLink::detached);
        let model = cx.new(|cx| AgentModel::new(link, cx));
        self._subscriptions.push(cx.subscribe(
            &model,
            move |this, model, event: &AgentModelEvent, cx| {
                if matches!(event, AgentModelEvent::Loaded)
                    && this.models.get(&agent) == Some(&model)
                {
                    this.sync(agent, FrameSummary::nothing(), cx);
                    this.ensure_timer(cx);
                }
            },
        ));
        self.models.insert(agent, model.clone());
        model
    }

    /// The model of an agent, loaded: the transcript seeded from the disk
    /// copy, the bulk load started, and what moved since shown.
    fn loaded_model(&mut self, agent: AgentId, cx: &mut Context<Self>) -> Entity<AgentModel> {
        let told = self.seed(agent);
        let model = self.model(agent, cx);
        if !model.read(cx).initial_load_started()
            && let Some(state) = self.transcripts.state(&agent).cloned()
        {
            let labels = self
                .registry
                .known_agents()
                .copied()
                .map(|id| (id, self.registry.agent_display_label(id)))
                .collect();
            model.update(cx, |model, cx| {
                model.start_initial_load(state, labels, now_ms(), cx)
            });
        } else if told {
            self.sync(agent, FrameSummary::everything(), cx);
        } else if model.read(cx).initial_load_ready() {
            self.sync(agent, FrameSummary::nothing(), cx);
        }
        model
    }

    /// Ticks the running durations of the open transcripts once a second
    /// while any of them has one.
    fn ensure_timer(&mut self, cx: &mut Context<Self>) {
        if self.timer.is_some() {
            return;
        }
        let ticking = self
            .surfaces
            .keys()
            .filter_map(|agent| self.models.get(agent))
            .any(|model| model.read(cx).has_timers());
        if !ticking {
            return;
        }
        self.timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let keep_going = this.update(cx, |this, cx| {
                    let mut any = false;
                    for agent in this.surfaces.keys() {
                        if let Some(model) = this.models.get(agent) {
                            any |= model.update(cx, |model, cx| {
                                model.tick_timers(now_ms(), cx);
                                model.has_timers()
                            });
                        }
                    }
                    any
                });
                if !matches!(keep_going, Ok(true)) {
                    break;
                }
            }
            let _ = this.update(cx, |this, _| this.timer = None);
        }));
    }

    /// Runs a snippet in the agent's workset on its host, for the action the
    /// user chose. What the code raised is echoed in one line after the
    /// action's name; `done`, if given, is echoed when it raised nothing.
    pub(crate) fn eval(
        &self,
        agent: AgentId,
        action: &'static str,
        code: String,
        done: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let (Some(agents), Some(workset)) =
            (self.agents_for(agent), self.registry.agent_workset(agent))
        else {
            self.frame.echo(
                "the agent's host is not attached",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let reply = agents.call(Eval { workset, code });
        let frame = self.frame.clone();
        cx.spawn(async move |_, cx| {
            let reply = reply.await;
            cx.update(|cx| match reply {
                Ok(result) => match result.raised() {
                    Some(raised) => frame.echo(
                        format!("{action}: {raised}"),
                        StyleClass::SystemImportant,
                        cx,
                    ),
                    None => {
                        if let Some(done) = done {
                            frame.echo(done, StyleClass::SystemInfo, cx);
                        }
                    }
                },
                Err(error) => frame.echo(
                    format!("{action}: {error:#}"),
                    StyleClass::SystemImportant,
                    cx,
                ),
            });
        })
        .detach();
    }

    /// Watches the first turn of an agent this window just started: the
    /// start is not done until the agent got to work.
    pub(crate) fn watch_first_turn(&mut self, agent: AgentId, cx: &mut Context<Self>) {
        self.first_turns.insert(agent, false);
        self.first_turn_moved(agent, cx);
    }

    fn first_turn_moved(&mut self, agent: AgentId, cx: &mut Context<Self>) {
        use rho_agent_types::Activity;
        let Some(worked) = self.first_turns.get_mut(&agent) else {
            return;
        };
        let Some(heard) = self.registry.agent_facts(agent).runtime else {
            return;
        };
        match heard.doing().activity {
            Activity::Failed | Activity::Lost => {
                self.first_turns.remove(&agent);
                let reason = heard.stopped_because().unwrap_or("its host gave no reason");
                self.frame.echo(
                    format!("agent did not start: {reason}"),
                    StyleClass::SystemImportant,
                    cx,
                );
            }
            Activity::Working | Activity::Retrying => *worked = true,
            Activity::Waiting | Activity::Archived => {
                self.first_turns.remove(&agent);
            }
            Activity::Idle => {
                if *worked {
                    self.first_turns.remove(&agent);
                }
            }
        }
    }

    /// The host logged a message the user sent to start an agent with.
    fn delivered(
        &mut self,
        endpoint: EndpointId,
        client_id: u64,
        message: MessageId,
        cx: &mut Context<Self>,
    ) {
        if let Some(plan) = self.starting.remove(&(endpoint, client_id)) {
            start::create(
                self,
                plan,
                ThreadId {
                    host: endpoint,
                    message,
                },
                cx,
            );
        }
    }

    /// What a surface under the agent dials with: its host's link, its
    /// encoded id, and what to call it. `None` while the host is away.
    fn workset_of(&self, agent: AgentId) -> Option<(&Link, String, String)> {
        let link = self.links.get(&self.registry.host_of_agent(agent)?)?;
        Some((
            link,
            agent.encoded(),
            self.registry.agent_display_label(agent),
        ))
    }

    fn agent_thread(&self, agent: AgentId, cx: &App) -> Option<ThreadId> {
        let mirrored = self.registry.mirrored(agent)?;
        let host = self.registry.host_endpoint(mirrored.host)?;
        let chat = self.mail.read(cx).host_mail(host)?;
        let message = chat.agent_thread(agent).or_else(|| {
            mirrored
                .identity
                .origin()
                .map(|origin| chat.thread_of(origin))
        })?;
        Some(ThreadId { host, message })
    }

    fn brief(&self, agent: AgentId, cx: &App) -> Option<String> {
        let mirrored = self.registry.mirrored(agent)?;
        let host = self.registry.host_endpoint(mirrored.host)?;
        let chat = self.mail.read(cx).host_mail(host)?;
        let origin = mirrored.identity.origin()?;
        let brief = chat.message(origin)?.text();
        (!brief.trim().is_empty()).then_some(brief)
    }

    fn open_transcript(
        &mut self,
        agent: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnySurface {
        let model = self.loaded_model(agent, cx);
        let system = cx.entity();
        let failed = self
            .registry
            .agent_facts(agent)
            .runtime
            .is_some_and(|heard| heard.state.activity() == rho_agent_types::Activity::Failed);
        let surface = cx.new(|cx| AgentSurface::new(system, agent, model, failed, window, cx));
        self.surfaces.insert(agent, surface.downgrade());
        self._subscriptions
            .push(cx.observe_release(&surface, move |this, _, cx| {
                this.surfaces.remove(&agent);
                this.note_open(cx);
            }));
        self.note_open(cx);
        AnySurface::new(surface)
    }

    /// The desktops an agent's workset runs, as its host last said.
    fn desktops_of(&self, agent: AgentId) -> Vec<String> {
        let id = agent.encoded();
        self.registry
            .host_of_agent(agent)
            .and_then(|host| self.sessions.get(&host))
            .into_iter()
            .flatten()
            .filter(|session| session.agent == id)
            .map(|session| session.name.clone())
            .collect()
    }

    /// The rows of an agent's surfaces: its terminals, shell and desktops,
    /// from the transcript or from any of them.
    fn agent_rows(&self, here: AgentAddress, cx: &mut Context<Self>) -> Vec<Row> {
        let agent = here.agent;
        if self.workset_of(agent).is_none() {
            return Vec::new();
        }
        let this = cx.entity();
        let mut rows = vec![
            Row::run("space", "verdicts", |_, window, cx| {
                window.dispatch_action(Box::new(rho_frame::keys::VerdictMenu), cx);
            }),
            Self::stop_row(this.clone(), agent),
        ];
        for (key, description, new) in [("t", "terminal", false), ("shift-t", "new terminal", true)]
        {
            let this = this.clone();
            rows.push(Row::run(key, description, move |_, _, cx| {
                this.update(cx, |this, cx| this.open_terminal(agent, new, cx));
            }));
        }
        rows.push(Row::run("x", "shell", move |frame, window, cx| {
            frame.open(AgentAddress::shell(agent).address(), window, cx);
        }));
        if here.part == Part::Shell {
            let this = this.clone();
            rows.push(Row::run("shift-x", "close shell", move |_, _, cx| {
                this.update(cx, |this, cx| this.close_shell(agent, cx));
            }));
        }
        if !self.desktops_of(agent).is_empty() {
            rows.push(Row::run("d", "desktop…", move |_, _, cx| {
                this.update(cx, |this, cx| this.open_desktop(agent, cx));
            }));
        }
        rows
    }

    /// Opens a terminal of the agent: with `new` a fresh one, else the
    /// first that runs, spawning one when none does.
    fn open_terminal(&mut self, agent: AgentId, new: bool, cx: &mut Context<Self>) {
        let Some((link, id, _)) = self.workset_of(agent) else {
            return;
        };
        let running = rho_terminal::channel::list(link, Some(id));
        cx.spawn(async move |this, cx| {
            let running = running.await;
            let _ = this.update(cx, |this, cx| match running {
                Ok(running) => {
                    let terminal = rho_terminal::ui::pick_terminal(&running, new);
                    this.frame
                        .open(AgentAddress::terminal(agent, terminal).address(), cx);
                }
                Err(error) => {
                    this.frame
                        .echo(format!("terminal: {error:#}"), StyleClass::SystemInfo, cx)
                }
            });
        })
        .detach();
    }

    /// Stops the agent's shell on its host, and closes its surface once
    /// the host has.
    fn close_shell(&mut self, agent: AgentId, cx: &mut Context<Self>) {
        let Some((link, id, _)) = self.workset_of(agent) else {
            return;
        };
        let closed = rho_shell_view::channel::close(link, id);
        cx.spawn(async move |this, cx| {
            let closed = closed.await;
            let _ = this.update(cx, |this, cx| match closed {
                Ok(()) => {
                    this.frame.echo("shell closed", StyleClass::SystemInfo, cx);
                    this.frame.close(AgentAddress::shell(agent).address(), cx);
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

    /// Opens the agent's one desktop, or asks which of its desktops.
    fn open_desktop(&mut self, agent: AgentId, cx: &mut Context<Self>) {
        let names = self.desktops_of(agent);
        let addresses: Vec<(String, Address)> = names
            .into_iter()
            .filter_map(|name| {
                let address = AgentAddress::desktop(agent, name.clone())?.address();
                Some((name, address))
            })
            .collect();
        match addresses.as_slice() {
            [] => self
                .frame
                .echo("the agent runs no desktop", StyleClass::SystemInfo, cx),
            [(_, address)] => self.frame.open(address.clone(), cx),
            _ => {
                let candidates: Vec<Candidate> = addresses
                    .iter()
                    .map(|(name, _)| Candidate {
                        value: name.clone(),
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
                                .filter(|candidate| {
                                    candidate.value.to_lowercase().contains(&needle)
                                })
                                .cloned()
                                .collect()
                        }),
                        Rc::new(move |frame, input, window, cx| {
                            let input = input.trim();
                            match addresses.iter().find(|(name, _)| name == input) {
                                Some((_, address)) => frame.open(address.clone(), window, cx),
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
    }

    pub(crate) fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Every host's quota history merged, with the accounts agents run on;
    /// `None` until a host has answered.
    pub(crate) fn quota_history(&self) -> Option<(Vec<QuotaSeries>, Vec<String>)> {
        self.quotas.has_history().then(|| {
            (
                self.quotas.merged_history(&self.registry),
                self.quotas.active_namespaces(&self.registry),
            )
        })
    }

    /// The hosts that answered for their quota history and have no
    /// account to read it from, when that is true of all of them.
    pub(crate) fn hosts_without_accounts(&self) -> Option<Vec<String>> {
        self.quotas.without_accounts(&self.registry)
    }

    pub(crate) fn attached_hosts(&self) -> usize {
        self.links.len()
    }

    /// Asks every host for the series `chart` draws; each answer lands as
    /// it comes and the charts it concerns draw again.
    pub(crate) fn ask(&self, chart: UsageChart, cx: &mut Context<Self>) {
        match chart.request(now_ms()) {
            Request::QuotaHistory => self.ask_every_host(
                QuotaHistory,
                |this, host, series, cx| {
                    this.quotas.set_history(host, series);
                    cx.emit(UsageMoved::Quota);
                },
                cx,
            ),
            Request::GlobalUsage { since_ms } => self.ask_every_host(
                GlobalUsage { since_ms },
                move |this, host, series, cx| {
                    this.usage.record_spend(chart, host, series);
                    cx.emit(UsageMoved::Series(chart));
                },
                cx,
            ),
            Request::AgentCostDistribution { since_ms } => self.ask_every_host(
                AgentCostDistribution { since_ms },
                move |this, host, series, cx| {
                    this.usage.record_agent_cost(chart.days, host, series);
                    cx.emit(UsageMoved::Series(chart));
                },
                cx,
            ),
        }
    }

    /// Asks every attached host the same question. An answer from a host
    /// detached since is dropped, so what it said stays forgotten.
    fn ask_every_host<C: Call + Clone>(
        &self,
        call: C,
        arrived: impl Fn(&mut Self, HostId, C::Reply, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) {
        for (&host, link) in &self.links {
            let reply = AgentsLink::new(link.clone()).call(call.clone());
            let arrived = arrived.clone();
            cx.spawn(async move |this, cx| {
                let reply = reply.await;
                this.update(cx, |this, cx| {
                    if !this.links.contains_key(&host) {
                        return;
                    }
                    match reply {
                        Ok(reply) => arrived(this, host, reply, cx),
                        Err(error) => {
                            let name = this.registry.host_name(host).to_owned();
                            this.frame.echo(
                                format!("{name}: {error:#}"),
                                StyleClass::SystemImportant,
                                cx,
                            );
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn stop_row(system: Entity<Self>, agent: AgentId) -> rho_frame::Row {
        rho_frame::Row::run("shift-c", "stop agent", move |_, _, cx| {
            system.update(cx, |system, cx| {
                let Some(agents) = system.agents_for(agent) else {
                    system.frame.echo(
                        "the agent's host is not attached",
                        StyleClass::SystemImportant,
                        cx,
                    );
                    return;
                };
                let reply = agents.call(rho_agents_client::protocol::worksets::StopAgent { agent });
                let frame = system.frame.clone();
                let label = system.registry.agent_display_label(agent);
                cx.spawn(async move |_, cx| {
                    let reply = reply.await;
                    cx.update(|cx| match reply {
                        Ok(()) => frame.echo(
                            format!("stopped by you: {label}"),
                            StyleClass::SystemInfo,
                            cx,
                        ),
                        Err(error) => frame.echo(
                            format!("did not stop {label}: {error:#}"),
                            StyleClass::SystemImportant,
                            cx,
                        ),
                    });
                })
                .detach();
            });
        })
    }

    fn thread_at(&self, point: &Point, cx: &App) -> Option<ThreadId> {
        let chat = self.mail.read(cx);
        point
            .subject
            .as_ref()
            .and_then(|address| chat.thread_of(address))
            .or_else(|| chat.thread_of(&point.surface))
    }
}

impl EventEmitter<UsageMoved> for AgentsSystem {}

impl System for AgentsSystem {
    fn kinds(&self) -> &'static [&'static str] {
        KINDS
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        if let Some(chart) = UsageChart::from_address(address) {
            let system = cx.entity();
            let surface = cx.new(|cx| UsageSurface::new(system, chart, window, cx));
            return Some(AnySurface::new(surface));
        }
        let AgentAddress { agent, part } = AgentAddress::from_address(address)?;
        let system = cx.weak_entity();
        let related = move |cx: &App| {
            system
                .upgrade()?
                .read(cx)
                .agent_thread(agent, cx)
                .map(|thread| thread.address())
        };
        if !matches!(part, Part::Desktop(_)) {
            self.registry.mirrored(agent)?;
        }
        let surface = match part {
            Part::Transcript => self.open_transcript(agent, window, cx),
            Part::Terminal(terminal) => {
                let (link, id, label) = self.workset_of(agent)?;
                AnySurface::new(
                    cx.new(|cx| TerminalSurface::new(link, id, label, terminal, window, cx)),
                )
            }
            Part::Shell => {
                let (link, id, label) = self.workset_of(agent)?;
                AnySurface::new(cx.new(|cx| ShellSurface::new(link, id, label, window, cx)))
            }
            Part::Desktop(name) => {
                let Some((link, id, label)) = self.workset_of(agent) else {
                    let surface = cx.new(|cx| {
                        DesktopSurface::unavailable(
                            agent.encoded(),
                            name,
                            "the agent's host is not attached",
                            cx,
                        )
                    });
                    return Some(AnySurface::new(surface).with_related_card(related));
                };
                if !self.desktops_of(agent).contains(&name) {
                    let surface = cx.new(|cx| {
                        DesktopSurface::unavailable(
                            label,
                            name,
                            "the agent no longer runs this desktop",
                            cx,
                        )
                    });
                    return Some(AnySurface::new(surface).with_related_card(related));
                }
                AnySurface::new(cx.new(|cx| DesktopSurface::new(link, id, label, name, window, cx)))
            }
        };
        Some(surface.with_related_card(related))
    }

    fn active_surface(&mut self, address: Option<&Address>, cx: &mut Context<Self>) {
        if let Some(chart) = address.and_then(UsageChart::from_address) {
            self.ask(chart, cx);
        }
    }

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        _endpoint: Option<EndpointId>,
        link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        self.links.insert(host, link.clone());
        self.registry.attach_host(host, name.to_owned());
        self.client.attach_host(host, name.to_owned());
        vec![self.client.stream(host), self.desktops.stream(host)]
    }

    fn detached(&mut self, host: HostId, cx: &mut Context<Self>) {
        self.client.detach_host(host);
        self.links.remove(&host);
        self.sessions.remove(&host);
        self.quotas.forget(host);
        self.usage.forget(host);
        cx.emit(UsageMoved::Host);
        let departed = self.registry.detach_host(host);
        self.deleted(departed, cx);
    }

    fn rows(&mut self, point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        if let Some(here) = AgentAddress::from_address(&point.surface) {
            return self.agent_rows(here, cx);
        }
        let mut rows = Vec::new();
        // On an agent in a thread's `with` line: the agent is steered from
        // there as from its transcript.
        if let Some(AgentAddress {
            agent,
            part: Part::Transcript,
        }) = point.subject.as_ref().and_then(AgentAddress::from_address)
            && self.registry.mirrored(agent).is_some()
        {
            rows.extend(surface::steer_rows(this.clone(), agent));
        }
        let Some(thread) = self.thread_at(point, cx) else {
            return rows;
        };
        if self.host_of(thread.host).is_none() {
            return rows;
        }
        rows.push(Row::menu(
            "a",
            start::rows(
                self,
                this.clone(),
                thread,
                start::Origin::Mail,
                start::Runtimes::Ask,
                cx,
            ),
        ));
        let draft = !self
            .mail
            .read(cx)
            .draft(thread.host, Some(thread.message))
            .trim()
            .is_empty();
        if draft {
            rows.push(Row::menu(
                "shift-s",
                start::rows(
                    self,
                    this,
                    thread,
                    start::Origin::Draft(rho_mail::ui::IdeaSource::ProjectDraft),
                    start::Runtimes::Ask,
                    cx,
                ),
            ));
        }
        rows
    }

    fn name(
        &self,
        address: &Address,
        _marks: &rho_frame::Marks,
        _cx: &App,
    ) -> Option<(String, String)> {
        if let Some(chart) = UsageChart::from_address(address) {
            return Some((chart.title(), "usage".to_owned()));
        }
        let AgentAddress { agent, part } = AgentAddress::from_address(address)?;
        self.registry.mirrored(agent)?;
        let (label, host) = self.label(agent);
        Some(match part {
            Part::Transcript => (label, host),
            part => (format!("{label} · {part}"), host),
        })
    }

    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        let agents = self.registry.known_agents().copied().map(|agent| {
            let (title, host) = self.label(agent);
            let facts = self.registry.agent_facts(agent);
            Found {
                address: AgentRef(agent).address(),
                title,
                description: match facts
                    .runtime
                    .as_ref()
                    .map(|heard| heard.state.activity().word())
                {
                    Some(word) => format!("{host} · {word}"),
                    None => host,
                },
                thread: None,
                recency: self.registry.agent_last_active(agent).map_or(0, |at| at.0),
            }
        });
        let desktops = self
            .sessions
            .iter()
            .flat_map(|(host, sessions)| sessions.iter().map(move |session| (*host, session)))
            .filter_map(|(host_id, session)| {
                let agent = AgentId::from_encoded(&session.agent).ok()?;
                if self.registry.host_of_agent(agent) != Some(host_id)
                    || !self.links.contains_key(&host_id)
                {
                    return None;
                }
                let (title, host) = self.label(agent);
                Some(Found {
                    address: AgentAddress::desktop(agent, session.name.clone())?.address(),
                    title: format!("{title} · desktop {}", session.name),
                    description: host,
                    thread: None,
                    recency: 0,
                })
            });
        agents
            .chain(desktops)
            .chain(UsageChart::all().map(|chart| Found {
                address: chart.address(),
                title: format!("usage › {}", chart.title()),
                description: String::new(),
                thread: None,
                recency: 0,
            }))
            .collect()
    }
}
