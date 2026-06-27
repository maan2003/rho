use bytes::{Bytes, BytesMut};
#[cfg(feature = "chrono")]
use chrono::{DateTime, Local, NaiveDate, NaiveTime, Utc};
#[cfg(feature = "indexmap")]
use indexmap::{IndexMap, IndexSet};
#[cfg(feature = "rust_decimal")]
use rust_decimal::Decimal;
use senax_encoder::Decoder;
use senax_encoder::Encoder;
use senax_encoder_derive::{Decode, Encode};
#[cfg(feature = "serde_json")]
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
#[cfg(any(feature = "rust_decimal", feature = "uuid"))]
use std::str::FromStr;
use std::sync::Arc;
#[cfg(feature = "ulid")]
use ulid::Ulid;
#[cfg(feature = "uuid")]
use uuid::Uuid;

#[derive(Encode, Decode, Debug, PartialEq)]
struct TestStruct {
    a: Option<i32>,
    b: Option<String>,
    c: Option<Vec<u8>>,
}

#[derive(Encode, Decode, Debug, PartialEq)]
enum TestEnum {
    A(String),
    B,
    C(i32, String),
    D { x: i32, y: String },
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct Point {
    x: i32,
    y: i32,
}

#[derive(Encode, Decode, Debug, PartialEq)]
enum Shape {
    Circle(Point, f64),
    Rectangle(Point, Point),
    None,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct CustomIdStruct {
    #[senax(id = 1234)]
    first: i32,
    #[senax(id = 5678)]
    second: String,
    #[senax(id = 9012)]
    optional: Option<bool>,
}

#[derive(Encode, Decode, Debug, PartialEq)]
enum CustomIdEnum {
    #[senax(id = 2468)]
    First(i32),
    #[senax(id = 1357)]
    Second(String),
    #[senax(id = 3691)]
    Third { x: f64, y: f64 },
}

#[test]
fn test_primitive_encode() {
    let u32_value: u32 = 42;
    let i32_value: i32 = -42;
    let f64_value: f64 = 3.14;
    let bool_value: bool = true;

    let mut buffer = BytesMut::new();
    u32_value.encode(&mut buffer).unwrap();
    i32_value.encode(&mut buffer).unwrap();
    f64_value.encode(&mut buffer).unwrap();
    bool_value.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    assert_eq!(u32::decode(&mut reader).unwrap(), 42);
    assert_eq!(i32::decode(&mut reader).unwrap(), -42);
    assert!((f64::decode(&mut reader).unwrap() - 3.14).abs() < 1e-10);
    assert_eq!(bool::decode(&mut reader).unwrap(), true);
}

#[test]
fn test_option_encode() {
    let values: Vec<Option<i32>> = vec![Some(42), None, Some(-42)];
    let mut buffer = BytesMut::new();
    for value in &values {
        value.encode(&mut buffer).unwrap();
    }
    let mut reader = buffer.freeze();
    let decoded: Vec<Option<i32>> = (0..3)
        .map(|_| Option::decode(&mut reader).unwrap())
        .collect();
    assert_eq!(values, decoded);
}

#[test]
fn test_array_encode() {
    let array: [i32; 3] = [1, 2, 3];
    let mut buffer = BytesMut::new();
    array.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded: [i32; 3] = Decoder::decode(&mut reader).unwrap();
    assert_eq!(array, decoded);
}

#[test]
fn test_vec_encode() {
    let vec = vec![1i32, 2, 3, 4, 5];
    let mut buffer = BytesMut::new();
    vec.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded: Vec<i32> = Decoder::decode(&mut reader).unwrap();
    assert_eq!(vec, decoded);
}

#[test]
fn test_struct_encode() {
    let test_struct = TestStruct {
        a: Some(42),
        b: Some("Hello".to_string()),
        c: Some(vec![1, 2, 3]),
    };
    let mut buffer = BytesMut::new();
    test_struct.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded = TestStruct::decode(&mut reader).unwrap();
    assert_eq!(test_struct, decoded);
}

#[test]
fn test_struct_partial_encode() {
    let test_struct = TestStruct {
        a: Some(42),
        b: None,
        c: Some(vec![1, 2, 3]),
    };
    let mut buffer = BytesMut::new();
    test_struct.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded = TestStruct::decode(&mut reader).unwrap();
    assert_eq!(test_struct, decoded);
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct MixedStruct {
    required: i32,
    optional: Option<String>,
    required_vec: Vec<u8>,
}

#[test]
fn test_mixed_struct_encode() {
    let test_struct = MixedStruct {
        required: 42,
        optional: Some("Hello".to_string()),
        required_vec: vec![1, 2, 3],
    };
    let mut buffer = BytesMut::new();
    test_struct.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded = MixedStruct::decode(&mut reader).unwrap();
    assert_eq!(test_struct, decoded);
}

#[test]
fn test_mixed_struct_partial_encode() {
    let test_struct = MixedStruct {
        required: 42,
        optional: None,
        required_vec: vec![1, 2, 3],
    };
    let mut buffer = BytesMut::new();
    test_struct.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded = MixedStruct::decode(&mut reader).unwrap();
    assert_eq!(test_struct, decoded);
}

#[test]
fn test_enum_encode() {
    let variants = vec![
        TestEnum::A("Hello".to_string()),
        TestEnum::B,
        TestEnum::C(42, "World".to_string()),
        TestEnum::D {
            x: 42,
            y: "World".to_string(),
        },
    ];
    let mut buffer = BytesMut::new();
    for variant in &variants {
        variant.encode(&mut buffer).unwrap();
    }
    let mut reader = buffer.freeze();
    let decoded: Vec<TestEnum> = variants
        .iter()
        .map(|_| TestEnum::decode(&mut reader).unwrap())
        .collect();
    assert_eq!(variants, decoded);
}

#[test]
fn test_enum_with_struct() {
    let shapes = vec![
        Shape::Circle(Point { x: 0, y: 0 }, 5.0),
        Shape::Rectangle(Point { x: 0, y: 0 }, Point { x: 10, y: 10 }),
        Shape::None,
    ];
    let mut buffer = BytesMut::new();
    for shape in &shapes {
        shape.encode(&mut buffer).unwrap();
    }
    let mut reader = buffer.freeze();
    let decoded: Vec<Shape> = shapes
        .iter()
        .map(|_| Shape::decode(&mut reader).unwrap())
        .collect();
    assert_eq!(shapes, decoded);
}

#[test]
fn test_custom_id_struct() {
    let original = CustomIdStruct {
        first: 42,
        second: "Hello".to_string(),
        optional: Some(true),
    };

    // Serialize
    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    // Deserialize
    let mut reader = buffer.freeze();
    let decoded = CustomIdStruct::decode(&mut reader).unwrap();

    // Verify match
    assert_eq!(original, decoded);
}

#[test]
fn test_custom_id_enum() {
    let variants = vec![
        CustomIdEnum::First(42),
        CustomIdEnum::Second("Hello".to_string()),
        CustomIdEnum::Third { x: 1.23, y: 4.56 },
    ];

    let mut buffer = BytesMut::new();
    for variant in &variants {
        variant.encode(&mut buffer).unwrap();
    }

    let mut reader = buffer.freeze();
    let decoded: Vec<CustomIdEnum> = (0..variants.len())
        .map(|_| CustomIdEnum::decode(&mut reader).unwrap())
        .collect();

    assert_eq!(variants, decoded);
}

#[test]
fn test_optional_field_with_custom_id() {
    let with_optional = CustomIdStruct {
        first: 42,
        second: "Hello".to_string(),
        optional: Some(true),
    };

    let without_optional = CustomIdStruct {
        first: 42,
        second: "Hello".to_string(),
        optional: None,
    };

    // Serialize
    let mut buffer_with = BytesMut::new();
    with_optional.encode(&mut buffer_with).unwrap();

    let mut buffer_without = BytesMut::new();
    without_optional.encode(&mut buffer_without).unwrap();

    // Compare buffer sizes (None case should be smaller as neither field ID nor value is written)
    assert!(buffer_with.len() > buffer_without.len());

    // Deserialize
    let mut reader_with = buffer_with.freeze();
    let decoded_with = CustomIdStruct::decode(&mut reader_with).unwrap();
    assert_eq!(with_optional, decoded_with);

    let mut reader_without = buffer_without.freeze();
    let decoded_without = CustomIdStruct::decode(&mut reader_without).unwrap();
    assert_eq!(without_optional, decoded_without);
}

#[test]
fn test_unsigned_signed_cross_decode() {
    // u16→i16, within range
    let v: u16 = 12345;
    let mut buf = BytesMut::new();
    v.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let got = i16::decode(&mut cur).unwrap();
    assert_eq!(got, v as i16);
    // u16→i16, out of range
    let v: u16 = 40000;
    let mut buf = BytesMut::new();
    v.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let res = i16::decode(&mut cur);
    assert!(res.is_err());
    // i16→u16, positive
    let v: i16 = 1234;
    let mut buf = BytesMut::new();
    v.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let got = u16::decode(&mut cur).unwrap();
    assert_eq!(got, v as u16);
    // i16→u16, negative value (should error)
    let v: i16 = -1234;
    let mut buf = BytesMut::new();
    v.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let res = u16::decode(&mut cur);
    assert!(res.is_err());
}

#[test]
fn test_unsigned_integer_compact_encoding() {
    // u8
    let u8_values = [0u8, 1, 42, 127, 128, 200, 255];
    for &v in &u8_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = u8::decode(&mut cur).unwrap();
        assert_eq!(v, got, "u8 roundtrip failed for {}", v);
    }
    // u16
    let u16_values = [0u16, 1, 42, 127, 128, 200, 255, 256, 1000, 65535];
    for &v in &u16_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = u16::decode(&mut cur).unwrap();
        assert_eq!(v, got, "u16 roundtrip failed for {}", v);
    }
    // u32
    let u32_values = [
        0u32, 1, 42, 127, 128, 200, 255, 256, 1000, 65535, 65536, 12345678, 4294967295,
    ];
    for &v in &u32_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = u32::decode(&mut cur).unwrap();
        assert_eq!(v, got, "u32 roundtrip failed for {}", v);
    }
    // u64
    let u64_values = [
        0u64,
        1,
        42,
        127,
        128,
        200,
        255,
        256,
        1000,
        65535,
        65536,
        12345678,
        4294967295,
        4294967296,
        9876543210123456789,
    ];
    for &v in &u64_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = u64::decode(&mut cur).unwrap();
        assert_eq!(v, got, "u64 roundtrip failed for {}", v);
    }
    // u128
    let u128_values = [
        0u128,
        1,
        42,
        127,
        128,
        200,
        255,
        256,
        1000,
        65535,
        65536,
        12345678,
        4294967295,
        4294967296,
        9876543210123456789,
        u64::MAX as u128,
        u128::MAX,
    ];
    for &v in &u128_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = u128::decode(&mut cur).unwrap();
        assert_eq!(v, got, "u128 roundtrip failed for {}", v);
    }
    // usize (platform-dependent but tested as u64 equivalent)
    let usize_values = [
        0usize,
        1,
        42,
        127,
        128,
        200,
        255,
        256,
        1000,
        65535,
        65536,
        12345678,
        4294967295,
        4294967296,
        9876543210123456789,
    ];
    for &v in &usize_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = usize::decode(&mut cur).unwrap();
        assert_eq!(v, got, "usize roundtrip failed for {}", v);
    }
}

