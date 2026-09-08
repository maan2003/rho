//! What there is to find, read from the store rather than from a screen.
//!
//! The finder used to ask the dashboard for its candidates, and the
//! dashboard knew them because it had just composed them into an editor.
//! That made the map the only thing that knew what existed: a surface, with
//! buffers and folds and inlays, standing between the store and a question
//! about what is in it. The map is going; the question is not.
//!
//! So the candidates come from `DeskCells` — the store client — with the
//! agent registry and the Slack facts beside it. Nothing here draws
//! anything, holds an editor, or needs one to have been drawn: the same
//! answer comes back on a window that has never opened the map.
//!
//! The shapes it walks are the store's own. A note is a heading and its
//! title is its first line; a label is a filing path; an agent or a page
//! hangs under whatever it was filed beneath. That is all the tree ever
//! was, and it is in the cells and not in the rows.

use std::collections::{BTreeSet, HashMap, HashSet};

use gpui::App;
use rho_agents::{AgentMap, HostId};
use rho_desk::cells::Id;

use crate::desk_view::{DeskCells, DeskNode};
use crate::find::{FindCandidate, FindTarget};

/// One host's nodes with the lookups a path needs, built once per question.
///
/// The finder asks for every candidate at once and then matches against
/// them, so the parent walks and the label paths are worth doing here
/// rather than per node: a breadcrumb is a walk to the root, and doing that
/// from scratch for every node is the shape that made the map expensive in
/// the first place.
pub(crate) struct HostNodes {
    nodes: Vec<DeskNode>,
    by_id: HashMap<Id, usize>,
    /// Each node's children, in the store's order. Walking every node to
    /// find one node's children is quadratic in a desk that only grows,
    /// which is what made dealing one expensive; it is an index instead.
    children: HashMap<Id, Vec<usize>>,
    by_agent: HashMap<rho_core::AgentId, usize>,
    by_page: HashMap<rho_desk::PageId, usize>,
    titles: HashMap<Id, String>,
    /// Every label's full filing path, `rho/agent`.
    label_paths: HashMap<Id, String>,
    /// The agents filed under each heading, for ranking a heading by how
    /// recently anything under it was touched.
    heading_agents: HashMap<Id, Vec<rho_core::AgentId>>,
}

impl HostNodes {
    /// Read one host's nodes out of the store.
    ///
    /// Titles come from the note buffers themselves, including the derived
    /// rows the map used to write into read-only ones. Reading buffers is
    /// why this belongs to a keystroke and not to a sync, and why it takes
    /// the store client by shared reference: asking what there is must not
    /// be able to change anything.
    pub(crate) fn of(desk: &DeskCells, host: HostId, cx: &App) -> Self {
        let nodes = desk.nodes(host).to_vec();
        let mut titles = HashMap::with_capacity(nodes.len());
        for node in &nodes {
            if let Some(buffer) = desk.buffer(host, &node.id) {
                titles.insert(
                    node.id.clone(),
                    crate::dashboard::note_title(&buffer.read(cx).text()).to_owned(),
                );
            }
        }
        Self::build(nodes, titles, desk.label_paths(host).into_iter().collect())
    }

    /// The same nodes with the notes' titles only, and no rope read.
    ///
    /// This is the dealer's source, and the dealer runs on every sync. A
    /// card is filed and ranked by notes — a breadcrumb is a path of
    /// headings — so the derived titles of machine rows are work it would
    /// never look at. The store client already keeps the note titles and
    /// refreshes them by comparing buffer versions, so this costs the
    /// nodes and the indexes and nothing else.
    pub(crate) fn of_notes(desk: &mut DeskCells, host: HostId, cx: &App) -> Self {
        let nodes = desk.nodes(host).to_vec();
        let titles = desk
            .note_titles(host, cx)
            .map(|titles| (*titles).clone())
            .unwrap_or_default();
        Self::build(nodes, titles, desk.label_paths(host).into_iter().collect())
    }

    /// The indexes, made in one pass over the nodes.
    fn build(
        nodes: Vec<DeskNode>,
        titles: HashMap<Id, String>,
        label_paths: HashMap<Id, String>,
    ) -> Self {
        let mut by_id = HashMap::with_capacity(nodes.len());
        let mut children: HashMap<Id, Vec<usize>> = HashMap::new();
        let mut by_agent = HashMap::new();
        let mut by_page = HashMap::new();
        for (at, node) in nodes.iter().enumerate() {
            by_id.insert(node.id.clone(), at);
            if let Some(parent) = &node.parent {
                children.entry(parent.clone()).or_default().push(at);
            }
            if let Some(agent) = node.agent() {
                by_agent.insert(agent, at);
            }
            if let Some(page) = node.page() {
                by_page.insert(page, at);
            }
        }
        let mut source = Self {
            nodes,
            by_id,
            children,
            by_agent,
            by_page,
            titles,
            label_paths,
            heading_agents: HashMap::new(),
        };
        source.heading_agents = source.build_heading_agents();
        source
    }

