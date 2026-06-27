use bytes::{BufMut, BytesMut};
use senax_encoder::encode;
use senax_encoder::{
    pack, unpack, Decode, Decoder, Encode, Encoder, Pack, Packer, Unpack, Unpacker,
};
use std::collections::HashMap;

#[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
struct SimpleStruct {
    id: u32,
    name: String,
    active: bool,
}

#[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
struct SimpleStructForComparison {
    id: u32,
    name: String,
    active: bool,
}

#[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
struct TupleStruct(u32, String, bool);

#[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
struct UnitStruct;

#[test]
fn test_pack_unpack_named_struct() {
    let original = SimpleStruct {
        id: 42,
        name: "hello".to_string(),
        active: true,
    };

    // Pack the struct
    let packed_bytes = pack(&original).unwrap();

    // Unpack the struct
    let mut reader = packed_bytes;
    let unpacked: SimpleStruct = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_pack_unpack_tuple_struct() {
    let original = TupleStruct(123, "world".to_string(), false);

    // Pack the struct
    let packed_bytes = pack(&original).unwrap();

    // Unpack the struct
    let mut reader = packed_bytes;
    let unpacked: TupleStruct = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_pack_unpack_unit_struct() {
    let original = UnitStruct;

    // Pack the struct
    let packed_bytes = pack(&original).unwrap();

    // Unpack the struct
    let mut reader = packed_bytes;
    let unpacked: UnitStruct = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_pack_vs_encode_size_difference() {
    let original_pack = SimpleStruct {
        id: 42,
        name: "hello".to_string(),
        active: true,
    };

    let original_encode = SimpleStructForComparison {
        id: 42,
        name: "hello".to_string(),
        active: true,
    };

    // Pack (no field IDs)
    let packed_bytes = pack(&original_pack).unwrap();

    // Encode (with field IDs)
    let encoded_bytes = encode(&original_encode).unwrap();

    // Pack should be smaller since it doesn't include field IDs
    println!("Packed size: {}", packed_bytes.len());
    println!("Encoded size: {}", encoded_bytes.len());
    assert!(packed_bytes.len() < encoded_bytes.len());
}

#[test]
fn test_pack_unpack_field_order_dependency() {
    // This test demonstrates that pack/unpack is order-dependent
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct StructA {
        first: u32,
        second: String,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct StructB {
        second: String, // Different order!
        first: u32,
    }

    let original_a = StructA {
        first: 42,
        second: "hello".to_string(),
    };

    // Pack as StructA
    let packed_bytes = pack(&original_a).unwrap();

    // Try to unpack as StructB (different field order)
    let mut reader = packed_bytes;
    let result: Result<StructB, _> = unpack(&mut reader);

    // This should fail or produce incorrect results because field order matters
    // We can't directly test the failure since the types are different,
    // but this demonstrates the concept
    assert!(result.is_err() || result.unwrap().first != 42);
}

#[test]
fn test_nested_structs_pack_unpack() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Inner {
        value: u32,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Outer {
        inner: Inner,
        name: String,
    }

    let original = Outer {
        inner: Inner { value: 123 },
        name: "nested".to_string(),
    };

    let packed_bytes = pack(&original).unwrap();
    let mut reader = packed_bytes;
    let unpacked: Outer = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_enum_named_variant_pack_unpack() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    enum TestEnumNamed {
        Named { id: u32, name: String },
        AnotherNamed { value: i32, flag: bool },
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    enum TestEnumUnnamed {
        Unnamed(u32, String),
        AnotherUnnamed(i32, bool),
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    enum TestEnumUnit {
        Unit,
        AnotherUnit,
    }

    // Test named variant enum
    let original_named = TestEnumNamed::Named {
        id: 42,
        name: "hello".to_string(),
    };

    let packed_bytes = pack(&original_named).unwrap();
    let mut reader = packed_bytes;
    let unpacked: TestEnumNamed = unpack(&mut reader).unwrap();
    assert_eq!(original_named, unpacked);

    // Test unnamed variant enum
    let original_unnamed = TestEnumUnnamed::Unnamed(42, "hello".to_string());

    let packed_bytes = pack(&original_unnamed).unwrap();
    let mut reader = packed_bytes;
    let unpacked: TestEnumUnnamed = unpack(&mut reader).unwrap();
    assert_eq!(original_unnamed, unpacked);

    // Test unit variant enum
    let original_unit = TestEnumUnit::Unit;

    let packed_bytes = pack(&original_unit).unwrap();
    let mut reader = packed_bytes;
    let unpacked: TestEnumUnit = unpack(&mut reader).unwrap();
    assert_eq!(original_unit, unpacked);
}

#[test]
fn test_enum_pack_vs_encode_size_difference() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    enum TestEnum {
        Named { id: u32, name: String, active: bool },
    }

    let original = TestEnum::Named {
        id: 42,
        name: "hello".to_string(),
        active: true,
    };

    // Pack (no field IDs for enum fields)
    let packed_bytes = pack(&original).unwrap();

    // Encode (with field IDs for enum fields)
    let encoded_bytes = encode(&original).unwrap();

    // Pack should be smaller since it doesn't include field IDs for enum fields
    println!("Enum packed size: {}", packed_bytes.len());
    println!("Enum encoded size: {}", encoded_bytes.len());
    assert!(packed_bytes.len() < encoded_bytes.len());
}

#[test]
fn test_enum_field_order_dependency() {
    // This test demonstrates that pack/unpack is order-dependent for enum fields too
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    enum EnumA {
        Named { first: u32, second: String },
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    enum EnumB {
        Named { second: String, first: u32 }, // Different field order!
    }

    let original_a = EnumA::Named {
        first: 42,
        second: "hello".to_string(),
    };

    // Pack as EnumA
    let packed_bytes = pack(&original_a).unwrap();

    // Try to unpack as EnumB (different field order)
    let mut reader = packed_bytes;
    let result: Result<EnumB, _> = unpack(&mut reader);

    // This should fail or produce incorrect results because field order matters
    if let Ok(unpacked) = result {
        let EnumB::Named { first, .. } = unpacked;
        assert_ne!(first, 42); // Field order mismatch should cause incorrect values
    }
}

#[test]
fn test_primitive_pack_unpack() {
    // Test bool with relaxed validation
    let bool_val = true;
    let packed_bool = pack(&bool_val).unwrap();
    let mut reader = packed_bool;
    let unpacked_bool: bool = unpack(&mut reader).unwrap();
    assert_eq!(bool_val, unpacked_bool);

    // Test bool unpack with non-standard values (should work with any non-zero)
    // Use trait method directly since we're testing raw binary data without magic number
    let mut writer = bytes::BytesMut::new();
    writer.put_u8(42); // Non-standard true value
    let mut reader = writer.freeze();
    let unpacked_bool: bool = bool::unpack(&mut reader).unwrap();
    assert_eq!(true, unpacked_bool);

    // Test f32 without tag
    let f32_val = 3.14159f32;
    let packed_f32 = pack(&f32_val).unwrap();
    let mut reader = packed_f32;
    let unpacked_f32: f32 = unpack(&mut reader).unwrap();
    assert_eq!(f32_val, unpacked_f32);

    // Test f64 without tag
    let f64_val = 2.718281828459045f64;
    let packed_f64 = pack(&f64_val).unwrap();
    let mut reader = packed_f64;
    let unpacked_f64: f64 = unpack(&mut reader).unwrap();
    assert_eq!(f64_val, unpacked_f64);
}

#[test]
#[cfg(feature = "chrono")]
fn test_datetime_pack_unpack() {
    use chrono::{DateTime, Local, Utc};

    // Test DateTime<Utc>
    let utc_dt = DateTime::from_timestamp(1640995200, 123456789).unwrap();
    let packed_utc = pack(&utc_dt).unwrap();
    let mut reader = packed_utc;
    let unpacked_utc: DateTime<Utc> = unpack(&mut reader).unwrap();
    assert_eq!(utc_dt, unpacked_utc);

    // Test DateTime<Local>
    let local_dt = utc_dt.with_timezone(&Local);
    let packed_local = pack(&local_dt).unwrap();
    let mut reader = packed_local;
    let unpacked_local: DateTime<Local> = unpack(&mut reader).unwrap();
    assert_eq!(local_dt, unpacked_local);
}

#[test]
#[cfg(feature = "chrono")]
fn test_datetime_default_and_non_default_pack_unpack() {
    use chrono::{DateTime, Local, Utc};

    // Test default DateTime<Utc> (should use TAG_NONE)
    let default_utc = DateTime::<Utc>::default();
    let packed_default_utc = pack(&default_utc).unwrap();
    let mut reader = packed_default_utc;
    let unpacked_default_utc: DateTime<Utc> = unpack(&mut reader).unwrap();
    assert_eq!(default_utc, unpacked_default_utc);

    // Test non-default DateTime<Utc> (should use TAG_CHRONO_DATETIME)
    let non_default_utc = DateTime::from_timestamp(1640995200, 123456789).unwrap();
    let packed_non_default_utc = pack(&non_default_utc).unwrap();
    let mut reader = packed_non_default_utc;
    let unpacked_non_default_utc: DateTime<Utc> = unpack(&mut reader).unwrap();
    assert_eq!(non_default_utc, unpacked_non_default_utc);

    // Test default DateTime<Local> (should use TAG_NONE)
    let default_local = DateTime::<Local>::default();
    let packed_default_local = pack(&default_local).unwrap();
    let mut reader = packed_default_local;
    let unpacked_default_local: DateTime<Local> = unpack(&mut reader).unwrap();
    assert_eq!(default_local, unpacked_default_local);

    // Test non-default DateTime<Local> (should use TAG_CHRONO_DATETIME)
    let non_default_local = DateTime::from_timestamp(1640995200, 123456789)
        .unwrap()
        .with_timezone(&Local);
    let packed_non_default_local = pack(&non_default_local).unwrap();
    let mut reader = packed_non_default_local;
    let unpacked_non_default_local: DateTime<Local> = unpack(&mut reader).unwrap();
    // Compare UTC timestamps since local timezone might vary
    assert_eq!(
        non_default_local.with_timezone(&Utc),
        unpacked_non_default_local.with_timezone(&Utc)
    );

    // Test various DateTime values
    let test_datetimes = vec![
        DateTime::from_timestamp(0, 0).unwrap(), // Unix epoch
        DateTime::from_timestamp(1234567890, 0).unwrap(), // 2009-02-13
        DateTime::from_timestamp(1640995200, 123456789).unwrap(), // 2022-01-01 with nanos
        DateTime::from_timestamp(-1, 999999999).unwrap(), // Before epoch
        DateTime::from_timestamp(2147483647, 999999999).unwrap(), // Year 2038 problem
    ];

    for &original_utc in &test_datetimes {
        // Test DateTime<Utc>
        let packed_utc = pack(&original_utc).unwrap();
        let mut reader = packed_utc;
        let unpacked_utc: DateTime<Utc> = unpack(&mut reader).unwrap();
        assert_eq!(
            original_utc, unpacked_utc,
            "Failed for UTC DateTime: {}",
            original_utc
        );

        // Test DateTime<Local>
        let original_local = original_utc.with_timezone(&Local);
        let packed_local = pack(&original_local).unwrap();
        let mut reader = packed_local;
        let unpacked_local: DateTime<Local> = unpack(&mut reader).unwrap();
        assert_eq!(
            original_local.with_timezone(&Utc),
            unpacked_local.with_timezone(&Utc),
            "Failed for Local DateTime: {}",
            original_local
        );
    }
}

#[test]
#[cfg(feature = "chrono")]
fn test_naive_datetime_pack_unpack() {
    use chrono::{DateTime, NaiveDateTime};

    // Test NaiveDateTime
    let naive_dt = DateTime::from_timestamp(1640995200, 123456789)
        .unwrap()
        .naive_utc();
    let packed_naive = pack(&naive_dt).unwrap();
    let mut reader = packed_naive;
    let unpacked_naive: NaiveDateTime = unpack(&mut reader).unwrap();
    assert_eq!(naive_dt, unpacked_naive);
}

#[test]
#[cfg(feature = "chrono")]
fn test_naive_datetime_default_and_non_default_pack_unpack() {
    use chrono::{DateTime, NaiveDateTime};

    // Test default NaiveDateTime (should use TAG_NONE)
    let default_naive = NaiveDateTime::default();
    let packed_default_naive = pack(&default_naive).unwrap();
    let mut reader = packed_default_naive;
    let unpacked_default_naive: NaiveDateTime = unpack(&mut reader).unwrap();
    assert_eq!(default_naive, unpacked_default_naive);

    // Test non-default NaiveDateTime (should use TAG_CHRONO_NAIVE_DATETIME)
    let non_default_naive = DateTime::from_timestamp(1640995200, 123456789)
        .unwrap()
        .naive_utc();
    let packed_non_default_naive = pack(&non_default_naive).unwrap();
    let mut reader = packed_non_default_naive;
    let unpacked_non_default_naive: NaiveDateTime = unpack(&mut reader).unwrap();
    assert_eq!(non_default_naive, unpacked_non_default_naive);

    // Test various NaiveDateTime values
    let test_datetimes = vec![
        DateTime::from_timestamp(0, 0).unwrap().naive_utc(), // Unix epoch
        DateTime::from_timestamp(1234567890, 0).unwrap().naive_utc(), // 2009-02-13
        DateTime::from_timestamp(1640995200, 123456789)
            .unwrap()
            .naive_utc(), // 2022-01-01 with nanos
        DateTime::from_timestamp(-1, 999999999).unwrap().naive_utc(), // Before epoch
        DateTime::from_timestamp(2147483647, 999999999)
            .unwrap()
            .naive_utc(), // Year 2038 problem
    ];

    for &original_naive in &test_datetimes {
        let packed_naive = pack(&original_naive).unwrap();
        let mut reader = packed_naive;
        let unpacked_naive: NaiveDateTime = unpack(&mut reader).unwrap();
        assert_eq!(
            original_naive, unpacked_naive,
            "Failed for NaiveDateTime: {}",
            original_naive
        );
    }
}

#[test]
#[cfg(feature = "chrono")]
fn test_naive_datetime_encode_decode() {
    use bytes::BytesMut;
    use chrono::{DateTime, NaiveDateTime};
    use senax_encoder::{Decoder, Encoder};

    let test_datetimes = vec![
        NaiveDateTime::default(),
        DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
        DateTime::from_timestamp(1640995200, 123456789)
            .unwrap()
            .naive_utc(),
        DateTime::from_timestamp(-1, 999999999).unwrap().naive_utc(),
        DateTime::from_timestamp(2147483647, 999999999)
            .unwrap()
            .naive_utc(),
    ];

    for &original in &test_datetimes {
        let mut buffer = BytesMut::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = buffer.freeze();
        let decoded = NaiveDateTime::decode(&mut reader).unwrap();

        assert_eq!(
            original, decoded,
            "Failed encode/decode for NaiveDateTime: {}",
            original
        );
    }
}

#[test]
#[cfg(feature = "chrono")]
fn test_naive_datetime_in_struct() {
    use chrono::{DateTime, NaiveDateTime};
    use senax_encoder::decode;

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct EventStruct {
        id: u32,
        name: String,
        event_time: NaiveDateTime,
        optional_time: Option<NaiveDateTime>,
    }

    let original = EventStruct {
        id: 42,
        name: "Test Event".to_string(),
        event_time: DateTime::from_timestamp(1640995200, 123456789)
            .unwrap()
            .naive_utc(),
        optional_time: Some(DateTime::from_timestamp(1641000000, 0).unwrap().naive_utc()),
    };

    // Test pack/unpack
    let packed_bytes = pack(&original).unwrap();
    let mut reader = packed_bytes;
    let unpacked: EventStruct = unpack(&mut reader).unwrap();
    assert_eq!(original, unpacked);

    // Test encode/decode
    let encoded_bytes = encode(&original).unwrap();
    let mut reader = encoded_bytes;
    let decoded: EventStruct = decode(&mut reader).unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn test_complex_nested_struct_pack_unpack() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Address {
        street: String,
        city: String,
        zip_code: u32,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Person {
        id: u64,
        name: String,
        age: u8,
        email: Option<String>,
        addresses: Vec<Address>,
        scores: HashMap<String, f64>,
        is_active: bool,
    }

    let original = Person {
        id: 12345,
        name: "田中太郎".to_string(),
        age: 30,
        email: Some("tanaka@example.com".to_string()),
        addresses: vec![
            Address {
                street: "1-2-3 Shibuya".to_string(),
                city: "Tokyo".to_string(),
                zip_code: 1500001,
            },
            Address {
                street: "4-5-6 Osaka".to_string(),
                city: "Osaka".to_string(),
                zip_code: 5300001,
            },
        ],
        scores: {
            let mut map = HashMap::new();
            map.insert("math".to_string(), 95.5);
            map.insert("english".to_string(), 87.2);
            map.insert("science".to_string(), 92.8);
            map
        },
        is_active: true,
    };

    // Pack the complex struct
    let packed_bytes = pack(&original).unwrap();

    // Unpack the complex struct
    let mut reader = packed_bytes;
    let unpacked: Person = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_complex_enum_with_nested_data_pack_unpack() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct UserProfile {
        username: String,
        followers: u32,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct ProductInfo {
        name: String,
        price: f64,
        categories: Vec<String>,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    enum ComplexEnum {
        User {
            profile: UserProfile,
            posts: Vec<String>,
            metadata: HashMap<String, String>,
        },
        Product {
            info: ProductInfo,
            reviews: Option<Vec<String>>,
            rating: f32,
        },
        System {
            version: String,
            config: HashMap<String, i32>,
        },
    }

    // Test User variant
    let user_variant = ComplexEnum::User {
        profile: UserProfile {
            username: "user123".to_string(),
            followers: 1500,
        },
        posts: vec!["Hello world!".to_string(), "Second post".to_string()],
        metadata: {
            let mut map = HashMap::new();
            map.insert("theme".to_string(), "dark".to_string());
            map.insert("language".to_string(), "ja".to_string());
            map
        },
    };

    let packed_user = pack(&user_variant).unwrap();
    let mut reader = packed_user;
    let unpacked_user: ComplexEnum = unpack(&mut reader).unwrap();
    assert_eq!(user_variant, unpacked_user);

    // Test Product variant
    let product_variant = ComplexEnum::Product {
        info: ProductInfo {
            name: "Laptop".to_string(),
            price: 999.99,
            categories: vec!["electronics".to_string(), "computers".to_string()],
        },
        reviews: Some(vec!["Great product!".to_string(), "Good value".to_string()]),
        rating: 4.5,
    };

    let packed_product = pack(&product_variant).unwrap();
    let mut reader = packed_product;
    let unpacked_product: ComplexEnum = unpack(&mut reader).unwrap();
    assert_eq!(product_variant, unpacked_product);
}

#[test]
fn test_deeply_nested_struct_pack_unpack() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Level4 {
        value: String,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Level3 {
        level4: Level4,
        numbers: Vec<i32>,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Level2 {
        level3: Level3,
        optional_data: Option<HashMap<String, f64>>,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Level1 {
        level2: Level2,
        flags: Vec<bool>,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct RootLevel {
        level1: Level1,
        metadata: HashMap<String, String>,
        id: u64,
    }

    let original = RootLevel {
        level1: Level1 {
            level2: Level2 {
                level3: Level3 {
                    level4: Level4 {
                        value: "deep nested value".to_string(),
                    },
                    numbers: vec![1, 2, 3, 4, 5],
                },
                optional_data: Some({
                    let mut map = HashMap::new();
                    map.insert("pi".to_string(), 3.14159);
                    map.insert("e".to_string(), 2.71828);
                    map
                }),
            },
            flags: vec![true, false, true, true],
        },
        metadata: {
            let mut map = HashMap::new();
            map.insert("created_by".to_string(), "test".to_string());
            map.insert("version".to_string(), "1.0".to_string());
            map
        },
        id: 999999,
    };

    let packed_bytes = pack(&original).unwrap();
    let mut reader = packed_bytes;
    let unpacked: RootLevel = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_complex_struct_pack_vs_encode_size() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct ComplexStruct {
        id: u64,
        name: String,
        values: Vec<f64>,
        metadata: HashMap<String, String>,
        optional_field: Option<String>,
        nested: Option<Box<ComplexStruct>>,
    }

    let original = ComplexStruct {
        id: 12345,
        name: "complex_test".to_string(),
        values: vec![1.1, 2.2, 3.3, 4.4, 5.5],
        metadata: {
            let mut map = HashMap::new();
            map.insert("key1".to_string(), "value1".to_string());
            map.insert("key2".to_string(), "value2".to_string());
            map.insert("key3".to_string(), "value3".to_string());
            map
        },
        optional_field: Some("optional_value".to_string()),
        nested: Some(Box::new(ComplexStruct {
            id: 67890,
            name: "nested".to_string(),
            values: vec![9.9, 8.8],
            metadata: HashMap::new(),
            optional_field: None,
            nested: None,
        })),
    };

    // Pack (no field IDs)
    let packed_bytes = pack(&original).unwrap();

    // Encode (with field IDs)
    let encoded_bytes = encode(&original).unwrap();

    println!("Complex struct packed size: {}", packed_bytes.len());
    println!("Complex struct encoded size: {}", encoded_bytes.len());

    // Pack should be significantly smaller due to no field IDs
    assert!(packed_bytes.len() < encoded_bytes.len());

    // Verify the data can be unpacked correctly
    let mut reader = packed_bytes;
    let unpacked: ComplexStruct = unpack(&mut reader).unwrap();
    assert_eq!(original, unpacked);
}

#[test]
fn test_array_of_complex_structs_pack_unpack() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Item {
        id: u32,
        name: String,
        tags: Vec<String>,
        properties: HashMap<String, f64>,
    }

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct Container {
        items: Vec<Item>,
        total_count: usize,
        summary: HashMap<String, u32>,
    }

    let original = Container {
        items: vec![
            Item {
                id: 1,
                name: "item1".to_string(),
                tags: vec!["tag1".to_string(), "tag2".to_string()],
                properties: {
                    let mut map = HashMap::new();
                    map.insert("weight".to_string(), 1.5);
                    map.insert("height".to_string(), 10.0);
                    map
                },
            },
            Item {
                id: 2,
                name: "item2".to_string(),
                tags: vec!["tag3".to_string()],
                properties: {
                    let mut map = HashMap::new();
                    map.insert("weight".to_string(), 2.3);
                    map.insert("width".to_string(), 5.0);
                    map
                },
            },
            Item {
                id: 3,
                name: "item3".to_string(),
                tags: vec![],
                properties: HashMap::new(),
            },
        ],
        total_count: 3,
        summary: {
            let mut map = HashMap::new();
            map.insert("processed".to_string(), 3);
            map.insert("errors".to_string(), 0);
            map
        },
    };

    let packed_bytes = pack(&original).unwrap();
    let mut reader = packed_bytes;
    let unpacked: Container = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_vec_u8_pack_unpack() {
    // Test empty Vec<u8>
    let empty_vec: Vec<u8> = Vec::new();
    let packed_empty = pack(&empty_vec).unwrap();
    let mut reader = packed_empty;
    let unpacked_empty: Vec<u8> = unpack(&mut reader).unwrap();
    assert_eq!(empty_vec, unpacked_empty);

    // Test Vec<u8> with various byte values
    let byte_vec = vec![0u8, 1, 127, 128, 255, 42, 100];
    let packed_bytes = pack(&byte_vec).unwrap();
    let mut reader = packed_bytes;
    let unpacked_bytes: Vec<u8> = unpack(&mut reader).unwrap();
    assert_eq!(byte_vec, unpacked_bytes);

    // Test large Vec<u8>
    let large_vec: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
    let packed_large = pack(&large_vec).unwrap();
    let mut reader = packed_large;
    let unpacked_large: Vec<u8> = unpack(&mut reader).unwrap();
    assert_eq!(large_vec, unpacked_large);

    // Test Vec<u8> with binary data
    let binary_data = vec![0xFF, 0x00, 0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56];
    let packed_binary = pack(&binary_data).unwrap();
    let mut reader = packed_binary;
    let unpacked_binary: Vec<u8> = unpack(&mut reader).unwrap();
    assert_eq!(binary_data, unpacked_binary);
}

#[test]
fn test_nested_vec_u8_pack_unpack() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct BinaryContainer {
        id: u32,
        name: String,
        data: Vec<u8>,
        chunks: Vec<Vec<u8>>,
    }

    let original = BinaryContainer {
        id: 123,
        name: "binary_data".to_string(),
        data: vec![0xFF, 0x00, 0xAB, 0xCD, 0xEF],
        chunks: vec![vec![1, 2, 3], vec![], vec![255, 254, 253, 252], vec![42]],
    };

    let packed_bytes = pack(&original).unwrap();
    let mut reader = packed_bytes;
    let unpacked: BinaryContainer = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_vec_u8_with_option_pack_unpack() {
    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct OptionalBinaryData {
        required_data: Vec<u8>,
        optional_data: Option<Vec<u8>>,
        optional_chunks: Option<Vec<Vec<u8>>>,
    }

    // Test with Some values
    let with_optional = OptionalBinaryData {
        required_data: vec![1, 2, 3, 4, 5],
        optional_data: Some(vec![10, 20, 30]),
        optional_chunks: Some(vec![vec![100, 101], vec![200, 201, 202]]),
    };

    let packed_with = pack(&with_optional).unwrap();
    let mut reader = packed_with;
    let unpacked_with: OptionalBinaryData = unpack(&mut reader).unwrap();
    assert_eq!(with_optional, unpacked_with);

    // Test with None values
    let without_optional = OptionalBinaryData {
        required_data: vec![5, 4, 3, 2, 1],
        optional_data: None,
        optional_chunks: None,
    };

    let packed_without = pack(&without_optional).unwrap();
    let mut reader = packed_without;
    let unpacked_without: OptionalBinaryData = unpack(&mut reader).unwrap();
    assert_eq!(without_optional, unpacked_without);
}

#[test]
fn test_bytes_pack_unpack() {
    use bytes::Bytes;

    // Test empty Bytes
    let empty_bytes = Bytes::new();
    let packed_empty = pack(&empty_bytes).unwrap();
    let mut reader = packed_empty;
    let unpacked_empty: Bytes = unpack(&mut reader).unwrap();
    assert_eq!(empty_bytes, unpacked_empty);

    // Test Bytes with various byte values
    let byte_data = Bytes::from_static(&[0u8, 1, 127, 128, 255, 42, 100]);
    let packed_bytes = pack(&byte_data).unwrap();
    let mut reader = packed_bytes;
    let unpacked_bytes: Bytes = unpack(&mut reader).unwrap();
    assert_eq!(byte_data, unpacked_bytes);

    // Test large Bytes
    let large_data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
    let large_bytes = Bytes::from(large_data.clone());
    let packed_large = pack(&large_bytes).unwrap();
    let mut reader = packed_large;
    let unpacked_large: Bytes = unpack(&mut reader).unwrap();
    assert_eq!(large_bytes, unpacked_large);

    // Test Bytes with binary data
    let binary_data = Bytes::from_static(&[0xFF, 0x00, 0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56]);
    let packed_binary = pack(&binary_data).unwrap();
    let mut reader = packed_binary;
    let unpacked_binary: Bytes = unpack(&mut reader).unwrap();
    assert_eq!(binary_data, unpacked_binary);

    // Test Bytes from string
    let string_data = "Hello, World! 🌍";
    let string_bytes = Bytes::from(string_data.as_bytes().to_vec());
    let packed_string = pack(&string_bytes).unwrap();
    let mut reader = packed_string;
    let unpacked_string: Bytes = unpack(&mut reader).unwrap();
    assert_eq!(string_bytes, unpacked_string);
}

#[test]
fn test_nested_bytes_pack_unpack() {
    use bytes::Bytes;

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct BytesContainer {
        id: u32,
        name: String,
        data: Bytes,
        chunks: Vec<Bytes>,
        metadata: Option<Bytes>,
    }

    let original = BytesContainer {
        id: 456,
        name: "bytes_container".to_string(),
        data: Bytes::from_static(&[0xFF, 0x00, 0xAB, 0xCD, 0xEF]),
        chunks: vec![
            Bytes::from_static(&[1, 2, 3]),
            Bytes::new(),
            Bytes::from_static(&[255, 254, 253, 252]),
            Bytes::from_static(&[42]),
        ],
        metadata: Some(Bytes::from("metadata content".as_bytes().to_vec())),
    };

    let packed_bytes = pack(&original).unwrap();
    let mut reader = packed_bytes;
    let unpacked: BytesContainer = unpack(&mut reader).unwrap();

    assert_eq!(original, unpacked);
}

#[test]
fn test_bytes_with_option_pack_unpack() {
    use bytes::Bytes;

    #[derive(Encode, Decode, Pack, Unpack, PartialEq, Debug)]
    struct OptionalBytesData {
        required_data: Bytes,
        optional_data: Option<Bytes>,
        optional_chunks: Option<Vec<Bytes>>,
    }

    // Test with Some values
    let with_optional = OptionalBytesData {
        required_data: Bytes::from_static(&[1, 2, 3, 4, 5]),
        optional_data: Some(Bytes::from_static(&[10, 20, 30])),
        optional_chunks: Some(vec![
            Bytes::from_static(&[100, 101]),
            Bytes::from_static(&[200, 201, 202]),
        ]),
    };

    let packed_with = pack(&with_optional).unwrap();
    let mut reader = packed_with;
    let unpacked_with: OptionalBytesData = unpack(&mut reader).unwrap();
    assert_eq!(with_optional, unpacked_with);

    // Test with None values
    let without_optional = OptionalBytesData {
        required_data: Bytes::from_static(&[5, 4, 3, 2, 1]),
        optional_data: None,
        optional_chunks: None,
    };

    let packed_without = pack(&without_optional).unwrap();
    let mut reader = packed_without;
    let unpacked_without: OptionalBytesData = unpack(&mut reader).unwrap();
    assert_eq!(without_optional, unpacked_without);
}

#[test]
fn test_bytes_vec_u8_pack_behavior() {
    use bytes::Bytes;

    // Test that Bytes and Vec<u8> use different formats
    let data = vec![1u8, 2, 3, 4, 5, 255, 0, 128];
    let vec_data = data.clone();
    let bytes_data = Bytes::from(data);

    // Pack both
    let packed_vec = pack(&vec_data).unwrap();
    let packed_bytes = pack(&bytes_data).unwrap();

    println!("Vec<u8> packed: {:?}", packed_vec.as_ref());
    println!("Bytes packed: {:?}", packed_bytes.as_ref());

    // Vec<u8> and Bytes now use different formats:
    // Vec<u8> uses TAG_ARRAY_VEC_SET format, Bytes uses TAG_BINARY format
    assert_ne!(packed_vec, packed_bytes);

    // Each can be unpacked correctly with its own type
    let mut reader_vec = packed_vec;
    let unpacked_vec: Vec<u8> = unpack(&mut reader_vec).unwrap();
    assert_eq!(vec_data, unpacked_vec);

    let mut reader_bytes = packed_bytes;
    let unpacked_bytes: Bytes = unpack(&mut reader_bytes).unwrap();
    assert_eq!(bytes_data, unpacked_bytes);
}

#[test]
fn test_bytes_string_pack_behavior() {
    use bytes::Bytes;

    // Test that Bytes and String with same UTF-8 content use different tags
    // This demonstrates the difference between the two types in pack format
    let text = "Hello, World! 🌍";
    let string_data = text.to_string();
    let bytes_data = Bytes::from(text.as_bytes().to_vec());

    // Pack both
    let packed_string = pack(&string_data).unwrap();
    let packed_bytes = pack(&bytes_data).unwrap();

    // They use different tags, so packed data will differ
    println!("String packed: {:?}", packed_string.as_ref());
    println!("Bytes packed: {:?}", packed_bytes.as_ref());
    assert_ne!(packed_string, packed_bytes);

    // Each can be unpacked correctly with its own type
    let mut reader_string = packed_string;
    let unpacked_string: String = unpack(&mut reader_string).unwrap();
    assert_eq!(string_data, unpacked_string);

    let mut reader_bytes = packed_bytes;
    let unpacked_bytes: Bytes = unpack(&mut reader_bytes).unwrap();
    assert_eq!(bytes_data, unpacked_bytes);
}

#[test]
fn test_all_primitive_types_pack_unpack() {
    // Test all unsigned integer types
    let u8_val: u8 = 255;
    let packed_u8 = pack(&u8_val).unwrap();
    let mut reader = packed_u8;
    let unpacked_u8: u8 = unpack(&mut reader).unwrap();
    assert_eq!(u8_val, unpacked_u8);

    let u16_val: u16 = 65535;
    let packed_u16 = pack(&u16_val).unwrap();
    let mut reader = packed_u16;
    let unpacked_u16: u16 = unpack(&mut reader).unwrap();
    assert_eq!(u16_val, unpacked_u16);

    let u32_val: u32 = 4294967295;
    let packed_u32 = pack(&u32_val).unwrap();
    let mut reader = packed_u32;
    let unpacked_u32: u32 = unpack(&mut reader).unwrap();
    assert_eq!(u32_val, unpacked_u32);

    let u64_val: u64 = 18446744073709551615;
    let packed_u64 = pack(&u64_val).unwrap();
    let mut reader = packed_u64;
    let unpacked_u64: u64 = unpack(&mut reader).unwrap();
    assert_eq!(u64_val, unpacked_u64);

    let u128_val: u128 = 340282366920938463463374607431768211455;
    let packed_u128 = pack(&u128_val).unwrap();
    let mut reader = packed_u128;
    let unpacked_u128: u128 = unpack(&mut reader).unwrap();
    assert_eq!(u128_val, unpacked_u128);

    let usize_val: usize = usize::MAX;
    let packed_usize = pack(&usize_val).unwrap();
    let mut reader = packed_usize;
    let unpacked_usize: usize = unpack(&mut reader).unwrap();
    assert_eq!(usize_val, unpacked_usize);

    // Test all signed integer types
    let i8_val: i8 = -128;
    let packed_i8 = pack(&i8_val).unwrap();
    let mut reader = packed_i8;
    let unpacked_i8: i8 = unpack(&mut reader).unwrap();
    assert_eq!(i8_val, unpacked_i8);

    let i16_val: i16 = -32768;
    let packed_i16 = pack(&i16_val).unwrap();
    let mut reader = packed_i16;
    let unpacked_i16: i16 = unpack(&mut reader).unwrap();
    assert_eq!(i16_val, unpacked_i16);

    let i32_val: i32 = -2147483648;
    let packed_i32 = pack(&i32_val).unwrap();
    let mut reader = packed_i32;
    let unpacked_i32: i32 = unpack(&mut reader).unwrap();
    assert_eq!(i32_val, unpacked_i32);

    let i64_val: i64 = -9223372036854775808;
    let packed_i64 = pack(&i64_val).unwrap();
    let mut reader = packed_i64;
    let unpacked_i64: i64 = unpack(&mut reader).unwrap();
    assert_eq!(i64_val, unpacked_i64);

    let i128_val: i128 = -170141183460469231731687303715884105728;
    let packed_i128 = pack(&i128_val).unwrap();
    let mut reader = packed_i128;
    let unpacked_i128: i128 = unpack(&mut reader).unwrap();
    assert_eq!(i128_val, unpacked_i128);

    let isize_val: isize = isize::MIN;
    let packed_isize = pack(&isize_val).unwrap();
    let mut reader = packed_isize;
    let unpacked_isize: isize = unpack(&mut reader).unwrap();
    assert_eq!(isize_val, unpacked_isize);

    // Test floating point types
    let f32_val: f32 = 3.14159265;
    let packed_f32 = pack(&f32_val).unwrap();
    let mut reader = packed_f32;
    let unpacked_f32: f32 = unpack(&mut reader).unwrap();
    assert_eq!(f32_val, unpacked_f32);

    let f64_val: f64 = 2.718281828459045;
    let packed_f64 = pack(&f64_val).unwrap();
    let mut reader = packed_f64;
    let unpacked_f64: f64 = unpack(&mut reader).unwrap();
    assert_eq!(f64_val, unpacked_f64);

    // Test boolean type
    let bool_true: bool = true;
    let packed_bool_true = pack(&bool_true).unwrap();
    let mut reader = packed_bool_true;
    let unpacked_bool_true: bool = unpack(&mut reader).unwrap();
    assert_eq!(bool_true, unpacked_bool_true);

    let bool_false: bool = false;
    let packed_bool_false = pack(&bool_false).unwrap();
    let mut reader = packed_bool_false;
    let unpacked_bool_false: bool = unpack(&mut reader).unwrap();
    assert_eq!(bool_false, unpacked_bool_false);

    // Test string type
    let string_val: String = "Hello, 世界! 🌍".to_string();
    let packed_string = pack(&string_val).unwrap();
    let mut reader = packed_string;
    let unpacked_string: String = unpack(&mut reader).unwrap();
    assert_eq!(string_val, unpacked_string);
}

#[test]
fn test_primitive_edge_cases_pack_unpack() {
    // Test zero values
    assert_eq!(0u8, {
        let packed = pack(&0u8).unwrap();
        let mut reader = packed;
        unpack::<u8>(&mut reader).unwrap()
    });

    assert_eq!(0i8, {
        let packed = pack(&0i8).unwrap();
        let mut reader = packed;
        unpack::<i8>(&mut reader).unwrap()
    });

    assert_eq!(0.0f32, {
        let packed = pack(&0.0f32).unwrap();
        let mut reader = packed;
        unpack::<f32>(&mut reader).unwrap()
    });

    assert_eq!(0.0f64, {
        let packed = pack(&0.0f64).unwrap();
        let mut reader = packed;
        unpack::<f64>(&mut reader).unwrap()
    });

    // Test maximum values
    assert_eq!(u8::MAX, {
        let packed = pack(&u8::MAX).unwrap();
        let mut reader = packed;
        unpack::<u8>(&mut reader).unwrap()
    });

    assert_eq!(i8::MAX, {
        let packed = pack(&i8::MAX).unwrap();
        let mut reader = packed;
        unpack::<i8>(&mut reader).unwrap()
    });

    // Test minimum values
    assert_eq!(i8::MIN, {
        let packed = pack(&i8::MIN).unwrap();
        let mut reader = packed;
        unpack::<i8>(&mut reader).unwrap()
    });

    assert_eq!(i16::MIN, {
        let packed = pack(&i16::MIN).unwrap();
        let mut reader = packed;
        unpack::<i16>(&mut reader).unwrap()
    });

    // Test special float values
    assert!(
        f32::NAN.is_nan() && {
            let packed = pack(&f32::NAN).unwrap();
            let mut reader = packed;
            let unpacked: f32 = unpack(&mut reader).unwrap();
            unpacked.is_nan()
        }
    );

    assert_eq!(f32::INFINITY, {
        let packed = pack(&f32::INFINITY).unwrap();
        let mut reader = packed;
        unpack::<f32>(&mut reader).unwrap()
    });

    assert_eq!(f32::NEG_INFINITY, {
        let packed = pack(&f32::NEG_INFINITY).unwrap();
        let mut reader = packed;
        unpack::<f32>(&mut reader).unwrap()
    });

    assert!(
        f64::NAN.is_nan() && {
            let packed = pack(&f64::NAN).unwrap();
            let mut reader = packed;
            let unpacked: f64 = unpack(&mut reader).unwrap();
            unpacked.is_nan()
        }
    );

    assert_eq!(f64::INFINITY, {
        let packed = pack(&f64::INFINITY).unwrap();
        let mut reader = packed;
        unpack::<f64>(&mut reader).unwrap()
    });

    assert_eq!(f64::NEG_INFINITY, {
        let packed = pack(&f64::NEG_INFINITY).unwrap();
        let mut reader = packed;
        unpack::<f64>(&mut reader).unwrap()
    });

    // Test empty string
    assert_eq!("".to_string(), {
        let packed = pack(&"".to_string()).unwrap();
        let mut reader = packed;
        let unpacked: String = unpack(&mut reader).unwrap();
        unpacked
    });
}

#[test]
fn test_primitive_arrays_pack_unpack() {
    // Test arrays of different primitive types
    let u8_array: [u8; 5] = [1, 2, 3, 4, 5];
    let packed_u8_array = pack(&u8_array).unwrap();
    let mut reader = packed_u8_array;
    let unpacked_u8_array: [u8; 5] = unpack(&mut reader).unwrap();
    assert_eq!(u8_array, unpacked_u8_array);

    let i32_array: [i32; 3] = [-1, 0, 1];
    let packed_i32_array = pack(&i32_array).unwrap();
    let mut reader = packed_i32_array;
    let unpacked_i32_array: [i32; 3] = unpack(&mut reader).unwrap();
    assert_eq!(i32_array, unpacked_i32_array);

    let f64_array: [f64; 2] = [3.14159, 2.71828];
    let packed_f64_array = pack(&f64_array).unwrap();
    let mut reader = packed_f64_array;
    let unpacked_f64_array: [f64; 2] = unpack(&mut reader).unwrap();
    assert_eq!(f64_array, unpacked_f64_array);

    let bool_array: [bool; 4] = [true, false, true, false];
    let packed_bool_array = pack(&bool_array).unwrap();
    let mut reader = packed_bool_array;
    let unpacked_bool_array: [bool; 4] = unpack(&mut reader).unwrap();
    assert_eq!(bool_array, unpacked_bool_array);
}

#[test]
fn test_char_encode_decode() {
    let test_chars = vec![
        '\0',         // null character
        'A',          // ASCII letter
        '1',          // ASCII digit
        ' ',          // space
        '!',          // ASCII symbol
        'あ',         // Japanese hiragana
        '漢',         // Japanese kanji
        '🚀',         // emoji
        '€',          // Euro symbol
        'Ω',          // Greek omega
        '\u{1F600}',  // grinning face emoji
        '\u{10FFFF}', // maximum Unicode code point
    ];

    for &original in &test_chars {
        let mut buffer = BytesMut::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = buffer.freeze();
        let decoded = char::decode(&mut reader).unwrap();

        assert_eq!(
            original, decoded,
            "Failed encode/decode for char: {:?}",
            original
        );
    }
}

#[test]
fn test_char_pack_unpack() {
    let test_chars = vec![
        '\0',         // null character
        'A',          // ASCII letter
        '1',          // ASCII digit
        ' ',          // space
        '!',          // ASCII symbol
        'あ',         // Japanese hiragana
        '漢',         // Japanese kanji
        '🚀',         // emoji
        '€',          // Euro symbol
        'Ω',          // Greek omega
        '\u{1F600}',  // grinning face emoji
        '\u{10FFFF}', // maximum Unicode code point
    ];

    for &original in &test_chars {
        let mut buffer = BytesMut::new();
        original.pack(&mut buffer).unwrap();

        let mut reader = buffer.freeze();
        let decoded = char::unpack(&mut reader).unwrap();

        assert_eq!(
            original, decoded,
            "Failed pack/unpack for char: {:?}",
            original
        );
    }
}

#[test]
fn test_char_pack_vs_encode_size() {
    let test_chars = vec![
        'A',          // ASCII
        'あ',         // Japanese
        '🚀',         // emoji
        '\u{10FFFF}', // max Unicode
    ];

    for &ch in &test_chars {
        let mut encode_buffer = BytesMut::new();
        ch.encode(&mut encode_buffer).unwrap();

        let mut pack_buffer = BytesMut::new();
        ch.pack(&mut pack_buffer).unwrap();

        // Pack should be smaller or equal to encode (no tag overhead)
        assert!(
            pack_buffer.len() <= encode_buffer.len(),
            "Pack size should be <= encode size for char: {:?} (pack: {}, encode: {})",
            ch,
            pack_buffer.len(),
            encode_buffer.len()
        );
    }
}

#[test]
fn test_char_is_default() {
    assert!(
        Encoder::is_default(&'\0'),
        "Null character should be default"
    );
    assert!(
        !Encoder::is_default(&'A'),
        "Non-null character should not be default"
    );
    assert!(
        !Encoder::is_default(&'あ'),
        "Unicode character should not be default"
    );
    assert!(!Encoder::is_default(&'🚀'), "Emoji should not be default");
}

#[test]
fn test_char_in_struct() {
    #[derive(Encode, Decode, Pack, Unpack, Debug, PartialEq)]
    struct CharStruct {
        letter: char,
        symbol: Option<char>,
        unicode_char: char,
    }

    let test_struct = CharStruct {
        letter: 'A',
        symbol: Some('€'),
        unicode_char: '漢',
    };

    // Test encode/decode
    let mut buffer = BytesMut::new();
    test_struct.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = CharStruct::decode(&mut reader).unwrap();

    assert_eq!(test_struct, decoded);

    // Test pack/unpack
    let mut pack_buffer = BytesMut::new();
    test_struct.pack(&mut pack_buffer).unwrap();

    let mut pack_reader = pack_buffer.freeze();
    let unpacked = CharStruct::unpack(&mut pack_reader).unwrap();

    assert_eq!(test_struct, unpacked);
}

#[test]
fn test_char_in_collections() {
    // Test Vec<char>
    let char_vec = vec!['H', 'e', 'l', 'l', 'o', '世', '界', '🌍'];

    // Encode/decode
    let mut buffer = BytesMut::new();
    char_vec.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded_vec: Vec<char> = Vec::decode(&mut reader).unwrap();

    assert_eq!(char_vec, decoded_vec);

    // Pack/unpack
    let mut pack_buffer = BytesMut::new();
    char_vec.pack(&mut pack_buffer).unwrap();

    let mut pack_reader = pack_buffer.freeze();
    let unpacked_vec: Vec<char> = Vec::unpack(&mut pack_reader).unwrap();

    assert_eq!(char_vec, unpacked_vec);

    // Test HashMap<char, String>
    let mut char_map = HashMap::new();
    char_map.insert('A', "Alpha".to_string());
    char_map.insert('あ', "Hiragana A".to_string());
    char_map.insert('🚀', "Rocket".to_string());

    // Encode/decode
    let mut buffer2 = BytesMut::new();
    char_map.encode(&mut buffer2).unwrap();

    let mut reader2 = buffer2.freeze();
    let decoded_map: HashMap<char, String> = HashMap::decode(&mut reader2).unwrap();

    assert_eq!(char_map, decoded_map);

    // Pack/unpack
    let mut pack_buffer2 = BytesMut::new();
    char_map.pack(&mut pack_buffer2).unwrap();

    let mut pack_reader2 = pack_buffer2.freeze();
    let unpacked_map: HashMap<char, String> = HashMap::unpack(&mut pack_reader2).unwrap();

    assert_eq!(char_map, unpacked_map);
}

#[test]
fn test_char_unicode_edge_cases() {
    let edge_cases = vec![
        '\u{0}',      // null
        '\u{7F}',     // DEL (last ASCII)
        '\u{80}',     // first non-ASCII
        '\u{FF}',     // last Latin-1
        '\u{100}',    // first beyond Latin-1
        '\u{D7FF}',   // last before surrogate range
        '\u{E000}',   // first after surrogate range
        '\u{FFFF}',   // last in BMP
        '\u{10000}',  // first supplementary
        '\u{10FFFF}', // last valid Unicode
    ];

    for &ch in &edge_cases {
        // Test encode/decode
        let mut encode_buffer = BytesMut::new();
        ch.encode(&mut encode_buffer).unwrap();

        let mut encode_reader = encode_buffer.freeze();
        let decoded = char::decode(&mut encode_reader).unwrap();

        assert_eq!(
            ch, decoded,
            "Failed encode/decode for edge case char: U+{:04X}",
            ch as u32
        );

        // Test pack/unpack
        let mut pack_buffer = BytesMut::new();
        ch.pack(&mut pack_buffer).unwrap();

        let mut pack_reader = pack_buffer.freeze();
        let unpacked = char::unpack(&mut pack_reader).unwrap();

        assert_eq!(
            ch, unpacked,
            "Failed pack/unpack for edge case char: U+{:04X}",
            ch as u32
        );
    }
}

#[test]
fn test_char_cross_compatibility() {
    // Test that char can be decoded from u32 and vice versa
    let test_char = '漢';
    let test_u32 = test_char as u32;

    // Encode char, decode as u32
    let mut char_buffer = BytesMut::new();
    test_char.encode(&mut char_buffer).unwrap();

    let mut char_reader = char_buffer.freeze();
    let decoded_u32 = u32::decode(&mut char_reader).unwrap();

    assert_eq!(test_u32, decoded_u32, "char should decode as u32");

    // Encode u32, decode as char (if valid Unicode)
    let mut u32_buffer = BytesMut::new();
    test_u32.encode(&mut u32_buffer).unwrap();

    let mut u32_reader = u32_buffer.freeze();
    let decoded_char = char::decode(&mut u32_reader).unwrap();

    assert_eq!(test_char, decoded_char, "u32 should decode as char");
}

#[test]
#[cfg(feature = "uuid")]
fn test_uuid_default_and_non_default_pack_unpack() {
    use std::str::FromStr;
    use uuid::Uuid;

    // Test default UUID (should use TAG_NONE)
    let default_uuid = Uuid::default();
    let packed_default = pack(&default_uuid).unwrap();
    let mut reader = packed_default;
    let unpacked_default: Uuid = unpack(&mut reader).unwrap();
    assert_eq!(default_uuid, unpacked_default);

    // Test non-default UUID (should use TAG_UUID)
    let non_default_uuid = Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let packed_non_default = pack(&non_default_uuid).unwrap();
    let mut reader = packed_non_default;
    let unpacked_non_default: Uuid = unpack(&mut reader).unwrap();
    assert_eq!(non_default_uuid, unpacked_non_default);

    // Test nil UUID (same as default)
    let nil_uuid = Uuid::nil();
    let packed_nil = pack(&nil_uuid).unwrap();
    let mut reader = packed_nil;
    let unpacked_nil: Uuid = unpack(&mut reader).unwrap();
    assert_eq!(nil_uuid, unpacked_nil);

    // Test various UUID values
    let test_uuids = vec![
        Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        Uuid::from_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
        Uuid::from_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    ];

    for &original in &test_uuids {
        let packed = pack(&original).unwrap();
        let mut reader = packed;
        let unpacked: Uuid = unpack(&mut reader).unwrap();
        assert_eq!(original, unpacked, "Failed for UUID: {}", original);
    }
}

#[test]
#[cfg(feature = "ulid")]
fn test_ulid_default_and_non_default_pack_unpack() {
    use ulid::Ulid;

    // Test default ULID (should use TAG_NONE)
    let default_ulid = Ulid::default();
    let packed_default = pack(&default_ulid).unwrap();
    let mut reader = packed_default;
    let unpacked_default: Ulid = unpack(&mut reader).unwrap();
    assert_eq!(default_ulid, unpacked_default);

    // Test non-default ULID (should use TAG_UUID)
    let non_default_ulid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let packed_non_default = pack(&non_default_ulid).unwrap();
    let mut reader = packed_non_default;
    let unpacked_non_default: Ulid = unpack(&mut reader).unwrap();
    assert_eq!(non_default_ulid, unpacked_non_default);

    // Test nil ULID (same as default)
    let nil_ulid = Ulid::nil();
    let packed_nil = pack(&nil_ulid).unwrap();
    let mut reader = packed_nil;
    let unpacked_nil: Ulid = unpack(&mut reader).unwrap();
    assert_eq!(nil_ulid, unpacked_nil);

    // Test various ULID values
    let test_ulids = vec![
        Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
        Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVS0").unwrap(),
        Ulid::from_string("7ZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap(),
        Ulid::new(),
        Ulid::new(),
    ];

    for &original in &test_ulids {
        let packed = pack(&original).unwrap();
        let mut reader = packed;
        let unpacked: Ulid = unpack(&mut reader).unwrap();
        assert_eq!(original, unpacked, "Failed for ULID: {}", original);
    }
}