#[test]
fn test_signed_integer_compact_encoding() {
    // i8
    let i8_values = [0i8, 1, -1, 42, -42, 127, -128];
    for &v in &i8_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = i8::decode(&mut cur).unwrap();
        assert_eq!(v, got, "i8 roundtrip failed for {}", v);
    }
    // i16
    let i16_values = [0i16, 1, -1, 42, -42, 127, -128, 32767, -32768];
    for &v in &i16_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = i16::decode(&mut cur).unwrap();
        assert_eq!(v, got, "i16 roundtrip failed for {}", v);
    }
    // i32
    let i32_values = [
        0i32,
        1,
        -1,
        42,
        -42,
        127,
        -128,
        32767,
        -32768,
        2147483647,
        -2147483648,
    ];
    for &v in &i32_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = i32::decode(&mut cur).unwrap();
        assert_eq!(v, got, "i32 roundtrip failed for {}", v);
    }
    // i64
    let i64_values = [
        0i64,
        1,
        -1,
        42,
        -42,
        127,
        -128,
        32767,
        -32768,
        2147483647,
        -2147483648,
    ];
    for &v in &i64_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = i64::decode(&mut cur).unwrap();
        assert_eq!(v, got, "i64 roundtrip failed for {}", v);
    }
    // i128
    let i128_values = [
        0i128,
        1,
        -1,
        42,
        -42,
        127,
        -128,
        32767,
        -32768,
        2147483647,
        -2147483648,
        9223372036854775807,
        -9223372036854775808,
    ];
    for &v in &i128_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = i128::decode(&mut cur).unwrap();
        assert_eq!(v, got, "i128 roundtrip failed for {}", v);
    }
    // isize (platform-dependent but tested as i64 equivalent)
    let isize_values = [
        0isize,
        1,
        -1,
        42,
        -42,
        127,
        -128,
        32767,
        -32768,
        2147483647,
        -2147483648,
        9223372036854775807,
        -9223372036854775808,
    ];
    for &v in &isize_values {
        let mut buf = BytesMut::new();
        v.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let got = isize::decode(&mut cur).unwrap();
        assert_eq!(v, got, "isize roundtrip failed for {}", v);
    }
}

#[test]
fn test_float_cross_decode() {
    // f32→f64 cross-decoding is now supported (both use string format)
    let v32: f32 = 3.1415927;
    let mut buf = BytesMut::new();
    v32.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let result64 = f64::decode(&mut cur).unwrap();
    assert!(
        (result64 - v32 as f64).abs() < 1e-6,
        "f32→f64 cross-decode failed: {} vs {}",
        result64,
        v32
    );

    // f64→f32 (with precision loss)
    let v64: f64 = 2.718281828459045;
    let mut buf = BytesMut::new();
    v64.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let got32 = f32::decode(&mut cur).unwrap();
    assert!(
        (got32.to_string().parse::<f64>().unwrap() - v64).abs() < 1e-6,
        "f64→f32 cross failed: {} vs {}",
        got32,
        v64
    );
}

#[test]
fn test_struct_field_skip_on_decode() {
    #[derive(Debug, PartialEq, Encode, Decode)]
    struct OldStruct {
        a: u32,
        b: u32,
        c: u32,
    }
    #[derive(Debug, PartialEq, Encode, Decode)]
    struct NewStruct {
        a: u32,
        c: u32,
    }
    let old = OldStruct {
        a: 42,
        b: 99,
        c: 100,
    };
    let mut buf = BytesMut::new();
    old.encode(&mut buf).unwrap();
    // Deserialize with NewStruct (b is ignored)
    let new = NewStruct::decode(&mut buf.freeze()).unwrap();
    assert_eq!(new, NewStruct { a: 42, c: 100 });
}

#[test]
fn test_enum_field_skip_on_decode() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    enum OldEnum {
        VariantA {
            id: u32,
            name: String,
            value: i64,
            flag: bool,
        },
        VariantB(u32, String, i64, bool),
        VariantC,
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    enum NewEnum {
        VariantA {
            // name and flag are skipped
            id: u32,
            value: i64,
        },
        VariantB(u32, String), // Error if field count does not match
        VariantC,
        VariantD, // New variant not present in serialized data
    }

    // --- VariantA (Named) ---
    let old_a = OldEnum::VariantA {
        id: 101,
        name: "Old Name A".to_string(),
        value: -1001,
        flag: true,
    };
    let mut buf_a = BytesMut::new();
    old_a.encode(&mut buf_a).unwrap();

    let mut reader_a = buf_a.freeze();
    let new_a = NewEnum::decode(&mut reader_a).unwrap();
    assert_eq!(
        new_a,
        NewEnum::VariantA {
            id: 101,
            value: -1001
        }
    );

    // --- VariantB (Unnamed) ---
    let old_b = OldEnum::VariantB(202, "Old Name B".to_string(), -2002, false);
    let mut buf_b = BytesMut::new();
    old_b.encode(&mut buf_b).unwrap();

    let mut reader_b = buf_b.freeze();
    let result_b = NewEnum::decode(&mut reader_b);
    assert!(result_b.is_err()); // Error if field count does not match

    // --- VariantC (Unit) ---
    let old_c = OldEnum::VariantC;
    let mut buf_c = BytesMut::new();
    old_c.encode(&mut buf_c).unwrap();

    let mut reader_c = buf_c.freeze();
    let new_c = NewEnum::decode(&mut reader_c).unwrap();
    assert_eq!(new_c, NewEnum::VariantC);
}

#[test]
fn test_set_and_map_collections() {
    // HashSet
    let mut hs = HashSet::new();
    hs.insert(10);
    hs.insert(20);
    hs.insert(30);
    let mut buf = BytesMut::new();
    hs.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let hs2 = HashSet::decode(&mut cur).unwrap();
    assert_eq!(hs, hs2);

    // BTreeSet
    let mut bs = BTreeSet::new();
    bs.insert(100);
    bs.insert(200);
    let mut buf = BytesMut::new();
    bs.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let bs2 = BTreeSet::decode(&mut cur).unwrap();
    assert_eq!(bs, bs2);

    #[cfg(feature = "indexmap")]
    {
        // IndexSet
        let mut is = IndexSet::new();
        is.insert("a".to_string());
        is.insert("b".to_string());
        let mut buf = BytesMut::new();
        is.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let is2 = IndexSet::decode(&mut cur).unwrap();
        assert_eq!(is, is2);
    }

    // BTreeMap
    let mut bm = BTreeMap::new();
    bm.insert(1, "one".to_string());
    bm.insert(2, "two".to_string());
    let mut buf = BytesMut::new();
    bm.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let bm2 = BTreeMap::decode(&mut cur).unwrap();
    assert_eq!(bm, bm2);

    #[cfg(feature = "indexmap")]
    {
        // IndexMap
        let mut im = IndexMap::new();
        im.insert("x".to_string(), 123);
        im.insert("y".to_string(), 456);
        let mut buf = BytesMut::new();
        im.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let im2: IndexMap<String, i32> = IndexMap::decode(&mut cur).unwrap();
        assert_eq!(im, im2);
    }
}

#[test]
#[cfg(feature = "chrono")]
fn test_chrono_datetime_utc() {
    let mut writer = BytesMut::new();
    let original = DateTime::from_timestamp(1640995200, 123456789).unwrap(); // 2022-01-01 00:00:00.123456789 UTC
    original.encode(&mut writer).unwrap();

    let mut reader = writer.freeze();
    let decoded = DateTime::<Utc>::decode(&mut reader).unwrap();

    assert_eq!(original, decoded);
}

#[test]
#[cfg(feature = "chrono")]
fn test_chrono_datetime_local() {
    let mut writer = BytesMut::new();
    let utc_dt = DateTime::from_timestamp(1640995200, 123456789).unwrap();
    let original = utc_dt.with_timezone(&Local);
    original.encode(&mut writer).unwrap();

    let mut reader = writer.freeze();
    let decoded = DateTime::<Local>::decode(&mut reader).unwrap();

    // Compare UTC timestamps since local timezone might vary
    assert_eq!(original.with_timezone(&Utc), decoded.with_timezone(&Utc));
}

#[test]
#[cfg(feature = "chrono")]
fn test_chrono_naive_date() {
    let mut writer = BytesMut::new();
    let original = NaiveDate::from_ymd_opt(2022, 1, 15).unwrap();
    original.encode(&mut writer).unwrap();

    let mut reader = writer.freeze();
    let decoded = NaiveDate::decode(&mut reader).unwrap();

    assert_eq!(original, decoded);
}

