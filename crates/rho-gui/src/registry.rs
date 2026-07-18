//! Agent lifecycle and selection.
//!
//! One explicit state machine instead of parallel sets: an agent is *known*
//! (appears in summaries or was announced), and optionally *live* (this
//! connection has received frames for it). Selection and next/previous
//! cycling operate over live agents only.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8PathBuf;
use rho_ui_proto::{AgentId, TagId, TagKind, UiAgentSummary, UiTag};

/// A workstream tag with its member agents resolved: the unit the rail rows
/// and per-task pane contexts are built around.
#[derive(Clone)]
pub struct Workstream {
    pub tag_id: TagId,
    pub name: String,
    pub status: rho_ui_proto::Status,
    pub hidden: bool,
    /// The workstream-group tag this stream sits under, from the tag's
    /// parent chain.
    pub group: Option<TagId>,
    pub agents: Vec<UiAgentSummary>,
}

impl Workstream {
    pub fn agent_ids(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.agents.iter().map(|agent| agent.agent_id)
    }
}

fn derive_workstreams(tags: &[UiTag], agents: &[UiAgentSummary]) -> Vec<Workstream> {
    tags.iter()
        .filter(|tag| tag.kind == TagKind::Workstream)
        .map(|tag| Workstream {
            tag_id: tag.tag_id,
            name: tag.name.clone(),
            status: tag.status,
            hidden: tag.hidden,
            group: tag.parent,
            agents: agents
                .iter()
                .filter(|agent| agent.tags.contains(&tag.tag_id))
                .cloned()
                .collect(),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentLife {
    Known,
    Live,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActivePane {
    /// Initial bootstrapping state: the draft surface is visible, but the
    /// first live agent may claim the view once daemon frames arrive.
    #[default]
    Startup,
    /// The user intentionally opened the new-agent compose surface.
    Draft,
    Agent(AgentId),
}

#[derive(Default)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, AgentLife>,
    /// Live attention overlay: broadcasts land here between `Ready`
    /// refreshes, which re-seed it from the summaries' snapshot.
    attention: BTreeMap<AgentId, rho_ui_proto::UiAttention>,
    /// Retained top-to-bottom rail order. Bucket changes are applied with a
    /// stable sort, so agents only move when their coarse rail bucket changes
    /// or when they are first seen.
    rail_order: Vec<AgentId>,
    /// Cached positions in `rail_order`, avoiding a linear scan for every row
    /// while constructing the rail.
    rail_ranks: BTreeMap<AgentId, usize>,
    /// Derived row membership and ordering, rebuilt only when rail-relevant
    /// state changes rather than on every window draw.
    topic_rail_layouts: BTreeMap<TagId, TopicRailLayout>,
    /// When the user last engaged each agent: seeded from summaries, bumped
    /// locally on send. Selects quiet agents for the active rail bucket.
    last_active: BTreeMap<AgentId, rho_core::UnixMs>,
    /// The latest tag snapshot from `Ready`, all kinds.
    tags: Vec<UiTag>,
    /// The latest agent summaries from `Ready`.
    summaries: Vec<UiAgentSummary>,
    /// Workstream tags joined with their member agents, derived from the two
    /// snapshots above.
    workstreams: Vec<Workstream>,
    /// Positions in `summaries`, used by all summary lookups.
    agent_locations: BTreeMap<AgentId, usize>,
    active: ActivePane,
    /// The daemon database's machine seed, from `Ready`; kept for consumers
    /// that resolve ids.
    machine_seed: u64,
    /// Last agent id counter, from `Ready`; keys uniform agent label prefix
    /// length.
    agent_counter: u64,
    /// Last workspace id counter, from `Ready`; keys uniform workspace label
    /// prefix length just like the generated-agent population does for agents.
    workspace_counter: u64,
}

#[derive(Default)]
struct TopicRailLayout {
    listed: Vec<(AgentId, usize)>,
    folded: Vec<(AgentId, usize)>,
}

struct TopicRailState<'a> {
    by_id: BTreeMap<AgentId, &'a UiAgentSummary>,
    root_by_id: BTreeMap<AgentId, AgentId>,
    selected_root: Option<AgentId>,
    top_roots: BTreeSet<AgentId>,
    extra_roots: BTreeSet<AgentId>,
}

impl<'a> TopicRailState<'a> {
    fn new(registry: &AgentRegistry, topic: &'a Workstream) -> Self {
        let by_id = topic
            .agents
            .iter()
            .map(|agent| (agent.agent_id, agent))
            .collect::<BTreeMap<_, _>>();
        let root_by_id = topic
            .agents
            .iter()
            .map(|agent| {
                let mut root = agent.agent_id;
                let mut seen = BTreeSet::new();
                while seen.insert(root) {
                    let Some(parent) = by_id.get(&root).and_then(|agent| agent.parent_agent) else {
                        break;
                    };
                    if !by_id.contains_key(&parent) {
                        break;
                    }
                    root = parent;
                }
                (agent.agent_id, root)
            })
            .collect::<BTreeMap<_, _>>();
        let selected_root = registry
            .selected_agent()
            .and_then(|selected| root_by_id.get(selected))
            .copied();
        let roots = topic
            .agents
            .iter()
            .filter(|agent| {
                agent
                    .parent_agent
                    .is_none_or(|parent| !by_id.contains_key(&parent))
            })
            .collect::<Vec<_>>();
        let top_roots = registry.top_bucket(roots.iter().copied());
        let mut extra = roots
            .into_iter()
            .filter(|agent| {
                agent.status != rho_ui_proto::Status::Pinned
                    && !agent.hidden
                    && !top_roots.contains(&agent.agent_id)
            })
            .collect::<Vec<_>>();
        extra.sort_by_key(|agent| {
            registry
                .rail_ranks
                .get(&agent.agent_id)
                .copied()
                .unwrap_or(usize::MAX)
        });
        let extra_roots = extra
            .into_iter()
            .take(5)
            .map(|agent| agent.agent_id)
            .collect();

        Self {
            by_id,
            root_by_id,
            selected_root,
            top_roots,
            extra_roots,
        }
    }

    fn auto_collapsed(&self, agent_id: AgentId) -> bool {
        let Some(root) = self.root_by_id.get(&agent_id).copied() else {
            return false;
        };
        root != agent_id && self.selected_root != Some(root)
    }

    fn folded(&self, agent_id: AgentId) -> bool {
        let Some(root_id) = self.root_by_id.get(&agent_id).copied() else {
            return false;
        };
        if self.auto_collapsed(agent_id) {
            return true;
        }

        let mut cursor = agent_id;
        let mut seen = BTreeSet::new();
        while seen.insert(cursor) {
            let Some(agent) = self.by_id.get(&cursor) else {
                break;
            };
            if agent.hidden {
                return true;
            }
            let Some(parent) = agent
                .parent_agent
                .filter(|parent| self.by_id.contains_key(parent))
            else {
                break;
            };
            cursor = parent;
        }

        let root = self.by_id[&root_id];
        if root.status == rho_ui_proto::Status::Pinned || self.selected_root == Some(root_id) {
            return false;
        }
        !self.top_roots.contains(&root_id) && !self.extra_roots.contains(&root_id)
    }
}

impl AgentRegistry {
    pub fn set_machine_seed(&mut self, machine_seed: u64) {
        self.machine_seed = machine_seed;
    }

    pub fn set_agent_counter(&mut self, agent_counter: u64) {
        self.agent_counter = agent_counter;
    }

    pub fn set_workspace_counter(&mut self, workspace_counter: u64) {
        self.workspace_counter = workspace_counter;
    }

    pub fn set_data(&mut self, tags: Vec<UiTag>, agents: Vec<UiAgentSummary>) {
        self.attention.clear();
        let mut unseen = Vec::new();
        for agent in &agents {
            self.agents
                .entry(agent.agent_id)
                .or_insert(AgentLife::Known);
            self.attention.insert(agent.agent_id, agent.attention);
            // Keep the freshest engagement signal: a local send can be
            // newer than the summary's persisted timestamp.
            let last_active = self
                .last_active
                .entry(agent.agent_id)
                .or_insert(rho_core::UnixMs(0));
            *last_active = (*last_active).max(agent.last_active);
            if !self.rail_ranks.contains_key(&agent.agent_id) {
                unseen.push((agent.last_active, agent.agent_id));
            }
        }
        // First-seen agents enter above the retained order, seeded by
        // engagement recency; already-placed agents keep their relative
        // position across refreshes.
        unseen.sort_by_key(|(last_active, agent_id)| (Reverse(*last_active), *agent_id));
        self.rail_order
            .splice(0..0, unseen.into_iter().map(|(_, agent_id)| agent_id));
        self.rail_ranks = self
            .rail_order
            .iter()
            .copied()
            .enumerate()
            .map(|(rank, agent_id)| (agent_id, rank))
            .collect();
        self.agent_locations = agents
            .iter()
            .enumerate()
            .map(|(index, agent)| (agent.agent_id, index))
            .collect();
        self.workstreams = derive_workstreams(&tags, &agents);
        self.tags = tags;
        self.summaries = agents;
        self.rebuild_topic_rail_layouts();
    }

    pub fn set_attention(&mut self, agent_id: AgentId, attention: rho_ui_proto::UiAttention) {
        if self.attention.insert(agent_id, attention) != Some(attention) {
            self.rebuild_topic_rail_layouts();
        }
    }

    pub fn attention(&self, agent_id: AgentId) -> rho_ui_proto::UiAttention {
        self.attention.get(&agent_id).copied().unwrap_or_default()
    }

    /// The user engaged this agent right now (sent it a message).
    pub fn touch_agent(&mut self, agent_id: AgentId) {
        self.last_active
            .insert(agent_id, rho_core::UnixMs(crate::workspace::now_ms()));
        self.rebuild_topic_rail_layouts();
    }

    /// Folded under the workstream's collapsed tail instead of listed.
    /// Explicitly hidden agents always fold; otherwise the rail shows pinned
    /// agents, the active bucket, five more agents from the quiet tail, and
    /// the current selection.
    pub fn agent_folded(&self, agent_id: AgentId) -> bool {
        let Some(workstream) = self
            .workstreams
            .iter()
            .find(|workstream| workstream.agent_ids().any(|id| id == agent_id))
        else {
            return false;
        };
        self.topic_rail_layouts
            .get(&workstream.tag_id)
            .is_some_and(|layout| layout.listed.iter().all(|(id, _)| *id != agent_id))
    }

    /// The rail-visible agent most in need of the user, excluding the one
    /// already on screen: highest attention wins, rail order breaks ties.
    /// Only Pending and above count — jumping to a quiet or merely working
    /// agent would be noise.
    pub fn next_attention_agent(&self) -> Option<AgentId> {
        let selected = self.selected_agent().copied();
        self.rail_order()
            .into_iter()
            .filter(|agent_id| Some(*agent_id) != selected && !self.agent_folded(*agent_id))
            .map(|agent_id| (agent_id, self.attention(agent_id)))
            .filter(|(_, attention)| *attention >= rho_ui_proto::UiAttention::Pending)
            .min_by_key(|(_, attention)| Reverse(*attention))
            .map(|(agent_id, _)| agent_id)
    }

    /// Where a new agent should work: the newest agent in the workstream sets
    /// the precedent, since sibling agents usually share a project.
    pub fn last_working_directory(&self, tag_id: TagId) -> Option<Utf8PathBuf> {
        self.workstreams
            .iter()
            .find(|workstream| workstream.tag_id == tag_id)?
            .agents
            .last()
            .map(|agent| agent.workspace.repo().to_owned())
    }

    /// The workstream tag an agent currently carries.
    pub fn workstream_of(&self, agent_id: AgentId) -> Option<TagId> {
        let agent = self.agent_summary(agent_id)?;
        agent.tags.iter().copied().find(|tag_id| {
            self.tags
                .iter()
                .any(|tag| tag.tag_id == *tag_id && tag.kind == TagKind::Workstream)
        })
    }

    /// The role-prefixed short display label, unique among generated IDs.
    pub fn agent_id_label(&self, agent_id: AgentId) -> String {
        let prefix_len = prefix_id::uniform_prefix_len(self.agent_counter, LABEL_HEADROOM).max(4);
        let prefix = self
            .agent_summary(agent_id)
            .map(|agent| agent.role.handle_prefix())
            .unwrap_or("eng");
        format!("{prefix}-{}", &agent_id.encoded()[..prefix_len])
    }

    pub fn working_directory(&self, agent_id: AgentId) -> Option<Utf8PathBuf> {
        self.agent_summary(agent_id)
            .map(|agent| agent.workspace.repo().to_owned())
    }

    pub fn agent_workspace(&self, agent_id: AgentId) -> Option<&rho_ui_proto::WorkspaceInfo> {
        self.agent_summary(agent_id).map(|agent| &agent.workspace)
    }

    pub fn workspace_id_label(&self, agent_id: AgentId) -> Option<String> {
        let workspace_id = self
            .agent_summary(agent_id)
            .and_then(|agent| agent.workspace.workspace_id())?;
        let prefix_len =
            prefix_id::uniform_prefix_len(self.workspace_counter, LABEL_HEADROOM).max(2);
        Some(format!("ws-{}", &workspace_id.encoded()[..prefix_len]))
    }

    pub fn agent_role(&self, agent_id: AgentId) -> Option<rho_ui_proto::AgentRole> {
        self.agent_summary(agent_id).map(|agent| agent.role)
    }

    /// The pin status of an agent, from summaries.
    pub fn agent_status(&self, agent_id: AgentId) -> rho_ui_proto::Status {
        self.agent_summary(agent_id)
            .map(|agent| agent.status)
            .unwrap_or(rho_ui_proto::Status::Normal)
    }

    fn agent_summary(&self, agent_id: AgentId) -> Option<&UiAgentSummary> {
        let index = self.agent_locations.get(&agent_id)?;
        self.summaries.get(*index)
    }

    pub fn agent_display_name(&self, agent_id: AgentId) -> Option<&str> {
        self.agent_summary(agent_id)
            .and_then(|agent| agent.display_name.as_deref())
    }

    pub fn agent_display_label(&self, agent_id: AgentId) -> String {
        let id_label = self.agent_id_label(agent_id);
        match self.agent_display_name(agent_id) {
            Some(name) if !name.trim().is_empty() => format!("{name} ({id_label})"),
            _ => id_label,
        }
    }

    pub fn add_tag(&mut self, tag: UiTag) {
        // Tags stay in the daemon's creation order; a new tag is the newest,
        // so it belongs at the end.
        let mut tags = std::mem::take(&mut self.tags);
        tags.retain(|existing| existing.tag_id != tag.tag_id);
        tags.push(tag);
        let agents = std::mem::take(&mut self.summaries);
        self.set_data(tags, agents);
    }

    pub fn tags(&self) -> &[UiTag] {
        &self.tags
    }

    pub fn workstreams(&self) -> &[Workstream] {
        &self.workstreams
    }

    /// Workstream-group tags, in daemon creation order.
    pub fn group_tags(&self) -> impl Iterator<Item = &UiTag> {
        self.tags
            .iter()
            .filter(|tag| tag.kind == TagKind::WorkstreamGroup)
    }

    pub fn mark_known(&mut self, agent_id: AgentId) {
        self.agents.entry(agent_id).or_insert(AgentLife::Known);
    }

    pub fn mark_live(&mut self, agent_id: AgentId) -> bool {
        self.agents.insert(agent_id, AgentLife::Live) != Some(AgentLife::Live)
    }

    pub fn is_live(&self, agent_id: AgentId) -> bool {
        self.agents.get(&agent_id) == Some(&AgentLife::Live)
    }

    pub fn active_pane(&self) -> ActivePane {
        self.active
    }

    pub fn selected_agent(&self) -> Option<&AgentId> {
        match &self.active {
            ActivePane::Agent(agent_id) => Some(agent_id),
            ActivePane::Startup | ActivePane::Draft => None,
        }
    }

    pub fn select_agent(&mut self, agent_id: AgentId) {
        let active = ActivePane::Agent(agent_id);
        if self.active != active {
            self.active = active;
            self.rebuild_topic_rail_layouts();
        }
    }

    pub fn enter_draft(&mut self) {
        if self.active != ActivePane::Draft {
            self.active = ActivePane::Draft;
            self.rebuild_topic_rail_layouts();
        }
    }

    /// Cycles through live, rail-visible agents by `delta`, starting from
    /// the current selection. Cycling follows rail order (workstreams, then
    /// agents within each); agent id order is meaningless.
    pub fn next_live_agent(&self, delta: isize) -> Option<AgentId> {
        let live = self
            .rail_order()
            .into_iter()
            .filter(|agent_id| {
                self.agents.get(agent_id) == Some(&AgentLife::Live) && !self.agent_folded(*agent_id)
            })
            .collect::<Vec<_>>();
        if live.is_empty() {
            return None;
        }
        let len = live.len() as isize;
        let index = self
            .selected_agent()
            .and_then(|selected| live.iter().position(|agent_id| agent_id == selected))
            .map(|index| (index as isize + delta).rem_euclid(len) as usize)
            .unwrap_or_else(|| if delta < 0 { live.len() - 1 } else { 0 });
        live.get(index).copied()
    }

    /// All agents in rail display order: pinned workstreams first, and within
    /// one pinned agents first, then the active bucket, then the retained
    /// order. Agents known outside any workstream trail at the end.
    fn rail_order(&self) -> Vec<AgentId> {
        let mut workstreams = self.workstreams.iter().collect::<Vec<_>>();
        workstreams.sort_by_key(|workstream| workstream.status != rho_ui_proto::Status::Pinned);

        let mut candidates = Vec::new();
        let mut hidden = BTreeSet::new();
        for workstream in workstreams {
            if workstream.hidden {
                hidden.extend(workstream.agent_ids());
                continue;
            }
            if let Some(layout) = self.topic_rail_layouts.get(&workstream.tag_id) {
                candidates.extend(layout.listed.iter().map(|(agent_id, _)| *agent_id));
            }
        }
        for agent_id in self.agents.keys() {
            if !hidden.contains(agent_id) && !candidates.contains(agent_id) {
                candidates.push(*agent_id);
            }
        }
        candidates
    }

    fn order_topic_agents_with_state<'a>(
        &self,
        state: &TopicRailState<'_>,
        mut agents: Vec<&'a UiAgentSummary>,
    ) -> Vec<&'a UiAgentSummary> {
        agents.sort_by_key(|agent| {
            (
                agent.status != rho_ui_proto::Status::Pinned,
                !state.top_roots.contains(&agent.agent_id),
                self.rail_ranks
                    .get(&agent.agent_id)
                    .copied()
                    .unwrap_or(usize::MAX),
            )
        });

        let visible_ids = agents
            .iter()
            .map(|agent| agent.agent_id)
            .collect::<BTreeSet<_>>();
        let mut children = BTreeMap::<Option<AgentId>, Vec<_>>::new();
        for agent in agents {
            let parent = agent
                .parent_agent
                .filter(|parent| state.by_id.contains_key(parent) && visible_ids.contains(parent));
            children.entry(parent).or_default().push(agent);
        }

        fn append<'a>(
            parent: Option<AgentId>,
            children: &BTreeMap<Option<AgentId>, Vec<&'a UiAgentSummary>>,
            seen: &mut BTreeSet<AgentId>,
            ordered: &mut Vec<&'a UiAgentSummary>,
        ) {
            for agent in children.get(&parent).into_iter().flatten() {
                if seen.insert(agent.agent_id) {
                    ordered.push(agent);
                    append(Some(agent.agent_id), children, seen, ordered);
                }
            }
        }

        let mut ordered = Vec::new();
        let mut seen = BTreeSet::new();
        append(None, &children, &mut seen, &mut ordered);
        // Persisted data should be acyclic, but don't drop rows if it is not.
        for agents in children.values() {
            for agent in agents {
                if seen.insert(agent.agent_id) {
                    ordered.push(agent);
                    append(Some(agent.agent_id), &children, &mut seen, &mut ordered);
                }
            }
        }
        ordered
    }

    fn rebuild_topic_rail_layouts(&mut self) {
        let layouts = self
            .workstreams
            .iter()
            .map(|topic| {
                let state = TopicRailState::new(self, topic);
                let (listed, mut folded): (Vec<_>, Vec<_>) = topic
                    .agents
                    .iter()
                    .filter(|agent| !state.auto_collapsed(agent.agent_id))
                    .partition(|agent| !state.folded(agent.agent_id));
                let listed = self.order_topic_agents_with_state(&state, listed);
                folded.sort_by_key(|agent| Reverse(agent.updated_at));
                let indexes = topic
                    .agents
                    .iter()
                    .enumerate()
                    .map(|(index, agent)| (agent.agent_id, index))
                    .collect::<BTreeMap<_, _>>();
                let cache = |agents: Vec<&UiAgentSummary>| {
                    agents
                        .into_iter()
                        .map(|agent| (agent.agent_id, indexes[&agent.agent_id]))
                        .collect()
                };
                (
                    topic.tag_id,
                    TopicRailLayout {
                        listed: cache(listed),
                        folded: cache(folded),
                    },
                )
            })
            .collect();
        self.topic_rail_layouts = layouts;
    }

    fn resolve_cached_agents<'a>(
        topic: &'a Workstream,
        cached: &[(AgentId, usize)],
    ) -> Vec<&'a UiAgentSummary> {
        cached
            .iter()
            .filter_map(|(agent_id, index)| {
                topic
                    .agents
                    .get(*index)
                    .filter(|agent| agent.agent_id == *agent_id)
                    .or_else(|| {
                        topic
                            .agents
                            .iter()
                            .find(|agent| agent.agent_id == *agent_id)
                    })
            })
            .collect()
    }

    pub(crate) fn split_workstream_agents<'a>(
        &self,
        topic: &'a Workstream,
    ) -> (
        Vec<&'a UiAgentSummary>,
        Vec<&'a UiAgentSummary>,
    ) {
        let Some(layout) = self.topic_rail_layouts.get(&topic.tag_id) else {
            return (Vec::new(), Vec::new());
        };
        (
            Self::resolve_cached_agents(topic, &layout.listed),
            Self::resolve_cached_agents(topic, &layout.folded),
        )
    }

    pub fn top_bucket<'a>(
        &self,
        agents: impl IntoIterator<Item = &'a UiAgentSummary>,
    ) -> BTreeSet<AgentId> {
        let mut normal = agents
            .into_iter()
            .filter(|agent| agent.status != rho_ui_proto::Status::Pinned && !agent.hidden)
            .collect::<Vec<_>>();
        let colored_count = normal
            .iter()
            .filter(|agent| self.attention(agent.agent_id) != rho_ui_proto::UiAttention::Quiet)
            .count();
        let recent_quiet_slots = 5usize.saturating_sub(colored_count);
        let mut top = normal
            .iter()
            .filter(|agent| self.attention(agent.agent_id) != rho_ui_proto::UiAttention::Quiet)
            .map(|agent| agent.agent_id)
            .collect::<BTreeSet<_>>();

        normal.sort_by_key(|agent| {
            (
                Reverse(
                    self.last_active
                        .get(&agent.agent_id)
                        .copied()
                        .unwrap_or(agent.last_active),
                ),
                agent.agent_id,
            )
        });
        top.extend(
            normal
                .into_iter()
                .filter(|agent| self.attention(agent.agent_id) == rho_ui_proto::UiAttention::Quiet)
                .take(recent_quiet_slots)
                .map(|agent| agent.agent_id),
        );
        top
    }

    /// Resolves an agent label (as produced by [`Self::agent_id_label`],
    /// with or without a leading `@`) or display name back to the agent id.
    pub fn agent_by_label(&self, label: &str) -> Option<AgentId> {
        let label = label.strip_prefix('@').unwrap_or(label);
        self.agents.keys().copied().find(|agent_id| {
            self.agent_id_label(*agent_id) == label
                || self
                    .agent_display_name(*agent_id)
                    .is_some_and(|name| name.eq_ignore_ascii_case(label))
        })
    }

    pub fn live_agents(&self) -> impl Iterator<Item = &AgentId> {
        self.agents
            .iter()
            .filter(|(_, life)| **life == AgentLife::Live)
            .map(|(agent_id, _)| agent_id)
    }
}

