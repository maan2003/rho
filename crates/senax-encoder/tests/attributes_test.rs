use bytes::BytesMut;
use senax_encoder::Decoder;
use senax_encoder::Encoder;
use senax_encoder_derive::{Decode, Encode};
#[allow(unused_imports)]
use std::collections::HashMap;

// =============================================================================
// Basic attribute feature tests
// =============================================================================

// =============================================================================
// #[senax(default)] test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct OriginalStruct {
    #[senax(id = 1)]
    old_field: i32,
    #[senax(id = 2)]
    another_field: String,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct ExtendedStruct {
    #[senax(id = 1)]
    old_field: i32,
    #[senax(id = 2)]
    another_field: String,
    #[senax(id = 3, default)]
    new_field: i32,
    #[senax(id = 4, default)]
    new_optional_field: Option<String>,
}

#[test]
fn test_default_attribute_backward_compatibility() {
    // Encode the old struct
    let original = OriginalStruct {
        old_field: 42,
        another_field: "hello".to_string(),
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    // Decode with the new struct (new fields get default values)
    let mut reader = buffer.freeze();
    let extended = ExtendedStruct::decode(&mut reader).unwrap();

    assert_eq!(extended.old_field, 42);
    assert_eq!(extended.another_field, "hello");
    assert_eq!(extended.new_field, 0); // Default::default() for i32
    assert_eq!(extended.new_optional_field, None); // Default::default() for Option<String>
}

#[test]
fn test_default_attribute_forward_compatibility() {
    // Encode the new struct
    let extended = ExtendedStruct {
        old_field: 100,
        another_field: "world".to_string(),
        new_field: 999,
        new_optional_field: Some("test".to_string()),
    };

    let mut buffer = BytesMut::new();
    extended.encode(&mut buffer).unwrap();

    // Decode with the old struct (new fields are ignored)
    let mut reader = buffer.freeze();
    let original = OriginalStruct::decode(&mut reader).unwrap();

    assert_eq!(original.old_field, 100);
    assert_eq!(original.another_field, "world");
}

// =============================================================================
// #[senax(skip_encode)] test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct StructWithSkipEncode {
    #[senax(id = 1)]
    normal_field: i32,
    #[senax(id = 2)]
    another_normal_field: String,
    #[senax(skip_encode, default)]
    skip_encode_field: f64,
    #[senax(id = 3)]
    last_field: bool,
}

impl Default for StructWithSkipEncode {
    fn default() -> Self {
        Self {
            normal_field: 0,
            another_normal_field: String::new(),
            skip_encode_field: 0.0,
            last_field: false,
        }
    }
}

#[test]
fn test_skip_encode_attribute() {
    let original = StructWithSkipEncode {
        normal_field: 42,
        another_normal_field: "test".to_string(),
        skip_encode_field: 3.14, // Not encoded
        last_field: true,
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    // Decode the struct
    let mut reader = buffer.freeze();
    let decoded = StructWithSkipEncode::decode(&mut reader).unwrap();

    assert_eq!(decoded.normal_field, 42);
    assert_eq!(decoded.another_normal_field, "test");
    assert_eq!(decoded.skip_encode_field, 0.0); // Default value (skip_encode+default)
    assert_eq!(decoded.last_field, true);
}

// =============================================================================
// #[senax(skip_encode)] with Option test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct StructWithSkipEncodeOption {
    #[senax(id = 1)]
    normal_field: i32,
    #[senax(skip_encode)] // Option, so no default is needed
    skip_encode_optional: Option<String>,
    #[senax(id = 2)]
    last_field: bool,
}

#[test]
fn test_skip_encode_with_option() {
    let original = StructWithSkipEncodeOption {
        normal_field: 42,
        skip_encode_optional: Some("this will not be encoded".to_string()),
        last_field: true,
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    // Decode the struct
    let mut reader = buffer.freeze();
    let decoded = StructWithSkipEncodeOption::decode(&mut reader).unwrap();

    assert_eq!(decoded.normal_field, 42);
    assert_eq!(decoded.skip_encode_optional, None);
    assert_eq!(decoded.last_field, true);
}

// =============================================================================
// #[senax(skip_decode)] test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct StructWithSkipDecode {
    #[senax(id = 1)]
    normal_field: i32,
    #[senax(id = 2, skip_decode)]
    skip_decode_field: String,
    #[senax(id = 3)]
    last_field: bool,
}

