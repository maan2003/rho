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

/// What a turn asks of the person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentWant {
    /// Something concrete to look at.
    Show,
    /// Something only the person can give: a decision, or an act.
    Ask,
    /// The person asked a question and this reply answers it.
    Answer,
}

/// How a turn stopped.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Errored { message: String },
}

/// A turn beginning or ending.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TurnEdge {
    Started,
    Ended(TurnOutcome),
}

/// One field of a sidecar proposal. `Clear` stays distinct from
/// `Unchanged` so a stale label can be dropped without inventing a new one.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum PresentationField {
    Unchanged,
    Set(String),
    Clear,
}
