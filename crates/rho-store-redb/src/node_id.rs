use anyhow::{Result, bail, ensure};
use serde::de::{Error as DeError, SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A stable node identifier in the transcript forest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeRef {
    pub lineage_id: u64,
    pub seq: u64,
}

impl NodeRef {
    pub const fn new(lineage_id: u64, seq: u64) -> Self {
        Self { lineage_id, seq }
    }
}

impl Serialize for NodeRef {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = encode_node_key(*self);
        let mut tuple = serializer.serialize_tuple(bytes.len())?;
        for byte in bytes {
            tuple.serialize_element(&byte)?;
        }
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for NodeRef {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_tuple(18, NodeRefVisitor)
    }
}

struct NodeRefVisitor;

impl<'de> Visitor<'de> for NodeRefVisitor {
    type Value = NodeRef;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("two ordered-varint encoded u64 values")
    }

    fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let lineage_id = read_ordvarint_from_seq(&mut seq)?;
        let seq_id = read_ordvarint_from_seq(&mut seq)?;
        Ok(NodeRef::new(lineage_id, seq_id))
    }
}

pub(crate) fn encode_node_key(node_ref: NodeRef) -> Vec<u8> {
    let mut key = encode_ordvarint(node_ref.lineage_id);
    key.extend(encode_ordvarint(node_ref.seq));
    key
}

pub(crate) fn decode_node_key(bytes: &[u8]) -> Result<NodeRef> {
    let (lineage_id, used) = decode_ordvarint(bytes)?;
    let (seq, seq_used) = decode_ordvarint(&bytes[used..])?;
    ensure!(
        used + seq_used == bytes.len(),
        "node key has trailing bytes"
    );
    Ok(NodeRef::new(lineage_id, seq))
}

fn read_ordvarint_from_seq<'de, A>(seq: &mut A) -> std::result::Result<u64, A::Error>
where
    A: SeqAccess<'de>,
{
    let first = seq
        .next_element::<u8>()?
        .ok_or_else(|| A::Error::custom("truncated ordered varint"))?;
    let len = ordvarint_len_from_header(first);
    let mut bytes = Vec::with_capacity(len);
    bytes.push(first);
    for _ in 1..len {
        bytes.push(
            seq.next_element::<u8>()?
                .ok_or_else(|| A::Error::custom("truncated ordered varint"))?,
        );
    }
    decode_ordvarint(&bytes)
        .map(|(value, _used)| value)
        .map_err(|error| A::Error::custom(error.to_string()))
}

fn ordvarint_len_from_header(header: u8) -> usize {
    match header {
        0..=240 => 1,
        241..=248 => 2,
        249 => 3,
        250..=255 => (header - 247) as usize + 1,
    }
}

pub(crate) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    for index in (0..end.len()).rev() {
        if end[index] != u8::MAX {
            end[index] += 1;
            end.truncate(index + 1);
            return Some(end);
        }
    }
    None
}

pub(crate) fn encode_ordvarint(value: u64) -> Vec<u8> {
    match value {
        0..=240 => vec![value as u8],
        241..=2_287 => {
            let adjusted = value - 240;
            vec![(adjusted / 256 + 241) as u8, (adjusted % 256) as u8]
        }
        2_288..=67_823 => {
            let adjusted = value - 2_288;
            vec![249, (adjusted / 256) as u8, (adjusted % 256) as u8]
        }
        67_824..=0xFF_FFFF => fixed_ordvarint(250, value, 3),
        0x1_000000..=0xFFFF_FFFF => fixed_ordvarint(251, value, 4),
        0x1_00000000..=0xFF_FFFF_FFFF => fixed_ordvarint(252, value, 5),
        0x100_00000000..=0xFFFF_FFFF_FFFF => fixed_ordvarint(253, value, 6),
        0x1_0000_00000000..=0xFF_FFFF_FFFF_FFFF => fixed_ordvarint(254, value, 7),
        _ => fixed_ordvarint(255, value, 8),
    }
}

fn fixed_ordvarint(tag: u8, value: u64, bytes: usize) -> Vec<u8> {
    let be = value.to_be_bytes();
    let mut output = Vec::with_capacity(bytes + 1);
    output.push(tag);
    output.extend_from_slice(&be[8 - bytes..]);
    output
}

pub(crate) fn decode_ordvarint(bytes: &[u8]) -> Result<(u64, usize)> {
    let Some((&tag, rest)) = bytes.split_first() else {
        bail!("empty ordered varint");
    };

    let decoded = match tag {
        0..=240 => (tag as u64, 1),
        241..=248 => {
            ensure!(!rest.is_empty(), "truncated ordered varint");
            (((tag as u64 - 241) * 256) + rest[0] as u64 + 240, 2)
        }
        249 => {
            ensure!(rest.len() >= 2, "truncated ordered varint");
            ((rest[0] as u64) * 256 + rest[1] as u64 + 2_288, 3)
        }
        250..=255 => {
            let len = (tag - 247) as usize;
            ensure!(rest.len() >= len, "truncated ordered varint");
            let mut buf = [0; 8];
            buf[8 - len..].copy_from_slice(&rest[..len]);
            (u64::from_be_bytes(buf), 1 + len)
        }
    };

    let canonical = encode_ordvarint(decoded.0);
    ensure!(
        canonical.as_slice() == &bytes[..decoded.1],
        "non-canonical ordered varint"
    );
    Ok(decoded)
}