/// New agents guaranteed between two label-length changes.
const LABEL_HEADROOM: u64 = 200;

#[cfg(test)]
mod tests {
    use rho_ui_proto::{AgentIdDomain, Status, UiAgentSummary, WorkspaceId, WorkspaceIdDomain};

    use super::*;

    fn agent_id(id: u64) -> AgentId {
        AgentId::from_counter(id, &AgentIdDomain(0)).unwrap()
    }

    /// Freshly-engaged fixture: `last_active` stays below now while `id`
    /// provides deterministic seeding order.
    fn agent(id: u64, status: Status) -> UiAgentSummary {
        UiAgentSummary {
            agent_id: agent_id(id),
            parent_agent: None,
            display_name: None,
            created_at: rho_core::UnixMs(id),
            updated_at: rho_core::UnixMs(id),
            role: rho_ui_proto::AgentRole::default(),
            workspace: rho_ui_proto::WorkspaceInfo::UserCheckout {
                repo: "/tmp".into(),
            },
            status,
            attention: rho_ui_proto::UiAttention::Quiet,
            last_active: rho_core::UnixMs(
                crate::workspace::now_ms()
                    .saturating_sub(10_000)
                    .saturating_add(id),
            ),
            hidden: false,
            tags: Vec::new(),
        }
    }

