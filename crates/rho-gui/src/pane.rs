//! A context's single viewport and the stable identities of surfaces it can
//! show.

use camino::Utf8PathBuf;
use rho_ui_proto::AgentId;

use crate::registry::HostId;

/// Stable identity of a surface, independent of its live view entity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceKey {
    Draft,
    Messages,
    DeskNode {
        host: HostId,
        node_id: rho_desk::NodeId,
    },
    Inbox(String),
    Transcript(AgentId),
    File {
        agent_id: AgentId,
        path: Utf8PathBuf,
    },
    Shell(AgentId),
    Diff {
        agent_id: AgentId,
    },
    Terminal {
        agent_id: AgentId,
        terminal_id: u64,
    },
    #[cfg(feature = "native")]
    Browser(rho_browser::PageId),
    ZulipInbox,
    ZulipNarrow {
        label: String,
    },
    #[cfg(feature = "native")]
    SlackList,
    /// One Slack conversation. The source is the identity: two threads in
    /// the same channel are two surfaces, and their labels are not unique.
    #[cfg(feature = "native")]
    SlackConversation(rho_slack::session::Source),
}

impl SurfaceKey {
    /// Conversation content, as opposed to an explicitly opened artifact.
    pub fn is_conversation(&self) -> bool {
        matches!(self, SurfaceKey::Draft | SurfaceKey::Transcript(_))
    }
}

/// The one viewport belonging to a context.
pub struct Pane<S> {
    pub surface: S,
    history: Vec<S>,
}

impl<S: PartialEq> Pane<S> {
    pub fn new(surface: S) -> Self {
        Self {
            surface,
            history: Vec::new(),
        }
    }

    pub fn show(&mut self, surface: S) {
        if surface == self.surface {
            return;
        }
        let previous = std::mem::replace(&mut self.surface, surface);
        self.history.retain(|candidate| *candidate != previous);
        self.history.push(previous);
    }

    pub fn purge_history(&mut self, matches: impl Fn(&S) -> bool) {
        self.history.retain(|surface| !matches(surface));
    }

    pub fn back(&mut self) -> bool {
        match self.history.pop() {
            Some(previous) => {
                self.surface = previous;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(n: u64) -> SurfaceKey {
        let id = AgentId::from_counter(n, &rho_ui_proto::AgentIdDomain(0)).unwrap();
        SurfaceKey::Transcript(id)
    }

    #[test]
    fn history_back() {
        let mut pane = Pane::new(SurfaceKey::Draft);
        pane.show(transcript(1));
        pane.show(transcript(2));
        assert!(pane.back());
        assert_eq!(pane.surface, transcript(1));
        assert!(pane.back());
        assert_eq!(pane.surface, SurfaceKey::Draft);
        assert!(!pane.back());
    }
}
