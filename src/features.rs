#[cfg(feature = "ahash")]
use ahash::{AHashMap, AHashSet};
#[cfg(feature = "bigdecimal")]
use bigdecimal::{BigDecimal, Zero};
#[allow(unused_imports)]
use bytes::{Buf, BufMut, Bytes, BytesMut};
#[cfg(feature = "chrono")]
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
#[cfg(feature = "fxhash")]
use fxhash::{FxHashMap, FxHashSet};
#[cfg(feature = "indexmap")]
use indexmap::{IndexMap, IndexSet};
#[cfg(feature = "rust_decimal")]
use rust_decimal::Decimal;
#[cfg(feature = "raw_value")]
use serde_json::value::RawValue;
#[cfg(feature = "serde_json")]
use serde_json::{Map, Number, Value};
#[cfg(feature = "smol_str")]
use smol_str::SmolStr;
#[cfg(feature = "ulid")]
use ulid::Ulid;
#[cfg(feature = "uuid")]
use uuid::Uuid;

#[allow(unused_imports)]
use crate::core::*;
#[allow(unused_imports)]
use crate::*;

// --- IndexSet ---
#[cfg(feature = "indexmap")]
impl<T: Encoder + Eq + std::hash::Hash> Encoder for IndexSet<T> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        encode_vec_length(self.len(), writer)?;
        for v in self {
            v.encode(writer)?;
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        self.is_empty()
    }
}
#[cfg(feature = "indexmap")]
impl<T: Decoder + Eq + std::hash::Hash + 'static> Decoder for IndexSet<T> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        let vec: Vec<T> = Vec::decode(reader)?;
        Ok(vec.into_iter().collect())
    }
}
#[cfg(feature = "indexmap")]
impl<T: Packer + Eq + std::hash::Hash> Packer for IndexSet<T> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        encode_vec_length(self.len(), writer)?;
        for v in self {
            v.pack(writer)?;
        }
        Ok(())
    }
}
#[cfg(feature = "indexmap")]
impl<T: Unpacker + Eq + std::hash::Hash + 'static> Unpacker for IndexSet<T> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        let vec: Vec<T> = Vec::unpack(reader)?;
        Ok(vec.into_iter().collect())
    }
}

// --- IndexMap ---
#[cfg(feature = "indexmap")]
impl<K: Encoder + Eq + std::hash::Hash, V: Encoder> Encoder for IndexMap<K, V> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_MAP);
        let len = self.len();
        len.encode(writer)?;
        for (k, v) in self {
            k.encode(writer)?;
            v.encode(writer)?;
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        self.is_empty()
    }
}
#[cfg(feature = "indexmap")]
impl<K: Decoder + Eq + std::hash::Hash, V: Decoder> Decoder for IndexMap<K, V> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        let len = read_map_header(reader)?;
        let mut map = IndexMap::with_capacity(len);
        for _ in 0..len {
            let k = K::decode(reader)?;
            let v = V::decode(reader)?;
            map.insert(k, v);
        }
        Ok(map)
    }
}
#[cfg(feature = "indexmap")]
impl<K: Packer + Eq + std::hash::Hash, V: Packer> Packer for IndexMap<K, V> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_MAP);
        let len = self.len();
        len.encode(writer)?;
        for (k, v) in self {
            k.pack(writer)?;
            v.pack(writer)?;
        }
        Ok(())
    }
}
#[cfg(feature = "indexmap")]
impl<K: Unpacker + Eq + std::hash::Hash, V: Unpacker> Unpacker for IndexMap<K, V> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        let len = read_map_header(reader)?;
        let mut map = IndexMap::with_capacity(len);
        for _ in 0..len {
            let k = K::unpack(reader)?;
            let v = V::unpack(reader)?;
            map.insert(k, v);
        }
        Ok(map)
    }
}

// --- DateTime<Utc> ---
/// Encodes a `chrono::DateTime<Utc>` as seconds and nanoseconds since the Unix epoch.
#[cfg(feature = "chrono")]
impl Encoder for DateTime<Utc> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_CHRONO_DATETIME);
        let timestamp_seconds = self.timestamp();
        let timestamp_nanos = self.timestamp_subsec_nanos();
        timestamp_seconds.encode(writer)?;
        timestamp_nanos.encode(writer)?;
        Ok(())
    }

    fn is_default(&self) -> bool {
        *self == DateTime::<Utc>::default()
    }
}
/// Decodes a `chrono::DateTime<Utc>` from the senax binary format.
#[cfg(feature = "chrono")]
impl Decoder for DateTime<Utc> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_CHRONO_DATETIME {
            return Err(EncoderError::Decode(format!(
                "Expected DateTime<Utc> tag ({}), got {}",
                TAG_CHRONO_DATETIME, tag
            )));
        }
        let timestamp_seconds = i64::decode(reader)?;
        let timestamp_nanos = u32::decode(reader)?;
        DateTime::from_timestamp(timestamp_seconds, timestamp_nanos).ok_or_else(|| {
            EncoderError::Decode(format!(
                "Invalid timestamp: {} seconds, {} nanos",
                timestamp_seconds, timestamp_nanos
            ))
        })
    }
}

