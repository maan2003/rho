//! The stable identities of the surfaces a context can show.
//!
//! The viewport itself, and the stack of where the reader has been in it,
//! is `rho_window::history::History` over these keys: one machine, held
//! once per context (`RHO-WINDOW-DESIGN.md`).

use camino::Utf8PathBuf;
use rho_agents::HostId;
use rho_ui_proto::AgentId;

/// Stable identity of a surface, independent of its live view entity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceKey {
    Draft,
    /// The dealer's ranking, read at a glance. Home is where a cold start
    /// and an empty queue land.
    Home,
    Messages,
    /// What the desk has spent: the four usage charts, one screen. Which
    /// chart is showing is the screen's own state and not its identity, so
    /// picking another from the menu redraws this surface rather than
    /// opening a second one.
    Usage,
    DeskNode {
        host: HostId,
        node_id: rho_desk::cells::Id,
    },
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
    Browser(rho_browser::PageId),
    SlackList,
    /// The places one Slack search found. The query is the identity, so a
    /// second search replaces the surface rather than stacking another one
    /// behind it: the results are a way through, not a thing to keep.
    SlackResults {
        query: String,
    },
    /// One Slack conversation. The source is the identity: two threads in
    /// the same channel are two surfaces, and their labels are not unique.
    SlackConversation(rho_slack::session::Source),
    /// A picture, shown full-window. The cached path is the identity; the
    /// title rides along because that path is rho's own bookkeeping and no
    /// reader should be shown it.
    Image {
        path: Utf8PathBuf,
        title: String,
    },
}

impl SurfaceKey {
    /// Conversation content, as opposed to an explicitly opened artifact.
    pub fn is_conversation(&self) -> bool {
        matches!(self, SurfaceKey::Draft | SurfaceKey::Transcript(_))
    }
}
