//! The model: the fold, the cursors and the disk copy, on their own
//! thread, and [`AgentsClient`], the window's handle on it.
//!
//! The main thread draws. Everything between a host's agents session and
//! what is drawn happens here: mail is folded into each agent's digest,
//! the directory into what each agent is, the rows of the open
//! transcripts' worksets are read and handed up, and all of it is written
//! to the copy. A reconnect's catch-up is many pages, and the main thread
//! hears one message for the whole of it.
//!
//! The connection itself stays where it is, on the shared tokio runtime:
//! the socket was never the cost, and the workspace-file, terminal, shell
//! and realtime channels hang off it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use rho_agent_hosts::HostStream;
use rho_agent_types::{Address, AgentId, Mail, MailId, UnixMs, WorksetId, WorksetKind, Wpos};

use crate::fold::{AgentIdentity, Digest, MirroredAgent, TranscriptRow};
use crate::protocol::rho2::{AgentEntry, ClientFrame, LogEvent, ServerFrame, WorksetEntry};
use crate::protocol::transcript::{InferenceState, Live, RuntimeState};
use crate::stream::{AgentCommands, AgentEvent, AgentFrame, AgentStream};
use crate::{HostId, Verdict};

/// What the main thread hears from the model.
pub enum ModelMsg {
    /// Every agent this client holds for a host, from the disk copy or
    /// after the copy started over. What the main thread had for the host
    /// is gone; this is what there is instead.
    Loaded {
        agents: Vec<MirroredAgent>,
        verdicts: Vec<(AgentId, Verdict)>,
    },
    /// Agents the directory or their mail moved, as they now stand. One
    /// message per frame once caught up, and one for a whole catch-up.
    Changed { agents: Vec<MirroredAgent> },
    /// Agents gone from their host's directory, and their copy with them.
    Deleted { agents: BTreeSet<AgentId> },
    /// Rows and mail of an agent the main thread follows, for the fold
    /// behind its transcript. Nothing else carries rows.
    Rows {
        agent_id: AgentId,
        rows: Vec<TranscriptRow>,
    },
    /// What an agent's runtime is doing, for every agent.
    Runtime {
        agent_id: AgentId,
        state: RuntimeState,
    },
    /// The live tail of an agent the window focuses.
    Live { agent_id: AgentId, live: Live },
    /// Whose agents these are: the host's database seed, which agent ids
    /// are encoded with, and the last agent-id counter it handed out. Once
    /// each time the host's agents stream opens, after anything it resets.
    Host {
        machine_seed: u64,
        agent_counter: u64,
    },
    /// Which provider accounts the host's agents may run on.
    Auth { auth: crate::protocol::AuthState },
    /// The host's quota: every account's latest usage.
    QuotaUsage {
        summaries: Vec<crate::protocol::QuotaSummary>,
    },
}

pub struct ModelEvent {
    pub host: HostId,
    pub msg: ModelMsg,
}

/// What the main thread asks of the model.
pub enum ModelCommand {
    /// An agent host the workspace attached, named before it is dialled: the
    /// name is how the disk copy knows it across restarts.
    AttachHost {
        host: HostId,
        name: String,
    },
    /// The way to send that agent host a command, once it has been dialled.
    HostCommands {
        host: HostId,
        commands: AgentCommands,
    },
    DetachHost(HostId),
    /// The agents whose rows the main thread wants, replaced wholesale.
    Follow(BTreeSet<AgentId>),
}

/// What reaches the model, in the order the main thread and the
/// connections produced it. One channel, so a host is always known
/// before its frames arrive.
// Under test the model is driven inline and the thread's loop is not
// built, so nothing reads what the shell puts on this queue.
#[cfg_attr(test, allow(dead_code))]
pub enum ToModel {
    Event(AgentEvent),
    Command(ModelCommand),
}

/// Where this client stands in reading one workset's log.
#[derive(Debug, Default)]
struct Reading {
    /// One past the newest row held.
    next: Wpos,
    /// The log is being read up to its end; the follow waits for it.
    reading: bool,
}

/// Where this client stands with one host.
struct HostModel {
    name: String,
    commands: Option<AgentCommands>,
    /// The stream opened before this client could speak back. The follows
    /// go out the moment it can.
    hello_pending: bool,
    /// The stream is open: `Hello` came and nothing has closed it.
    open: bool,
    machine_seed: u64,
    /// The newest mail held.
    mail: MailId,
    /// The newest mail on the host when the stream opened. Until `mail`
    /// reaches it the main thread hears nothing of the digests: a catch-up
    /// is not news, it is arrears.
    mail_head: MailId,
    /// Agents moved since the main thread last heard. Empty except during
    /// a catch-up.
    pending: BTreeSet<AgentId>,
    /// The kind of each workset of the directory.
    kinds: BTreeMap<WorksetId, WorksetKind>,
    /// Mail of agents the directory has not named (yet).
    strays: BTreeMap<AgentId, Digest>,
    /// The worksets of the agents followed, and how far each is read.
    worksets: BTreeMap<WorksetId, Reading>,
}