    fn named_agent(id: u64, name: &str) -> UiAgentSummary {
        UiAgentSummary {
            display_name: Some(name.to_owned()),
            ..agent(id, Status::Normal)
        }
    }

    fn workspace_agent(id: u64, workspace_id: WorkspaceId) -> UiAgentSummary {
        UiAgentSummary {
            agent_id: agent_id(id),
            parent_agent: None,
            display_name: None,
            created_at: rho_core::UnixMs(id),
            updated_at: rho_core::UnixMs(id),
            role: rho_ui_proto::AgentRole::default(),
            workspace: rho_ui_proto::WorkspaceInfo::Workspace {
                repo: "/tmp".into(),
                id: workspace_id,
            },
            status: Status::Normal,
            attention: rho_ui_proto::UiAttention::Quiet,
            last_active: rho_core::UnixMs(0),
            hidden: false,
            tags: Vec::new(),
        }
    }

    /// A workstream tag with its members' `tags` set to match.
    fn topic(id: u64, status: Status, mut agents: Vec<UiAgentSummary>) -> (UiTag, Vec<UiAgentSummary>) {
        for agent in &mut agents {
            agent.tags = vec![TagId(id)];
        }
        (
            UiTag {
                tag_id: TagId(id),
                name: id.to_string(),
                kind: TagKind::Workstream,
                parent: None,
                status,
                hidden: false,
            },
            agents,
        )
    }

