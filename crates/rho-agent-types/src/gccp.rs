//! The names GCCP gives things ([docs/gccp.md](../../../docs/gccp.md)): the
//! endpoint a harness is reached at, a recipient inside it, and the address
//! the two make.

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// A harness's iroh endpoint id: its public key. As text it is 64 lowercase
/// hexadecimal digits.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct EndpointId(pub [u8; 32]);

impl std::str::FromStr for EndpointId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let shape = || format!("an endpoint id is 64 lowercase hex digits, not {text:?}");
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        if text.len() != 64 {
            return Err(shape());
        }
        let mut endpoint = [0; 32];
        for (byte, pair) in endpoint.iter_mut().zip(text.as_bytes().chunks(2)) {
            let (Some(high), Some(low)) = (digit(pair[0]), digit(pair[1])) else {
                return Err(shape());
            };
            *byte = high << 4 | low;
        }
        Ok(Self(endpoint))
    }
}

impl std::fmt::Display for EndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
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

    #[test]
    fn an_address_reads_back_as_it_is_written() {
        let text = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f/x4.b_c-9";
        let address: PeerAddress = text.parse().unwrap();
        assert_eq!(address.endpoint.0[1], 1);
        assert_eq!(address.endpoint.0[31], 0x1f);
        assert_eq!(address.local.as_str(), "x4.b_c-9");
        assert_eq!(address.to_string(), text);
        assert_eq!(text[..64].parse::<EndpointId>().unwrap(), address.endpoint);
    }

    #[test]
    fn an_address_outside_the_grammar_is_no_address() {
        let endpoint = "00".repeat(32);
        for bad in [
            endpoint.clone(),
            format!("{endpoint}/"),
            format!("{endpoint}/Upper"),
            format!("{endpoint}/a/b"),
            format!("{endpoint}/{}", "a".repeat(65)),
            format!("{}/a", "0".repeat(63)),
            format!("{}/a", "0".repeat(66)),
            format!("{}/a", "AB".repeat(32)),
            format!("{}/a", "zz".repeat(32)),
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
