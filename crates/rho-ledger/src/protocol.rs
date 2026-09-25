//! Opaque append-only byte logs exchanged by devices and hosts.
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
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Open;
impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::LedgerLog;
}
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ClientFrame {
    Hello { have: BTreeMap<LogId, u64> },
    Append { log: LogId, at: u64, bytes: Vec<u8> },
}
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    Lengths { logs: BTreeMap<LogId, u64> },
    Bytes { log: LogId, at: u64, bytes: Vec<u8> },
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
