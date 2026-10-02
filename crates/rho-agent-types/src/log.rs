//! Where things are in an agent's log, and the facts it records that the
//! runtime writes and every reader takes as they are.

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// A position in one agent's log: dense, starting at zero with the
/// agent's creation, never reused. A rewind is told at a new position
/// (`Rewound`) and hides the ones before it; nothing moves.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct AgentPos(#[senax(default)] pub u64);

impl AgentPos {
    pub const ZERO: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("agent log position overflow"))
    }
}

/// One place in a host's journal, the global order of every append on
/// that host. Zero is "before anything".
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct Seq(pub u64);

impl Seq {
    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("journal overflow"))
    }
}

/// What a send to the user is for, as the agent classed it: the dealer
/// ranks the conversation on this alone (`rho-dealer/cases.md`, A1–A6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum SendKind {
    /// Asks the user for something the work needs.
    Ask,
    /// Delivers what the user asked for. Sends from before kinds existed
    /// read as this.
    #[default]
    Result,
    /// Acknowledgement or progress: the agent's status line until any
    /// later message.
    Status,
    /// Worth keeping, but the user need not read it now: in the
    /// conversation, never a card.
    Fyi,
}

/// One field of a sidecar proposal. `Clear` stays distinct from
/// `Unchanged` so a stale label can be dropped without inventing a new one.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum PresentationField {
    Unchanged,
    Set(String),
    Clear,
}