/// Packs a `chrono::DateTime<Utc>` as seconds and nanoseconds without a type tag.
#[cfg(feature = "chrono")]
impl Packer for DateTime<Utc> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        if *self == DateTime::<Utc>::default() {
            writer.put_u8(TAG_NONE);
        } else {
            writer.put_u8(TAG_CHRONO_DATETIME);
            let timestamp_seconds = self.timestamp();
            let timestamp_nanos = self.timestamp_subsec_nanos();
            timestamp_seconds.pack(writer)?;
            timestamp_nanos.pack(writer)?;
        }
        Ok(())
    }
}

/// Unpacks a `chrono::DateTime<Utc>` from the pack format.
#[cfg(feature = "chrono")]
impl Unpacker for DateTime<Utc> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        match tag {
            TAG_NONE => Ok(DateTime::<Utc>::default()),
            TAG_CHRONO_DATETIME => {
                let timestamp_seconds = i64::unpack(reader)?;
                let timestamp_nanos = u32::unpack(reader)?;
                DateTime::from_timestamp(timestamp_seconds, timestamp_nanos).ok_or_else(|| {
                    EncoderError::Decode(format!(
                        "Invalid timestamp: {} seconds, {} nanos",
                        timestamp_seconds, timestamp_nanos
                    ))
                })
            }
            _ => Err(EncoderError::Decode(format!(
                "Expected DateTime<Utc> tag ({} or {}), got {}",
                TAG_NONE, TAG_CHRONO_DATETIME, tag
            ))),
        }
    }
}

// --- DateTime<Local> ---
#[cfg(feature = "chrono")]
impl Encoder for DateTime<Local> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_CHRONO_DATETIME);
        let timestamp_seconds = self.timestamp();
        let timestamp_nanos = self.timestamp_subsec_nanos();
        timestamp_seconds.encode(writer)?;
        timestamp_nanos.encode(writer)?;
        Ok(())
    }

    fn is_default(&self) -> bool {
        *self == DateTime::<Local>::default()
    }
}
#[cfg(feature = "chrono")]
impl Decoder for DateTime<Local> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_CHRONO_DATETIME {
            return Err(EncoderError::Decode(format!(
                "Expected DateTime<Local> tag ({}), got {}",
                TAG_CHRONO_DATETIME, tag
            )));
        }
        let timestamp_seconds = i64::decode(reader)?;
        let timestamp_nanos = u32::decode(reader)?;
        let utc_dt =
            DateTime::from_timestamp(timestamp_seconds, timestamp_nanos).ok_or_else(|| {
                EncoderError::Decode(format!(
                    "Invalid timestamp: {} seconds, {} nanos",
                    timestamp_seconds, timestamp_nanos
                ))
            })?;
        Ok(utc_dt.with_timezone(&Local))
    }
}

/// Packs a `chrono::DateTime<Local>` as seconds and nanoseconds without a type tag.
#[cfg(feature = "chrono")]
impl Packer for DateTime<Local> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        if *self == DateTime::<Local>::default() {
            writer.put_u8(TAG_NONE);
        } else {
            writer.put_u8(TAG_CHRONO_DATETIME);
            let timestamp_seconds = self.timestamp();
            let timestamp_nanos = self.timestamp_subsec_nanos();
            timestamp_seconds.pack(writer)?;
            timestamp_nanos.pack(writer)?;
        }
        Ok(())
    }
}

/// Unpacks a `chrono::DateTime<Local>` from the pack format.
#[cfg(feature = "chrono")]
impl Unpacker for DateTime<Local> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        match tag {
            TAG_NONE => Ok(DateTime::<Local>::default()),
            TAG_CHRONO_DATETIME => {
                let timestamp_seconds = i64::unpack(reader)?;
                let timestamp_nanos = u32::unpack(reader)?;
                let utc_dt = DateTime::from_timestamp(timestamp_seconds, timestamp_nanos)
                    .ok_or_else(|| {
                        EncoderError::Decode(format!(
                            "Invalid timestamp: {} seconds, {} nanos",
                            timestamp_seconds, timestamp_nanos
                        ))
                    })?;
                Ok(utc_dt.with_timezone(&Local))
            }
            _ => Err(EncoderError::Decode(format!(
                "Expected DateTime<Local> tag ({} or {}), got {}",
                TAG_NONE, TAG_CHRONO_DATETIME, tag
            ))),
        }
    }
}

