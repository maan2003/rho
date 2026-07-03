//! Fixed-length counter IDs with short dynamic prefixes.
//!
//! `PrefixId` maps a monotonically increasing counter to an 8-character
//! lowercase alphanumeric (`a-z0-9`) ID. Properties:
//!
//! - **Short unique prefixes**: the low-order base36 counter digits are emitted
//!   first, so the first `36^k` generated IDs are all distinguishable by their
//!   first `k` characters (36 IDs by 1 character, ~1.3k by 2, ~47k by 3).
//!   Prefixes are not stored in the ID; [`PrefixId::from_prefix`] resolves one
//!   arithmetically against the first `total_generated` IDs.
//! - **No visible ordering**: each digit is shifted by a hash of the preceding
//!   characters and scrambled through exponentiation in `GF(37)`, so
//!   consecutive counters produce unrelated-looking IDs. The mapping is still
//!   deterministic — the first character cycles through a fixed (scrambled)
//!   order with period 36, which is unavoidable given the prefix-uniqueness
//!   guarantee.
//! - **Domain separation**: encoding is keyed by [`PrefixIdDomain`], so the
//!   same counter encodes differently across ID families.
//! - **Stateless**: counter and ID convert in both directions with no lookup
//!   table; only the raw counter is stored (fixed-width `u64` in redb).

use std::fmt;
use std::hash::Hasher;
use std::marker::PhantomData;

const ALPHABET: &[u8; BASE as usize] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const BASE: u64 = 36;

/// A primitive root modulo 37, so its powers visit every nonzero residue.
const GENERATOR: u64 = 5;

/// `EXP[e] = 5^(e+1) mod 37 - 1`: a bijection on base36 digits that maps
/// consecutive digits to unrelated characters, so encoded IDs do not look
/// sequential even though the alphabet is in canonical order.
const EXP: [u8; BASE as usize] = {
    let mut table = [0; BASE as usize];
    let mut power = 1u64;
    let mut exponent = 0;
    while exponent < BASE as usize {
        power = power * GENERATOR % (BASE + 1);
        table[exponent] = (power - 1) as u8;
        exponent += 1;
    }
    table
};

/// Discrete logarithm: the inverse of [`EXP`].
const LOG: [u8; BASE as usize] = {
    let mut table = [0; BASE as usize];
    let mut exponent = 0;
    while exponent < BASE as usize {
        table[EXP[exponent] as usize] = exponent as u8;
        exponent += 1;
    }
    table
};

/// Fixed ID length.
pub const LEN: usize = 8;

/// Number of distinct IDs representable by this crate: `36^8`.
pub const CAPACITY: u64 = BASE.pow(LEN as u32);

/// A fixed-length alphanumeric ID optimized for short dynamic prefixes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrefixId<D: PrefixIdDomain> {
    counter: u64,
    _domain: PhantomData<fn() -> D>,
}

/// Marker trait selecting the hash domain for a prefix ID family.
pub trait PrefixIdDomain {
    const HASH_DOMAIN: &'static str;
}

impl<D: PrefixIdDomain> PrefixId<D> {
    pub const MIN: Self = Self {
        counter: 0,
        _domain: PhantomData,
    };

    pub const MAX: Self = Self {
        counter: CAPACITY - 1,
        _domain: PhantomData,
    };

    /// Creates an ID from a counter, returning `None` once `36^8` is exceeded.
    pub fn from_counter(counter: u64) -> Option<Self> {
        (counter < CAPACITY).then_some(Self {
            counter,
            _domain: PhantomData,
        })
    }

    /// Returns the original counter.
    pub fn to_counter(self) -> u64 {
        self.counter
    }

    /// Returns the encoded 8-character ID.
    pub fn encoded(&self) -> String {
        let mut bytes = [0; LEN];
        let mut remaining = self.counter;
        let mut hasher = domain_hasher::<D>();

        for pos in 0..LEN {
            let digit = (remaining % BASE) as u8;
            remaining /= BASE;

            let encoded = EXP[((digit + prefix_shift(&hasher)) % BASE as u8) as usize];
            bytes[pos] = ALPHABET[encoded as usize];
            hasher.write(&bytes[pos..=pos]);
        }

        String::from_utf8(bytes.to_vec()).expect("PrefixId contains only ASCII bytes")
    }

    /// Returns the shortest prefix length that uniquely identifies this ID
    /// among the first `total_generated` IDs.
    ///
    /// Panics if this ID is not in the generated range.
    pub fn unique_prefix_len(&self, total_generated: u64) -> usize {
        assert!(
            self.counter < total_generated,
            "PrefixId must be within the generated range"
        );
        assert!(
            total_generated <= CAPACITY,
            "total_generated must not exceed PrefixId capacity"
        );

        for len in 1..=LEN {
            let modulus = BASE.pow(len as u32);
            let residue = self.counter % modulus;
            let matches = 1 + (total_generated - 1 - residue) / modulus;
            if matches == 1 {
                return len;
            }
        }

        LEN
    }

