//! The map: every agent this client knows, and the indexes the screens
//! read it through.
//!
//! The cost rule holds here from the first line:
//! **an event costs what it touched plus a lookup, and a read costs what
//! it draws.** That is what the indexes are for, and it is what the map
//! is shaped around rather than something checked afterwards.
//!
//! Per event, for `k` agents changed in a map of `n`:
//!
//! - being told about agents (`told`, `restore`, `tell`) — `k log n` for the
//!   rows and every index that mentions them, plus `k log k` to sort the ones
//!   this client has never seen into the front of the order.
//! - the user's filing (`set_agent_filings`) — `k log n`, and nothing at all
//!   when the filing it is given is the filing already held.
//! - a verdict, an activity, a touch, a life change — one lookup.
//! - a host detaching or starting over — `k log n` in the agents that departed.
//!   Nothing walks the agents that did not.
//!
//! Per read, the reads the screens actually make:
//!
//! - a row's own facts (`agent_facts`, `attention`, `agent_display_label`,
//!   `agent_human_name`, `agent_muted`, …) — one lookup each, so a frame costs
//!   the rows it draws.
//! - `agent_children` — the children, no search; `agent_subtree` — the subtree
//!   it returns, plus sorting it.
//! - `next_agent` — a lookup and the agents it steps over, never a pass over
//!   the map to find where the point is.
//! - `agent_by_tag` — a binary search of one host's agents of one role.
//! - `host_agents` — the host's agents.
//!
//! The one read left that is the whole map is `agent_by_label`, which
//! walks every agent asking what it is called. It answers what the user
//! typed in the minibuffer, once per completion, and it is honest to say
//! so here rather than to index it before anything is slow.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8PathBuf;
use rho_agent_hosts::HostId;
use rho_agent_types::AgentId;

use crate::fold::{AgentIdentity, Digest, MirroredAgent, Verdict};
use crate::now_ms;
use crate::protocol::rho2::SpecView;

const LABEL_HEADROOM: u64 = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentLife {
    Known,
    Live,
}

#[derive(Default)]
struct HostSnapshot {
    name: String,
    machine_seed: u64,
    agent_counter: u64,
}

/// The user's own filing of an agent, which lives on the desk and never on
/// the wire: whether they muted it, the labels they placed it under, and
/// the name they gave it after it was running.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentFiling {
    /// The user's mute, which is the desk saying the agent's node is
    /// muted. There is no second way to put an agent away.
    pub muted: bool,
    pub labels: Vec<String>,
    /// `Property::Name` on the agent's own cell. `None` is an agent the
    /// user has not named, which reads as whatever the runtime titled it.
    pub name: Option<String>,
}

/// Uninterpreted chronology a view may read without folding the story
/// itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentFacts {
    /// What its host says it is doing now; `None` while unheard.
    pub runtime: Option<crate::protocol::transcript::RuntimeState>,
    pub last_user_message_at: rho_agent_types::UnixMs,
}

type TagAgents = BTreeMap<HostId, BTreeMap<&'static str, Vec<(String, AgentId)>>>;

/// Where an agent sits in the order the user moves through. Keys are
/// handed out once and never move again: a newly discovered agent takes a
/// key below every key in use, which puts it at the front without
/// renumbering anyone. Two agents that arrive together keep the order they
/// were sorted into.
type OrderKey = i64;

#[derive(Default)]
pub struct AgentMap {
    agents: BTreeMap<AgentId, AgentLife>,
    /// What the user said about each agent; attention is derived from
    /// this and the digest, never stored.
    verdicts: BTreeMap<AgentId, Verdict>,
    /// Ephemeral snapshots, never part of the mirror or persisted filing.
    runtime: BTreeMap<AgentId, crate::protocol::transcript::RuntimeState>,
    disconnected_hosts: BTreeSet<HostId>,
    /// The mirror, folded: what every agent is and what happened to it.
    mirror: BTreeMap<AgentId, MirroredAgent>,
    /// The user's filing, from the store: muted agents, their labels,
    /// and what the user called them.
    filing: BTreeMap<AgentId, AgentFiling>,
    /// Where each agent is currently filed in the `children` and
    /// `tag_agents` indexes. An index has to be told what to remove as well
    /// as what to add, and the fold holds only what the agent is now, so
    /// this holds what it was indexed as. Nothing else reads it.
    indexed_as: BTreeMap<AgentId, (Option<AgentId>, &'static str)>,
    last_active: BTreeMap<AgentId, rho_agent_types::UnixMs>,
    hosts: BTreeMap<HostId, HostSnapshot>,

    // The indexes. Every one of them is kept as the agents that changed
    // are written, never made again from the whole map: an event costs
    // what it touched plus a lookup, and a screen reads what it draws.
    /// Host → its agents. Detaching or resetting a host reads this rather
    /// than walking every agent asking where it came from.
    by_host: BTreeMap<HostId, BTreeSet<AgentId>>,
    agent_hosts: BTreeMap<AgentId, HostId>,
    /// Parent → children, in agent-id order. `agent_subtree` runs on every
    /// dashboard row every frame, so it must not scan the map per call.
    children: BTreeMap<AgentId, Vec<AgentId>>,
    /// Host → role prefix → the agents with that prefix, by encoded id.
    /// What `@eng-b8os` is looked up in.
    tag_agents: TagAgents,
    /// The order the user moves through, and each agent's place in it.
    order: BTreeMap<OrderKey, AgentId>,
    order_keys: BTreeMap<AgentId, OrderKey>,
    /// The next key to hand out at the front. Only ever decreases.
    order_front: OrderKey,
    /// The agents a screen will draw, in order: everything not filed away.
    /// `next_agent` steps through this without collecting it.
    visible: BTreeSet<(OrderKey, AgentId)>,
}

impl AgentMap {
    pub fn attach_host(&mut self, host: HostId, name: String) {
        self.hosts.entry(host).or_default().name = name;
        self.disconnected_hosts.remove(&host);
    }

