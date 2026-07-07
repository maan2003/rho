//! # senax-encoder
//!
//! A fast, compact, and schema-evolution-friendly binary serialization library
//! for Rust.
//!
//! - Supports struct/enum encoding with field/variant IDs for forward/backward
//!   compatibility
//! - Efficient encoding for primitives, collections, Option, String, bytes, and
//!   popular crates (chrono, uuid, ulid, rust_decimal, bigdecimal, indexmap,
//!   fxhash, ahash, smol_str, serde_json)
//! - Custom derive macros for ergonomic usage
//! - Feature-gated support for optional dependencies
//!
//! ## Binary Format
//!
//! This library provides two binary formats:
//! - **Encode format**: Supports schema evolution with field IDs and type tags.
//! - **Pack format**: Compact format without schema evolution support.
//!
//! ## Attribute Macros
//!
//! You can control encoding/decoding behavior using the following attributes:
//!
//! - `#[senax(id = N)]` — Assigns a custom field or variant ID (u64). Ensures
//!   stable wire format across versions.
//! - `#[senax(default)]` — If a field is missing during decoding, its value is
//!   set to `Default::default()` instead of causing an error. For `Option<T>`,
//!   this means `None`.
//! - `#[senax(skip_encode)]` — This field is not written during encoding. On
//!   decode, it is set to `Default::default()`.
//! - `#[senax(skip_decode)]` — This field is ignored during decoding and always
//!   set to `Default::default()`. It is still encoded if present.
//! - `#[senax(skip_default)]` — This field is not written during encoding if
//!   its value equals the default value. On decode, missing fields are set to
//!   `Default::default()`.
//! - `#[senax(rename = "name")]` — Use the given string as the logical
//!   field/variant name for ID calculation. Useful for renaming fields/variants
//!   while keeping the same wire format.
//!
//! ## Feature Flags
//!
//! The following optional features enable support for popular crates and types:
//!
//! ### External Crate Support
//! - `chrono` — Enables encoding/decoding of `chrono::DateTime`, `NaiveDate`,
//!   and `NaiveTime` types.
//! - `uuid` — Enables encoding/decoding of `uuid::Uuid`.
//! - `ulid` — Enables encoding/decoding of `ulid::Ulid` (shares the same tag as
//!   UUID for binary compatibility).
//! - `rust_decimal` — Enables encoding/decoding of `rust_decimal::Decimal`.
//! - `bigdecimal` — Enables encoding/decoding of `bigdecimal::BigDecimal`
//!   (stored as scientific notation string).
//! - `indexmap` — Enables encoding/decoding of `IndexMap` and `IndexSet`
//!   collections.
//! - `fxhash` — Enables encoding/decoding of `fxhash::FxHashMap` and
//!   `fxhash::FxHashSet` (fast hash collections).
//! - `ahash` — Enables encoding/decoding of `ahash::AHashMap` and
//!   `ahash::AHashSet` (high-performance hash collections).
//! - `smol_str` — Enables encoding/decoding of `smol_str::SmolStr` (small
//!   string optimization).
//! - `serde_json` — Enables encoding/decoding of `serde_json::Value` (JSON
//!   values as dynamic type).
//! - `raw_value` — Enables encoding/decoding of
//!   `Box<serde_json::value::RawValue>` (raw JSON strings). Requires
//!   `serde_json` feature.

pub mod core;
mod features;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use bytes::{Buf, BufMut, Bytes, BytesMut};
pub use senax_encoder_derive::{Decode, Encode, Pack, Unpack};

#[doc(hidden)]
pub mod __private {
    pub use paste;
}

/// Errors that can occur during encoding or decoding operations.
#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    /// The value could not be encoded (e.g., unsupported type or logic error).
    #[error("Encode error: {0}")]
    Encode(String),
    /// The value could not be decoded (e.g., invalid data, type mismatch, or
    /// schema evolution error).
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
    #[error(
        "Structure hash mismatch for {struct_name}: expected 0x{expected:016X}, got 0x{actual:016X}"
    )]
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
    #[error(
        "Field count mismatch for variant {enum_name}::{variant_name}: expected {expected}, got {actual}"
    )]
    FieldCountMismatch {
        enum_name: &'static str,
        variant_name: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error(
        "Structure hash mismatch for variant {enum_name}::{variant_name}: expected 0x{expected:016X}, got 0x{actual:016X}"
    )]
    StructureHashMismatch {
        enum_name: &'static str,
        variant_name: &'static str,
        expected: u64,
        actual: u64,
    },
}

