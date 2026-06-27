//! # senax-encoder
//!
//! A fast, compact, and schema-evolution-friendly binary serialization library for Rust.
//!
//! - Supports struct/enum encoding with field/variant IDs for forward/backward compatibility
//! - Efficient encoding for primitives, collections, Option, String, bytes, and popular crates (chrono, uuid, ulid, rust_decimal, bigdecimal, indexmap, fxhash, ahash, smol_str, serde_json)
//! - Custom derive macros for ergonomic usage
//! - Feature-gated support for optional dependencies
//!
//! ## Binary Format
//!
//! This library uses magic numbers to distinguish between different serialization formats:
//! - **Encode format**: Starts with magic number `0xA55A` (2 bytes, little-endian)
//!   - Used by `encode()` and `encode_to()` functions
//!   - Supports schema evolution with field IDs and type tags
//! - **Pack format**: Starts with magic number `0xDADA` (2 bytes, little-endian)
//!   - Used by `pack()` and `pack_to()` functions
//!   - Compact format without schema evolution support
//!
//! Magic numbers are only added when using the convenience functions in this library.
//! Direct trait method calls (`Encoder::encode`, `Packer::pack`) do not include magic numbers.
//!
//! ## Attribute Macros
//!
//! You can control encoding/decoding behavior using the following attributes:
//!
//! - `#[senax(id = N)]` — Assigns a custom field or variant ID (u64). Ensures stable wire format across versions.
//! - `#[senax(default)]` — If a field is missing during decoding, its value is set to `Default::default()` instead of causing an error. For `Option<T>`, this means `None`.
//! - `#[senax(skip_encode)]` — This field is not written during encoding. On decode, it is set to `Default::default()`.
//! - `#[senax(skip_decode)]` — This field is ignored during decoding and always set to `Default::default()`. It is still encoded if present.
//! - `#[senax(skip_default)]` — This field is not written during encoding if its value equals the default value. On decode, missing fields are set to `Default::default()`.
//! - `#[senax(rename = "name")]` — Use the given string as the logical field/variant name for ID calculation. Useful for renaming fields/variants while keeping the same wire format.
//!
//! ## Feature Flags
//!
//! The following optional features enable support for popular crates and types:
//!
//! ### External Crate Support
//! - `chrono` — Enables encoding/decoding of `chrono::DateTime`, `NaiveDate`, and `NaiveTime` types.
//! - `uuid` — Enables encoding/decoding of `uuid::Uuid`.
//! - `ulid` — Enables encoding/decoding of `ulid::Ulid` (shares the same tag as UUID for binary compatibility).
//! - `rust_decimal` — Enables encoding/decoding of `rust_decimal::Decimal`.
//! - `bigdecimal` — Enables encoding/decoding of `bigdecimal::BigDecimal` (stored as scientific notation string).
//! - `indexmap` — Enables encoding/decoding of `IndexMap` and `IndexSet` collections.
//! - `fxhash` — Enables encoding/decoding of `fxhash::FxHashMap` and `fxhash::FxHashSet` (fast hash collections).
//! - `ahash` — Enables encoding/decoding of `ahash::AHashMap` and `ahash::AHashSet` (high-performance hash collections).
//! - `smol_str` — Enables encoding/decoding of `smol_str::SmolStr` (small string optimization).
//! - `serde_json` — Enables encoding/decoding of `serde_json::Value` (JSON values as dynamic type).
//! - `raw_value` — Enables encoding/decoding of `Box<serde_json::value::RawValue>` (raw JSON strings). Requires `serde_json` feature.

pub mod core;
mod features;

use bytes::{Buf, BufMut, Bytes, BytesMut};
pub use senax_encoder_derive::{Decode, Encode, Pack, Unpack};
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

