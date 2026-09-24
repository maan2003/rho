//! The desk stream: one GUI's replica of the desk, kept in step with a
//! host's copy.
//!
//! A stream of its own, opened by [`rho_rpc::protocol::Protocol::Desk`], so the
//! desk's handshake and its writes belong to the desk client alone and a
//! whole-store answer never queues ahead of anything else the host says.
//! Every frame after the opening one is a [`ClientFrame`] or a
//! [`ServerFrame`].
//!
//! The desk is the client's; the host holds a copy so that clients can sync
//! through it. So nothing here is a request with an answer except
//! [`ClientFrame::Sync`]: writes are made on the client when the user makes
//! them, and the host pokes every stream when its copy moves.

use std::collections::BTreeMap;

use senax_encoder::{Pack, Unpack};

use super::cells::{BodySnapshot, BodyVersion, CellMutation, DeviceId, Id, Snapshot, Version};
use super::{TextOperation, TextTransaction};

/// What a client says on its desk stream.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ClientFrame {
    /// Binds this stream to `device` and asks for what the client lacks.
    /// The host answers exactly one [`ServerFrame::Synced`]. A stream that
    /// has not synced may not write.
    Sync {
        device: DeviceId,
        known: Version,
        /// Which store the client counted `known` in, when it holds a
        /// replica at all. A version is a count of writes per device inside
        /// one store; carried to another store the same numbers name writes
        /// that never happened. So a client that comes back holding one
        /// says whose numbers these are, and a host that does not
        /// recognise the name answers with the whole store rather than a
        /// difference from a number that was never its own.
        store: Option<DeviceId>,
        /// How much of each note's text the client already holds, by note.
        /// The host answers with the operations these lack and leaves out
        /// the bodies with nothing new in them; a note missing from the map
        /// is one the client has never held, and comes whole.
        bodies: BTreeMap<Id, BodyVersion>,
    },
    /// The client's half of a sync: the cells it holds that the host's
    /// frontier does not cover. The store is the client's, so the host
    /// catches up from it the same way it is caught up from.
    CellsApply {
        cells: Snapshot,
    },
    MutationApply {
        mutation: CellMutation,
    },
    /// An edit to a note's body, which is the only text the store holds.
    TextApply {
        id: Id,
        operation: TextOperation,
        transaction: Option<TextTransaction>,
    },
}

/// What a host says on a desk stream.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    /// The answer to [`ClientFrame::Sync`].
    Synced {
        /// The store this delta was counted in, so a client holding a
        /// replica can tell whether what it kept is behind this store or
        /// about a different one. When it does not match what the client
        /// holds, `delta` is the whole store, not a difference.
        store: DeviceId,
        node_namespace: u16,
        delta: Snapshot,
        bodies: Vec<BodySnapshot>,
    },
    /// The host's copy moved: a poke, not a delta. The client syncs when
    /// `frontier` is past what it holds.
    CellsAvailable { frontier: Version },
    /// A body edit, from whichever stream made it.
    TextApplied {
        id: Id,
        operation: TextOperation,
        transaction: Option<TextTransaction>,
    },
    /// This stream fell behind the host's pokes and missed some; the client
    /// syncs again rather than trust what it holds is current.
    ResyncRequired,
    /// A newer stream took this one's device: the last frame on it. Two
    /// writers under one device id would collide in the CRDT's per-device
    /// namespace, so the newest wins and the older is told why it ends.
    Displaced,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TreeClock;
    use crate::protocol::cells::{CellWrite, Property, Stamp, Uuid};

    fn round_trips<
        T: senax_encoder::Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
    >(
        frame: T,
    ) {
        let bytes = senax_encoder::pack(&frame).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: T = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn frames_round_trip() {
        let device = DeviceId([7; 16]);
        let id = Id::Note(Uuid([9; 16]));
        let mutation = CellMutation {
            stamp: Stamp {
                device,
                version: 12,
            },
            writes: vec![CellWrite {
                id: id.clone(),
                property: Property::Labeled {
                    label: Id::Label(Uuid([3; 16])),
                    present: true,
                },
            }],
            verdict: None,
        };
        let operation = TextOperation::Edit {
            timestamp: TreeClock {
                value: 1,
                replica_id: 4,
            },
            version: Vec::new(),
            ranges: vec![(0, 0)],
            new_text: vec!["note".into()],
        };
        round_trips(ClientFrame::Sync {
            device,
            known: Version::from([(device, 11)]),
            store: Some(device),
            bodies: BTreeMap::from([(id.clone(), BodyVersion::from([(4, 1)]))]),
        });
        round_trips(ClientFrame::MutationApply { mutation });
        round_trips(ClientFrame::TextApply {
            id: id.clone(),
            operation: operation.clone(),
            transaction: None,
        });
        round_trips(ServerFrame::Synced {
            store: device,
            node_namespace: 4,
            delta: Snapshot::default(),
            bodies: Vec::new(),
        });
        round_trips(ServerFrame::TextApplied {
            id,
            operation,
            transaction: None,
        });
        round_trips(ServerFrame::Displaced);
    }
}
