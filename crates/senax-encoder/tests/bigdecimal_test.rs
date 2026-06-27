#![cfg(feature = "bigdecimal")]

use bigdecimal::BigDecimal;
use senax_encoder::{decode, encode, pack, unpack, Encoder};
use std::str::FromStr;

#[test]
fn test_bigdecimal_encode_decode() {
    // Test with regular decimal
    let value = BigDecimal::from_str("123.456").unwrap();
    let mut bytes = encode(&value).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(value, decoded);

    // Test with zero
    let zero = BigDecimal::from(0);
    let mut bytes = encode(&zero).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(zero, decoded);

    // Test with negative
    let negative = BigDecimal::from_str("-987.654321").unwrap();
    let mut bytes = encode(&negative).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(negative, decoded);
}

#[test]
fn test_bigdecimal_pack_unpack() {
    let value = BigDecimal::from_str("123.456").unwrap();
    let mut bytes = pack(&value).unwrap();
    let unpacked: BigDecimal = unpack(&mut bytes).unwrap();
    assert_eq!(value, unpacked);
}

#[test]
fn test_bigdecimal_large_numbers() {
    // Test with very large number
    let large = BigDecimal::from_str("12345678901234567890.123456789").unwrap();
    let mut bytes = encode(&large).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(large, decoded);

    // Test with very small number
    let small = BigDecimal::from_str("0.000000000000000001").unwrap();
    let mut bytes = encode(&small).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(small, decoded);
}

#[test]
fn test_bigdecimal_scientific_notation() {
    // Test with scientific notation
    let sci = BigDecimal::from_str("1.23e10").unwrap();
    let mut bytes = encode(&sci).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(sci, decoded);

    let sci_negative = BigDecimal::from_str("1.23e-10").unwrap();
    let mut bytes = encode(&sci_negative).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(sci_negative, decoded);
}

#[test]
fn test_bigdecimal_is_default() {
    let zero = BigDecimal::from(0);
    assert!(zero.is_default());

    let non_zero = BigDecimal::from_str("0.001").unwrap();
    assert!(!non_zero.is_default());
}

#[test]
fn test_bigdecimal_in_struct() {
    use senax_encoder_derive::{Decode, Encode, Pack, Unpack};

    #[derive(Encode, Decode, Pack, Unpack, Debug, PartialEq)]
    struct PriceInfo {
        #[senax(id = 1)]
        amount: BigDecimal,
        #[senax(id = 2)]
        currency: String,
    }

    let info = PriceInfo {
        amount: BigDecimal::from_str("99.99").unwrap(),
        currency: "USD".to_string(),
    };

    // Test encode/decode
    let mut bytes = encode(&info).unwrap();
    let decoded: PriceInfo = decode(&mut bytes).unwrap();
    assert_eq!(info, decoded);

    // Test pack/unpack
    let mut packed = pack(&info).unwrap();
    let unpacked: PriceInfo = unpack(&mut packed).unwrap();
    assert_eq!(info, unpacked);
}

#[test]
fn test_bigdecimal_in_option() {
    use senax_encoder_derive::{Decode, Encode};

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct OptionalPrice {
        #[senax(id = 1)]
        price: Option<BigDecimal>,
    }

    // Test with Some
    let with_price = OptionalPrice {
        price: Some(BigDecimal::from_str("19.99").unwrap()),
    };
    let mut bytes = encode(&with_price).unwrap();
    let decoded: OptionalPrice = decode(&mut bytes).unwrap();
    assert_eq!(with_price, decoded);

    // Test with None
    let without_price = OptionalPrice { price: None };
    let mut bytes = encode(&without_price).unwrap();
    let decoded: OptionalPrice = decode(&mut bytes).unwrap();
    assert_eq!(without_price, decoded);
}

#[test]
fn test_bigdecimal_in_vec() {
    let values = vec![
        BigDecimal::from_str("0").unwrap(),
        BigDecimal::from_str("2.2").unwrap(),
        BigDecimal::from_str("3.3").unwrap(),
    ];

    let mut bytes = encode(&values).unwrap();
    let decoded: Vec<BigDecimal> = decode(&mut bytes).unwrap();
    assert_eq!(values, decoded);
}

#[test]
fn test_bigdecimal_precision() {
    // Test that precision is preserved
    let precise = BigDecimal::from_str("1.23456789012345678901234567890").unwrap();
    let mut bytes = encode(&precise).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(precise.to_string(), decoded.to_string());
}

#[test]
fn test_bigdecimal_special_values() {
    // Test with integer
    let integer = BigDecimal::from(12345);
    let mut bytes = encode(&integer).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(integer, decoded);

    // Test with trailing zeros
    let trailing = BigDecimal::from_str("100.00").unwrap();
    let mut bytes = encode(&trailing).unwrap();
    let decoded: BigDecimal = decode(&mut bytes).unwrap();
    // Note: BigDecimal may normalize trailing zeros
    assert_eq!(trailing, decoded);
}
