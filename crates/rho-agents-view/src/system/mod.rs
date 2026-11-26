//! The agents, plugged into the frame.
//!
//! Agents have no list and no place of their own: the work is looked into
//! through its workset's transcript, `workset:<id>`, which merges the work
//! of every agent in it; `agent:<id>` is the same transcript narrowed to
//! one agent ([`Scope`]). The system holds the model thread's client, the
//! directory, the transcripts it has open, and one [`AgentModel`] per
//! opened scope; a surface observes its model. Agents start from a
//! project's ideas, on the host, never from a menu here.
//!
//! A workset's terminals, shells and desktops hang under the agents that
//! started them ([`AgentAddress`]): the same system opens them, with the
//! agent's host as their link, and finds every one that runs. Its commands
//! ([`commands`]) are on every screen and act on what the screen is
//! about.
//! The system also opens the usage charts ([`crate::usage`]): it asks the
//! hosts it reaches and keeps their answers, and a chart observes it.

mod accounts;
mod address;
mod commands;
mod steer;
mod surface;
mod work_name;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

pub use address::{AgentAddress, Part, Scope, WorksetAddress};
pub use commands::SPELLINGS;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task, WeakEntity, Window,
};
use rho_agent_hosts::{HostId, HostStream, Hosts, Link};
use rho_agent_types::{AgentId, EndpointId, MessageId, WorksetId};
use rho_agents_client::model::{AgentsClient, ModelEvent, ModelMsg};
use rho_agents_client::protocol::worksets::Eval;
use rho_agents_client::protocol::{AgentCostDistribution, GlobalUsage, QuotaHistory, QuotaSeries};
use rho_agents_client::quota::Quotas;
use rho_agents_client::remote::AgentsLink;
use rho_agents_client::store::FrameSummary;
use rho_agents_client::{AgentMap, Heard, now_ms};
use rho_chat::ui::{ChatSystem, Event as MailEvent, MailAudience, Participant};
use rho_chat::{AgentRef, ThreadId};
use rho_desktop_client::protocol::DesktopSession;
use rho_desktop_client::stream::{DesktopStreams, DesktopsEvent};
use rho_desktop_client::ui::DesktopSurface;
use rho_frame::{
    Address, Addressed as _, AnySurface, Candidate, CardInspection, Command, Found, FrameLink,
    Marks, Opening, Point, System,
};
use rho_rpc::protocol::Call;
use rho_shell_view::ui::ShellSurface;
use rho_terminal::ui::TerminalSurface;
use rho_window::style::StyleClass;
use surface::TranscriptSurface;
use work_name::WorkName;

use crate::agent_view::{AgentModel, AgentModelEvent};
use crate::render::{self, Label, Labels};
use crate::transcript::{TranscriptFrame, Transcripts};
use crate::usage::{OPEN_USAGE, Request, Usage, UsageChart, UsageMoved, UsageSurface};

/// Every kind the system opens: agents, and the usage charts
/// (`SPEC-usage-addresses`).
const KINDS: &[&str] = &[AgentRef::KIND, WorksetAddress::KIND, UsageChart::KIND];