#[test]
#[cfg(feature = "chrono")]
fn test_chrono_naive_time() {
    let mut writer = BytesMut::new();
    let original = NaiveTime::from_hms_nano_opt(14, 30, 45, 123456789).unwrap();
    original.encode(&mut writer).unwrap();

    let mut reader = writer.freeze();
    let decoded = NaiveTime::decode(&mut reader).unwrap();

    assert_eq!(original, decoded);
}

#[test]
#[cfg(feature = "chrono")]
fn test_chrono_datetime_cross_timezone() {
    // Serialize with DateTime<Utc> and deserialize with DateTime<Local>
    let mut writer_utc = BytesMut::new();
    let utc_original = DateTime::from_timestamp(1640995200, 123456789).unwrap();
    utc_original.encode(&mut writer_utc).unwrap();

    let mut reader_utc = writer_utc.freeze();
    let local_from_utc = DateTime::<Local>::decode(&mut reader_utc).unwrap();

    // Verify that UTC times match
    assert_eq!(utc_original, local_from_utc.with_timezone(&Utc));

    // Serialize with DateTime<Local> and deserialize with DateTime<Utc>
    let mut writer_local = BytesMut::new();
    let local_original = utc_original.with_timezone(&Local);
    local_original.encode(&mut writer_local).unwrap();

    let mut reader_local = writer_local.freeze();
    let utc_from_local = DateTime::<Utc>::decode(&mut reader_local).unwrap();

    // Verify that UTC times match
    assert_eq!(local_original.with_timezone(&Utc), utc_from_local);
}

#[test]
fn test_bytes_encode() {
    let original_data = b"Hello, World! This is binary data \x00\x01\x02\xFF";
    let original_bytes = Bytes::from_static(original_data);

    let mut writer = BytesMut::new();
    original_bytes.encode(&mut writer).unwrap();

    let mut reader = writer.freeze();
    let decoded = Bytes::decode(&mut reader).unwrap();

    assert_eq!(original_bytes, decoded);
}

#[test]
fn test_bytes_from_string_data() {
    // Serialize as String and deserialize as Bytes
    let original_string = "Hello, 日本語 test!";
    let mut writer = BytesMut::new();
    original_string.to_string().encode(&mut writer).unwrap();

    let mut reader = writer.freeze();
    let bytes_result = Bytes::decode(&mut reader).unwrap();

    // Verify they match as UTF-8 byte sequence
    assert_eq!(bytes_result, Bytes::from(original_string.as_bytes()));
}

#[test]
fn test_bytes_empty() {
    let empty_bytes = Bytes::new();

    let mut writer = BytesMut::new();
    empty_bytes.encode(&mut writer).unwrap();

    let mut reader = writer.freeze();
    let decoded = Bytes::decode(&mut reader).unwrap();

    assert_eq!(empty_bytes, decoded);
    assert_eq!(decoded.len(), 0);
}

#[test]
fn test_bytes_string_compatibility() {
    // Short string (TAG_STRING_BASE range)
    let short_string = "short";
    let mut writer_short = BytesMut::new();
    short_string.to_string().encode(&mut writer_short).unwrap();

    let mut reader_short = writer_short.freeze();
    let bytes_short = Bytes::decode(&mut reader_short).unwrap();
    assert_eq!(bytes_short, Bytes::from(short_string.as_bytes()));

    // Long string (TAG_STRING_LONG range)
    let long_string = "a".repeat(100);
    let expected_bytes = Bytes::from(long_string.as_bytes().to_vec());
    let mut writer_long = BytesMut::new();
    long_string.encode(&mut writer_long).unwrap();

    let mut reader_long = writer_long.freeze();
    let bytes_long = Bytes::decode(&mut reader_long).unwrap();
    assert_eq!(bytes_long, expected_bytes);
}

// For complex data structure tests (Option<u16> removed)
#[derive(Encode, Decode, Debug, PartialEq)]
struct Address {
    street: String,
    city: String,
    postal_code: Option<String>,
    coordinates: Option<(f64, f64)>,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct PersonalInfo {
    first_name: String,
    last_name: String,
    age: u8,
    email: Option<String>,
    phone_numbers: Vec<String>,
}

#[derive(Encode, Decode, Debug, PartialEq)]
#[cfg(feature = "chrono")]
enum PaymentMethod {
    Cash,
    CreditCard {
        number: String,
        expiry: NaiveDate,
    },
    BankTransfer(String, String), // bank_name, account_number
    DigitalWallet {
        provider: String,
        wallet_id: String,
        verified: bool,
    },
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct OrderItem {
    product_id: u64,
    name: String,
    quantity: u32,
    unit_price: f64,
    metadata: HashMap<String, String>,
    tags: HashSet<String>,
}

#[derive(Encode, Decode, Debug, PartialEq)]
#[cfg(all(feature = "chrono", feature = "indexmap"))]
struct ComplexOrder {
    order_id: u64,
    customer: PersonalInfo,
    shipping_address: Address,
    billing_address: Option<Address>,
    items: Vec<OrderItem>,
    payment_methods: Vec<PaymentMethod>,
    order_date: DateTime<Utc>,
    delivery_date: Option<NaiveDate>,
    special_instructions: Option<String>,
    discount_codes: BTreeSet<String>,
    order_notes: IndexMap<String, String>,
    binary_attachment: Option<Bytes>,
    nested_data: HashMap<String, Vec<HashMap<String, Option<(i32, f64, bool)>>>>,
}

#[test]
#[cfg(all(feature = "chrono", feature = "indexmap"))]
fn test_complex_struct_encode() {
    let complex_order = ComplexOrder {
        order_id: 12345678901234567890u64,
        customer: PersonalInfo {
            first_name: "田中".to_string(),
            last_name: "太郎".to_string(),
            age: 35,
            email: Some("tanaka.taro@example.com".to_string()),
            phone_numbers: vec!["090-1234-5678".to_string(), "03-1234-5678".to_string()],
        },
        shipping_address: Address {
            street: "渋谷区神南1-2-3".to_string(),
            city: "東京都".to_string(),
            postal_code: Some("150-0041".to_string()),
            coordinates: Some((35.6627, 139.7039)),
        },
        billing_address: Some(Address {
            street: "新宿区新宿4-5-6".to_string(),
            city: "東京都".to_string(),
            postal_code: Some("160-0022".to_string()),
            coordinates: None,
        }),
        items: vec![
            OrderItem {
                product_id: 987654321,
                name: "高級ボールペン".to_string(),
                quantity: 2,
                unit_price: 1250.50,
                metadata: {
                    let mut map = HashMap::new();
                    map.insert("color".to_string(), "black".to_string());
                    map.insert("brand".to_string(), "Premium".to_string());
                    map
                },
                tags: {
                    let mut set = HashSet::new();
                    set.insert("office".to_string());
                    set.insert("premium".to_string());
                    set.insert("writing".to_string());
                    set
                },
            },
            OrderItem {
                product_id: 123456789,
                name: "ノートブック".to_string(),
                quantity: 5,
                unit_price: 320.75,
                metadata: HashMap::new(),
                tags: {
                    let mut set = HashSet::new();
                    set.insert("office".to_string());
                    set.insert("paper".to_string());
                    set
                },
            },
        ],
        payment_methods: vec![
            PaymentMethod::CreditCard {
                number: "1234-5678-9012-3456".to_string(),
                expiry: NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
            },
            PaymentMethod::DigitalWallet {
                provider: "PayPay".to_string(),
                wallet_id: "user123456".to_string(),
                verified: true,
            },
        ],
        order_date: DateTime::from_timestamp(1640995200, 123456789).unwrap(),
        delivery_date: Some(NaiveDate::from_ymd_opt(2022, 1, 10).unwrap()),
        special_instructions: Some("玄関前に置いてください".to_string()),
        discount_codes: {
            let mut set = BTreeSet::new();
            set.insert("NEWYEAR2022".to_string());
            set.insert("FIRSTTIME".to_string());
            set
        },
        order_notes: {
            let mut map = IndexMap::new();
            map.insert("warehouse".to_string(), "checked".to_string());
            map.insert("shipping".to_string(), "expedited".to_string());
            map.insert("customer_service".to_string(), "vip_customer".to_string());
            map
        },
        binary_attachment: Some(Bytes::from_static(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR")),
        nested_data: {
            let mut outer_map = HashMap::new();
            let mut inner_vec = Vec::new();
            let mut inner_map1 = HashMap::new();
            inner_map1.insert("key1".to_string(), Some((42, 3.14159, true)));
            inner_map1.insert("key2".to_string(), None);
            let mut inner_map2 = HashMap::new();
            inner_map2.insert("nested".to_string(), Some((-123, 2.71828, false)));
            inner_vec.push(inner_map1);
            inner_vec.push(inner_map2);
            outer_map.insert("category_a".to_string(), inner_vec);
            outer_map
        },
    };

    // Serialize
    let mut buffer = BytesMut::new();
    complex_order.encode(&mut buffer).unwrap();

    // Check buffer size (should be large since it's complex data)
    println!("Encoded complex order size: {} bytes", buffer.len());
    assert!(buffer.len() > 100); // Should be over 100 bytes since it's sufficiently complex data

    // Deserialize
    let mut reader = buffer.freeze();
    let decoded = ComplexOrder::decode(&mut reader).unwrap();

    // 完全一致確認
    assert_eq!(complex_order, decoded);

    // いくつかの特定のフィールドを個別に確認
    assert_eq!(decoded.order_id, 12345678901234567890u64);
    assert_eq!(decoded.customer.first_name, "田中");
    assert_eq!(decoded.items.len(), 2);
    assert_eq!(decoded.items[0].quantity, 2);
    assert_eq!(decoded.payment_methods.len(), 2);
    assert!(matches!(
        decoded.payment_methods[0],
        PaymentMethod::CreditCard { .. }
    ));
    assert_eq!(decoded.binary_attachment.is_some(), true);
    assert_eq!(decoded.discount_codes.len(), 2);
    assert_eq!(decoded.order_notes.len(), 3);
    assert_eq!(decoded.nested_data.len(), 1);
}

#[test]
#[cfg(all(feature = "chrono", feature = "indexmap"))]
fn test_complex_struct_partial_fields() {
    // Test with some optional fields as None
    let minimal_order = ComplexOrder {
        order_id: 1,
        customer: PersonalInfo {
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            age: 25,
            email: None,
            phone_numbers: vec![],
        },
        shipping_address: Address {
            street: "Test Street".to_string(),
            city: "Test City".to_string(),
            postal_code: None,
            coordinates: None,
        },
        billing_address: None,
        items: vec![],
        payment_methods: vec![PaymentMethod::Cash],
        order_date: DateTime::from_timestamp(1640995200, 0).unwrap(),
        delivery_date: None,
        special_instructions: None,
        discount_codes: BTreeSet::new(),
        order_notes: IndexMap::new(),
        binary_attachment: None,
        nested_data: HashMap::new(),
    };

    let mut buffer = BytesMut::new();
    minimal_order.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = ComplexOrder::decode(&mut reader).unwrap();

    assert_eq!(minimal_order, decoded);
    assert!(decoded.billing_address.is_none());
    assert!(decoded.customer.email.is_none());
    assert!(decoded.items.is_empty());
    assert!(decoded.binary_attachment.is_none());
}

#[test]
fn test_option_u16_debug() {
    // Basic test for Option<u16>
    let value: Option<u16> = Some(123);
    let mut buffer = BytesMut::new();
    value.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Option::<u16>::decode(&mut reader).unwrap();
    assert_eq!(value, decoded);

    // None case
    let none_value: Option<u16> = None;
    let mut buffer2 = BytesMut::new();
    none_value.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded2 = Option::<u16>::decode(&mut reader2).unwrap();
    assert_eq!(none_value, decoded2);
}

#[test]
#[cfg(feature = "chrono")]
fn test_payment_method_debug() {
    // Single PaymentMethod test
    let payment = PaymentMethod::CreditCard {
        number: "1234-5678-9012-3456".to_string(),
        expiry: NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
    };

    let mut buffer = BytesMut::new();
    payment.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = PaymentMethod::decode(&mut reader).unwrap();
    assert_eq!(payment, decoded);
}

#[test]
fn test_simple_struct_with_option() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct SimpleStruct {
        value: Option<u16>,
    }

    let test_struct = SimpleStruct { value: Some(123) };

    let mut buffer = BytesMut::new();
    test_struct.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = SimpleStruct::decode(&mut reader).unwrap();
    assert_eq!(test_struct, decoded);
}

#[test]
fn test_simple_enum_with_option() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    enum SimpleEnum {
        VariantA { value: Option<u16> },
        VariantB,
    }

    let enum_value = SimpleEnum::VariantA { value: Some(123) };

    let mut buffer = BytesMut::new();
    enum_value.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = SimpleEnum::decode(&mut reader).unwrap();
    assert_eq!(enum_value, decoded);
}

#[test]
#[cfg(feature = "chrono")]
fn test_enum_with_chrono() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    enum SimpleEnumChrono {
        VariantA { date: NaiveDate },
        VariantB,
    }

