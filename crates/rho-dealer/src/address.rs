//! What a thing in the user's world is called: in memory, in ledger keys,
//! in history, on screen.
//!
//! An address is `kind:rest`. The kind names the system the thing belongs
//! to, and the rest is that system's own key for it; nothing here knows
//! what any rest means, so a kind a newer build adds still reads, files
//! and ranks here. Two addresses are the same thing when their text is.
//! No address contains `|`, which ends an address in a ledger key.
//!
//! A note is the dealer's own kind: the one kind of node rho mints rather
//! than finds.

use std::fmt;

/// A system's own key for one of its things, and the way back.
pub trait Addressed: Sized {
    /// The kind every address of this type starts with.
    const KIND: &'static str;

    fn rest(&self) -> String;

    /// `None` for a rest this type does not read.
    fn from_rest(rest: &str) -> Option<Self>;

    fn address(&self) -> Address {
        Address::new(Self::KIND, self.rest())
    }

    /// `None` for an address of another kind, or a rest this type does
    /// not read.
    fn from_address(address: &Address) -> Option<Self> {
        (address.kind() == Self::KIND)
            .then(|| Self::from_rest(address.rest()))
            .flatten()
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    key: String,
    /// Where `:` is.
    split: usize,
}

/// What threads were called before rho2, and what every entry written
/// then still says. A topic's id was its first message's id and the host
/// kept every id, so a topic is the thread under the same number.
const LEGACY_KINDS: [(&str, &str); 1] = [("topic", "thread")];

impl Address {
    pub const NOTE: &'static str = "note";

    /// `kind:rest`. The kind is a word: not empty, no `:`; neither part
    /// holds `|`.
    pub fn new(kind: &str, rest: impl fmt::Display) -> Self {
        let key = format!("{kind}:{rest}");
        assert!(
            !kind.is_empty() && !kind.contains(':') && !key.contains('|'),
            "not an address: {key}"
        );
        Self {
            split: kind.len(),
            key,
        }
    }

    /// Reads a key back. `None` for text that is not an address at all;
    /// a kind this build does not know still reads, and is left alone.
    pub fn parse(key: &str) -> Option<Self> {
        let split = key.find(':')?;
        if split == 0 || key.contains('|') {
            return None;
        }
        let (kind, rest) = (&key[..split], &key[split + 1..]);
        let kind = LEGACY_KINDS
            .iter()
            .find(|(old, _)| *old == kind)
            .map_or(kind, |(_, new)| new);
        Some(Self::new(kind, rest))
    }

    pub fn kind(&self) -> &str {
        &self.key[..self.split]
    }

    pub fn rest(&self) -> &str {
        &self.key[self.split + 1..]
    }

    /// The address as text, the way ledger keys name it.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn into_key(self) -> String {
        self.key
    }

    pub fn note(id: uuid::Uuid) -> Self {
        Self::new(Self::NOTE, id.simple())
    }

    pub fn as_note(&self) -> Option<uuid::Uuid> {
        (self.kind() == Self::NOTE)
            .then(|| uuid::Uuid::try_parse(self.rest()).ok())
            .flatten()
    }

    /// Made by rho rather than found in a source.
    pub fn is_minted(&self) -> bool {
        self.kind() == Self::NOTE
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key)
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({})", self.key)
    }
}

/// An address goes over the wire as its key.
impl senax_encoder::Encoder for Address {
    fn encode(&self, writer: &mut bytes::BytesMut) -> senax_encoder::Result<()> {
        self.key.encode(writer)
    }

    fn is_default(&self) -> bool {
        false
    }
}

impl senax_encoder::Decoder for Address {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        let key = String::decode(reader)?;
        Self::parse(&key)
            .ok_or_else(|| senax_encoder::EncoderError::Decode(format!("not an address: {key}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_reads_back_from_its_key() {
        let note = Address::note(uuid::Uuid::new_v4());
        assert_eq!(note.kind(), "note");
        assert!(note.as_note().is_some());
        assert_eq!(Address::parse(note.key()), Some(note.clone()));
        let thread = Address::new("thread", "27/318");
        assert_eq!(thread.rest(), "27/318");
        assert_eq!(Address::parse(thread.key()).as_ref(), Some(&thread));
        assert_eq!(
            Address::parse("gadget:1").map(|a| a.kind().to_owned()),
            Some("gadget".into())
        );
        assert_eq!(Address::parse("nothing"), None);
        assert_eq!(Address::parse(":rest"), None);
        assert_eq!(Address::parse("a:b|c"), None);
        assert!(!thread.is_minted() && note.is_minted());
    }

    #[test]
    fn a_topic_written_before_rho2_is_a_thread() {
        let old = Address::parse("topic:00000000000000ff/17").unwrap();
        assert_eq!(old, Address::new("thread", "00000000000000ff/17"));
        let mut bytes = senax_encoder::encode(&"topic:0000000000000000/1".to_owned()).unwrap();
        let decoded: Address = senax_encoder::decode(&mut bytes).unwrap();
        assert_eq!(decoded.key(), "thread:0000000000000000/1");
    }
}