/// Errors that can occur during encoding or decoding operations.
#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    /// The value could not be encoded (e.g., unsupported type or logic error).
    #[error("Encode error: {0}")]
    Encode(String),
    /// The value could not be decoded (e.g., invalid data, type mismatch, or schema evolution error).
    #[error("Decode error: {0}")]
    Decode(String),
    /// The buffer did not contain enough data to complete the operation.
    #[error("Insufficient data in buffer")]
    InsufficientData,
    /// Struct-specific decode error
    #[error(transparent)]
    StructDecode(#[from] StructDecodeError),
    /// Enum-specific decode error
    #[error(transparent)]
    EnumDecode(#[from] EnumDecodeError),
}

/// The result type used throughout this crate for encode/decode operations.
///
/// All `Encode` and `Decode` trait methods return this type.
pub type Result<T> = std::result::Result<T, EncoderError>;

/// Derive-specific error types for struct operations
#[derive(Debug, thiserror::Error)]
pub enum StructDecodeError {
    #[error("Expected struct named tag ({expected}), got {actual}")]
    InvalidTag { expected: u8, actual: u8 },
    #[error("Required field '{field}' not found for struct {struct_name}")]
    MissingRequiredField {
        field: &'static str,
        struct_name: &'static str,
    },
    #[error("Field count mismatch for struct {struct_name}: expected {expected}, got {actual}")]
    FieldCountMismatch {
        struct_name: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("Structure hash mismatch for {struct_name}: expected 0x{expected:016X}, got 0x{actual:016X}")]
    StructureHashMismatch {
        struct_name: &'static str,
        expected: u64,
        actual: u64,
    },
}

/// Derive-specific error types for enum operations
#[derive(Debug, thiserror::Error)]
pub enum EnumDecodeError {
    #[error("Unknown enum tag: {tag} for enum {enum_name}")]
    UnknownTag { tag: u8, enum_name: &'static str },
    #[error("Unknown variant ID: 0x{variant_id:016X} for enum {enum_name}")]
    UnknownVariantId {
        variant_id: u64,
        enum_name: &'static str,
    },
    #[error("Unknown unit variant ID: 0x{variant_id:016X} for enum {enum_name}")]
    UnknownUnitVariantId {
        variant_id: u64,
        enum_name: &'static str,
    },
    #[error("Unknown named variant ID: 0x{variant_id:016X} for enum {enum_name}")]
    UnknownNamedVariantId {
        variant_id: u64,
        enum_name: &'static str,
    },
    #[error("Unknown unnamed variant ID: 0x{variant_id:016X} for enum {enum_name}")]
    UnknownUnnamedVariantId {
        variant_id: u64,
        enum_name: &'static str,
    },
    #[error("Required field '{field}' not found for variant {enum_name}::{variant_name}")]
    MissingRequiredField {
        field: &'static str,
        enum_name: &'static str,
        variant_name: &'static str,
    },
    #[error("Field count mismatch for variant {enum_name}::{variant_name}: expected {expected}, got {actual}")]
    FieldCountMismatch {
        enum_name: &'static str,
        variant_name: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("Structure hash mismatch for variant {enum_name}::{variant_name}: expected 0x{expected:016X}, got 0x{actual:016X}")]
    StructureHashMismatch {
        enum_name: &'static str,
        variant_name: &'static str,
        expected: u64,
        actual: u64,
    },
}

/// Magic number for encoded format (0xA55A in little-endian)
const ENCODE_MAGIC: u16 = 0xA55A;

/// Magic number for packed format (0xDADA in little-endian)
const PACK_MAGIC: u16 = 0xDADA;

/// Convenience function to decode a value from bytes.
///
/// This function expects and verifies the encode magic number (0xA55A) at the beginning of the data.
/// It provides schema evolution support through field IDs and type tags.
///
/// # Arguments
/// * `reader` - The buffer to read the encoded bytes from.
///
/// # Example
/// ```rust
/// use senax_encoder::{encode, decode, Encode, Decode};
/// use bytes::BytesMut;
///
/// #[derive(Encode, Decode, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct { id: 42, name: "hello".to_string() };
/// let mut buf = encode(&value).unwrap();
/// let decoded: MyStruct = decode(&mut buf).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn decode<T: Decoder>(reader: &mut Bytes) -> Result<T> {
    if reader.remaining() < 2 {
        return Err(EncoderError::InsufficientData);
    }
    let magic = reader.get_u16_le();
    if magic != ENCODE_MAGIC {
        return Err(EncoderError::Decode(format!(
            "Invalid encode magic number: expected 0x{:04X}, got 0x{:04X}",
            ENCODE_MAGIC, magic
        )));
    }
    T::decode(reader)
}

/// Convenience function to encode a value to bytes with magic number.
///
/// This function adds the encode magic number (0xA55A) at the beginning of the data
/// and provides schema evolution support through field IDs and type tags.
///
/// # Arguments
/// * `value` - The value to encode.
///
/// # Example
/// ```rust
/// use senax_encoder::{encode, decode, Encode, Decode};
/// use bytes::BytesMut;
///
/// #[derive(Encode, Decode, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct { id: 42, name: "hello".to_string() };
/// let mut buf = encode(&value).unwrap();
/// let decoded: MyStruct = decode(&mut buf).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn encode<T: Encoder>(value: &T) -> Result<Bytes> {
    let mut writer = BytesMut::new();
    writer.put_u16_le(ENCODE_MAGIC);
    value.encode(&mut writer)?;
    Ok(writer.freeze())
}