/// Convenience function to decode a value from bytes.
///
/// This function decodes the schema-evolution-friendly encode format.
///
/// # Arguments
/// * `reader` - The buffer to read the encoded bytes from.
///
/// # Example
/// ```rust
/// use bytes::BytesMut;
/// use senax_encoder::{Decode, Encode, decode, encode};
///
/// #[derive(Encode, Decode, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct {
///     id: 42,
///     name: "hello".to_string(),
/// };
/// let mut buf = encode(&value).unwrap();
/// let decoded: MyStruct = decode(&mut buf).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn decode<T: Decoder>(reader: &mut impl Buf) -> Result<T> {
    T::decode(reader)
}

/// Convenience function to encode a value to bytes.
///
/// This function encodes the schema-evolution-friendly encode format.
///
/// # Arguments
/// * `value` - The value to encode.
///
/// # Example
/// ```rust
/// use bytes::BytesMut;
/// use senax_encoder::{Decode, Encode, decode, encode};
///
/// #[derive(Encode, Decode, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct {
///     id: 42,
///     name: "hello".to_string(),
/// };
/// let mut buf = encode(&value).unwrap();
/// let decoded: MyStruct = decode(&mut buf).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn encode<T: Encoder>(value: &T) -> Result<Bytes> {
    let mut writer = BytesMut::new();
    value.encode(&mut writer)?;
    Ok(writer.freeze())
}

/// Convenience function to encode a value to an existing BytesMut buffer.
///
/// This function encodes the schema-evolution-friendly encode format.
///
/// # Arguments
/// * `value` - The value to encode.
/// * `writer` - The buffer to write the encoded bytes into.
///
/// # Example
/// ```rust
/// use bytes::{Bytes, BytesMut};
/// use senax_encoder::{Decode, Encode, decode, encode_to};
///
/// #[derive(Encode, Decode, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct {
///     id: 42,
///     name: "hello".to_string(),
/// };
/// let mut buf = BytesMut::new();
/// encode_to(&value, &mut buf).unwrap();
/// let mut data = buf.freeze();
/// let decoded: MyStruct = decode(&mut data).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn encode_to<T: Encoder>(value: &T, writer: &mut BytesMut) -> Result<()> {
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
    /// This method includes field IDs and type tags for forward/backward
    /// compatibility. Use this when you need schema evolution support.
    ///
    /// # Arguments
    /// * `writer` - The buffer to write the encoded bytes into.
    fn encode(&self, writer: &mut BytesMut) -> Result<()>;

    /// Returns true if this value equals its default value.
    /// Used by `#[senax(skip_default)]` attribute to skip encoding default
    /// values.
    fn is_default(&self) -> bool;
}