    pub(crate) fn nodes(&self) -> &[DeskNode] {
        &self.nodes
    }

    /// Where a node sits in the store's order, which is the tie-break a
    /// card carries.
    pub(crate) fn order_of(&self, id: &Id) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    /// The order an agent's own row sits at, or after everything when the
    /// agent has no row. Filing decides where a card is shown, never
    /// whether it exists, so an unfiled agent still gets a number.
    pub(crate) fn agent_order(&self, agent: rho_core::AgentId) -> usize {
        self.by_agent.get(&agent).copied().unwrap_or(usize::MAX)
    }

    pub(crate) fn children(&self, id: &Id) -> impl Iterator<Item = &DeskNode> {
        self.children
            .get(id)
            .into_iter()
            .flatten()
            .map(|at| &self.nodes[*at])
    }

    pub(crate) fn agent_node(&self, agent: rho_core::AgentId) -> Option<&DeskNode> {
        self.by_agent.get(&agent).map(|at| &self.nodes[*at])
    }

    /// The row a page is filed as, if it is filed at all.
    pub(crate) fn page_node(&self, page: rho_desk::PageId) -> Option<&DeskNode> {
        self.by_page.get(&page).map(|index| &self.nodes[*index])
    }

    pub(crate) fn node(&self, id: &Id) -> Option<&DeskNode> {
        self.by_id.get(id).map(|index| &self.nodes[*index])
    }

    pub(crate) fn title(&self, id: &Id) -> Option<&str> {
        self.titles.get(id).map(String::as_str)
    }

    /// A note's title trimmed to something worth showing, or nothing.
    pub(crate) fn shown_title(&self, id: &Id) -> Option<String> {
        self.title(id)
            .and_then(|text| text.lines().next())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    }

    /// The agents filed anywhere under a heading, in the store's order.
    pub(crate) fn agents_under(&self, heading: &Id) -> &[rho_core::AgentId] {
        self.heading_agents
            .get(heading)
            .map_or(&[], |agents| agents.as_slice())
    }

    /// The same nodes in the same places: ids and parents, in order. Asked
    /// against the desk's own node list, which is already to hand, and not
    /// against another source — building one to find out whether it was
    /// needed is the cost this question exists to avoid.
    pub(crate) fn same_shape_as(&self, nodes: &[DeskNode]) -> bool {
        self.nodes.len() == nodes.len()
            && self
                .nodes
                .iter()
                .zip(nodes)
                .all(|(held, fresh)| held.id == fresh.id && held.under == fresh.under)
    }

    /// The rows a delta named, copied across. The dealer's source stands
    /// one step behind the desk exactly as the map's does: a verdict moves
    /// a node's state where it sits and moves no shape, so the indexes
    /// still hold and only the nodes are replaced.
    ///
    /// Answers false when a row the delta names is not where it was, which
    /// is the caller's cue that the shape moved and the whole source has to
    /// be taken again.
    pub(crate) fn patch(&mut self, touched: &BTreeSet<Id>, nodes: &[DeskNode]) -> bool {
        for id in touched {
            let Some(at) = self.by_id.get(id).copied() else {
                return false;
            };
            let Some(fresh) = nodes.get(at) else {
                return false;
            };
            if fresh.id != *id {
                return false;
            }
            self.nodes[at] = fresh.clone();
        }
        true
    }

    /// The path of headings above a node, `nixos › poco on linux`.
    pub(crate) fn breadcrumb(&self, id: &Id) -> String {
        let mut path = Vec::new();
        let mut cursor = Some(id.clone());
        while let Some(id) = cursor {
            let Some(node) = self.node(&id) else { break };
            if node.is_note() {
                path.push(self.title(&id).unwrap_or(""));
            }
            cursor = node.parent.clone();
        }
        path.reverse();
        path.join(" › ")
    }

