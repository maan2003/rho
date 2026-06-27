# senax-encoder Binary Format Specification

**Version:** 1.1  
**Date:** 2025  
**Status:** Draft

## Table of Contents

1. [Overview](#overview)
2. [Format Basics](#format-basics)
3. [Tag System](#tag-system)
4. [Data Type Specifications](#data-type-specifications)
5. [Struct and Enum Encoding](#struct-and-enum-encoding)
6. [Schema Evolution](#schema-evolution)
7. [Implementation Notes](#implementation-notes)

## 1. Overview

The senax-encoder binary format is designed for efficient, compact serialization with a focus on forward and backward compatibility. Each value is tagged with a type identifier, enabling schema evolution and version compatibility.

### Key Design Principles

- **Compact Representation:** Variable-length encoding for common values
- **Self-describing:** Each value includes type information
- **Version Resilience:** Unknown fields/types can be safely skipped
- **Little Endian:** Consistent byte order across platforms

## 2. Format Basics

### 2.1 Byte Order

All multi-byte integers are encoded in **little-endian** format.

### 2.2 Basic Structure

All encoded values follow this pattern:
```
[TAG:u8] [DATA:variable]
```
Where:
- `TAG` is a single byte identifying the type and encoding method
- `DATA` is the encoded value, format depends on the tag

### 2.3 Variable-Length Integer Encoding

For optimal space efficiency, integers use variable-length encoding:
- Values 0-127: Encoded directly in the tag byte
- Larger values: Use dedicated tag + payload encoding
- Signed integers: Negative values use bit-inverted encoding (not ZigZag)

### 2.4 Optimized Field ID Encoding

Field IDs and variant IDs use an optimized encoding scheme for space efficiency:

**Encoding Rules:**
- **Field IDs 1-250**: Encoded as single `u8` byte
- **Field IDs 251+**: Encoded as `0xFF` marker byte followed by `u64` little-endian
- **Terminator**: Encoded as `0x00` byte to mark end of fields

**Format:**
```
// Small field ID (1-250)
[field_id:u8] [field_value]

// Large field ID (251+)  
[0xFF] [field_id:u64_le] [field_value]

// Terminator
[0x00]
```

**Size Benefits:**
- Most field IDs (1-250) use only 1 byte instead of 8 bytes
- Terminator uses 1 byte instead of 8 bytes
- Large field IDs (rare) use 9 bytes (1 marker + 8 data)

**Examples:**
```
field_id=1   -> [0x01]              // Direct u8 encoding
field_id=250 -> [0xFA]              // Direct u8 encoding (250 = 0xFA)
field_id=251 -> [0xFF, 0xFB, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]  // Marker + u64_le
terminator   -> [0x00]              // End of fields
```

This optimization significantly reduces binary size for typical structs and enums while maintaining full u64 field ID range support.

## 3. Tag System

### 3.1 Tag Assignment

Tags are assigned in ranges for semantic grouping:
```rust
pub const TAG_ZERO: u8 = 0;
pub const TAG_ONE: u8 = 1;
// 2-127: Direct encoding for values 2-127
pub const TAG_U8_127: u8 = 127;      // Value 127
// Extended integer types
pub const TAG_NONE: u8 = 128;
pub const TAG_SOME: u8 = 129;
pub const TAG_U8: u8 = 131;
pub const TAG_U16: u8 = 132;
pub const TAG_U32: u8 = 133;
pub const TAG_U64: u8 = 134;
pub const TAG_U128: u8 = 135;
pub const TAG_NEGATIVE: u8 = 136;
// Floating point
pub const TAG_F32: u8 = 137;
pub const TAG_F64: u8 = 138;
// Strings
pub const TAG_STRING_BASE: u8 = 139;  // 139-179: Short strings (0-40 chars)
pub const TAG_STRING_LONG: u8 = 180;
// Collections and containers
pub const TAG_BINARY: u8 = 181;
pub const TAG_STRUCT_UNIT: u8 = 182;
pub const TAG_STRUCT_NAMED: u8 = 183;
pub const TAG_STRUCT_UNNAMED: u8 = 184;
pub const TAG_ENUM: u8 = 185;
pub const TAG_ENUM_NAMED: u8 = 186;
pub const TAG_ENUM_UNNAMED: u8 = 187;
pub const TAG_ARRAY_VEC_SET_BASE: u8 = 188;  // 188-193: Short arrays (0-5 elements)
pub const TAG_ARRAY_VEC_SET_LONG: u8 = 194;
pub const TAG_TUPLE: u8 = 195;
pub const TAG_MAP: u8 = 196;
// Extended types (optional features)
pub const TAG_CHRONO_DATETIME: u8 = 197;
pub const TAG_CHRONO_NAIVE_DATE: u8 = 198;
pub const TAG_CHRONO_NAIVE_TIME: u8 = 199;
pub const TAG_DECIMAL: u8 = 200;
pub const TAG_UUID: u8 = 201;  // Shared by UUID and ULID
```

## 4. Data Type Specifications

### 4.1 Boolean

**Encoding:**
- `false`: `TAG_ZERO` (0x00)
- `true`: `TAG_ONE` (0x01)

**Example:**
```
true  -> [0x01]
false -> [0x00]
```

### 4.2 Unsigned Integers

**Compact Encoding (0-127):**
```
value -> [TAG_ZERO + value]
```
**Extended Encoding:**
```
u8    -> [TAG_U8] [value-128:u8]        (range: 128-383)
u16   -> [TAG_U16] [value:u16_le]       (range: 256-65535)
u32   -> [TAG_U32] [value:u32_le]       (range: 65536-4294967295)
u64   -> [TAG_U64] [value:u64_le]       (range: 4294967296-18446744073709551615)
u128  -> [TAG_U128] [value:u128_le]     (range: 18446744073709551616+)
```
**Size Selection:**
- 0-127: Direct encoding (1 byte total)
- 128-383: u8 encoding (2 bytes total) - stores value-128
- 384-65535: u16 encoding (3 bytes total)
- etc.

**Examples:**
```
42     -> [0x2A]           // TAG_ZERO + 42 = 0 + 42 = 42 = 0x2A
128    -> [0x83, 0x00]     // TAG_U8, 128-128=0
255    -> [0x83, 0x7F]     // TAG_U8, 255-128=127
383    -> [0x83, 0xFF]     // TAG_U8, 383-128=255
384    -> [0x84, 0x80, 0x01]  // TAG_U16, 384 in LE
```

### 4.3 Signed Integers

**Special Cases:**
- `0`: `TAG_ZERO` (0x00)
- `1`: `TAG_ONE` (0x01)

**Encoding Rule:**
- 0 and positive values: Encoded as unsigned integers
- Negative values: `TAG_NEGATIVE` (0x88) + bit-inverted encoding

**Format:**
```
// 0, positive values
[value:variable_uint]
// Negative values
[TAG_NEGATIVE] [(!n):variable_uint]
```
**Examples:**
```
0      -> [0x00]              // TAG_ZERO
1      -> [0x01]              // TAG_ONE
2      -> [0x02]              // TAG_ZERO+2
-1     -> [0x88, 0x00]        // TAG_NEGATIVE, !(-1)=0 -> TAG_ZERO
-2     -> [0x88, 0x01]        // TAG_NEGATIVE, !(-2)=1 -> TAG_ONE
-128   -> [0x88, 0x7F]        // TAG_NEGATIVE, !(-128)=127 -> TAG_ZERO+127
```

### 4.4 Floating Point

**Format:**
```
f32 -> [TAG_F32] [value:f32_le]
f64 -> [TAG_F64] [value:f64_le]
```
**Cross-Type Decoding:**
- f64 can be decoded as f32 (with potential precision loss)
- f32 to f64 cross-decoding is not supported due to precision ambiguity

### 4.5 Strings

**Short Strings (0-40 bytes):**
```
[TAG_STRING_BASE + length] [utf8_bytes]
```
**Long Strings:**
```
[TAG_STRING_LONG] [length:variable_uint] [utf8_bytes]
```
**Examples:**
```
""      -> [0x8B]                    // TAG_STRING_BASE + 0
"hi"    -> [0x8D, 0x68, 0x69]       // TAG_STRING_BASE + 2, "hi"
"long"  -> [0xB4, 0x04, 0x6C, 0x6F, 0x6E, 0x67]  // TAG_STRING_LONG, length=4, "long"
```

### 4.6 Option Types

**Format:**
```
None    -> [TAG_NONE]     // 0x80 (128)
Some(v) -> [TAG_SOME] [encoded_value]  // 0x81 (129) + value
```

### 4.7 Collections

#### Arrays, Vectors, Sets

**Short Collections (0-5 elements):**
```
[TAG_ARRAY_VEC_SET_BASE + count] [element1] [element2] ...
```
**Long Collections:**
```
[TAG_ARRAY_VEC_SET_LONG] [count:variable_uint] [element1] [element2] ...
```
#### Maps

**Format:**
```
[TAG_MAP] [count:variable_uint] [key1] [value1] [key2] [value2] ...
```
#### Tuples

**Format:**
```
[TAG_TUPLE] [element_count:variable_uint] [element1] [element2] ...
```

### 4.8 Binary Data

**Vec<u8> and Bytes:**
```
[TAG_BINARY] [length:variable_uint] [raw_bytes]
```

### 4.9 Extended Types (Feature-Dependent)

#### DateTime (chrono feature)

**Format:**
```
[TAG_CHRONO_DATETIME] [seconds:i64] [nanos:u32]
```
All DateTime types (UTC, Local) are normalized to UTC for storage.

#### NaiveDate (chrono feature)

**Format:**
```
[TAG_CHRONO_NAIVE_DATE] [days_from_epoch:i64]
```
Epoch: 1970-01-01

#### NaiveTime (chrono feature)

**Format:**
```
[TAG_CHRONO_NAIVE_TIME] [seconds_from_midnight:u32] [nanoseconds:u32]
```

#### NaiveDateTime (chrono feature)

**Format:**
```
[TAG_CHRONO_NAIVE_DATETIME] [seconds:i64] [nanos:u32]
```
Stores as seconds and nanoseconds since Unix epoch (1970-01-01 00:00:00 UTC).

#### Decimal (rust_decimal feature)

**Format:**
```
[TAG_DECIMAL] [mantissa:i128] [scale:u32]
```

#### UUID/ULID (uuid/ulid features)

**Format:**
```
[TAG_UUID] [value:u128_le]
```
**Note:** UUID and ULID share the same tag and are binary compatible at the encoding level.

### 4.14 serde_json::Value (Feature: serde_json)

Dynamic JSON values are supported when the `serde_json` feature is enabled. Each JSON value variant has its own tag:

- **TAG_JSON_NULL (202)**: JSON null value
- **TAG_JSON_BOOL (203)**: JSON boolean (uses existing boolean encoding)
- **TAG_JSON_NUMBER (204)**: JSON number with type preservation
- **TAG_JSON_STRING (205)**: JSON string (uses existing string encoding)
- **TAG_JSON_ARRAY (206)**: JSON array
- **TAG_JSON_OBJECT (207)**: JSON object

#### JSON Number Encoding

JSON numbers are encoded with type preservation to maintain integer/float distinction:

**Format:** `TAG_JSON_NUMBER` + `type_marker` + `value`

- `type_marker = 0`: Unsigned integer, followed by u64 encoding
- `type_marker = 1`: Signed integer, followed by i64 encoding  
- `type_marker = 2`: Float, followed by f64 encoding

**Examples:**
- `42` (integer) → `[204, 0, ...]` (TAG_JSON_NUMBER, unsigned integer marker, i64 encoding)
- `3.14159` (float) → `[204, 2, ...]` (TAG_JSON_NUMBER, float marker, f64 encoding)

#### JSON Array Encoding

**Format:** `TAG_JSON_ARRAY` + `length` + `elements...`

#### JSON Object Encoding

**Format:** `TAG_JSON_OBJECT` + `length` + `(key, value)...`

Keys are encoded as strings, values are recursively encoded as JSON values.

**Examples:**
- `null` → `[202]`
- `true` → `[203, 4]` (TAG_JSON_BOOL, TAG_ONE)
- `"hello"` → `[205, 144]` (TAG_JSON_STRING, string encoding)
- `[]` → `[206, 3]` (TAG_JSON_ARRAY, length 0)
- `{}` → `[207, 3]` (TAG_JSON_OBJECT, length 0)

## 5. Struct and Enum Encoding

### 5.1 Unit Structs

**Format:**
```
[TAG_STRUCT_UNIT]
```

### 5.2 Named Field Structs

**Format:**
```
[TAG_STRUCT_NAMED] [field_id_optimized] [field_value] ... [0x00]
```
**Field Encoding Rules:**
- Each field is encoded as `[field_id_optimized] [field_value]`
- Field IDs are derived from field names (CRC64(ECMA-182) hash) or custom `#[senax(id=n)]` attributes
- Field IDs 1-250 are encoded as single `u8` bytes
- Field IDs 251+ are encoded as `0xFF` marker + `u64` little-endian
- Optional fields with `None` values are omitted entirely
- Terminator: single zero byte (0x00) marks end of fields

### 5.3 Unnamed Field Structs (Tuples)

**Format:**
```
[TAG_STRUCT_UNNAMED] [field_count:variable_uint] [field1] [field2] ...
```

### 5.4 Enums

#### Unit Variants

**Format:**
```
[TAG_ENUM] [variant_id_optimized]
```
#### Named Field Variants

**Format:**
```
[TAG_ENUM_NAMED] [variant_id_optimized] [field_id_optimized] [field_value] ... [0x00]
```
#### Unnamed Field Variants

**Format:**
```
[TAG_ENUM_UNNAMED] [variant_id_optimized] [field_count:variable_uint] [field1] [field2] ...
```
**Variant ID Assignment:**
- Derived from variant name (CRC64 hash) or custom `#[senax(id=n)]` attributes
- Variant IDs 1-250 are encoded as single `u8` bytes
- Variant IDs 251+ are encoded as `0xFF` marker + `u64` little-endian
- Must be stable across versions for compatibility

## 6. Schema Evolution

### 6.1 Forward Compatibility

**Adding Fields:**
- New optional fields: Automatically handled (default to None)
- New required fields: Must have defaults or be made optional
  - In addition to having a Rust default value, you **must** explicitly annotate the field with `#[senax(default)]` to ensure forward/backward compatibility.
- Fields with `#[senax(skip_default)]`: Only encoded when value differs from default, automatically use default value when missing during decode

**Adding Enum Variants:**
- Use custom `#[senax(id=n)]` for stable IDs
- Unknown variants cause decode errors

### 6.2 Backward Compatibility

**Removing Fields:**
- Unknown field IDs are automatically skipped during decoding
- No decoder changes required

**Removing Enum Variants:**
- May cause decode errors if old data contains removed variants
- Consider deprecation strategy

### 6.3 Field Reordering

Field order changes are automatically handled due to ID-based encoding.

### 6.4 Type Changes

**Compatible Changes:**
- `u32` ↔ `i64` (if values fit)
- `f32` ↔ `f64`
- `u32` → `Option<u32>`

**Incompatible Changes:**
- `String` → `u32`
- `Vec<T>` → `HashMap<K,V>`
- None → Required

## 7. Implementation Notes

### 7.1 Skip Function

Decoders must implement a `skip_value()` function that can skip unknown tagged values without parsing them. This enables forward compatibility.

### 7.2 Error Handling

**Decode Errors:**
- Invalid UTF-8 in strings
- Unknown enum variants
- Malformed data (unexpected EOF, invalid tags)
- Type conversion failures

### 7.3 Endianness

All multi-byte values use little-endian encoding for consistency across platforms.

---

**This specification defines the complete binary format for senax-encoder. Implementations should follow these rules exactly to ensure cross-version and cross-platform compatibility.**