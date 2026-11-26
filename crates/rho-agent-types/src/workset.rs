//! Worksets: the units the agent host keeps state in
//! ([ARCH-agent-host](../../../agent-host/specs/ARCH-agent-host.md)).

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// How a workset's process sees the filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum WorksetKind {
    /// A private `/src`, empty at creation, in a namespace of its own.
    Isolated,
    /// The host as it is, with no namespace, starting in `cwd`.
    Host { cwd: Utf8PathBuf },
}

/// A position in one workset's log: dense from zero, never reused.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct Wpos(pub u64);

impl Wpos {
    pub const ZERO: Self = Self(0);

    pub fn next(self) -> Self {
        Self(
            self.0
                .checked_add(1)
                .expect("workset log position overflow"),
        )
    }
}

/// How far `rewind agent` takes an agent back: its last turns, or every
/// turn that started after a step of its log (`rewind agent to here`).
/// Which requests start a turn is its runtime's to say; each lists their
/// starts, oldest first, and this picks the first taken back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Rewind {
    Turns(u32),
    After(Wpos),
}

impl Rewind {
    /// Of `starts`, the turns' first rows oldest first, the index of the
    /// first turn taken back; `None` when there is none to take back.
    pub fn first_taken(self, starts: &[Wpos]) -> Option<usize> {
        match self {
            Self::Turns(0) => None,
            Self::Turns(turns) => {
                (!starts.is_empty()).then(|| starts.len().saturating_sub(turns as usize))
            }
            Self::After(step) => starts.iter().position(|start| *start > step),
        }
    }
}

/// Who speaks in an input: where the model reads it and how soon it wakes for
/// it ([DECISION-roles](../../../agent-host/rho-agent/specs/DECISION-roles.
/// md)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Role {
    /// A person typed it. It wakes the agent soon and revives a stopped one.
    User,
    /// Anything no person typed and Rho did not write, such as another agent's
    /// message. The model reads it as a tool's output.
    Tool,
    /// Rho's own words. It wakes nobody and rides along with the next request.
    System,
}

/// String properties Python sets on an agent or a workset. On an agent's,
/// the GUI gives meaning to the keys in [`props`].
pub type Props = BTreeMap<String, String>;

/// The prop keys the GUI reads.
pub mod props {
    /// The agent's name for a reader.
    pub const NAME: &str = "name";
    /// A generated title, when there is no name. Present once naming was
    /// attempted, empty when it found none.
    pub const TITLE: &str = "title";
    /// The encoded id of the agent this one answers to.
    pub const PARENT: &str = "parent";
    /// The chat message the agent was started from, as `chat:<seq>`.
    pub const ORIGIN: &str = "origin";
    /// The directory the agent works in, as it sees it: where its
    /// notebook starts, and where a terminal or shell beside it starts.
    pub const CWD: &str = "cwd";
    /// The repository the agent's directory was cloned from, as the user
    /// named it: a URL or a path on the host.
    pub const REPO: &str = "repo";
}

/// Where an agent works, as its props say: the `cwd` prop, when set. An
/// agent without one works where its workset starts.
pub fn agent_cwd(props: &Props) -> Option<&camino::Utf8Path> {
    props
        .get(props::CWD)
        .map(|cwd| cwd.trim())
        .filter(|cwd| !cwd.is_empty())
        .map(camino::Utf8Path::new)
}
