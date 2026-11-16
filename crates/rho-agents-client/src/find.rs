//! Answering a query with agents.
//!
//! Find is one prompt over every source, so the scoring and the prompt are
//! not this crate's; what is, is which names an agent answers to. The
//! reader looks for what they remember, which is rarely the title the
//! agent ended up with: they remember the label too, and either finds it.

use rho_agent_types::AgentId;

use crate::map::AgentMap;

/// One agent as an answer to a query: the name a row shows for it, the
/// other names it answers to, and how recently it was used.
///
/// This is a hit, not a card. A card claims the reader's attention and
/// carries a reason for doing so; a hit only answers what was asked. When
/// a card's reason becomes a type of its own, a hit gains one too — the
/// same type, in its own place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentHit {
    pub agent_id: AgentId,
    /// What to show: the title the tree has for it, or the agent's own
    /// name where the tree has none.
    pub title: String,
    /// Names the query matches but the row never shows: the agent's label.
    pub aka: Vec<String>,
    /// Unix milliseconds of the last use, for ranking equal matches: when
    /// it was made or last touched. Zero where nothing records a use.
    pub recency: i64,
}

/// The agent as a hit, under the title the tree gives it. `None` means the
/// tree has no title, and the agent's own name stands in.
pub fn hit(registry: &AgentMap, agent_id: AgentId, title: Option<String>) -> AgentHit {
    let title = title.unwrap_or_else(|| registry.agent_human_name(agent_id));
    AgentHit {
        agent_id,
        title,
        aka: vec![registry.agent_id_label(agent_id)],
        recency: registry
            .agent_last_active(agent_id)
            .map(|active| active.0 as i64)
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_hosts::HostId;
    use rho_agent_types::{AgentIdDomain, EndpointId, UnixMs, props};

    use super::*;
    use crate::{AgentIdentity, MirroredAgent};

    fn agent() -> AgentId {
        AgentId::from_counter(1, &AgentIdDomain(0)).expect("an agent id")
    }

    /// The agent as the model thread hands it up: its directory entry,
    /// made at 7, with these props.
    fn told(registry: &mut AgentMap, props: rho_agent_types::Props) {
        let host = HostId::default();
        registry.set_host_data(host, 0, EndpointId([0; 32]), 1);
        let identity = AgentIdentity::for_test(agent(), props, UnixMs(7));
        registry.told(vec![MirroredAgent::new(host, identity)]);
    }

    /// The row shows the tree's title; the label is a name it answers to
    /// that no row shows, and it ranks by when it was made.
    #[test]
    fn an_agent_answers_to_its_label() {
        let mut registry = AgentMap::default();
        told(&mut registry, Default::default());
        let hit = hit(&registry, agent(), Some("mirror".to_owned()));
        assert_eq!(hit.title, "mirror");
        let label = registry.agent_id_label(agent());
        assert!(label.starts_with("ag-"), "{label}");
        assert_eq!(hit.aka, vec![label]);
        assert_eq!(hit.recency, 7);
    }

    /// A tree with no title for an agent falls back to the agent's own
    /// name rather than showing it as untitled.
    #[test]
    fn an_untitled_agent_is_shown_by_its_own_name() {
        let mut registry = AgentMap::default();
        told(
            &mut registry,
            [(props::NAME.to_owned(), "write the note".to_owned())]
                .into_iter()
                .collect(),
        );
        assert_eq!(hit(&registry, agent(), None).title, "write the note");
    }
}
