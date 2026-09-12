//! Answering a query with agents.
//!
//! Find is one prompt over every source, so the scoring and the prompt are
//! not this crate's; what is, is which names an agent answers to. The
//! reader looks for what they remember, which is rarely the title the
//! agent ended up with: they remember the label, or the thing they last
//! asked for. Every one of those names finds it.

use rho_ui_proto::AgentId;

use crate::map::AgentMap;

/// One agent as an answer to a query: the name a row shows for it, the
/// other names it answers to, and how recently it was used.
///
/// This is a hit, not a card. A card claims the reader's attention and
/// carries a reason for doing so; a hit only answers what was asked. When
/// a card's reason becomes a type of its own, a hit gains one too — the
/// same type, in its own place (`GUI-CRATES-DESIGN.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentHit {
    pub agent_id: AgentId,
    /// What to show: the title the tree has for it, or the agent's own
    /// name where the tree has none.
    pub title: String,
    /// Names the query matches but the row never shows: the agent's label,
    /// and the last thing the user said to it when that is not the title
    /// already.
    pub aka: Vec<String>,
    /// Unix milliseconds of the last use, for ranking equal matches. Zero
    /// where nothing records a use.
    pub recency: i64,
}

/// The agent as a hit, under the title the tree gives it. `None` means the
/// tree has no title, and the agent's own name stands in.
pub fn hit(registry: &AgentMap, agent_id: AgentId, title: Option<String>) -> AgentHit {
    let title = title.unwrap_or_else(|| registry.agent_human_name(agent_id));
    let mut aka = vec![registry.agent_id_label(agent_id)];
    if let Some(said) = registry.agent_last_user_message(agent_id)
        && said != title
    {
        aka.push(said.to_owned());
    }
    AgentHit {
        agent_id,
        title,
        aka,
        recency: registry
            .agent_last_active(agent_id)
            .map(|active| active.0 as i64)
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use rho_core::{MessageDelivery, UnixMs};
    use rho_hosts::HostId;
    use rho_ui_proto::AgentIdDomain;
    use rho_ui_proto::mirror::{
        AgentPos, MirrorEvent, RuntimeKind, SpawnedBy, TurnEdge, TurnOutcome,
    };

    use super::*;
    use crate::MirroredAgent;

    fn agent() -> AgentId {
        AgentId::from_counter(1, &AgentIdDomain(0)).expect("an agent id")
    }

    fn created(at: u64) -> MirrorEvent {
        MirrorEvent::Created {
            role: rho_ui_proto::AgentRole::default(),
            runtime: RuntimeKind::Rho,
            place: rho_ui_proto::Place {
                workset: "0123456789ab".into(),
                cwd: "/src/repo".into(),
                mode: Default::default(),
                origin: None,
            },
            spawned_by: SpawnedBy::Direct,
            spawn_name: None,
            parent: None,
            model: "sol".to_owned(),
            at: UnixMs(at),
        }
    }

    fn said(text: &str, at: u64) -> MirrorEvent {
        MirrorEvent::Message {
            from: None,
            text: text.to_owned(),
            delivery: MessageDelivery::Immediate,
            at: UnixMs(at),
        }
    }

    /// The agent as the model thread hands it up: its first row, then the
    /// rows after it folded into the same digest.
    fn told(registry: &mut AgentMap, events: Vec<MirrorEvent>) {
        let host = HostId::default();
        registry.set_host_data(host, 0, 1);
        let mut rows = events.into_iter();
        let first = rows.next().expect("an agent starts with its Created");
        let mut mirrored =
            MirroredAgent::new(host, agent(), &first).expect("the first row is a Created");
        for (offset, event) in rows.enumerate() {
            mirrored.digest.tell(AgentPos(offset as u64 + 1), &event);
        }
        registry.told(vec![mirrored]);
    }

    /// What the reader remembers is what they last asked for, so that is a
    /// name the agent answers to even though no row shows it.
    #[test]
    fn an_agent_answers_to_what_was_last_said_to_it() {
        let mut registry = AgentMap::default();
        told(
            &mut registry,
            vec![
                created(1),
                said("fix the flaky mirror test", 2),
                MirrorEvent::Turn {
                    edge: TurnEdge::Ended(TurnOutcome::Completed),
                    at: UnixMs(3),
                },
            ],
        );

        let hit = hit(&registry, agent(), Some("mirror".to_owned()));
        assert_eq!(hit.title, "mirror");
        assert_eq!(
            hit.aka,
            vec![
                registry.agent_id_label(agent()),
                "fix the flaky mirror test".to_owned()
            ],
            "the label and the last thing said both find it"
        );
    }

    /// The same name twice is not two names: a title that is already what
    /// was last said is not repeated as an alias.
    #[test]
    fn a_title_is_not_repeated_as_an_alias() {
        let mut registry = AgentMap::default();
        told(&mut registry, vec![created(1), said("mirror test", 2)]);

        let hit = hit(&registry, agent(), Some("mirror test".to_owned()));
        assert_eq!(hit.aka, vec![registry.agent_id_label(agent())]);
    }

    /// A tree with no title for an agent falls back to the agent's own
    /// name rather than showing it as untitled.
    #[test]
    fn an_untitled_agent_is_shown_by_its_own_name() {
        let mut registry = AgentMap::default();
        told(&mut registry, vec![created(1), said("write the note", 2)]);

        let hit = hit(&registry, agent(), None);
        assert_eq!(hit.title, registry.agent_human_name(agent()));
    }
}
