//! The model: the fold, the cursors and the disk copy, on their own
//! thread, and [`AgentsClient`], the window's handle on it.
//!
//! The main thread draws. Everything between a host's agents session and
//! what is drawn happens here: mail is folded into each agent's digest,
//! the directory into what each agent is, the rows of the open
//! transcripts' agents are read and handed up, and all of it is written
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
use rho_agent_types::{Address, AgentId, Mail, MailId, WorksetId, WorksetKind, Wpos};

use crate::HostId;
use crate::fold::{AgentIdentity, Digest, MirroredAgent, TranscriptRow};
use crate::protocol::transcript::{Live, RuntimeState};
use crate::protocol::worksets::{
    AgentEntry, AgentLogPage, ClientFrame, LogEvent, LogNext, ServerFrame, WorksetEntry,
};
use crate::stream::{AgentCommands, AgentEvent, AgentFrame, AgentStream, ReadId};

/// What the main thread hears from the model.
pub enum ModelMsg {
    /// Every agent this client holds for a host, from the disk copy or
    /// after the copy started over. What the main thread had for the host
    /// is gone; this is what there is instead.
    Loaded { agents: Vec<MirroredAgent> },
    /// Agents the directory or their mail moved, as they now stand. One
    /// message per frame once caught up, and one for a whole catch-up.
    Changed { agents: Vec<MirroredAgent> },
    /// Agents gone from their host's directory, and their copy with them.
    Deleted { agents: BTreeSet<AgentId> },
    /// Rows and mail of an agent the main thread follows, for the fold
    /// behind its transcript, once its workset's log is read: what came
    /// before is [`ModelMsg::Read`].
    Rows {
        agent_id: AgentId,
        rows: Vec<TranscriptRow>,
    },
    /// A followed agent's workset log is read to its end and on disk, with
    /// its mail. The transcript is the disk copy now, in time order; told
    /// as rows, the log's past would land after mail newer than it.
    Read { agent_id: AgentId },
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

/// Where this client stands with one followed agent's rows.
#[derive(Debug)]
enum AgentRows {
    /// Its log is being read to its end by the read named. Rows its
    /// workset's follow pushes meanwhile wait for the read's end.
    Reading {
        read: ReadId,
        heard: Vec<(Wpos, LogEvent)>,
    },
    /// Read: its rows come as its workset's follow pushes them, from
    /// `next` on.
    Live { next: Wpos },
    /// Not read, as the stream is not open, or the read stopped short:
    /// the transcript is what the copy holds and what is pushed after it,
    /// until the stream opens again and reads it.
    Unread,
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
    /// The followed agents of this host, and where each stands.
    agents: BTreeMap<AgentId, AgentRows>,
    /// The worksets the host pushes rows of, each from where its follow
    /// started: every row from there on comes.
    worksets: BTreeMap<WorksetId, Wpos>,
    /// What the next read of an agent's log is numbered.
    next_read: u64,
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
            agents: BTreeMap::new(),
            worksets: BTreeMap::new(),
            next_read: 0,
        }
    }

    /// The worksets the host is to push, each from where its follow
    /// started.
    fn follow_worksets(&self) {
        let Some(commands) = self.commands.as_ref().filter(|_| self.open) else {
            return;
        };
        let from = self
            .worksets
            .iter()
            .map(|(workset, from)| (*workset, *from))
            .collect();
        commands.send(ClientFrame::FollowWorksets { from });
    }

    /// Whether `read` is the read of `agent` that this client waits for.
    fn reads(&self, agent: AgentId, read: ReadId) -> bool {
        matches!(
            self.agents.get(&agent),
            Some(AgentRows::Reading { read: reading, .. }) if *reading == read
        )
    }

    /// Reads an agent's log from where the copy stands to its end; the
    /// stream's opening reads an agent followed before it.
    fn read(&mut self, agent: AgentId) {
        let (Some(commands), true) = (self.commands.as_ref(), self.open) else {
            self.agents.insert(agent, AgentRows::Unread);
            return;
        };
        let read = ReadId(self.next_read);
        self.next_read += 1;
        commands.read_log(agent, read, crate::cache::rows_next(agent));
        self.agents.insert(
            agent,
            AgentRows::Reading {
                read,
                heard: Vec::new(),
            },
        );
    }
}