/// Trait for types that can be packed into a compact binary format.
///
/// This trait provides compact serialization without schema evolution support.
/// Use this when you need maximum performance and don't require
/// forward/backward compatibility.
///
/// # Errors
/// Returns `EncoderError` if the value cannot be packed.
pub trait Packer {
    /// Pack the value into the given buffer without schema evolution support.
    ///
    /// This method stores data in a compact format without field IDs or type
    /// tags. The format is not schema-evolution-friendly but offers better
    /// performance.
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
/// Returns `EncoderError` if the value cannot be decoded or the data is
/// invalid.
pub trait Decoder: Sized {
    /// Decode the value from the given buffer with schema evolution support.
    ///
    /// This method expects field IDs and type tags for forward/backward
    /// compatibility. Use this when you need schema evolution support.
    ///
    /// # Arguments
    /// * `reader` - The buffer to read the encoded bytes from.
    fn decode(reader: &mut impl Buf) -> Result<Self>;
}

/// A senax-encoded dynamically tagged value.
///
/// This is a small typetag-like building block for extension points. The wire
/// shape is:
///
/// ```text
/// [tag: String] [body: Bytes]
/// ```
///
/// `body` is length-delimited, so decoders can skip unknown tags without
/// preserving their bytes.
pub trait TaggedSenax: Encoder + Decoder {
    /// Stable type tag for this payload inside its extension point.
    const TAG: &'static str;
}

/// Declares a separate inventory-backed senax tagged trait-object extension
/// point.
///
/// Each invocation creates an independent trait, registry entry type, unknown
/// fallback type, and `Encoder`/`Decoder` impls for `Box<dyn Trait>`.
///
/// ```ignore
/// senax_encoder::declare_senax_tagged_trait!(
///     pub trait ProviderData,
///     unknown = UnknownProviderData
/// );
/// ```
#[macro_export]
macro_rules! declare_senax_tagged_trait {
    (
        $vis:vis trait $trait_name:ident,
        unknown = $unknown_name:ident $(,)?
    ) => {
        $crate::__private::paste::paste! {
            $vis trait $trait_name: std::fmt::Debug + Send + Sync + 'static {
                fn tag(&self) -> &str;
                fn encode_tagged_body(
                    &self,
                    writer: &mut bytes::BytesMut,
                ) -> $crate::Result<()>;
                fn as_any(&self) -> &dyn std::any::Any;
            }

            impl<T> $trait_name for T
            where
                T: $crate::TaggedSenax + std::fmt::Debug + Send + Sync + 'static,
            {
                fn tag(&self) -> &str {
                    <T as $crate::TaggedSenax>::TAG
                }

                fn encode_tagged_body(
                    &self,
                    writer: &mut bytes::BytesMut,
                ) -> $crate::Result<()> {
                    $crate::Encoder::encode(self, writer)
                }

                fn as_any(&self) -> &dyn std::any::Any {
                    self
                }
            }

            #[derive(Debug, Clone, PartialEq, Eq)]
            $vis struct $unknown_name {
                pub tag: String,
            }

            impl $trait_name for $unknown_name {
                fn tag(&self) -> &str {
                    &self.tag
                }

                fn encode_tagged_body(
                    &self,
                    _writer: &mut bytes::BytesMut,
                ) -> $crate::Result<()> {
                    Ok(())
                }

                fn as_any(&self) -> &dyn std::any::Any {
                    self
                }
            }

            #[doc(hidden)]
            pub struct [<__Senax $trait_name Entry>] {
                pub tag: &'static str,
                pub decode: fn(bytes::Bytes) -> $crate::Result<Box<dyn $trait_name>>,
            }

            impl [<__Senax $trait_name Entry>] {
                pub const fn new(
                    tag: &'static str,
                    decode: fn(bytes::Bytes) -> $crate::Result<Box<dyn $trait_name>>,
                ) -> Self {
                    Self { tag, decode }
                }
            }

            inventory::collect!([<__Senax $trait_name Entry>]);

            impl $crate::Encoder for Box<dyn $trait_name> {
                fn encode(&self, writer: &mut bytes::BytesMut) -> $crate::Result<()> {
                    self.tag().to_owned().encode(writer)?;
                    let mut body = bytes::BytesMut::new();
                    self.encode_tagged_body(&mut body)?;
                    body.freeze().encode(writer)
                }

                fn is_default(&self) -> bool {
                    false
                }
            }

            impl $crate::Decoder for Box<dyn $trait_name> {
                fn decode(reader: &mut impl bytes::Buf) -> $crate::Result<Self> {
                    let tag = String::decode(reader)?;
                    let body = bytes::Bytes::decode(reader)?;
                    for entry in inventory::iter::<[<__Senax $trait_name Entry>]> {
                        if entry.tag == tag {
                            return (entry.decode)(body);
                        }
                    }
                    Ok(Box::new($unknown_name { tag }))
                }
            }
        }
    };
}

/// Registers one concrete type with a trait-specific registry declared by
/// [`declare_senax_tagged_trait!`], and implements [`TaggedSenax`] for it.
#[macro_export]
macro_rules! register_senax_tagged {
    (
        trait = $trait_name:ident,
        type = $ty:ty,
        tag = $tag:expr $(,)?
    ) => {
        impl $crate::TaggedSenax for $ty {
            const TAG: &'static str = $tag;
        }

        $crate::__private::paste::paste! {
            inventory::submit! {
                [<__Senax $trait_name Entry>]::new(
                    $tag,
                    |mut body: bytes::Bytes| -> $crate::Result<Box<dyn $trait_name>> {
                        use bytes::Buf as _;
                        let value = <$ty as $crate::Decoder>::decode(&mut body)?;
                        if body.remaining() != 0 {
                            return Err($crate::EncoderError::Decode(format!(
                                "Trailing bytes while decoding registered tagged senax value '{}': {}",
                                $tag,
                                body.remaining()
                            )));
                        }
                        Ok(Box::new(value) as Box<dyn $trait_name>)
                    },
                )
            }
        }
    };
}