// --- NaiveDate ---
#[cfg(feature = "chrono")]
impl Encoder for NaiveDate {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_CHRONO_NAIVE_DATE);
        // Store as days since 1970-01-01
        let days_from_epoch = self
            .signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
            .num_days();
        days_from_epoch.encode(writer)?;
        Ok(())
    }

    fn is_default(&self) -> bool {
        *self == NaiveDate::default()
    }
}
#[cfg(feature = "chrono")]
impl Decoder for NaiveDate {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_CHRONO_NAIVE_DATE {
            return Err(EncoderError::Decode(format!(
                "Expected NaiveDate tag ({}), got {}",
                TAG_CHRONO_NAIVE_DATE, tag
            )));
        }
        let days_from_epoch = i64::decode(reader)?;
        let epoch_date = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        epoch_date
            .checked_add_signed(chrono::TimeDelta::try_days(days_from_epoch).unwrap())
            .ok_or_else(|| {
                EncoderError::Decode(format!("Invalid days from epoch: {}", days_from_epoch))
            })
    }
}
#[cfg(feature = "chrono")]
impl Unpacker for NaiveDate {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_CHRONO_NAIVE_DATE {
            return Err(EncoderError::Decode(format!(
                "Expected NaiveDate tag ({}), got {}",
                TAG_CHRONO_NAIVE_DATE, tag
            )));
        }
        let days_from_epoch = i64::unpack(reader)?;
        let epoch_date = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        epoch_date
            .checked_add_signed(chrono::TimeDelta::try_days(days_from_epoch).unwrap())
            .ok_or_else(|| {
                EncoderError::Decode(format!("Invalid days from epoch: {}", days_from_epoch))
            })
    }
}
#[cfg(feature = "chrono")]
impl Packer for NaiveDate {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_CHRONO_NAIVE_DATE);
        // Store as days since 1970-01-01
        let days_from_epoch = self
            .signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
            .num_days();
        days_from_epoch.pack(writer)?;
        Ok(())
    }
}

// --- NaiveTime ---
#[cfg(feature = "chrono")]
impl Encoder for NaiveTime {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_CHRONO_NAIVE_TIME);
        // Store seconds and nanoseconds from 00:00:00 separately
        let seconds_from_midnight = self.num_seconds_from_midnight();
        let nanoseconds = self.nanosecond();
        seconds_from_midnight.encode(writer)?;
        nanoseconds.encode(writer)?;
        Ok(())
    }

    fn is_default(&self) -> bool {
        *self == NaiveTime::default()
    }
}
#[cfg(feature = "chrono")]
impl Decoder for NaiveTime {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_CHRONO_NAIVE_TIME {
            return Err(EncoderError::Decode(format!(
                "Expected NaiveTime tag ({}), got {}",
                TAG_CHRONO_NAIVE_TIME, tag
            )));
        }
        let seconds_from_midnight = u32::decode(reader)?;
        let nanoseconds = u32::decode(reader)?;
        NaiveTime::from_num_seconds_from_midnight_opt(seconds_from_midnight, nanoseconds)
            .ok_or_else(|| {
                EncoderError::Decode(format!(
                    "Invalid seconds from midnight: {}, nanoseconds: {}",
                    seconds_from_midnight, nanoseconds
                ))
            })
    }
}
#[cfg(feature = "chrono")]
impl Unpacker for NaiveTime {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_CHRONO_NAIVE_TIME {
            return Err(EncoderError::Decode(format!(
                "Expected NaiveTime tag ({}), got {}",
                TAG_CHRONO_NAIVE_TIME, tag
            )));
        }
        let seconds_from_midnight = u32::unpack(reader)?;
        let nanoseconds = u32::unpack(reader)?;
        NaiveTime::from_num_seconds_from_midnight_opt(seconds_from_midnight, nanoseconds)
            .ok_or_else(|| {
                EncoderError::Decode(format!(
                    "Invalid seconds from midnight: {}, nanoseconds: {}",
                    seconds_from_midnight, nanoseconds
                ))
            })
    }
}
#[cfg(feature = "chrono")]
impl Packer for NaiveTime {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_CHRONO_NAIVE_TIME);
        // Store seconds and nanoseconds from 00:00:00 separately
        let seconds_from_midnight = self.num_seconds_from_midnight();
        let nanoseconds = self.nanosecond();
        seconds_from_midnight.pack(writer)?;
        nanoseconds.pack(writer)?;
        Ok(())
    }
}

