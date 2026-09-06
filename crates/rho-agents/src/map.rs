//! The map: every agent this client knows, and the indexes the screens
//! read it through.
//!
//! The cost rule (`GUI-MODEL-DESIGN.md`) holds here from the first line:
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
//!   `agent_human_name`, `agent_hidden`, …) — one lookup each, so a frame costs
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
use rho_hosts::HostId;
use rho_ui_proto::AgentId;
use rho_ui_proto::mirror::AgentWant;
#[cfg(test)]
use rho_ui_proto::mirror::LogEntry;

use crate::fold::{AgentIdentity, Attention, Digest, MirroredAgent, Verdict, Wants, attention};
use crate::now_ms;

pub const HIDE_LABEL: &str = "hide";
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

/// One agent as the rails read it: its head, what its story folded to, and
/// the user's own filing, which comes from the store rather than the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSummary {
    pub agent_id: AgentId,
    pub parent_agent: Option<AgentId>,
    pub display_name: Option<String>,
    pub created_at: rho_core::UnixMs,
    pub role: rho_ui_proto::AgentRole,
    pub workspace: rho_ui_proto::WorkspaceInfo,
    pub last_active: rho_core::UnixMs,
    /// The user filed this agent away: the store's `Muted`, fed in by the
    /// view rather than read here.
    pub hidden: bool,
    pub last_user_message_text: String,
    pub activity: Option<String>,
    pub labels: Vec<String>,
}

impl AgentSummary {
    fn of(mirrored: &MirroredAgent) -> Self {
        let identity = &mirrored.identity;
        let digest = &mirrored.digest;
        Self {
            agent_id: identity.agent_id,
            parent_agent: identity.parent,
            display_name: mirrored.title().map(str::to_owned),
            created_at: identity.created_at,
            role: identity.role,
            // Every agent is created with at least one workdir, so the
            // fallback is only for a creation that arrived malformed.
            workspace: identity.workspace().cloned().unwrap_or(
                rho_ui_proto::WorkspaceInfo::UserCheckout {
                    repo: Default::default(),
                },
            ),
            last_active: digest.last_active.max(identity.created_at),
            hidden: false,
            last_user_message_text: digest.last_user_message_text.clone(),
            activity: digest.activity.clone(),
            labels: Vec::new(),
        }
    }
}

/// Uninterpreted chronology a view may read without folding the story
/// itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentFacts {
    pub turn_running: bool,
    /// When the running turn began, when the client saw it start.
    pub turn_started_at: Option<rho_core::UnixMs>,
    pub last_turn_ended: Option<rho_core::UnixMs>,
    pub last_user_message_at: rho_core::UnixMs,
    /// The last turn said it wants something only the user can give.
    pub needs_you_hint: bool,
    /// The last turn died. Nobody but the user can restart it, so this
    /// waits on the user as much as a question does.
    pub errored: bool,
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
    activities: BTreeMap<AgentId, String>,
    /// The mirror, folded: what every agent is and what happened to it.
    mirror: BTreeMap<AgentId, MirroredAgent>,
    /// The user's filing, from the store: hidden agents and their labels.
    filing: BTreeMap<AgentId, (bool, Vec<String>)>,
    /// One row per agent, in agent-id order: what the rails read.
    summaries: BTreeMap<AgentId, AgentSummary>,
    last_active: BTreeMap<AgentId, rho_core::UnixMs>,
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
    announced_hosts: BTreeMap<AgentId, HostId>,
}

impl AgentMap {
    pub fn attach_host(&mut self, host: HostId, name: String) {
        self.hosts.entry(host).or_default().name = name;
    }

