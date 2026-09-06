//! Agent lifecycle, selection, naming, and host ownership shared by Rho
//! clients.

pub mod fold;
pub mod render;
pub mod session;
pub mod store;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use camino::Utf8PathBuf;
use rho_ui_proto::AgentId;
use rho_ui_proto::mirror::AgentWant;
#[cfg(test)]
use rho_ui_proto::mirror::LogEntry;

pub use crate::fold::{
    AgentIdentity, Attention, AttentionFacts, DIGEST_VERSION, Digest, MirroredAgent,
    TranscriptFold, Verdict, Wants, attention, one_line, transcript,
};

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostId(pub u32);

impl fmt::Display for HostId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "host{}", self.0)
    }
}

pub const HIDE_LABEL: &str = "hide";
const LABEL_HEADROOM: u64 = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentLife {
    Known,
    Live,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActivePane {
    #[default]
    Startup,
    Draft,
    Agent(AgentId),
}

#[derive(Default)]
struct HostSnapshot {
    name: String,
    machine_seed: u64,
    agent_counter: u64,
    agents: Vec<AgentSummary>,
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

#[derive(Default)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, AgentLife>,
    /// What the view derived, pushed back in so every rail reads one
    /// answer rather than each deriving its own.
    /// What the user said about each agent; attention is derived from
    /// this and the digest, never stored.
    verdicts: BTreeMap<AgentId, Verdict>,
    activities: BTreeMap<AgentId, String>,
    /// The mirror, folded: what every agent is and what happened to it.
    mirror: BTreeMap<AgentId, MirroredAgent>,
    /// The user's filing, from the store: hidden agents and their labels.
    filing: BTreeMap<AgentId, (bool, Vec<String>)>,
    order: Vec<AgentId>,
    last_active: BTreeMap<AgentId, rho_core::UnixMs>,
    hosts: BTreeMap<HostId, HostSnapshot>,
    summaries: Vec<AgentSummary>,
    agent_locations: BTreeMap<AgentId, usize>,
    /// Parent → children (in summary order), rebuilt with `summaries`.
    /// `agent_subtree` runs on every dashboard row every frame, so it
    /// must not scan the whole registry per call.
    children: BTreeMap<AgentId, Vec<AgentId>>,
    agent_hosts: BTreeMap<AgentId, HostId>,
    tag_agents: TagAgents,
    announced_hosts: BTreeMap<AgentId, HostId>,
    active: ActivePane,
    deal_count_revision: u64,
}

impl AgentRegistry {
    pub fn attach_host(&mut self, host: HostId, name: String) {
        self.hosts.entry(host).or_default().name = name;
        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
    }

    pub fn detach_host(&mut self, host: HostId) {
        let Some(snapshot) = self.hosts.remove(&host) else {
            return;
        };
        let departed = snapshot
            .agents
            .iter()
            .map(|a| a.agent_id)
            .chain(
                self.announced_hosts
                    .iter()
                    .filter(|(_, owner)| **owner == host)
                    .map(|(id, _)| *id),
            )
            .collect::<BTreeSet<_>>();
        self.forget_agents(&departed);
        self.rebuild(None);
        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
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
        self.rebuild(Some(host));
        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
    }

    /// Drops everything mirrored from a host that is still attached: for
    /// a daemon whose database is not the one this client mirrored.
    pub fn reset_host(&mut self, host: HostId) {
        let departed = self
            .mirror
            .values()
            .filter(|mirrored| mirrored.host == host)
            .map(MirroredAgent::agent_id)
            .collect::<BTreeSet<_>>();
        self.forget_agents(&departed);
        self.rebuild(None);
        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
    }

    fn forget_agents(&mut self, departed: &BTreeSet<AgentId>) {
        self.agents.retain(|id, _| !departed.contains(id));
        self.verdicts.retain(|id, _| !departed.contains(id));
        self.activities.retain(|id, _| !departed.contains(id));
        self.mirror.retain(|id, _| !departed.contains(id));
        self.filing.retain(|id, _| !departed.contains(id));
        self.last_active.retain(|id, _| !departed.contains(id));
        self.announced_hosts.retain(|id, _| !departed.contains(id));
        self.order.retain(|id| !departed.contains(id));
        if matches!(self.active, ActivePane::Agent(id) if departed.contains(&id)) {
            self.active = ActivePane::Draft;
        }
    }

    /// A run of a host's log, folded. The client's own fold runs on the
    /// model thread now; this is what the registry's tests fold with, and
    /// what `told` is checked against.
    #[cfg(test)]
    pub fn tell(&mut self, host: HostId, entries: &[LogEntry]) -> Vec<AgentId> {
        let mut changed = Vec::new();
        let mut attention_before = BTreeMap::new();
        for entry in entries {
            attention_before
                .entry(entry.agent_id)
                .or_insert_with(|| self.attention(entry.agent_id));
            let told = match self.mirror.get_mut(&entry.agent_id) {
                Some(mirrored) => mirrored.tell(entry.pos, &entry.event),
                None => match MirroredAgent::new(host, entry.agent_id, &entry.event) {
                    Some(mirrored) => {
                        self.mirror.insert(entry.agent_id, mirrored);
                        self.agents
                            .entry(entry.agent_id)
                            .or_insert(AgentLife::Known);
                        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
                        true
                    }
                    None => false,
                },
            };
            if told && !changed.contains(&entry.agent_id) {
                changed.push(entry.agent_id);
            }
        }
        if !changed.is_empty() {
            self.rebuild(None);
        }
        // A row can move attention on its own (a turn ends asking for
        // the user); the dealer reads the revision to know.
        if attention_before
            .iter()
            .any(|(agent_id, before)| self.attention(*agent_id) != *before)
        {
            self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
        }
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
        let mut attention_before = BTreeMap::new();
        for mirrored in agents {
            let agent_id = mirrored.agent_id();
            attention_before
                .entry(agent_id)
                .or_insert_with(|| self.attention(agent_id));
            if self.mirror.insert(agent_id, mirrored).is_none() {
                self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
            }
            self.agents.entry(agent_id).or_insert(AgentLife::Known);
            changed.push(agent_id);
        }
        self.rebuild(None);
        // A row can move attention on its own (a turn ends asking for
        // the user); the dealer reads the revision to know.
        if attention_before
            .iter()
            .any(|(agent_id, before)| self.attention(*agent_id) != *before)
        {
            self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
        }
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
        let mut moved = false;
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
            moved = true;
        }
        if !moved {
            return false;
        }
        self.rebuild(None);
        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
        true
    }

    /// Every agent this client knows, oldest first.
    pub fn summaries(&self) -> &[AgentSummary] {
        &self.summaries
    }

    /// Agents read back from the client's own copy, digest and all, so
    /// nothing has to be folded again. Rows that come after fold on top.
    pub fn restore(&mut self, agents: Vec<MirroredAgent>) {
        if agents.is_empty() {
            return;
        }
        for mirrored in agents {
            let agent_id = mirrored.agent_id();
            self.mirror.insert(agent_id, mirrored);
            self.agents.entry(agent_id).or_insert(AgentLife::Known);
        }
        self.rebuild(None);
        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
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

    /// Every agent mirrored from a host.
    pub fn host_agents(&self, host: HostId) -> Vec<AgentId> {
        self.mirror
            .values()
            .filter(|mirrored| mirrored.host == host)
            .map(MirroredAgent::agent_id)
            .collect()
    }

    fn rebuild(&mut self, refreshed: Option<HostId>) {
        let _ = refreshed;
        for snapshot in self.hosts.values_mut() {
            snapshot.agents.clear();
        }
        let mut summaries = Vec::new();
        let mut unseen = Vec::new();
        for mirrored in self.mirror.values() {
            let mut summary = AgentSummary::of(mirrored);
            if let Some((hidden, labels)) = self.filing.get(&summary.agent_id) {
                summary.hidden = *hidden;
                summary.labels = labels.clone();
            }
            self.agents
                .entry(summary.agent_id)
                .or_insert(AgentLife::Known);
            if let Some(activity) = &summary.activity {
                self.activities.insert(summary.agent_id, activity.clone());
            } else {
                self.activities.remove(&summary.agent_id);
            }
            let active = self
                .last_active
                .entry(summary.agent_id)
                .or_insert(rho_core::UnixMs(0));
            *active = (*active).max(summary.last_active);
            if !self.order.contains(&summary.agent_id) {
                unseen.push((summary.last_active, summary.agent_id));
            }
            if let Some(snapshot) = self.hosts.get_mut(&mirrored.host) {
                snapshot.agents.push(summary.clone());
            }
            summaries.push(summary);
        }
        self.verdicts.retain(|id, _| self.mirror.contains_key(id));
        self.agent_hosts = self
            .mirror
            .iter()
            .map(|(agent_id, mirrored)| (*agent_id, mirrored.host))
            .collect();
        unseen.sort_by_key(|(active, id)| (Reverse(*active), *id));
        self.order
            .splice(0..0, unseen.into_iter().map(|(_, id)| id));
        self.order.retain(|id| self.agents.contains_key(id));
        self.agent_locations = summaries
            .iter()
            .enumerate()
            .map(|(i, agent)| (agent.agent_id, i))
            .collect();
        self.children = BTreeMap::new();
        for agent in &summaries {
            if let Some(parent) = agent.parent_agent {
                self.children
                    .entry(parent)
                    .or_default()
                    .push(agent.agent_id);
            }
        }
        self.tag_agents = BTreeMap::new();
        for (agent_id, location) in &self.agent_locations {
            let agent = &summaries[*location];
            let Some(host) = self.agent_hosts.get(agent_id) else {
                continue;
            };
            self.tag_agents
                .entry(*host)
                .or_default()
                .entry(agent.role.handle_prefix())
                .or_default()
                .push((agent_id.encoded(), *agent_id));
        }
        for roles in self.tag_agents.values_mut() {
            for agents in roles.values_mut() {
                agents.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            }
        }
        self.summaries = summaries;
    }

    /// What the user last said about this agent. Returns whether it is
    /// new, so the caller writes it down only then.
    pub fn set_agent_verdict(&mut self, agent_id: AgentId, verdict: Verdict) -> bool {
        if self.verdicts.get(&agent_id) == Some(&verdict) {
            return false;
        }
        let before = self.attention(agent_id);
        self.verdicts.insert(agent_id, verdict);
        if self.attention(agent_id) != before {
            self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
        }
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

    pub fn next_attention_agent(&self) -> Option<AgentId> {
        let selected = self.selected_agent().copied();
        self.order
            .iter()
            .copied()
            .filter(|id| Some(*id) != selected && !self.agent_folded(*id))
            .map(|id| (id, self.attention(id)))
            .filter(|(_, a)| *a >= Attention::Pending)
            .min_by_key(|(_, a)| Reverse(*a))
            .map(|(id, _)| id)
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
        // Callers see members in summary order, as before.
        descendants.sort_by_key(|id| self.agent_locations.get(id).copied());
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
        self.agent_locations
            .get(&agent_id)
            .and_then(|i| self.summaries.get(*i))
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
            self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
        }
    }
    pub fn mark_live(&mut self, agent_id: AgentId) -> bool {
        let previous = self.agents.insert(agent_id, AgentLife::Live);
        if previous.is_none() {
            self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
        }
        previous != Some(AgentLife::Live)
    }
    pub fn mark_not_live(&mut self, agent_id: AgentId) {
        if self.agents.insert(agent_id, AgentLife::Known).is_none() {
            self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
        }
    }
    pub fn active_pane(&self) -> ActivePane {
        self.active
    }
    pub fn selected_agent(&self) -> Option<&AgentId> {
        if let ActivePane::Agent(id) = &self.active {
            Some(id)
        } else {
            None
        }
    }
    pub fn select_agent(&mut self, agent_id: AgentId) {
        self.active = ActivePane::Agent(agent_id);
    }
    pub fn enter_draft(&mut self) {
        self.active = ActivePane::Draft;
    }
    pub fn next_agent(&self, delta: isize) -> Option<AgentId> {
        let visible = self
            .order
            .iter()
            .copied()
            .filter(|id| !self.agent_folded(*id))
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return None;
        }
        let index = self
            .selected_agent()
            .and_then(|selected| visible.iter().position(|id| id == selected))
            .map(|i| (i as isize + delta).rem_euclid(visible.len() as isize) as usize)
            .unwrap_or_else(|| if delta < 0 { visible.len() - 1 } else { 0 });
        visible.get(index).copied()
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

    /// Changes only when inputs that can affect the Desk deal count change.
    /// Activity text and last-active timestamps affect presentation/order, not
    /// the number of cards, so streaming those fields leaves this stable.
    pub fn deal_count_revision(&self) -> u64 {
        self.deal_count_revision
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
        MirrorEvent::Created {
            role: rho_ui_proto::AgentRole::default(),
            runtime: RuntimeKind::Rho,
            workdirs: vec![rho_ui_proto::WorkspaceInfo::UserCheckout {
                repo: "/repo".into(),
            }],
            spawned_by: SpawnedBy::Direct,
            spawn_name: None,
            parent: None,
            model: "sol".to_owned(),
            at: UnixMs(at),
        }
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
        let mut registry = AgentRegistry::default();
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

        let before = registry.deal_count_revision();
        assert!(registry.set_agent_filings(filings.clone()));
        assert_eq!(
            registry.deal_count_revision(),
            before.wrapping_add(1),
            "filing the desk rebuilt more than once"
        );

        // A desk sync that files what is already filed says so, so what is
        // derived from filing is not made again.
        let after = registry.deal_count_revision();
        assert!(!registry.set_agent_filings(filings));
        assert_eq!(registry.deal_count_revision(), after);
    }

    #[test]
    fn deal_count_revision_ignores_presentation_only_updates() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let mut registry = AgentRegistry::default();

        registry.mark_known(agent_id);
        let known_revision = registry.deal_count_revision();
        registry.set_activity(agent_id, "writing tests".to_owned());
        registry.touch_agent(agent_id);
        assert_eq!(registry.deal_count_revision(), known_revision);

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
        let pending_revision = registry.deal_count_revision();
        assert_ne!(pending_revision, known_revision);

        // Dealing with it is the user's verdict; saying it again is not.
        let handled = Verdict {
            handled_through: AgentPos(3),
            muted: false,
        };
        assert!(registry.set_agent_verdict(agent_id, handled));
        assert_eq!(registry.attention(agent_id), Attention::Quiet);
        let handled_revision = registry.deal_count_revision();
        assert_ne!(handled_revision, pending_revision);
        assert!(!registry.set_agent_verdict(agent_id, handled));
        assert_eq!(registry.deal_count_revision(), handled_revision);
    }

    /// The rails read the log, not the daemon: a turn that starts and
    /// ends asking for something leaves the fold saying exactly that.
    #[test]
    fn the_log_is_what_the_rails_read() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let host = HostId::default();
        let mut registry = AgentRegistry::default();
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
        assert!(registry.summaries().is_empty());
    }
}