// --- NaiveDateTime ---
#[cfg(feature = "chrono")]
impl Encoder for NaiveDateTime {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_CHRONO_NAIVE_DATETIME);
        // Store as seconds and nanoseconds since Unix epoch (1970-01-01 00:00:00)
        let timestamp_seconds = self.and_utc().timestamp();
        let timestamp_nanos = self.and_utc().timestamp_subsec_nanos();
        timestamp_seconds.encode(writer)?;
        timestamp_nanos.encode(writer)?;
        Ok(())
    }

    fn is_default(&self) -> bool {
        *self == NaiveDateTime::default()
    }
}

#[cfg(feature = "chrono")]
impl Decoder for NaiveDateTime {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_CHRONO_NAIVE_DATETIME {
            return Err(EncoderError::Decode(format!(
                "Expected NaiveDateTime tag ({}), got {}",
                TAG_CHRONO_NAIVE_DATETIME, tag
            )));
        }
        let timestamp_seconds = i64::decode(reader)?;
        let timestamp_nanos = u32::decode(reader)?;
        Ok(DateTime::from_timestamp(timestamp_seconds, timestamp_nanos)
            .ok_or_else(|| {
                EncoderError::Decode(format!(
                    "Invalid timestamp: {} seconds, {} nanos",
                    timestamp_seconds, timestamp_nanos
                ))
            })?
            .naive_utc())
    }
}

#[cfg(feature = "chrono")]
impl Packer for NaiveDateTime {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        if *self == NaiveDateTime::default() {
            writer.put_u8(TAG_NONE);
        } else {
            writer.put_u8(TAG_CHRONO_NAIVE_DATETIME);
            let timestamp_seconds = self.and_utc().timestamp();
            let timestamp_nanos = self.and_utc().timestamp_subsec_nanos();
            timestamp_seconds.pack(writer)?;
            timestamp_nanos.pack(writer)?;
        }
        Ok(())
    }
}

#[cfg(feature = "chrono")]
impl Unpacker for NaiveDateTime {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        match tag {
            TAG_NONE => Ok(NaiveDateTime::default()),
            TAG_CHRONO_NAIVE_DATETIME => {
                let timestamp_seconds = i64::unpack(reader)?;
                let timestamp_nanos = u32::unpack(reader)?;
                Ok(DateTime::from_timestamp(timestamp_seconds, timestamp_nanos)
                    .ok_or_else(|| {
                        EncoderError::Decode(format!(
                            "Invalid timestamp: {} seconds, {} nanos",
                            timestamp_seconds, timestamp_nanos
                        ))
                    })?
                    .naive_utc())
            }
            _ => Err(EncoderError::Decode(format!(
                "Expected NaiveDateTime tag ({} or {}), got {}",
                TAG_NONE, TAG_CHRONO_NAIVE_DATETIME, tag
            ))),
        }
    }
}

// --- Decimal ---
#[cfg(feature = "rust_decimal")]
impl Encoder for Decimal {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        // Convert to string and encode as STRING
        // Note: Pack/Unpack still uses binary format for efficiency.
        // The string format provides better compatibility and readability for Encode/Decode.
        let s = self.to_string();
        s.encode(writer)
    }

    fn is_default(&self) -> bool {
        *self == Decimal::default()
    }
}
#[cfg(feature = "rust_decimal")]
impl Packer for Decimal {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        self.encode(writer)
    }
}
#[cfg(feature = "rust_decimal")]
impl Decoder for Decimal {
    /// Decodes a Decimal from a string, legacy binary format, or i128.
    ///
    /// This decoder supports:
    /// - New string format (TAG_STRING_BASE..TAG_STRING_LONG)
    /// - Legacy binary format (TAG_DECIMAL + mantissa + scale)
    /// - i128 cross-decode (TAG_ZERO..TAG_U128, TAG_NEGATIVE)
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }

        // Peek at the tag to determine format
        let tag = reader.chunk()[0];

        // Try string format first (new format)
        if (TAG_STRING_BASE..=TAG_STRING_LONG).contains(&tag) {
            let s = String::decode(reader)?;
            return s.parse::<Decimal>().map_err(|e| {
                EncoderError::Decode(format!("Invalid Decimal string '{}': {}", s, e))
            });
        }

        // Try i128 cross-decode
        if tag == TAG_NEGATIVE || (TAG_ZERO..=TAG_U128).contains(&tag) {
            let i128_val = i128::decode(reader)?;
            return Ok(Decimal::from(i128_val));
        }

        // Fall back to legacy binary format for backward compatibility
        reader.advance(1); // consume the tag
        if tag != TAG_DECIMAL {
            return Err(EncoderError::Decode(format!(
                "Expected Decimal string ({}..={}), binary tag ({}), or integer tag, got {}",
                TAG_STRING_BASE, TAG_STRING_LONG, TAG_DECIMAL, tag
            )));
        }
        let mantissa = i128::decode(reader)?;
        let scale = u32::decode(reader)?;

        Decimal::try_from_i128_with_scale(mantissa, scale).map_err(|e| {
            EncoderError::Decode(format!(
                "Invalid decimal: mantissa={}, scale={}, error={}",
                mantissa, scale, e
            ))
        })
    }
}
#[cfg(feature = "rust_decimal")]
impl Unpacker for Decimal {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        Self::decode(reader)
    }
}

