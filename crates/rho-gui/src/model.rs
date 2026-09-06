//! The model: the fold, the journal cursor and the disk mirror, on their
//! own thread.
//!
//! The main thread draws. Everything between a daemon's frames and what is
//! drawn happens here: rows are folded into agents, written to
//! `agent-mirror.redb`, and announced to the main thread as changes. A
//! reconnect's catch-up is thousands of pages, and the main thread hears
//! one message for the whole of it.
//!
//! The connection itself stays where it is, on the shared tokio runtime:
//! the socket was never the cost, and the workspace-file, terminal, shell
//! and realtime channels hang off it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use rho_registry::Verdict;
use rho_ui_proto::mirror::{AgentPos, LogEntry, MirrorEvent, Seq};
use rho_ui_proto::{AgentId, ClientMessage};

use crate::connection::{Commands, ConnEvent, HostEvent};
use crate::registry::HostId;

/// What the main thread hears from the model.
pub(crate) enum ModelMsg {
    /// Every agent this client holds for a host, from the disk copy or
    /// after the copy started over. What the main thread had for the host
    /// is gone; this is what there is instead.
    Loaded {
        agents: Vec<rho_registry::MirroredAgent>,
        verdicts: Vec<(AgentId, Verdict)>,
    },
    /// The agents a run of the log moved, as they now stand. One message
    /// per page once caught up, and one for a whole catch-up.
    Changed {
        agents: Vec<rho_registry::MirroredAgent>,
    },
    /// Rows of an agent the main thread follows, for the fold behind its
    /// transcript. Nothing else carries rows.
    Rows {
        agent_id: AgentId,
        rows: Vec<(AgentPos, MirrorEvent)>,
    },
    /// A frame the model has no part in: desk deltas, auth, errors, and
    /// the live tail of an agent the main thread follows. Handed on
    /// unchanged.
    Event(ConnEvent),
}

pub(crate) struct ModelEvent {
    pub host: HostId,
    pub msg: ModelMsg,
}

/// What the main thread asks of the model.
pub(crate) enum ModelCommand {
    /// A daemon the workspace attached, named before it is dialled: the
    /// name is how the disk copy knows it across restarts.
    AttachHost {
        host: HostId,
        name: String,
    },
    /// The way to send that daemon a command, once it has been dialled.
    HostCommands {
        host: HostId,
        commands: Commands,
    },
    DetachHost(HostId),
    /// The agents whose rows the main thread wants, replaced wholesale.
    Follow(BTreeSet<AgentId>),
}

/// What reaches the model, in the order the main thread and the
/// connections produced it. One channel, so a host is always known
/// before its frames arrive.
pub(crate) enum ToModel {
    Event(HostEvent),
    Command(ModelCommand),
}

/// Where this client stands in one host's journal.
struct HostModel {
    name: String,
    commands: Option<Commands>,
    /// A `Ready` answered before this client could speak back. The follow
    /// goes out the moment it can.
    follow_pending: bool,
    machine_seed: u64,
    seq: Seq,
    /// The journal head `Ready` named. Until `seq` reaches it the main
    /// thread hears nothing: a catch-up is not news, it is arrears.
    head: Seq,
    /// Agents moved since the main thread last heard, and the rows of the
    /// followed ones. Empty except during a catch-up.
    pending: BTreeSet<AgentId>,
    pending_rows: BTreeMap<AgentId, Vec<(AgentPos, MirrorEvent)>>,
}

impl HostModel {
    fn new(name: String) -> Self {
        Self {
            name,
            commands: None,
            follow_pending: false,
            machine_seed: 0,
            seq: Seq(0),
            head: Seq(0),
            pending: BTreeSet::new(),
            pending_rows: BTreeMap::new(),
        }
    }
}

/// The fold of every agent, the cursors, and who is followed.
pub(crate) struct Model {
    hosts: HashMap<HostId, HostModel>,
    agents: BTreeMap<AgentId, rho_registry::MirroredAgent>,
    followed: BTreeSet<AgentId>,
    /// The disk copy, read once: hosts are attached one at a time and each
    /// takes the agents filed under its name.
    stored: Option<crate::mirror::Loaded>,
}

impl Model {
    pub(crate) fn new() -> Self {
        Self {
            hosts: HashMap::new(),
            agents: BTreeMap::new(),
            followed: BTreeSet::new(),
            stored: None,
        }
    }