impl HostModel {
    fn new(name: String) -> Self {
        Self {
            name,
            commands: None,
            hello_pending: false,
            open: false,
            machine_seed: 0,
            mail: MailId::default(),
            mail_head: MailId::default(),
            pending: BTreeSet::new(),
            kinds: BTreeMap::new(),
            strays: BTreeMap::new(),
            worksets: BTreeMap::new(),
        }
    }

    /// The worksets read to their end, from where each stands: what the
    /// host is to push.
    fn follow_worksets(&self) {
        let Some(commands) = self.commands.as_ref().filter(|_| self.open) else {
            return;
        };
        let from = self
            .worksets
            .iter()
            .filter(|(_, reading)| !reading.reading)
            .map(|(workset, reading)| (*workset, reading.next))
            .collect();
        commands.send(ClientFrame::FollowWorksets { from });
    }

    /// Reads a workset's log from where this client stands to its end.
    fn read(&mut self, workset: WorksetId) {
        let (Some(commands), true) = (self.commands.as_ref(), self.open) else {
            return;
        };
        let reading = self.worksets.entry(workset).or_default();
        reading.reading = true;
        commands.read_log(workset, reading.next);
    }
}

/// The fold of every agent, the cursors, and who is followed.
pub struct Model {
    hosts: HashMap<HostId, HostModel>,
    /// The directory, with each agent's digest.
    agents: BTreeMap<AgentId, MirroredAgent>,
    followed: BTreeSet<AgentId>,
    /// The disk copy, read once: hosts are attached one at a time and each
    /// takes the agents filed under its name.
    stored: Option<crate::cache::Loaded>,
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    pub fn new() -> Self {
        Self {
            hosts: HashMap::new(),
            agents: BTreeMap::new(),
            followed: BTreeSet::new(),
            stored: None,
        }
    }

    pub fn command(&mut self, command: ModelCommand) -> Vec<ModelEvent> {
        match command {
            ModelCommand::AttachHost { host, name } => self.attach(host, name),
            ModelCommand::HostCommands { host, commands } => {
                let Some(slot) = self.hosts.get_mut(&host) else {
                    return Vec::new();
                };
                slot.commands = Some(commands);
                if std::mem::take(&mut slot.hello_pending) {
                    self.follow(host);
                }
                Vec::new()
            }
            ModelCommand::DetachHost(host) => {
                // The disk copy keeps the host's mail: a host detached is
                // not a host disowned, and the copy is what a later attach
                // starts from.
                self.hosts.remove(&host);
                self.agents.retain(|_, agent| agent.host != host);
                Vec::new()
            }
            ModelCommand::Follow(agents) => {
                self.followed = agents;
                let hosts = self.hosts.keys().copied().collect::<Vec<_>>();
                for host in hosts {
                    self.refollow(host);
                }
                Vec::new()
            }
        }
    }

    /// A host the workspace attached, with what the last session left of
    /// it: the cursor `FollowMail` will send, and the agents already
    /// folded.
    pub fn attach(&mut self, host: HostId, name: String) -> Vec<ModelEvent> {
        let mut slot = HostModel::new(name.clone());
        let stored = self.stored.get_or_insert_with(|| {
            crate::cache::flush();
            crate::cache::load()
        });
        if let Some(cursor) = stored.hosts.iter().find(|cursor| cursor.name == name) {
            slot.machine_seed = cursor.machine_seed;
            slot.mail = cursor.mail;
            slot.mail_head = cursor.mail;
        }
        let mut agents = Vec::new();
        let mut verdicts = Vec::new();
        for loaded in &stored.agents {
            if loaded.host != name {
                continue;
            }
            let agent_id = loaded.identity.agent_id();
            slot.kinds
                .insert(loaded.identity.entry.workset, loaded.identity.kind.clone());
            agents.push(MirroredAgent {
                host,
                identity: loaded.identity.clone(),
                digest: loaded.digest.clone(),
            });
            if let Some(verdict) = loaded.verdict {
                verdicts.push((agent_id, verdict));
            }
        }
        self.hosts.insert(host, slot);
        for agent in &agents {
            self.agents.insert(agent.agent_id(), agent.clone());
        }
        if agents.is_empty() && verdicts.is_empty() {
            return Vec::new();
        }
        vec![ModelEvent {
            host,
            msg: ModelMsg::Loaded { agents, verdicts },
        }]
    }