    let enum_value = SimpleEnumChrono::VariantA {
        date: NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
    };

    let mut buffer = BytesMut::new();
    enum_value.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = SimpleEnumChrono::decode(&mut reader).unwrap();
    assert_eq!(enum_value, decoded);
}

#[test]
fn test_cross_decode_struct_option() {
    use bytes::BytesMut;

    #[derive(Debug, PartialEq, Encode, Decode)]
    struct StructA {
        value: i32,
    }
    #[derive(Debug, PartialEq, Encode, Decode)]
    struct StructB {
        value: Option<i32>,
    }

    // StructA → StructB
    let a = StructA { value: 42 };
    let mut buf = BytesMut::new();
    a.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let b = StructB::decode(&mut cur).unwrap();
    assert_eq!(b.value, Some(42));

    // StructB(Some) → StructA
    let b = StructB { value: Some(-99) };
    let mut buf = BytesMut::new();
    b.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let a = StructA::decode(&mut cur).unwrap();
    assert_eq!(a.value, -99);

    // StructB(None) → StructA (should error)
    let b = StructB { value: None };
    let mut buf = BytesMut::new();
    b.encode(&mut buf).unwrap();
    let mut cur = buf.freeze();
    let a = StructA::decode(&mut cur);
    assert!(a.is_err(), "None→i32 should error");
}

#[test]
#[cfg(feature = "rust_decimal")]
fn test_decimal_encode() {
    // Test various Decimal values
    let test_values = vec![
        Decimal::new(0, 0),                                                   // 0
        Decimal::new(123, 0),                                                 // 123
        Decimal::new(-456, 0),                                                // -456
        Decimal::new(12345, 2),                                               // 123.45
        Decimal::new(-67890, 3),                                              // -67.890
        Decimal::from_str("3.14159").unwrap(),                                // π approximation
        Decimal::from_str("999999999999999999999999999.999999999").unwrap(),  // Large value
        Decimal::from_str("-999999999999999999999999999.999999999").unwrap(), // Large negative value
    ];

    for &original in &test_values {
        let mut buffer = BytesMut::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = buffer.freeze();
        let decoded = Decimal::decode(&mut reader).unwrap();

        assert_eq!(
            original, decoded,
            "Failed roundtrip for decimal: {}",
            original
        );
    }
}

#[test]
#[cfg(feature = "rust_decimal")]
fn test_decimal_precision() {
    // Test high-precision Decimal values
    let original = Decimal::from_str("0.123456789012345678901234567890").unwrap();

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Decimal::decode(&mut reader).unwrap();

    assert_eq!(original, decoded);
    assert_eq!(original.to_string(), decoded.to_string());
}

#[test]
#[cfg(feature = "rust_decimal")]
fn test_decimal_zero_and_negative_zero() {
    // Zero and negative zero (Decimal always positive zero)
    let zero = Decimal::ZERO;
    let negative_zero = Decimal::new(0, 0);

    let mut buffer = BytesMut::new();
    zero.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Decimal::decode(&mut reader).unwrap();

    assert_eq!(zero, decoded);
    assert_eq!(negative_zero, decoded);
}

#[test]
#[cfg(feature = "rust_decimal")]
fn test_decimal_in_struct() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct PriceStruct {
        name: String,
        price: Decimal,
        quantity: u32,
        total: Option<Decimal>,
    }

    let product = PriceStruct {
        name: "ノートパソコン".to_string(),
        price: Decimal::from_str("89999.99").unwrap(),
        quantity: 2,
        total: Some(Decimal::from_str("179999.98").unwrap()),
    };

    let mut buffer = BytesMut::new();
    product.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = PriceStruct::decode(&mut reader).unwrap();

    assert_eq!(product, decoded);
}

#[test]
#[cfg(feature = "rust_decimal")]
fn test_decimal_in_collections() {
    // Test Vec<Decimal>
    let prices = vec![
        Decimal::from_str("10.50").unwrap(),
        Decimal::from_str("25.99").unwrap(),
        Decimal::from_str("5.00").unwrap(),
    ];

    let mut buffer = BytesMut::new();
    prices.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded: Vec<Decimal> = Vec::decode(&mut reader).unwrap();

    assert_eq!(prices, decoded);

    // Test HashMap<String, Decimal>
    let mut price_map = HashMap::new();
    price_map.insert("apple".to_string(), Decimal::from_str("1.50").unwrap());
    price_map.insert("banana".to_string(), Decimal::from_str("0.75").unwrap());

    let mut buffer2 = BytesMut::new();
    price_map.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded_map: HashMap<String, Decimal> = HashMap::decode(&mut reader2).unwrap();

    assert_eq!(price_map, decoded_map);
}

#[test]
#[cfg(feature = "uuid")]
fn test_uuid_encode() {
    // Test various UUID values
    let test_uuids = vec![
        Uuid::nil(),                                                     // nil UUID
        Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap(), // Standard UUID
        Uuid::from_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(), // v1 UUID
        Uuid::from_str("6ba7b811-9dad-11d1-80b4-00c04fd430c8").unwrap(), // Another UUID
        Uuid::from_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap(), // Max UUID
    ];

    for &original in &test_uuids {
        let mut buffer = BytesMut::new();
        original.encode(&mut buffer).unwrap();

        // UUID should be 1 + 16 = 17 bytes (1 tag + 16 data bytes)
        assert_eq!(
            buffer.len(),
            17,
            "UUID encoding should produce 17 bytes for UUID: {}",
            original
        );

        let mut reader = buffer.freeze();
        let decoded = Uuid::decode(&mut reader).unwrap();

        assert_eq!(original, decoded, "Failed roundtrip for UUID: {}", original);
    }
}

#[test]
#[cfg(feature = "uuid")]
fn test_uuid_v4_random() {
    // Test for random v4 UUID
    let original = Uuid::new_v4();

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Uuid::decode(&mut reader).unwrap();

    assert_eq!(original, decoded);
    assert_eq!(original.to_string(), decoded.to_string());
}

#[test]
#[cfg(feature = "uuid")]
fn test_uuid_in_struct() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct UserStruct {
        user_id: Uuid,
        name: String,
        session_id: Option<Uuid>,
    }

    let user = UserStruct {
        user_id: Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        name: "田中太郎".to_string(),
        session_id: Some(Uuid::from_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()),
    };

    let mut buffer = BytesMut::new();
    user.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = UserStruct::decode(&mut reader).unwrap();

    assert_eq!(user, decoded);
}

#[test]
#[cfg(feature = "uuid")]
fn test_uuid_in_collections() {
    // Test Vec<Uuid>
    let uuid_list = vec![
        Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        Uuid::from_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
        Uuid::nil(),
    ];

    let mut buffer = BytesMut::new();
    uuid_list.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded: Vec<Uuid> = Vec::decode(&mut reader).unwrap();

    assert_eq!(uuid_list, decoded);

    // Test HashMap<Uuid, String>
    let mut uuid_map = HashMap::new();
    uuid_map.insert(
        Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        "user1".to_string(),
    );
    uuid_map.insert(
        Uuid::from_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
        "user2".to_string(),
    );

    let mut buffer2 = BytesMut::new();
    uuid_map.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded_map: HashMap<Uuid, String> = HashMap::decode(&mut reader2).unwrap();

    assert_eq!(uuid_map, decoded_map);
}