    pub(crate) fn command(&mut self, command: ModelCommand) -> Vec<ModelEvent> {
        match command {
            ModelCommand::AttachHost { host, name } => self.attach(host, name),
            ModelCommand::HostCommands { host, commands } => {
                let Some(slot) = self.hosts.get_mut(&host) else {
                    return Vec::new();
                };
                slot.commands = Some(commands);
                if std::mem::take(&mut slot.follow_pending) {
                    slot.commands
                        .as_ref()
                        .expect("just set")
                        .send(ClientMessage::Follow { since: slot.seq });
                }
                Vec::new()
            }
            ModelCommand::DetachHost(host) => {
                // The disk copy keeps the host's rows: a daemon detached
                // is not a daemon disowned, and the rows are what a later
                // attach starts from.
                self.hosts.remove(&host);
                self.agents.retain(|_, agent| agent.host != host);
                Vec::new()
            }
            ModelCommand::Follow(agents) => {
                self.followed = agents;
                Vec::new()
            }
        }
    }

    /// A host the workspace attached, with what the last session left of
    /// it: the cursor `Follow` will send, and the agents already folded.
    pub(crate) fn attach(&mut self, host: HostId, name: String) -> Vec<ModelEvent> {
        let mut slot = HostModel::new(name.clone());
        let stored = self.stored.get_or_insert_with(crate::mirror::load);
        if let Some(cursor) = stored.hosts.iter().find(|cursor| cursor.name == name) {
            slot.machine_seed = cursor.machine_seed;
            slot.seq = cursor.seq;
            slot.head = cursor.seq;
        }
        let mut agents = Vec::new();
        let mut verdicts = Vec::new();
        for (agent_id, mirrored) in &stored.agents {
            if mirrored.host != name {
                continue;
            }
            agents.push(rho_registry::MirroredAgent {
                host,
                identity: mirrored.snapshot.identity.clone(),
                digest: mirrored.snapshot.digest.clone(),
            });
            if let Some(verdict) = mirrored.verdict.clone() {
                verdicts.push((*agent_id, verdict));
            }
        }
        self.hosts.insert(host, slot);
        for agent in &agents {
            self.agents.insert(agent.identity.agent_id, agent.clone());
        }
        if agents.is_empty() && verdicts.is_empty() {
            return Vec::new();
        }
        vec![ModelEvent {
            host,
            msg: ModelMsg::Loaded { agents, verdicts },
        }]
    }

    /// One frame from a daemon. Rows are folded and written; everything
    /// else is handed on.
    pub(crate) fn ingest(&mut self, host: HostId, event: ConnEvent) -> Vec<ModelEvent> {
        #[cfg(test)]
        if let ConnEvent::Many(events) = event {
            return events
                .into_iter()
                .flat_map(|event| self.ingest(host, event))
                .collect();
        }
        if let ConnEvent::Log { entries } = event {
            return self.told(host, entries);
        }
        // A live tell for an agent no screen is reading says nothing the
        // digest does not: what a reader sees of it comes from the Turn
        // rows the fold already folded. Dropping it here is what keeps a
        // connect from costing the main thread one rebuild per agent.
        if let ConnEvent::Live { agent_id, .. } = &event
            && !self.followed.contains(agent_id)
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let ConnEvent::Ready {
            machine_seed,
            journal_head,
            ..
        } = &event
        {
            out.extend(self.ready(host, *machine_seed, *journal_head));
        }
        out.push(ModelEvent {
            host,
            msg: ModelMsg::Event(event),
        });
        out
    }

    /// A daemon that has answered: how far its journal runs, and whether
    /// it is the database this copy counts in. Asks for the tail.
    fn ready(&mut self, host: HostId, machine_seed: u64, journal_head: Seq) -> Vec<ModelEvent> {
        let slot = self
            .hosts
            .entry(host)
            .or_insert_with(|| HostModel::new(String::new()));
        slot.head = journal_head;
        // A daemon whose database is not the one this client mirrored, or
        // whose journal is shorter than the copy: the copy starts over.
        let started_over = slot.machine_seed != machine_seed || slot.seq > journal_head;
        let mut out = Vec::new();
        if started_over {
            slot.machine_seed = machine_seed;
            slot.seq = Seq(0);
            slot.pending.clear();
            slot.pending_rows.clear();
            crate::mirror::reset_host(&slot.name);
            self.agents.retain(|_, agent| agent.host != host);
            out.push(ModelEvent {
                host,
                msg: ModelMsg::Loaded {
                    agents: Vec::new(),
                    verdicts: Vec::new(),
                },
            });
        }
        let slot = self.hosts.get_mut(&host).expect("the host was just here");
        tracing::debug!(
            host = slot.name,
            since = slot.seq.0,
            head = journal_head.0,
            started_over,
            "following the host's journal"
        );
        match &slot.commands {
            // Everything after the newest entry held.
            Some(commands) => commands.send(ClientMessage::Follow { since: slot.seq }),
            None => slot.follow_pending = true,
        }
        out
    }