    /// One frame from a host's agents stream.
    pub fn ingest(&mut self, host: HostId, frame: AgentFrame) -> Vec<ModelEvent> {
        if !self.hosts.contains_key(&host) {
            return Vec::new();
        }
        let event = |msg| vec![ModelEvent { host, msg }];
        match frame {
            AgentFrame::Server(frame) => match frame {
                ServerFrame::Hello {
                    machine_seed,
                    mail_head,
                    agent_counter,
                } => {
                    let mut out = self.hello(host, machine_seed, mail_head);
                    out.push(ModelEvent {
                        host,
                        msg: ModelMsg::Host {
                            machine_seed,
                            agent_counter,
                        },
                    });
                    out
                }
                ServerFrame::Auth { auth } => event(ModelMsg::Auth { auth }),
                ServerFrame::QuotaUsage { summaries } => event(ModelMsg::QuotaUsage { summaries }),
                ServerFrame::Directory { worksets, agents } => {
                    self.directory(host, worksets, agents)
                }
                ServerFrame::Workset(entry) => self.workset(host, entry),
                ServerFrame::WorksetDeleted { workset } => self.workset_deleted(host, workset),
                ServerFrame::Agent(entry) => {
                    let mut out = self.agent(host, entry);
                    self.refollow(host);
                    out.extend(self.announce(host));
                    out
                }
                ServerFrame::Mail { mails } => self.mail(host, mails),
                ServerFrame::Rows { workset, rows } => self.rows(host, workset, rows),
                ServerFrame::Live { agent_id, live } => {
                    if !self.agents.contains_key(&agent_id) {
                        return Vec::new();
                    }
                    event(ModelMsg::Live { agent_id, live })
                }
                ServerFrame::Status { agent_id, state } => self.status(host, agent_id, state),
                // Nothing here asks for a detail body: the transcript
                // draws a call's line and never its output.
                ServerFrame::Detail { .. } => Vec::new(),
            },
            AgentFrame::Log { workset, page } => {
                let done = page.next.is_none();
                let out = self.rows(host, workset, page.rows);
                if done {
                    let slot = self.hosts.get_mut(&host).expect("the host is here");
                    if let Some(reading) = slot.worksets.get_mut(&workset) {
                        reading.reading = false;
                        slot.follow_worksets();
                    }
                }
                out
            }
            AgentFrame::LogFailed { workset } => {
                let slot = self.hosts.get_mut(&host).expect("the host is here");
                if let Some(reading) = slot.worksets.get_mut(&workset) {
                    reading.reading = false;
                }
                Vec::new()
            }
        }
    }