    /// A host this client is no longer attached to: everything it told us
    /// goes with it. Returns the agents that departed, so the window can
    /// move the point off one it was in.
    pub fn detach_host(&mut self, host: HostId) -> BTreeSet<AgentId> {
        self.disconnected_hosts.remove(&host);
        if self.hosts.remove(&host).is_none() {
            return BTreeSet::new();
        }
        let departed = self.by_host.get(&host).cloned().unwrap_or_default();
        self.forget_agents(&departed);
        for agent_id in &departed {
            self.runtime.remove(agent_id);
        }
        departed
    }

    pub fn host_name(&self, host: HostId) -> &str {
        self.hosts
            .get(&host)
            .map(|h| h.name.as_str())
            .unwrap_or_default()
    }

    pub fn hosts(&self) -> impl Iterator<Item = (HostId, &str)> {
        self.hosts
            .iter()
            .map(|(id, host)| (*id, host.name.as_str()))
    }

    pub fn host_machine_seed(&self, host: HostId) -> u64 {
        self.hosts
            .get(&host)
            .map(|h| h.machine_seed)
            .unwrap_or_default()
    }

    pub fn host_of_agent(&self, agent_id: AgentId) -> Option<HostId> {
        self.agent_hosts.get(&agent_id).copied()
    }

    /// What a host's agents stream says of it when it opens. The agents
    /// come by the directory, not here.
    pub fn set_host_data(&mut self, host: HostId, machine_seed: u64, agent_counter: u64) {
        let snapshot = self.hosts.entry(host).or_default();
        snapshot.machine_seed = machine_seed;
        snapshot.agent_counter = agent_counter;
    }

    /// Drops everything mirrored from a host that is still attached: for
    /// an agent host whose database is not the one this client mirrored.
    pub fn reset_host(&mut self, host: HostId) -> BTreeSet<AgentId> {
        let departed = self.by_host.get(&host).cloned().unwrap_or_default();
        self.forget_agents(&departed);
        departed
    }

    /// The agents named are gone: every table and every index that
    /// mentions one drops it. Costs what departed and a lookup each, not a
    /// pass over the map.
    pub fn forget_agents(&mut self, departed: &BTreeSet<AgentId>) {
        for agent_id in departed {
            self.unindex_agent(*agent_id);
            self.runtime.remove(agent_id);
            self.agents.remove(agent_id);
            self.verdicts.remove(agent_id);
            self.mirror.remove(agent_id);
            self.filing.remove(agent_id);
            self.last_active.remove(agent_id);
            self.indexed_as.remove(agent_id);
        }
    }

    /// The agents the model folded, as they now stand: what this client
    /// held of each is replaced. The fold is the model thread's, so a
    /// catch-up costs one of these and not one per page.
    ///
    /// Returns the agents that changed.
    pub fn told(&mut self, agents: Vec<MirroredAgent>) -> Vec<AgentId> {
        if agents.is_empty() {
            return Vec::new();
        }
        let mut changed = Vec::new();
        for mirrored in agents {
            let agent_id = mirrored.agent_id();
            self.mirror.insert(agent_id, mirrored);
            self.agents.entry(agent_id).or_insert(AgentLife::Known);
            changed.push(agent_id);
        }
        self.refresh(changed.iter().copied());
        changed
    }