#[test]
#[cfg(feature = "uuid")]
fn test_uuid_nil_special_case() {
    // Test for nil UUID special case
    let nil_uuid = Uuid::nil();

    let mut buffer = BytesMut::new();
    nil_uuid.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Uuid::decode(&mut reader).unwrap();

    assert_eq!(nil_uuid, decoded);
    assert_eq!(decoded.to_string(), "00000000-0000-0000-0000-000000000000");
    assert!(decoded.is_nil());
}

#[test]
#[cfg(feature = "ulid")]
fn test_ulid_encode() {
    // Test various ULID values
    let test_ulids = vec![
        Ulid::nil(),                                              // nil ULID
        Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(), // Standard ULID
        Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVS0").unwrap(), // Another ULID
        Ulid::from_string("7ZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap(), // Max ULID
    ];

    for &original in &test_ulids {
        let mut buffer = BytesMut::new();
        original.encode(&mut buffer).unwrap();

        // ULID should be 1 + 16 = 17 bytes (1 tag + 16 data bytes)
        assert_eq!(
            buffer.len(),
            17,
            "ULID encoding should produce 17 bytes for ULID: {}",
            original
        );

        let mut reader = buffer.freeze();
        let decoded = Ulid::decode(&mut reader).unwrap();

        assert_eq!(original, decoded, "Failed roundtrip for ULID: {}", original);
    }
}

#[test]
#[cfg(feature = "ulid")]
fn test_ulid_new() {
    // Test for new ULID generation
    let original = Ulid::new();

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Ulid::decode(&mut reader).unwrap();

    assert_eq!(original, decoded);
    assert_eq!(original.to_string(), decoded.to_string());
}

#[test]
#[cfg(feature = "ulid")]
fn test_ulid_in_struct() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct EntityStruct {
        entity_id: Ulid,
        name: String,
        parent_id: Option<Ulid>,
    }

    let entity = EntityStruct {
        entity_id: Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
        name: "テストエンティティ".to_string(),
        parent_id: Some(Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVS0").unwrap()),
    };

    let mut buffer = BytesMut::new();
    entity.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = EntityStruct::decode(&mut reader).unwrap();

    assert_eq!(entity, decoded);
}

#[test]
#[cfg(feature = "ulid")]
fn test_ulid_in_collections() {
    // Test Vec<Ulid>
    let ulid_list = vec![
        Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
        Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVS0").unwrap(),
        Ulid::nil(),
    ];

    let mut buffer = BytesMut::new();
    ulid_list.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded: Vec<Ulid> = Vec::decode(&mut reader).unwrap();

    assert_eq!(ulid_list, decoded);

    // Test HashMap<Ulid, String>
    let mut ulid_map = HashMap::new();
    ulid_map.insert(
        Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
        "entity1".to_string(),
    );
    ulid_map.insert(
        Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVS0").unwrap(),
        "entity2".to_string(),
    );

    let mut buffer2 = BytesMut::new();
    ulid_map.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded_map: HashMap<Ulid, String> = HashMap::decode(&mut reader2).unwrap();

    assert_eq!(ulid_map, decoded_map);
}

#[test]
#[cfg(feature = "ulid")]
fn test_ulid_sorting() {
    // Test ULID sorting (time-based)
    use std::thread;
    use std::time::Duration;

    let ulid1 = Ulid::new();
    thread::sleep(Duration::from_millis(1)); // Ensure time difference
    let ulid2 = Ulid::new();

    // ULID should be sortable by time
    assert!(
        ulid1 < ulid2,
        "ULID should be sortable by time: {} < {}",
        ulid1,
        ulid2
    );

    // After encoding/decoding, sort order should be maintained
    let mut buffer1 = BytesMut::new();
    ulid1.encode(&mut buffer1).unwrap();
    let mut reader1 = buffer1.freeze();
    let decoded1 = Ulid::decode(&mut reader1).unwrap();

    let mut buffer2 = BytesMut::new();
    ulid2.encode(&mut buffer2).unwrap();
    let mut reader2 = buffer2.freeze();
    let decoded2 = Ulid::decode(&mut reader2).unwrap();

    assert!(decoded1 < decoded2);
    assert_eq!(ulid1, decoded1);
    assert_eq!(ulid2, decoded2);
}

#[test]
#[cfg(all(feature = "ulid", feature = "uuid"))]
fn test_uuid_ulid_compatibility() {
    // Test UUID and ULID compatibility (same tag)
    let uuid = Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let ulid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();

    // Encode UUID as bytes and then decode as ULID (should not error, binary compatible)
    let mut buffer_uuid = BytesMut::new();
    uuid.encode(&mut buffer_uuid).unwrap();
    let mut reader_uuid = buffer_uuid.freeze();
    let ulid_from_uuid = Ulid::decode(&mut reader_uuid).unwrap();

    // Encode ULID as bytes and then decode as UUID
    let mut buffer_ulid = BytesMut::new();
    ulid.encode(&mut buffer_ulid).unwrap();
    let mut reader_ulid = buffer_ulid.freeze();
    let uuid_from_ulid = Uuid::decode(&mut reader_ulid).unwrap();

    // Confirm binary conversion works (value is different but should not error)
    assert_ne!(ulid_from_uuid.to_string(), uuid.to_string());
    assert_ne!(uuid_from_ulid.to_string(), ulid.to_string());

    // However, original value should encode/decode correctly
    let mut buffer_uuid_orig = BytesMut::new();
    uuid.encode(&mut buffer_uuid_orig).unwrap();
    let mut reader_uuid_orig = buffer_uuid_orig.freeze();
    let uuid_decoded = Uuid::decode(&mut reader_uuid_orig).unwrap();
    assert_eq!(uuid, uuid_decoded);

    let mut buffer_ulid_orig = BytesMut::new();
    ulid.encode(&mut buffer_ulid_orig).unwrap();
    let mut reader_ulid_orig = buffer_ulid_orig.freeze();
    let ulid_decoded = Ulid::decode(&mut reader_ulid_orig).unwrap();
    assert_eq!(ulid, ulid_decoded);
}

#[test]
fn test_u8_optimization_boundaries() {
    use bytes::BytesMut;
    use senax_encoder::{Decoder, Encoder};

    // Test TAG_U8 optimization boundaries
    let test_cases = [
        // TAG_ZERO boundaries
        (127u16, vec![127]), // TAG_ZERO + 127 = 127
        // TAG_U8 range
        (128u16, vec![131, 0]),   // TAG_U8, 128-128=0
        (383u16, vec![131, 255]), // TAG_U8, 383-128=255 (max)
        // TAG_U16 starts
        (384u16, vec![132, 128, 1]), // TAG_U16, 384 in LE
    ];

    for (val, expected) in test_cases {
        let mut buf = BytesMut::new();
        val.encode(&mut buf).unwrap();
        assert_eq!(buf.as_ref(), expected, "Failed encoding for value {}", val);

        // Round-trip test
        let mut read_buf = buf.freeze();
        let decoded = u16::decode(&mut read_buf).unwrap();
        assert_eq!(decoded, val, "Round-trip failed for value {}", val);
    }
}

#[test]
fn test_arc_generic_encode_decode() {
    // Arc<String>
    let values_string = vec![
        Arc::new(String::from("")),
        Arc::new(String::from("hello")),
        Arc::new(String::from("こんにちは世界")),
        Arc::new(String::from(
            "a very long string with unicode 🚀 and symbols !@#$%^&*()",
        )),
    ];
    for original in values_string {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = Arc::<String>::decode(&mut cur).unwrap();
        assert_eq!(&*original, &*decoded, "Arc<String> roundtrip failed");
        // Check that Arc is not the same pointer, but content is equal
        assert!(!Arc::ptr_eq(&original, &decoded) || Arc::strong_count(&original) == 1);
    }

    // Arc<i32>
    let values_i32 = vec![
        Arc::new(0i32),
        Arc::new(42i32),
        Arc::new(-123i32),
        Arc::new(i32::MAX),
        Arc::new(i32::MIN),
    ];
    for original in values_i32 {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = Arc::<i32>::decode(&mut cur).unwrap();
        assert_eq!(&*original, &*decoded, "Arc<i32> roundtrip failed");
    }

    // Arc<Vec<u8>>
    let values_vec = vec![
        Arc::new(Vec::<u8>::new()),
        Arc::new(vec![1, 2, 3, 4, 5]),
        Arc::new(vec![0xFF, 0x00, 0xAB, 0xCD]),
    ];
    for original in values_vec {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = Arc::<Vec<u8>>::decode(&mut cur).unwrap();
        assert_eq!(&*original, &*decoded, "Arc<Vec<u8>> roundtrip failed");
    }

    // Arc<Option<String>>
    let values_option = vec![
        Arc::new(None::<String>),
        Arc::new(Some("test".to_string())),
        Arc::new(Some("".to_string())),
    ];
    for original in values_option {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = Arc::<Option<String>>::decode(&mut cur).unwrap();
        assert_eq!(
            &*original, &*decoded,
            "Arc<Option<String>> roundtrip failed"
        );
    }
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct ArcGenericStruct {
    name: Arc<String>,
    id: Arc<i32>,
    data: Arc<Vec<u8>>,
    nickname: Option<Arc<String>>,
}

#[test]
fn test_struct_with_arc_generic_fields() {
    let cases = vec![
        ArcGenericStruct {
            name: Arc::new("Alice".to_string()),
            id: Arc::new(42),
            data: Arc::new(vec![1, 2, 3]),
            nickname: Some(Arc::new("Ally".to_string())),
        },
        ArcGenericStruct {
            name: Arc::new("Bob".to_string()),
            id: Arc::new(0),
            data: Arc::new(Vec::new()),
            nickname: None,
        },
        ArcGenericStruct {
            name: Arc::new("".to_string()),
            id: Arc::new(-999),
            data: Arc::new(vec![0xFF]),
            nickname: Some(Arc::new("".to_string())),
        },
        ArcGenericStruct {
            name: Arc::new("こんにちは".to_string()),
            id: Arc::new(i32::MAX),
            data: Arc::new(vec![0x00, 0x01, 0x02]),
            nickname: Some(Arc::new("世界".to_string())),
        },
    ];
    for original in cases {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = ArcGenericStruct::decode(&mut cur).unwrap();
        assert_eq!(original, decoded, "ArcGenericStruct roundtrip failed");
    }
}

#[test]
fn test_arc_is_default_behavior() {
    assert!(Arc::new(String::new()).is_default());
    assert!(Arc::new("".to_string()).is_default());
    assert!(!Arc::new("hello".to_string()).is_default());
    assert!(Arc::new(0i32).is_default());
    assert!(!Arc::new(1i32).is_default());
    assert!(Arc::new(Vec::<u8>::new()).is_default());
    assert!(Arc::new(None::<String>).is_default());
}

#[test]
#[cfg(feature = "serde_json")]
fn test_serde_json_value_encode() {
    // Test various JSON Value types
    let test_values: Vec<Value> = vec![
        Value::Null,
        Value::Bool(true),
        Value::Bool(false),
        json!(42),
        json!(3.14159),
        json!("hello world"),
        json!([1, 2, 3, "four", null]),
        json!({
            "name": "Alice",
            "age": 30,
            "active": true,
            "balance": 123.45,
            "tags": ["user", "premium"],
            "metadata": {
                "last_login": "2024-01-01",
                "preferences": {
                    "theme": "dark",
                    "notifications": true
                }
            }
        }),
    ];

    for original in &test_values {
        let mut buffer = BytesMut::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = buffer.freeze();
        let decoded = Value::decode(&mut reader).unwrap();

        assert_eq!(
            original, &decoded,
            "Failed roundtrip for JSON Value: {}",
            original
        );
    }
}

#[test]
#[cfg(feature = "serde_json")]
fn test_serde_json_value_complex() {
    // Test complex nested JSON structure
    let complex_json = json!({
        "users": [
            {
                "id": 1,
                "name": "Alice",
                "profile": {
                    "email": "alice@example.com",
                    "verified": true,
                    "scores": [95.5, 87.2, 91.0]
                }
            },
            {
                "id": 2,
                "name": "Bob",
                "profile": {
                    "email": "bob@example.com",
                    "verified": false,
                    "scores": null
                }
            }
        ],
        "metadata": {
            "total": 2,
            "last_updated": "2024-01-01T00:00:00Z",
            "features": {
                "advanced_search": true,
                "notifications": false
            }
        }
    });

    let mut buffer = BytesMut::new();
    complex_json.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Value::decode(&mut reader).unwrap();

    assert_eq!(complex_json, decoded);
}

#[test]
#[cfg(feature = "serde_json")]
fn test_serde_json_value_in_struct() {
    // Test JSON Value as struct field
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct ConfigStruct {
        name: String,
        config: Value,
        enabled: bool,
    }

    let original = ConfigStruct {
        name: "app_config".to_string(),
        config: json!({
            "database": {
                "host": "localhost",
                "port": 5432,
                "ssl": true
            },
            "cache": {
                "ttl": 3600,
                "enabled": true
            },
            "features": ["analytics", "auth", "notifications"]
        }),
        enabled: true,
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = ConfigStruct::decode(&mut reader).unwrap();

    assert_eq!(original, decoded);
}

#[test]
#[cfg(feature = "serde_json")]
fn test_serde_json_value_empty_containers() {
    // Test empty arrays and objects
    let test_values: Vec<Value> = vec![
        json!([]),
        json!({}),
        json!({
            "empty_array": [],
            "empty_object": {},
            "nested": {
                "also_empty": []
            }
        }),
    ];

    for original in &test_values {
        let mut buffer = BytesMut::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = buffer.freeze();
        let decoded = Value::decode(&mut reader).unwrap();

        assert_eq!(original, &decoded);
    }
}

#[test]
fn test_is_default_method() {
    use senax_encoder::Encoder;

    // Test primitive types
    assert!(0u32.is_default());
    assert!(!42u32.is_default());
    assert!(0i32.is_default());
    assert!(!(-5i32).is_default());
    assert!(false.is_default());
    assert!(!true.is_default());
    assert!(0.0f64.is_default());
    assert!(!3.14f64.is_default());

    // Test String
    assert!("".to_string().is_default());
    assert!(!"hello".to_string().is_default());

    // Test Option
    assert!(Option::<i32>::None.is_default());
    assert!(!Some(42).is_default());

    // Test Vec
    assert!(Vec::<i32>::new().is_default());
    assert!(!vec![1, 2, 3].is_default());

    // Test tuples
    assert!(().is_default());
    assert!((0, "".to_string()).is_default());
    assert!(!(42, "hello".to_string()).is_default());
}

#[test]
fn test_skip_default_attribute() {
    use senax_encoder::{Decoder, Encoder};

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct TestSkipDefault {
        #[senax(skip_default)]
        optional_field: i32,
        normal_field: String,
        #[senax(skip_default)]
        default_string: String,
    }

    // Test with default values - should be skipped during encoding
    let test_default = TestSkipDefault {
        optional_field: 0, // default value
        normal_field: "hello".to_string(),
        default_string: "".to_string(), // default value
    };

    let mut buffer = BytesMut::new();
    test_default.encode(&mut buffer).unwrap();
    let buffer1_len = buffer.len();

    // Decode and verify
    let mut reader = buffer.freeze();
    let decoded = TestSkipDefault::decode(&mut reader).unwrap();
    assert_eq!(test_default, decoded);

    // Test with non-default values - should be encoded
    let test_non_default = TestSkipDefault {
        optional_field: 42, // non-default value
        normal_field: "world".to_string(),
        default_string: "non-empty".to_string(), // non-default value
    };

    let mut buffer2 = BytesMut::new();
    test_non_default.encode(&mut buffer2).unwrap();
    let buffer2_len = buffer2.len();

    // Decode and verify
    let mut reader2 = buffer2.freeze();
    let decoded2 = TestSkipDefault::decode(&mut reader2).unwrap();
    assert_eq!(test_non_default, decoded2);

    // Buffer with default values should be smaller than buffer with non-default values
    // (since default fields are skipped)
    assert!(buffer1_len < buffer2_len);
}

#[test]
fn test_tuple_as_values() {
    // Test tuples as struct fields
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct TupleContainer {
        coordinates: (f64, f64),
        rgb_color: (u8, u8, u8),
        name_and_score: (String, i32),
        optional_pair: Option<(i32, String)>,
        nested_tuple: ((i32, i32), (String, bool)),
    }

    let container = TupleContainer {
        coordinates: (35.6762, 139.6503), // Tokyo coordinates
        rgb_color: (255, 128, 64),
        name_and_score: ("Alice".to_string(), 95),
        optional_pair: Some((42, "test".to_string())),
        nested_tuple: ((10, 20), ("nested".to_string(), true)),
    };

    let mut buffer = BytesMut::new();
    container.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = TupleContainer::decode(&mut reader).unwrap();

    assert_eq!(container, decoded);

    // Test with None optional tuple
    let container_with_none = TupleContainer {
        coordinates: (0.0, 0.0),
        rgb_color: (0, 0, 0),
        name_and_score: ("".to_string(), 0),
        optional_pair: None,
        nested_tuple: ((0, 0), ("".to_string(), false)),
    };

    let mut buffer2 = BytesMut::new();
    container_with_none.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded2 = TupleContainer::decode(&mut reader2).unwrap();

    assert_eq!(container_with_none, decoded2);
}

#[test]
fn test_tuple_in_collections() {
    // Test tuples in Vec
    let tuple_vec = vec![
        (1, "first".to_string()),
        (2, "second".to_string()),
        (3, "third".to_string()),
    ];

    let mut buffer = BytesMut::new();
    tuple_vec.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded: Vec<(i32, String)> = Vec::decode(&mut reader).unwrap();

    assert_eq!(tuple_vec, decoded);

    // Test tuples in HashMap
    let mut tuple_map = HashMap::new();
    tuple_map.insert("point1".to_string(), (10, 20));
    tuple_map.insert("point2".to_string(), (30, 40));

    let mut buffer2 = BytesMut::new();
    tuple_map.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded_map: HashMap<String, (i32, i32)> = HashMap::decode(&mut reader2).unwrap();

    assert_eq!(tuple_map, decoded_map);
}

#[test]
fn test_various_tuple_sizes() {
    // Test tuples of different sizes individually

    // Test 1-tuple
    let tuple_1 = (42,);
    let mut buffer = BytesMut::new();
    tuple_1.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_1 = <(i32,)>::decode(&mut reader).unwrap();
    assert_eq!(tuple_1, decoded_1);

    // Test 2-tuple
    let tuple_2 = (1, 2);
    let mut buffer = BytesMut::new();
    tuple_2.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_2 = <(i32, i32)>::decode(&mut reader).unwrap();
    assert_eq!(tuple_2, decoded_2);

    // Test 3-tuple
    let tuple_3 = (1, "two".to_string(), 3.0);
    let mut buffer = BytesMut::new();
    tuple_3.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_3 = <(i32, String, f64)>::decode(&mut reader).unwrap();
    assert_eq!(tuple_3, decoded_3);

    // Test 4-tuple
    let tuple_4 = (1, 2, 3, 4);
    let mut buffer = BytesMut::new();
    tuple_4.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_4 = <(i32, i32, i32, i32)>::decode(&mut reader).unwrap();
    assert_eq!(tuple_4, decoded_4);

    // Test 5-tuple
    let tuple_5 = (1, 2, 3, 4, 5);
    let mut buffer = BytesMut::new();
    tuple_5.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_5 = <(i32, i32, i32, i32, i32)>::decode(&mut reader).unwrap();
    assert_eq!(tuple_5, decoded_5);

    // Test 10-tuple
    let tuple_10 = (1, 2, 3, 4, 5, 6, 7, 8, 9, 10);
    let mut buffer = BytesMut::new();
    tuple_10.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_10 =
        <(i32, i32, i32, i32, i32, i32, i32, i32, i32, i32)>::decode(&mut reader).unwrap();
    assert_eq!(tuple_10, decoded_10);

    // Test 12-tuple (newly supported)
    let tuple_12 = (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
    let mut buffer = BytesMut::new();
    tuple_12.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_12 =
        <(i32, i32, i32, i32, i32, i32, i32, i32, i32, i32, i32, i32)>::decode(&mut reader)
            .unwrap();
    assert_eq!(tuple_12, decoded_12);
}

#[test]
fn test_unnamed_structs() {
    // Test basic tuple struct
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Point2D(f64, f64);

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Color(u8, u8, u8, u8); // RGBA

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct UserId(u64);

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Wrapper(String, i32, bool);

    let point = Point2D(10.5, 20.3);
    let color = Color(255, 128, 64, 200);
    let user_id = UserId(123456789);
    let wrapper = Wrapper("test".to_string(), 42, true);

    // Test Point2D
    let mut buffer = BytesMut::new();
    point.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_point = Point2D::decode(&mut reader).unwrap();
    assert_eq!(point, decoded_point);

    // Test Color
    let mut buffer = BytesMut::new();
    color.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_color = Color::decode(&mut reader).unwrap();
    assert_eq!(color, decoded_color);

    // Test UserId
    let mut buffer = BytesMut::new();
    user_id.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_user_id = UserId::decode(&mut reader).unwrap();
    assert_eq!(user_id, decoded_user_id);

    // Test Wrapper
    let mut buffer = BytesMut::new();
    wrapper.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_wrapper = Wrapper::decode(&mut reader).unwrap();
    assert_eq!(wrapper, decoded_wrapper);
}

#[test]
fn test_unnamed_struct_with_complex_types() {
    // Test tuple struct with complex types
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct ComplexTuple(Option<String>, Vec<i32>, HashMap<String, u32>, (f64, f64));

    let mut map = HashMap::new();
    map.insert("key1".to_string(), 100);
    map.insert("key2".to_string(), 200);

    let complex = ComplexTuple(
        Some("hello".to_string()),
        vec![1, 2, 3, 4, 5],
        map,
        (3.14, 2.71),
    );

    let mut buffer = BytesMut::new();
    complex.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = ComplexTuple::decode(&mut reader).unwrap();

    assert_eq!(complex, decoded);

    // Test with None option
    let complex_none = ComplexTuple(None, vec![], HashMap::new(), (0.0, 0.0));

    let mut buffer2 = BytesMut::new();
    complex_none.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded2 = ComplexTuple::decode(&mut reader2).unwrap();

    assert_eq!(complex_none, decoded2);
}

#[test]
fn test_unnamed_struct_in_collections() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Point(i32, i32);

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Distance(f64);

    // Test Vec of tuple structs
    let points = vec![Point(0, 0), Point(10, 20), Point(-5, 15)];

    let mut buffer = BytesMut::new();
    points.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded_points: Vec<Point> = Vec::decode(&mut reader).unwrap();

    assert_eq!(points, decoded_points);

    // Test HashMap with tuple struct as value
    let mut distance_map = HashMap::new();
    distance_map.insert("short".to_string(), Distance(1.5));
    distance_map.insert("medium".to_string(), Distance(10.0));
    distance_map.insert("long".to_string(), Distance(100.5));

    let mut buffer2 = BytesMut::new();
    distance_map.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded_map: HashMap<String, Distance> = HashMap::decode(&mut reader2).unwrap();

    assert_eq!(distance_map, decoded_map);
}

#[test]
fn test_nested_unnamed_structs() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Inner(i32, String);

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Outer(Inner, bool, Option<Inner>);

    let inner1 = Inner(42, "first".to_string());
    let inner2 = Inner(99, "second".to_string());
    let outer = Outer(inner1, true, Some(inner2));

    let mut buffer = BytesMut::new();
    outer.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Outer::decode(&mut reader).unwrap();

    assert_eq!(outer, decoded);

    // Test with None inner
    let inner_only = Inner(123, "only".to_string());
    let outer_none = Outer(inner_only, false, None);

    let mut buffer2 = BytesMut::new();
    outer_none.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded2 = Outer::decode(&mut reader2).unwrap();

    assert_eq!(outer_none, decoded2);
}

#[test]
fn test_enum_field_skip_default() {
    use senax_encoder::{Decoder, Encoder};

    #[derive(Encode, Decode, Debug, PartialEq)]
    enum TestEnumSkipDefault {
        VariantA {
            #[senax(skip_default)]
            optional_field: i32,
            normal_field: String,
            #[senax(skip_default)]
            default_string: String,
        },
        VariantB {
            value: u64,
        },
    }

    // Test with default values - should be skipped during encoding
    let test_default = TestEnumSkipDefault::VariantA {
        optional_field: 0, // default value
        normal_field: "hello".to_string(),
        default_string: "".to_string(), // default value
    };

    let mut buffer = BytesMut::new();
    test_default.encode(&mut buffer).unwrap();
    let buffer1_len = buffer.len();

    // Decode and verify
    let mut reader = buffer.freeze();
    let decoded = TestEnumSkipDefault::decode(&mut reader).unwrap();
    assert_eq!(test_default, decoded);

    // Test with non-default values - should be encoded
    let test_non_default = TestEnumSkipDefault::VariantA {
        optional_field: 42, // non-default value
        normal_field: "world".to_string(),
        default_string: "non-empty".to_string(), // non-default value
    };

    let mut buffer2 = BytesMut::new();
    test_non_default.encode(&mut buffer2).unwrap();
    let buffer2_len = buffer2.len();

    // Decode and verify
    let mut reader2 = buffer2.freeze();
    let decoded2 = TestEnumSkipDefault::decode(&mut reader2).unwrap();
    assert_eq!(test_non_default, decoded2);

    // Buffer with default values should be smaller than buffer with non-default values
    // (since default fields are skipped)
    assert!(buffer1_len < buffer2_len);
}

#[test]
fn test_enum_default_attribute() {
    use senax_encoder::{Decoder, Encoder};

    #[derive(Default, Encode, Decode, Debug, PartialEq)]
    enum Padding {
        Space,
        Zero,
        #[default]
        None,
    }

    #[derive(Default, Encode, Decode, Debug, PartialEq)]
    enum Status {
        #[default]
        Inactive,
        Active {
            enabled: bool,
        },
        Pending(String),
    }

    // Test unit variant with #[default]
    let default_padding = Padding::default();
    assert_eq!(default_padding, Padding::None);

    // Test that the default variant returns true for is_default()
    assert!(default_padding.is_default());
    assert!(!Padding::Space.is_default());
    assert!(!Padding::Zero.is_default());

    // Test struct variant with #[default]
    let default_status = Status::default();
    assert_eq!(default_status, Status::Inactive);
    assert!(default_status.is_default());
    assert!(!Status::Active { enabled: true }.is_default());
    assert!(!Status::Active { enabled: false }.is_default()); // Even with default field values
    assert!(!Status::Pending("test".to_string()).is_default());

    // Test encoding/decoding
    let mut buffer = BytesMut::new();
    default_padding.encode(&mut buffer).unwrap();
    let mut reader = buffer.freeze();
    let decoded_padding = Padding::decode(&mut reader).unwrap();
    assert_eq!(default_padding, decoded_padding);
    assert!(decoded_padding.is_default());

    let mut buffer2 = BytesMut::new();
    default_status.encode(&mut buffer2).unwrap();
    let mut reader2 = buffer2.freeze();
    let decoded_status = Status::decode(&mut reader2).unwrap();
    assert_eq!(default_status, decoded_status);
    assert!(decoded_status.is_default());
}

#[cfg(feature = "fxhash")]
#[test]
fn test_fxhashmap_encode_decode() {
    use fxhash::FxHashMap;

    let mut map = FxHashMap::default();
    map.insert("key1".to_string(), 42u32);
    map.insert("key2".to_string(), 100u32);

    let mut buf = BytesMut::new();
    map.encode(&mut buf).unwrap();

    let decoded: FxHashMap<String, u32> = FxHashMap::decode(&mut buf.freeze()).unwrap();
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded.get("key1"), Some(&42));
    assert_eq!(decoded.get("key2"), Some(&100));

    // Test is_default
    let empty_map: FxHashMap<String, u32> = FxHashMap::default();
    assert!(empty_map.is_default());
    assert!(!map.is_default());
}

#[cfg(feature = "ahash")]
#[test]
fn test_ahashmap_encode_decode() {
    use ahash::AHashMap;

    let mut map = AHashMap::new();
    map.insert("key1".to_string(), 42u32);
    map.insert("key2".to_string(), 100u32);

    let mut buf = BytesMut::new();
    map.encode(&mut buf).unwrap();

    let decoded: AHashMap<String, u32> = AHashMap::decode(&mut buf.freeze()).unwrap();
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded.get("key1"), Some(&42));
    assert_eq!(decoded.get("key2"), Some(&100));

    // Test is_default
    let empty_map: AHashMap<String, u32> = AHashMap::new();
    assert!(empty_map.is_default());
    assert!(!map.is_default());
}

