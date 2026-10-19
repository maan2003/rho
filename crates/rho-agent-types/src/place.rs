//! Where an agent works: its directory, and the workset it is in.

use std::fmt;
use std::str::FromStr;

use bytes::{Buf, BytesMut};
use camino::Utf8PathBuf;
use senax_encoder::{Decode, Decoder, Encode, Encoder, Pack, Packer, Unpack, Unpacker};

/// An agent's place: its working directory as it sees it, and the workset
/// whose view it runs in. An agent with a workset sees it at `/src` and
/// usually works there (`/src/<repo>/...`). An agent without one runs on
/// the host as it is, with no namespace, in a host directory. Stored inline
/// on the agent record.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct Place {
    pub cwd: Utf8PathBuf,
    pub workset: Option<WorksetId>,
}

/// A workset's id: six random bytes, written as twelve lowercase hex
/// digits, which is also the name of its directory. Encoded as that text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorksetId(pub [u8; 6]);

impl fmt::Display for WorksetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for WorksetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WorksetId({self})")
    }
}

impl FromStr for WorksetId {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let digits = text.as_bytes();
        anyhow::ensure!(
            digits.len() == 12
                && digits
                    .iter()
                    .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f')),
            "not a workset id: {text:?}"
        );
        let mut bytes = [0; 6];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
        }
        Ok(Self(bytes))
    }
}

impl Encoder for WorksetId {
    fn encode(&self, writer: &mut BytesMut) -> senax_encoder::Result<()> {
        self.to_string().encode(writer)
    }

    fn is_default(&self) -> bool {
        false
    }
}

impl Decoder for WorksetId {
    fn decode(reader: &mut impl Buf) -> senax_encoder::Result<Self> {
        String::decode(reader)?
            .parse()
            .map_err(|error: anyhow::Error| senax_encoder::EncoderError::Decode(error.to_string()))
    }
}

impl Packer for WorksetId {
    fn pack(&self, writer: &mut BytesMut) -> senax_encoder::Result<()> {
        self.to_string().pack(writer)
    }
}

impl Unpacker for WorksetId {
    fn unpack(reader: &mut impl Buf) -> senax_encoder::Result<Self> {
        String::unpack(reader)?
            .parse()
            .map_err(|error: anyhow::Error| senax_encoder::EncoderError::Decode(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workset_ids_are_twelve_lowercase_hex_digits() {
        let id: WorksetId = "0a1b2c3d4e5f".parse().unwrap();
        assert_eq!(id.0, [0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f]);
        assert_eq!(id.to_string(), "0a1b2c3d4e5f");
        for bad in [
            "0A1B2C3D4E5F",
            "0a1b2c3d4e5",
            "0a1b2c3d4e5f0",
            "adopted-0a1b2c",
            "+a1b2c3d4e5f",
        ] {
            assert!(bad.parse::<WorksetId>().is_err(), "{bad}");
        }
    }

    /// Records written when the workset was a `String` and an `origin` sat
    /// beside it decode unchanged.
    #[test]
    fn places_written_before_the_id_type_decode() {
        #[derive(Encode)]
        struct Before {
            workset: String,
            cwd: Utf8PathBuf,
            origin: Option<Utf8PathBuf>,
        }
        let bytes = senax_encoder::encode(&Before {
            workset: "0a1b2c3d4e5f".into(),
            cwd: "/src/rho".into(),
            origin: Some("git@github.com:o/rho".into()),
        })
        .unwrap();
        let place: Place = senax_encoder::decode(&mut bytes.as_ref()).unwrap();
        assert_eq!(
            place,
            Place {
                cwd: "/src/rho".into(),
                workset: Some("0a1b2c3d4e5f".parse().unwrap()),
            }
        );
        let host = senax_encoder::encode(&Place {
            cwd: "/home/u".into(),
            workset: None,
        })
        .unwrap();
        let place: Place = senax_encoder::decode(&mut host.as_ref()).unwrap();
        assert_eq!(place.workset, None);
    }
}
