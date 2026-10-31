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

/// How an input wakes the agent it reaches (`wake.rs` decides when).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum InputWake {
    /// A person is waiting: soon, and it revives a stopped agent.
    Person,
    /// Another agent wrote.
    Agent,
    /// Rides along with the next request.
    Quiet,
}

/// String properties Python sets on an agent. Rust reads none of them; the
/// GUI gives meaning to the keys in [`props`].
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
    /// The directory a label shows.
    pub const CWD: &str = "cwd";
}
