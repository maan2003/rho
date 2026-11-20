//! Where under an agent an address points
//! ([SPEC-agent-address](../../specs/SPEC-agent-address.md)).

use std::fmt;

use rho_agent_types::AgentId;
use rho_frame::Addressed;
use rho_mail::AgentRef;

/// What of an agent an `agent:` address opens.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// `agent:<id>`: the transcript.
    Transcript,
    /// `agent:<id>/term/<n>`: terminal `n` of the agent's workset.
    Terminal(u64),
    /// `agent:<id>/shell`: the agent's one host-owned shell.
    Shell,
    /// `agent:<id>/desktop/<name>`: a desktop the agent's workset runs.
    Desktop(String),
}

/// An agent and the part of it an address opens. The id is the host's,
/// unique across hosts, so the address carries no host.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentAddress {
    pub agent: AgentId,
    pub part: Part,
}

impl AgentAddress {
    pub fn transcript(agent: AgentId) -> Self {
        Self {
            agent,
            part: Part::Transcript,
        }
    }

    pub fn terminal(agent: AgentId, terminal: u64) -> Self {
        Self {
            agent,
            part: Part::Terminal(terminal),
        }
    }

    pub fn shell(agent: AgentId) -> Self {
        Self {
            agent,
            part: Part::Shell,
        }
    }

    /// `None` for a name that would not read back: empty, or with `/`.
    pub fn desktop(agent: AgentId, name: String) -> Option<Self> {
        (!name.is_empty() && !name.contains('/')).then_some(Self {
            agent,
            part: Part::Desktop(name),
        })
    }
}

impl Addressed for AgentAddress {
    const KIND: &'static str = AgentRef::KIND;

    fn rest(&self) -> String {
        let id = AgentRef(self.agent).rest();
        match &self.part {
            Part::Transcript => id,
            Part::Terminal(terminal) => format!("{id}/term/{terminal}"),
            Part::Shell => format!("{id}/shell"),
            Part::Desktop(name) => format!("{id}/desktop/{name}"),
        }
    }

    fn from_rest(rest: &str) -> Option<Self> {
        let (id, part) = match rest.split_once('/') {
            Some((id, part)) => (id, Some(part)),
            None => (rest, None),
        };
        let agent = AgentRef::from_rest(id)?.0;
        let part = match part.map(|part| part.split_once('/').unwrap_or((part, ""))) {
            None => Part::Transcript,
            Some(("shell", "")) => Part::Shell,
            Some(("term", n)) => Part::Terminal(n.parse().ok()?),
            Some(("desktop", name)) => return Self::desktop(agent, name.to_owned()),
            Some(_) => return None,
        };
        Some(Self { agent, part })
    }
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Part::Transcript => f.write_str("transcript"),
            Part::Terminal(terminal) => write!(f, "term {terminal}"),
            Part::Shell => f.write_str("shell"),
            Part::Desktop(name) => write!(f, "desktop {name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_frame::Address;

    use super::*;

    fn agent() -> AgentId {
        AgentId::from_counter(7, &rho_agent_types::AgentIdDomain(7)).unwrap()
    }

    #[test]
    fn every_part_reads_back() {
        let id = AgentRef(agent()).rest();
        for (address, rest) in [
            (AgentAddress::transcript(agent()), id.clone()),
            (AgentAddress::terminal(agent(), 3), format!("{id}/term/3")),
            (AgentAddress::shell(agent()), format!("{id}/shell")),
            (
                AgentAddress::desktop(agent(), "browser".into()).unwrap(),
                format!("{id}/desktop/browser"),
            ),
        ] {
            assert_eq!(address.address().key(), format!("agent:{rest}"));
            let read =
                AgentAddress::from_address(&Address::parse(&format!("agent:{rest}")).unwrap());
            assert_eq!(read, Some(address));
        }
    }

    #[test]
    fn the_plain_form_is_the_participants_one() {
        assert_eq!(
            AgentAddress::transcript(agent()).address(),
            AgentRef(agent()).address()
        );
    }

    #[test]
    fn a_part_needs_its_shape() {
        let id = AgentRef(agent()).rest();
        for rest in [
            format!("{id}/"),
            format!("{id}/term"),
            format!("{id}/term/x"),
            format!("{id}/term/1/2"),
            format!("{id}/shell/1"),
            format!("{id}/desktop"),
            format!("{id}/desktop/"),
            format!("{id}/desktop/a/b"),
            format!("{id}/window/1"),
            "nobody/term/1".to_owned(),
            "eng-ht08".to_owned(),
        ] {
            assert_eq!(AgentAddress::from_rest(&rest), None, "{rest}");
        }
        assert_eq!(AgentAddress::desktop(agent(), "a/b".into()), None);
    }
}
