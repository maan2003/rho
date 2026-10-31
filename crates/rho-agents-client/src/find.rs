//! Answering a query with agents.
//!
//! Find is one prompt over every source, so the scoring and the prompt are
//! not this crate's; what is, is which names an agent answers to. The
//! reader looks for what they remember, which is rarely the title the
//! agent ended up with: they remember the label, or the thing they last
//! asked for. Every one of those names finds it.

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
    use rho_agent_hosts::HostId;
    use rho_agent_types::{Address, AgentIdDomain, ContentPart, Mail, MailId, SendKind, UnixMs};

    use super::*;
    use crate::{AgentIdentity, MirroredAgent};

    fn agent() -> AgentId {
        AgentId::from_counter(1, &AgentIdDomain(0)).expect("an agent id")
    }

    fn mail(id: u64, from: Address, to: Address, kind: Option<SendKind>, text: &str) -> Mail {
        Mail {
            id: MailId(id),
            at: UnixMs(id),
            from,
            to,
            thread: rho_agent_types::ThreadId(MailId(id)),
            kind,
            content: vec![ContentPart::Text { text: text.into() }],
        }
    }

    fn said(id: u64, text: &str) -> Mail {
        mail(id, Address::User, Address::Agent(agent()), None, text)
    }

    /// The agent as the model thread hands it up: its directory entry, its
    /// mail folded into its digest.
    fn told(registry: &mut AgentMap, mails: Vec<Mail>) {
        let host = HostId::default();
        registry.set_host_data(host, 0, 1);
        let identity = AgentIdentity::for_test(
            agent(),
            [("user_facing".to_owned(), "true".to_owned())].into(),
            UnixMs(1),
        );
        let mut mirrored = MirroredAgent::new(host, identity);
        for mail in &mails {
            mirrored.tell(mail);
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
                said(2, "fix the flaky mirror test"),
                mail(
                    3,
                    Address::Agent(agent()),
                    Address::User,
                    Some(SendKind::Result),
                    "fixed",
                ),
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
        assert_eq!(hit.recency, 3);
    }

    /// The same name twice is not two names.
    #[test]
    fn a_title_is_not_repeated_as_an_alias() {
        let mut registry = AgentMap::default();
        told(&mut registry, vec![said(2, "mirror test")]);
        let hit = hit(&registry, agent(), Some("mirror test".to_owned()));
        assert_eq!(hit.aka, vec![registry.agent_id_label(agent())]);
    }

    /// A tree with no title for an agent falls back to the agent's own
    /// name rather than showing it as untitled.
    #[test]
    fn an_untitled_agent_is_shown_by_its_own_name() {
        let mut registry = AgentMap::default();
        told(&mut registry, vec![said(2, "write the note")]);
        let hit = hit(&registry, agent(), None);
        assert_eq!(hit.title, "write the note");
    }
}
