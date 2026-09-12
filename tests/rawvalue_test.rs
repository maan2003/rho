#![cfg(feature = "raw_value")]

use bytes::BytesMut;
use senax_encoder::*;
use serde_json::value::RawValue;

#[test]
fn test_raw_value_encode_decode() {
    // Test simple JSON string
    let raw_json = r#"{"name":"Alice","age":30}"#;
    let original = RawValue::from_string(raw_json.to_string()).unwrap();

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Box::<RawValue>::decode(&mut reader).unwrap();

    assert_eq!(original.get(), decoded.get());
}

#[test]
fn test_raw_value_with_array() {
    let raw_json = r#"[1,2,3,"four",null,{"nested":true}]"#;
    let original = RawValue::from_string(raw_json.to_string()).unwrap();

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Box::<RawValue>::decode(&mut reader).unwrap();

    assert_eq!(original.get(), decoded.get());
}

#[test]
fn test_raw_value_in_struct() {
    #[derive(Encode, Decode, Debug)]
    struct Config {
        name: String,
        data: Box<RawValue>,
        version: u32,
    }

    let raw_json = r#"{"key":"value","nested":{"x":1,"y":2}}"#;
    let original = Config {
        name: "test_config".to_string(),
        data: RawValue::from_string(raw_json.to_string()).unwrap(),
        version: 1,
    };

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Config::decode(&mut reader).unwrap();

    assert_eq!(original.name, decoded.name);
    assert_eq!(original.data.get(), decoded.data.get());
    assert_eq!(original.version, decoded.version);
}

#[test]
fn test_raw_value_empty_json() {
    // Test empty object and array
    let test_cases = vec!["{}", "[]", r#""""#, "null", "true", "42"];

    for json_str in test_cases {
        let original = RawValue::from_string(json_str.to_string()).unwrap();

        let mut buffer = BytesMut::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = buffer.freeze();
        let decoded = Box::<RawValue>::decode(&mut reader).unwrap();

        assert_eq!(original.get(), decoded.get(), "Failed for: {}", json_str);
    }
}

#[test]
fn test_raw_value_long_json() {
    // Test with a long JSON string to trigger TAG_STRING_LONG
    let mut json_obj = String::from("{");
    for i in 0..100 {
        if i > 0 {
            json_obj.push(',');
        }
        json_obj.push_str(&format!(r#""field{}":"value{}""#, i, i));
    }
    json_obj.push('}');

    let original = RawValue::from_string(json_obj).unwrap();

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Box::<RawValue>::decode(&mut reader).unwrap();

    assert_eq!(original.get(), decoded.get());
}

#[test]
fn test_raw_value_is_default() {
    // RawValue always contains valid JSON, so an empty JSON object is the smallest valid JSON
    // We don't test for empty string as it's not valid JSON

    // Small JSON should not be default (since is_default checks if the string is empty)
    let non_empty_raw = RawValue::from_string("{}".to_string()).unwrap();
    assert!(!non_empty_raw.is_default());

    let non_empty_raw2 = RawValue::from_string("null".to_string()).unwrap();
    assert!(!non_empty_raw2.is_default());
}

#[test]
fn test_raw_value_pack_unpack() {
    let raw_json = r#"{"test":"data","num":123}"#;
    let original = RawValue::from_string(raw_json.to_string()).unwrap();

    let mut buffer = BytesMut::new();
    original.pack(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Box::<RawValue>::unpack(&mut reader).unwrap();

    assert_eq!(original.get(), decoded.get());
}

#[test]
fn test_raw_value_with_option() {
    #[derive(Encode, Decode, Debug)]
    struct OptionalConfig {
        id: u64,
        metadata: Option<Box<RawValue>>,
    }

    // Test with Some
    let raw_json = r#"{"meta":"data"}"#;
    let original_some = OptionalConfig {
        id: 42,
        metadata: Some(RawValue::from_string(raw_json.to_string()).unwrap()),
    };

    let mut buffer = BytesMut::new();
    original_some.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded_some = OptionalConfig::decode(&mut reader).unwrap();

    assert_eq!(original_some.id, decoded_some.id);
    assert_eq!(
        original_some.metadata.unwrap().get(),
        decoded_some.metadata.unwrap().get()
    );

    // Test with None
    let original_none = OptionalConfig {
        id: 99,
        metadata: None,
    };

    let mut buffer = BytesMut::new();
    original_none.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded_none = OptionalConfig::decode(&mut reader).unwrap();

    assert_eq!(original_none.id, decoded_none.id);
    assert!(decoded_none.metadata.is_none());
}

#[test]
fn test_raw_value_in_vec() {
    let json_items = vec![r#"{"id":1}"#, r#"{"id":2}"#, r#"{"id":3}"#];

    let original: Vec<Box<RawValue>> = json_items
        .iter()
        .map(|s| RawValue::from_string(s.to_string()).unwrap())
        .collect();

    let mut buffer = BytesMut::new();
    original.encode(&mut buffer).unwrap();

    let mut reader = buffer.freeze();
    let decoded = Vec::<Box<RawValue>>::decode(&mut reader).unwrap();

    assert_eq!(original.len(), decoded.len());
    for (orig, dec) in original.iter().zip(decoded.iter()) {
        assert_eq!(orig.get(), dec.get());
    }
}
