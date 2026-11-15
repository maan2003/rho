//! The agents, plugged into the frame.
//!
//! An agent has no list and no place of its own: its transcript opens from
//! an `agent:` address, which a thread's participants and an agent's posts
//! carry. The system holds the model thread's client, the directory, the
//! transcripts it has open, and one [`AgentModel`] per opened agent; a
//! surface observes its model. Starting an agent is a row on a thread
//! surface: the message under the point is the agent's origin.
//!
//! The system also opens the usage charts ([`crate::usage`]): it asks the
//! hosts it reaches and keeps their answers, and a chart observes it.

mod start;
mod surface;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task, WeakEntity, Window,
};
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_agent_types::{AgentId, MessageId};
use rho_agents_client::model::{AgentsClient, ModelEvent, ModelMsg};
use rho_agents_client::protocol::worksets::Eval;
use rho_agents_client::protocol::{AgentCostDistribution, GlobalUsage, QuotaHistory, QuotaSeries};
use rho_agents_client::quota::Quotas;
use rho_agents_client::remote::AgentsLink;
use rho_agents_client::store::FrameSummary;
use rho_agents_client::{AgentMap, now_ms};
use rho_chat::ui::{ChatSystem, Event as ChatEvent};
use rho_chat::{AgentRef, ThreadId};
use rho_frame::{Address, Addressed as _, AnySurface, Found, FrameLink, Marks, Point, Row, System};
use rho_rpc::protocol::Call;
use rho_window::style::StyleClass;
use surface::AgentSurface;

use crate::agent_view::{AgentModel, AgentModelEvent};
use crate::transcript::{TranscriptFrame, Transcripts};
use crate::usage::{self, Request, Usage, UsageChart, UsageMoved, UsageSurface};

/// Every kind the system opens: agents, and the usage charts
/// (`SPEC-usage-addresses`).
const KINDS: &[&str] = &[AgentRef::KIND, UsageChart::KIND];

fn agent_of(address: &Address) -> Option<AgentId> {
    AgentRef::from_address(address).map(|agent| agent.0)
}

pub struct AgentsSystem {
    client: AgentsClient,
    registry: AgentMap,
    transcripts: Transcripts,
    models: HashMap<AgentId, Entity<AgentModel>>,
    /// The surfaces open on agents, weakly: one the frame dropped is one
    /// less transcript to follow.
    surfaces: HashMap<AgentId, WeakEntity<AgentSurface>>,
    links: HashMap<HostId, Link>,
    quotas: Quotas,
    /// What the hosts answered for the spend charts.
    usage: Usage,
    chat: Entity<ChatSystem>,
    frame: FrameLink,
    /// Agents asked for with the user's message, by the id the chat gave
    /// the message: started once the host has logged it.
    starting: HashMap<(u64, u64), start::Plan>,
    timer: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentsSystem {
    pub fn new(chat: Entity<ChatSystem>, frame: FrameLink, cx: &mut Context<Self>) -> Self {
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
        let delivered = cx.subscribe(&chat, |this, _, event: &ChatEvent, cx| {
            if let ChatEvent::Delivered {
                machine_seed,
                client_id,
                message,
            } = event
            {
                this.delivered(*machine_seed, *client_id, *message, cx);
            }
        });
        Self {
            client,
            registry: AgentMap::default(),
            transcripts: Transcripts::default(),
            models: HashMap::new(),
            surfaces: HashMap::new(),
            links: HashMap::new(),
            quotas: Quotas::default(),
            usage: Usage::default(),
            chat,
            frame,
            starting: HashMap::new(),
            timer: None,
            _tasks: vec![pump],
            _subscriptions: vec![delivered],
        }
    }

    pub fn registry(&self) -> &AgentMap {
        &self.registry
    }

    /// Whether `agent` of the host whose database seed is `machine_seed`
    /// still exists: `true` for a host this client has not heard from,
    /// since nothing says otherwise.
    /// What the chat calls an agent of the host with `machine_seed`, and
    /// `None` once the agent is gone. A host not attached keeps every
    /// agent of its threads, by id.
    pub fn agent_label(&self, machine_seed: u64, agent: AgentId) -> Option<String> {
        match self.host_of_seed(machine_seed) {
            Some(_) => self
                .registry
                .mirrored(agent)
                .map(|_| self.registry.agent_display_label(agent)),
            None => Some(agent.encoded()),
        }
    }

    fn host_of_seed(&self, machine_seed: u64) -> Option<HostId> {
        self.registry
            .hosts()
            .map(|(host, _)| host)
            .find(|host| self.registry.host_machine_seed(*host) == machine_seed)
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
            ModelMsg::Read { agent_id } => {
                if !self.transcripts.is_open(&agent_id) {
                    return;
                }
                let rows = self.client.read_transcript(agent_id);
                if let Some(state) = self.transcripts.reread(agent_id, &rows) {
                    self.land(agent_id, TranscriptFrame::Fold(state), cx);
                }
            }
            ModelMsg::Runtime { agent_id, state } => {
                if self.transcripts.state(&agent_id).is_some() {
                    self.land(agent_id, TranscriptFrame::Runtime(state.clone()), cx);
                }
                if self.registry.set_runtime(agent_id, state) {
                    self.changed(cx);
                }
            }
            ModelMsg::Live { agent_id, live } => {
                self.land(agent_id, TranscriptFrame::Live(live), cx);
            }
            ModelMsg::Host {
                machine_seed,
                agent_counter,
            } => {
                self.registry
                    .set_host_data(host, machine_seed, agent_counter);
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
            if self.surfaces.remove(agent).is_some() {
                self.frame.close(AgentRef(*agent).address(), cx);
            }
        }
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
    fn seed(&mut self, agent: AgentId) -> bool {
        if self.transcripts.is_open(&agent) || self.registry.agent_workset(agent).is_none() {
            return false;
        }
        let rows = self.client.read_transcript(agent);
        if !self.transcripts.seed(agent, &rows) {
            return false;
        }
        if let Some(state) = self.registry.agent_facts(agent).runtime {
            self.transcripts
                .apply(agent, TranscriptFrame::Runtime(state));
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

    /// Runs a snippet in the agent's workset on its host. What the code
    /// raised is echoed.
    pub(crate) fn eval(&self, agent: AgentId, code: String, cx: &mut Context<Self>) {
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
                Ok(result) => {
                    if let Some(error) = result.error {
                        frame.echo(error, StyleClass::SystemImportant, cx);
                    }
                }
                Err(error) => frame.echo(format!("{error:#}"), StyleClass::SystemImportant, cx),
            });
        })
        .detach();
    }