    /// A host this client is no longer attached to: everything it told us
    /// goes with it. Returns the agents that departed, so the window can
    /// move the point off one it was in.
    pub fn detach_host(&mut self, host: HostId) -> BTreeSet<AgentId> {
        if self.hosts.remove(&host).is_none() {
            return BTreeSet::new();
        }
        let departed = self
            .by_host
            .get(&host)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .chain(
                self.announced_hosts
                    .iter()
                    .filter(|(_, owner)| **owner == host)
                    .map(|(id, _)| *id),
            )
            .collect::<BTreeSet<_>>();
        self.forget_agents(&departed);
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

    pub fn host_count(&self) -> usize {
        self.hosts.len()
    }

    pub fn host_machine_seed(&self, host: HostId) -> u64 {
        self.hosts
            .get(&host)
            .map(|h| h.machine_seed)
            .unwrap_or_default()
    }

    pub fn host_of_agent(&self, agent_id: AgentId) -> Option<HostId> {
        self.agent_hosts
            .get(&agent_id)
            .or_else(|| self.announced_hosts.get(&agent_id))
            .copied()
    }

    pub fn note_agent_created(&mut self, host: HostId, agent_id: AgentId) {
        self.announced_hosts.insert(agent_id, host);
        self.mark_known(agent_id);
    }

    /// What `Ready` says of a host. The agents come by the log, not here.
    pub fn set_host_data(&mut self, host: HostId, machine_seed: u64, agent_counter: u64) {
        let snapshot = self.hosts.entry(host).or_default();
        snapshot.machine_seed = machine_seed;
        snapshot.agent_counter = agent_counter;
    }

    /// Drops everything mirrored from a host that is still attached: for
    /// a daemon whose database is not the one this client mirrored.
    pub fn reset_host(&mut self, host: HostId) -> BTreeSet<AgentId> {
        let departed = self.by_host.get(&host).cloned().unwrap_or_default();
        self.forget_agents(&departed);
        departed
    }

    /// The agents named are gone: every table and every index that
    /// mentions one drops it. Costs what departed and a lookup each, not a
    /// pass over the map.
    fn forget_agents(&mut self, departed: &BTreeSet<AgentId>) {
        for agent_id in departed {
            self.unindex_agent(*agent_id);
            self.agents.remove(agent_id);
            self.verdicts.remove(agent_id);
            self.activities.remove(agent_id);
            self.mirror.remove(agent_id);
            self.filing.remove(agent_id);
            self.last_active.remove(agent_id);
            self.announced_hosts.remove(agent_id);
            self.summaries.remove(agent_id);
        }
    }

    /// A run of a host's log, folded. The client's own fold runs on the
    /// model thread now; this is what the registry's tests fold with, and
    /// what `told` is checked against.
    #[cfg(test)]
    pub fn tell(&mut self, host: HostId, entries: &[LogEntry]) -> Vec<AgentId> {
        let mut changed = Vec::new();
        for entry in entries {
            let told = match self.mirror.get_mut(&entry.agent_id) {
                Some(mirrored) => mirrored.tell(entry.pos, &entry.event),
                None => match MirroredAgent::new(host, entry.agent_id, &entry.event) {
                    Some(mirrored) => {
                        self.mirror.insert(entry.agent_id, mirrored);
                        self.agents
                            .entry(entry.agent_id)
                            .or_insert(AgentLife::Known);
                        true
                    }
                    None => false,
                },
            };
            if told && !changed.contains(&entry.agent_id) {
                changed.push(entry.agent_id);
            }
        }
        self.refresh(changed.iter().copied());
        changed
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

    /// The user's filing of the agents named, from the store: hidden, and
    /// the labels they put on them. Nothing on the wire carries either.
    ///
    /// The desk files them all at once, so they are taken all at once: one
    /// rebuild for the lot. Filed one by one, a first desk sync of n agents
    /// cost n rebuilds of n agents. Says whether any filing moved, so that
    /// what is derived from filing is made again only when it did.
    pub fn set_agent_filings(
        &mut self,
        filings: impl IntoIterator<Item = (AgentId, bool, Vec<String>)>,
    ) -> bool {
        let mut moved = Vec::new();
        for (agent_id, hidden, labels) in filings {
            let hidden = hidden || labels.iter().any(|label| label == HIDE_LABEL);
            if self
                .filing
                .get(&agent_id)
                .is_some_and(|filed| filed.0 == hidden && filed.1 == labels)
            {
                continue;
            }
            self.filing.insert(agent_id, (hidden, labels));
            moved.push(agent_id);
        }
        if moved.is_empty() {
            return false;
        }
        self.refresh(moved);
        true
    }

    /// Every agent this client knows, in agent-id order.
    pub fn summaries(&self) -> impl ExactSizeIterator<Item = &AgentSummary> {
        self.summaries.values()
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
        let mut summary = AgentSummary::of(mirrored);
        if let Some((hidden, labels)) = self.filing.get(&agent_id) {
            summary.hidden = *hidden;
            summary.labels = labels.clone();
        }
        self.agents.entry(agent_id).or_insert(AgentLife::Known);
        match &summary.activity {
            Some(activity) => {
                self.activities.insert(agent_id, activity.clone());
            }
            None => {
                self.activities.remove(&agent_id);
            }
        }
        let active = self
            .last_active
            .entry(agent_id)
            .or_insert(rho_core::UnixMs(0));
        *active = (*active).max(summary.last_active);

        let previous = self.summaries.get(&agent_id);
        let was = previous.map(|row| (row.parent_agent, row.role.handle_prefix()));
        let now = (summary.parent_agent, summary.role.handle_prefix());
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
        self.summaries.insert(agent_id, summary);
    }

    /// Whether a screen draws this agent at all: the user's filing is the
    /// only thing that says so, and moving through the map reads it.
    fn reindex_visibility(&mut self, agent_id: AgentId) {
        let Some(key) = self.order_keys.get(&agent_id).copied() else {
            return;
        };
        let entry = (key, agent_id);
        let hidden = self
            .summaries
            .get(&agent_id)
            .is_some_and(|summary| summary.hidden);
        if hidden {
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
        if let Some(row) = self.summaries.get(&agent_id) {
            let parent = row.parent_agent;
            let tag = row.role.handle_prefix();
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
    pub fn set_activity(&mut self, agent_id: AgentId, activity: String) {
        self.activities.insert(agent_id, activity);
    }
    pub fn agent_activity(&self, agent_id: AgentId) -> Option<&str> {
        self.activities.get(&agent_id).map(String::as_str)
    }
    /// What the agent's last finished turn says it wants, while the ball
    /// is still the user's.
    pub fn agent_wants(&self, agent_id: AgentId) -> Option<&Wants> {
        matches!(
            self.attention(agent_id),
            Attention::Pending | Attention::Quiet
        )
        .then(|| self.agent_digest(agent_id)?.wants.as_ref())
        .flatten()
    }
    /// Derived from the digest and the verdict, never stored.
    pub fn attention(&self, agent_id: AgentId) -> Attention {
        match self.agent_digest(agent_id) {
            Some(digest) => attention(digest.attention_facts(), self.agent_verdict(agent_id)),
            None => Attention::Quiet,
        }
    }
    pub fn touch_agent(&mut self, agent_id: AgentId) {
        self.last_active
            .insert(agent_id, rho_core::UnixMs(now_ms()));
    }
    pub fn agent_folded(&self, agent_id: AgentId) -> bool {
        self.agent_summary(agent_id)
            .is_some_and(|agent| agent.hidden)
    }

    pub fn agent_subtree(&self, agent_id: AgentId) -> Vec<AgentId> {
        // Hidden agents are excluded from the result but still walked,
        // so descendants behind a hidden intermediate are found.
        let mut seen = BTreeSet::from([agent_id]);
        let mut queue = vec![agent_id];
        let mut descendants = Vec::new();
        while let Some(cursor) = queue.pop() {
            for child in self.children.get(&cursor).map_or(&[][..], Vec::as_slice) {
                if seen.insert(*child) {
                    queue.push(*child);
                    if !self.agent_summary(*child).is_some_and(|a| a.hidden) {
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
    /// preserves the runtime hierarchy and includes hidden agents so a full
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
            .agent_summary(agent_id)
            .map(|a| a.role.handle_prefix())
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

    pub fn working_directory(&self, agent_id: AgentId) -> Option<Utf8PathBuf> {
        self.agent_summary(agent_id)
            .map(|a| a.workspace.repo().to_owned())
    }
    pub fn agent_workspace(&self, agent_id: AgentId) -> Option<&rho_ui_proto::WorkspaceInfo> {
        self.agent_summary(agent_id).map(|a| &a.workspace)
    }
    pub fn workspace_id_label(&self, agent_id: AgentId) -> Option<String> {
        self.agent_summary(agent_id)
            .and_then(|a| a.workspace.workspace_id())
            .map(|id| format!("ws-{}", id.encoded()))
    }
    pub fn agent_role(&self, agent_id: AgentId) -> Option<rho_ui_proto::AgentRole> {
        self.agent_summary(agent_id).map(|a| a.role)
    }
    pub fn agent_parent(&self, agent_id: AgentId) -> Option<AgentId> {
        self.agent_summary(agent_id)
            .and_then(|agent| agent.parent_agent)
    }
    pub fn agent_hidden(&self, agent_id: AgentId) -> bool {
        self.agent_summary(agent_id)
            .is_some_and(|agent| agent.hidden)
    }
    pub fn agent_pinned(&self, agent_id: AgentId) -> bool {
        self.agent_summary(agent_id)
            .is_some_and(|agent| agent.labels.iter().any(|label| label == "pin"))
    }
    pub fn agent_attention_reason(&self, agent_id: AgentId) -> Option<&str> {
        self.agent_digest(agent_id)
            .and_then(|digest| digest.wants.as_ref())
            .and_then(|wants| wants.summary.as_deref())
            .or_else(|| {
                self.agent_summary(agent_id)
                    .map(|agent| agent.last_user_message_text.as_str())
            })
            .filter(|reason| !reason.trim().is_empty())
    }
    pub fn agent_last_active(&self, agent_id: AgentId) -> Option<rho_core::UnixMs> {
        self.last_active.get(&agent_id).copied()
    }
    /// The chronology, folded from the story rather than sent.
    pub fn agent_facts(&self, agent_id: AgentId) -> AgentFacts {
        let Some(digest) = self.agent_digest(agent_id) else {
            return AgentFacts::default();
        };
        AgentFacts {
            turn_running: digest.turn_running,
            turn_started_at: digest.turn_started_at,
            last_turn_ended: digest.last_turn_ended,
            last_user_message_at: digest.last_user_message_at,
            needs_you_hint: digest
                .wants
                .as_ref()
                .is_some_and(|wants| wants.want == AgentWant::Ask),
            errored: digest.errored.is_some(),
        }
    }
    fn agent_summary(&self, agent_id: AgentId) -> Option<&AgentSummary> {
        self.summaries.get(&agent_id)
    }
    pub fn agent_display_name(&self, agent_id: AgentId) -> Option<&str> {
        self.agent_summary(agent_id)
            .and_then(|a| a.display_name.as_deref())
    }
    pub fn agent_display_label(&self, agent_id: AgentId) -> String {
        let id = self.agent_id_label(agent_id);
        self.agent_display_name(agent_id)
            .filter(|n| !n.trim().is_empty())
            .map_or_else(|| id.clone(), |n| format!("{n} ({id})"))
    }
    /// What the user last said to the agent, for finding it by the words
    /// they remember rather than by a name they never gave it.
    pub fn agent_last_user_message(&self, agent_id: AgentId) -> Option<&str> {
        self.agent_summary(agent_id)
            .map(|agent| agent.last_user_message_text.trim())
            .filter(|text| !text.is_empty())
    }
    pub fn agent_human_name(&self, agent_id: AgentId) -> String {
        let Some(agent) = self.agent_summary(agent_id) else {
            return "Untitled agent".into();
        };
        if let Some(name) = agent
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            return name.into();
        }
        if !agent.last_user_message_text.trim().is_empty() {
            return agent.last_user_message_text.trim().into();
        }
        if agent.role.is_engineer() {
            "Engineer".into()
        } else {
            "Advisor".into()
        }
    }

    pub fn mark_known(&mut self, agent_id: AgentId) {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.agents.entry(agent_id) {
            entry.insert(AgentLife::Known);
        }
    }
    pub fn mark_live(&mut self, agent_id: AgentId) -> bool {
        let previous = self.agents.insert(agent_id, AgentLife::Live);
        previous != Some(AgentLife::Live)
    }
    pub fn mark_not_live(&mut self, agent_id: AgentId) {
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
    use rho_core::UnixMs;
    use rho_ui_proto::AgentIdDomain;
    use rho_ui_proto::mirror::{
        AgentPos, MirrorEvent, RuntimeKind, Seq, SpawnedBy, TurnEdge, TurnOutcome,
    };

    use super::*;

    fn created(at: u64) -> MirrorEvent {
        child_of(None, at)
    }

    fn child_of(parent: Option<AgentId>, at: u64) -> MirrorEvent {
        MirrorEvent::Created {
            role: rho_ui_proto::AgentRole::default(),
            runtime: RuntimeKind::Rho,
            workdirs: vec![rho_ui_proto::WorkspaceInfo::UserCheckout {
                repo: "/repo".into(),
            }],
            spawned_by: SpawnedBy::Direct,
            spawn_name: None,
            parent,
            model: "sol".to_owned(),
            at: UnixMs(at),
        }
    }

    fn agent(nth: u64) -> AgentId {
        AgentId::from_counter(nth, &AgentIdDomain(0)).unwrap()
    }

    fn log(agent_id: AgentId, from: u64, events: Vec<MirrorEvent>) -> Vec<LogEntry> {
        events
            .into_iter()
            .enumerate()
            .map(|(offset, event)| LogEntry {
                seq: Seq(from + offset as u64 + 1),
                agent_id,
                pos: AgentPos(from + offset as u64),
                event,
            })
            .collect()
    }

    /// The desk files every agent in one go, so the registry rebuilds once.
    /// Filed one at a time this was a rebuild of n agents per agent, and it
    /// was 3,120 of 3,395 main-thread samples on a first desk sync.
    #[test]
    fn filing_a_whole_desk_rebuilds_once() {
        let mut registry = AgentMap::default();
        let agents = (1..=32)
            .map(|nth| AgentId::from_counter(nth, &AgentIdDomain(0)).unwrap())
            .collect::<Vec<_>>();
        for agent_id in &agents {
            registry.mark_known(*agent_id);
        }
        let filings = agents
            .iter()
            .map(|agent_id| (*agent_id, false, vec!["rho/agent".to_owned()]))
            .collect::<Vec<_>>();

        assert!(registry.set_agent_filings(filings.clone()));

        // A desk sync that files what is already filed says so, so what is
        // derived from filing is not made again.
        assert!(!registry.set_agent_filings(filings));
    }

    #[test]
    fn a_verdict_said_twice_changes_nothing_the_second_time() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let mut registry = AgentMap::default();

        registry.mark_known(agent_id);
        registry.set_activity(agent_id, "writing tests".to_owned());
        registry.touch_agent(agent_id);
        assert_eq!(registry.attention(agent_id), Attention::Quiet);

        // A turn that ends asking for the user moves attention by itself.
        let host = HostId::default();
        registry.set_host_data(host, 0, 1);
        registry.tell(
            host,
            &log(
                agent_id,
                0,
                vec![
                    created(1),
                    MirrorEvent::Wants {
                        want: AgentWant::Ask,
                        summary: None,
                        at: UnixMs(2),
                    },
                    MirrorEvent::Turn {
                        edge: TurnEdge::Ended(TurnOutcome::Completed),
                        at: UnixMs(3),
                    },
                ],
            ),
        );
        assert_eq!(registry.attention(agent_id), Attention::Pending);

        // Dealing with it is the user's verdict; saying it again is not.
        let handled = Verdict {
            handled_through: AgentPos(3),
            muted: false,
        };
        assert!(registry.set_agent_verdict(agent_id, handled));
        assert_eq!(registry.attention(agent_id), Attention::Quiet);
        assert!(!registry.set_agent_verdict(agent_id, handled));
        assert_eq!(registry.attention(agent_id), Attention::Quiet);
    }

    /// The rails read the log, not the daemon: a turn that starts and
    /// ends asking for something leaves the fold saying exactly that.
    #[test]
    fn the_log_is_what_the_rails_read() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let host = HostId::default();
        let mut registry = AgentMap::default();
        registry.set_host_data(host, 0, 1);
        // A row before the creation says nothing.
        assert!(
            registry
                .tell(
                    host,
                    &log(
                        agent_id,
                        1,
                        vec![MirrorEvent::Turn {
                            edge: TurnEdge::Started,
                            at: UnixMs(11),
                        }]
                    )
                )
                .is_empty()
        );
        assert_eq!(
            registry.tell(
                host,
                &log(
                    agent_id,
                    0,
                    vec![
                        created(1),
                        MirrorEvent::Message {
                            from: None,
                            text: "do the thing\nand then some".to_owned(),
                            delivery: rho_core::MessageDelivery::Immediate,
                            at: UnixMs(10),
                        },
                        MirrorEvent::Turn {
                            edge: TurnEdge::Started,
                            at: UnixMs(11),
                        },
                    ],
                )
            ),
            [agent_id]
        );
        assert!(registry.agent_facts(agent_id).turn_running);
        assert_eq!(
            registry.agent_attention_reason(agent_id),
            Some("do the thing")
        );
        assert_eq!(registry.summaries().len(), 1);
        assert_eq!(registry.host_of_agent(agent_id), Some(host));

        registry.tell(
            host,
            &log(
                agent_id,
                3,
                vec![
                    MirrorEvent::Wants {
                        want: AgentWant::Ask,
                        summary: Some("needs a decision".to_owned()),
                        at: UnixMs(12),
                    },
                    MirrorEvent::Turn {
                        edge: TurnEdge::Ended(TurnOutcome::Completed),
                        at: UnixMs(13),
                    },
                ],
            ),
        );
        let facts = registry.agent_facts(agent_id);
        assert!(!facts.turn_running);
        assert!(facts.needs_you_hint);
        assert_eq!(
            registry.agent_attention_reason(agent_id),
            Some("needs a decision")
        );
        // Replaying a range the fold already holds changes nothing.
        assert!(
            registry
                .tell(
                    host,
                    &log(
                        agent_id,
                        3,
                        vec![MirrorEvent::Wants {
                            want: AgentWant::Ask,
                            summary: Some("needs a decision".to_owned()),
                            at: UnixMs(12),
                        }]
                    )
                )
                .is_empty()
        );
        assert_eq!(registry.agent_digest(agent_id).unwrap().newest, AgentPos(5));

        registry.reset_host(host);
        assert_eq!(registry.summaries().len(), 0);
    }

    /// The order is what the user moves through, and it is handed out
    /// once: an agent that arrives later goes to the front and moves
    /// nobody, and one that arrives again does not move at all. Before the
    /// indexes this was a splice into a vector after a pass over the whole
    /// map, and the positions of everyone already in it shifted.
    #[test]
    fn the_order_puts_the_newest_first_and_leaves_the_rest_where_they_were() {
        let host = HostId::default();
        let mut map = AgentMap::default();
        map.set_host_data(host, 0, 3);
        map.tell(host, &log(agent(1), 0, vec![created(10)]));
        map.tell(host, &log(agent(2), 0, vec![created(20)]));

        // Newest first: agent 2 arrived after agent 1, so it leads.
        assert_eq!(map.next_agent(None, 1), Some(agent(2)));
        assert_eq!(map.next_agent(Some(agent(2)), 1), Some(agent(1)));
        // And the ends wrap, in both directions.
        assert_eq!(map.next_agent(Some(agent(1)), 1), Some(agent(2)));
        assert_eq!(map.next_agent(Some(agent(2)), -1), Some(agent(1)));
        assert_eq!(map.next_agent(None, -1), Some(agent(1)));

        // A third agent takes the front and the other two keep their order.
        map.tell(host, &log(agent(3), 0, vec![created(30)]));
        assert_eq!(map.next_agent(None, 1), Some(agent(3)));
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(2)));
        assert_eq!(map.next_agent(Some(agent(2)), 1), Some(agent(1)));

        // Being told about an agent again is not arriving again.
        map.tell(
            host,
            &log(
                agent(1),
                1,
                vec![MirrorEvent::Turn {
                    edge: TurnEdge::Started,
                    at: UnixMs(40),
                }],
            ),
        );
        assert_eq!(map.next_agent(None, 1), Some(agent(3)));
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(2)));
    }

    /// Filing an agent away takes it out of the way of the keys that move
    /// through the map, and unfiling puts it back where it was rather than
    /// at the end.
    #[test]
    fn a_filed_agent_is_stepped_over_and_comes_back_in_place() {
        let host = HostId::default();
        let mut map = AgentMap::default();
        map.set_host_data(host, 0, 3);
        for (nth, at) in [(1, 10), (2, 20), (3, 30)] {
            map.tell(host, &log(agent(nth), 0, vec![created(at)]));
        }
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(2)));

        assert!(map.set_agent_filings([(agent(2), true, Vec::new())]));
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(1)));
        // The agent the point is in can be one that is filed away; the
        // step out of it starts from the front rather than nowhere.
        assert_eq!(map.next_agent(Some(agent(2)), 1), Some(agent(3)));

        assert!(map.set_agent_filings([(agent(2), false, Vec::new())]));
        assert_eq!(map.next_agent(Some(agent(3)), 1), Some(agent(2)));
    }

