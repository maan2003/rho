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