    /// Resolves a prefix against the first `total_generated` IDs.
    ///
    /// `total_generated` is the count of IDs generated so far, so the
    /// considered counter range is `0..total_generated`.
    pub fn from_prefix(
        prefix: &str,
        total_generated: u64,
    ) -> Result<PrefixResolution<D>, ParsePrefixIdError> {
        if prefix.len() > LEN {
            return Err(ParsePrefixIdError::PrefixTooLong {
                max: LEN,
                actual: prefix.len(),
            });
        }
        if total_generated > CAPACITY {
            return Err(ParsePrefixIdError::TooManyGenerated {
                capacity: CAPACITY,
                actual: total_generated,
            });
        }

        let mut residue = 0u64;
        let mut place = 1u64;
        let mut hasher = domain_hasher::<D>();

        for byte in prefix.bytes() {
            let encoded = ALPHABET
                .iter()
                .position(|candidate| *candidate == byte)
                .ok_or(ParsePrefixIdError::InvalidCharacter(byte as char))?
                as u8;
            let digit = (LOG[encoded as usize] + BASE as u8 - prefix_shift(&hasher)) % BASE as u8;

            residue += digit as u64 * place;
            place *= BASE;
            hasher.write(&[byte]);
        }

        let modulus = BASE.pow(prefix.len() as u32);
        let matches = if residue >= total_generated {
            0
        } else {
            1 + (total_generated - 1 - residue) / modulus
        };

        Ok(match matches {
            0 => PrefixResolution::NotFound,
            1 => PrefixResolution::Unique(PrefixId {
                counter: residue,
                _domain: PhantomData,
            }),
            _ => PrefixResolution::Ambiguous { matches },
        })
    }
}

impl<D: PrefixIdDomain> fmt::Debug for PrefixId<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PrefixId").field(&self.counter).finish()
    }
}

impl<D: PrefixIdDomain> senax_encoder::Encoder for PrefixId<D> {
    fn encode(&self, writer: &mut bytes::BytesMut) -> senax_encoder::Result<()> {
        senax_encoder::Encoder::encode(&self.counter, writer)
    }

    fn is_default(&self) -> bool {
        self.counter == 0
    }
}

impl<D: PrefixIdDomain> senax_encoder::Packer for PrefixId<D> {
    fn pack(&self, writer: &mut bytes::BytesMut) -> senax_encoder::Result<()> {
        senax_encoder::Packer::pack(&self.counter, writer)
    }
}

impl<D: PrefixIdDomain> senax_encoder::Decoder for PrefixId<D> {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        let counter = <u64 as senax_encoder::Decoder>::decode(reader)?;
        Self::from_counter(counter).ok_or_else(|| {
            senax_encoder::EncoderError::Decode(format!(
                "prefix id counter exceeds capacity: {counter}"
            ))
        })
    }
}

impl<D: PrefixIdDomain> senax_encoder::Unpacker for PrefixId<D> {
    fn unpack(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        let counter = <u64 as senax_encoder::Unpacker>::unpack(reader)?;
        Self::from_counter(counter).ok_or_else(|| {
            senax_encoder::EncoderError::Decode(format!(
                "prefix id counter exceeds capacity: {counter}"
            ))
        })
    }
}

impl<D: PrefixIdDomain> redb::Value for PrefixId<D> {
    type SelfType<'a>
        = Self
    where
        Self: 'a;
    type AsBytes<'a>
        = [u8; size_of::<u64>()]
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        Some(size_of::<u64>())
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let counter = u64::from_le_bytes(data.try_into().expect("redb stored invalid PrefixId"));
        Self::from_counter(counter).expect("redb stored PrefixId counter above capacity")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        value.counter.to_le_bytes()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new(&format!("prefix_id::PrefixId<{}>", D::HASH_DOMAIN))
    }
}

impl<D: PrefixIdDomain> redb::Key for PrefixId<D> {
    fn compare(data1: &[u8], data2: &[u8]) -> std::cmp::Ordering {
        u64::from_le_bytes(data1.try_into().expect("redb stored invalid PrefixId")).cmp(
            &u64::from_le_bytes(data2.try_into().expect("redb stored invalid PrefixId")),
        )
    }
}

/// Result of resolving a prefix against generated IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixResolution<D: PrefixIdDomain> {
    Unique(PrefixId<D>),
    Ambiguous { matches: u64 },
    NotFound,
}

/// Error returned when parsing or resolving a [`PrefixId`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsePrefixIdError {
    PrefixTooLong { max: usize, actual: usize },
    InvalidCharacter(char),
    TooManyGenerated { capacity: u64, actual: u64 },
}