#[cfg(feature = "fxhash")]
#[test]
fn test_fxhashset_encode_decode() {
    use fxhash::FxHashSet;

    let mut set = FxHashSet::default();
    set.insert("item1".to_string());
    set.insert("item2".to_string());
    set.insert("item3".to_string());

    let mut buf = BytesMut::new();
    set.encode(&mut buf).unwrap();

    let decoded: FxHashSet<String> = FxHashSet::decode(&mut buf.freeze()).unwrap();
    assert_eq!(decoded.len(), 3);
    assert!(decoded.contains("item1"));
    assert!(decoded.contains("item2"));
    assert!(decoded.contains("item3"));

    // Test is_default
    let empty_set: FxHashSet<String> = FxHashSet::default();
    assert!(empty_set.is_default());
    assert!(!set.is_default());
}

#[cfg(feature = "ahash")]
#[test]
fn test_ahashset_encode_decode() {
    use ahash::AHashSet;

    let mut set = AHashSet::new();
    set.insert("item1".to_string());
    set.insert("item2".to_string());
    set.insert("item3".to_string());

    let mut buf = BytesMut::new();
    set.encode(&mut buf).unwrap();

    let decoded: AHashSet<String> = AHashSet::decode(&mut buf.freeze()).unwrap();
    assert_eq!(decoded.len(), 3);
    assert!(decoded.contains("item1"));
    assert!(decoded.contains("item2"));
    assert!(decoded.contains("item3"));

    // Test is_default
    let empty_set: AHashSet<String> = AHashSet::new();
    assert!(empty_set.is_default());
    assert!(!set.is_default());
}