    /// Each heading and the agents filed anywhere beneath it.
    ///
    /// The map kept this as it composed. Derived here instead by walking
    /// each agent up to its nearest heading, which is the same answer from
    /// the side that does not need a row to have been drawn.
    fn build_heading_agents(&self) -> HashMap<Id, Vec<rho_core::AgentId>> {
        let mut under: HashMap<Id, Vec<rho_core::AgentId>> = HashMap::new();
        for node in &self.nodes {
            let Some(agent_id) = node.agent() else {
                continue;
            };
            let mut cursor = node.parent.clone();
            while let Some(id) = cursor {
                let Some(above) = self.node(&id) else { break };
                if above.is_note() {
                    under.entry(id).or_default().push(agent_id);
                    break;
                }
                cursor = above.parent.clone();
            }
        }
        under
    }
}

/// Every findable thing, across every host the store client holds.
///
/// Slack conversations are not here: `find` adds those itself from the
/// mirror it already reads, because what is findable about a conversation
/// is its name in Slack and not where anyone filed it.
pub(crate) fn find_candidates(
    desk: &DeskCells,
    registry: &AgentMap,
    cx: &App,
) -> Vec<FindCandidate> {
    let mut candidates = Vec::new();
    let mut filed = HashSet::new();
    for host in desk.hosts() {
        let source = HostNodes::of(desk, host, cx);
        for node in &source.nodes {
            if let Some(agent_id) = node.agent() {
                filed.insert(agent_id);
            }
            let breadcrumb = source.breadcrumb(&node.id);
            let under = |title: String| {
                if breadcrumb.is_empty() {
                    title
                } else {
                    format!("{breadcrumb} › {title}")
                }
            };
            // A thing is as often remembered by what it is filed under as
            // by where it sits, so each label names it too.
            let labelled = |title: &str| {
                node.labels
                    .iter()
                    .filter_map(|label| source.label_paths.get(label))
                    .map(|path| format!("{path} › {title}"))
                    .collect::<Vec<_>>()
            };
            let candidate = match &node.id {
                Id::Note(_) => {
                    if breadcrumb.is_empty() {
                        continue;
                    }
                    FindCandidate {
                        labels: labelled(&breadcrumb),
                        aka: Vec::new(),
                        path: breadcrumb.clone(),
                        kind: "topic",
                        target: FindTarget::Topic {
                            host,
                            node_id: node.id.clone(),
                        },
                        recency: source
                            .heading_agents
                            .get(&node.id)
                            .into_iter()
                            .flatten()
                            .filter_map(|agent_id| registry.agent_last_active(*agent_id))
                            .map(|active| active.0 as i64)
                            .max()
                            .unwrap_or_default(),
                    }
                }
                Id::Agent(_) => {
                    let Some(agent_id) = node.agent() else {
                        continue;
                    };
                    // Which names an agent answers to is the agent crate's;
                    // where it sits in the tree is this node's.
                    let hit = rho_agents::find::hit(
                        registry,
                        agent_id,
                        source.shown_title(&node.id.clone()),
                    );
                    FindCandidate {
                        labels: labelled(&hit.title),
                        aka: hit.aka,
                        path: under(hit.title),
                        kind: "agent",
                        target: FindTarget::Agent(agent_id),
                        recency: hit.recency,
                    }
                }
                Id::Page(_) => {
                    // The store's page id and the browser's are the same
                    // sixteen bytes under two names.
                    let Some(page_id) = node
                        .page()
                        .map(|page| rho_browser::PageId(uuid::Uuid::from_bytes(page.0)))
                    else {
                        continue;
                    };
                    // The map wrote a page's name into its read-only row and
                    // the finder read it back out. Nothing draws that row
                    // now, so the name comes from the browser, which is
                    // where it came from in the first place.
                    let title = source.shown_title(&node.id).unwrap_or_else(|| {
                        rho_browser::live_page_name(page_id).unwrap_or_else(|| "page".to_owned())
                    });
                    FindCandidate {
                        labels: labelled(&title),
                        aka: Vec::new(),
                        path: under(title),
                        kind: "page",
                        target: FindTarget::Page(page_id),
                        recency: 0,
                    }
                }
                _ => continue,
            };
            candidates.push(candidate);
        }
    }
    // An agent nobody filed is findable all the same: filing says where a
    // thing sits, and the finder is for the ones the reader cannot point at.
    for agent_id in registry.known_agents().copied() {
        if filed.contains(&agent_id) || registry.agent_hidden(agent_id) {
            continue;
        }
        let hit = rho_agents::find::hit(registry, agent_id, None);
        candidates.push(FindCandidate {
            labels: Vec::new(),
            aka: hit.aka,
            path: hit.title,
            kind: "agent",
            target: FindTarget::Agent(agent_id),
            recency: hit.recency,
        });
    }
    candidates
}