    /// The indexes are what the screens read, so they must say the same
    /// thing after a change as they would if they had been made from
    /// scratch: a child reparented, an agent gone, a host reset.
    #[test]
    fn the_indexes_follow_what_changed() {
        let host = HostId::default();
        let mut map = AgentMap::default();
        map.set_host_data(host, 0, 3);
        map.tell(host, &log(agent(1), 0, vec![created(10)]));
        map.tell(host, &log(agent(2), 0, vec![child_of(Some(agent(1)), 20)]));
        map.tell(host, &log(agent(3), 0, vec![child_of(Some(agent(1)), 30)]));

        // Children and subtree members come out in agent-id order, the
        // order the rails have always drawn them in.
        let mut children = vec![agent(2), agent(3)];
        children.sort_unstable();
        assert_eq!(map.agent_children(agent(1)), children);
        assert_eq!(
            map.agent_subtree(agent(1)),
            [vec![agent(1)], children.clone()].concat()
        );
        let mut all = vec![agent(1), agent(2), agent(3)];
        all.sort_unstable();
        assert_eq!(map.host_agents(host), all);
        // The tag index answers the handle the user types.
        let handle = map.agent_id_label(agent(2));
        let (_, tag) = handle.split_once('/').unwrap_or(("", &handle));
        assert_eq!(map.agent_by_tag(host, tag), Some(agent(2)));

        // A host that starts over takes its agents out of every index.
        let departed = map.reset_host(host);
        assert_eq!(departed.into_iter().collect::<Vec<_>>(), all);
        assert_eq!(map.summaries().len(), 0);
        assert!(map.agent_children(agent(1)).is_empty());
        assert_eq!(map.host_agents(host), []);
        assert_eq!(map.agent_by_tag(host, tag), None);
        assert_eq!(map.next_agent(None, 1), None);
    }
}