#[cfg(feature = "smol_str")]
#[test]
fn test_smolstr_encode_decode() {
    use smol_str::SmolStr;

    // Test various SmolStr values
    let test_values = vec![
        SmolStr::new(""),                       // Empty string
        SmolStr::new("short"),                  // Short string
        SmolStr::new("Hello, World!"),          // Medium string
        SmolStr::new("こんにちは世界"),         // Unicode string
        SmolStr::new("a".repeat(100)),          // Long string
        SmolStr::new("🚀 Rust is awesome! 🦀"), // Emoji string
    ];

    for original in &test_values {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();

        let decoded = SmolStr::decode(&mut buf.freeze()).unwrap();
        assert_eq!(
            original, &decoded,
            "Failed roundtrip for SmolStr: {}",
            original
        );
    }

    // Test is_default
    let empty_smolstr = SmolStr::new("");
    assert!(empty_smolstr.is_default());

    let non_empty_smolstr = SmolStr::new("test");
    assert!(!non_empty_smolstr.is_default());
}

#[cfg(feature = "smol_str")]
#[test]
fn test_smolstr_string_compatibility() {
    use smol_str::SmolStr;

    // Test that SmolStr can decode from String-encoded data
    let original_string = "Hello, SmolStr!";
    let mut buf = BytesMut::new();
    original_string.to_string().encode(&mut buf).unwrap();

    let decoded_smolstr = SmolStr::decode(&mut buf.freeze()).unwrap();
    assert_eq!(decoded_smolstr.as_str(), original_string);

    // Test that String can decode from SmolStr-encoded data
    let original_smolstr = SmolStr::new("Hello, String!");
    let mut buf2 = BytesMut::new();
    original_smolstr.encode(&mut buf2).unwrap();

    let decoded_string = String::decode(&mut buf2.freeze()).unwrap();
    assert_eq!(decoded_string, original_smolstr.as_str());
}

