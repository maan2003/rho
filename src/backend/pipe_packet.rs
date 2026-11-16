//! Message-pipe routing frame. Tokio/Mio may split a native message at 4 KiB.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

pub(super) const MAX_PACKET: usize = u16::MAX as usize;
pub(super) const TOKEN_BYTES: usize = 32;
pub(super) const HEADER_BYTES: usize = TOKEN_BYTES + 2;

pub(super) struct Packet {
    pub(super) source: [u8; TOKEN_BYTES],
    // bounded: a single QUIC datagram, at most MAX_PACKET bytes.
    pub(super) contents: Vec<u8>,
}

impl Packet {
    pub(super) fn encode(source: [u8; TOKEN_BYTES], contents: &[u8]) -> io::Result<Vec<u8>> {
        let len = u16::try_from(contents.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "oversized pipe packet"))?;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty pipe packet",
            ));
        }
        // bounded: HEADER_BYTES + MAX_PACKET.
        let mut bytes = Vec::with_capacity(HEADER_BYTES + contents.len());
        bytes.extend_from_slice(&len.to_be_bytes());
        bytes.extend_from_slice(&source);
        bytes.extend_from_slice(contents);
        Ok(bytes)
    }

    pub(super) async fn read(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Self> {
        let mut header = [0; HEADER_BYTES];
        reader.read_exact(&mut header).await?;
        let len = u16::from_be_bytes([header[0], header[1]]) as usize;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty pipe packet",
            ));
        }
        let mut contents = vec![0; len];
        reader.read_exact(&mut contents).await?;
        Ok(Self {
            source: header[2..].try_into().expect("fixed routing token"),
            contents,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn split_native_message_preserves_datagrams() {
        let (mut write, mut read) = tokio::io::duplex(7);
        let writer = tokio::spawn(async move {
            for size in [8192, 1200] {
                let bytes = Packet::encode([7; TOKEN_BYTES], &vec![0xa5; size]).unwrap();
                write.write_all(&bytes).await.unwrap();
            }
        });
        for size in [8192, 1200] {
            let packet = Packet::read(&mut read).await.unwrap();
            assert_eq!(packet.source, [7; TOKEN_BYTES]);
            assert_eq!(packet.contents, vec![0xa5; size]);
        }
        writer.await.unwrap();
    }

    #[test]
    fn reject_empty_and_oversized_messages() {
        assert!(Packet::encode([0; TOKEN_BYTES], &[]).is_err());
        assert!(Packet::encode([0; TOKEN_BYTES], &vec![0; MAX_PACKET + 1]).is_err());
    }
}
