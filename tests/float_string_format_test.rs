use bytes::{BufMut, BytesMut};
use senax_encoder::{decode, encode};

#[test]
fn test_f32_scientific_notation_format() {
    // Test that f32 is encoded as scientific notation string
    let value: f32 = 3.14159;
    let mut bytes = encode(&value).unwrap();
    let decoded: f32 = decode(&mut bytes).unwrap();
    assert!((decoded - value).abs() < 1e-6);

    // Test zero
    let zero: f32 = 0.0;
    let mut bytes = encode(&zero).unwrap();
    let decoded: f32 = decode(&mut bytes).unwrap();
    assert_eq!(decoded, zero);

    // Test negative
    let negative: f32 = -123.456;
    let mut bytes = encode(&negative).unwrap();
    let decoded: f32 = decode(&mut bytes).unwrap();
    assert!((decoded - negative).abs() < 1e-3);
}

#[test]
fn test_f64_scientific_notation_format() {
    // Test that f64 is encoded as scientific notation string
    let value: f64 = 3.141592653589793;
    let mut bytes = encode(&value).unwrap();
    let decoded: f64 = decode(&mut bytes).unwrap();
    assert!((decoded - value).abs() < 1e-15);

    // Test zero
    let zero: f64 = 0.0;
    let mut bytes = encode(&zero).unwrap();
    let decoded: f64 = decode(&mut bytes).unwrap();
    assert_eq!(decoded, zero);

    // Test negative
    let negative: f64 = -123.456789012345;
    let mut bytes = encode(&negative).unwrap();
    let decoded: f64 = decode(&mut bytes).unwrap();
    assert!((decoded - negative).abs() < 1e-12);
}

#[test]
fn test_f32_backward_compatibility() {
    // Test that legacy binary format can still be decoded
    let value: f32 = 42.5;

    // Create legacy binary format manually (TAG_F32 + 4 bytes)
    let mut writer = BytesMut::new();
    writer.put_u16_le(0xA55A); // MAGIC_ENCODE (little-endian)
    writer.put_u8(137); // TAG_F32
    writer.put_f32_le(value);

    let mut bytes = writer.freeze();
    let decoded: f32 = decode(&mut bytes).unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn test_f64_backward_compatibility() {
    // Test that legacy binary format can still be decoded
    let value: f64 = 123.456;

    // Create legacy binary format manually (TAG_F64 + 8 bytes)
    let mut writer = BytesMut::new();
    writer.put_u16_le(0xA55A); // MAGIC_ENCODE (little-endian)
    writer.put_u8(138); // TAG_F64
    writer.put_f64_le(value);

    let mut bytes = writer.freeze();
    let decoded: f64 = decode(&mut bytes).unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn test_f32_f64_cross_decode_string_format() {
    // Test cross-decoding with new string format
    let v32: f32 = 3.14159;
    let mut bytes = encode(&v32).unwrap();
    let v64: f64 = decode(&mut bytes).unwrap();
    assert!((v64 - v32 as f64).abs() < 1e-6);

    let v64_orig: f64 = 2.71828;
    let mut bytes = encode(&v64_orig).unwrap();
    let v32_decoded: f32 = decode(&mut bytes).unwrap();
    assert!((v32_decoded as f64 - v64_orig).abs() < 1e-5);
}

#[test]
fn test_f32_extreme_values() {
    let values = vec![
        f32::MIN,
        f32::MAX,
        f32::MIN_POSITIVE,
        1e-10,
        1e10,
        -1e-10,
        -1e10,
    ];

    for &value in &values {
        let mut bytes = encode(&value).unwrap();
        let decoded: f32 = decode(&mut bytes).unwrap();
        // Use relative error for extreme values
        let relative_error = if value != 0.0 {
            ((decoded - value) / value).abs()
        } else {
            (decoded - value).abs()
        };
        assert!(
            relative_error < 1e-6,
            "Failed for value {}: decoded = {}, error = {}",
            value,
            decoded,
            relative_error
        );
    }
}

#[test]
fn test_f64_extreme_values() {
    let values = vec![
        f64::MIN,
        f64::MAX,
        f64::MIN_POSITIVE,
        1e-100,
        1e100,
        -1e-100,
        -1e100,
    ];

    for &value in &values {
        let mut bytes = encode(&value).unwrap();
        let decoded: f64 = decode(&mut bytes).unwrap();
        // Use relative error for extreme values
        let relative_error = if value != 0.0 {
            ((decoded - value) / value).abs()
        } else {
            (decoded - value).abs()
        };
        assert!(
            relative_error < 1e-15,
            "Failed for value {}: decoded = {}, error = {}",
            value,
            decoded,
            relative_error
        );
    }
}

#[cfg(feature = "rust_decimal")]
#[test]
fn test_decimal_scientific_notation_format() {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    // Test that Decimal is encoded as scientific notation string
    let value = Decimal::from_str("123.456").unwrap();
    let mut bytes = encode(&value).unwrap();
    let decoded: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decoded, value);

    // Test zero
    let zero = Decimal::ZERO;
    let mut bytes = encode(&zero).unwrap();
    let decoded: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decoded, zero);

    // Test negative
    let negative = Decimal::from_str("-987.654321").unwrap();
    let mut bytes = encode(&negative).unwrap();
    let decoded: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decoded, negative);
}

#[cfg(feature = "rust_decimal")]
#[test]
fn test_decimal_backward_compatibility() {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let value = Decimal::from_str("123.456").unwrap();

    // Create legacy binary format manually (TAG_DECIMAL + mantissa + scale)
    let mut writer = BytesMut::new();
    writer.put_u16_le(0xA55A); // MAGIC_ENCODE (little-endian)
    writer.put_u8(200); // TAG_DECIMAL

    // Encode mantissa (i128) and scale (u32)
    let mantissa = value.mantissa();
    let scale = value.scale();

    // Encode i128 (mantissa)
    writer.put_u8(135); // TAG_U128
    writer.put_u128_le(mantissa as u128);

    // Encode u32 (scale)
    writer.put_u8(133); // TAG_U32
    writer.put_u32_le(scale);

    let mut bytes = writer.freeze();
    let decoded: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decoded, value);
}