/// Convenience function to encode a value to an existing BytesMut buffer with magic number.
///
/// This function adds the encode magic number (0xA55A) at the current position in the buffer
/// and provides schema evolution support through field IDs and type tags.
///
/// # Arguments
/// * `value` - The value to encode.
/// * `writer` - The buffer to write the encoded bytes into.
///
/// # Example
/// ```rust
/// use senax_encoder::{encode_to, decode, Encode, Decode};
/// use bytes::{BytesMut, Bytes};
///
/// #[derive(Encode, Decode, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct { id: 42, name: "hello".to_string() };
/// let mut buf = BytesMut::new();
/// encode_to(&value, &mut buf).unwrap();
/// let mut data = buf.freeze();
/// let decoded: MyStruct = decode(&mut data).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn encode_to<T: Encoder>(value: &T, writer: &mut BytesMut) -> Result<()> {
    writer.put_u16_le(ENCODE_MAGIC);
    value.encode(writer)
}

/// Trait for types that can be encoded into the senax binary format.
///
/// Implement this trait for your type to enable serialization.
/// Most users should use `#[derive(Encode)]` instead of manual implementation.
///
/// # Errors
/// Returns `EncoderError` if the value cannot be encoded.
pub trait Encoder {
    /// Encode the value into the given buffer with schema evolution support.
    ///
    /// This method includes field IDs and type tags for forward/backward compatibility.
    /// Use this when you need schema evolution support.
    ///
    /// # Arguments
    /// * `writer` - The buffer to write the encoded bytes into.
    fn encode(&self, writer: &mut BytesMut) -> Result<()>;

    /// Returns true if this value equals its default value.
    /// Used by `#[senax(skip_default)]` attribute to skip encoding default values.
    fn is_default(&self) -> bool;
}

/// Trait for types that can be packed into a compact binary format.
///
/// This trait provides compact serialization without schema evolution support.
/// Use this when you need maximum performance and don't require forward/backward compatibility.
///
/// # Errors
/// Returns `EncoderError` if the value cannot be packed.
pub trait Packer {
    /// Pack the value into the given buffer without schema evolution support.
    ///
    /// This method stores data in a compact format without field IDs or type tags.
    /// The format is not schema-evolution-friendly but offers better performance.
    ///
    /// # Arguments
    /// * `writer` - The buffer to write the packed bytes into.
    fn pack(&self, writer: &mut BytesMut) -> Result<()>;
}