pub struct AgentsSystem {
    client: AgentsClient,
    registry: AgentMap,
    transcripts: Transcripts,
    models: HashMap<Scope, Entity<AgentModel>>,
    /// The transcripts open on screen, weakly: one the frame dropped is
    /// less to follow.
    surfaces: HashMap<Scope, WeakEntity<TranscriptSurface>>,
    links: HashMap<HostId, Link>,
    /// Each host's desktops stream, and the desktops it last said it runs.
    desktops: DesktopStreams,
    sessions: HashMap<HostId, Vec<DesktopSession>>,
    quotas: Quotas,
    /// What the hosts answered for the spend charts.
    usage: Usage,
    mail: Entity<ChatSystem>,
    frame: FrameLink,
    timer: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentsSystem {
    pub fn new(mail: Entity<ChatSystem>, frame: FrameLink, cx: &mut Context<Self>) -> Self {
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
                    this.changed(cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        TranscriptSurface::bind_keys(cx);
        UsageSurface::bind_keys(cx);
        rho_terminal::ui::bind_keys(cx);
        rho_shell_view::ui::bind_keys(cx);
        let delivered = cx.subscribe(&mail, |this, _, event: &MailEvent, cx| match event {
            // The workset's transcript, at the step that wrote the mail.
            MailEvent::Inspect { agent, mail } => {
                let agent = *agent;
                let mail = mail.clone();
                let system = cx.weak_entity();
                this.frame.with(cx, move |frame, window, cx| {
                    let Some(system) = system.upgrade() else {
                        return;
                    };
                    let Some(workset) = system.read(cx).registry.agent_workset(agent) else {
                        frame.echo(
                            "the agent's transcript is not available",
                            StyleClass::SystemInfo,
                            cx,
                        );
                        return;
                    };
                    let scope = Scope::Workset(workset);
                    let card = frame.card_to_inspect(cx);
                    frame.open_inspection(scope.address(), card, window, cx);
                    if let Some(surface) = system
                        .read(cx)
                        .surfaces
                        .get(&scope)
                        .and_then(WeakEntity::upgrade)
                    {
                        surface.update(cx, |surface, cx| surface.inspect(agent, mail, window, cx));
                    }
                });
            }
            MailEvent::Changed { endpoint } => {
                let endpoint = *endpoint;
                let system = cx.weak_entity();
                cx.defer(move |cx| {
                    if let Some(system) = system.upgrade() {
                        system.update(cx, |system, cx| system.mail_caught_up(endpoint, cx));
                    }
                });
            }
            MailEvent::DraftSent(_) => {}
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
            timer: None,
            _tasks: vec![pump, desktops_pump],
            _subscriptions: vec![delivered],
        }
    }

    pub fn registry(&self) -> &AgentMap {
        &self.registry
    }

    /// The places `Start a project` offers on the host with this identity:
    /// the repositories its worksets were made on, then the directories its
    /// agents work in as they are, newest first and each once.
    pub fn known_project_places(&self, endpoint: EndpointId) -> Vec<Candidate> {
        let Some(host) = self.host_of(endpoint) else {
            return Vec::new();
        };
        let registry = self.registry();
        let mut agents: Vec<_> = registry
            .host_agents(host)
            .into_iter()
            .filter_map(|agent| registry.agent_identity(agent))
            .collect();
        agents.sort_by_key(|agent| std::cmp::Reverse(agent.entry.created_at));
        let repositories = agents
            .iter()
            .filter_map(|agent| Some((agent.repository()?.to_owned(), "repository")));
        let directories = agents.iter().filter_map(|agent| {
            let place = agent.place();
            place
                .workset
                .is_none()
                .then(|| (place.cwd.to_string(), "directory on the host"))
        });
        let mut places: Vec<Candidate> = Vec::new();
        for (value, description) in repositories.chain(directories) {
            if !places.iter().any(|known| known.value == value) {
                places.push(Candidate {
                    value,
                    description: description.to_owned(),
                });
            }
        }
        places
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

    /// Why the agent's host cannot take a command now, in the echo line's
    /// words: it is attached but away, or not attached at all. `hosts` is
    /// the frame's, which alone knows how each connection stands.
    pub(super) fn unreachable(&self, agent: AgentId, hosts: &Hosts) -> Option<&'static str> {
        let host = self
            .registry
            .host_of_agent(agent)
            .and_then(|host| hosts.get(host));
        steer::unreachable(host.map(|host| &host.status))
    }

    /// What the agent's work is called (`work_name`): the project and
    /// subject of the thread it works for, or of its workset's lead when
    /// it joined none, as a child does.
    fn work_name(&self, agent: AgentId, cx: &App) -> WorkName {
        let thread = self.agent_thread(agent, cx).or_else(|| {
            self.lead_of(agent)
                .filter(|lead| *lead != agent)
                .and_then(|lead| self.agent_thread(lead, cx))
        });
        let mail = self.mail.read(cx);
        let (project, subject) = thread
            .and_then(|thread| {
                let root = mail.host_chat(thread.host)?.root_of(thread.message);
                let project = mail.project_address(ThreadId {
                    host: thread.host,
                    message: root,
                });
                // A project's own root is not a piece of work's thread.
                let subject = (thread.message != root)
                    .then(|| mail.host_chat(thread.host)?.name(thread.message))
                    .flatten()
                    .filter(|subject| !subject.trim().is_empty());
                Some((project, subject))
            })
            .unwrap_or_default();
        let shared = self
            .registry
            .agent_workset(agent)
            .is_some_and(|workset| self.workset_agents(workset).len() > 1);
        WorkName {
            project,
            subject,
            code: self.registry.agent_code(agent),
            shared,
        }
    }

    fn host_label(&self, agent: AgentId) -> String {
        self.registry
            .host_of_agent(agent)
            .map(|host| self.registry.host_name(host).to_owned())
            .unwrap_or_default()
    }

    /// The agent by its work, with its host, for the status line, `find`
    /// and the prompts that ask for an agent.
    fn label(&self, agent: AgentId, cx: &App) -> (String, String) {
        (self.work_name(agent, cx).agent(), self.host_label(agent))
    }

    /// The agent's workset by its work, with its host: what the workset
    /// and its tools are called.
    fn work_label(&self, agent: AgentId, cx: &App) -> (String, String) {
        (self.work_name(agent, cx).work(), self.host_label(agent))
    }

    fn model_event(&mut self, host: HostId, msg: ModelMsg, cx: &mut Context<Self>) {
        match msg {
            ModelMsg::Loaded { agents, .. } => {
                for agent in self.registry.host_agents(host) {
                    self.transcripts.forget(agent);
                }
                self.registry.reset_host(host);
                self.registry.restore(agents);
                self.refresh_views(cx);
                self.note_open(cx);
                self.changed(cx);
            }
            ModelMsg::Changed { agents } => {
                let changed = self.registry.told(agents);
                if changed.is_empty() {
                    return;
                }
                self.refresh_views(cx);
                self.note_open(cx);
                self.changed(cx);
            }
            ModelMsg::Deleted { agents } => self.deleted(agents, cx),
            ModelMsg::Listed => {
                self.registry.set_listed(host);
                self.changed(cx);
            }
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
                let Some(mail) = inspection else {
                    return;
                };
                let surfaces: Vec<_> = self
                    .surfaces
                    .iter()
                    .filter(|(scope, _)| {
                        self.scope_agents(**scope)
                            .is_some_and(|(_, agents)| agents.contains(&agent_id))
                    })
                    .map(|(_, surface)| surface.clone())
                    .collect();
                self.frame.with(cx, move |_, window, cx| {
                    for surface in surfaces.iter().filter_map(WeakEntity::upgrade) {
                        surface.update(cx, |surface, cx| {
                            surface.read_inspection(mail, window, cx);
                        });
                    }
                });
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

    /// The agents of a workset, the one it started with first.
    fn workset_agents(&self, workset: WorksetId) -> Vec<AgentId> {
        let mut agents: Vec<_> = self
            .registry
            .known_agents()
            .copied()
            .filter(|agent| self.registry.agent_workset(*agent) == Some(workset))
            .filter_map(|agent| {
                let created = self.registry.mirrored(agent)?.identity.entry.created_at;
                Some((created, agent))
            })
            .collect();
        agents.sort();
        agents.into_iter().map(|(_, agent)| agent).collect()
    }

    /// The agent a workset is named after: the one it started with.
    fn lead_of(&self, agent: AgentId) -> Option<AgentId> {
        let workset = self.registry.agent_workset(agent)?;
        self.workset_agents(workset).first().copied()
    }

    /// The agent a scope's transcript is headed by, and every agent it
    /// shows, the lead first; `None` while none is known.
    fn scope_agents(&self, scope: Scope) -> Option<(AgentId, Vec<AgentId>)> {
        let agents = match scope {
            Scope::Workset(workset) => self.workset_agents(workset),
            Scope::Agent(agent) => self
                .registry
                .mirrored(agent)
                .map(|_| vec![agent])
                .unwrap_or_default(),
        };
        Some((*agents.first()?, agents))
    }

    fn scope_workset(&self, scope: Scope) -> Option<WorksetId> {
        match scope {
            Scope::Workset(workset) => Some(workset),
            Scope::Agent(agent) => self.registry.agent_workset(agent),
        }
    }

    /// What a transcript screen is called, with its host: the work of a
    /// workset, or of the one agent it is narrowed to.
    fn scope_label(&self, scope: Scope, cx: &App) -> Option<(String, String)> {
        let (shown, _) = self.scope_agents(scope)?;
        Some(match scope {
            Scope::Workset(_) => self.work_label(shown, cx),
            Scope::Agent(agent) => self.label(agent, cx),
        })
    }

    /// Lands a frame on an agent's transcript and shows what moved on
    /// every screen of it.
    fn land(&mut self, agent: AgentId, frame: TranscriptFrame, cx: &mut Context<Self>) {
        let change = self.transcripts.apply(agent, frame);
        if change.was_live && self.registry.mark_live(agent) {
            cx.notify();
        }
        for (scope, summary) in change.views {
            self.sync(scope, summary, cx);
        }
        self.ensure_timer(cx);
    }

    fn sync(&mut self, scope: Scope, summary: FrameSummary, cx: &mut Context<Self>) {
        let (Some(model), Some(state), Some((lead, _))) = (
            self.models.get(&scope),
            self.transcripts.view(&scope),
            self.scope_agents(scope),
        ) else {
            return;
        };
        self.show_brief(lead, model, cx);
        model.update(cx, |model, cx| {
            model.sync(
                state,
                summary,
                now_ms(),
                &|needed, cx| self.labels(lead, needed, cx),
                cx,
            )
        });
    }

    fn labels(&self, agent: AgentId, needed: HashSet<Label>, cx: &App) -> Labels {
        let mut labels = Labels::new();
        let mut writes = HashSet::new();
        let endpoint = self
            .registry
            .host_of_agent(agent)
            .and_then(|host| self.registry.host_endpoint(host));
        let mail = self.mail.read(cx);
        let log = endpoint.and_then(|endpoint| mail.host_chat(endpoint));
        for name in needed {
            match name {
                Label::Agent(id) => {
                    labels.insert(name, self.registry.agent_display_label(id));
                }
                Label::Delivery(id) => {
                    let Some(mail) = log.and_then(|log| log.message(id)) else {
                        continue;
                    };
                    let who = match mail.from {
                        rho_agent_types::Address::User => "you".to_owned(),
                        rho_agent_types::Address::Agent(from) => {
                            self.registry.agent_display_label(from)
                        }
                        rho_agent_types::Address::System { .. }
                        | rho_agent_types::Address::StartFailed => "the project".to_owned(),
                    };
                    labels.insert(
                        name,
                        format!("{who} · {}", rho_frame::thread::when(mail.at.0 as i64)),
                    );
                }
                Label::Write(id, version) => {
                    writes.insert((id, version));
                }
            }
        }
        if !writes.is_empty()
            && let Some(endpoint) = endpoint
        {
            for ((id, version), written) in mail.written_mail(endpoint, &writes) {
                let verb = match written.audience {
                    MailAudience::System => "reported",
                    MailAudience::User => "wrote to you",
                    MailAudience::Agent => "wrote to an agent",
                };
                let words = render::preview(&written.words);
                let caption = if words.is_empty() {
                    verb.to_owned()
                } else {
                    format!("{verb}: \"{words}\"")
                };
                labels.insert(Label::Write(id, version), caption);
            }
        }
        labels
    }

    /// Mail and the workset use separate streams. Refresh only captions
    /// that could not be resolved when their transcript rows arrived.
    fn mail_caught_up(&mut self, endpoint: EndpointId, cx: &mut Context<Self>) {
        let Some(host) = self.host_of(endpoint) else {
            return;
        };
        for (scope, model) in &self.models {
            let Some((lead, _)) = self.scope_agents(*scope) else {
                continue;
            };
            if self.registry.host_of_agent(lead) != Some(host) || !self.surfaces.contains_key(scope)
            {
                continue;
            }
            self.refresh_mail_labels(*scope, lead, model, cx);
            self.show_brief(lead, model, cx);
        }
    }

    /// The brief is the lead's origin message, which mail owns.
    fn show_brief(&self, lead: AgentId, model: &Entity<AgentModel>, cx: &mut Context<Self>) {
        let brief = self.brief(lead, cx);
        model.update(cx, |model, cx| model.set_brief(brief.as_deref(), cx));
    }

    fn refresh_mail_labels(
        &self,
        scope: Scope,
        lead: AgentId,
        model: &Entity<AgentModel>,
        cx: &mut Context<Self>,
    ) {
        let unresolved = model.read(cx).unresolved_mail().collect::<Vec<_>>();
        let available = self.labels(
            lead,
            unresolved.iter().map(|(_, label)| *label).collect(),
            cx,
        );
        if let Some(first) = unresolved
            .iter()
            .find_map(|(block, label)| available.contains_key(label).then_some(*block))
            && let Some(state) = self.transcripts.view(&scope)
        {
            let summary = FrameSummary {
                first_changed_block: Some(first),
                incremental: None,
            };
            model.update(cx, |model, cx| {
                model.sync(
                    state,
                    summary,
                    now_ms(),
                    &|needed, cx| {
                        let missing = needed
                            .into_iter()
                            .filter(|name| !available.contains_key(name))
                            .collect();
                        let mut labels = available.clone();
                        labels.extend(self.labels(lead, missing, cx));
                        labels
                    },
                    cx,
                )
            });
        }
    }

    /// A durable deletion removes host-owned history: the surfaces go too,
    /// so history cannot show what the host no longer has. A workset with
    /// an agent left keeps its transcript, without the ones that went.
    fn deleted(&mut self, agents: BTreeSet<AgentId>, cx: &mut Context<Self>) {
        for agent in &agents {
            self.transcripts.forget(*agent);
        }
        let worksets: BTreeSet<WorksetId> = agents
            .iter()
            .filter_map(|agent| self.registry.agent_workset(*agent))
            .collect();
        self.registry.forget_agents(&agents);
        let emptied: BTreeSet<WorksetId> = worksets
            .into_iter()
            .filter(|workset| self.workset_agents(*workset).is_empty())
            .collect();
        let gone = |scope: &Scope| match scope {
            Scope::Agent(agent) => agents.contains(agent),
            Scope::Workset(workset) => emptied.contains(workset),
        };
        let scopes: Vec<Scope> = self.models.keys().copied().filter(gone).collect();
        for scope in scopes {
            self.models.remove(&scope);
            self.surfaces.remove(&scope);
            self.transcripts.hide(&scope);
        }
        // Every screen of theirs: the transcripts, terminals, shells, and
        // desktops.
        let (agents, worksets) = (agents.clone(), emptied);
        self.frame.close_all(
            move |address| {
                AgentAddress::from_address(address).is_some_and(|at| agents.contains(&at.agent))
                    || WorksetAddress::from_address(address)
                        .is_some_and(|at| worksets.contains(&at.0))
            },
            cx,
        );
        self.refresh_views(cx);
        self.note_open(cx);
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        self.frame.changed(cx);
    }

    /// A workset on screen shows every agent in it: one that joined is
    /// merged in, one that left is taken out.
    fn refresh_views(&mut self, cx: &mut Context<Self>) {
        let scopes: Vec<Scope> = self.models.keys().copied().collect();
        for scope in scopes {
            let Some((lead, agents)) = self.scope_agents(scope) else {
                continue;
            };
            for agent in &agents {
                self.seed(*agent, cx);
            }
            let summary = self.transcripts.show(scope, lead, agents);
            if summary.first_changed_block.is_some() {
                self.sync(scope, summary, cx);
            }
        }
    }

    /// Tells the model which transcripts to follow and every host which
    /// agents to stream live: those of the transcripts on screen.
    fn note_open(&mut self, _cx: &mut Context<Self>) {
        self.surfaces
            .retain(|_, surface| surface.upgrade().is_some());
        let open: BTreeSet<AgentId> = self
            .surfaces
            .keys()
            .filter_map(|scope| self.scope_agents(*scope))
            .flat_map(|(_, agents)| agents)
            .collect();
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

    /// Opens an agent's transcript from the disk copy, with the runtime
    /// state already heard, and shows it on the screens that show the
    /// agent. An agent already open is left as it is.
    fn seed(&mut self, agent: AgentId, cx: &mut Context<Self>) {
        if self.transcripts.is_open(&agent) || self.registry.agent_workset(agent).is_none() {
            return;
        }
        let rows = self.client.read_transcript(agent);
        let Some(mut moved) = self.transcripts.seed(agent, &rows) else {
            return;
        };
        if let Some(heard) = self.registry.agent_facts(agent).runtime {
            moved.extend(
                self.transcripts
                    .apply(agent, TranscriptFrame::Runtime(heard.state))
                    .views,
            );
        }
        for (scope, summary) in moved {
            self.sync(scope, summary, cx);
        }
    }

    fn model(&mut self, scope: Scope, cx: &mut Context<Self>) -> Entity<AgentModel> {
        if let Some(model) = self.models.get(&scope) {
            return model.clone();
        }
        let link = self
            .scope_agents(scope)
            .and_then(|(lead, _)| self.agents_for(lead))
            .unwrap_or_else(AgentsLink::detached);
        let model = cx.new(|cx| AgentModel::new(link, cx));
        self._subscriptions.push(cx.subscribe(
            &model,
            move |this, model, event: &AgentModelEvent, cx| {
                if matches!(event, AgentModelEvent::Loaded)
                    && this.models.get(&scope) == Some(&model)
                {
                    this.sync(scope, FrameSummary::nothing(), cx);
                    if let Some((lead, _)) = this.scope_agents(scope) {
                        this.refresh_mail_labels(scope, lead, &model, cx);
                    }
                    this.ensure_timer(cx);
                }
            },
        ));
        self.models.insert(scope, model.clone());
        model
    }

    /// The model of a scope, loaded: its agents' transcripts seeded from
    /// the disk copy and merged, the bulk load started, and what moved
    /// since shown.
    fn loaded_model(&mut self, scope: Scope, cx: &mut Context<Self>) -> Entity<AgentModel> {
        let model = self.model(scope, cx);
        let Some((lead, agents)) = self.scope_agents(scope) else {
            return model;
        };
        for agent in &agents {
            self.seed(*agent, cx);
        }
        let summary = self.transcripts.show(scope, lead, agents);
        if !model.read(cx).initial_load_started()
            && let Some(state) = self.transcripts.view(&scope).cloned()
        {
            let needed = state
                .blocks
                .iter()
                .filter_map(|block| Label::for_block(block))
                .collect();
            let labels = self.labels(lead, needed, cx);
            model.update(cx, |model, cx| {
                model.start_initial_load(state, labels, now_ms(), cx)
            });
        } else {
            self.sync(scope, summary, cx);
        }
        self.show_brief(lead, &model, cx);
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
            self.frame
                .echo(steer::NOT_ATTACHED, StyleClass::SystemInfo, cx);
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

    /// What a surface under the agent dials with: its host's link and its
    /// encoded id. `None` while the host is away.
    fn workset_of(&self, agent: AgentId) -> Option<(&Link, String)> {
        let link = self.links.get(&self.registry.host_of_agent(agent)?)?;
        Some((link, agent.encoded()))
    }

    /// Whether the agent's host is attached; the echo line says so when
    /// it is not.
    fn attached(&self, agent: AgentId, cx: &mut App) -> bool {
        let attached = self.workset_of(agent).is_some();
        if !attached {
            self.frame
                .echo(steer::NOT_ATTACHED, StyleClass::SystemInfo, cx);
        }
        attached
    }

    fn agent_thread(&self, agent: AgentId, cx: &App) -> Option<ThreadId> {
        let mirrored = self.registry.mirrored(agent)?;
        let host = self.registry.host_endpoint(mirrored.host)?;
        let chat = self.mail.read(cx).host_chat(host)?;
        let message = chat.agent_thread(agent).or_else(|| {
            mirrored
                .identity
                .origin()
                .map(|origin| chat.thread_of(origin))
        })?;
        Some(ThreadId { host, message })
    }

    /// The work an agent does, as `<project> · <thread subject>`: its
    /// workset's thread. `None` while the work has no thread.
    pub fn agent_work(&self, agent: AgentId, cx: &App) -> Option<String> {
        let lead = self.lead_of(agent).unwrap_or(agent);
        let (subject, project) = self
            .mail
            .read(cx)
            .thread_name(self.agent_thread(lead, cx)?, cx)?;
        Some(format!("{project} · {subject}"))
    }

    fn brief(&self, agent: AgentId, cx: &App) -> Option<String> {
        let mirrored = self.registry.mirrored(agent)?;
        let host = self.registry.host_endpoint(mirrored.host)?;
        let chat = self.mail.read(cx).host_chat(host)?;
        let origin = mirrored.identity.origin()?;
        let brief = chat.message(origin)?.text();
        (!brief.trim().is_empty()).then_some(brief)
    }

    fn open_transcript(
        &mut self,
        scope: Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnySurface {
        let model = self.loaded_model(scope, cx);
        let system = cx.entity();
        let surface = cx.new(|cx| TranscriptSurface::new(system, scope, model, window, cx));
        self.surfaces.insert(scope, surface.downgrade());
        self._subscriptions
            .push(cx.observe_release(&surface, move |this, _, cx| {
                this.surfaces.remove(&scope);
                this.note_open(cx);
            }));
        self.note_open(cx);
        AnySurface::new(surface)
    }

    /// Who an id a host lists a tool under is, if it is an agent of that
    /// host this window holds.
    fn tool_owner(&self, host: HostId, encoded: &str) -> Option<AgentId> {
        let agent = AgentId::from_encoded(encoded).ok()?;
        (self.registry.host_of_agent(agent) == Some(host)).then_some(agent)
    }

    /// The desktops running in the agent's workset, as its host last said,
    /// and under which agent each runs.
    fn desktops_of(&self, agent: AgentId) -> Vec<(AgentId, String)> {
        let (Some(host), Some(workset)) = (
            self.registry.host_of_agent(agent),
            self.registry.agent_workset(agent),
        ) else {
            return Vec::new();
        };
        self.sessions
            .get(&host)
            .into_iter()
            .flatten()
            .filter_map(|session| {
                let owner = self.tool_owner(host, &session.agent)?;
                (self.registry.agent_workset(owner) == Some(workset))
                    .then(|| (owner, session.name.clone()))
            })
            .collect()
    }

    /// Opens a terminal of the agent's workset: with `new` a fresh one
    /// under the agent; else the one that runs, asking which when several
    /// do, and starting one when none does.
    fn open_terminal(
        &mut self,
        agent: AgentId,
        new: bool,
        card: Option<CardInspection>,
        cx: &mut Context<Self>,
    ) {
        if !self.attached(agent, cx) {
            return;
        }
        let Some((link, _)) = self.workset_of(agent) else {
            return;
        };
        let running = rho_terminal::channel::list(link, None);
        cx.spawn(async move |this, cx| {
            let running = running.await;
            let _ = this.update(cx, |this, cx| {
                let running = match running {
                    Ok(running) => running,
                    Err(error) => {
                        return this.frame.echo(
                            format!("terminal: {error:#}"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                    }
                };
                let (Some(host), workset) = (
                    this.registry.host_of_agent(agent),
                    this.registry.agent_workset(agent),
                ) else {
                    return;
                };
                let in_workset = running
                    .into_iter()
                    .filter_map(|info| {
                        let owner = this.tool_owner(host, &info.agent)?;
                        (this.registry.agent_workset(owner) == workset).then_some((owner, info))
                    })
                    .collect();
                match commands::pick_terminal(agent, in_workset, new) {
                    commands::TerminalPick::Start(terminal) => {
                        this.open_tool(AgentAddress::terminal(agent, terminal), card, cx)
                    }
                    commands::TerminalPick::Open(owner, terminal) => {
                        this.open_tool(AgentAddress::terminal(owner, terminal), card, cx)
                    }
                    commands::TerminalPick::Ask(running) => {
                        let choices = running
                            .into_iter()
                            .map(|(owner, info)| {
                                let address = AgentAddress::terminal(owner, info.terminal_id);
                                let value = this.tool_title(&address, cx);
                                (value, info.title, address)
                            })
                            .collect();
                        this.ask_tool("terminal:", choices, card, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// What a tool is called in `Places` and the prompts that offer
    /// it: its workset's work and the tool, `tide · Flaky clock test ·
    /// term 1`. A tool is the workset's, whichever agent started it.
    fn tool_title(&self, at: &AgentAddress, cx: &App) -> String {
        work_name::tool_title(&self.work_label(at.agent, cx).0, &at.part)
    }

    fn open_tool(&self, at: AgentAddress, card: Option<CardInspection>, cx: &mut App) {
        let address = at.address();
        self.frame.with(cx, move |frame, window, cx| {
            frame.open_inspection(address, card, window, cx);
        });
    }

    /// Asks which of several running tools to open: each by its title,
    /// with a word on what it shows.
    fn ask_tool(
        &self,
        question: &'static str,
        choices: Vec<(String, String, AgentAddress)>,
        card: Option<CardInspection>,
        cx: &mut App,
    ) {
        let choices = Rc::new(choices);
        self.frame.with(cx, move |frame, window, cx| {
            let offered = choices.clone();
            frame.open_prompt(
                question,
                Rc::new(move |_, input, _| {
                    let needle = input.trim().to_lowercase();
                    offered
                        .iter()
                        .filter(|(value, description, _)| {
                            value.to_lowercase().contains(&needle)
                                || description.to_lowercase().contains(&needle)
                        })
                        .map(|(value, description, _)| Candidate {
                            value: value.clone(),
                            description: description.clone(),
                        })
                        .collect()
                }),
                Rc::new(move |frame, input, window, cx| {
                    let input = input.trim();
                    match choices.iter().find(|(value, _, _)| value == input) {
                        Some((_, _, at)) => {
                            frame.open_inspection(at.address(), card.clone(), window, cx)
                        }
                        None => frame.echo(
                            &format!("{question} none called {input:?}"),
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

    /// Stops the agent's shell on its host, and closes its surface once
    /// the host has.
    fn close_shell(&mut self, agent: AgentId, cx: &mut Context<Self>) {
        if !self.attached(agent, cx) {
            return;
        }
        let Some((link, id)) = self.workset_of(agent) else {
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

    /// Opens the workset's one desktop, or asks which of its desktops.
    fn open_desktop(
        &mut self,
        agent: AgentId,
        card: Option<CardInspection>,
        cx: &mut Context<Self>,
    ) {
        let desktops: Vec<AgentAddress> = self
            .desktops_of(agent)
            .into_iter()
            .filter_map(|(owner, name)| AgentAddress::desktop(owner, name))
            .collect();
        match desktops.as_slice() {
            [] => self
                .frame
                .echo("the workset runs no desktop", StyleClass::SystemInfo, cx),
            [desktop] => self.open_tool(desktop.clone(), card, cx),
            _ => {
                let choices = desktops
                    .into_iter()
                    .map(|at| (self.tool_title(&at, cx), "desktop".to_owned(), at))
                    .collect();
                self.ask_tool("desktop:", choices, card, cx);
            }
        }
    }

    pub(crate) fn usage(&self) -> &Usage {
        &self.usage
    }

    pub(crate) fn frame(&self) -> &FrameLink {
        &self.frame
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
        let chat = self.mail.read(cx);
        point
            .subject()
            .and_then(|address| chat.thread_of(address))
            .or_else(|| chat.thread_of(&point.surface))
    }
}

impl EventEmitter<UsageMoved> for AgentsSystem {}

impl System for AgentsSystem {
    fn kinds(&self) -> &'static [&'static str] {
        KINDS
    }

    /// An agent or workset this window does not hold may be on a host
    /// that has not yet listed its agents, and a desktop on one that has
    /// not yet said which it runs: those wait. Only what a host that has
    /// said is without is gone.
    fn open(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) -> Opening {
        if let Some(chart) = UsageChart::from_address(address) {
            let system = cx.entity();
            let surface = cx.new(|cx| UsageSurface::new(system, chart, window, cx));
            return Opening::Surface(AnySurface::new(surface));
        }
        let unknown = || {
            let unlisted = self.registry.unlisted_hosts();
            match unlisted.is_empty() {
                true => Opening::Gone,
                false => Opening::Waiting(unlisted),
            }
        };
        if let Some(scope @ Scope::Workset(_)) = Scope::from_address(address) {
            return match self.scope_agents(scope) {
                Some(_) => Opening::Surface(self.open_transcript(scope, window, cx)),
                None => unknown(),
            };
        }
        let Some(AgentAddress { agent, part }) = AgentAddress::from_address(address) else {
            return Opening::Gone;
        };
        let Some(host) = self.registry.host_of_agent(agent) else {
            return unknown();
        };
        let Some((link, id)) = self.workset_of(agent) else {
            return Opening::Gone;
        };
        let label = self.work_label(agent, cx).0;
        let surface = match part {
            Part::Transcript => self.open_transcript(Scope::Agent(agent), window, cx),
            Part::Terminal(terminal) => AnySurface::new(
                cx.new(|cx| TerminalSurface::new(link, id, label, terminal, window, cx)),
            ),
            Part::Shell => {
                AnySurface::new(cx.new(|cx| ShellSurface::new(link, id, label, window, cx)))
            }
            Part::Desktop(name) => {
                if !self.sessions.contains_key(&host) {
                    return Opening::Waiting(vec![host]);
                }
                if !self
                    .desktops_of(agent)
                    .iter()
                    .any(|(owner, held)| *owner == agent && *held == name)
                {
                    return Opening::Gone;
                }
                AnySurface::new(cx.new(|cx| DesktopSurface::new(link, id, label, name, window, cx)))
            }
        };
        Opening::Surface(surface)
    }

    /// A chart in view asks the hosts for what it draws.
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

    /// The commands about agents are on every screen once any host has
    /// agents to act on; the screen says only which they act on.
    /// Switching the Claude account and opening the usage charts are on
    /// every screen always: with no host attached the first says so.
    fn commands(
        &mut self,
        point: &Point,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Command> {
        let this = cx.entity();
        let reported = this.downgrade();
        let hosts: Vec<String> = self
            .registry
            .hosts()
            .map(|(_, name)| name.to_owned())
            .collect();
        let on = match hosts.as_slice() {
            [] => "no host attached".to_owned(),
            [host] => host.clone(),
            _ => "pick host…".to_owned(),
        };
        let account = accounts::command(
            Rc::new(move |host, cx| {
                reported
                    .upgrade()
                    .map(|system| system.read(cx).quotas.summaries(host).to_vec())
                    .unwrap_or_default()
            }),
            on,
        );
        let usage = Command::simple(&OPEN_USAGE, |frame, window, cx| {
            frame.open(UsageChart::FIRST.address(), window, cx)
        })
        .target(UsageChart::FIRST.title());
        let mut commands = match hosts.is_empty() {
            true => Vec::new(),
            false => self.agent_commands(this, self.here(point, cx), cx),
        };
        commands.extend([account, usage]);
        commands
    }

    fn name(
        &self,
        address: &Address,
        _marks: &rho_frame::Marks,
        cx: &App,
    ) -> Option<(String, String)> {
        if let Some(chart) = UsageChart::from_address(address) {
            return Some((chart.title(), "usage".to_owned()));
        }
        if let Some(scope @ Scope::Workset(_)) = Scope::from_address(address) {
            let (lead, _) = self.scope_agents(scope)?;
            return Some(self.work_label(lead, cx));
        }
        let at = AgentAddress::from_address(address)?;
        self.registry.mirrored(at.agent)?;
        let (label, host) = self.label(at.agent, cx);
        Some(match at.part {
            Part::Transcript => (label, host),
            _ => (self.tool_title(&at, cx), host),
        })
    }

    fn agent_name(&self, agent: AgentId, cx: &App) -> Option<String> {
        self.registry.mirrored(agent)?;
        Some(self.label(agent, cx).0)
    }

    /// The usage charts, each found by `usage` too. Transcripts and the
    /// tools under them are places once visited ([`rho_frame::Frame`]
    /// remembers them), so an unvisited one is not listed.
    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        UsageChart::all()
            .map(|chart| Found {
                address: chart.address(),
                title: chart.title(),
                description: String::new(),
                thread: None,
                recency: 0,
                also: vec!["usage".to_owned()],
            })
            .collect()
    }
}

/// Whom a transcript screen is named after: its workset's lead, and the
/// agent whose folder and runtime it shows.
pub(super) struct ScopeName {
    pub workset: AgentId,
    pub shown: AgentId,
}
