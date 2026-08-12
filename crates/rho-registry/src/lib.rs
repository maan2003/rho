//! Agent lifecycle, selection, naming, and host ownership shared by Rho
//! clients.

pub mod session;
pub mod store;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use camino::Utf8PathBuf;
use rho_ui_proto::{AgentId, UiAgentSummary};

pub fn now_ms() -> u64 {
    #[cfg(not(target_family = "wasm"))]
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(target_family = "wasm")]
    use web_time::{SystemTime, UNIX_EPOCH};
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
const VERDICT_GRACE_MS: u64 = 15_000;
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
    agents: Vec<UiAgentSummary>,
}

type TagAgents = BTreeMap<HostId, BTreeMap<&'static str, Vec<(String, AgentId)>>>;

#[derive(Default)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, AgentLife>,
    attention: BTreeMap<AgentId, rho_ui_proto::UiAttention>,
    pending_verdicts: BTreeMap<AgentId, (rho_ui_proto::UiAttention, rho_core::UnixMs)>,
    activities: BTreeMap<AgentId, String>,
    turn_reports: BTreeMap<AgentId, rho_ui_proto::UiTurnReport>,
    order: Vec<AgentId>,
    last_active: BTreeMap<AgentId, rho_core::UnixMs>,
    hosts: BTreeMap<HostId, HostSnapshot>,
    summaries: Vec<UiAgentSummary>,
    agent_locations: BTreeMap<AgentId, usize>,
    /// Parent → children (in summary order), rebuilt with `summaries`.
    /// `agent_subtree` runs on every dashboard row every frame, so it
    /// must not scan the whole registry per call.
    children: BTreeMap<AgentId, Vec<AgentId>>,
    agent_hosts: BTreeMap<AgentId, HostId>,
    tag_agents: TagAgents,
    announced_hosts: BTreeMap<AgentId, HostId>,
    active: ActivePane,
}

impl AgentRegistry {
    pub fn attach_host(&mut self, host: HostId, name: String) {
        self.hosts.entry(host).or_default().name = name;
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
        self.turn_reports.retain(|id, _| !departed.contains(id));
        self.last_active.retain(|id, _| !departed.contains(id));
        self.announced_hosts.retain(|id, _| !departed.contains(id));
        self.order.retain(|id| !departed.contains(id));
        if matches!(self.active, ActivePane::Agent(id) if departed.contains(&id)) {
            self.active = ActivePane::Draft;
        }
        self.rebuild(None);
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
        mut agents: Vec<UiAgentSummary>,
    ) {
        for agent in &mut agents {
            agent.hidden |= agent.labels.iter().any(|label| label == HIDE_LABEL);
        }
        let snapshot = self.hosts.entry(host).or_default();
        snapshot.machine_seed = machine_seed;
        snapshot.agent_counter = agent_counter;
        snapshot.agents = agents;
        self.rebuild(Some(host));
    }

    pub fn set_data(&mut self, agents: Vec<UiAgentSummary>) {
        let host = HostId::default();
        let (seed, counter) = self
            .hosts
            .get(&host)
            .map(|h| (h.machine_seed, h.agent_counter))
            .unwrap_or_default();
        self.set_host_data(host, seed, counter, agents);
    }

