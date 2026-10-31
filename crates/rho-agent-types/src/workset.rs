//! Worksets and mail: the units rho2 keeps state in
//! ([ARCH-rho2](../../../agent-host/specs/ARCH-rho2.md)).

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::{AgentId, ContentPart, SendKind, UnixMs};

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

/// Who mail is from or to.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub enum Address {
    User,
    Agent(AgentId),
}

/// A mail's place in the host's mail log: dense from one, never reused.
/// Zero is "before any mail".
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct MailId(pub u64);

impl MailId {
    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("mail log overflow"))
    }
}

/// One mail, as the mail log keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Mail {
    pub id: MailId,
    pub at: UnixMs,
    pub from: Address,
    pub to: Address,
    /// The mail this one answers.
    pub re: Option<MailId>,
    /// What a mail to the user is for; the dealer ranks on it.
    pub kind: Option<SendKind>,
    pub content: Vec<ContentPart>,
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
    /// `"true"` when the agent belongs in the user's list and the dealer.
    pub const USER_FACING: &str = "user_facing";
    /// The directory a label shows.
    pub const CWD: &str = "cwd";
}