// --- BigDecimal ---
#[cfg(feature = "bigdecimal")]
impl Encoder for BigDecimal {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        // Convert to scientific notation string and encode as STRING
        let s = self.to_scientific_notation();
        s.encode(writer)
    }

    fn is_default(&self) -> bool {
        self.is_zero()
    }
}
#[cfg(feature = "bigdecimal")]
impl Packer for BigDecimal {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        self.encode(writer)
    }
}
#[cfg(feature = "bigdecimal")]
impl Decoder for BigDecimal {
    /// Decodes a BigDecimal from a string or i128.
    ///
    /// This decoder supports:
    /// - String format (TAG_STRING_BASE..TAG_STRING_LONG)
    /// - i128 cross-decode (TAG_ZERO..TAG_U128, TAG_NEGATIVE)
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }

        // Peek at the tag to determine format
        let tag = reader.chunk()[0];

        // Try i128 cross-decode first
        if tag == TAG_NEGATIVE || (TAG_ZERO..=TAG_U128).contains(&tag) {
            let i128_val = i128::decode(reader)?;
            return Ok(BigDecimal::from(i128_val));
        }

        // Fall back to string format
        let s = String::decode(reader)?;
        s.parse::<BigDecimal>()
            .map_err(|e| EncoderError::Decode(format!("Invalid BigDecimal string '{}': {}", s, e)))
    }
}
#[cfg(feature = "bigdecimal")]
impl Unpacker for BigDecimal {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        Self::decode(reader)
    }
}

// --- UUID ---
#[cfg(feature = "uuid")]
impl Encoder for Uuid {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_UUID);
        // Write UUID as u128 little-endian in fixed 16 bytes
        let uuid_u128 = self.as_u128();
        writer.put_u128_le(uuid_u128);
        Ok(())
    }

    fn is_default(&self) -> bool {
        *self == Uuid::default()
    }
}
#[cfg(feature = "uuid")]
impl Packer for Uuid {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        if *self == Uuid::default() {
            writer.put_u8(TAG_NONE);
        } else {
            writer.put_u8(TAG_UUID);
            // Write UUID as u128 little-endian in fixed 16 bytes
            let uuid_u128 = self.as_u128();
            writer.put_u128_le(uuid_u128);
        }
        Ok(())
    }
}
#[cfg(feature = "uuid")]
impl Decoder for Uuid {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_UUID {
            return Err(EncoderError::Decode(format!(
                "Expected UUID tag ({}), got {}",
                TAG_UUID, tag
            )));
        }
        if reader.remaining() < 16 {
            return Err(EncoderError::InsufficientData);
        }
        let uuid_u128 = reader.get_u128_le();
        Ok(Uuid::from_u128(uuid_u128))
    }
}
#[cfg(feature = "uuid")]
impl Unpacker for Uuid {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        match tag {
            TAG_NONE => Ok(Uuid::default()),
            TAG_UUID => {
                if reader.remaining() < 16 {
                    return Err(EncoderError::InsufficientData);
                }
                let uuid_u128 = reader.get_u128_le();
                Ok(Uuid::from_u128(uuid_u128))
            }
            _ => Err(EncoderError::Decode(format!(
                "Expected UUID tag ({} or {}), got {}",
                TAG_NONE, TAG_UUID, tag
            ))),
        }
    }
}