/// A followed agent's log read to its end, or as far as it could be: its
/// transcript is its disk copy now.
fn read_ended(host: HostId, agent_id: AgentId) -> Vec<ModelEvent> {
    vec![ModelEvent {
        host,
        msg: ModelMsg::Read { agent_id },
    }]
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
        for loaded in &stored.agents {
            if loaded.host != name {
                continue;
            }
            slot.kinds
                .insert(loaded.identity.entry.workset, loaded.identity.kind.clone());
            agents.push(MirroredAgent {
                host,
                identity: loaded.identity.clone(),
                digest: loaded.digest.clone(),
            });
        }
        self.hosts.insert(host, slot);
        for agent in &agents {
            self.agents.insert(agent.agent_id(), agent.clone());
        }
        if agents.is_empty() {
            return Vec::new();
        }
        vec![ModelEvent {
            host,
            msg: ModelMsg::Loaded { agents },
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
                // Rows are told by agent: the workset is only how they came.
                ServerFrame::Rows { workset: _, rows } => self.rows(host, rows),
                ServerFrame::Live { agent_id, live } => {
                    if !self.agents.contains_key(&agent_id) {
                        return Vec::new();
                    }
                    event(ModelMsg::Live { agent_id, live })
                }
                ServerFrame::Status { agent_id, state } => vec![ModelEvent {
                    host,
                    msg: ModelMsg::Runtime { agent_id, state },
                }],
                // Nothing here asks for a detail body: the transcript
                // draws a call's line and never its output.
                ServerFrame::Detail { .. } => Vec::new(),
            },
            AgentFrame::Log { agent, read, page } => self.log_page(host, agent, read, page),
            // What was read is on disk; the transcript is that much.
            AgentFrame::LogFailed { agent, read } => {
                let slot = self.hosts.get_mut(&host).expect("the host is here");
                if !slot.reads(agent, read) {
                    return Vec::new();
                }
                slot.agents.insert(agent, AgentRows::Unread);
                read_ended(host, agent)
            }
        }
    }

    /// A page of a followed agent's log, to disk. The read's end hands the
    /// agent its disk copy and leaves its rows to its workset's follow.
    fn log_page(
        &mut self,
        host: HostId,
        agent: AgentId,
        read: ReadId,
        page: AgentLogPage,
    ) -> Vec<ModelEvent> {
        let Some(workset) = self
            .agents
            .get(&agent)
            .map(|held| held.identity.entry.workset)
        else {
            return Vec::new();
        };
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        if !slot.reads(agent, read) {
            return Vec::new();
        }
        crate::cache::write_rows(agent, page.rows);
        let LogNext::Follow(end) = page.next else {
            return Vec::new();
        };
        match slot.worksets.get(&workset) {
            // The workset's follow started past the read's end: the rows
            // between are neither read nor pushed. Read on to a later end.
            Some(start) if *start > end => {
                if let Some(commands) = &slot.commands {
                    commands.read_log(agent, read, end);
                }
                return Vec::new();
            }
            Some(_) => {}
            None => {
                slot.worksets.insert(workset, end);
                slot.follow_worksets();
            }
        }
        let Some(AgentRows::Reading { heard, .. }) = slot.agents.remove(&agent) else {
            unreachable!("the agent is being read");
        };
        let heard = heard
            .into_iter()
            .filter(|(pos, _)| *pos >= end)
            .collect::<Vec<_>>();
        let next = heard.last().map_or(end, |(pos, _)| pos.next());
        crate::cache::write_rows(agent, heard);
        slot.agents.insert(agent, AgentRows::Live { next });
        read_ended(host, agent)
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
            crate::cache::reset_host(&slot.name);
            // The reads below start from what the copy holds: nothing, once
            // the reset is written.
            crate::cache::flush();
            self.agents.retain(|_, agent| agent.host != host);
            out.push(ModelEvent {
                host,
                msg: ModelMsg::Loaded { agents: Vec::new() },
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
    /// followed agents read again from where the copy stands. The new
    /// stream follows no workset until a read ends.
    fn follow(&mut self, host: HostId) {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        if let Some(commands) = &slot.commands {
            commands.send(ClientFrame::FollowMail { since: slot.mail });
        }
        slot.worksets.clear();
        let agents = slot.agents.keys().copied().collect::<Vec<_>>();
        for agent in agents {
            slot.read(agent);
        }
        slot.follow_worksets();
    }

    /// The followed agents on a host, after the followed set or the
    /// directory moved: an agent newly followed is read from where the
    /// copy stands; one no longer followed, and a workset none of the
    /// followed is in, is let go.
    fn refollow(&mut self, host: HostId) {
        let needed = self
            .followed
            .iter()
            .filter_map(|agent_id| self.agents.get(agent_id))
            .filter(|agent| agent.host == host)
            .map(|agent| (agent.agent_id(), agent.identity.entry.workset))
            .collect::<BTreeMap<_, _>>();
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        let held = slot.agents.keys().copied().collect::<BTreeSet<_>>();
        if held.iter().eq(needed.keys()) {
            return;
        }
        slot.agents.retain(|agent, _| needed.contains_key(agent));
        let worksets = needed.values().copied().collect::<BTreeSet<_>>();
        slot.worksets
            .retain(|workset, _| worksets.contains(workset));
        for agent in needed.keys().filter(|agent| !held.contains(agent)) {
            slot.read(*agent);
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
                if !digest.tell(agent_id, mail) {
                    continue;
                }
                if named {
                    slot.pending.insert(agent_id);
                }
                // An agent whose log is being read hears its mail with the
                // read's end.
                let read_pending =
                    matches!(slot.agents.get(&agent_id), Some(AgentRows::Reading { .. }));
                if self.followed.contains(&agent_id) && !read_pending {
                    rows.entry(agent_id)
                        .or_default()
                        .push(TranscriptRow::Mail(mail.clone()));
                }
            }
        }
        slot.mail = slot.mail.max(through);
        crate::cache::write_mail(&slot.name, slot.mail, mails);
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

    /// Rows a followed workset's follow pushed: those of the followed
    /// agents written and handed up, or kept for the end of their read.
    fn rows(&mut self, host: HostId, rows: Vec<(Wpos, LogEvent)>) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        let mut handed: BTreeMap<AgentId, Vec<(Wpos, LogEvent)>> = BTreeMap::new();
        for (pos, event) in rows {
            let Some(agent) = event.agent() else {
                continue;
            };
            match slot.agents.get_mut(&agent) {
                Some(AgentRows::Reading { heard, .. }) => heard.push((pos, event)),
                // A follow that started before the read's end pushes again
                // what the read told.
                Some(AgentRows::Live { next }) if pos >= *next => {
                    *next = pos.next();
                    handed.entry(agent).or_default().push((pos, event));
                }
                Some(AgentRows::Unread) => handed.entry(agent).or_default().push((pos, event)),
                _ => {}
            }
        }
        handed
            .into_iter()
            .map(|(agent_id, rows)| {
                crate::cache::write_rows(agent_id, rows.clone());
                ModelEvent {
                    host,
                    msg: ModelMsg::Rows {
                        agent_id,
                        rows: rows
                            .into_iter()
                            .map(|(pos, event)| TranscriptRow::Log(pos, event))
                            .collect(),
                    },
                }
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

    /// The mail the disk copy holds of an agent's, oldest first. The copy
    /// may trail what was just heard by a queued write; this waits for it.
    pub fn read_mail(&self, agent_id: AgentId) -> Vec<rho_agent_types::Mail> {
        crate::cache::flush();
        crate::cache::read_mail(agent_id)
    }

    /// Everything the disk copy holds of an agent's transcript: its rows
    /// and its mail, in time order. The copy may trail what was just heard
    /// by a queued write; this waits for it.
    pub fn read_transcript(&self, agent_id: AgentId) -> Vec<TranscriptRow> {
        crate::cache::flush();
        crate::fold::interleave(
            agent_id,
            crate::cache::read_rows(agent_id),
            crate::cache::read_mail(agent_id),
        )
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
    use rho_agent_types::{ContentPart, SendKind, UnixMs};

    use super::*;
    use crate::fold::Said;
    use crate::protocol::transcript::RuntimeKind;
    use crate::protocol::worksets::SpecView;
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
            thread: rho_agent_types::ThreadId(MailId(id)),
            kind,
            content: vec![ContentPart::Text {
                text: format!("mail {id}"),
            }],
        }
    }

    fn mails(mails: Vec<Mail>) -> AgentFrame {
        ServerFrame::Mail { mails }.into()
    }

    /// What the agent's threads want of the user, by thread.
    fn wants(model: &Model, agent_id: AgentId) -> Vec<Said> {
        model.agents[&agent_id]
            .digest
            .threads
            .values()
            .filter_map(|thread| thread.wants.map(|wanting| wanting.said))
            .collect()
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
                crate::stream::Command::ReadLog { agent, read, from } => {
                    format!("ReadLog {} {} #{}", agent.encoded(), from.0, read.0)
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
        assert_eq!(wants(&model, agent(1)), [Said::Ask]);
    }

    /// Mail heard before the directory names its agent is kept for it.
    #[test]
    fn mail_before_the_directory_waits_for_its_agent() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        model.ingest(HOST, mails(vec![asks(1, agent(1))]));
        let out = model.ingest(HOST, ServerFrame::Agent(entry(agent(1), workset(1))).into());
        assert_eq!(changed(&out), [agent(1)]);
        assert_eq!(wants(&model, agent(1)), [Said::Ask]);
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

    fn input(agent_id: AgentId, at: u64) -> LogEvent {
        LogEvent::Input {
            agent: agent_id,
            mail: None,
            text: "go".into(),
            wake: rho_agent_types::InputWake::Quiet,
            at: UnixMs(at),
        }
    }

    /// What the model handed up: each agent's rows told, by position, and
    /// `Read` for a read's end.
    fn handed(out: &[ModelEvent]) -> Vec<(AgentId, Option<Vec<u64>>)> {
        out.iter()
            .filter_map(|event| match &event.msg {
                ModelMsg::Rows { agent_id, rows } => Some((
                    *agent_id,
                    Some(
                        rows.iter()
                            .filter_map(|row| match row {
                                TranscriptRow::Log(pos, _) => Some(pos.0),
                                TranscriptRow::Mail(_) => None,
                            })
                            .collect(),
                    ),
                )),
                ModelMsg::Read { agent_id } => Some((*agent_id, None)),
                _ => None,
            })
            .collect()
    }

    fn page(
        agent_id: AgentId,
        read: u64,
        rows: Vec<(Wpos, LogEvent)>,
        next: LogNext,
    ) -> AgentFrame {
        AgentFrame::Log {
            agent: agent_id,
            read: ReadId(read),
            page: AgentLogPage { rows, next },
        }
    }

    fn pushed(rows: Vec<(Wpos, LogEvent)>) -> AgentFrame {
        ServerFrame::Rows {
            workset: workset(1),
            rows,
        }
        .into()
    }

    /// Following an agent reads its own log to the end, then asks the host
    /// to push its workset from where the read ended; the read's end hands
    /// the transcript, and rows go up after it for the followed agent only,
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
            [
                format!("ReadLog {} 0 #0", agent(1).encoded()),
                "FollowWorksets { from: [] }".to_owned()
            ]
        );

        // The log's past and the mail heard meanwhile go to disk, not up:
        // told as rows, the past would land after the newer mail.
        let out = model.ingest(
            HOST,
            page(
                agent(1),
                0,
                vec![(Wpos(0), input(agent(1), 1))],
                LogNext::Page(Wpos(1)),
            ),
        );
        assert_eq!(handed(&out), []);
        let out = model.ingest(HOST, mails(vec![asks(1, agent(1))]));
        assert_eq!(
            handed(&out),
            [],
            "mail of an agent being read waits for the read"
        );
        assert!(said(&stream).is_empty(), "the follow waits for the read");
        let out = model.ingest(
            HOST,
            page(
                agent(1),
                0,
                vec![(Wpos(4), input(agent(1), 2))],
                LogNext::Follow(Wpos(7)),
            ),
        );
        assert_eq!(
            handed(&out),
            [(agent(1), None)],
            "the read's end hands the followed agent its disk copy, and nobody else"
        );
        assert_eq!(
            said(&stream),
            [format!(
                "FollowWorksets {{ from: [({:?}, Wpos(7))] }}",
                workset(1)
            )],
            "the workset is followed from where the read ended, not from the agent's last row"
        );

        // Followed now: what comes goes up as it comes, for its agent only,
        // and a row the read already told is not told twice.
        let out = model.ingest(
            HOST,
            pushed(vec![
                (Wpos(4), input(agent(1), 2)),
                (Wpos(7), input(agent(2), 3)),
                (Wpos(8), input(agent(1), 4)),
            ]),
        );
        assert_eq!(handed(&out), [(agent(1), Some(vec![8]))]);
        let out = model.ingest(HOST, mails(vec![asks(2, agent(2)), asks(3, agent(1))]));
        assert!(matches!(&out[0].msg, ModelMsg::Rows { agent_id, rows }
            if *agent_id == agent(1) && matches!(&rows[..], [TranscriptRow::Mail(mail)] if mail.id == MailId(3))));

        // A new stream reads again, and follows nothing until the read ends.
        model.ingest(HOST, hello(3));
        assert_eq!(
            said(&stream),
            [
                "FollowMail { since: MailId(3) }".to_owned(),
                format!("ReadLog {} 0 #1", agent(1).encoded()),
                "FollowWorksets { from: [] }".to_owned(),
            ]
        );
        let out = model.ingest(
            HOST,
            page(agent(1), 0, Vec::new(), LogNext::Follow(Wpos(9))),
        );
        assert_eq!(
            handed(&out),
            [],
            "a page of the read given up on is not the read's end"
        );
    }

    /// Two reads in one workset end in either order. The follow starts
    /// from the first end; a read that ended before it reads on, so the
    /// rows between are neither lost nor told twice, and the rows pushed
    /// while it read wait for its end.
    #[test]
    fn a_read_that_ends_before_the_follow_reads_on() {
        let mut model = model();
        let (stream, commands) = AgentStream::new(HOST, futures_mpsc::unbounded().0);
        model.command(ModelCommand::HostCommands {
            host: HOST,
            commands,
        });
        model.ingest(HOST, hello(0));
        model.ingest(
            HOST,
            directory(&[(agent(1), workset(1)), (agent(2), workset(1))]),
        );
        model.command(ModelCommand::Follow(BTreeSet::from([agent(1), agent(2)])));
        said(&stream);

        let out = model.ingest(
            HOST,
            page(agent(1), 0, Vec::new(), LogNext::Follow(Wpos(10))),
        );
        assert_eq!(handed(&out), [(agent(1), None)]);
        assert_eq!(
            said(&stream),
            [format!(
                "FollowWorksets {{ from: [({:?}, Wpos(10))] }}",
                workset(1)
            )]
        );
        // Pushed while agent 2 is still read: its row waits.
        let out = model.ingest(
            HOST,
            pushed(vec![
                (Wpos(10), input(agent(2), 1)),
                (Wpos(11), input(agent(1), 2)),
            ]),
        );
        assert_eq!(handed(&out), [(agent(1), Some(vec![11]))]);

        // Agent 2's read ended at 8, before the follow's start: 8 and 9
        // were neither read nor pushed.
        let out = model.ingest(
            HOST,
            page(agent(2), 1, Vec::new(), LogNext::Follow(Wpos(8))),
        );
        assert_eq!(handed(&out), [], "not read to its end yet");
        assert_eq!(
            said(&stream),
            [format!("ReadLog {} 8 #1", agent(2).encoded())]
        );
        let out = model.ingest(
            HOST,
            page(
                agent(2),
                1,
                vec![
                    (Wpos(9), input(agent(2), 0)),
                    (Wpos(10), input(agent(2), 1)),
                ],
                LogNext::Follow(Wpos(12)),
            ),
        );
        assert_eq!(handed(&out), [(agent(2), None)]);
        assert!(said(&stream).is_empty(), "the workset is followed already");
        let out = model.ingest(
            HOST,
            pushed(vec![
                (Wpos(12), input(agent(2), 3)),
                (Wpos(13), input(agent(1), 4)),
            ]),
        );
        assert_eq!(
            handed(&out),
            [(agent(1), Some(vec![13])), (agent(2), Some(vec![12]))]
        );
    }

    /// A reconnect that replays mail already held moves nothing: the
    /// user's reply read the ask once, and the result after it stays owed.
    #[test]
    fn replayed_mail_moves_nothing() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        model.ingest(HOST, directory(&[(agent(1), workset(1))]));
        let in_first = |mut mail: Mail| {
            mail.thread = rho_agent_types::ThreadId(MailId(1));
            mail
        };
        let page = vec![
            asks(1, agent(1)),
            in_first(mail(2, Address::User, Address::Agent(agent(1)), None)),
            in_first(mail(
                3,
                Address::Agent(agent(1)),
                Address::User,
                Some(SendKind::Result),
            )),
        ];
        model.ingest(HOST, mails(page.clone()));
        assert!(changed(&model.ingest(HOST, mails(page))).is_empty());
        assert_eq!(wants(&model, agent(1)), [Said::Result]);
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