/// Trait for types that can be decoded from the senax binary format.
///
/// Implement this trait for your type to enable deserialization.
/// Most users should use `#[derive(Decode)]` instead of manual implementation.
///
/// # Errors
/// Returns `EncoderError` if the value cannot be decoded or the data is invalid.
pub trait Decoder: Sized {
    /// Decode the value from the given buffer with schema evolution support.
    ///
    /// This method expects field IDs and type tags for forward/backward compatibility.
    /// Use this when you need schema evolution support.
    ///
    /// # Arguments
    /// * `reader` - The buffer to read the encoded bytes from.
    fn decode(reader: &mut Bytes) -> Result<Self>;
}

/// Trait for types that can be unpacked from a compact binary format.
///
/// This trait provides compact deserialization without schema evolution support.
/// Use this when you need maximum performance and don't require forward/backward compatibility.
///
/// # Errors
/// Returns `EncoderError` if the value cannot be unpacked or the data is invalid.
pub trait Unpacker: Sized {
    /// Unpack the value from the given buffer without schema evolution support.
    ///
    /// This method reads data from a compact format without field IDs or type tags.
    /// The format is not schema-evolution-friendly but offers better performance.
    ///
    /// # Arguments
    /// * `reader` - The buffer to read the packed bytes from.
    fn unpack(reader: &mut Bytes) -> Result<Self>;
}

/// Convenience function to pack a value to bytes with magic number.
///
/// This function adds the pack magic number (0xDADA) at the beginning of the data.
/// The packed format is compact but not schema-evolution-friendly.
///
/// # Arguments
/// * `value` - The value to pack.
///
/// # Example
/// ```rust
/// use senax_encoder::{pack, unpack, Pack, Unpack};
/// use bytes::BytesMut;
///
/// #[derive(Pack, Unpack, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct { id: 42, name: "hello".to_string() };
/// let mut buf = pack(&value).unwrap();
/// let decoded: MyStruct = unpack(&mut buf).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn pack<T: Packer>(value: &T) -> Result<Bytes> {
    let mut writer = BytesMut::new();
    writer.put_u16_le(PACK_MAGIC);
    value.pack(&mut writer)?;
    Ok(writer.freeze())
}

/// Convenience function to pack a value to an existing BytesMut buffer with magic number.
///
/// This function adds the pack magic number (0xDADA) at the current position in the buffer.
/// The packed format is compact but not schema-evolution-friendly.
///
/// # Arguments
/// * `value` - The value to pack.
/// * `writer` - The buffer to write the packed bytes into.
///
/// # Example
/// ```rust
/// use senax_encoder::{pack_to, unpack, Pack, Unpack};
/// use bytes::{BytesMut, Bytes};
///
/// #[derive(Pack, Unpack, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct { id: 42, name: "hello".to_string() };
/// let mut buf = BytesMut::new();
/// pack_to(&value, &mut buf).unwrap();
/// let mut data = buf.freeze();
/// let decoded: MyStruct = unpack(&mut data).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn pack_to<T: Packer>(value: &T, writer: &mut BytesMut) -> Result<()> {
    writer.put_u16_le(PACK_MAGIC);
    value.pack(writer)
}

/// Convenience function to unpack a value from bytes.
///
/// This function expects and verifies the pack magic number (0xDADA) at the beginning of the data.
/// The packed format is compact but not schema-evolution-friendly.
///
/// # Arguments
/// * `reader` - The buffer to read the packed bytes from.
///
/// # Example
/// ```rust
/// use senax_encoder::{pack, unpack, Pack, Unpack};
/// use bytes::BytesMut;
///
/// #[derive(Pack, Unpack, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct { id: 42, name: "hello".to_string() };
/// let mut buf = pack(&value).unwrap();
/// let decoded: MyStruct = unpack(&mut buf).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn unpack<T: Unpacker>(reader: &mut Bytes) -> Result<T> {
    if reader.remaining() < 2 {
        return Err(EncoderError::InsufficientData);
    }
    let magic = reader.get_u16_le();
    if magic != PACK_MAGIC {
        return Err(EncoderError::Decode(format!(
            "Invalid pack magic number: expected 0x{:04X}, got 0x{:04X}",
            PACK_MAGIC, magic
        )));
    }
    T::unpack(reader)
}
