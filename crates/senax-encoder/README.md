[![Latest version](https://img.shields.io/crates/v/senax-encoder.svg)](https://crates.io/crates/senax-encoder)

# senax-encoder

A fast, compact, and schema-evolution-friendly binary serialization library for Rust.

- Supports struct/enum encoding with field/variant IDs for forward/backward compatibility
- Efficient encoding for primitives, collections, Option, String, bytes, and popular crates (chrono, uuid, ulid, rust_decimal, bigdecimal, indexmap, fxhash, ahash, smol_str, serde_json)
- Custom derive macros for ergonomic usage
- Feature-gated support for optional dependencies

## Features

- Compact, efficient encoding for a wide range of types (primitives, collections, Option, String, bytes, chrono, uuid, ulid, rust_decimal, bigdecimal, indexmap, serde_json)
- Schema evolution and version compatibility via field/variant IDs and tag-based format
- Attribute macros for fine-grained control (custom IDs, default values, skip encode/decode, renaming, compact ID encoding)
- Feature flags for optional support of popular crates
- Suitable for network protocols, storage, and applications requiring forward/backward compatibility

## Attribute Macros

You can control encoding/decoding behavior using the following attributes:

### Container-level attributes (struct/enum level):
- `#[senax(disable_encode)]` — Generates stub implementations (unimplemented!) for `Encode` and `Decode` traits. Useful for improving build efficiency during development when you're not yet ready to fully implement serialization.
- `#[senax(disable_pack)]` — Generates stub implementations (unimplemented!) for `Pack` and `Unpack` traits. Can be combined with `disable_encode`.

### Field-level attributes:
- `#[senax(id = N)]` — Assigns a custom field or variant ID (u64). Ensures stable wire format across versions.
- `#[senax(default)]` — If a field is missing during decoding, its value is set to `Default::default()` instead of causing an error. For `Option<T>`, this means `None`.
- `#[senax(skip_encode)]` — This field is not written during encoding. On decode, it is set to `Default::default()`.
- `#[senax(skip_decode)]` — This field is ignored during decoding and always set to `Default::default()`. It is still encoded if present.
- `#[senax(skip_default)]` — This field is not written during encoding if its value equals the default value. On decode, missing fields are set to `Default::default()`.
- `#[senax(rename = "name")]` — Use the given string as the logical field/variant name for ID calculation. Useful for renaming fields/variants while keeping the same wire format.

### Example: Development-time stubs
```rust
// Disable Encode/Decode during development
#[derive(Encode, Decode)]
#[senax(disable_encode)]
struct WorkInProgress {
    #[senax(id=1)]
    field1: i32,
}

// Disable Pack/Unpack only
#[derive(Pack, Unpack)]
#[senax(disable_pack)]
struct AnotherStruct {
    field1: i32,
}

// Disable both
#[derive(Encode, Decode, Pack, Unpack)]
#[senax(disable_encode, disable_pack)]
struct AllDisabled {
    field1: i32,
}
```

## Feature Flags

The following optional features enable support for popular crates and types:

### External Crate Support
- `chrono` — Enables encoding/decoding of `chrono::DateTime`, `NaiveDate`, and `NaiveTime` types.
- `uuid` — Enables encoding/decoding of `uuid::Uuid`.
- `ulid` — Enables encoding/decoding of `ulid::Ulid` (shares the same tag as UUID for binary compatibility).
- `rust_decimal` — Enables encoding/decoding of `rust_decimal::Decimal`.
- `bigdecimal` — Enables encoding/decoding of `bigdecimal::BigDecimal` (stored as scientific notation string).
- `indexmap` — Enables encoding/decoding of `IndexMap` and `IndexSet` collections.
- `fxhash` — Enables encoding/decoding of `fxhash::FxHashMap` and `fxhash::FxHashSet` (fast hash collections).
- `ahash` — Enables encoding/decoding of `ahash::AHashMap` and `ahash::AHashSet` (high-performance hash collections).
- `smol_str` — Enables encoding/decoding of `smol_str::SmolStr` (small string optimization).
- `serde_json` — Enables encoding/decoding of `serde_json::Value` for dynamic JSON data.

## Quick Start

Add to your `Cargo.toml`:
```toml
[dependencies]
senax-encoder = "0.1"
```

Basic usage:
```rust
use senax_encoder::{Encode, Decode};

#[derive(Encode, Decode, Debug, PartialEq)]
struct User {
    id: u32,
    name: String,
    email: Option<String>,
}

let user = User { id: 42, name: "Alice".into(), email: Some("alice@example.com".into()) };

// Schema evolution support (with field IDs)
let mut bytes = senax_encoder::encode(&user).unwrap();
let decoded: User = senax_encoder::decode(&mut bytes).unwrap();
assert_eq!(user, decoded);

// Compact encoding (without field IDs, smaller size)
let mut packed = senax_encoder::pack(&user).unwrap();
let unpacked: User = senax_encoder::unpack(&mut packed).unwrap();
assert_eq!(user, unpacked);
```

## Usage

### 1. Derive macros for automatic implementation
```rust
#[derive(Encode, Decode)]
struct MyStruct {
    #[senax(id=1)]
    foo: u32,
    bar: Option<String>,
}
```

### 2. Binary encode/decode
```rust
let mut bytes = senax_encoder::encode(&value)?;
let value2: MyStruct = senax_encoder::decode(&mut bytes)?;
```

### 3. Compact pack/unpack (without schema evolution)
```rust
// Pack for maximum compactness (no field IDs, smaller size)
let mut bytes = senax_encoder::pack(&value)?;
let value2: MyStruct = senax_encoder::unpack(&mut bytes)?;

// Note: pack/unpack is field-order dependent and doesn't support schema evolution
// Use when you need maximum performance and size optimization
```

### 4. Schema evolution (adding/removing/changing fields)
- Field IDs are **automatically generated from field names (CRC64)** by default.
  - Use `#[senax(id=...)]` only if you need to resolve a collision.
- Because mapping is by field ID (u64):
  - **Old struct → new struct**:
    - New fields of type `Option` become `None` if missing.
    - New required fields without `default` will cause a decode error if missing.
  - **New struct → old struct**: unknown fields are automatically skipped.
- **No field names are stored, only u64 IDs, so field addition/removal/reordering/type changes are robust.**

### 5. Feature flags
- Enable only the types you need: `indexmap`, `chrono`, `rust_decimal`, `uuid`, `ulid`, `serde_json`, etc.
- Minimizes dependencies and build time.

## Supported Types

### Core Types (always available)
- Primitives: `u8~u128`, `i8~i128`, `f32`, `f64`, `bool`, `String`, `Bytes` (zero-copy binary data)
- Option, Vec, arrays, HashMap, BTreeMap, Set, Tuple, Enum, Struct, Arc, Box

### Feature-gated Types
When respective features are enabled:

- **chrono**: `DateTime<Utc>`, `DateTime<Local>`, `NaiveDate`, `NaiveTime`
- **uuid**: `Uuid`
- **ulid**: `Ulid`
- **rust_decimal**: `Decimal`
- **bigdecimal**: `BigDecimal` (stored as scientific notation string)
- **indexmap**: `IndexMap`, `IndexSet`
- **fxhash**: `FxHashMap`, `FxHashSet` (fast hash collections)
- **ahash**: `AHashMap`, `AHashSet` (high-performance hash collections)
- **smol_str**: `SmolStr` (small string optimization)
- **serde_json**: `Value` (dynamic JSON data)

## Type Compatibility and Cross-Decoding

The senax-encoder supports automatic type conversion for compatible types during decoding, enabling schema evolution. However, certain conversions are not supported due to precision or data loss concerns.

### ✅ Supported Cross-Type Decoding

- **Integer types**: Any unsigned integer can be decoded as a larger unsigned integer (e.g., `u16` → `u32`)
- **Signed integers**: Can be decoded as larger signed integers if the value fits within the target range
- **Unsigned to signed**: Supported if the value fits within the signed type's positive range
- **Floating point**: `f64` can be decoded as `f32` (with potential precision loss)
- **Float cross-decoding**: `f32` ↔ `f64` (bidirectional since v0.2.2 with string format)
- **Integer to float**: Any integer type (including `i128`) can be decoded as `f32` or `f64`
- **Integer to decimal**: Any integer type can be decoded as `rust_decimal::Decimal` or `bigdecimal::BigDecimal`
- **Container expansion**: `T` can be decoded as `Option<T>`

### ❌ Unsupported Cross-Type Decoding

- **Signed to unsigned**: Negative values cannot be decoded as unsigned types
- **Integer overflow**: Values too large for the target type will cause decode errors
- **Container shrinking**: `Option<T>` cannot be automatically decoded as `T` (use explicit handling)
- **Float to integer**: Floating-point values cannot be decoded as integer types

### ⚠️ Important Notes

- **Type changes are automatically applied when compatible**, but incompatible conversions will result in decode errors.
- **Always test schema evolution scenarios** with actual data before deploying changes.
- **For critical applications**, prefer explicit type versioning over relying on automatic conversion.
- **Float precision**: When working with floating-point numbers, use the same precision consistently to avoid conversion issues.

Example of compatible schema evolution:
```rust
// Version 1
#[derive(Encode, Decode)]
struct User {
    id: u32,        // Will be compatible with u64 in v2
    name: String,
}

// Version 2 - Compatible changes
#[derive(Encode, Decode)]  
struct User {
    id: u64,                    // ✅ u32 → u64 automatic conversion
    name: String,
    email: Option<String>,      // ✅ New optional field
    #[senax(default)]
    age: u32,                   // ✅ New field with default
}
```

## Custom Encoder/Decoder Implementation

When implementing custom `Encoder` and `Decoder` traits for your types, follow these important guidelines to ensure proper binary format consistency:

### ✅ Best Practices

- **Single encode call**: Each value should be encoded with exactly one `encode()` call that writes all necessary data atomically.
- **Use tuples for multiple values**: If you need to encode multiple related values, group them into a tuple rather than making separate encode calls.
- **Error handling**: Always check for insufficient data in your decoder and return appropriate errors.

### ❌ Common Mistakes to Avoid

```rust
// ❌ WRONG: Multiple separate encode calls
impl Encoder for MyType {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        self.field1.encode(writer)?;  // First encode call
        self.field2.encode(writer)?;  // Second encode call - WRONG!
        Ok(())
    }
}

// ✅ CORRECT: Single encode call with tuple
impl Encoder for MyType {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        (self.field1, self.field2).encode(writer)  // Single encode call
    }
}
```

### Manual Implementation Example

```rust
use senax_encoder::{Encoder, Decoder, EncoderError};
use bytes::{BytesMut, Bytes};

struct Point3D {
    x: f64,
    y: f64, 
    z: f64,
}

impl Encoder for Point3D {
    fn encode(&self, writer: &mut BytesMut) -> senax_encoder::Result<()> {
        // ✅ Encode as single tuple
        (self.x, self.y, self.z).encode(writer)
    }

    fn is_default(&self) -> bool {
        self.x == 0.0 && self.y == 0.0 && self.z == 0.0
    }
}

impl Decoder for Point3D {
    fn decode(reader: &mut Bytes) -> senax_encoder::Result<Self> {
        // ✅ Decode the same tuple structure
        let (x, y, z) = <(f64, f64, f64)>::decode(reader)?;
        Ok(Point3D { x, y, z })
    }
}
```

### Advanced Example with Complex Data

```rust
struct CustomFormat {
    header: String,
    data: Vec<u8>,
    checksum: u32,
}

impl Encoder for CustomFormat {
    fn encode(&self, writer: &mut BytesMut) -> senax_encoder::Result<()> {
        // ✅ Group all fields into a single tuple
        (
            &self.header,
            &self.data, 
            self.checksum
        ).encode(writer)
    }

    fn is_default(&self) -> bool {
        self.header.is_empty() && self.data.is_empty() && self.checksum == 0
    }
}

impl Decoder for CustomFormat {
    fn decode(reader: &mut Bytes) -> senax_encoder::Result<Self> {
        // ✅ Decode the same tuple structure
        let (header, data, checksum) = <(String, Vec<u8>, u32)>::decode(reader)?;
        Ok(CustomFormat { header, data, checksum })
    }
}
```

### Why This Matters

- **Format consistency**: Each value gets exactly one tag in the binary format
- **Schema evolution**: The library can properly skip unknown fields during forward/backward compatibility


**Note**: For most use cases, prefer using `#[derive(Encode, Decode)]` which automatically follows these best practices.

## License

Licensed under either of

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

#### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
