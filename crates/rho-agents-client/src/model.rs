//! The model: the fold, the cursors and the disk copy, on their own
//! thread, and [`AgentsClient`], the window's handle on it.
//!
//! The main thread draws. Everything between a host's agents session and
//! what is drawn happens here: the chat log is folded into each host's
//! topics and each agent's digest,
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
use rho_agent_types::{AgentId, ChatRecord, ChatSeq, Message, WorksetId, WorksetKind, Wpos};

use crate::HostId;
use crate::chat::{HostChat, Told};
use crate::fold::{AgentIdentity, Digest, MirroredAgent, TranscriptRow};
use crate::protocol::transcript::{Live, RuntimeState};
use crate::protocol::worksets::{AgentEntry, ClientFrame, LogEvent, ServerFrame, WorksetEntry};
use crate::stream::{AgentCommands, AgentEvent, AgentFrame, AgentStream};

/// What the main thread hears from the model.
pub enum ModelMsg {
    /// Every agent and the whole chat this client holds for a host, from
    /// the disk copy or after the copy started over, and the database the
    /// host's ids count in. What the main thread had for the host is gone;
    /// this is what there is instead.
    Loaded {
        machine_seed: u64,
        agents: Vec<MirroredAgent>,
        chat: HostChat,
    },
    /// Agents the directory or their messages moved, as they now stand.
    /// One message per frame once caught up, and one for a whole catch-up.
    Changed { agents: Vec<MirroredAgent> },
    /// Streams and topics that moved, as they now stand; caught up as
    /// `Changed` is.
    Chat { moved: HostChat },
    /// Agents gone from their host's directory, and their copy with them.
    Deleted { agents: BTreeSet<AgentId> },
    /// Rows and messages of an agent the main thread follows, for the fold
    /// behind its transcript, once its workset's log is read: what came
    /// before is [`ModelMsg::Read`].
    Rows {
        agent_id: AgentId,
        rows: Vec<TranscriptRow>,
    },
    /// A followed agent's workset log is read to its end and on disk, with
    /// its messages. The transcript is the disk copy now, in time order;
    /// told as rows, the log's past would land after messages newer than
    /// it.
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
    /// The newest chat event held.
    chat_held: ChatSeq,
    /// The newest chat event on the host when the stream opened. Until
    /// `chat_held` reaches it the main thread hears nothing of the digests
    /// or the topics: a catch-up is not news, it is arrears.
    chat_head: ChatSeq,
    /// The host's chat, folded.
    chat: HostChat,
    /// Agents moved since the main thread last heard. Empty except during
    /// a catch-up.
    pending: BTreeSet<AgentId>,
    /// Streams and topics moved since the main thread last heard.
    pending_chat: HostChat,
    /// The kind of each workset of the directory.
    kinds: BTreeMap<WorksetId, WorksetKind>,
    /// Messages of agents the directory has not named (yet).
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
            chat_held: ChatSeq::ZERO,
            chat_head: ChatSeq::ZERO,
            chat: HostChat::default(),
            pending: BTreeSet::new(),
            pending_chat: HostChat::default(),
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
                // The disk copy keeps the host's chat: a host detached is
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
    /// it: the cursor `FollowChat` will send, its chat, and the agents
    /// already folded.
    pub fn attach(&mut self, host: HostId, name: String) -> Vec<ModelEvent> {
        let mut slot = HostModel::new(name.clone());
        let stored = self.stored.get_or_insert_with(|| {
            crate::cache::flush();
            crate::cache::load()
        });
        let Some(cursor) = stored.hosts.iter().find(|cursor| cursor.name == name) else {
            self.hosts.insert(host, slot);
            return Vec::new();
        };
        slot.machine_seed = cursor.machine_seed;
        slot.chat_held = cursor.chat;
        slot.chat_head = cursor.chat;
        let mut agents = BTreeMap::new();
        for loaded in &stored.agents {
            if loaded.host != name {
                continue;
            }
            slot.kinds
                .insert(loaded.identity.entry.workset, loaded.identity.kind.clone());
            agents.insert(
                loaded.identity.agent_id(),
                MirroredAgent::new(host, loaded.identity.clone()),
            );
        }
        // The chat is read back and folded again, a topic at a time, and
        // the digests with it.
        for record in crate::cache::read_chat(&name) {
            if let Told::Topic {
                message: Some((message, parties)),
                ..
            } = slot.chat.tell(&record)
            {
                for agent_id in parties {
                    let digest = match agents.get_mut(&agent_id) {
                        Some(agent) => &mut agent.digest,
                        None => slot.strays.entry(agent_id).or_default(),
                    };
                    digest.tell(&message);
                }
            }
        }
        let chat = slot.chat.clone();
        let machine_seed = slot.machine_seed;
        self.hosts.insert(host, slot);
        self.agents.extend(agents.clone());
        vec![ModelEvent {
            host,
            msg: ModelMsg::Loaded {
                machine_seed,
                agents: agents.into_values().collect(),
                chat,
            },
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
                    chat_head,
                    agent_counter,
                } => {
                    let mut out = self.hello(host, machine_seed, chat_head);
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
                ServerFrame::Chat { records } => self.chat(host, records),
                ServerFrame::Rows { workset, rows } => self.rows(host, workset, rows),
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
            AgentFrame::Log { workset, page } => {
                let done = page.next.is_none();
                let mut out = self.rows(host, workset, page.rows);
                if done {
                    out.extend(self.read_ended(host, workset));
                    let slot = self.hosts.get_mut(&host).expect("the host is here");
                    slot.follow_worksets();
                }
                out
            }
            // What was read is on disk; the transcript is that much.
            AgentFrame::LogFailed { workset } => self.read_ended(host, workset),
        }
    }

    /// A workset's log read to its end, or as far as it could be: each
    /// followed agent in it is its disk copy now.
    fn read_ended(&mut self, host: HostId, workset: WorksetId) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        let Some(reading) = slot.worksets.get_mut(&workset) else {
            return Vec::new();
        };
        reading.reading = false;
        self.followed
            .iter()
            .filter(|agent_id| {
                self.agents.get(agent_id).is_some_and(|agent| {
                    agent.host == host && agent.identity.entry.workset == workset
                })
            })
            .map(|agent_id| ModelEvent {
                host,
                msg: ModelMsg::Read {
                    agent_id: *agent_id,
                },
            })
            .collect()
    }

