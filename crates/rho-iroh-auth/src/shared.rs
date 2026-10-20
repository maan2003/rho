use std::fmt;

use iroh::EndpointId;
use iroh::endpoint::Connection;
use sha2::{Digest as _, Sha512};

const ENROLLMENT_CODE_LABEL: &[u8] = b"rho-iroh-auth enrollment code v2";
const TLS_EXPORTER_LABEL: &[u8] = b"rho-iroh-auth tls exporter v2";

const ENROLLMENT_CODE_CHARS: usize = 10;
const ENROLLMENT_CODE_BITS: usize = ENROLLMENT_CODE_CHARS * 5;
const ENROLLMENT_CODE_MASK: u64 = (1u64 << ENROLLMENT_CODE_BITS) - 1;
const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// A fixed-length, human-entered Crockford Base32 enrollment code.
///
/// Private representation invariant: the `u64` contains exactly the 50 bits
/// displayed as 10 Crockford Base32 symbols. The upper 14 bits are always zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EnrollmentCode(u64);

impl EnrollmentCode {
    fn from_bits(value: u64) -> Self {
        assert!(
            value <= ENROLLMENT_CODE_MASK,
            "enrollment code exceeds 50 bits"
        );
        Self(value)
    }
}

impl fmt::Display for EnrollmentCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_assert!(self.0 <= ENROLLMENT_CODE_MASK);
        for index in 0..ENROLLMENT_CODE_CHARS {
            if index == 5 {
                f.write_str("-")?;
            }
            let shift = (ENROLLMENT_CODE_CHARS - index - 1) * 5;
            let symbol = ((self.0 >> shift) & 0b1_1111) as usize;
            f.write_str(std::str::from_utf8(&CROCKFORD[symbol..symbol + 1]).expect("ascii"))?;
        }
        Ok(())
    }
}

impl std::str::FromStr for EnrollmentCode {
    type Err = ParseEnrollmentCodeError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut value = 0u64;
        let mut chars = 0usize;
        for ch in input.chars() {
            if ch == '-' || ch.is_whitespace() {
                continue;
            }
            let ch = ch.to_ascii_lowercase();
            let ch = match ch {
                'o' => '0',
                'i' | 'l' => '1',
                other => other,
            };
            let Some(symbol) = CROCKFORD.iter().position(|byte| *byte == ch as u8) else {
                return Err(ParseEnrollmentCodeError);
            };
            chars += 1;
            if chars > ENROLLMENT_CODE_CHARS {
                return Err(ParseEnrollmentCodeError);
            }
            value = (value << 5) | symbol as u64;
        }
        if chars != ENROLLMENT_CODE_CHARS {
            return Err(ParseEnrollmentCodeError);
        }
        Ok(Self::from_bits(value))
    }
}

/// Invalid enrollment-code text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseEnrollmentCodeError;

impl fmt::Display for ParseEnrollmentCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid enrollment code")
    }
}

impl std::error::Error for ParseEnrollmentCodeError {}

/// A one-time secret that pairs a client with an agent host: whoever
/// shows it in the auth exchange is trusted, once, within its lifetime.
/// Printed as 64 hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PairingKey([u8; 32]);

impl PairingKey {
    pub fn generate() -> Self {
        use rand::TryRng as _;
        let mut bytes = [0; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .expect("system entropy");
        Self(bytes)
    }

    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Never printed by accident: a key in a log is a key given away.
impl fmt::Debug for PairingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingKey(..)")
    }
}

impl fmt::Display for PairingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl std::str::FromStr for PairingKey {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let digits = input.as_bytes();
        if digits.len() != 64 || !digits.iter().all(u8::is_ascii_hexdigit) {
            return Err("a pairing key is 64 hex digits".to_owned());
        }
        let mut bytes = [0; 32];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
            let pair = std::str::from_utf8(pair).expect("ascii");
            *byte = u8::from_str_radix(pair, 16).expect("hex digits");
        }
        Ok(Self(bytes))
    }
}

/// What an agent host prints for a client to pair with it:
/// `rho://<endpoint-id>?key=<pairing-key>`. The endpoint id is all iroh
/// needs to find the host. The link names nothing: the client asks its user
/// what to call the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingLink {
    pub endpoint_id: EndpointId,
    pub key: PairingKey,
}

pub const PAIRING_SCHEME: &str = "rho://";

impl fmt::Display for PairingLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PAIRING_SCHEME}{}?key={}", self.endpoint_id, self.key)
    }
}

