#![recursion_limit = "256"]

//! Shared MoQ framing and optional VP9 codec, without compositor dependencies.
pub const MAX_PACKET: usize = 16 * 1024 * 1024;
#[cfg(any(feature = "encoder", feature = "decoder"))]
pub mod codec;
pub mod media;

/// Receiver-budgeted production, before frames acquire codec dependencies.
pub mod sender;

/// A checkpoint changes the durable VP9 reference; a state never changes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameKind {
    Key,
    Checkpoint,
    State,
}

/// Dependency identity carried independently of MoQ's stream/group sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub kind: FrameKind,
    /// Timestamp of the keyframe starting this decoder lifetime.
    pub epoch: u64,
    /// Timestamp of the checkpoint needed before decoding this picture; zero
    /// for a key.
    pub base: u64,
}
impl Header {
    pub const SIZE: usize = 17;

    pub fn pack(self, payload: bytes::Bytes) -> bytes::Bytes {
        let mut data = bytes::BytesMut::with_capacity(Self::SIZE + payload.len());
        data.extend_from_slice(&[match self.kind {
            FrameKind::Key => 0,
            FrameKind::Checkpoint => 1,
            FrameKind::State => 2,
        }]);
        data.extend_from_slice(&self.epoch.to_be_bytes());
        data.extend_from_slice(&self.base.to_be_bytes());
        data.extend_from_slice(&payload);
        data.freeze()
    }

    pub fn unpack(mut data: bytes::Bytes) -> anyhow::Result<(Self, bytes::Bytes)> {
        anyhow::ensure!(
            data.len() >= Self::SIZE,
            "truncated video dependency header"
        );
        let kind = match data[0] {
            0 => FrameKind::Key,
            1 => FrameKind::Checkpoint,
            2 => FrameKind::State,
            _ => anyhow::bail!("invalid video frame kind"),
        };
        let epoch = u64::from_be_bytes(data[1..9].try_into().unwrap());
        let base = u64::from_be_bytes(data[9..17].try_into().unwrap());
        anyhow::ensure!(
            epoch > 0
                && if kind == FrameKind::Key {
                    base == 0
                } else {
                    base >= epoch
                },
            "invalid video dependency identity"
        );
        let payload = data.split_off(Self::SIZE);
        Ok((Self { kind, epoch, base }, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependency_header_round_trips_and_rejects_unknown_kinds() {
        let header = Header {
            kind: FrameKind::Checkpoint,
            epoch: 731,
            base: 1809,
        };
        let data = header.pack(bytes::Bytes::from_static(b"vp9"));
        assert_eq!(
            Header::unpack(data.clone()).unwrap(),
            (header, bytes::Bytes::from_static(b"vp9"))
        );
        assert!(Header::unpack(data.slice(..16)).is_err());
        let mut invalid = data.to_vec();
        invalid[0] = 4;
        assert!(Header::unpack(invalid.into()).is_err());
    }
}