/// Trait for types that can be unpacked from a compact binary format.
///
/// This trait provides compact deserialization without schema evolution
/// support. Use this when you need maximum performance and don't require
/// forward/backward compatibility.
///
/// # Errors
/// Returns `EncoderError` if the value cannot be unpacked or the data is
/// invalid.
pub trait Unpacker: Sized {
    /// Unpack the value from the given buffer without schema evolution support.
    ///
    /// This method reads data from a compact format without field IDs or type
    /// tags. The format is not schema-evolution-friendly but offers better
    /// performance.
    ///
    /// # Arguments
    /// * `reader` - The buffer to read the packed bytes from.
    fn unpack(reader: &mut impl Buf) -> Result<Self>;
}

/// Convenience function to pack a value to bytes.
///
/// The packed format is compact but not schema-evolution-friendly.
///
/// # Arguments
/// * `value` - The value to pack.
///
/// # Example
/// ```rust
/// use bytes::BytesMut;
/// use senax_encoder::{Pack, Unpack, pack, unpack};
///
/// #[derive(Pack, Unpack, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct {
///     id: 42,
///     name: "hello".to_string(),
/// };
/// let mut buf = pack(&value).unwrap();
/// let decoded: MyStruct = unpack(&mut buf).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn pack<T: Packer>(value: &T) -> Result<Bytes> {
    let mut writer = BytesMut::new();
    value.pack(&mut writer)?;
    Ok(writer.freeze())
}

/// Convenience function to pack a value to an existing BytesMut buffer.
///
/// The packed format is compact but not schema-evolution-friendly.
///
/// # Arguments
/// * `value` - The value to pack.
/// * `writer` - The buffer to write the packed bytes into.
///
/// # Example
/// ```rust
/// use bytes::{Bytes, BytesMut};
/// use senax_encoder::{Pack, Unpack, pack_to, unpack};
///
/// #[derive(Pack, Unpack, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct {
///     id: 42,
///     name: "hello".to_string(),
/// };
/// let mut buf = BytesMut::new();
/// pack_to(&value, &mut buf).unwrap();
/// let mut data = buf.freeze();
/// let decoded: MyStruct = unpack(&mut data).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn pack_to<T: Packer>(value: &T, writer: &mut BytesMut) -> Result<()> {
    value.pack(writer)
}

/// Convenience function to unpack a value from bytes.
///
/// The packed format is compact but not schema-evolution-friendly.
///
/// # Arguments
/// * `reader` - The buffer to read the packed bytes from.
///
/// # Example
/// ```rust
/// use bytes::BytesMut;
/// use senax_encoder::{Pack, Unpack, pack, unpack};
///
/// #[derive(Pack, Unpack, PartialEq, Debug)]
/// struct MyStruct {
///     id: u32,
///     name: String,
/// }
///
/// let value = MyStruct {
///     id: 42,
///     name: "hello".to_string(),
/// };
/// let mut buf = pack(&value).unwrap();
/// let decoded: MyStruct = unpack(&mut buf).unwrap();
/// assert_eq!(value, decoded);
/// ```
pub fn unpack<T: Unpacker>(reader: &mut impl Buf) -> Result<T> {
    T::unpack(reader)
}

impl<'a, B> Encoder for std::borrow::Cow<'a, B>
where
    B: ToOwned + ?Sized,
    B::Owned: Encoder,
{
    fn encode(&self, writer: &mut bytes::BytesMut) -> Result<()> {
        self.as_ref().to_owned().encode(writer)
    }

    fn is_default(&self) -> bool {
        self.as_ref().to_owned().is_default()
    }
}

impl<'a, B> Decoder for std::borrow::Cow<'a, B>
where
    B: ToOwned + ?Sized,
    B::Owned: Decoder,
{
    fn decode(reader: &mut impl bytes::Buf) -> Result<Self> {
        Ok(Self::Owned(B::Owned::decode(reader)?))
    }
}