    fn set_topics(registry: &mut AgentRegistry, topics: Vec<(UiTag, Vec<UiAgentSummary>)>) {
        let mut tags = Vec::new();
        let mut agents = Vec::new();
        for (tag, members) in topics {
            tags.push(tag);
            agents.extend(members);
        }
        registry.set_data(tags, agents);
    }

    #[test]
    fn cycling_follows_active_rail_order() {
        let mut registry = AgentRegistry::default();
        let mut recent = agent(3, Status::Normal);
        recent.last_active = rho_core::UnixMs(crate::workspace::now_ms() + 100);
        set_topics(&mut registry, vec![topic(
            1,
            Status::Normal,
            vec![agent(1, Status::Normal), agent(2, Status::Pinned), recent],
        )]);
        for id in 1..=3 {
            registry.mark_live(agent_id(id));
        }

        // Active rail order is pinned first, then most recently engaged
        // (agent 3's newer last-user-message seeds it above the idle 1):
        // 2, 3, 1. Forward cycling should move down that visible order.
        registry.select_agent(agent_id(2));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(3)));
        registry.select_agent(agent_id(3));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(1)));
        assert_eq!(registry.next_live_agent(-1), Some(agent_id(2)));
    }

    #[test]
    fn hidden_topics_are_excluded_from_rail_navigation() {
        let mut registry = AgentRegistry::default();
        let mut hidden = topic(1, Status::Normal, vec![agent(1, Status::Normal)]);
        hidden.0.hidden = true;
        set_topics(&mut registry, vec![
            hidden,
            topic(2, Status::Normal, vec![agent(2, Status::Normal)]),
        ]);
        registry.agents.insert(agent_id(1), AgentLife::Live);
        registry.agents.insert(agent_id(2), AgentLife::Live);

        assert_eq!(registry.next_live_agent(1), Some(agent_id(2)));
    }

    #[test]
    fn attention_change_keeps_retained_order_inside_top_bucket() {
        use rho_ui_proto::UiAttention;

        let mut registry = AgentRegistry::default();
        let agents = (1..=3)
            .map(|id| agent(id, Status::Normal))
            .collect::<Vec<_>>();
        set_topics(&mut registry, vec![topic(1, Status::Normal, agents)]);
        for id in 1..=3 {
            registry.mark_live(agent_id(id));
        }
        // Seeded by engagement recency: 3, 2, 1.
        registry.select_agent(agent_id(3));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(2)));

        // Agent 1 works and settles back to Quiet. Since all three agents
        // remain inside the top bucket, attention changes do not reshuffle
        // their retained order.
        registry.set_attention(agent_id(1), UiAttention::Working);
        registry.set_attention(agent_id(1), UiAttention::Quiet);
        registry.select_agent(agent_id(1));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(3)));
        registry.select_agent(agent_id(3));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(2)));

        // A repeat of the same level also leaves the retained order alone.
        registry.set_attention(agent_id(2), UiAttention::Quiet);
        registry.select_agent(agent_id(1));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(3)));
    }

    #[test]
    fn attention_moves_agent_into_but_not_to_front_of_top_bucket() {
        use rho_ui_proto::UiAttention;

        let mut registry = AgentRegistry::default();
        let agents = (1..=7)
            .map(|id| agent(id, Status::Normal))
            .collect::<Vec<_>>();
        set_topics(&mut registry, vec![topic(1, Status::Normal, agents)]);
        for id in 1..=7 {
            registry.mark_live(agent_id(id));
        }

        // Seeded by engagement recency: 7, 6, 5, 4, 3 are the quiet top
        // bucket; 2 and 1 remain visible as the extra tail.
        registry.select_agent(agent_id(4));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(3)));
        registry.select_agent(agent_id(3));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(2)));

        // Coloring agent 1 admits it to the top bucket, but stable retained
        // order keeps it behind the already-top agents instead of jumping to
        // row one.
        registry.set_attention(agent_id(1), UiAttention::Working);
        registry.select_agent(agent_id(4));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(1)));
        registry.select_agent(agent_id(1));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(3)));
    }

    #[test]
    fn cycling_and_attention_jump_skip_folded_agents() {
        use rho_ui_proto::UiAttention;

        let mut registry = AgentRegistry::default();
        let mut filed = agent(2, Status::Normal);
        filed.hidden = true;
        let mut agents = (1..=13)
            .map(|id| agent(id, Status::Normal))
            .collect::<Vec<_>>();
        agents[1] = filed;
        set_topics(&mut registry, vec![topic(1, Status::Normal, agents)]);
        for id in 1..=13 {
            registry.mark_live(agent_id(id));
        }

        // Explicitly filed agents fold, and quiet agents outside the active
        // bucket plus the five-agent tail fold automatically.
        assert!(registry.agent_folded(agent_id(2)));
        assert!(registry.agent_folded(agent_id(1)));

        // Attention unfolds an otherwise folded agent: it needs the user, so it
        // rejoins cycling and can win the jump.
        registry.select_agent(agent_id(10));
        registry.set_attention(agent_id(1), UiAttention::Pending);
        assert!(!registry.agent_folded(agent_id(1)));
        assert_eq!(registry.next_attention_agent(), Some(agent_id(1)));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(1)));
    }

    #[test]
    fn attention_jump_picks_most_urgent_excluding_selected() {
        use rho_ui_proto::UiAttention;

        let mut registry = AgentRegistry::default();
        set_topics(&mut registry, vec![topic(
            1,
            Status::Normal,
            vec![
                agent(1, Status::Normal),
                agent(2, Status::Normal),
                agent(3, Status::Normal),
            ],
        )]);
        assert_eq!(registry.next_attention_agent(), None);

        registry.set_attention(agent_id(1), UiAttention::Pending);
        registry.set_attention(agent_id(2), UiAttention::NeedsInput);
        registry.set_attention(agent_id(3), UiAttention::Working);
        assert_eq!(registry.next_attention_agent(), Some(agent_id(2)));

        // The agent already on screen never wins the jump; the next-most
        // urgent one does. Working alone never qualifies.
        registry.select_agent(agent_id(2));
        assert_eq!(registry.next_attention_agent(), Some(agent_id(1)));
        registry.set_attention(agent_id(1), UiAttention::Quiet);
        assert_eq!(registry.next_attention_agent(), None);
    }

    #[test]
    fn pins_stay_above_attention_bucket_in_rail_order() {
        use rho_ui_proto::UiAttention;

        let mut registry = AgentRegistry::default();
        set_topics(&mut registry, vec![topic(
            1,
            Status::Normal,
            vec![agent(1, Status::Normal), agent(2, Status::Pinned)],
        )]);
        for id in 1..=2 {
            registry.mark_live(agent_id(id));
        }
        registry.set_attention(agent_id(1), UiAttention::NeedsInput);

        // Pinned agent 2 would lead by status, but agent 1's attention
        // pushes it to the top of the rail (and thus of cycling).
        registry.select_agent(agent_id(1));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(2)));
        assert_eq!(registry.next_live_agent(-1), Some(agent_id(2)));
    }

    #[test]
    fn agents_outside_active_bucket_and_tail_fold_until_engaged_again() {
        let mut registry = AgentRegistry::default();
        let mut idle = agent(1, Status::Normal);
        idle.last_active = rho_core::UnixMs(0);
        let mut idle_pinned = agent(2, Status::Pinned);
        idle_pinned.last_active = rho_core::UnixMs(0);
        let mut agents = vec![idle, idle_pinned];
        agents.extend((3..=13).map(|id| agent(id, Status::Normal)));
        set_topics(&mut registry, vec![topic(1, Status::Normal, agents)]);

        // The quiet tail beyond the active bucket plus five more folds away;
        // pins and active bucket agents stay. Fresh engagement revives it.
        assert!(registry.agent_folded(agent_id(1)));
        assert!(!registry.agent_folded(agent_id(2)));
        assert!(!registry.agent_folded(agent_id(13)));
        registry.touch_agent(agent_id(1));
        assert!(!registry.agent_folded(agent_id(1)));
    }

    #[test]
    fn workspace_labels_use_uniform_unique_prefixes() {
        let domain = WorkspaceIdDomain(0);
        let short_workspace = WorkspaceId::from_counter(1, &domain).unwrap();
        let long_workspace = WorkspaceId::from_counter(36 * 36, &domain).unwrap();

        let mut registry = AgentRegistry::default();
        registry.set_machine_seed(0);
        registry.set_workspace_counter(36 * 36);
        set_topics(&mut registry, vec![topic(
            1,
            Status::Normal,
            vec![
                workspace_agent(1, short_workspace),
                workspace_agent(2, long_workspace),
                agent(3, Status::Normal),
            ],
        )]);

        assert_eq!(
            registry.workspace_id_label(agent_id(1)),
            Some(format!("ws-{}", &short_workspace.encoded()[..3]))
        );
        assert_eq!(
            registry.workspace_id_label(agent_id(2)),
            Some(format!("ws-{}", &long_workspace.encoded()[..3]))
        );
        assert_eq!(registry.workspace_id_label(agent_id(3)), None);
    }

    #[test]
    fn agent_labels_use_ready_counter() {
        let id = agent_id(1);
        let mut registry = AgentRegistry::default();
        registry.set_agent_counter(36 * 36);

        assert_eq!(
            registry.agent_id_label(id),
            format!("eng-{}", &id.encoded()[..4])
        );
    }

    #[test]
    fn agent_lookup_accepts_display_name() {
        let mut registry = AgentRegistry::default();
        set_topics(&mut registry, vec![topic(
            1,
            Status::Normal,
            vec![named_agent(1, "Fix Tests")],
        )]);

        assert_eq!(registry.agent_by_label("fix tests"), Some(agent_id(1)));
        assert_eq!(
            registry.agent_by_label(&registry.agent_id_label(agent_id(1))),
            Some(agent_id(1))
        );
    }
}
