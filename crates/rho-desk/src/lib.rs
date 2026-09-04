//! The words of a note, as a text CRDT, and the ids the store leans on.
//!
//! The convergent movable tree that used to live here is gone: the store
//! holds facts about typed ids (`cells`), and a note's body is the only
//! document mechanics left.

use clock::{Global, Lamport, ReplicaId};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use text::{EditOperation, FullOffset, Operation, UndoOperation};

pub mod cells;

/// Lamport timestamp used by structural operations.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct TreeClock {
    pub value: u32,
    pub replica_id: u16,
}

impl From<Lamport> for TreeClock {
    fn from(value: Lamport) -> Self {
        Self {
            value: value.value,
            replica_id: value.replica_id.as_u16(),
        }
    }
}

impl From<TreeClock> for Lamport {
    fn from(value: TreeClock) -> Self {
        Lamport {
            value: value.value,
            replica_id: ReplicaId::new(value.replica_id),
        }
    }
}

/// Opaque browser-page identity without coupling the document model to the
/// browser crate.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct PageId(pub [u8; 16]);

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TextOperation {
    Edit {
        timestamp: TreeClock,
        version: Vec<TreeClock>,
        ranges: Vec<(u64, u64)>,
        new_text: Vec<String>,
    },
    Undo {
        timestamp: TreeClock,
        version: Vec<TreeClock>,
        counts: Vec<(TreeClock, u32)>,
    },
}

impl TextOperation {
    pub fn timestamp(&self) -> TreeClock {
        match self {
            Self::Edit { timestamp, .. } | Self::Undo { timestamp, .. } => *timestamp,
        }
    }

    pub fn from_text(operation: &Operation) -> Self {
        match operation {
            Operation::Edit(edit) => Self::Edit {
                timestamp: edit.timestamp.into(),
                version: edit
                    .version
                    .iter()
                    .filter(|c| c.value != 0)
                    .map(Into::into)
                    .collect(),
                ranges: edit
                    .ranges
                    .iter()
                    .map(|r| (r.start.0 as u64, r.end.0 as u64))
                    .collect(),
                new_text: edit.new_text.iter().map(ToString::to_string).collect(),
            },
            Operation::Undo(undo) => {
                let mut counts = undo
                    .counts
                    .iter()
                    .map(|(clock, count)| ((*clock).into(), *count))
                    .collect::<Vec<_>>();
                counts.sort_by_key(|(clock, _)| *clock);
                Self::Undo {
                    timestamp: undo.timestamp.into(),
                    version: undo
                        .version
                        .iter()
                        .filter(|c| c.value != 0)
                        .map(Into::into)
                        .collect(),
                    counts,
                }
            }
        }
    }

    pub fn to_text(&self) -> Result<Operation, String> {
        Ok(match self {
            Self::Edit {
                timestamp,
                version,
                ranges,
                new_text,
            } => {
                if ranges.len() != new_text.len() {
                    return Err("Desk node edit range/text count mismatch".into());
                }
                if ranges.len() > 65_536
                    || new_text.iter().map(String::len).sum::<usize>() > 4 * 1024 * 1024
                {
                    return Err("Desk node edit is too large".into());
                }
                Operation::Edit(EditOperation {
                    timestamp: (*timestamp).into(),
                    version: version.iter().copied().map(Into::into).collect::<Global>(),
                    ranges: ranges
                        .iter()
                        .map(|(start, end)| {
                            Ok(FullOffset(
                                usize::try_from(*start)
                                    .map_err(|_| "Desk node edit offset overflow")?,
                            )
                                ..FullOffset(
                                    usize::try_from(*end)
                                        .map_err(|_| "Desk node edit offset overflow")?,
                                ))
                        })
                        .collect::<Result<_, String>>()?,
                    new_text: new_text.iter().map(|text| text.as_str().into()).collect(),
                })
            }
            Self::Undo {
                timestamp,
                version,
                counts,
            } => Operation::Undo(UndoOperation {
                timestamp: (*timestamp).into(),
                version: version.iter().copied().map(Into::into).collect(),
                counts: counts
                    .iter()
                    .map(|(clock, count)| ((*clock).into(), *count))
                    .collect(),
            }),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TextTransaction {
    pub id: TreeClock,
    pub edit_ids: Vec<TreeClock>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_undo_counts_have_a_canonical_wire_order() {
        let operation = text::Operation::Undo(text::UndoOperation {
            timestamp: clock(3, 1).into(),
            version: Global::new(),
            counts: [(clock(2, 2).into(), 1), (clock(1, 1).into(), 1)]
                .into_iter()
                .collect(),
        });
        let TextOperation::Undo { counts, .. } = TextOperation::from_text(&operation) else {
            panic!("undo changed variants")
        };
        assert!(counts.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    fn clock(value: u32, replica_id: u16) -> TreeClock {
        TreeClock { value, replica_id }
    }
}
