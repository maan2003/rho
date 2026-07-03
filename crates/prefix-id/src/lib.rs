//! Fixed-length counter IDs with short dynamic prefixes.
//!
//! `PrefixId` maps a monotonically increasing counter to a 12-character
//! lowercase alphanumeric (`a-z0-9`) ID, and holds the encoded characters —
//! the counter is derived via [`PrefixId::to_counter`]. Properties:
//!
//! - **Short unique prefixes**: the low-order base36 counter digits are emitted
//!   first, so the first `36^k` generated IDs are all distinguishable by their
//!   first `k` characters (36 IDs by 1 character, ~1.3k by 2, ~47k by 3).
//!   Prefixes are not stored in the ID; [`PrefixId::from_prefix`] resolves one
//!   arithmetically against the first `total_generated` IDs.
//! - **No visible ordering**: each digit passes through a pseudorandom
//!   permutation seeded by a hash of the preceding characters, so consecutive
//!   counters produce unrelated-looking IDs. Consequently `Ord` (and redb key
//!   order) is a stable but meaningless storage order — order by creation
//!   timestamps instead.
//! - **Cross-machine uniqueness**: every character is scrambled keyed by the
//!   domain's machine seed, so IDs from machines with different seeds
//!   collide with probability ~`36^-12` per ID pair, and nothing about an ID
//!   is readable or linkable by outsiders. Resolution self-scopes: a foreign
//!   full ID decodes under the wrong seed to a pseudorandom counter, which
//!   almost surely exceeds `total_generated` and resolves to `NotFound`.
//! - **Domain separation**: encoding is keyed by [`PrefixIdDomain::KIND`], so
//!   the same counter encodes differently across ID families.
//! - **Stateless**: counter and ID convert in both directions with no lookup
//!   table; the encoded bytes are what is stored and sent.

use std::fmt;
use std::hash::Hasher;
use std::marker::PhantomData;

const ALPHABET: &[u8; BASE as usize] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const BASE: u64 = 36;

/// Fixed ID length: the largest for which `36^LEN` still fits in a `u64`.
pub const LEN: usize = 12;

/// Number of distinct counters representable per machine: `36^12`.
pub const CAPACITY: u64 = BASE.pow(LEN as u32);

/// A fixed-length alphanumeric ID optimized for short dynamic prefixes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrefixId<D: PrefixIdDomain> {
    bytes: [u8; LEN],
    _domain: PhantomData<fn() -> D>,
}

/// Keys the encoding for an ID family.
pub trait PrefixIdDomain {
    /// Distinguishes ID families (agents vs topics); part of every hash key.
    const KIND: &'static str;

    /// Wide random identity of the generating machine, persisted once per
    /// database. It keys all character scrambling, so it must be
    /// full-entropy random, not a small assigned number.
    fn machine_seed(&self) -> u64;
}

impl<D: PrefixIdDomain> PrefixId<D> {
    /// Smallest value in storage order. Not a valid encoding; range bound
    /// only.
    pub const MIN: Self = Self {
        bytes: [0; LEN],
        _domain: PhantomData,
    };

    /// Largest value in storage order. Not a valid encoding; range bound
    /// only.
    pub const MAX: Self = Self {
        bytes: [u8::MAX; LEN],
        _domain: PhantomData,
    };

    /// Encodes a counter, returning `None` once `36^12` is exceeded.
    pub fn from_counter(counter: u64, domain: &D) -> Option<Self> {
        if counter >= CAPACITY {
            return None;
        }

        let mut bytes = [0; LEN];
        let mut remaining = counter;
        let mut hasher = machine_hasher(domain);

        for pos in 0..LEN {
            let digit = (remaining % BASE) as u8;
            remaining /= BASE;

            bytes[pos] = ALPHABET[permute_digit(&hasher, digit) as usize];
            hasher.write(&bytes[pos..=pos]);
        }

        Some(Self {
            bytes,
            _domain: PhantomData,
        })
    }