impl Default for StructWithSkipDecode {
    fn default() -> Self {
        Self {
            normal_field: 0,
            skip_decode_field: String::new(),
            last_field: false,
        }
    }
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct CompatibleStruct {
    #[senax(id = 1)]
    normal_field: i32,
    #[senax(id = 2)]
    old_field: String, // skip_decode_field and old_field have the same ID
    #[senax(id = 3)]
    last_field: bool,
}

#[test]
fn test_skip_decode_attribute() {
    // Encode a compatible struct
    let compatible = CompatibleStruct {
        normal_field: 42,
        old_field: "this will be ignored".to_string(),
        last_field: true,
    };

    let mut buffer = BytesMut::new();
    compatible.encode(&mut buffer).unwrap();

    // Decode with the struct that has skip_decode_field
    let mut reader = buffer.freeze();
    let decoded = StructWithSkipDecode::decode(&mut reader).unwrap();

    assert_eq!(decoded.normal_field, 42);
    assert_eq!(decoded.skip_decode_field, String::new()); // Default::default() = ""
    assert_eq!(decoded.last_field, true);
}

// =============================================================================
// Multiple attribute combination test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct ComplexAttributeStruct {
    #[senax(id = 1)]
    normal_field: i32,
    #[senax(id = 2, default)]
    default_field: String,
    #[senax(skip_encode, default)]
    skip_encode_field: f64,
    #[senax(id = 3, skip_decode, default)]
    skip_decode_with_default: Vec<u8>,
    #[senax(id = 4)]
    last_normal_field: bool,
}

impl Default for ComplexAttributeStruct {
    fn default() -> Self {
        Self {
            normal_field: 0,
            default_field: "default".to_string(),
            skip_encode_field: 99.9,
            skip_decode_with_default: vec![1, 2, 3],
            last_normal_field: false,
        }
    }
}

