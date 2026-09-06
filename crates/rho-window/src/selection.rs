//! Which pane the point is in.
//!
//! The window keeps one point across every screen: the user is in a draft,
//! or in an agent, or has not landed anywhere yet. Nothing about what those
//! panes show lives here — the map answers which agents exist and in what
//! order, and hands the answer back to the window, which is what says which
//! of them the point is in.
//!
//! The panes named here are today's: the startup screen, the draft and an
//! agent. As the desk and Slack screens move out of the workspace this
//! grows their panes too; it is the window's list, not the agents crate's.

use rho_core::AgentId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActivePane {
    #[default]
    Startup,
    Draft,
    Agent(AgentId),
}

/// The point, as the window holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    active: ActivePane,
}

impl Selection {
    pub fn active_pane(&self) -> ActivePane {
        self.active
    }

    pub fn selected_agent(&self) -> Option<AgentId> {
        match self.active {
            ActivePane::Agent(agent_id) => Some(agent_id),
            ActivePane::Startup | ActivePane::Draft => None,
        }
    }

    pub fn select_agent(&mut self, agent_id: AgentId) {
        self.active = ActivePane::Agent(agent_id);
    }

    pub fn enter_draft(&mut self) {
        self.active = ActivePane::Draft;
    }

    /// The agent the point was in is gone. The point cannot stay on
    /// something that no longer exists, so it falls back to the draft;
    /// says whether it moved, so a caller redraws only then.
    pub fn forget(&mut self, gone: impl Fn(AgentId) -> bool) -> bool {
        match self.active {
            ActivePane::Agent(agent_id) if gone(agent_id) => {
                self.active = ActivePane::Draft;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_core::AgentIdDomain;

    use super::*;

    fn agent(nth: u64) -> AgentId {
        AgentId::from_counter(nth, &AgentIdDomain(0)).unwrap()
    }

    #[test]
    fn the_point_leaves_an_agent_that_is_gone() {
        let mut selection = Selection::default();
        assert_eq!(selection.active_pane(), ActivePane::Startup);
        assert_eq!(selection.selected_agent(), None);

        selection.select_agent(agent(1));
        assert_eq!(selection.selected_agent(), Some(agent(1)));

        // Another agent departing leaves the point where it is.
        assert!(!selection.forget(|id| id == agent(2)));
        assert_eq!(selection.selected_agent(), Some(agent(1)));

        assert!(selection.forget(|id| id == agent(1)));
        assert_eq!(selection.active_pane(), ActivePane::Draft);
        // Nothing to forget twice.
        assert!(!selection.forget(|_| true));
    }
}