    /// An agents stream that has opened: how far the host's chat runs, and
    /// whether it is the database this copy counts in. Asks for the rest.
    fn hello(&mut self, host: HostId, machine_seed: u64, chat_head: ChatSeq) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        slot.chat_head = chat_head;
        slot.open = true;
        // A host whose database is not the one this client copied, or
        // whose chat log is shorter than the copy: the copy starts over.
        let started_over = slot.machine_seed != machine_seed || slot.chat_held > chat_head;
        let mut out = Vec::new();
        if started_over {
            slot.machine_seed = machine_seed;
            slot.chat_held = ChatSeq::ZERO;
            slot.chat = HostChat::default();
            slot.pending.clear();
            slot.pending_chat = HostChat::default();
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
                    machine_seed,
                    agents: Vec::new(),
                    chat: HostChat::default(),
                },
            });
        }
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        crate::cache::write_host(&slot.name, machine_seed);
        tracing::debug!(
            host = slot.name,
            since = slot.chat_held.0,
            head = chat_head.0,
            started_over,
            "following the host's chat"
        );
        if slot.commands.is_some() {
            self.follow(host);
        } else {
            slot.hello_pending = true;
        }
        out
    }

    /// Everything a new stream is told: the chat since the cursor, and the
    /// followed worksets read again from where they stand.
    fn follow(&mut self, host: HostId) {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        if let Some(commands) = &slot.commands {
            commands.send(ClientFrame::FollowChat {
                since: slot.chat_held,
            });
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
                        },
                    ))
                })
                .collect(),
        );
    }

    /// A run of the host's chat log: folded into its topics and into the
    /// digests of the agents each message is from or reaches, handed to the
    /// transcripts that follow them, and written.
    fn chat(&mut self, host: HostId, records: Vec<ChatRecord>) -> Vec<ModelEvent> {
        let Some(through) = records.iter().map(|record| record.seq).max() else {
            return Vec::new();
        };
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        let mut rows: BTreeMap<AgentId, Vec<TranscriptRow>> = BTreeMap::new();
        let mut delivered: Vec<(AgentId, Message)> = Vec::new();
        for record in &records {
            match slot.chat.tell(record) {
                Told::Nothing => {}
                Told::Stream(stream) => {
                    slot.pending_chat
                        .streams
                        .insert(stream, slot.chat.streams[&stream].clone());
                }
                Told::Topic { topic, message } => {
                    slot.pending_chat
                        .topics
                        .insert(topic, slot.chat.topics[&topic].clone());
                    let Some((message, parties)) = message else {
                        continue;
                    };
                    for agent_id in parties {
                        delivered.push((agent_id, message.clone()));
                    }
                }
            }
        }
        for (agent_id, message) in &delivered {
            let named = self.agents.contains_key(agent_id);
            match self.agents.get_mut(agent_id) {
                Some(agent) => agent.digest.tell(message),
                None => slot.strays.entry(*agent_id).or_default().tell(message),
            }
            if named {
                slot.pending.insert(*agent_id);
            }
            // An agent whose log is being read hears its messages with the
            // read's end.
            let read_pending = self
                .agents
                .get(agent_id)
                .and_then(|agent| slot.worksets.get(&agent.identity.entry.workset))
                .is_some_and(|reading| reading.reading);
            if self.followed.contains(agent_id) && !read_pending {
                rows.entry(*agent_id)
                    .or_default()
                    .push(TranscriptRow::Message(message.clone()));
            }
        }
        slot.chat_held = slot.chat_held.max(through);
        crate::cache::write_chat(
            &slot.name,
            slot.chat_held,
            records,
            delivered
                .iter()
                .map(|(agent_id, message)| (*agent_id, message.id, message.topic))
                .collect(),
        );
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
        let held = reading.reading;
        crate::cache::write_rows(workset, rows.clone());
        // The log's past goes to disk only: the read's end hands it whole.
        if held {
            return Vec::new();
        }
        let mut handed: BTreeMap<AgentId, Vec<TranscriptRow>> = BTreeMap::new();
        for (pos, event) in &rows {
            if let Some(agent_id) = event.agent().filter(|agent| self.followed.contains(agent)) {
                handed
                    .entry(agent_id)
                    .or_default()
                    .push(TranscriptRow::Log(*pos, event.clone()));
            }
        }
        handed
            .into_iter()
            .map(|(agent_id, rows)| ModelEvent {
                host,
                msg: ModelMsg::Rows { agent_id, rows },
            })
            .collect()
    }

    /// The agents and topics moved, once the host's chat has caught up
    /// with its head.
    fn announce(&mut self, host: HostId) -> Vec<ModelEvent> {
        let slot = self.hosts.get_mut(&host).expect("the host is here");
        if slot.chat_held < slot.chat_head {
            return Vec::new();
        }
        let mut out = Vec::new();
        let moved = std::mem::take(&mut slot.pending_chat);
        if moved != HostChat::default() {
            out.push(ModelEvent {
                host,
                msg: ModelMsg::Chat { moved },
            });
        }
        let agents = std::mem::take(&mut slot.pending)
            .into_iter()
            .filter_map(|agent_id| self.agents.get(&agent_id).cloned())
            .collect::<Vec<_>>();
        if !agents.is_empty() {
            out.push(ModelEvent {
                host,
                msg: ModelMsg::Changed { agents },
            });
        }
        out
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

    /// The messages the disk copy holds of a topic of the host named,
    /// oldest first. The copy may trail what was just heard by a queued
    /// write; this waits for it.
    pub fn read_topic(&self, host: &str, topic: rho_agent_types::TopicId) -> Vec<Message> {
        crate::cache::flush();
        crate::cache::read_topic(host, topic)
    }

    /// Everything the disk copy holds of an agent's transcript: its
    /// workset's rows about it and the messages it posted or received, in
    /// time order. The copy may trail what was just heard by a queued
    /// write; this waits for it.
    pub fn read_transcript(&self, agent_id: AgentId, workset: WorksetId) -> Vec<TranscriptRow> {
        crate::cache::flush();
        crate::fold::interleave(
            agent_id,
            crate::cache::read_rows(workset),
            crate::cache::read_agent_messages(agent_id),
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
    use rho_agent_types::{
        Address, ChatEvent, ContentPart, Mention, MentionKind, StreamId, TopicId, UnixMs,
    };

    use super::*;
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
                props: Default::default(),
            },
            created_at: UnixMs(1),
        }
    }

    fn hello(chat_head: u64) -> AgentFrame {
        ServerFrame::Hello {
            machine_seed: 7,
            chat_head: ChatSeq(chat_head),
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

    fn record(seq: u64, event: ChatEvent) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event,
        }
    }

    /// The stream at 1 and a topic at 2 with these agents.
    fn topic_with(agents: &[AgentId]) -> Vec<ChatRecord> {
        vec![
            record(1, ChatEvent::StreamCreated { name: "rho".into() }),
            record(
                2,
                ChatEvent::TopicStarted {
                    stream: StreamId(ChatSeq(1)),
                    name: "the topic".into(),
                    by: Address::User,
                    agents: agents.to_vec(),
                },
            ),
        ]
    }

    const TOPIC: TopicId = TopicId(ChatSeq(2));

    fn posted(seq: u64, from: Address, mentions: Vec<Mention>) -> ChatRecord {
        record(
            seq,
            ChatEvent::Posted {
                topic: TOPIC,
                from,
                mentions,
                content: vec![ContentPart::Text {
                    text: format!("message {seq}"),
                }],
            },
        )
    }

    fn asks(seq: u64, agent_id: AgentId) -> ChatRecord {
        posted(
            seq,
            Address::Agent(agent_id),
            vec![Mention {
                to: Address::User,
                kind: MentionKind::Ask,
            }],
        )
    }

    fn chat(records: Vec<ChatRecord>) -> AgentFrame {
        ServerFrame::Chat { records }.into()
    }

    /// The kinds of the topic's mentions of the user, oldest first.
    fn mentions(model: &Model) -> Vec<MentionKind> {
        model.hosts[&HOST].chat.topics[&TOPIC]
            .mentions
            .iter()
            .map(|mention| mention.kind)
            .collect()
    }

    fn topics_moved(events: &[ModelEvent]) -> Vec<TopicId> {
        events
            .iter()
            .flat_map(|event| match &event.msg {
                ModelMsg::Chat { moved } => moved.topics.keys().copied().collect(),
                _ => Vec::new(),
            })
            .collect()
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
    fn a_catch_up_says_nothing_until_the_chat_reaches_the_head() {
        let mut model = model();
        model.ingest(HOST, hello(4));
        assert!(
            changed(&model.ingest(
                HOST,
                directory(&[(agent(1), workset(1)), (agent(2), workset(2))])
            ))
            .is_empty(),
            "the directory waits for the chat it is read with"
        );
        let mut page = topic_with(&[agent(1)]);
        page.push(asks(3, agent(1)));
        let out = model.ingest(HOST, chat(page));
        assert!(changed(&out).is_empty() && topics_moved(&out).is_empty());
        let last = model.ingest(HOST, chat(vec![asks(4, agent(2))]));
        assert_eq!(
            changed(&last),
            vec![agent(1), agent(2)],
            "the page that reaches the head names every agent the catch-up moved"
        );
        assert_eq!(topics_moved(&last), [TOPIC], "and every topic");
        assert_eq!(mentions(&model), [MentionKind::Ask, MentionKind::Ask]);
        assert!(
            model.hosts[&HOST].chat.topics[&TOPIC]
                .agents
                .contains_key(&agent(2)),
            "posting joins the topic"
        );
    }

    /// Messages heard before the directory names their agent are kept for
    /// it.
    #[test]
    fn messages_before_the_directory_wait_for_their_agent() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        let mut page = topic_with(&[agent(1)]);
        page.push(posted(3, Address::User, Vec::new()));
        model.ingest(HOST, chat(page));
        let out = model.ingest(HOST, ServerFrame::Agent(entry(agent(1), workset(1))).into());
        assert_eq!(changed(&out), [agent(1)]);
        assert_eq!(
            model.agents[&agent(1)].digest.last_user_message_text,
            "message 3"
        );
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
    /// host to push it from there; the read's end hands the transcript, and
    /// rows go up after it for the followed agent only, with its messages.
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
                "FollowChat { since: ChatSeq(0) }",
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
            message: None,
            text: "go".into(),
            wake: rho_agent_types::InputWake::Quiet,
            at: UnixMs(at),
        };
        let handed = |out: &[ModelEvent]| {
            out.iter()
                .filter_map(|event| match &event.msg {
                    ModelMsg::Rows { agent_id, rows } => Some((*agent_id, rows.len())),
                    ModelMsg::Read { agent_id } => Some((*agent_id, usize::MAX)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let page = |rows, next| AgentFrame::Log {
            workset: workset(1),
            page: crate::protocol::worksets::WorksetLogPage { rows, next },
        };
        // The log's past and the messages heard meanwhile go to disk, not
        // up: told as rows, the past would land after the newer messages.
        let out = model.ingest(
            HOST,
            page(vec![(Wpos(0), input(agent(1), 1))], Some(Wpos(1))),
        );
        assert_eq!(handed(&out), []);
        let mut records = topic_with(&[agent(1)]);
        records.push(asks(3, agent(1)));
        let out = model.ingest(HOST, chat(records));
        assert_eq!(
            handed(&out),
            [],
            "a message of an agent being read waits for the read"
        );
        assert!(said(&stream).is_empty(), "the follow waits for the read");
        let out = model.ingest(HOST, page(vec![(Wpos(1), input(agent(2), 2))], None));
        assert_eq!(
            handed(&out),
            [(agent(1), usize::MAX)],
            "the read's end hands the followed agent its disk copy, and nobody else"
        );
        assert_eq!(
            said(&stream),
            [format!(
                "FollowWorksets {{ from: [({:?}, Wpos(2))] }}",
                workset(1)
            )]
        );

        // Followed now: what comes goes up as it comes, for its agent only.
        let out = model.ingest(
            HOST,
            ServerFrame::Rows {
                workset: workset(1),
                rows: vec![(Wpos(2), input(agent(2), 3)), (Wpos(3), input(agent(1), 4))],
            }
            .into(),
        );
        assert_eq!(handed(&out), [(agent(1), 1)]);
        // Agent 2 is not in the topic: its post reaches agent 1, and
        // agent 1's own post is its own.
        let out = model.ingest(HOST, chat(vec![asks(4, agent(2)), asks(5, agent(1))]));
        let ids = |rows: &[TranscriptRow]| {
            rows.iter()
                .map(|row| match row {
                    TranscriptRow::Message(message) => message.id.0.0,
                    TranscriptRow::Log(..) => 0,
                })
                .collect::<Vec<_>>()
        };
        assert!(matches!(&out[0].msg, ModelMsg::Rows { agent_id, rows }
            if *agent_id == agent(1) && ids(rows) == [4, 5]));

        // A new stream reads again from where the copy stands.
        model.ingest(HOST, hello(5));
        assert_eq!(
            said(&stream),
            [
                "FollowChat { since: ChatSeq(5) }".to_owned(),
                "ReadLog 010101010101 4".to_owned(),
                "FollowWorksets { from: [] }".to_owned(),
            ]
        );
    }

    /// A reconnect that replays events already held moves nothing.
    #[test]
    fn a_replayed_page_moves_nothing() {
        let mut model = model();
        model.ingest(HOST, hello(0));
        model.ingest(HOST, directory(&[(agent(1), workset(1))]));
        let mut page = topic_with(&[agent(1)]);
        page.extend([
            asks(3, agent(1)),
            posted(4, Address::User, Vec::new()),
            posted(
                5,
                Address::Agent(agent(1)),
                vec![Mention {
                    to: Address::User,
                    kind: MentionKind::Result,
                }],
            ),
        ]);
        model.ingest(HOST, chat(page.clone()));
        let again = model.ingest(HOST, chat(page));
        assert!(changed(&again).is_empty() && topics_moved(&again).is_empty());
        assert_eq!(mentions(&model), [MentionKind::Ask, MentionKind::Result]);
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
                chat_head: ChatSeq(0),
                agent_counter: 0,
            }
            .into(),
        );
        assert!(matches!(&out[0].msg, ModelMsg::Loaded { agents, .. } if agents.is_empty()));
        assert!(model.agents.is_empty());
    }
}