#[cfg(feature = "smol_str")]
#[test]
fn test_smolstr_in_struct() {
    use smol_str::SmolStr;

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct SmolStrStruct {
        name: SmolStr,
        description: Option<SmolStr>,
        tags: Vec<SmolStr>,
    }

    let test_struct = SmolStrStruct {
        name: SmolStr::new("test_name"),
        description: Some(SmolStr::new("test description")),
        tags: vec![
            SmolStr::new("tag1"),
            SmolStr::new("tag2"),
            SmolStr::new("tag3"),
        ],
    };

    let mut buf = BytesMut::new();
    test_struct.encode(&mut buf).unwrap();

    let decoded = SmolStrStruct::decode(&mut buf.freeze()).unwrap();
    assert_eq!(test_struct, decoded);
}

#[cfg(feature = "smol_str")]
#[test]
fn test_smolstr_in_collections() {
    use smol_str::SmolStr;
    use std::collections::HashMap;

    // Test Vec<SmolStr>
    let smolstr_vec = vec![
        SmolStr::new("first"),
        SmolStr::new("second"),
        SmolStr::new("third"),
    ];

    let mut buf = BytesMut::new();
    smolstr_vec.encode(&mut buf).unwrap();

    let decoded_vec: Vec<SmolStr> = Vec::decode(&mut buf.freeze()).unwrap();
    assert_eq!(smolstr_vec, decoded_vec);

    // Test HashMap<SmolStr, String>
    let mut smolstr_map = HashMap::new();
    smolstr_map.insert(SmolStr::new("key1"), "value1".to_string());
    smolstr_map.insert(SmolStr::new("key2"), "value2".to_string());

    let mut buf2 = BytesMut::new();
    smolstr_map.encode(&mut buf2).unwrap();

    let decoded_map: HashMap<SmolStr, String> = HashMap::decode(&mut buf2.freeze()).unwrap();
    assert_eq!(smolstr_map, decoded_map);
}

#[cfg(all(feature = "fxhash", feature = "ahash", feature = "smol_str"))]
#[test]
fn test_combined_features() {
    use ahash::AHashMap;
    use fxhash::FxHashSet;
    use smol_str::SmolStr;

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct CombinedStruct {
        fx_set: FxHashSet<SmolStr>,
        a_map: AHashMap<SmolStr, u32>,
    }

    let mut fx_set = FxHashSet::default();
    fx_set.insert(SmolStr::new("item1"));
    fx_set.insert(SmolStr::new("item2"));

    let mut a_map = AHashMap::new();
    a_map.insert(SmolStr::new("key1"), 100);
    a_map.insert(SmolStr::new("key2"), 200);

    let combined = CombinedStruct { fx_set, a_map };

    let mut buf = BytesMut::new();
    combined.encode(&mut buf).unwrap();

    let decoded = CombinedStruct::decode(&mut buf.freeze()).unwrap();
    assert_eq!(combined, decoded);
}

#[test]
fn test_box_generic_encode_decode() {
    // Box<String>
    let values_string = vec![
        Box::new(String::from("")),
        Box::new(String::from("hello")),
        Box::new(String::from("こんにちは世界")),
        Box::new(String::from(
            "a very long string with unicode 🚀 and symbols !@#$%^&*()",
        )),
    ];
    for original in values_string {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = Box::<String>::decode(&mut cur).unwrap();
        assert_eq!(&*original, &*decoded, "Box<String> roundtrip failed");
    }

    // Box<i32>
    let values_i32 = vec![
        Box::new(0i32),
        Box::new(42i32),
        Box::new(-123i32),
        Box::new(i32::MAX),
        Box::new(i32::MIN),
    ];
    for original in values_i32 {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = Box::<i32>::decode(&mut cur).unwrap();
        assert_eq!(&*original, &*decoded, "Box<i32> roundtrip failed");
    }

    // Box<Vec<u8>>
    let values_vec = vec![
        Box::new(Vec::<u8>::new()),
        Box::new(vec![1, 2, 3, 4, 5]),
        Box::new(vec![0xFF, 0x00, 0xAB, 0xCD]),
    ];
    for original in values_vec {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = Box::<Vec<u8>>::decode(&mut cur).unwrap();
        assert_eq!(&*original, &*decoded, "Box<Vec<u8>> roundtrip failed");
    }

    // Box<Option<String>>
    let values_option = vec![
        Box::new(None::<String>),
        Box::new(Some("test".to_string())),
        Box::new(Some("".to_string())),
    ];
    for original in values_option {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = Box::<Option<String>>::decode(&mut cur).unwrap();
        assert_eq!(
            &*original, &*decoded,
            "Box<Option<String>> roundtrip failed"
        );
    }
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct BoxGenericStruct {
    name: Box<String>,
    id: Box<i32>,
    data: Box<Vec<u8>>,
    nickname: Option<Box<String>>,
}

#[test]
fn test_struct_with_box_generic_fields() {
    let cases = vec![
        BoxGenericStruct {
            name: Box::new("Alice".to_string()),
            id: Box::new(42),
            data: Box::new(vec![1, 2, 3]),
            nickname: Some(Box::new("Ally".to_string())),
        },
        BoxGenericStruct {
            name: Box::new("Bob".to_string()),
            id: Box::new(0),
            data: Box::new(Vec::new()),
            nickname: None,
        },
        BoxGenericStruct {
            name: Box::new("".to_string()),
            id: Box::new(-999),
            data: Box::new(vec![0xFF]),
            nickname: Some(Box::new("".to_string())),
        },
        BoxGenericStruct {
            name: Box::new("こんにちは".to_string()),
            id: Box::new(i32::MAX),
            data: Box::new(vec![0x00, 0x01, 0x02]),
            nickname: Some(Box::new("世界".to_string())),
        },
    ];
    for original in cases {
        let mut buf = BytesMut::new();
        original.encode(&mut buf).unwrap();
        let mut cur = buf.freeze();
        let decoded = BoxGenericStruct::decode(&mut cur).unwrap();
        assert_eq!(original, decoded, "BoxGenericStruct roundtrip failed");
    }
}

#[test]
#[allow(unused_allocation)]
fn test_box_is_default_behavior() {
    assert!(Box::new(String::new()).is_default());
    assert!(Box::new("".to_string()).is_default());
    assert!(!Box::new("hello".to_string()).is_default());
    assert!(Box::new(0i32).is_default());
    assert!(!Box::new(1i32).is_default());
    assert!(Box::new(Vec::<u8>::new()).is_default());
    assert!(Box::new(None::<String>).is_default());
}

#[test]
fn test_box_vs_arc_compatibility() {
    // Test that Box<T> and Arc<T> encode the same way (since they both encode the inner value)
    let box_value = Box::new("test".to_string());
    let arc_value = Arc::new("test".to_string());

    let mut box_buf = BytesMut::new();
    box_value.encode(&mut box_buf).unwrap();

    let mut arc_buf = BytesMut::new();
    arc_value.encode(&mut arc_buf).unwrap();

    // The encoded bytes should be identical
    assert_eq!(box_buf.as_ref(), arc_buf.as_ref());

    // Cross-decoding should work
    let box_from_arc = Box::<String>::decode(&mut arc_buf.freeze()).unwrap();
    let arc_from_box = Arc::<String>::decode(&mut box_buf.freeze()).unwrap();

    assert_eq!(&*box_value, &*box_from_arc);
    assert_eq!(&*arc_value, &*arc_from_box);
}