    /// A run of a host's log: folded, written, and announced once the
    /// follow has caught up with the head.
    fn told(&mut self, host: HostId, entries: Vec<LogEntry>) -> Vec<ModelEvent> {
        let Some(page_seq) = entries.last().map(|entry| entry.seq) else {
            return Vec::new();
        };
        if !self.hosts.contains_key(&host) {
            return Vec::new();
        }
        let mut changed = BTreeSet::new();
        let mut rows: BTreeMap<AgentId, Vec<(AgentPos, MirrorEvent)>> = BTreeMap::new();
        for entry in &entries {
            let told = match self.agents.get_mut(&entry.agent_id) {
                Some(mirrored) => mirrored.tell(entry.pos, &entry.event),
                // A row for an agent whose creation this client never
                // heard says nothing; without it nothing can be folded.
                None => {
                    match rho_registry::MirroredAgent::new(host, entry.agent_id, &entry.event) {
                        Some(mirrored) => {
                            self.agents.insert(entry.agent_id, mirrored);
                            true
                        }
                        None => false,
                    }
                }
            };
            if !told {
                continue;
            }
            changed.insert(entry.agent_id);
            if self.followed.contains(&entry.agent_id) {
                rows.entry(entry.agent_id)
                    .or_default()
                    .push((entry.pos, entry.event.clone()));
            }
        }
        // The copy keeps only what could be folded, and the cursor moves
        // whether anything could be or not: the seq says the journal has
        // been seen through here, never that a row was kept here.
        let kept = entries
            .into_iter()
            .filter(|entry| self.agents.contains_key(&entry.agent_id))
            .collect::<Vec<_>>();
        let digests = changed
            .iter()
            .filter_map(|agent_id| {
                let mirrored = self.agents.get(agent_id)?;
                Some((
                    *agent_id,
                    crate::mirror::AgentSnapshot::new(
                        mirrored.identity.clone(),
                        mirrored.digest.clone(),
                    ),
                ))
            })
            .collect();
        let slot = self.hosts.get_mut(&host).expect("the host was just here");
        crate::mirror::write_log(&slot.name, slot.machine_seed, page_seq, kept, digests);
        slot.seq = slot.seq.max(page_seq);
        slot.pending.extend(changed);
        for (agent_id, mut page) in rows {
            slot.pending_rows
                .entry(agent_id)
                .or_default()
                .append(&mut page);
        }
        if slot.seq < slot.head {
            return Vec::new();
        }
        let pending = std::mem::take(&mut slot.pending);
        let pending_rows = std::mem::take(&mut slot.pending_rows);
        let mut out = pending_rows
            .into_iter()
            .map(|(agent_id, rows)| ModelEvent {
                host,
                msg: ModelMsg::Rows { agent_id, rows },
            })
            .collect::<Vec<_>>();
        let agents = pending
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

/// The channels the workspace holds: frames in from every connection,
/// commands in from the main thread, changes out to it.
pub(crate) struct ModelChannels {
    pub incoming: futures_mpsc::UnboundedSender<ToModel>,
    pub changes: futures_mpsc::UnboundedReceiver<ModelEvent>,
}

/// Starts the model on its own thread. A std thread, not a background
/// task: it must not take its turn behind the frames it feeds.
pub(crate) fn spawn() -> ModelChannels {
    let (incoming, incoming_rx) = futures_mpsc::unbounded();
    let (changes_tx, changes) = futures_mpsc::unbounded();
    // A test drives the model inline, on its own thread, so that it can
    // assert in the frame it fed; a second thread would only wake the test
    // scheduler from the wrong place.
    #[cfg(not(test))]
    std::thread::Builder::new()
        .name("rho-model".to_owned())
        .spawn(move || futures::executor::block_on(run(incoming_rx, changes_tx)))
        .expect("spawn the model thread");
    #[cfg(test)]
    drop((incoming_rx, changes_tx));
    ModelChannels { incoming, changes }
}

async fn run(
    mut incoming: futures_mpsc::UnboundedReceiver<ToModel>,
    changes: futures_mpsc::UnboundedSender<ModelEvent>,
) {
    let mut model = Model::new();
    while let Some(item) = incoming.next().await {
        let out = match item {
            ToModel::Event(HostEvent { host, event }) => model.ingest(host, event),
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
    use rho_ui_proto::mirror::{PresentationField, RuntimeKind, SpawnedBy};
    use rho_ui_proto::{AgentRole, AuthState};

    use super::*;

    const HOST: HostId = HostId(0);

    fn agent(id: u64) -> AgentId {
        AgentId::from_counter(id, &rho_ui_proto::AgentIdDomain(0)).expect("an agent id")
    }

    fn created(seq: u64, agent_id: AgentId) -> LogEntry {
        LogEntry {
            seq: Seq(seq),
            agent_id,
            pos: AgentPos::ZERO,
            event: MirrorEvent::Created {
                role: AgentRole::default(),
                runtime: RuntimeKind::Claude,
                workdirs: Vec::new(),
                spawned_by: SpawnedBy::Direct,
                spawn_name: None,
                parent: None,
                model: "test-model".to_owned(),
                at: rho_core::UnixMs(0),
            },
        }
    }

    fn presented(seq: u64, agent_id: AgentId, pos: u64, title: &str) -> LogEntry {
        LogEntry {
            seq: Seq(seq),
            agent_id,
            pos: AgentPos(pos),
            event: MirrorEvent::Presented {
                title: PresentationField::Set(title.to_owned()),
                activity: PresentationField::Unchanged,
                at: rho_core::UnixMs(0),
            },
        }
    }

    fn ready(journal_head: u64) -> ConnEvent {
        ConnEvent::Ready {
            auth: AuthState {
                namespaces: Vec::new(),
                disabled_namespaces: Vec::new(),
                active_namespace: None,
            },
            machine_seed: 7,
            agent_counter: 0,
            journal_head: Seq(journal_head),
        }
    }

    fn live(agent_id: AgentId) -> ConnEvent {
        ConnEvent::Live {
            agent_id,
            live: rho_ui_proto::mirror::Live::Idle,
        }
    }

    fn changed(events: &[ModelEvent]) -> Vec<AgentId> {
        events
            .iter()
            .flat_map(|event| match &event.msg {
                ModelMsg::Changed { agents } => {
                    agents.iter().map(|agent| agent.agent_id()).collect()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn a_catch_up_says_nothing_until_it_reaches_the_head() {
        let mut model = Model::new();
        model.attach(HOST, "local".to_owned());
        model.ingest(HOST, ready(3));

        let first = model.ingest(
            HOST,
            ConnEvent::Log {
                entries: vec![created(1, agent(1)), created(2, agent(2))],
            },
        );
        assert!(
            changed(&first).is_empty(),
            "a page short of the head is arrears, not news"
        );

        let last = model.ingest(
            HOST,
            ConnEvent::Log {
                entries: vec![presented(3, agent(1), 1, "a title")],
            },
        );
        assert_eq!(
            changed(&last),
            vec![agent(1), agent(2)],
            "the page that reaches the head names everything the catch-up moved"
        );
    }

    #[test]
    fn a_page_this_client_cannot_fold_still_moves_the_cursor() {
        let mut model = Model::new();
        model.attach(HOST, "local".to_owned());
        model.ingest(HOST, ready(2));
        // A row for an agent whose creation this client never heard.
        model.ingest(
            HOST,
            ConnEvent::Log {
                entries: vec![presented(2, agent(9), 4, "a title")],
            },
        );
        assert_eq!(
            model.hosts[&HOST].seq,
            Seq(2),
            "the seq says the journal has been seen through here, not that a row was kept"
        );
    }

    #[test]
    fn a_live_tell_for_an_agent_nobody_reads_stops_here() {
        let mut model = Model::new();
        model.attach(HOST, "local".to_owned());
        model.ingest(HOST, ready(0));
        model.command(ModelCommand::Follow(BTreeSet::from([agent(1)])));

        let read = model.ingest(HOST, live(agent(1)));
        assert_eq!(read.len(), 1, "the tail of a followed agent goes up");

        let unread = model.ingest(HOST, live(agent(2)));
        assert!(
            unread.is_empty(),
            "a connect tells every agent's tail; only the read ones cost the main thread anything"
        );
    }

    #[test]
    fn rows_go_up_only_for_the_agents_the_reader_follows() {
        let mut model = Model::new();
        model.attach(HOST, "local".to_owned());
        model.ingest(HOST, ready(0));
        model.command(ModelCommand::Follow(BTreeSet::from([agent(1)])));
        let out = model.ingest(
            HOST,
            ConnEvent::Log {
                entries: vec![created(1, agent(1)), created(2, agent(2))],
            },
        );
        let rows = out
            .iter()
            .filter_map(|event| match &event.msg {
                ModelMsg::Rows { agent_id, .. } => Some(*agent_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(rows, vec![agent(1)]);
    }
}