    /// An agents stream that has opened: how far the host's mail runs, and
    /// whether it is the database this copy counts in. Asks for the rest.
    fn hello(&mut self, host: HostId, machine_seed: u64, mail_head: MailId) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        slot.mail_head = mail_head;
        slot.open = true;
        // A host whose database is not the one this client copied, or
        // whose mail log is shorter than the copy: the copy starts over.
        let started_over = slot.machine_seed != machine_seed || slot.mail > mail_head;
        let mut out = Vec::new();
        if started_over {
            slot.machine_seed = machine_seed;
            slot.mail = MailId::default();
            slot.pending.clear();
            slot.kinds.clear();
            slot.strays.clear();
            for reading in slot.worksets.values_mut() {
                reading.next = Wpos::ZERO;
            }
            crate::cache::reset_host(&slot.name);
            self.agents.retain(|_, agent| agent.host != host);
            out.push(ModelEvent {
                host,
                msg: ModelMsg::Loaded {
                    agents: Vec::new(),
                    verdicts: Vec::new(),
                },
            });
        }
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        crate::cache::write_host(&slot.name, machine_seed);
        tracing::debug!(
            host = slot.name,
            since = slot.mail.0,
            head = mail_head.0,
            started_over,
            "following the host's mail"
        );
        if slot.commands.is_some() {
            self.follow(host);
        } else {
            slot.hello_pending = true;
        }
        out
    }

    /// Everything a new stream is told: the mail since the cursor, and the
    /// followed worksets read again from where they stand.
    fn follow(&mut self, host: HostId) {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        if let Some(commands) = &slot.commands {
            commands.send(ClientFrame::FollowMail { since: slot.mail });
        }
        let worksets = slot.worksets.keys().copied().collect::<Vec<_>>();
        for workset in worksets {
            slot.read(workset);
        }
        slot.follow_worksets();
    }

    /// The worksets of the followed agents on a host, after the followed
    /// set or the directory moved: a workset newly needed is read from
    /// where the copy stands, one no longer needed is let go.
    fn refollow(&mut self, host: HostId) {
        let needed = self
            .followed
            .iter()
            .filter_map(|agent_id| self.agents.get(agent_id))
            .filter(|agent| agent.host == host)
            .map(|agent| agent.identity.entry.workset)
            .collect::<BTreeSet<_>>();
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        let held = slot.worksets.keys().copied().collect::<BTreeSet<_>>();
        if held == needed {
            return;
        }
        slot.worksets.retain(|workset, _| needed.contains(workset));
        for workset in needed.difference(&held) {
            slot.worksets.insert(
                *workset,
                Reading {
                    next: crate::cache::rows_next(*workset),
                    reading: false,
                },
            );
            slot.read(*workset);
        }
        slot.follow_worksets();
    }

    /// The directory, whole: what it no longer names is gone.
    fn directory(
        &mut self,
        host: HostId,
        worksets: Vec<WorksetEntry>,
        agents: Vec<AgentEntry>,
    ) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        slot.kinds = worksets
            .into_iter()
            .map(|entry| (entry.workset, entry.kind))
            .collect();
        let named = agents
            .iter()
            .map(|entry| entry.agent)
            .collect::<BTreeSet<_>>();
        let departed = self
            .agents
            .values()
            .filter(|agent| agent.host == host && !named.contains(&agent.agent_id()))
            .map(MirroredAgent::agent_id)
            .collect::<BTreeSet<_>>();
        let mut out = self.depart(host, departed);
        for entry in agents {
            out.extend(self.agent(host, entry));
        }
        self.refollow(host);
        out.extend(self.announce(host));
        out
    }

    fn workset(&mut self, host: HostId, entry: WorksetEntry) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        slot.kinds.insert(entry.workset, entry.kind.clone());
        let mut moved = Vec::new();
        for agent in self.agents.values_mut() {
            if agent.host == host
                && agent.identity.entry.workset == entry.workset
                && agent.identity.kind != entry.kind
            {
                agent.identity.kind = entry.kind.clone();
                moved.push(agent.agent_id());
            }
        }
        self.store(&moved);
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        slot.pending.extend(moved);
        self.announce(host)
    }

    fn workset_deleted(&mut self, host: HostId, workset: WorksetId) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        slot.kinds.remove(&workset);
        let departed = self
            .agents
            .values()
            .filter(|agent| agent.host == host && agent.identity.entry.workset == workset)
            .map(MirroredAgent::agent_id)
            .collect();
        crate::cache::forget_workset(workset);
        let out = self.depart(host, departed);
        self.refollow(host);
        out
    }

    /// Agents gone from a host's directory: forgotten here and on disk.
    fn depart(&mut self, host: HostId, departed: BTreeSet<AgentId>) -> Vec<ModelEvent> {
        if departed.is_empty() {
            return Vec::new();
        }
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        for agent_id in &departed {
            self.agents.remove(agent_id);
            self.followed.remove(agent_id);
            slot.pending.remove(agent_id);
            slot.strays.remove(agent_id);
        }
        crate::cache::forget_agents(departed.iter().copied().collect());
        // A later attach must not reuse the startup copy of a departed
        // agent.
        self.stored = None;
        vec![ModelEvent {
            host,
            msg: ModelMsg::Deleted { agents: departed },
        }]
    }

    /// One directory entry, as it now stands.
    fn agent(&mut self, host: HostId, entry: AgentEntry) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        let agent_id = entry.agent;
        // A workset the directory has not described yet is isolated, the
        // kind every policy-made workset is.
        let kind = slot
            .kinds
            .get(&entry.workset)
            .cloned()
            .unwrap_or(WorksetKind::Isolated);
        let identity = AgentIdentity { entry, kind };
        match self.agents.get_mut(&agent_id) {
            Some(held) if held.identity == identity => return Vec::new(),
            Some(held) => held.identity = identity,
            None => {
                let digest = slot.strays.remove(&agent_id).unwrap_or_default();
                self.agents.insert(
                    agent_id,
                    MirroredAgent {
                        host,
                        identity,
                        digest,
                    },
                );
            }
        }
        self.store(&[agent_id]);
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        slot.pending.insert(agent_id);
        Vec::new()
    }

    /// The agents named, written to the copy as they now stand.
    fn store(&self, agents: &[AgentId]) {
        crate::cache::write_agents(
            agents
                .iter()
                .filter_map(|agent_id| {
                    let agent = self.agents.get(agent_id)?;
                    let host = self.hosts.get(&agent.host)?;
                    Some((
                        *agent_id,
                        crate::cache::StoredAgent {
                            host: host.name.clone(),
                            identity: agent.identity.clone(),
                            stop: agent.digest.stop(),
                        },
                    ))
                })
                .collect(),
        );
    }

    /// A run of the host's mail log: folded into the digests of the agents
    /// it is from and to, handed to the transcripts that follow them, and
    /// written.
    fn mail(&mut self, host: HostId, mails: Vec<Mail>) -> Vec<ModelEvent> {
        let Some(through) = mails.iter().map(|mail| mail.id).max() else {
            return Vec::new();
        };
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        let mut rows: BTreeMap<AgentId, Vec<TranscriptRow>> = BTreeMap::new();
        let mut stops_moved = Vec::new();
        for mail in &mails {
            let parties = [mail.from, mail.to]
                .into_iter()
                .filter_map(|address| match address {
                    Address::Agent(agent) => Some(agent),
                    Address::User => None,
                })
                .collect::<BTreeSet<_>>();
            for agent_id in parties {
                let named = self.agents.contains_key(&agent_id);
                let digest = match self.agents.get_mut(&agent_id) {
                    Some(agent) => &mut agent.digest,
                    None => slot.strays.entry(agent_id).or_default(),
                };
                let stop = digest.stop();
                if !digest.tell(agent_id, mail) {
                    continue;
                }
                if digest.stop() != stop {
                    stops_moved.push(agent_id);
                }
                if named {
                    slot.pending.insert(agent_id);
                }
                if self.followed.contains(&agent_id) {
                    rows.entry(agent_id)
                        .or_default()
                        .push(TranscriptRow::Mail(mail.clone()));
                }
            }
        }
        slot.mail = slot.mail.max(through);
        crate::cache::write_mail(&slot.name, slot.mail, mails);
        self.store(&stops_moved);
        let mut out = rows
            .into_iter()
            .map(|(agent_id, rows)| ModelEvent {
                host,
                msg: ModelMsg::Rows { agent_id, rows },
            })
            .collect::<Vec<_>>();
        out.extend(self.announce(host));
        out
    }

    /// What an agent's runtime is doing: a stop is put to the user, and
    /// anything else ends one.
    fn status(&mut self, host: HostId, agent_id: AgentId, state: RuntimeState) -> Vec<ModelEvent> {
        let slot = self.hosts.get(&host).expect("the host is here");
        let head = slot.mail.max(slot.mail_head);
        let Some(agent) = self.agents.get_mut(&agent_id) else {
            return Vec::new();
        };
        let moved = match state.inference {
            InferenceState::Failed { .. } => agent.digest.stopped(head, UnixMs::now()),
            _ => agent.digest.running(),
        };
        let mut out = Vec::new();
        if moved {
            self.store(&[agent_id]);
            let slot = self.hosts.get_mut(&host).expect("the host is here");
            slot.pending.insert(agent_id);
            out.extend(self.announce(host));
        }
        out.push(ModelEvent {
            host,
            msg: ModelMsg::Runtime { agent_id, state },
        });
        out
    }

    /// Rows of a followed workset: written, and handed up for the agents
    /// followed.
    fn rows(
        &mut self,
        host: HostId,
        workset: WorksetId,
        rows: Vec<(Wpos, LogEvent)>,
    ) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        let Some(reading) = slot.worksets.get_mut(&workset) else {
            // A workset no longer followed: its rows are of no transcript.
            return Vec::new();
        };
        let Some(last) = rows.last().map(|(pos, _)| *pos) else {
            return Vec::new();
        };
        reading.next = reading.next.max(last.next());
        let mut handed: BTreeMap<AgentId, Vec<TranscriptRow>> = BTreeMap::new();
        for (pos, event) in &rows {
            if let Some(agent_id) = event.agent().filter(|agent| self.followed.contains(agent)) {
                handed
                    .entry(agent_id)
                    .or_default()
                    .push(TranscriptRow::Log(*pos, event.clone()));
            }
        }
        crate::cache::write_rows(workset, rows);
        handed
            .into_iter()
            .map(|(agent_id, rows)| ModelEvent {
                host,
                msg: ModelMsg::Rows { agent_id, rows },
            })
            .collect()
    }

    /// The agents moved, once the host's mail has caught up with its head.
    fn announce(&mut self, host: HostId) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        if slot.mail < slot.mail_head || slot.pending.is_empty() {
            return Vec::new();
        }
        let agents = std::mem::take(&mut slot.pending)
            .into_iter()
            .filter_map(|agent_id| self.agents.get(&agent_id).cloned())
            .collect::<Vec<_>>();
        if agents.is_empty() {
            return Vec::new();
        }
        vec![ModelEvent {
            host,
            msg: ModelMsg::Changed { agents },
        }]
    }
}