    /// Decodes the original counter. Decoding under a foreign machine's
    /// domain yields a pseudorandom counter instead.
    ///
    /// Panics on values that are not valid encodings ([`Self::MIN`],
    /// [`Self::MAX`], or corrupt storage).
    pub fn to_counter(&self, domain: &D) -> u64 {
        let mut counter = 0u64;
        let mut place = 1u64;
        let mut hasher = machine_hasher(domain);

        for byte in self.bytes {
            let encoded = ALPHABET
                .iter()
                .position(|candidate| *candidate == byte)
                .expect("PrefixId bytes are not a valid encoding") as u8;

            counter += unpermute_digit(&hasher, encoded) as u64 * place;
            place *= BASE;
            hasher.write(&[byte]);
        }

        counter
    }

    /// The ID string.
    pub fn encoded(&self) -> String {
        String::from_utf8(self.bytes.to_vec()).expect("PrefixId contains only ASCII bytes")
    }

    /// Resolves a prefix against the first `total_generated` IDs.
    ///
    /// `total_generated` is the count of IDs generated so far, so the
    /// considered counter range is `0..total_generated`.
    pub fn from_prefix(
        prefix: &str,
        total_generated: u64,
        domain: &D,
    ) -> Result<PrefixResolution<D>, ParsePrefixIdError> {
        assert!(
            total_generated <= CAPACITY,
            "total_generated must not exceed PrefixId capacity"
        );
        if prefix.len() > LEN {
            return Err(ParsePrefixIdError::PrefixTooLong {
                max: LEN,
                actual: prefix.len(),
            });
        }

        let mut residue = 0u64;
        let mut place = 1u64;
        let mut hasher = machine_hasher(domain);

        for byte in prefix.bytes() {
            let encoded = ALPHABET
                .iter()
                .position(|candidate| *candidate == byte)
                .ok_or(ParsePrefixIdError::InvalidCharacter(byte as char))?
                as u8;
            let digit = unpermute_digit(&hasher, encoded);

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
            1 => PrefixResolution::Unique(
                Self::from_counter(residue, domain).expect("resolved residue is below capacity"),
            ),
            _ => PrefixResolution::Ambiguous { matches },
        })
    }
}

impl<D: PrefixIdDomain> fmt::Debug for PrefixId<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PrefixId")
            .field(&String::from_utf8_lossy(&self.bytes))
            .finish()
    }
}

impl<D: PrefixIdDomain> senax_encoder::Encoder for PrefixId<D> {
    fn encode(&self, writer: &mut bytes::BytesMut) -> senax_encoder::Result<()> {
        senax_encoder::Encoder::encode(&bytes::Bytes::copy_from_slice(&self.bytes), writer)
    }

    fn is_default(&self) -> bool {
        false
    }
}

impl<D: PrefixIdDomain> senax_encoder::Packer for PrefixId<D> {
    fn pack(&self, writer: &mut bytes::BytesMut) -> senax_encoder::Result<()> {
        senax_encoder::Packer::pack(&bytes::Bytes::copy_from_slice(&self.bytes), writer)
    }
}

impl<D: PrefixIdDomain> senax_encoder::Decoder for PrefixId<D> {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        Self::from_wire(<bytes::Bytes as senax_encoder::Decoder>::decode(reader)?)
    }
}

impl<D: PrefixIdDomain> senax_encoder::Unpacker for PrefixId<D> {
    fn unpack(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        Self::from_wire(<bytes::Bytes as senax_encoder::Unpacker>::unpack(reader)?)
    }
}

