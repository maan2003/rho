//! What a device and a host say about the ledger,
//! [`rho_rpc::protocol::Protocol::Ledger`].
//!
//! A host keeps, for every device, that device's segments from its latest
//! base on: a base holds all of a device's entries, so nothing before it is
//! needed. A device says what it has already read and hears everything
//! after that, then every segment any device puts while the stream lasts.

use std::collections::BTreeMap;

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// One of the user's devices, as the ledger knows it. Its own id, kept in
/// the device's database: not the key it authenticates to hosts with, so
/// a device can be enrolled with a host however the host allows.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct DeviceId(pub [u8; 16]);

/// Opens a ledger stream.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Open;

impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Ledger;
}

/// A run of one device's entries, sealed. `seq` counts the device's
/// segments and only grows; a `base` holds every entry the device has, so
/// a host drops what came before it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Segment {
    pub seq: u64,
    pub base: bool,
    pub sealed: Vec<u8>,
}

/// What a device says on its ledger stream.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ClientFrame {
    /// The first frame: the newest segment already read of every device.
    /// The host answers [`ServerFrame::Heads`], then the segments after
    /// these, then every segment put while the stream lasts.
    Hello { known: BTreeMap<DeviceId, u64> },
    /// A segment of the device's own, for the host to keep and pass on.
    Put { device: DeviceId, segment: Segment },
}

/// What a host says on a ledger stream.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    /// The newest segment the host holds of every device, so a device can
    /// tell whether the host has its own.
    Heads { heads: BTreeMap<DeviceId, u64> },
    /// Segments of one device, in order.
    Segments {
        device: DeviceId,
        segments: Vec<Segment>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trips<T>(frame: T)
    where
        T: senax_encoder::Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
    {
        let bytes = senax_encoder::pack(&frame).unwrap();
        let mut slice: &[u8] = &bytes;
        assert_eq!(senax_encoder::unpack::<T>(&mut slice).unwrap(), frame);
    }

    #[test]
    fn frames_round_trip() {
        let device = DeviceId([3; 16]);
        let segment = Segment {
            seq: 4,
            base: true,
            sealed: vec![1, 2, 3],
        };
        round_trips(ClientFrame::Hello {
            known: BTreeMap::from([(device, 2)]),
        });
        round_trips(ClientFrame::Put {
            device,
            segment: segment.clone(),
        });
        round_trips(ServerFrame::Heads {
            heads: BTreeMap::from([(device, 4)]),
        });
        round_trips(ServerFrame::Segments {
            device,
            segments: vec![segment],
        });
    }
}