/// The window's handle on the agents: the model thread, what it is told,
/// and the disk copy behind it.
pub struct AgentsClient {
    incoming: futures_mpsc::UnboundedSender<ToModel>,
    /// Each attached host's agents stream, for the focus the window says.
    streams: std::sync::Mutex<HashMap<HostId, AgentCommands>>,
}

impl AgentsClient {
    /// Starts the model on its own thread. A std thread, not a background
    /// task: it must not take its turn behind the frames it feeds. The
    /// receiver is what the window hears.
    pub fn spawn() -> (Self, futures_mpsc::UnboundedReceiver<ModelEvent>) {
        let (incoming, incoming_rx) = futures_mpsc::unbounded();
        let (changes_tx, changes) = futures_mpsc::unbounded();
        std::thread::Builder::new()
            .name("rho-model".to_owned())
            .spawn(move || futures::executor::block_on(run(incoming_rx, changes_tx)))
            .expect("spawn the model thread");
        (
            Self {
                incoming,
                streams: Default::default(),
            },
            changes,
        )
    }

    /// A client with no model behind it, for a test that drives the model
    /// inline on its own thread so that it can assert in the frame it fed.
    /// The caller decides, under its own `cfg(test)`.
    pub fn detached() -> (Self, futures_mpsc::UnboundedReceiver<ModelEvent>) {
        let (incoming, _incoming_rx) = futures_mpsc::unbounded();
        let (_changes_tx, changes) = futures_mpsc::unbounded();
        (
            Self {
                incoming,
                streams: Default::default(),
            },
            changes,
        )
    }