// --- ULID ---
#[cfg(feature = "ulid")]
impl Encoder for Ulid {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_UUID); // Use same tag as UUID
                                 // Write ULID as u128 little-endian in fixed 16 bytes
        let ulid_u128 = self.0;
        writer.put_u128_le(ulid_u128);
        Ok(())
    }

    fn is_default(&self) -> bool {
        *self == Ulid::default()
    }
}
#[cfg(feature = "ulid")]
impl Packer for Ulid {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        if *self == Ulid::default() {
            writer.put_u8(TAG_NONE);
        } else {
            writer.put_u8(TAG_UUID); // Use same tag as UUID
                                     // Write ULID as u128 little-endian in fixed 16 bytes
            let ulid_u128 = self.0;
            writer.put_u128_le(ulid_u128);
        }
        Ok(())
    }
}
#[cfg(feature = "ulid")]
impl Decoder for Ulid {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != TAG_UUID {
            return Err(EncoderError::Decode(format!(
                "Expected ULID tag ({}), got {}",
                TAG_UUID, tag
            )));
        }
        if reader.remaining() < 16 {
            return Err(EncoderError::InsufficientData);
        }
        let ulid_u128 = reader.get_u128_le();
        Ok(Ulid(ulid_u128))
    }
}
#[cfg(feature = "ulid")]
impl Unpacker for Ulid {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        match tag {
            TAG_NONE => Ok(Ulid::default()),
            TAG_UUID => {
                if reader.remaining() < 16 {
                    return Err(EncoderError::InsufficientData);
                }
                let ulid_u128 = reader.get_u128_le();
                Ok(Ulid(ulid_u128))
            }
            _ => Err(EncoderError::Decode(format!(
                "Expected ULID tag ({} or {}), got {}",
                TAG_NONE, TAG_UUID, tag
            ))),
        }
    }
}

// --- serde_json::Value ---
#[cfg(feature = "serde_json")]
impl Encoder for Value {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        match self {
            Value::Null => {
                writer.put_u8(TAG_JSON_NULL);
                Ok(())
            }
            Value::Bool(b) => {
                writer.put_u8(TAG_JSON_BOOL);
                b.encode(writer)?;
                Ok(())
            }
            Value::Number(n) => {
                writer.put_u8(TAG_JSON_NUMBER);
                // Preserve integer/float distinction where possible
                if let Some(u) = n.as_u64() {
                    // Encode as tagged unsigned integer
                    writer.put_u8(0); // Unsigned integer (u64) marker
                    u.encode(writer)?;
                } else if let Some(i) = n.as_i64() {
                    // Encode as tagged signed integer
                    writer.put_u8(1); // Signed integer (i64) marker
                    i.encode(writer)?;
                } else {
                    // Encode as float
                    writer.put_u8(2); // Float marker
                    let float_val = n.as_f64().unwrap_or(0.0);
                    float_val.encode(writer)?;
                }
                Ok(())
            }
            Value::String(s) => {
                writer.put_u8(TAG_JSON_STRING);
                s.encode(writer)?;
                Ok(())
            }
            Value::Array(arr) => {
                writer.put_u8(TAG_JSON_ARRAY);
                let len = arr.len();
                len.encode(writer)?;
                for item in arr {
                    item.encode(writer)?;
                }
                Ok(())
            }
            Value::Object(obj) => {
                writer.put_u8(TAG_JSON_OBJECT);
                let len = obj.len();
                len.encode(writer)?;
                for (key, value) in obj {
                    key.encode(writer)?;
                    value.encode(writer)?;
                }
                Ok(())
            }
        }
    }

    fn is_default(&self) -> bool {
        *self == Value::default()
    }
}

#[cfg(feature = "serde_json")]
impl Packer for Value {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        self.encode(writer)
    }
}

#[cfg(feature = "serde_json")]
impl Decoder for Value {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        match tag {
            TAG_JSON_NULL => Ok(Value::Null),
            TAG_JSON_BOOL => {
                let b = bool::decode(reader)?;
                Ok(Value::Bool(b))
            }
            TAG_JSON_NUMBER => {
                if reader.remaining() == 0 {
                    return Err(EncoderError::InsufficientData);
                }
                let number_type = reader.get_u8();
                match number_type {
                    0 => {
                        // Unsigned integer
                        let u = u64::decode(reader)?;
                        Ok(Value::Number(Number::from(u)))
                    }
                    1 => {
                        // Signed integer
                        let i = i64::decode(reader)?;
                        Ok(Value::Number(Number::from(i)))
                    }
                    2 => {
                        // Float
                        let f = f64::decode(reader)?;
                        Ok(Value::Number(
                            Number::from_f64(f).unwrap_or(Number::from(0)),
                        ))
                    }
                    _ => Err(EncoderError::Decode(format!(
                        "Invalid JSON Number type marker: {}",
                        number_type
                    ))),
                }
            }
            TAG_JSON_STRING => {
                let s = String::decode(reader)?;
                Ok(Value::String(s))
            }
            TAG_JSON_ARRAY => {
                let len = usize::decode(reader)?;
                let mut arr = Vec::with_capacity(len);
                for _ in 0..len {
                    arr.push(Value::decode(reader)?);
                }
                Ok(Value::Array(arr))
            }
            TAG_JSON_OBJECT => {
                let len = usize::decode(reader)?;
                let mut obj = Map::with_capacity(len);
                for _ in 0..len {
                    let key = String::decode(reader)?;
                    let value = Value::decode(reader)?;
                    obj.insert(key, value);
                }
                Ok(Value::Object(obj))
            }
            _ => Err(EncoderError::Decode(format!(
                "Expected JSON Value tag (202-207), got {}",
                tag
            ))),
        }
    }
}

