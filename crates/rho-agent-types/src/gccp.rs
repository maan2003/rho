//! The names GCCP gives things ([docs/gccp.md](../../../docs/gccp.md)): the
//! endpoint a harness is reached at, a recipient inside it, and the address
//! the two make.

use std::fmt::Write as _;

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// An iroh endpoint id: its public key. As text, everywhere rho shows or
/// reads one, it is z-base-32 as pkarr and iroh's `to_z32` write it: 52 of
/// [`Z32`], most significant bit first, the last 4 bits zero.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct EndpointId(pub [u8; 32]);

/// The z-base-32 alphabet: each symbol stands for its index.
const Z32: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";
/// The symbols of an endpoint id: 256 bits, 5 to a symbol.
const Z32_LEN: usize = 52;

impl std::str::FromStr for EndpointId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let shape = || format!("an endpoint id is {Z32_LEN} of z-base-32, not {text:?}");
        if text.len() != Z32_LEN {
            return Err(shape());
        }
        let mut endpoint = [0; 32];
        let (mut bits, mut held, mut at) = (0u32, 0u32, 0);
        for symbol in text.bytes() {
            let value = Z32.iter().position(|&z| z == symbol).ok_or_else(shape)?;
            bits = bits << 5 | value as u32;
            held += 5;
            if held >= 8 {
                held -= 8;
                endpoint[at] = (bits >> held) as u8;
                at += 1;
            }
        }
        // One text for each id: the bits past the last byte are zero.
        if bits & ((1 << held) - 1) != 0 {
            return Err(shape());
        }
        Ok(Self(endpoint))
    }
}

impl std::fmt::Display for EndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (mut bits, mut held) = (0u32, 0u32);
        let mut symbol = |value: u32| f.write_char(char::from(Z32[(value & 31) as usize]));
        for byte in self.0 {
            bits = bits << 8 | u32::from(byte);
            held += 8;
            while held >= 5 {
                held -= 5;
                symbol(bits >> held)?;
            }
        }
        if held > 0 {
            symbol(bits << (5 - held))?;
        }
        Ok(())
    }
}

/// The name of a recipient inside a harness: 1 to 64 bytes of `a-z`, `0-9`,
/// `.`, `_` and `-`. Only the harness that owns it gives it meaning.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub struct LocalId(String);

impl LocalId {
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for LocalId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let allowed = |byte: u8| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        };
        if text.is_empty() || text.len() > Self::MAX_LEN || !text.bytes().all(allowed) {
            return Err(format!(
                "a local id is 1 to {} of a-z, 0-9, '.', '_' and '-', not {text:?}",
                Self::MAX_LEN
            ));
        }
        Ok(Self(text.to_owned()))
    }
}

impl std::fmt::Display for LocalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A GCCP address: a recipient on a harness, here or elsewhere. As text it is
/// `<endpoint id>/<local id>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub struct PeerAddress {
    pub endpoint: EndpointId,
    pub local: LocalId,
}

impl std::str::FromStr for PeerAddress {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (endpoint, local) = text
            .split_once('/')
            .ok_or_else(|| format!("an address is <endpoint id>/<local id>, not {text:?}"))?;
        Ok(Self {
            endpoint: endpoint.parse()?,
            local: local.parse()?,
        })
    }
}

impl std::fmt::Display for PeerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.endpoint, self.local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes 0 to 31 in z-base-32, as an encoder apart from this one
    /// writes them.
    const COUNTING: &str = "yyyoryarywdyqnyjbefoadeqbhebnrounoktcfaadrpbs8y7daxo";

    #[test]
    fn an_address_reads_back_as_it_is_written() {
        let counting = EndpointId(std::array::from_fn(|n| n as u8));
        assert_eq!(counting.to_string(), COUNTING);
        let text = format!("{COUNTING}/x4.b_c-9");
        let address: PeerAddress = text.parse().unwrap();
        assert_eq!(address.endpoint, counting);
        assert_eq!(address.local.as_str(), "x4.b_c-9");
        assert_eq!(address.to_string(), text);
        for endpoint in [[0; 32], [0xff; 32], [0xa1; 32]] {
            let endpoint = EndpointId(endpoint);
            let written = endpoint.to_string();
            assert_eq!(written.len(), 52);
            assert_eq!(written.parse(), Ok(endpoint));
        }
        assert_eq!(EndpointId([0; 32]).to_string(), "y".repeat(52));
    }

    #[test]
    fn an_address_outside_the_grammar_is_no_address() {
        let endpoint = "y".repeat(52);
        for bad in [
            endpoint.clone(),
            format!("{endpoint}/"),
            format!("{endpoint}/Upper"),
            format!("{endpoint}/a/b"),
            format!("{endpoint}/{}", "a".repeat(65)),
            format!("{}/a", "y".repeat(51)),
            format!("{}/a", "y".repeat(53)),
            format!("{}/a", "Y".repeat(52)),
            // Not in the alphabet: z-base-32 has no 0, 2 or l.
            format!("{}0/a", "y".repeat(51)),
            format!("{}l/a", "y".repeat(51)),
            // The bits past the last byte are not zero: `b` is 00001.
            format!("{}b/a", "y".repeat(51)),
            // The hex that rho wrote before.
            format!("{}/a", "00".repeat(32)),
        ] {
            assert!(bad.parse::<PeerAddress>().is_err(), "{bad}");
        }
        assert!(
            format!("{endpoint}/{}", "a".repeat(64))
                .parse::<PeerAddress>()
                .is_ok()
        );
        assert!(format!("{endpoint}/a").parse::<EndpointId>().is_err());
    }
}