    /// The host logged a message the user sent to start an agent with.
    fn delivered(
        &mut self,
        machine_seed: u64,
        client_id: u64,
        message: MessageId,
        cx: &mut Context<Self>,
    ) {
        if let Some(plan) = self.starting.remove(&(machine_seed, client_id)) {
            start::create(self, plan, message, cx);
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

    fn thread_at(&self, point: &Point, cx: &App) -> Option<ThreadId> {
        let chat = self.chat.read(cx);
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
            self.ask(chart, cx);
            return Some(AnySurface::new(surface));
        }
        let agent = agent_of(address)?;
        self.registry.mirrored(agent)?;
        let model = self.loaded_model(agent, cx);
        let system = cx.entity();
        let surface = cx.new(|cx| AgentSurface::new(system, agent, model, window, cx));
        self.surfaces.insert(agent, surface.downgrade());
        self._subscriptions
            .push(cx.observe_release(&surface, move |this, _, cx| {
                this.surfaces.remove(&agent);
                this.note_open(cx);
            }));
        self.note_open(cx);
        Some(AnySurface::new(surface))
    }

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        self.links.insert(host, link.clone());
        self.registry.attach_host(host, name.to_owned());
        self.client.attach_host(host, name.to_owned());
        vec![self.client.stream(host)]
    }

    fn detached(&mut self, host: HostId, cx: &mut Context<Self>) {
        self.client.detach_host(host);
        self.links.remove(&host);
        self.quotas.forget(host);
        self.usage.forget(host);
        cx.emit(UsageMoved::Host);
        let departed = self.registry.detach_host(host);
        self.deleted(departed, cx);
    }

    fn rows(&mut self, point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        let mut rows = vec![Row::menu("shift-u", "usage…", usage::menu(this.clone()))];
        let Some(thread) = self.thread_at(point, cx) else {
            return rows;
        };
        if self.host_of_seed(thread.machine_seed).is_none() {
            return rows;
        }
        rows.push(Row::menu(
            "a",
            "start agent here",
            start::rows(self, this.clone(), thread, start::Origin::Message, cx),
        ));
        let draft = !self
            .chat
            .read(cx)
            .draft(thread.machine_seed, Some(thread.message))
            .trim()
            .is_empty();
        if draft {
            rows.push(Row::menu(
                "shift-s",
                "send and start agent",
                start::rows(self, this, thread, start::Origin::Draft, cx),
            ));
        }
        rows
    }

    fn name(&self, address: &Address, _cx: &App) -> Option<(String, String)> {
        if let Some(chart) = UsageChart::from_address(address) {
            return Some((chart.title(), "usage".to_owned()));
        }
        let agent = agent_of(address)?;
        self.registry.mirrored(agent)?;
        Some(self.label(agent))
    }

    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        self.registry
            .known_agents()
            .copied()
            .map(|agent| {
                let (title, host) = self.label(agent);
                let facts = self.registry.agent_facts(agent);
                Found {
                    address: AgentRef(agent).address(),
                    title,
                    description: match facts.runtime.as_ref().map(surface::status_word) {
                        Some(word) => format!("{host} · {word}"),
                        None => host,
                    },
                    recency: self.registry.agent_last_active(agent).map_or(0, |at| at.0),
                }
            })
            .chain(UsageChart::all().map(|chart| Found {
                address: chart.address(),
                title: chart.title(),
                description: "usage".to_owned(),
                recency: 0,
            }))
            .collect()
    }
}