impl std::str::FromStr for PairingLink {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let rest = input
            .trim()
            .strip_prefix(PAIRING_SCHEME)
            .ok_or_else(|| format!("a pairing link starts with {PAIRING_SCHEME}"))?;
        let (endpoint_id, query) = rest.split_once('?').ok_or("a pairing link has a key")?;
        let endpoint_id = endpoint_id
            .trim_end_matches('/')
            .parse()
            .map_err(|error| format!("invalid endpoint id in pairing link: {error}"))?;
        let mut key = None;
        // Other parameters are room for what a later host adds.
        for pair in query.split('&') {
            if let Some(("key", value)) = pair.split_once('=') {
                key = Some(value.parse()?);
            }
        }
        Ok(Self {
            endpoint_id,
            key: key.ok_or("a pairing link has a key")?,
        })
    }
}

pub(crate) fn enrollment_code(
    conn: &Connection,
    server_endpoint_id: EndpointId,
    client_endpoint_id: EndpointId,
) -> EnrollmentCode {
    let mut exporter = [0u8; 32];
    let context = exporter_context(server_endpoint_id, client_endpoint_id);
    conn.export_keying_material(&mut exporter, TLS_EXPORTER_LABEL, &context)
        .expect("32-byte iroh enrollment TLS exporter");

    let mut hasher = Sha512::new();
    hash_len_prefixed(&mut hasher, ENROLLMENT_CODE_LABEL);
    hash_len_prefixed(&mut hasher, server_endpoint_id.as_bytes());
    hash_len_prefixed(&mut hasher, client_endpoint_id.as_bytes());
    hash_len_prefixed(&mut hasher, &exporter);
    let digest = hasher.finalize();
    let first_64 = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
    EnrollmentCode::from_bits(first_64 >> 14)
}

fn exporter_context(server_endpoint_id: EndpointId, client_endpoint_id: EndpointId) -> Vec<u8> {
    let mut context = Vec::with_capacity(16 + 32 + 32);
    context.extend_from_slice(b"server");
    context.extend_from_slice(server_endpoint_id.as_bytes());
    context.extend_from_slice(b"client");
    context.extend_from_slice(client_endpoint_id.as_bytes());
    context
}

fn hash_len_prefixed(hasher: &mut Sha512, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::str::FromStr;

    use super::*;

    #[test]
    fn code_parsing_accepts_human_variants() {
        let code = EnrollmentCode::from_str("abcd efgh il").unwrap();
        assert_eq!(code.to_string(), "abcde-fgh11");
        assert!(EnrollmentCode::from_str("too-short").is_err());
    }

    #[test]
    fn code_display_is_grouped_crockford() {
        let code = EnrollmentCode::from_str("ABCD-EFGH-JK").unwrap();
        assert_eq!(code.to_string(), "abcde-fghjk");
        let chars = code.to_string().replace('-', "").into_bytes();
        assert_eq!(chars.len(), ENROLLMENT_CODE_CHARS);
        assert!(chars.iter().all(|ch| CROCKFORD.contains(ch)));
    }

    #[test]
    fn parser_rejects_non_crockford_and_wrong_lengths() {
        let invalid = ["ABCD-EFGH-J!", "ABCD-EFGH-JK2", "ABCD-EFGH-J"];
        let rejected = invalid
            .into_iter()
            .filter(|code| EnrollmentCode::from_str(code).is_err())
            .collect::<HashSet<_>>();
        assert_eq!(rejected.len(), invalid.len());
    }
}

#[cfg(test)]
mod pairing_tests {
    use super::*;

    #[test]
    fn a_link_reads_back_as_it_was_printed() {
        let link = PairingLink {
            endpoint_id: iroh::SecretKey::from_bytes(&[3; 32]).public(),
            key: PairingKey::from_bytes([0xa5; 32]),
        };
        let text = link.to_string();
        assert!(text.starts_with("rho://"), "{text}");
        assert!(text.contains(&"a5".repeat(32)), "{text}");
        assert_eq!(text.parse::<PairingLink>().unwrap(), link);
        // A later host may add parameters, in any order.
        let reordered = format!("rho://{}?v=2&key={}", link.endpoint_id, link.key);
        assert_eq!(reordered.parse::<PairingLink>().unwrap(), link);
    }

    #[test]
    fn a_link_without_its_key_or_with_a_bad_one_is_refused() {
        let id = iroh::SecretKey::from_bytes(&[3; 32]).public();
        for text in [
            format!("rho://{id}?v=2"),
            format!("rho://{id}?key=abc"),
            format!("rho://{id}?key={}", "g".repeat(64)),
            format!("https://{id}?key={}", "0".repeat(64)),
        ] {
            assert!(text.parse::<PairingLink>().is_err(), "{text}");
        }
    }

    #[test]
    fn a_key_never_shows_in_debug_output() {
        let key = PairingKey::from_bytes([0xa5; 32]);
        assert!(!format!("{key:?}").contains("a5"));
    }
}
