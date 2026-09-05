//! Agent lifecycle, selection, naming, and host ownership shared by Rho
//! clients.

pub mod session;
pub mod store;
pub mod story;
pub mod story_view;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use camino::Utf8PathBuf;
use rho_ui_proto::AgentId;
use rho_ui_proto::story::{UiAgentHead, UiStoryEvent, UiStoryPos};

pub use crate::story::{Attention, MirroredAgent, StoryDigest, Wants};

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
        let head = &mirrored.head;
        let digest = &mirrored.digest;
        Self {
            agent_id: head.agent_id,
            parent_agent: head.parent,
            display_name: head.title().map(str::to_owned),
            created_at: head.created_at,
            role: head.role,
            // Every agent is created with at least one workdir, so the
            // fallback is only for a head that arrived malformed.
            workspace: head.workspace().cloned().unwrap_or(
                rho_ui_proto::WorkspaceInfo::UserCheckout {
                    repo: Default::default(),
                },
            ),
            last_active: digest.last_active.max(head.created_at),
            hidden: false,
            last_user_message_text: digest.last_user_message_text.clone(),
            activity: head.activity.clone(),
            labels: Vec::new(),
        }
    }
}

/// Uninterpreted chronology a view may read without folding the story
/// itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentFacts {
    pub turn_running: bool,
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
    attention: BTreeMap<AgentId, Attention>,
    activities: BTreeMap<AgentId, String>,
    /// The mirror: every agent's head and the fold of its story.
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
        self.agents.retain(|id, _| !departed.contains(id));
        self.attention.retain(|id, _| !departed.contains(id));
        self.activities.retain(|id, _| !departed.contains(id));
        self.mirror.retain(|id, _| !departed.contains(id));
        self.filing.retain(|id, _| !departed.contains(id));
        self.last_active.retain(|id, _| !departed.contains(id));
        self.announced_hosts.retain(|id, _| !departed.contains(id));
        self.order.retain(|id| !departed.contains(id));
        if matches!(self.active, ActivePane::Agent(id) if departed.contains(&id)) {
            self.active = ActivePane::Draft;
        }
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

    pub fn set_host_data(
        &mut self,
        host: HostId,
        machine_seed: u64,
        agent_counter: u64,
        heads: Vec<UiAgentHead>,
    ) {
        let live = heads
            .iter()
            .map(|head| head.agent_id)
            .collect::<BTreeSet<_>>();
        self.mirror
            .retain(|agent_id, mirrored| live.contains(agent_id) || mirrored.host != host);
        for head in heads {
            self.set_head(host, head);
        }
        let snapshot = self.hosts.entry(host).or_default();
        snapshot.machine_seed = machine_seed;
        snapshot.agent_counter = agent_counter;
        self.rebuild(Some(host));
        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
    }

    /// One agent's head, from `Ready` or from a later `AgentHead`. The
    /// fold of its story is kept: a head says what the agent is, never
    /// what has happened to it.
    pub fn set_head(&mut self, host: HostId, head: UiAgentHead) {
        let agent_id = head.agent_id;
        match self.mirror.get_mut(&agent_id) {
            Some(mirrored) => {
                mirrored.host = host;
                mirrored.digest.turn_running = head.turn_running;
                mirrored.head = head;
            }
            None => {
                self.mirror.insert(agent_id, MirroredAgent::new(host, head));
            }
        }
        self.agents.entry(agent_id).or_insert(AgentLife::Known);
    }

    /// A run of one agent's story, folded. Positions the client already
    /// holds are skipped, so a repeated range is harmless.
    ///
    /// Returns whether the client is now behind the daemon's own head,
    /// which is how a gap asks to be filled again.
    pub fn tell_story(&mut self, agent_id: AgentId, from: UiStoryPos, events: &[UiStoryEvent]) {
        let Some(mirrored) = self.mirror.get_mut(&agent_id) else {
            return;
        };
        mirrored.tell(from, events);
        let digest = mirrored.digest.clone();
        let host = mirrored.host;
        self.last_active
            .entry(agent_id)
            .and_modify(|active| *active = (*active).max(digest.last_active))
            .or_insert(digest.last_active);
        let _ = host;
        self.rebuild(None);
    }

    /// What this client holds of every agent's story: the version vector
    /// `AgentLogs` sends.
    pub fn known_story_positions(&self, host: HostId) -> Vec<(AgentId, UiStoryPos)> {
        self.mirror
            .values()
            .filter(|mirrored| mirrored.host == host)
            .map(|mirrored| (mirrored.agent_id(), mirrored.digest.newest))
            .collect()
    }

    /// Whether this client's fold has fallen behind the head the daemon
    /// last sent, which is what makes it ask for the gap.
    pub fn story_gaps(&self, host: HostId) -> Vec<(AgentId, UiStoryPos)> {
        self.mirror
            .values()
            .filter(|mirrored| mirrored.host == host)
            .filter(|mirrored| mirrored.digest.newest.0 < mirrored.head.story_pos.0)
            .map(|mirrored| (mirrored.agent_id(), mirrored.digest.newest))
            .collect()
    }

    /// The user's filing of an agent, from the store: hidden, and the
    /// labels they put on it. Nothing on the wire carries either.
    pub fn set_agent_filing(&mut self, agent_id: AgentId, hidden: bool, labels: Vec<String>) {
        let hidden = hidden || labels.iter().any(|label| label == HIDE_LABEL);
        if self.filing.get(&agent_id) == Some(&(hidden, labels.clone())) {
            return;
        }
        self.filing.insert(agent_id, (hidden, labels));
        self.rebuild(None);
        self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
    }

    /// Every agent this client knows, oldest first.
    pub fn summaries(&self) -> &[AgentSummary] {
        &self.summaries
    }

    pub fn agent_digest(&self, agent_id: AgentId) -> Option<&StoryDigest> {
        self.mirror.get(&agent_id).map(|mirrored| &mirrored.digest)
    }

    pub fn agent_head(&self, agent_id: AgentId) -> Option<&UiAgentHead> {
        self.mirror.get(&agent_id).map(|mirrored| &mirrored.head)
    }

    pub fn set_data(&mut self, agents: Vec<UiAgentHead>) {
        let host = HostId::default();
        let (seed, counter) = self
            .hosts
            .get(&host)
            .map(|h| (h.machine_seed, h.agent_counter))
            .unwrap_or_default();
        self.set_host_data(host, seed, counter, agents);
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
        self.attention.retain(|id, _| self.mirror.contains_key(id));
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

    /// What the view derived for this agent. Attention is decided in one
    /// place (`desk_view`) and kept here so every rail agrees.
    pub fn set_attention(&mut self, agent_id: AgentId, attention: Attention) {
        let changed = self.attention(agent_id) != attention;
        self.attention.insert(agent_id, attention);
        if changed {
            self.deal_count_revision = self.deal_count_revision.wrapping_add(1);
        }
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
    pub fn attention(&self, agent_id: AgentId) -> Attention {
        self.attention.get(&agent_id).copied().unwrap_or_default()
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
            last_turn_ended: digest.last_turn_ended,
            last_user_message_at: digest.last_user_message_at,
            needs_you_hint: digest
                .wants
                .as_ref()
                .is_some_and(|wants| wants.want == rho_ui_proto::story::UiAgentWant::Ask),
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
        if agent.role.is_pm() {
            "Project manager".into()
        } else if agent.role.is_engineer() {
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
    use rho_ui_proto::AgentIdDomain;
    use rho_ui_proto::story::{UiAgentHead, UiRuntimeKind, UiSpawnedBy, UiStoryEvent, UiStoryPos};

    use super::*;

    fn head(agent_id: AgentId) -> UiAgentHead {
        UiAgentHead {
            agent_id,
            story_pos: UiStoryPos(0),
            role: rho_ui_proto::AgentRole::PM,
            runtime_kind: UiRuntimeKind::Rho,
            workdirs: vec![rho_ui_proto::WorkspaceInfo::UserCheckout {
                repo: "/repo".into(),
            }],
            spawned_by: UiSpawnedBy::Direct,
            parent: None,
            spawn_name: None,
            generated_title: None,
            activity: None,
            turn_running: false,
            created_at: rho_core::UnixMs(1),
        }
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

        registry.set_attention(agent_id, Attention::Pending);
        let pending_revision = registry.deal_count_revision();
        assert_ne!(pending_revision, known_revision);
        registry.set_attention(agent_id, Attention::Pending);
        assert_eq!(registry.deal_count_revision(), pending_revision);
    }

    /// The rails read the story, not the daemon: a turn that starts and
    /// ends asking for something leaves the fold saying exactly that.
    #[test]
    fn the_story_is_what_the_rails_read() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let mut registry = AgentRegistry::default();
        registry.set_host_data(HostId::default(), 0, 1, vec![head(agent_id)]);

        registry.tell_story(
            agent_id,
            UiStoryPos(0),
            &[
                UiStoryEvent::UserMessage {
                    text: "do the thing\nand then some".to_owned(),
                    at: rho_core::UnixMs(10),
                },
                UiStoryEvent::TurnStarted {
                    at: rho_core::UnixMs(11),
                },
            ],
        );
        assert!(registry.agent_facts(agent_id).turn_running);
        assert_eq!(
            registry.agent_attention_reason(agent_id),
            Some("do the thing")
        );

        registry.tell_story(
            agent_id,
            UiStoryPos(2),
            &[
                UiStoryEvent::Wants {
                    want: rho_ui_proto::story::UiAgentWant::Ask,
                    summary: Some("needs a decision".to_owned()),
                    at: rho_core::UnixMs(12),
                },
                UiStoryEvent::TurnEnded {
                    outcome: rho_ui_proto::story::UiTurnOutcome::Completed,
                    at: rho_core::UnixMs(13),
                },
            ],
        );
        let facts = registry.agent_facts(agent_id);
        assert!(!facts.turn_running);
        assert!(facts.needs_you_hint);
        assert_eq!(
            registry.agent_attention_reason(agent_id),
            Some("needs a decision")
        );
        // Replaying a range the fold already holds changes nothing.
        registry.tell_story(
            agent_id,
            UiStoryPos(2),
            &[UiStoryEvent::Wants {
                want: rho_ui_proto::story::UiAgentWant::Ask,
                summary: Some("needs a decision".to_owned()),
                at: rho_core::UnixMs(12),
            }],
        );
        assert_eq!(
            registry.agent_digest(agent_id).unwrap().newest,
            UiStoryPos(4)
        );
    }
}