    /// A host the window attached, named before it is dialled: the name is
    /// how the disk copy knows it across restarts.
    pub fn attach_host(&self, host: HostId, name: String) {
        self.command(ModelCommand::AttachHost { host, name });
    }

    /// The agents stream for a host just attached, for the host to open on
    /// every connection. Its frames go to the model, and the model and
    /// [`Self::focus`] speak on it.
    pub fn stream(&self, host: HostId) -> std::sync::Arc<dyn HostStream> {
        let (stream, commands) = AgentStream::new(host, self.incoming.clone());
        self.streams.lock().unwrap().insert(host, commands.clone());
        self.command(ModelCommand::HostCommands { host, commands });
        std::sync::Arc::new(stream)
    }

    pub fn detach_host(&self, host: HostId) {
        self.streams.lock().unwrap().remove(&host);
        self.command(ModelCommand::DetachHost(host));
    }

    /// The agents whose live frames a host should stream: every open pane
    /// on it, replaced wholesale.
    pub fn focus(&self, host: HostId, agent_ids: Vec<AgentId>) {
        if let Some(commands) = self.streams.lock().unwrap().get(&host) {
            commands.focus(agent_ids);
        }
    }

    /// The agents whose rows the window wants, replaced wholesale.
    pub fn follow(&self, agents: BTreeSet<AgentId>) {
        self.command(ModelCommand::Follow(agents));
    }

    /// Everything the disk copy holds of an agent's transcript: its
    /// workset's rows about it and its mail, in time order. The copy may
    /// trail what was just heard by a queued write; this waits for it.
    pub fn read_transcript(&self, agent_id: AgentId, workset: WorksetId) -> Vec<TranscriptRow> {
        crate::cache::flush();
        crate::fold::interleave(
            agent_id,
            crate::cache::read_rows(workset),
            crate::cache::read_mail(agent_id),
        )
    }

    /// The user's verdict on an agent, kept with the copy.
    pub fn set_verdict(&self, agent_id: AgentId, verdict: Verdict) {
        crate::cache::write_verdict(agent_id, verdict);
    }

    fn command(&self, command: ModelCommand) {
        // A model that has gone went with the window; nobody is left to
        // tell.
        let _ = self.incoming.unbounded_send(ToModel::Command(command));
    }
}