    /// The user's filing of the agents named, from the store: muted, and
    /// the labels they put on them. Nothing on the wire carries either.
    ///
    /// The desk files them all at once, so they are taken all at once: one
    /// rebuild for the lot. Filed one by one, a first desk sync of n agents
    /// cost n rebuilds of n agents. Says whether any filing moved, so that
    /// what is derived from filing is made again only when it did.
    pub fn set_agent_filings(
        &mut self,
        filings: impl IntoIterator<Item = (AgentId, AgentFiling)>,
    ) -> bool {
        let mut moved = Vec::new();
        for (agent_id, filing) in filings {
            if self.filing.get(&agent_id) == Some(&filing) {
                continue;
            }
            self.filing.insert(agent_id, filing);
            moved.push(agent_id);
        }
        if moved.is_empty() {
            return false;
        }
        self.refresh(moved);
        true
    }

    /// Every agent this client knows, folded, in agent-id order.
    pub fn all_mirrored(&self) -> impl ExactSizeIterator<Item = &MirroredAgent> {
        self.mirror.values()
    }

    /// Agents read back from the client's own copy, digest and all, so
    /// nothing has to be folded again. Rows that come after fold on top.
    pub fn restore(&mut self, agents: Vec<MirroredAgent>) {
        if agents.is_empty() {
            return;
        }
        let mut restored = Vec::new();
        for mirrored in agents {
            let agent_id = mirrored.agent_id();
            self.mirror.insert(agent_id, mirrored);
            self.agents.entry(agent_id).or_insert(AgentLife::Known);
            restored.push(agent_id);
        }
        self.refresh(restored);
    }

    pub fn mirrored(&self, agent_id: AgentId) -> Option<&MirroredAgent> {
        self.mirror.get(&agent_id)
    }

    pub fn agent_digest(&self, agent_id: AgentId) -> Option<&Digest> {
        self.mirror.get(&agent_id).map(|mirrored| &mirrored.digest)
    }

    pub fn agent_identity(&self, agent_id: AgentId) -> Option<&AgentIdentity> {
        self.mirror
            .get(&agent_id)
            .map(|mirrored| &mirrored.identity)
    }