impl<D: PrefixIdDomain> PrefixId<D> {
    fn from_wire(raw: bytes::Bytes) -> senax_encoder::Result<Self> {
        let bytes: [u8; LEN] = raw.as_ref().try_into().map_err(|_| {
            senax_encoder::EncoderError::Decode(format!(
                "prefix id must be {LEN} bytes, got {}",
                raw.len()
            ))
        })?;
        if !bytes.iter().all(|byte| ALPHABET.contains(byte)) {
            return Err(senax_encoder::EncoderError::Decode(
                "prefix id contains bytes outside its alphabet".to_owned(),
            ));
        }
        Ok(Self {
            bytes,
            _domain: PhantomData,
        })
    }
}

impl<D: PrefixIdDomain> redb::Value for PrefixId<D> {
    type SelfType<'a>
        = Self
    where
        Self: 'a;
    type AsBytes<'a>
        = [u8; LEN]
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        Some(LEN)
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        Self {
            bytes: data.try_into().expect("redb stored invalid PrefixId"),
            _domain: PhantomData,
        }
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        value.bytes
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new(&format!(
            "prefix_id::PrefixId<{}>",
            std::any::type_name::<D>()
        ))
    }
}

impl<D: PrefixIdDomain> redb::Key for PrefixId<D> {
    /// Storage order over the scrambled characters — stable, but unrelated
    /// to creation order.
    fn compare(data1: &[u8], data2: &[u8]) -> std::cmp::Ordering {
        data1.cmp(data2)
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
}

impl fmt::Display for ParsePrefixIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixTooLong { max, actual } => {
                write!(f, "prefix is too long: max {max}, got {actual}")
            }
            Self::InvalidCharacter(char) => write!(f, "invalid prefix id character: {char:?}"),
        }
    }
}

impl std::error::Error for ParsePrefixIdError {}

/// Uniform display-prefix length for a population of IDs with counters up to
/// `max_counter`: the smallest `len` such that the first `36^len` IDs cover
/// the population plus `headroom` future ones. Because `36^len` is a power of
/// the base, every ID's shortest unique prefix within that range is exactly
/// `len` characters — labels are uniform, and lengthen together only when the
/// population crosses `36^len - headroom` (at least `headroom` creations
/// apart, in practice far more).
pub fn uniform_prefix_len(max_counter: u64, headroom: u64) -> usize {
    let mut len = 2;
    let mut covered = BASE.pow(2);
    while covered < max_counter.saturating_add(1 + headroom) && len < LEN {
        len += 1;
        covered *= BASE;
    }
    len
}

/// Keys all character scrambling for one machine's IDs.
fn machine_hasher<D: PrefixIdDomain>(domain: &D) -> fnv::FnvHasher {
    let mut hasher = fnv::FnvHasher::default();
    hasher.write(D::KIND.as_bytes());
    hasher.write(&domain.machine_seed().to_le_bytes());
    hasher
}

/// Pointwise keyed bijection on base36 digits, seeded by the hash of the
/// domain and preceding characters: a Feistel network over 6-bit values with
/// cycle walking to stay in `0..36`. Evaluates a single digit in either
/// direction without materializing the permutation.
fn permute_digit(hasher: &fnv::FnvHasher, digit: u8) -> u8 {
    let seed = hasher.finish();
    let mut value = digit;
    loop {
        value = feistel(seed, value, 0..FEISTEL_ROUNDS);
        if value < BASE as u8 {
            return value;
        }
    }
}

/// The inverse of [`permute_digit`].
fn unpermute_digit(hasher: &fnv::FnvHasher, digit: u8) -> u8 {
    let seed = hasher.finish();
    let mut value = digit;
    loop {
        value = feistel(seed, value, (0..FEISTEL_ROUNDS).rev());
        if value < BASE as u8 {
            return value;
        }
    }
}

const FEISTEL_ROUNDS: u8 = 4;

/// A balanced Feistel cipher on 3+3 bit halves. The output swaps the halves,
/// which makes running the same rounds in reverse order the exact inverse.
fn feistel(seed: u64, value: u8, rounds: impl Iterator<Item = u8>) -> u8 {
    let (mut left, mut right) = (value >> 3, value & 7);
    for round in rounds {
        let mixed = left ^ round_bits(seed, round, right);
        (left, right) = (right, mixed);
    }
    right << 3 | left
}