async fn run(
    mut incoming: futures_mpsc::UnboundedReceiver<ToModel>,
    changes: futures_mpsc::UnboundedSender<ModelEvent>,
) {
    let mut model = Model::new();
    while let Some(item) = incoming.next().await {
        let out = match item {
            ToModel::Event(AgentEvent { host, frame }) => model.ingest(host, frame),
            ToModel::Command(command) => model.command(command),
        };
        for event in out {
            if changes.unbounded_send(event).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{ContentPart, SendKind};

    use super::*;
    use crate::fold::Said;
    use crate::protocol::rho2::SpecView;
    use crate::protocol::transcript::RuntimeKind;
    use crate::stream::AgentStream;

    const HOST: HostId = HostId(0);

    fn agent(id: u64) -> AgentId {
        AgentId::from_counter(id, &rho_agent_types::AgentIdDomain(0)).expect("an agent id")
    }

    fn workset(n: u8) -> WorksetId {
        WorksetId([n; 6])
    }

    fn entry(agent_id: AgentId, workset: WorksetId) -> AgentEntry {
        AgentEntry {
            agent: agent_id,
            workset,
            spec: SpecView {
                runtime: RuntimeKind::Rho,
                model: "sol".into(),
                effort: "medium".into(),
                props: [("user_facing".to_owned(), "true".to_owned())].into(),
            },
            created_at: UnixMs(1),
        }
    }

    fn hello(mail_head: u64) -> AgentFrame {
        ServerFrame::Hello {
            machine_seed: 7,
            mail_head: MailId(mail_head),
            agent_counter: 3,
        }
        .into()
    }

    fn directory(agents: &[(AgentId, WorksetId)]) -> AgentFrame {
        ServerFrame::Directory {
            worksets: agents
                .iter()
                .map(|(_, workset)| WorksetEntry {
                    workset: *workset,
                    kind: WorksetKind::Isolated,
                    created_at: UnixMs(1),
                })
                .collect(),
            agents: agents
                .iter()
                .map(|(agent_id, workset)| entry(*agent_id, *workset))
                .collect(),
        }
        .into()
    }

    fn mail(id: u64, from: Address, to: Address, kind: Option<SendKind>) -> Mail {
        Mail {
            id: MailId(id),
            at: UnixMs(id),
            from,
            to,
            re: None,
            kind,
            content: vec![ContentPart::Text {
                text: format!("mail {id}"),
            }],
        }
    }

    fn mails(mails: Vec<Mail>) -> AgentFrame {
        ServerFrame::Mail { mails }.into()
    }

    fn asks(id: u64, agent_id: AgentId) -> Mail {
        mail(
            id,
            Address::Agent(agent_id),
            Address::User,
            Some(SendKind::Ask),
        )
    }

    fn changed(events: &[ModelEvent]) -> Vec<AgentId> {
        events
            .iter()
            .flat_map(|event| match &event.msg {
                ModelMsg::Changed { agents } => {
                    agents.iter().map(MirroredAgent::agent_id).collect()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    fn model() -> Model {
        let mut model = Model::new();
        model.attach(HOST, "local".to_owned());
        model
    }

    /// The commands a model said to a host, drained.
    fn said(stream: &AgentStream) -> Vec<String> {
        let mut commands = stream.commands.try_lock().unwrap();
        let mut said = Vec::new();
        while let Ok(command) = commands.try_recv() {
            said.push(match command {
                crate::stream::Command::Frame(frame) => format!("{frame:?}"),
                crate::stream::Command::ReadLog { workset, from } => {
                    format!("ReadLog {workset} {}", from.0)
                }
            });
        }
        said
    }

    #[test]
    fn a_catch_up_says_nothing_until_the_mail_reaches_the_head() {
        let mut model = model();
        model.ingest(HOST, hello(3));
        assert!(
            changed(&model.ingest(
                HOST,
                directory(&[(agent(1), workset(1)), (agent(2), workset(2))])
            ))
            .is_empty(),
            "the directory waits for the mail it is read with"
        );
        assert!(changed(&model.ingest(HOST, mails(vec![asks(1, agent(1))]))).is_empty());
        let last = model.ingest(HOST, mails(vec![asks(3, agent(2))]));
        assert_eq!(
            changed(&last),
            vec![agent(1), agent(2)],
            "the page that reaches the head names everything the catch-up moved"
        );
        assert_eq!(model.agents[&agent(1)].digest.unread.len(), 1);
    }

    /// Mail heard before the directory names its agent is kept for it.
    #[test]
    fn mail_before_the_directory_waits_for_its_agent() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        model.ingest(HOST, mails(vec![asks(1, agent(1))]));
        let out = model.ingest(HOST, ServerFrame::Agent(entry(agent(1), workset(1))).into());
        assert_eq!(changed(&out), [agent(1)]);
        assert_eq!(model.agents[&agent(1)].digest.unread.len(), 1);
    }

    /// The directory is whole: an agent it no longer names is gone, and so
    /// is its workset's, and nothing live of it is heard after.
    #[test]
    fn the_directory_names_every_agent_there_is() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        model.ingest(
            HOST,
            directory(&[(agent(1), workset(1)), (agent(2), workset(2))]),
        );
        let out = model.ingest(HOST, directory(&[(agent(2), workset(2))]));
        assert!(
            matches!(&out[..], [ModelEvent { msg: ModelMsg::Deleted { agents }, .. }]
            if agents == &BTreeSet::from([agent(1)]))
        );
        let live = ServerFrame::Live {
            agent_id: agent(1),
            live: Live::Snapshot {
                state: RuntimeState::default(),
                response: None,
                draft: None,
            },
        };
        assert!(model.ingest(HOST, live.into()).is_empty());
        let out = model.ingest(
            HOST,
            ServerFrame::WorksetDeleted {
                workset: workset(2),
            }
            .into(),
        );
        assert!(
            matches!(&out[..], [ModelEvent { msg: ModelMsg::Deleted { agents }, .. }]
            if agents == &BTreeSet::from([agent(2)]))
        );
        assert!(model.agents.is_empty());
    }

    /// Following an agent reads its workset's log to the end, then asks the
    /// host to push it from there; rows go up for the followed agent only,
    /// with its mail.
    #[test]
    fn a_followed_agent_is_read_then_followed() {
        let mut model = model();
        let (stream, commands) = AgentStream::new(HOST, futures_mpsc::unbounded().0);
        model.command(ModelCommand::HostCommands {
            host: HOST,
            commands,
        });
        model.ingest(HOST, hello(0));
        assert_eq!(
            said(&stream),
            [
                "FollowMail { since: MailId(0) }",
                "FollowWorksets { from: [] }"
            ]
        );
        model.ingest(
            HOST,
            directory(&[(agent(1), workset(1)), (agent(2), workset(1))]),
        );
        said(&stream);
        model.command(ModelCommand::Follow(BTreeSet::from([agent(1)])));
        assert_eq!(
            said(&stream),
            ["ReadLog 010101010101 0", "FollowWorksets { from: [] }"]
        );

        let input = |agent_id, at| LogEvent::Input {
            agent: agent_id,
            mail: None,
            text: "go".into(),
            wake: rho_agent_types::InputWake::Quiet,
            at: UnixMs(at),
        };
        let out = model.ingest(
            HOST,
            AgentFrame::Log {
                workset: workset(1),
                page: crate::protocol::rho2::WorksetLogPage {
                    rows: vec![(Wpos(0), input(agent(1), 1)), (Wpos(1), input(agent(2), 2))],
                    next: None,
                },
            },
        );
        let handed = out
            .iter()
            .filter_map(|event| match &event.msg {
                ModelMsg::Rows { agent_id, rows } => Some((*agent_id, rows.len())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(handed, [(agent(1), 1)]);
        assert_eq!(
            said(&stream),
            [format!(
                "FollowWorksets {{ from: [({:?}, Wpos(2))] }}",
                workset(1)
            )]
        );

        let out = model.ingest(HOST, mails(vec![asks(1, agent(1)), asks(2, agent(2))]));
        assert!(matches!(&out[0].msg, ModelMsg::Rows { agent_id, rows }
            if *agent_id == agent(1) && matches!(&rows[..], [TranscriptRow::Mail(mail)] if mail.id == MailId(1))));

        // A new stream reads again from where the copy stands.
        model.ingest(HOST, hello(2));
        assert_eq!(
            said(&stream),
            [
                "FollowMail { since: MailId(2) }".to_owned(),
                "ReadLog 010101010101 2".to_owned(),
                "FollowWorksets { from: [] }".to_owned(),
            ]
        );
    }

    /// A stop is put to the user once, however often the host repeats it,
    /// and ends when the runtime runs again.
    #[test]
    fn a_status_that_failed_is_a_stop() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        model.ingest(HOST, directory(&[(agent(1), workset(1))]));
        let failed = || -> AgentFrame {
            ServerFrame::Status {
                agent_id: agent(1),
                state: RuntimeState {
                    inference: InferenceState::Failed {
                        error: "quota".into(),
                    },
                    ..Default::default()
                },
            }
            .into()
        };
        let out = model.ingest(HOST, failed());
        assert_eq!(changed(&out), [agent(1)]);
        assert!(matches!(
            out.last().map(|event| &event.msg),
            Some(ModelMsg::Runtime { .. })
        ));
        assert_eq!(
            model.agents[&agent(1)]
                .digest
                .strongest_unread(Default::default())
                .map(|unread| unread.said),
            Some(Said::Stopped)
        );
        assert!(
            changed(&model.ingest(HOST, failed())).is_empty(),
            "the same stop"
        );
        let running = ServerFrame::Status {
            agent_id: agent(1),
            state: RuntimeState::default(),
        };
        assert_eq!(changed(&model.ingest(HOST, running.into())), [agent(1)]);
        assert!(model.agents[&agent(1)].digest.unread.is_empty());
    }

    /// A reconnect that replays mail already held moves nothing: the
    /// user's reply read the ask once, and the result after it stays owed.
    #[test]
    fn replayed_mail_moves_nothing() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        model.ingest(HOST, directory(&[(agent(1), workset(1))]));
        let page = vec![
            asks(1, agent(1)),
            mail(2, Address::User, Address::Agent(agent(1)), None),
            mail(
                3,
                Address::Agent(agent(1)),
                Address::User,
                Some(SendKind::Result),
            ),
        ];
        model.ingest(HOST, mails(page.clone()));
        assert!(changed(&model.ingest(HOST, mails(page))).is_empty());
        let unread = &model.agents[&agent(1)].digest.unread;
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].said, Said::Result);
    }

    /// A host on another database starts the copy over.
    #[test]
    fn another_database_starts_over() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        model.ingest(HOST, directory(&[(agent(1), workset(1))]));
        let out = model.ingest(
            HOST,
            ServerFrame::Hello {
                machine_seed: 8,
                mail_head: MailId(0),
                agent_counter: 0,
            }
            .into(),
        );
        assert!(matches!(&out[0].msg, ModelMsg::Loaded { agents, .. } if agents.is_empty()));
        assert!(model.agents.is_empty());
    }
}