    /// Every agent mirrored from a host: a read of the index, so it costs
    /// what the host has rather than what the map holds.
    pub fn host_agents(&self, host: HostId) -> Vec<AgentId> {
        self.by_host
            .get(&host)
            .map(|agents| agents.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The agents named have changed: their rows and every index that
    /// mentions them are made again, and nothing else is looked at. Costs
    /// one lookup per index per agent, plus sorting the ones that are new
    /// to the map into the front of the order.
    fn refresh(&mut self, changed: impl IntoIterator<Item = AgentId>) {
        let changed = changed.into_iter().collect::<Vec<_>>();
        if changed.is_empty() {
            return;
        }
        let mut fresh = Vec::new();
        for agent_id in &changed {
            if !self.order_keys.contains_key(agent_id) && self.mirror.contains_key(agent_id) {
                fresh.push(*agent_id);
            }
            self.refresh_row(*agent_id);
        }
        // Agents this client has not seen before go to the front, most
        // recently active first, the way a whole run of them used to be
        // spliced in after a rebuild.
        fresh.sort_by_key(|agent_id| {
            (
                Reverse(self.last_active.get(agent_id).copied().unwrap_or_default()),
                *agent_id,
            )
        });
        self.order_front -= fresh.len() as OrderKey;
        for (offset, agent_id) in fresh.into_iter().enumerate() {
            let key = self.order_front + offset as OrderKey;
            self.order.insert(key, agent_id);
            self.order_keys.insert(agent_id, key);
        }
        for agent_id in changed {
            self.reindex_visibility(agent_id);
        }
    }

    /// One agent's row, and the indexes that depend on what the row says:
    /// which host it is on, whose child it is, what it is tagged.
    fn refresh_row(&mut self, agent_id: AgentId) {
        let Some(mirrored) = self.mirror.get(&agent_id) else {
            return;
        };
        let host = mirrored.host;
        let identity = &mirrored.identity;
        let digest = &mirrored.digest;
        let last_active = digest.last_active.max(identity.entry.created_at);
        let now = (identity.parent(), identity.handle_prefix());
        self.agents.entry(agent_id).or_insert(AgentLife::Known);
        let active = self
            .last_active
            .entry(agent_id)
            .or_insert(rho_agent_types::UnixMs(0));
        *active = (*active).max(last_active);

        let was = self.indexed_as.get(&agent_id).copied();
        let moved_host = self.agent_hosts.insert(agent_id, host) != Some(host);
        if moved_host {
            self.by_host.retain(|owner, agents| {
                if *owner != host {
                    agents.remove(&agent_id);
                }
                !agents.is_empty()
            });
            self.by_host.entry(host).or_default().insert(agent_id);
        }
        if was.map(|(parent, _)| parent) != Some(now.0) {
            if let Some(parent) = was.and_then(|(parent, _)| parent) {
                self.remove_child(parent, agent_id);
            }
            if let Some(parent) = now.0 {
                let children = self.children.entry(parent).or_default();
                if let Err(at) = children.binary_search(&agent_id) {
                    children.insert(at, agent_id);
                }
            }
        }
        if was.map(|(_, tag)| tag) != Some(now.1) || moved_host {
            if let Some((_, tag)) = was {
                self.untag(agent_id, tag);
            }
            let tagged = self
                .tag_agents
                .entry(host)
                .or_default()
                .entry(now.1)
                .or_default();
            let entry = (agent_id.encoded(), agent_id);
            if let Err(at) = tagged.binary_search_by(|held| held.0.cmp(&entry.0)) {
                tagged.insert(at, entry);
            }
        }
        self.indexed_as.insert(agent_id, now);
    }

    /// Whether a screen draws this agent at all: the user's filing is the
    /// only thing that says so, and moving through the map reads it.
    fn reindex_visibility(&mut self, agent_id: AgentId) {
        let Some(key) = self.order_keys.get(&agent_id).copied() else {
            return;
        };
        let entry = (key, agent_id);
        if self.agent_muted(agent_id) {
            self.visible.remove(&entry);
        } else {
            self.visible.insert(entry);
        }
    }

    /// Everything an agent is in, dropped: for one that has departed.
    fn unindex_agent(&mut self, agent_id: AgentId) {
        if let Some(host) = self.agent_hosts.remove(&agent_id)
            && let Some(agents) = self.by_host.get_mut(&host)
        {
            agents.remove(&agent_id);
            if agents.is_empty() {
                self.by_host.remove(&host);
            }
        }
        if let Some((parent, tag)) = self.indexed_as.remove(&agent_id) {
            if let Some(parent) = parent {
                self.remove_child(parent, agent_id);
            }
            self.untag(agent_id, tag);
        }
        self.children.remove(&agent_id);
        if let Some(key) = self.order_keys.remove(&agent_id) {
            self.order.remove(&key);
            self.visible.remove(&(key, agent_id));
        }
    }

    fn remove_child(&mut self, parent: AgentId, child: AgentId) {
        if let Some(children) = self.children.get_mut(&parent) {
            children.retain(|held| *held != child);
            if children.is_empty() {
                self.children.remove(&parent);
            }
        }
    }

    fn untag(&mut self, agent_id: AgentId, tag: &'static str) {
        for roles in self.tag_agents.values_mut() {
            if let Some(tagged) = roles.get_mut(tag) {
                tagged.retain(|held| held.1 != agent_id);
            }
        }
    }

    /// What the user last said about this agent. Returns whether it is
    /// new, so the caller writes it down only then.
    pub fn set_agent_verdict(&mut self, agent_id: AgentId, verdict: Verdict) -> bool {
        if self.verdicts.get(&agent_id) == Some(&verdict) {
            return false;
        }
        self.verdicts.insert(agent_id, verdict);
        true
    }

    pub fn agent_verdict(&self, agent_id: AgentId) -> Verdict {
        self.verdicts.get(&agent_id).copied().unwrap_or_default()
    }
    /// The agent's status line: its newest status, until any later
    /// message from either side.
    pub fn agent_status(&self, agent_id: AgentId) -> Option<&str> {
        self.agent_digest(agent_id)?
            .status
            .as_ref()
            .map(|(_, text)| text.as_str())
    }
    pub fn touch_agent(&mut self, agent_id: AgentId) {
        self.last_active
            .insert(agent_id, rho_agent_types::UnixMs(now_ms()));
    }
    pub fn agent_subtree(&self, agent_id: AgentId) -> Vec<AgentId> {
        // Hidden agents are excluded from the result but still walked,
        // so descendants behind a muted intermediate are found.
        let mut seen = BTreeSet::from([agent_id]);
        let mut queue = vec![agent_id];
        let mut descendants = Vec::new();
        while let Some(cursor) = queue.pop() {
            for child in self.children.get(&cursor).map_or(&[][..], Vec::as_slice) {
                if seen.insert(*child) {
                    queue.push(*child);
                    if !self.agent_muted(*child) {
                        descendants.push(*child);
                    }
                }
            }
        }
        // Callers see members in summary order, which is agent-id order.
        descendants.sort_unstable();
        let mut result = vec![agent_id];
        result.extend(descendants);
        result
    }

    /// Direct children in stable summary order. Unlike `agent_subtree`, this
    /// preserves the runtime hierarchy and includes muted agents so a full
    /// tree never silently rewrites its ancestry.
    pub fn agent_children(&self, agent_id: AgentId) -> &[AgentId] {
        self.children.get(&agent_id).map_or(&[][..], Vec::as_slice)
    }

    pub fn agent_id_label(&self, agent_id: AgentId) -> String {
        let host = self.host_of_agent(agent_id);
        let counter = host
            .and_then(|h| self.hosts.get(&h))
            .map(|h| h.agent_counter)
            .unwrap_or_else(|| {
                self.hosts
                    .values()
                    .map(|h| h.agent_counter)
                    .max()
                    .unwrap_or_default()
            });
        let len = prefix_id::uniform_prefix_len(counter, LABEL_HEADROOM).max(4);
        let prefix = self
            .agent_identity(agent_id)
            .map(AgentIdentity::handle_prefix)
            .unwrap_or("eng");
        let label = format!("{prefix}-{}", &agent_id.encoded()[..len]);
        match host.filter(|_| self.hosts.len() > 1) {
            Some(h) => format!("{}/{label}", self.host_name(h)),
            None => label,
        }
    }
    /// Resolves a bare desk tag (`eng-x7y2`) to the unique matching agent on
    /// `host`. Prefixes written when the id space was smaller keep resolving
    /// for as long as they stay unambiguous.
    pub fn agent_by_tag(&self, host: HostId, label: &str) -> Option<AgentId> {
        let (role_prefix, encoded_prefix) = label.split_once('-')?;
        let agents = self.tag_agents.get(&host)?.get(role_prefix)?;
        let start = agents.partition_point(|(encoded, _)| encoded.as_str() < encoded_prefix);
        let found = agents.get(start)?.1;
        agents[start]
            .0
            .starts_with(encoded_prefix)
            .then_some(found)
            .filter(|_| {
                !agents
                    .get(start + 1)
                    .is_some_and(|(encoded, _)| encoded.starts_with(encoded_prefix))
            })
    }

    /// The directory the agent works in, as the agent sees it.
    pub fn working_directory(&self, agent_id: AgentId) -> Option<Utf8PathBuf> {
        self.agent_place(agent_id).map(|place| place.cwd)
    }
    /// Where the agent works.
    pub fn agent_place(&self, agent_id: AgentId) -> Option<rho_agent_types::Place> {
        self.agent_identity(agent_id).map(AgentIdentity::place)
    }
    /// The workset the agent is in.
    pub fn agent_workset(&self, agent_id: AgentId) -> Option<rho_agent_types::WorksetId> {
        self.agent_identity(agent_id)
            .map(|identity| identity.entry.workset)
    }
    /// What the agent runs on.
    pub fn agent_spec(&self, agent_id: AgentId) -> Option<&SpecView> {
        self.agent_identity(agent_id)
            .map(|identity| &identity.entry.spec)
    }
    pub fn agent_parent(&self, agent_id: AgentId) -> Option<AgentId> {
        self.agent_identity(agent_id)
            .and_then(AgentIdentity::parent)
    }
    /// The user manages this agent: the policy that made it said it is
    /// user-facing. Any other agent works for an agent: it is not dealt,
    /// not on Home, not in the running list, and not in Find, and its
    /// waiting reaches the user through its parent's card. Filing is
    /// placement and says nothing about it.
    pub fn owned_by_user(&self, agent_id: AgentId) -> bool {
        self.agent_identity(agent_id)
            .is_some_and(AgentIdentity::user_facing)
    }
    /// The user muted this agent: they said "not this one", and nothing
    /// the agent does takes that back. Every list that draws agents asks
    /// this and leaves it out.
    pub fn agent_muted(&self, agent_id: AgentId) -> bool {
        self.filing
            .get(&agent_id)
            .is_some_and(|filing| filing.muted)
    }
    fn agent_labels(&self, agent_id: AgentId) -> &[String] {
        self.filing
            .get(&agent_id)
            .map_or(&[][..], |filing| filing.labels.as_slice())
    }
    pub fn agent_last_active(&self, agent_id: AgentId) -> Option<rho_agent_types::UnixMs> {
        self.last_active.get(&agent_id).copied()
    }
    /// The chronology, folded from the story rather than sent.
    pub fn agent_facts(&self, agent_id: AgentId) -> AgentFacts {
        let Some(digest) = self.agent_digest(agent_id) else {
            return AgentFacts::default();
        };
        AgentFacts {
            runtime: self.runtime.get(&agent_id).cloned(),
            last_user_message_at: digest.last_user_message_at,
        }
    }
    /// What the agent is called: the name the user gave it if they gave
    /// one, else what the runtime titled it. A name is the user saying
    /// which agent this is; a title is a guess from the first thing said
    /// to it, so the name wins.
    pub fn agent_display_name(&self, agent_id: AgentId) -> Option<&str> {
        self.filing
            .get(&agent_id)
            .and_then(|filing| filing.name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .or_else(|| self.mirror.get(&agent_id).and_then(MirroredAgent::title))
    }
    pub fn agent_display_label(&self, agent_id: AgentId) -> String {
        let id = self.agent_id_label(agent_id);
        self.agent_display_name(agent_id)
            .filter(|n| !n.trim().is_empty())
            .map_or_else(|| id.clone(), |n| format!("{n} ({id})"))
    }
    /// Where the user filed the agent, after its name. The labels are the
    /// ones this map already holds for it — the desk's filing, fed in by
    /// `set_agent_filings` — so a name costs a lookup and a join, and a
    /// frame that draws k agents pays for k of them and nothing else.
    ///
    /// An agent filed nowhere reads exactly as it did before: a name with
    /// no labels has nothing to say about placement.
    pub fn agent_name_with_labels(&self, agent_id: AgentId, name: String) -> String {
        match self.agent_labels(agent_id) {
            labels if !labels.is_empty() => format!("{name} · {}", labels.join(" › ")),
            _ => name,
        }
    }
    /// What the user last said to the agent, for finding it by the words
    /// they remember rather than by a name they never gave it.
    pub fn agent_last_user_message(&self, agent_id: AgentId) -> Option<&str> {
        self.agent_digest(agent_id)
            .map(|digest| digest.last_user_message_text.trim())
            .filter(|text| !text.is_empty())
    }
    pub fn agent_human_name(&self, agent_id: AgentId) -> String {
        let Some(identity) = self.agent_identity(agent_id) else {
            return "Untitled agent".into();
        };
        if let Some(name) = self.agent_display_name(agent_id) {
            return name.into();
        }
        if let Some(said) = self.agent_last_user_message(agent_id) {
            return said.into();
        }
        if identity.is_advisor() {
            "Advisor".into()
        } else {
            "Engineer".into()
        }
    }

    pub fn mark_known(&mut self, agent_id: AgentId) {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.agents.entry(agent_id) {
            entry.insert(AgentLife::Known);
        }
    }
    pub fn set_runtime(
        &mut self,
        agent_id: AgentId,
        state: crate::protocol::transcript::RuntimeState,
    ) -> bool {
        let changed = self.runtime.insert(agent_id, state.clone()).as_ref() != Some(&state);
        if let Some(host) = self.host_of_agent(agent_id) {
            self.disconnected_hosts.remove(&host);
        }
        changed
    }

    pub fn clear_host_runtime(&mut self, host: HostId) {
        self.disconnected_hosts.insert(host);
        let agents = self.by_host.get(&host).cloned().unwrap_or_default();
        for agent_id in agents {
            self.runtime.remove(&agent_id);
        }
    }

    pub fn mark_live(&mut self, agent_id: AgentId) -> bool {
        let previous = self.agents.insert(agent_id, AgentLife::Live);
        previous != Some(AgentLife::Live)
    }
    pub fn mark_not_live(&mut self, agent_id: AgentId) {
        self.runtime.remove(&agent_id);
        self.agents.insert(agent_id, AgentLife::Known);
    }
    /// The agent `delta` steps from the one the user is in, through the
    /// agents a screen draws, wrapping at either end. Where the point is
    /// belongs to the window, so it is handed in rather than kept here.
    ///
    /// Each step is a lookup in the order, so a press costs what it moves
    /// over and not what exists.
    pub fn next_agent(&self, selected: Option<AgentId>, delta: isize) -> Option<AgentId> {
        let first = self.visible.iter().next().copied()?;
        let last = self.visible.iter().next_back().copied()?;
        let Some(mut at) = selected
            .and_then(|agent_id| Some((*self.order_keys.get(&agent_id)?, agent_id)))
            .filter(|entry| self.visible.contains(entry))
        else {
            return Some(if delta < 0 { last.1 } else { first.1 });
        };
        for _ in 0..delta.unsigned_abs() {
            at = if delta < 0 {
                self.visible
                    .range(..at)
                    .next_back()
                    .copied()
                    .unwrap_or(last)
            } else {
                self.visible
                    .range((std::ops::Bound::Excluded(at), std::ops::Bound::Unbounded))
                    .next()
                    .copied()
                    .unwrap_or(first)
            };
        }
        Some(at.1)
    }
    pub fn agent_by_label(&self, label: &str) -> Option<AgentId> {
        let label = label.strip_prefix('@').unwrap_or(label);
        let exact = self.agents.keys().copied().find(|id| {
            self.agent_id_label(*id) == label
                || self
                    .agent_display_name(*id)
                    .is_some_and(|n| n.eq_ignore_ascii_case(label))
        });
        if exact.is_some() || label.contains('/') {
            return exact;
        }
        let mut matches = self
            .agents
            .keys()
            .copied()
            .filter(|id| self.agent_id_label(*id).rsplit('/').next() == Some(label));
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }
    pub fn known_agents(&self) -> impl Iterator<Item = &AgentId> {
        self.agents.keys()
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{
        Address, AgentIdDomain, ContentPart, Mail, MailId, SendKind, UnixMs, WorksetKind, props,
    };

    use super::*;
    use crate::fold::ConversationPos;
    use crate::protocol::rho2::AgentEntry;
    use crate::protocol::transcript::RuntimeKind;

    fn agent(nth: u64) -> AgentId {
        AgentId::from_counter(nth, &AgentIdDomain(0)).unwrap()
    }

    /// An agent as the directory names it: the user's unless it has a
    /// parent.
    fn named(nth: u64, parent: Option<AgentId>, at: u64) -> MirroredAgent {
        let mut spec_props = Props::new();
        match parent {
            Some(parent) => {
                spec_props.insert(props::PARENT.into(), parent.encoded());
            }
            None => {
                spec_props.insert(props::USER_FACING.into(), "true".into());
            }
        }
        MirroredAgent::new(
            HostId::default(),
            AgentIdentity {
                entry: AgentEntry {
                    agent: agent(nth),
                    workset: "0123456789ab".parse().unwrap(),
                    spec: SpecView {
                        runtime: RuntimeKind::Rho,
                        model: "sol".into(),
                        effort: "medium".into(),
                        props: spec_props,
                    },
                    created_at: UnixMs(at),
                },
                kind: WorksetKind::Isolated,
            },
        )
    }

    type Props = rho_agent_types::Props;

    fn to_user(id: u64, from: AgentId, kind: SendKind, text: &str) -> Mail {
        Mail {
            id: MailId(id),
            at: UnixMs(id),
            from: Address::Agent(from),
            to: Address::User,
            thread: rho_agent_types::ThreadId(MailId(id)),
            kind: Some(kind),
            content: vec![ContentPart::Text { text: text.into() }],
        }
    }

    #[test]
    fn runtime_status_is_live_and_disconnect_clears_it() {
        use crate::protocol::transcript::RuntimeState;
        let host = HostId::default();
        let mut map = AgentMap::default();
        map.told(vec![named(1, None, 1)]);
        map.set_runtime(
            agent(1),
            RuntimeState {
                running_tasks: 2,
                ..Default::default()
            },
        );
        assert_eq!(
            map.agent_facts(agent(1))
                .runtime
                .map(|runtime| runtime.running_tasks),
            Some(2)
        );
        map.clear_host_runtime(host);
        assert!(map.agent_facts(agent(1)).runtime.is_none());
    }

    /// Whose an agent is comes from its props: user-facing is the user's,
    /// and an agent with neither is not, whatever its parent.
    #[test]
    fn an_agent_is_the_users_when_the_policy_says_so() {
        let mut registry = AgentMap::default();
        registry.set_host_data(HostId::default(), 0, 3);
        let mut neither = named(3, None, 3);
        neither.identity.entry.spec.props.clear();
        registry.told(vec![
            named(1, None, 1),
            named(2, Some(agent(1)), 2),
            neither,
        ]);

        assert!(registry.owned_by_user(agent(1)));
        assert!(!registry.owned_by_user(agent(2)));
        assert!(!registry.owned_by_user(agent(3)));
        assert_eq!(registry.agent_parent(agent(2)), Some(agent(1)));
        assert_eq!(registry.agent_children(agent(1)), [agent(2)]);
    }

    /// The desk files every agent in one go, so the registry rebuilds once.
    #[test]
    fn filing_a_whole_desk_rebuilds_once() {
        let mut registry = AgentMap::default();
        let agents = (1..=32).map(agent).collect::<Vec<_>>();
        for agent_id in &agents {
            registry.mark_known(*agent_id);
        }
        let filings = agents
            .iter()
            .map(|agent_id| {
                (
                    *agent_id,
                    AgentFiling {
                        labels: vec!["rho/agent".to_owned()],
                        ..AgentFiling::default()
                    },
                )
            })
            .collect::<Vec<_>>();
        assert!(registry.set_agent_filings(filings.clone()));
        assert!(!registry.set_agent_filings(filings));
    }

    #[test]
    fn a_verdict_said_twice_changes_nothing_the_second_time() {
        let mut registry = AgentMap::default();
        registry.mark_known(agent(1));
        let handled = Verdict {
            handled_through: ConversationPos(3),
            muted: false,
        };
        assert!(registry.set_agent_verdict(agent(1), handled));
        assert!(!registry.set_agent_verdict(agent(1), handled));
        assert_eq!(registry.agent_verdict(agent(1)), handled);
    }

    /// What the rails read is the fold of the agent's mail: its status
    /// line, and what it put to the user.
    #[test]
    fn the_mail_is_what_the_rails_read() {
        let host = HostId::default();
        let mut registry = AgentMap::default();
        registry.set_host_data(host, 0, 1);
        let mut mirrored = named(1, None, 1);
        mirrored.tell(&to_user(1, agent(1), SendKind::Status, "reading"));
        assert_eq!(registry.told(vec![mirrored.clone()]), [agent(1)]);
        assert_eq!(registry.agent_status(agent(1)), Some("reading"));
        assert!(registry.agent_digest(agent(1)).unwrap().unread.is_empty());
        assert_eq!(registry.host_of_agent(agent(1)), Some(host));

        mirrored.tell(&to_user(2, agent(1), SendKind::Ask, "which one?"));
        registry.told(vec![mirrored]);
        assert_eq!(registry.agent_status(agent(1)), None, "a send hides it");
        assert_eq!(registry.agent_digest(agent(1)).unwrap().unread.len(), 1);

        registry.reset_host(host);
        assert_eq!(registry.all_mirrored().len(), 0);
    }

    /// Labels read the preset: an advisor is `adv-`, anything else `eng-`,
    /// and the label finds the agent again.
    #[test]
    fn a_label_reads_the_preset() {
        let host = HostId::default();
        let mut map = AgentMap::default();
        map.set_host_data(host, 0, 3);
        let mut advisor = named(2, None, 2);
        advisor
            .identity
            .entry
            .spec
            .props
            .insert(crate::fold::PRESET.into(), "advisor".into());
        map.told(vec![named(1, None, 1), advisor]);
        let engineer = map.agent_id_label(agent(1));
        let advisor = map.agent_id_label(agent(2));
        assert!(engineer.starts_with("eng-"), "{engineer}");
        assert!(advisor.starts_with("adv-"), "{advisor}");
        assert_eq!(map.agent_by_tag(host, &advisor), Some(agent(2)));
        assert_eq!(map.agent_human_name(agent(2)), "Advisor");
    }

    /// The order is what the user moves through, and it is handed out
    /// once: an agent that arrives later goes to the front and moves
    /// nobody, and one that arrives again does not move at all.
    #[test]
    fn the_order_puts_the_newest_first_and_leaves_the_rest_where_they_were() {
        let mut map = AgentMap::default();
        map.set_host_data(HostId::default(), 0, 3);
        map.told(vec![named(1, None, 10)]);
        map.told(vec![named(2, None, 20)]);
        assert_eq!(map.next_agent(None, 1), Some(agent(2)));
        assert_eq!(map.next_agent(Some(agent(2)), 1), Some(agent(1)));
        assert_eq!(map.next_agent(Some(agent(1)), 1), Some(agent(2)));
        assert_eq!(map.next_agent(Some(agent(2)), -1), Some(agent(1)));
        assert_eq!(map.next_agent(None, -1), Some(agent(1)));

        map.told(vec![named(3, None, 30)]);
        assert_eq!(map.next_agent(None, 1), Some(agent(3)));
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(2)));

        let mut again = named(1, None, 10);
        again.tell(&to_user(1, agent(1), SendKind::Result, "news"));
        map.told(vec![again]);
        assert_eq!(map.next_agent(None, 1), Some(agent(3)));
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(2)));
    }

