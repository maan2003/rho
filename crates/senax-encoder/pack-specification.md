# senax-encoder Pack Format Specification

**Version:** 1.0  
**Date:** 2025  
**Status:** Draft

## Table of Contents

1. [Overview](#overview)
2. [Format Basics](#format-basics)
3. [Data Type Specifications](#data-type-specifications)
4. [Struct and Enum Packing](#struct-and-enum-packing)
5. [Optimization Features](#optimization-features)
6. [Implementation Notes](#implementation-notes)

## 1. Overview

The senax-encoder pack format is designed for maximum compactness and performance when schema evolution is not required. Unlike the encode format, pack does not include field IDs for struct fields, resulting in significantly smaller binary size but requiring strict field order compatibility.

### Key Design Principles

- **Maximum Compactness:** No field IDs for struct fields, minimal overhead
- **High Performance:** Direct binary representation without metadata
- **Order Dependent:** Field order must match exactly between pack/unpack
- **Little Endian:** Consistent byte order across platforms

### Pack vs Encode Comparison

| Feature | Pack Format | Encode Format |
|---------|-------------|---------------|
| Binary Size | Smaller | Larger |
| Schema Evolution | Not Supported | Full Support |
| Field Order | Must Match | Independent |
| Performance | Faster | Moderate |
| Use Case | High-performance, stable schemas | Evolving APIs, long-term storage |

## 2. Format Basics

### 2.1 Magic Numbers

The senax-encoder library uses magic numbers to distinguish between different serialization formats when using convenience functions:

**Encode Format:** `0xA55A` (2 bytes, little-endian)
- Used by `encode()` and `encode_to()` functions
- Supports schema evolution with field IDs and type tags
- Binary starts with: `[0x5A, 0xA5]`

**Pack Format:** `0xDADA` (2 bytes, little-endian)  
- Used by `pack()` and `pack_to()` functions
- Compact format without schema evolution support
- Binary starts with: `[0xDA, 0xDA]`

**Important Notes:**
- Magic numbers are **only added by convenience functions** (`pack()`, `encode()`, etc.)
- Direct trait method calls (`Packer::pack`, `Encoder::encode`) do **not** include magic numbers
- The `unpack()` and `decode()` functions validate and consume the magic numbers
- This specification describes the format **after** magic number (raw pack format)

### 2.2 Byte Order

All multi-byte integers are encoded in **little-endian** format.

### 2.3 Basic Structure

Pack format uses direct binary representation with minimal tagging:

**Primitives:**
```
[DATA:variable]  // Direct encoding without type tags (except for optimizations)
```

**Containers:**
```
[LENGTH:variable] [DATA:variable]  // Length-prefixed for collections
```

**Structs:**
```
[HASH:u64_le] [FIELD1] [FIELD2] ...  // Named structs: structure hash + fields in order
[COUNT:variable] [FIELD1] [FIELD2] ...  // Tuple structs: field count + fields in order
// (no data)  // Unit structs: no additional data
```

**Enums:**
```
[VARIANT_ID:variable] [HASH:u64_le] [FIELD1] [FIELD2] ...  // Named variants
[VARIANT_ID:variable] [COUNT:variable] [FIELD1] [FIELD2] ...  // Tuple variants  
[VARIANT_ID:variable]  // Unit variants
```

### 2.4 Structure Hash

Named structs and enum variants include a CRC64 structure hash for validation:
- Computed from type name, field names, and field types
- Ensures pack/unpack compatibility
- Detects structural mismatches at runtime

## 3. Data Type Specifications

### 3.1 Boolean

**Pack Format:**
- `false`: `0x00`
- `true`: `0x01`

**Unpack Behavior:**
- `0x00` → `false`
- Any non-zero value → `true` (relaxed validation)

### 3.2 Unsigned Integers (u16, u32, u64, u128, usize)

**Pack Format (Variable-Length Encoding):**

All unsigned integers except u8 use the same variable-length encoding as the encode format:

```
// Small values (0-127)
value -> [TAG_ZERO + value]  // Direct encoding in tag byte

// Medium values (128-383)  
value -> [TAG_U8] [value - 128]  // 2 bytes total

// Larger values use appropriate width tags
u16 -> [TAG_U16] [value:u16_le]  // if value > 383
u32 -> [TAG_U32] [value:u32_le]  // if value > 65535
u64 -> [TAG_U64] [value:u64_le]  // if value > 4294967295
u128 -> [TAG_U128] [value:u128_le]  // if value > 18446744073709551615
usize -> (same as u32 or u64 depending on platform)
```

**Tags:**
- `TAG_ZERO = 0x00` to `TAG_U8_127 = 0x7F` (values 0-127)
- `TAG_U8 = 0x83` (131)
- `TAG_U16 = 0x84` (132)
- `TAG_U32 = 0x85` (133)
- `TAG_U64 = 0x86` (134)
- `TAG_U128 = 0x87` (135)

**Size Examples:**
- 0-127: 1 byte
- 128-383: 2 bytes
- 384-65535: 3 bytes (u16)
- 65536-4294967295: 5 bytes (u32)
- etc.

### 3.3 u8 (Special Case)

**Pack Format:**
```
u8 -> [value:u8]  // Direct 1-byte encoding without tags
```

**Size:** Always 1 byte (no tag overhead)

### 3.4 Signed Integers (i8, i16, i32, i64, i128, isize)

**Pack Format (Variable-Length Encoding):**

All signed integers use the same variable-length encoding as the encode format:

```
// Positive values (0 and above)
value -> (encoded as corresponding unsigned type)

// Negative values  
value -> [TAG_NEGATIVE] [encoded_inverted_value]
```

Where `encoded_inverted_value` is `!(value as unsigned_type)` encoded using the unsigned format.

**Tag:**
- `TAG_NEGATIVE = 0x88` (136)

**Examples:**
```
0     -> [0x00]                    // 1 byte
42    -> [0x2A]                    // 1 byte  
-1    -> [0x88] [0x00]             // 2 bytes (TAG_NEGATIVE + inverted 0)
-42   -> [0x88] [0x29]             // 2 bytes (TAG_NEGATIVE + inverted 41)
1000  -> [0x84] [0xE8, 0x03]       // 3 bytes (TAG_U16 + 1000 as u16_le)
-1000 -> [0x88] [0x84] [0xE7, 0x03] // 4 bytes (TAG_NEGATIVE + TAG_U16 + inverted 999)
```

### 3.5 Floating Point (f32, f64)

**Pack Format with Optimization:**
```
f32 -> [TAG_NONE]                    // if value == 0.0
    -> [TAG_F32] [value:f32_le]      // if value != 0.0

f64 -> [TAG_NONE]                    // if value == 0.0  
    -> [TAG_F64] [value:f64_le]      // if value != 0.0
```

**Tags:**
- `TAG_NONE = 0x80`
- `TAG_F32 = 0x89`
- `TAG_F64 = 0x8A`

**Size:**
- 0.0 values: 1 byte
- Non-zero f32: 5 bytes (1 tag + 4 data)
- Non-zero f64: 9 bytes (1 tag + 8 data)

### 3.6 Character (char)

**Pack Format (Variable-Length Encoding):**
```
char -> (encoded as u32 using variable-length format)
```

The Unicode code point is encoded using the same variable-length format as u32:
- Small code points (0-127): 1 byte
- Medium code points (128-383): 2 bytes  
- Larger code points: 3-5 bytes depending on value

**Size:** 1-5 bytes depending on Unicode code point value

### 3.7 String

**Pack Format:**
```
// Short strings (0-40 characters)
String -> [TAG_STRING_BASE + len] [utf8_bytes]

// Long strings (41+ characters)  
String -> [TAG_STRING_LONG] [len:variable] [utf8_bytes]
```

**Tags:**
- `TAG_STRING_BASE = 0x8B` (139)
- `TAG_STRING_LONG = 0xB4` (180)

**Examples:**
```
""      -> [0x8B]                    // Empty string
"Hi"    -> [0x8D, 0x48, 0x69]        // 2-char string
"Long..." -> [0xB4, len, utf8_bytes] // Long string
```

### 3.8 Option<T>

**Pack Format:**
```
None    -> [TAG_NONE]
Some(v) -> [TAG_SOME] [packed_value]
```

**Tags:**
- `TAG_NONE = 0x80`
- `TAG_SOME = 0x81`

### 3.9 Vec<T>

**Pack Format:**
```
// Short vectors (0-5 elements)
Vec<T> -> [TAG_ARRAY_VEC_SET_BASE + len] [element1] [element2] ...

// Long vectors (6+ elements)
Vec<T> -> [TAG_ARRAY_VEC_SET_LONG] [len:variable] [element1] [element2] ...
```

**Tags:**
- `TAG_ARRAY_VEC_SET_BASE = 0xBC` (188)
- `TAG_ARRAY_VEC_SET_LONG = 0xC2` (194)

### 3.10 Arrays [T; N]

**Pack Format:**
```
// Short arrays (0-5 elements)
[T; N] -> [TAG_ARRAY_VEC_SET_BASE + N] [element1] [element2] ... [elementN]

// Long arrays (6+ elements)
[T; N] -> [TAG_ARRAY_VEC_SET_LONG] [N:variable] [element1] [element2] ... [elementN]
```

**Tags:**
- `TAG_ARRAY_VEC_SET_BASE = 0xBC` (188)
- `TAG_ARRAY_VEC_SET_LONG = 0xC2` (194)

**Note:** Array length N is encoded and validated against the expected size during unpack.

### 3.11 HashMap<K, V>

**Pack Format:**
```
HashMap -> [TAG_MAP] [len:variable] [key1] [value1] [key2] [value2] ...
```

**Tag:**
- `TAG_MAP = 0xC4` (196)

### 3.12 Tuples

**Pack Format:**
```
() -> [TAG_TUPLE] [0:variable]
(T1,) -> [TAG_TUPLE] [1:variable] [element1]
(T1, T2) -> [TAG_TUPLE] [2:variable] [element1] [element2]
...
```

**Tag:**
- `TAG_TUPLE = 0xC3` (195)

### 3.13 Bytes

**Pack Format:**
```
Bytes -> [TAG_BINARY] [len:variable] [byte_data]
```

**Tag:**
- `TAG_BINARY = 0xB5` (181)

### 3.14 Extended Types (Feature-Dependent)

#### DateTime (chrono feature)

**Pack Format:**
```
DateTime<Utc> -> [TAG_NONE]                          // if default value
              -> [TAG_CHRONO_DATETIME] [seconds:i64] [nanos:u32]  // if non-default

DateTime<Local> -> [TAG_NONE]                        // if default value  
                -> [TAG_CHRONO_DATETIME] [seconds:i64] [nanos:u32]  // if non-default
```

All DateTime types are normalized to UTC for storage.

**Tags:**
- `TAG_NONE = 0x80`
- `TAG_CHRONO_DATETIME = 0xC5` (197)

#### NaiveDate (chrono feature)

**Pack Format:**
```
NaiveDate -> [TAG_CHRONO_NAIVE_DATE] [days_from_epoch:i64]
```

**Tag:**
- `TAG_CHRONO_NAIVE_DATE = 0xC6` (198)

**Epoch:** 1970-01-01

#### NaiveTime (chrono feature)

**Pack Format:**
```
NaiveTime -> [TAG_CHRONO_NAIVE_TIME] [seconds_from_midnight:u32] [nanoseconds:u32]
```

**Tag:**
- `TAG_CHRONO_NAIVE_TIME = 0xC7` (199)

#### NaiveDateTime (chrono feature)

**Pack Format:**
```
NaiveDateTime -> [TAG_NONE]                              // if default value
              -> [TAG_CHRONO_NAIVE_DATETIME] [seconds:i64] [nanos:u32]  // if non-default
```

Stores as seconds and nanoseconds since Unix epoch (1970-01-01 00:00:00 UTC).

**Tags:**
- `TAG_NONE = 0x80`
- `TAG_CHRONO_NAIVE_DATETIME = 0xD0` (208)

#### Decimal (rust_decimal feature)

**Pack Format:**
```
Decimal -> [TAG_DECIMAL] [mantissa:i128] [scale:u32]
```

**Tag:**
- `TAG_DECIMAL = 0xC8` (200)

#### UUID/ULID (uuid/ulid features)

**Pack Format:**
```
UUID -> [TAG_NONE]               // if default/nil value
     -> [TAG_UUID] [value:u128_le]  // if non-default

ULID -> [TAG_NONE]               // if default/nil value  
     -> [TAG_UUID] [value:u128_le]  // if non-default
```

**Tags:**
- `TAG_NONE = 0x80`
- `TAG_UUID = 0xC9` (201)

**Note:** UUID and ULID share the same tag and are binary compatible at the encoding level.

## 4. Struct and Enum Packing

### 4.1 Named Structs

**Pack Format:**
```
struct MyStruct {
    field1: T1,
    field2: T2,
} -> [structure_hash:u64_le] [packed_field1] [packed_field2]
```

**Structure Hash:** CRC64 of "type:MyStruct|struct|named|field1:T1|field2:T2"

**Example:**
```rust
struct User {
    id: u32,
    name: String,
}

// Packed as: [hash:u64] [id:u32] [name:String]
```

### 4.2 Tuple Structs

**Pack Format:**
```
struct MyStruct(T1, T2) -> [field_count:variable] [packed_field1] [packed_field2]
```

**Example:**
```rust
struct Point(f32, f32);

// Packed as: [2:variable] [x:f32] [y:f32]
```

### 4.3 Unit Structs

**Pack Format:**
```
struct MyStruct; -> // (no additional data)
```

### 4.4 Enums

#### Named Variants
```
enum MyEnum {
    Variant { field1: T1, field2: T2 }
} -> [variant_id:variable] [structure_hash:u64_le] [packed_field1] [packed_field2]
```

#### Tuple Variants
```
enum MyEnum {
    Variant(T1, T2)
} -> [variant_id:variable] [field_count:variable] [packed_field1] [packed_field2]
```

#### Unit Variants
```
enum MyEnum {
    Variant
} -> [variant_id:variable]
```

**Variant IDs:** CRC64 hash of variant name (same as encode format)

## 5. Optimization Features

### 5.1 Zero/Default Value Optimization

**f32/f64:** 0.0 values use single `TAG_NONE` byte instead of full representation.

**chrono types:** Default values use single `TAG_NONE` byte:
- `DateTime<Utc>::default()` → `[TAG_NONE]`
- `DateTime<Local>::default()` → `[TAG_NONE]`
- `NaiveDateTime::default()` → `[TAG_NONE]`

**UUID/ULID:** Nil/default values use single `TAG_NONE` byte:
- `Uuid::nil()` → `[TAG_NONE]`
- `Ulid::nil()` → `[TAG_NONE]`

### 5.2 Short Collection Optimization

**Vectors/Arrays/Sets:** Collections with 0-5 elements encode length in the tag byte.

### 5.3 Short String Optimization

**Strings:** Strings with 0-40 characters encode length in the tag byte.

### 5.4 Direct Integer Encoding

**Integers:** Variable-length encoding with optimized representation for small values.

## 6. Implementation Notes

### 6.1 Field Order Dependency

**Critical:** Pack format requires exact field order matching between pack and unpack operations.

```rust
// This works
struct A { x: u32, y: String }
struct B { x: u32, y: String }  // Same order

// This fails  
struct A { x: u32, y: String }
struct C { y: String, x: u32 }  // Different order
```

### 6.2 Structure Hash Validation

Named structs and enum variants validate structure hash during unpack:
- Prevents accidental type mismatches
- Detects field reordering
- Ensures pack/unpack compatibility

### 6.3 Cross-Type Compatibility

Pack format supports the same cross-type decoding as encode format:
- Integer widening (u16 → u32)
- Signed/unsigned conversion (where safe)

### 6.4 Performance Characteristics

**Pack Advantages:**
- 20-50% smaller binary size (no field IDs)
- Faster encoding/decoding (less metadata)
- Direct memory mapping potential

**Pack Limitations:**
- No schema evolution support
- Field order dependency
- Requires exact type matching

### 6.5 Use Case Guidelines

**Use Pack When:**
- Maximum performance is required
- Binary size is critical
- Schema is stable and controlled
- Both ends use the same code version

**Use Encode When:**
- Schema evolution is needed
- Long-term data storage
- API versioning is required
- Forward/backward compatibility is important

## 7. Examples

### 7.1 Simple Struct

```rust
#[derive(Pack, Unpack)]
struct Point {
    x: f32,
    y: f32,
}

let point = Point { x: 1.0, y: 2.0 };
// Packed: [hash:8] [TAG_F32][1.0:4] [TAG_F32][2.0:4] = 18 bytes
```

### 7.2 Enum with Mixed Variants

```rust
#[derive(Pack, Unpack)]
enum Message {
    #[senax(id=1)]
    Text(String),
    #[senax(id=2)]
    Data { id: u32, payload: Vec<u8> },
    Ping,
}

let msg = Message::Data { id: 42, payload: vec![1, 2, 3] };
// Packed: [variant_id] [hash:8] [42:1] [TAG_ARRAY+3][1][2][3] = 14 bytes
```

### 7.3 Optimization Examples

```rust
// Floating point optimization
let zero_f32 = 0.0f32;        // Packed: [TAG_NONE] = 1 byte
let nonzero_f32 = 3.14f32;    // Packed: [TAG_F32][3.14:4] = 5 bytes

// Collection optimization
let empty_vec: Vec<u8> = vec![];     // Packed: [TAG_ARRAY+0] = 1 byte  
let small_vec = vec![1u8, 2, 3];     // Packed: [TAG_ARRAY+3][1][2][3] = 4 bytes
let large_vec = vec![0u8; 100];      // Packed: [TAG_ARRAY_LONG][100][data] = ~103 bytes

// String optimization
let short_str = "Hi";                // Packed: [TAG_STRING+2][Hi] = 3 bytes
let empty_str = "";                  // Packed: [TAG_STRING+0] = 1 byte

// Chrono type optimization
use chrono::{DateTime, NaiveDateTime, Utc};
let default_dt = DateTime::<Utc>::default();  // Packed: [TAG_NONE] = 1 byte
let actual_dt = DateTime::from_timestamp(1640995200, 0).unwrap();  
// Packed: [TAG_CHRONO_DATETIME][seconds:i64][nanos:u32] = 13 bytes

let default_naive = NaiveDateTime::default();  // Packed: [TAG_NONE] = 1 byte
let actual_naive = DateTime::from_timestamp(1640995200, 123456789).unwrap().naive_utc();
// Packed: [TAG_CHRONO_NAIVE_DATETIME][seconds:i64][nanos:u32] = 13 bytes

// UUID optimization  
use uuid::Uuid;
let nil_uuid = Uuid::nil();          // Packed: [TAG_NONE] = 1 byte
let actual_uuid = Uuid::new_v4();    // Packed: [TAG_UUID][value:u128] = 17 bytes
```

## 8. Migration Guide

### 8.1 From Encode to Pack

1. Ensure schema stability
2. Verify field order consistency
3. Test with actual data
4. Measure size/performance gains

### 8.2 From Pack to Encode

1. Add field IDs if needed
2. Update error handling for unknown fields
3. Test schema evolution scenarios
4. Consider migration strategy for existing data
