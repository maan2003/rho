//! Opaque append-only byte logs and note slots exchanged by devices and
//! hosts.
use std::collections::BTreeMap;

use senax_encoder::{Decode, Encode, Pack, Unpack};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct DeviceId(pub [u8; 16]);
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct LogId(pub [u8; 16]);
/// One note's place on a host, named so the host cannot tell whose.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct SlotId(pub [u8; 16]);
/// A host's slot store, so a device knows which host's slots it has seen.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct StoreId(pub [u8; 16]);
/// A blob's hash: what a put names as the blob it replaces.
pub type BlobHash = [u8; 32];
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Open;
impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::LedgerLog;
}
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ClientFrame {
    /// What the device holds of each log, and how far it has seen each
    /// store's slots.
    Hello {
        have: BTreeMap<LogId, u64>,
        slots: BTreeMap<StoreId, u64>,
    },
    Append {
        log: LogId,
        at: u64,
        bytes: Vec<u8>,
    },
    /// Replace the slot's blob, only if it still holds `prev`.
    Put {
        slot: SlotId,
        prev: Option<BlobHash>,
        blob: Vec<u8>,
    },
}
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    Lengths {
        store: StoreId,
        logs: BTreeMap<LogId, u64>,
    },
    Bytes {
        log: LogId,
        at: u64,
        bytes: Vec<u8>,
    },
    /// A slot's blob as of `version`: newer than the device has seen, or
    /// the one a refused put did not replace.
    Slot {
        slot: SlotId,
        version: u64,
        blob: Vec<u8>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_frames_round_trip() {
        let log = LogId([9; 16]);
        let frames = [
            ClientFrame::Hello {
                have: BTreeMap::from([(log, 37)]),
                slots: BTreeMap::from([(StoreId([3; 16]), 5)]),
            },
            ClientFrame::Put {
                slot: SlotId([2; 16]),
                prev: Some([1; 32]),
                blob: vec![4],
            },
            ClientFrame::Append {
                log,
                at: 37,
                bytes: vec![0, 9, 0],
            },
        ];
        for frame in frames {
            let encoded = senax_encoder::pack(&frame).unwrap();
            let mut slice: &[u8] = &encoded;
            assert_eq!(
                senax_encoder::unpack::<ClientFrame>(&mut slice).unwrap(),
                frame
            );
        }
        let frame = ServerFrame::Bytes {
            log,
            at: 37,
            bytes: vec![1, 2],
        };
        let encoded = senax_encoder::pack(&frame).unwrap();
        let mut slice: &[u8] = &encoded;
        assert_eq!(
            senax_encoder::unpack::<ServerFrame>(&mut slice).unwrap(),
            frame
        );
    }
}