#[cfg(feature = "serde_json")]
impl Unpacker for Value {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        Self::decode(reader)
    }
}

// --- FxHashMap ---
#[cfg(feature = "fxhash")]
impl<K: Encoder + Eq + std::hash::Hash, V: Encoder> Encoder for FxHashMap<K, V> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_MAP);
        let len = self.len();
        len.encode(writer)?;
        for (k, v) in self {
            k.encode(writer)?;
            v.encode(writer)?;
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        self.is_empty()
    }
}
#[cfg(feature = "fxhash")]
impl<K: Packer + Eq + std::hash::Hash, V: Packer> Packer for FxHashMap<K, V> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_MAP);
        let len = self.len();
        len.encode(writer)?;
        for (k, v) in self {
            k.pack(writer)?;
            v.pack(writer)?;
        }
        Ok(())
    }
}
#[cfg(feature = "fxhash")]
impl<K: Decoder + Eq + std::hash::Hash, V: Decoder> Decoder for FxHashMap<K, V> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        let len = read_map_header(reader)?;
        let mut map = FxHashMap::with_capacity_and_hasher(len, Default::default());
        for _ in 0..len {
            let k = K::decode(reader)?;
            let v = V::decode(reader)?;
            map.insert(k, v);
        }
        Ok(map)
    }
}
#[cfg(feature = "fxhash")]
impl<K: Unpacker + Eq + std::hash::Hash, V: Unpacker> Unpacker for FxHashMap<K, V> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        let len = read_map_header(reader)?;
        let mut map = FxHashMap::with_capacity_and_hasher(len, Default::default());
        for _ in 0..len {
            let k = K::unpack(reader)?;
            let v = V::unpack(reader)?;
            map.insert(k, v);
        }
        Ok(map)
    }
}

// --- AHashMap ---
#[cfg(feature = "ahash")]
impl<K: Encoder + Eq + std::hash::Hash, V: Encoder> Encoder for AHashMap<K, V> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_MAP);
        let len = self.len();
        len.encode(writer)?;
        for (k, v) in self {
            k.encode(writer)?;
            v.encode(writer)?;
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        self.is_empty()
    }
}
#[cfg(feature = "ahash")]
impl<K: Packer + Eq + std::hash::Hash, V: Packer> Packer for AHashMap<K, V> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        writer.put_u8(TAG_MAP);
        let len = self.len();
        len.encode(writer)?;
        for (k, v) in self {
            k.pack(writer)?;
            v.pack(writer)?;
        }
        Ok(())
    }
}
#[cfg(feature = "ahash")]
impl<K: Decoder + Eq + std::hash::Hash, V: Decoder> Decoder for AHashMap<K, V> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        let len = read_map_header(reader)?;
        let mut map = AHashMap::with_capacity(len);
        for _ in 0..len {
            let k = K::decode(reader)?;
            let v = V::decode(reader)?;
            map.insert(k, v);
        }
        Ok(map)
    }
}
#[cfg(feature = "ahash")]
impl<K: Unpacker + Eq + std::hash::Hash, V: Unpacker> Unpacker for AHashMap<K, V> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        let len = read_map_header(reader)?;
        let mut map = AHashMap::with_capacity(len);
        for _ in 0..len {
            let k = K::unpack(reader)?;
            let v = V::unpack(reader)?;
            map.insert(k, v);
        }
        Ok(map)
    }
}

// --- FxHashSet ---
#[cfg(feature = "fxhash")]
impl<T: Encoder + Eq + std::hash::Hash> Encoder for FxHashSet<T> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        encode_vec_length(self.len(), writer)?;
        for v in self {
            v.encode(writer)?;
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        self.is_empty()
    }
}
#[cfg(feature = "fxhash")]
impl<T: Packer + Eq + std::hash::Hash> Packer for FxHashSet<T> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        encode_vec_length(self.len(), writer)?;
        for v in self {
            v.pack(writer)?;
        }
        Ok(())
    }
}
#[cfg(feature = "fxhash")]
impl<T: Decoder + Eq + std::hash::Hash + 'static> Decoder for FxHashSet<T> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        let vec: Vec<T> = Vec::decode(reader)?;
        Ok(vec.into_iter().collect())
    }
}
#[cfg(feature = "fxhash")]
impl<T: Unpacker + Eq + std::hash::Hash + 'static> Unpacker for FxHashSet<T> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        let vec: Vec<T> = Vec::unpack(reader)?;
        Ok(vec.into_iter().collect())
    }
}