fn round_bits(seed: u64, round: u8, half: u8) -> u8 {
    (splitmix64(seed ^ (round << 3 | half) as u64) & 7) as u8
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4B9F9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    struct TestDomain;

    impl PrefixIdDomain for TestDomain {
        const KIND: &'static str = "test-id";

        fn machine_seed(&self) -> u64 {
            0x746573742d6d6163
        }
    }

    /// Same kind as [`TestDomain`], different machine.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    struct TestMachine(u64);

    impl PrefixIdDomain for TestMachine {
        const KIND: &'static str = "test-id";

        fn machine_seed(&self) -> u64 {
            self.0
        }
    }

    type Id = PrefixId<TestDomain>;

    fn id(counter: u64) -> Id {
        Id::from_counter(counter, &TestDomain).unwrap()
    }

    #[test]
    fn ids_are_fixed_length_and_lowercase_alphanumeric() {
        for counter in 0..1_000 {
            assert_eq!(id(counter).encoded().len(), LEN);
            assert!(
                id(counter)
                    .encoded()
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            );
        }
    }

    #[test]
    fn round_trips_counters_through_full_prefix() {
        let counters = [0, 1, 35, 36, 37, 1_000, 1_000_000, CAPACITY - 1];

        for counter in counters {
            assert_eq!(id(counter).to_counter(&TestDomain), counter);
            assert_eq!(
                Id::from_prefix(&id(counter).encoded(), CAPACITY, &TestDomain).unwrap(),
                PrefixResolution::Unique(id(counter))
            );
        }
    }

    #[test]
    fn digit_permutation_round_trips_for_many_seeds() {
        for seed in 0..1_000u64 {
            let mut hasher = fnv::FnvHasher::default();
            hasher.write(&seed.to_le_bytes());
            let mut images = HashSet::new();
            for digit in 0..BASE as u8 {
                let permuted = permute_digit(&hasher, digit);
                assert!(permuted < BASE as u8);
                assert!(images.insert(permuted));
                assert_eq!(unpermute_digit(&hasher, permuted), digit);
            }
        }
    }

    #[test]
    fn domains_use_different_first_character_orders() {
        fn first_char_cycle<D: PrefixIdDomain>(domain: &D) -> Vec<u8> {
            (0..BASE)
                .map(|counter| {
                    PrefixId::<D>::from_counter(counter, domain)
                        .unwrap()
                        .encoded()
                        .into_bytes()[0]
                })
                .collect()
        }

        let test_cycle = first_char_cycle(&TestDomain);
        let other_cycle = first_char_cycle(&OtherDomain);
        let is_rotation = (0..BASE as usize).any(|offset| {
            (0..BASE as usize)
                .all(|i| test_cycle[i] == other_cycle[(i + offset) % BASE as usize])
        });
        assert!(!is_rotation, "domains share a first-character order");
    }

    #[test]
    fn consecutive_counters_do_not_look_sequential() {
        let canonical_index = |counter: u64| {
            let first = id(counter).encoded().into_bytes()[0];
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
        assert!(Id::from_counter(CAPACITY - 1, &TestDomain).is_some());
        assert!(Id::from_counter(CAPACITY, &TestDomain).is_none());
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    struct OtherDomain;

    impl PrefixIdDomain for OtherDomain {
        const KIND: &'static str = "other-id";

        fn machine_seed(&self) -> u64 {
            0x746573742d6d6163
        }
    }

    #[test]
    fn machine_seed_changes_encoding_and_scopes_resolution() {
        let (machine_a, machine_b) = (TestMachine(1), TestMachine(2));

        let id_a = PrefixId::<TestMachine>::from_counter(42, &machine_a).unwrap();
        let id_b = PrefixId::<TestMachine>::from_counter(42, &machine_b).unwrap();
        assert_ne!(id_a, id_b);

        // A full ID resolves on its own machine; under a foreign seed it
        // decodes to a pseudorandom counter far beyond any realistic
        // `total_generated`, so resolution self-scopes.
        assert_eq!(
            PrefixId::from_prefix(&id_a.encoded(), 100, &machine_a).unwrap(),
            PrefixResolution::Unique(id_a)
        );
        assert_eq!(
            PrefixId::from_prefix(&id_a.encoded(), 100, &machine_b).unwrap(),
            PrefixResolution::NotFound
        );
    }

    #[test]
    fn ids_from_different_machines_do_not_collide() {
        let (machine_a, machine_b) = (TestMachine(1), TestMachine(2));
        for counter_a in 0..100u64 {
            let id_a = PrefixId::<TestMachine>::from_counter(counter_a, &machine_a).unwrap();
            for counter_b in 0..100u64 {
                let id_b = PrefixId::<TestMachine>::from_counter(counter_b, &machine_b).unwrap();
                assert_ne!(id_a, id_b);
            }
        }
    }

    #[test]
    fn hash_domain_changes_encoding() {
        let test_id = id(42);
        let other_id = PrefixId::<OtherDomain>::from_counter(42, &OtherDomain).unwrap();

        assert_ne!(test_id.encoded(), other_id.encoded());
        assert_eq!(other_id.to_counter(&OtherDomain), 42);
    }

    #[test]
    fn first_base_to_k_ids_are_unique_by_k_char_prefix() {
        assert_unique_prefixes(36, 1);
        assert_unique_prefixes(36 * 36, 2);
        assert_unique_prefixes(36 * 36 * 36, 3);
    }

    #[test]
    fn resolves_unique_prefixes() {
        assert_eq!(
            Id::from_prefix(&id(20).encoded()[..1], 36, &TestDomain).unwrap(),
            PrefixResolution::Unique(id(20))
        );
        assert_eq!(
            Id::from_prefix(&id(20).encoded()[..2], 36 * 36, &TestDomain).unwrap(),
            PrefixResolution::Unique(id(20))
        );
    }

    #[test]
    fn resolves_ambiguous_prefixes() {
        assert_eq!(
            Id::from_prefix(&id(20).encoded()[..1], 36 + 21, &TestDomain).unwrap(),
            PrefixResolution::Ambiguous { matches: 2 }
        );
    }

    #[test]
    fn resolves_missing_prefixes() {
        assert_eq!(
            Id::from_prefix(&id(20).encoded()[..1], 20, &TestDomain).unwrap(),
            PrefixResolution::NotFound
        );
    }

    fn assert_unique_prefixes(count: usize, prefix_len: usize) {
        let mut prefixes = HashSet::new();
        for counter in 0..count as u64 {
            assert!(prefixes.insert(id(counter).encoded()[..prefix_len].to_owned()));
        }
    }

    #[test]
    fn uniform_prefix_lens_leave_headroom_between_changes() {
        const HEADROOM: u64 = 200;
        assert_eq!(uniform_prefix_len(0, HEADROOM), 2);
        let mut last_change = 0u64;
        let mut last_len = uniform_prefix_len(0, HEADROOM);
        for max_counter in 1..100_000u64 {
            let len = uniform_prefix_len(max_counter, HEADROOM);
            assert!(len >= last_len, "prefix lengths must be monotonic");
            if len != last_len {
                assert!(
                    max_counter - last_change >= HEADROOM,
                    "prefix length changed after only {} ids",
                    max_counter - last_change
                );
                last_change = max_counter;
                last_len = len;
            }
        }
        assert_eq!(last_len, 4, "expected the 3- and 4-character thresholds");
    }

    #[test]
    fn uniform_prefix_len_is_unique_within_covered_range() {
        let len = uniform_prefix_len(999, 200);
        assert_unique_prefixes(36usize.pow(len as u32), len);
    }
}