#[test]
fn test_multiple_attributes_combination() {
    let original = ComplexAttributeStruct {
        normal_field: 100,
        default_field: "custom".to_string(),
        skip_encode_field: 123.456,              // Not encoded
        skip_decode_with_default: vec![7, 8, 9], // Ignored on decode
        last_normal_field: true,
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = ComplexAttributeStruct::decode(&mut reader).unwrap();

    assert_eq!(decoded.normal_field, 100);
    assert_eq!(decoded.default_field, "custom");
    assert_eq!(decoded.skip_encode_field, 0.0); // Default value (skip_encode+default)
    assert_eq!(decoded.skip_decode_with_default, Vec::<u8>::new()); // Default value (skip_decode+default)
    assert_eq!(decoded.last_normal_field, true);
}

// =============================================================================
// Attribute tests for Enum
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
enum EnumWithAttributes {
    #[senax(id = 1)]
    VariantA {
        #[senax(id = 1)]
        normal_field: i32,
        #[senax(id = 2, default)]
        default_field: String,
        #[senax(skip_encode, default)]
        _skip_encode_field: f64,
    },
    #[senax(id = 2)]
    VariantB(i32, String),
    #[senax(id = 3)]
    VariantC,
}

#[test]
fn test_enum_with_attributes() {
    let original = EnumWithAttributes::VariantA {
        normal_field: 42,
        default_field: "test".to_string(),
        _skip_encode_field: 3.14, // Not encoded
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = EnumWithAttributes::decode(&mut reader).unwrap();

    if let EnumWithAttributes::VariantA {
        normal_field,
        default_field,
        _skip_encode_field: _,
    } = decoded
    {
        assert_eq!(normal_field, 42);
        assert_eq!(default_field, "test");
        // _skip_encode_field is not used (should be default value)
    } else {
        panic!("Unexpected variant");
    }
}

// =============================================================================
// Error case tests
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct RequiredFieldStruct {
    #[senax(id = 1)]
    required_field: i32,
    #[senax(id = 2)]
    another_required: String,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct MissingRequiredStruct {
    #[senax(id = 1)]
    required_field: i32,
    // id=2 field does not exist
    #[senax(id = 3, default)]
    optional_field: String,
}

#[test]
fn test_missing_required_field_error() {
    let missing = MissingRequiredStruct {
        required_field: 42,
        optional_field: "test".to_string(),
    };

    let mut buffer = BytesMut::new();
    missing.encode(&mut buffer).unwrap();

    // Decoding with missing required field should error
    let mut reader = buffer.freeze();
    let result = RequiredFieldStruct::decode(&mut reader);

    assert!(result.is_err());
    // Check that error message contains "MissingRequiredField"
    let error_msg = format!("{:?}", result.unwrap_err());
    assert!(error_msg.contains("MissingRequiredField"));
}

// =============================================================================
// Option field with default attribute test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct StructWithOptionDefault {
    #[senax(id = 1)]
    normal_field: i32,
    #[senax(id = 2, default)]
    optional_with_default: Option<String>,
    #[senax(id = 3)]
    regular_optional: Option<i32>,
}

#[test]
fn test_option_field_with_default() {
    // Option field with default attribute
    let _original = StructWithOptionDefault {
        normal_field: 42,
        optional_with_default: Some("test".to_string()),
        regular_optional: None,
    };

    // Create another struct to encode without id=2
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct PartialStruct {
        #[senax(id = 1)]
        normal_field: i32,
        // id=2 is skipped
        #[senax(id = 3)]
        regular_optional: Option<i32>,
    }

    let partial = PartialStruct {
        normal_field: 42,
        regular_optional: None,
    };

    let mut buffer = BytesMut::new();
    partial.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = StructWithOptionDefault::decode(&mut reader).unwrap();

    assert_eq!(decoded.normal_field, 42);
    assert_eq!(decoded.optional_with_default, None); // default value
    assert_eq!(decoded.regular_optional, None);
}

// =============================================================================
// Custom ID and attribute combination test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct CustomIdWithAttributes {
    #[senax(id = 0x1000)]
    field_a: i32,
    #[senax(id = 0x2000, default)]
    field_b: String,
    #[senax(id = 0x3000, skip_encode, default)]
    field_c: f64,
    #[senax(id = 0x4000, skip_decode)]
    field_d: bool,
}

impl Default for CustomIdWithAttributes {
    fn default() -> Self {
        Self {
            field_a: 0,
            field_b: "default".to_string(),
            field_c: 0.0,
            field_d: false,
        }
    }
}

#[test]
fn test_custom_id_with_attributes() {
    let original = CustomIdWithAttributes {
        field_a: 123,
        field_b: "custom".to_string(),
        field_c: 456.789, // skip_encode
        field_d: true,    // skip_decode
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = CustomIdWithAttributes::decode(&mut reader).unwrap();

    assert_eq!(decoded.field_a, 123);
    assert_eq!(decoded.field_b, "custom");
    assert_eq!(decoded.field_c, 0.0); // Default value
    assert_eq!(decoded.field_d, false); // Default value
}

// =============================================================================
// #[senax(rename="name")] test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct OriginalWithRename {
    #[senax(rename = "old_name")] // Field name changed, but ID is calculated from "old_name"
    new_field_name: i32,
    #[senax(id = 2)]
    another_field: String,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct CompatibleWithOldName {
    #[senax(id = 1, rename = "old_name")] // Explicit ID overrides rename-based ID calculation
    field_with_explicit_id: i32,
    #[senax(id = 2)]
    another_field: String,
}

// Simulate a struct originally defined as old_name
#[derive(Encode, Decode, Debug, PartialEq)]
struct LegacyStruct {
    old_name: i32, // CRC32("old_name") ID
    #[senax(id = 2)]
    another_field: String,
}

#[test]
fn test_rename_attribute_compatibility() {
    // Encode the new struct
    let new_struct = OriginalWithRename {
        new_field_name: 42,
        another_field: "test".to_string(),
    };

    let mut buffer = BytesMut::new();
    new_struct.encode(&mut buffer).unwrap();

    // Decode with the old struct (rename="old_name" helps to generate the same ID)
    let mut reader = buffer.freeze();
    let legacy = LegacyStruct::decode(&mut reader).unwrap();

    assert_eq!(legacy.old_name, 42);
    assert_eq!(legacy.another_field, "test");
}

#[test]
fn test_rename_with_explicit_id() {
    // Test that explicit ID takes precedence over rename-based ID calculation

    // Create a struct that would have a different ID if CRC32("different_name") was used
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct DifferentNameStruct {
        #[senax(id = 1)] // Same explicit ID as CompatibleWithOldName
        different_name: i32, // Different field name
        #[senax(id = 2)]
        another_field: String,
    }

    // Encode with DifferentNameStruct
    let original = DifferentNameStruct {
        different_name: 42,
        another_field: "test".to_string(),
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    // Decode with CompatibleWithOldName - should work because both use id=1
    let mut reader = buffer.freeze();
    let decoded = CompatibleWithOldName::decode(&mut reader).unwrap();

    assert_eq!(decoded.field_with_explicit_id, 42);
    assert_eq!(decoded.another_field, "test");

    // Test the reverse direction
    let compat_struct = CompatibleWithOldName {
        field_with_explicit_id: 99,
        another_field: "reverse".to_string(),
    };

    let mut buffer2 = BytesMut::new();
    compat_struct.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded2 = DifferentNameStruct::decode(&mut reader2).unwrap();

    assert_eq!(decoded2.different_name, 99);
    assert_eq!(decoded2.another_field, "reverse");
}

// =============================================================================
// Enum rename attribute test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
enum EnumWithRename {
    #[senax(rename = "OldVariantName")] // Variant name changed
    NewVariantName {
        #[senax(rename = "old_field")] // Field name also changed
        new_field: i32,
        #[senax(id = 2)]
        stable_field: String,
    },
    #[senax(id = 100)] // Explicit ID
    AnotherVariant,
}

#[derive(Encode, Decode, Debug, PartialEq)]
enum LegacyEnum {
    OldVariantName {
        // CRC32("OldVariantName") ID
        old_field: i32, // CRC32("old_field") ID
        #[senax(id = 2)]
        stable_field: String,
    },
    #[senax(id = 100)]
    AnotherVariant,
}

#[test]
fn test_enum_rename_compatibility() {
    let new_enum = EnumWithRename::NewVariantName {
        new_field: 123,
        stable_field: "stable".to_string(),
    };

    let mut buffer = BytesMut::new();
    new_enum.encode(&mut buffer).unwrap();

    // Decode with the old enum (rename helps to maintain compatibility)
    let mut reader = buffer.freeze();
    let legacy = LegacyEnum::decode(&mut reader).unwrap();

    if let LegacyEnum::OldVariantName {
        old_field,
        stable_field,
    } = legacy
    {
        assert_eq!(old_field, 123);
        assert_eq!(stable_field, "stable");
    } else {
        panic!("Unexpected variant");
    }
}

// =============================================================================
// rename attribute and default attribute combination test
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct RenameWithDefault {
    #[senax(rename = "legacy_field", default)]
    modern_field: String,
    #[senax(id = 2)]
    normal_field: i32,
}

#[test]
fn test_rename_with_default() {
    // Simulate old data with missing field
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct PartialLegacy {
        #[senax(id = 2)]
        normal_field: i32,
        // legacy_field is missing
    }

    let partial = PartialLegacy { normal_field: 42 };

    let mut buffer = BytesMut::new();
    partial.encode(&mut buffer).unwrap();

    // With rename+default, field gets default value even if missing
    let mut reader = buffer.freeze();
    let decoded = RenameWithDefault::decode(&mut reader).unwrap();

    assert_eq!(decoded.modern_field, String::new()); // Default::default()
    assert_eq!(decoded.normal_field, 42);
}

#[test]
fn test_rename_only_behavior() {
    // Test that rename without explicit ID uses CRC32-based ID calculation

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct WithRenameOnly {
        #[senax(rename = "original_field")] // Uses CRC32("original_field") as ID
        renamed_field: i32,
        #[senax(id = 100)] // Explicit ID to avoid collision
        other_field: String,
    }

    // Simulate the original struct that had "original_field" as the actual field name
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct OriginalFieldStruct {
        original_field: i32, // CRC32("original_field") ID - same as rename calculation
        #[senax(id = 100)]
        other_field: String,
    }

    // Encode with the original struct
    let original = OriginalFieldStruct {
        original_field: 123,
        other_field: "compatible".to_string(),
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    // Decode with the renamed struct - should work because rename="original_field"
    // generates the same ID as the actual field name "original_field"
    let mut reader = buffer.freeze();
    let decoded = WithRenameOnly::decode(&mut reader).unwrap();

    assert_eq!(decoded.renamed_field, 123);
    assert_eq!(decoded.other_field, "compatible");

    // Test reverse direction
    let renamed_struct = WithRenameOnly {
        renamed_field: 456,
        other_field: "reverse".to_string(),
    };

    let mut buffer2 = BytesMut::new();
    renamed_struct.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded2 = OriginalFieldStruct::decode(&mut reader2).unwrap();

    assert_eq!(decoded2.original_field, 456);
    assert_eq!(decoded2.other_field, "reverse");
}

// =============================================================================
// Check that field IDs are written using optimized encoding
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
struct StructWithU8Id {
    #[senax(id = 1)]
    field_a: i32,
    #[senax(id = 2)]
    field_b: String,
}

#[test]
fn test_struct_with_u8_id() {
    let original = StructWithU8Id {
        field_a: 42,
        field_b: "abc".to_string(),
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let bytes = buffer.freeze();

    // Check that field IDs are written using optimized encoding
    assert_eq!(bytes[0], senax_encoder::core::TAG_STRUCT_NAMED);
    // bytes[1] should contain field ID 1 as u8
    assert_eq!(bytes[1], 1);
    // bytes[2] should contain TAG_ZERO (for value 42) followed by 42
    assert_eq!(bytes[2], senax_encoder::core::TAG_ZERO + 42);
    // bytes[3] should contain field ID 2 as u8
    assert_eq!(bytes[3], 2);
    // bytes[4] should be string tag for "abc"
    assert_eq!(bytes[4], senax_encoder::core::TAG_STRING_BASE + 3);
    // bytes[5..8] should be "abc"
    assert_eq!(&bytes[5..8], b"abc");
    // bytes[8] should be terminator (0 as u8)
    assert_eq!(bytes[8], 0);
}

// =============================================================================
// Check that variant IDs are written using optimized encoding
// =============================================================================

#[derive(Encode, Decode, Debug, PartialEq)]
enum EnumWithU8Id {
    #[senax(id = 1)]
    VariantA {
        #[senax(id = 1)]
        normal_field: i32,
        #[senax(id = 2, default)]
        default_field: String,
        #[senax(skip_encode, default)]
        _skip_encode_field: f64,
    },
    #[senax(id = 2)]
    VariantB(i32, String),
    #[senax(id = 3)]
    VariantC,
}

#[test]
fn test_enum_with_u8_id() {
    let original = EnumWithU8Id::VariantA {
        normal_field: 42,
        default_field: "test".to_string(),
        _skip_encode_field: 3.14, // Not encoded
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let bytes = buffer.freeze();

    // Check that variant IDs are written using optimized encoding
    assert_eq!(bytes[0], senax_encoder::core::TAG_ENUM_NAMED);
    // bytes[1] should contain variant ID 1 as u8
    assert_eq!(bytes[1], 1);
}