// --- AHashSet ---
#[cfg(feature = "ahash")]
impl<T: Encoder + Eq + std::hash::Hash> Encoder for AHashSet<T> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        encode_vec_length(self.len(), writer)?;
        for v in self {
            v.encode(writer)?;
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        self.is_empty()
    }
}
#[cfg(feature = "ahash")]
impl<T: Packer + Eq + std::hash::Hash> Packer for AHashSet<T> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        encode_vec_length(self.len(), writer)?;
        for v in self {
            v.pack(writer)?;
        }
        Ok(())
    }
}
#[cfg(feature = "ahash")]
impl<T: Decoder + Eq + std::hash::Hash + 'static> Decoder for AHashSet<T> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        let vec: Vec<T> = Vec::decode(reader)?;
        Ok(vec.into_iter().collect())
    }
}
#[cfg(feature = "ahash")]
impl<T: Unpacker + Eq + std::hash::Hash + 'static> Unpacker for AHashSet<T> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        let vec: Vec<T> = Vec::unpack(reader)?;
        Ok(vec.into_iter().collect())
    }
}

// --- SmolStr ---
#[cfg(feature = "smol_str")]
impl Encoder for SmolStr {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        let len = self.len();
        let max_short = (TAG_STRING_LONG - TAG_STRING_BASE - 1) as usize;
        if len <= max_short {
            let tag = TAG_STRING_BASE + len as u8;
            writer.put_u8(tag);
            writer.put_slice(self.as_bytes());
        } else {
            writer.put_u8(TAG_STRING_LONG);
            len.encode(writer)?;
            writer.put_slice(self.as_bytes());
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        self.is_empty()
    }
}
#[cfg(feature = "smol_str")]
impl Packer for SmolStr {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        self.encode(writer)
    }
}
#[cfg(feature = "smol_str")]
impl Decoder for SmolStr {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        let len = if (TAG_STRING_BASE..TAG_STRING_LONG).contains(&tag) {
            (tag - TAG_STRING_BASE) as usize
        } else if tag == TAG_STRING_LONG {
            usize::decode(reader)?
        } else {
            return Err(EncoderError::Decode(format!(
                "Expected String tag ({}..={}), got {}",
                TAG_STRING_BASE, TAG_STRING_LONG, tag
            )));
        };
        if reader.remaining() < len {
            return Err(EncoderError::InsufficientData);
        }
        let mut bytes = vec![0u8; len];
        if len > 0 {
            reader.copy_to_slice(&mut bytes);
        }
        let string = String::from_utf8(bytes).map_err(|e| EncoderError::Decode(e.to_string()))?;
        Ok(SmolStr::new(string))
    }
}
#[cfg(feature = "smol_str")]
impl Unpacker for SmolStr {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        Self::decode(reader)
    }
}

// --- Box<serde_json::value::RawValue> ---
#[cfg(feature = "raw_value")]
impl Encoder for Box<RawValue> {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        let json_str = self.get();
        let len = json_str.len();
        if len < (TAG_STRING_LONG - TAG_STRING_BASE) as usize {
            writer.put_u8(TAG_STRING_BASE + len as u8);
            writer.put_slice(json_str.as_bytes());
        } else {
            writer.put_u8(TAG_STRING_LONG);
            len.encode(writer)?;
            writer.put_slice(json_str.as_bytes());
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        self.get().is_empty()
    }
}

#[cfg(feature = "raw_value")]
impl Packer for Box<RawValue> {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        self.encode(writer)
    }
}

#[cfg(feature = "raw_value")]
impl Decoder for Box<RawValue> {
    fn decode(reader: &mut Bytes) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        let len = if (TAG_STRING_BASE..TAG_STRING_LONG).contains(&tag) {
            (tag - TAG_STRING_BASE) as usize
        } else if tag == TAG_STRING_LONG {
            usize::decode(reader)?
        } else {
            return Err(EncoderError::Decode(format!(
                "Expected String tag ({}..={}), got {}",
                TAG_STRING_BASE, TAG_STRING_LONG, tag
            )));
        };
        if reader.remaining() < len {
            return Err(EncoderError::InsufficientData);
        }
        let mut bytes = vec![0u8; len];
        if len > 0 {
            reader.copy_to_slice(&mut bytes);
        }
        let string = String::from_utf8(bytes).map_err(|e| EncoderError::Decode(e.to_string()))?;
        RawValue::from_string(string).map_err(|e| EncoderError::Decode(e.to_string()))
    }
}

#[cfg(feature = "raw_value")]
impl Unpacker for Box<RawValue> {
    fn unpack(reader: &mut Bytes) -> Result<Self> {
        Self::decode(reader)
    }
}