    #[test]
    fn a_filed_agent_is_stepped_over_and_comes_back_in_place() {
        let mut map = AgentMap::default();
        map.set_host_data(HostId::default(), 0, 3);
        for (nth, at) in [(1, 10), (2, 20), (3, 30)] {
            map.told(vec![named(nth, None, at)]);
        }
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(2)));
        assert!(map.set_agent_filings([(
            agent(2),
            AgentFiling {
                muted: true,
                ..AgentFiling::default()
            },
        )]));
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(1)));
        assert_eq!(map.next_agent(Some(agent(2)), 1), Some(agent(3)));
        assert!(map.set_agent_filings([(agent(2), AgentFiling::default())]));
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(2)));
    }

    /// The indexes say the same after a change as they would if made from
    /// scratch: a child reparented, an agent gone, a host reset.
    #[test]
    fn the_indexes_follow_what_changed() {
        let host = HostId::default();
        let mut map = AgentMap::default();
        map.set_host_data(host, 0, 3);
        map.told(vec![
            named(1, None, 10),
            named(2, Some(agent(1)), 20),
            named(3, Some(agent(1)), 30),
        ]);
        let mut children = vec![agent(2), agent(3)];
        children.sort_unstable();
        assert_eq!(map.agent_children(agent(1)), children);
        assert_eq!(
            map.agent_subtree(agent(1)),
            [vec![agent(1)], children.clone()].concat()
        );
        // Reparented by a changed spec.
        map.told(vec![named(3, Some(agent(2)), 30)]);
        assert_eq!(map.agent_children(agent(1)), [agent(2)]);
        assert_eq!(map.agent_children(agent(2)), [agent(3)]);

        let handle = map.agent_id_label(agent(2));
        assert_eq!(map.agent_by_tag(host, &handle), Some(agent(2)));
        map.forget_agents(&BTreeSet::from([agent(2)]));
        assert_eq!(map.agent_by_tag(host, &handle), None);
        assert!(map.agent_children(agent(2)).is_empty());

        let departed = map.reset_host(host);
        assert_eq!(departed.len(), 2);
        assert_eq!(map.all_mirrored().len(), 0);
        assert_eq!(map.host_agents(host), []);
        assert_eq!(map.next_agent(None, 1), None);
    }
}