    fn rebuild(&mut self, refreshed: Option<HostId>) {
        self.agent_hosts = self
            .hosts
            .iter()
            .flat_map(|(host, snapshot)| {
                snapshot.agents.iter().map(|agent| (agent.agent_id, *host))
            })
            .collect();
        let from_refreshed =
            |id: &AgentId| refreshed.is_some() && self.agent_hosts.get(id).copied() == refreshed;
        let now = rho_core::UnixMs(now_ms());
        self.pending_verdicts
            .retain(|_, (_, sent)| now.0.saturating_sub(sent.0) < VERDICT_GRACE_MS);
        let awaiting = self
            .pending_verdicts
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        self.attention.retain(|id, _| {
            self.agent_hosts.contains_key(id) && (!from_refreshed(id) || awaiting.contains(id))
        });
        self.activities
            .retain(|id, _| self.agent_hosts.contains_key(id) && !from_refreshed(id));
        self.turn_reports
            .retain(|id, _| self.agent_hosts.contains_key(id) && !from_refreshed(id));

        let mut summaries = Vec::new();
        let mut unseen = Vec::new();
        for snapshot in self.hosts.values() {
            for agent in &snapshot.agents {
                self.agents
                    .entry(agent.agent_id)
                    .or_insert(AgentLife::Known);
                self.attention
                    .entry(agent.agent_id)
                    .or_insert(agent.attention);
                if let Some(activity) = &agent.activity {
                    self.activities
                        .entry(agent.agent_id)
                        .or_insert_with(|| activity.clone());
                }
                if let Some(report) = &agent.turn_report {
                    self.turn_reports
                        .entry(agent.agent_id)
                        .or_insert_with(|| report.clone());
                }
                let active = self
                    .last_active
                    .entry(agent.agent_id)
                    .or_insert(rho_core::UnixMs(0));
                *active = (*active).max(agent.last_active);
                if !self.order.contains(&agent.agent_id) {
                    unseen.push((agent.last_active, agent.agent_id));
                }
            }
            summaries.extend(snapshot.agents.iter().cloned());
        }
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

    pub fn set_attention(&mut self, agent_id: AgentId, attention: rho_ui_proto::UiAttention) {
        self.pending_verdicts.remove(&agent_id);
        self.attention.insert(agent_id, attention);
    }
    pub fn expect_attention(&mut self, agent_id: AgentId, attention: rho_ui_proto::UiAttention) {
        self.pending_verdicts
            .insert(agent_id, (attention, rho_core::UnixMs(now_ms())));
        self.attention.insert(agent_id, attention);
    }
    pub fn set_activity(&mut self, agent_id: AgentId, activity: String) {
        self.activities.insert(agent_id, activity);
    }
    pub fn agent_activity(&self, agent_id: AgentId) -> Option<&str> {
        self.activities.get(&agent_id).map(String::as_str)
    }
    pub fn set_turn_report(&mut self, agent_id: AgentId, report: rho_ui_proto::UiTurnReport) {
        self.turn_reports.insert(agent_id, report);
    }
    pub fn agent_turn_report(&self, agent_id: AgentId) -> Option<&rho_ui_proto::UiTurnReport> {
        matches!(
            self.attention(agent_id),
            rho_ui_proto::UiAttention::Pending | rho_ui_proto::UiAttention::Quiet
        )
        .then(|| self.turn_reports.get(&agent_id))
        .flatten()
    }
    pub fn attention(&self, agent_id: AgentId) -> rho_ui_proto::UiAttention {
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
            .filter(|(_, a)| *a >= rho_ui_proto::UiAttention::Pending)
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
    pub fn agent_disposition(&self, agent_id: AgentId) -> Option<rho_ui_proto::AgentDisposition> {
        self.agent_summary(agent_id).map(|agent| agent.disposition)
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
        self.turn_reports
            .get(&agent_id)
            .map(|report| report.summary.as_str())
            .or_else(|| {
                self.agent_summary(agent_id)
                    .map(|agent| agent.last_user_message_text.as_str())
            })
            .filter(|reason| !reason.trim().is_empty())
    }
    pub fn agent_last_active(&self, agent_id: AgentId) -> Option<rho_core::UnixMs> {
        self.last_active.get(&agent_id).copied()
    }
    fn agent_summary(&self, agent_id: AgentId) -> Option<&UiAgentSummary> {
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
        self.agents.entry(agent_id).or_insert(AgentLife::Known);
    }
    pub fn mark_live(&mut self, agent_id: AgentId) -> bool {
        self.agents.insert(agent_id, AgentLife::Live) != Some(AgentLife::Live)
    }
    pub fn mark_not_live(&mut self, agent_id: AgentId) {
        self.agents.insert(agent_id, AgentLife::Known);
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
}