impl fmt::Display for ParsePrefixIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixTooLong { max, actual } => {
                write!(f, "prefix is too long: max {max}, got {actual}")
            }
            Self::InvalidCharacter(char) => write!(f, "invalid prefix id character: {char:?}"),
            Self::TooManyGenerated { capacity, actual } => write!(
                f,
                "too many generated ids: capacity {capacity}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ParsePrefixIdError {}

/// A hasher fed the domain and then each emitted character, so the digit shift
/// at each position depends on the characters before it.
fn domain_hasher<D: PrefixIdDomain>() -> fnv::FnvHasher {
    let mut hasher = fnv::FnvHasher::default();
    hasher.write(D::HASH_DOMAIN.as_bytes());
    hasher
}

fn prefix_shift(hasher: &fnv::FnvHasher) -> u8 {
    (hasher.finish() % BASE) as u8
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    struct TestDomain;

    impl PrefixIdDomain for TestDomain {
        const HASH_DOMAIN: &'static str = "test-id";
    }

    type Id = PrefixId<TestDomain>;

    #[test]
    fn ids_are_fixed_length_and_lowercase_alphanumeric() {
        for counter in 0..1_000 {
            let id = Id::from_counter(counter).unwrap();
            assert_eq!(id.encoded().len(), LEN);
            assert!(
                id.encoded()
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            );
        }
    }

    #[test]
    fn round_trips_counters_through_full_prefix() {
        let counters = [0, 1, 35, 36, 37, 1_000, 1_000_000, CAPACITY - 1];

        for counter in counters {
            let id = Id::from_counter(counter).unwrap();
            assert_eq!(id.to_counter(), counter);
            assert_eq!(
                Id::from_prefix(&id.encoded(), CAPACITY).unwrap(),
                PrefixResolution::Unique(id)
            );
        }
    }

    #[test]
    fn exp_table_is_a_bijection() {
        let mut digits: HashSet<u8> = EXP.iter().copied().collect();
        assert_eq!(digits.len(), BASE as usize);
        for (digit, log) in LOG.iter().enumerate() {
            assert_eq!(EXP[*log as usize] as usize, digit);
            digits.remove(&(digit as u8));
        }
        assert!(digits.is_empty());
    }

    #[test]
    fn consecutive_counters_do_not_look_sequential() {
        let canonical_index = |counter: u64| {
            let first = Id::from_counter(counter).unwrap().encoded().into_bytes()[0];
            ALPHABET.iter().position(|byte| *byte == first).unwrap()
        };

        let adjacent = (0..100)
            .filter(|counter| {
                (canonical_index(counter + 1) + BASE as usize - canonical_index(*counter))
                    % BASE as usize
                    == 1
            })
            .count();
        assert!(adjacent < 10, "first characters walk the alphabet");
    }

    #[test]
    fn rejects_counters_at_capacity() {
        assert!(Id::from_counter(CAPACITY - 1).is_some());
        assert!(Id::from_counter(CAPACITY).is_none());
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    struct OtherDomain;

    impl PrefixIdDomain for OtherDomain {
        const HASH_DOMAIN: &'static str = "other-id";
    }

    #[test]
    fn hash_domain_changes_encoding() {
        let test_id = Id::from_counter(42).unwrap();
        let other_id = PrefixId::<OtherDomain>::from_counter(42).unwrap();

        assert_ne!(test_id.encoded(), other_id.encoded());
        assert_eq!(other_id.to_counter(), 42);
    }

    #[test]
    fn first_base_to_k_ids_are_unique_by_k_char_prefix() {
        assert_unique_prefixes(36, 1);
        assert_unique_prefixes(36 * 36, 2);
        assert_unique_prefixes(36 * 36 * 36, 3);
    }

    #[test]
    fn resolves_unique_prefixes() {
        let id = Id::from_counter(20).unwrap();
        assert_eq!(
            Id::from_prefix(&id.encoded()[..1], 36).unwrap(),
            PrefixResolution::Unique(id)
        );
        assert_eq!(
            Id::from_prefix(&id.encoded()[..2], 36 * 36).unwrap(),
            PrefixResolution::Unique(id)
        );
    }

    #[test]
    fn resolves_ambiguous_prefixes() {
        let id = Id::from_counter(20).unwrap();
        assert_eq!(
            Id::from_prefix(&id.encoded()[..1], 36 + 21).unwrap(),
            PrefixResolution::Ambiguous { matches: 2 }
        );
    }

    #[test]
    fn resolves_missing_prefixes() {
        let id = Id::from_counter(20).unwrap();
        assert_eq!(
            Id::from_prefix(&id.encoded()[..1], 20).unwrap(),
            PrefixResolution::NotFound
        );
    }

    #[test]
    fn reports_unique_prefix_len() {
        let id = Id::from_counter(20).unwrap();
        assert_eq!(id.unique_prefix_len(21), 1);
        assert_eq!(id.unique_prefix_len(36 + 21), 2);
    }

    #[test]
    #[should_panic(expected = "PrefixId must be within the generated range")]
    fn unique_prefix_len_requires_generated_id() {
        let id = Id::from_counter(20).unwrap();
        let _ = id.unique_prefix_len(20);
    }

    fn assert_unique_prefixes(count: usize, prefix_len: usize) {
        let mut prefixes = HashSet::new();
        for counter in 0..count as u64 {
            let id = Id::from_counter(counter).unwrap();
            assert!(prefixes.insert(id.encoded()[..prefix_len].to_owned()));
        }
    }
}
