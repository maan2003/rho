use senax_encoder::{decode, encode};

#[test]
fn test_i128_to_f32_cross_decode() {
    // Positive integer
    let i128_val: i128 = 42;
    let mut bytes = encode(&i128_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, 42.0);

    // Negative integer
    let i128_val: i128 = -123;
    let mut bytes = encode(&i128_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, -123.0);

    // Zero
    let i128_val: i128 = 0;
    let mut bytes = encode(&i128_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, 0.0);

    // Large positive
    let i128_val: i128 = 1000000;
    let mut bytes = encode(&i128_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, 1000000.0);

    // Large negative
    let i128_val: i128 = -999999;
    let mut bytes = encode(&i128_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, -999999.0);
}

#[test]
fn test_i128_to_f64_cross_decode() {
    // Positive integer
    let i128_val: i128 = 42;
    let mut bytes = encode(&i128_val).unwrap();
    let f64_val: f64 = decode(&mut bytes).unwrap();
    assert_eq!(f64_val, 42.0);

    // Negative integer
    let i128_val: i128 = -123;
    let mut bytes = encode(&i128_val).unwrap();
    let f64_val: f64 = decode(&mut bytes).unwrap();
    assert_eq!(f64_val, -123.0);

    // Zero
    let i128_val: i128 = 0;
    let mut bytes = encode(&i128_val).unwrap();
    let f64_val: f64 = decode(&mut bytes).unwrap();
    assert_eq!(f64_val, 0.0);

    // Large positive
    let i128_val: i128 = 123456789012345i128;
    let mut bytes = encode(&i128_val).unwrap();
    let f64_val: f64 = decode(&mut bytes).unwrap();
    assert_eq!(f64_val, 123456789012345.0);

    // Large negative
    let i128_val: i128 = -987654321098765i128;
    let mut bytes = encode(&i128_val).unwrap();
    let f64_val: f64 = decode(&mut bytes).unwrap();
    assert_eq!(f64_val, -987654321098765.0);
}

#[test]
fn test_various_integer_types_to_f32() {
    // u8
    let u8_val: u8 = 100;
    let mut bytes = encode(&u8_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, 100.0);

    // u16
    let u16_val: u16 = 30000;
    let mut bytes = encode(&u16_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, 30000.0);

    // u32
    let u32_val: u32 = 1000000;
    let mut bytes = encode(&u32_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, 1000000.0);

    // i32
    let i32_val: i32 = -50000;
    let mut bytes = encode(&i32_val).unwrap();
    let f32_val: f32 = decode(&mut bytes).unwrap();
    assert_eq!(f32_val, -50000.0);
}

#[test]
fn test_various_integer_types_to_f64() {
    // u64
    let u64_val: u64 = 9876543210;
    let mut bytes = encode(&u64_val).unwrap();
    let f64_val: f64 = decode(&mut bytes).unwrap();
    assert_eq!(f64_val, 9876543210.0);

    // i64
    let i64_val: i64 = -9876543210;
    let mut bytes = encode(&i64_val).unwrap();
    let f64_val: f64 = decode(&mut bytes).unwrap();
    assert_eq!(f64_val, -9876543210.0);
}

#[cfg(feature = "rust_decimal")]
#[test]
fn test_i128_to_decimal_cross_decode() {
    use rust_decimal::Decimal;

    // Positive integer
    let i128_val: i128 = 42;
    let mut bytes = encode(&i128_val).unwrap();
    let decimal_val: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decimal_val, Decimal::from(42));

    // Negative integer
    let i128_val: i128 = -123;
    let mut bytes = encode(&i128_val).unwrap();
    let decimal_val: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decimal_val, Decimal::from(-123));

    // Zero
    let i128_val: i128 = 0;
    let mut bytes = encode(&i128_val).unwrap();
    let decimal_val: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decimal_val, Decimal::ZERO);

    // Large positive
    let i128_val: i128 = 123456789012345i128;
    let mut bytes = encode(&i128_val).unwrap();
    let decimal_val: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decimal_val, Decimal::from(123456789012345i128));

    // Large negative
    let i128_val: i128 = -987654321098765i128;
    let mut bytes = encode(&i128_val).unwrap();
    let decimal_val: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decimal_val, Decimal::from(-987654321098765i128));
}

#[cfg(feature = "rust_decimal")]
#[test]
fn test_various_integer_types_to_decimal() {
    use rust_decimal::Decimal;

    // u32
    let u32_val: u32 = 999999;
    let mut bytes = encode(&u32_val).unwrap();
    let decimal_val: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decimal_val, Decimal::from(999999));

    // i32
    let i32_val: i32 = -888888;
    let mut bytes = encode(&i32_val).unwrap();
    let decimal_val: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decimal_val, Decimal::from(-888888));

    // u64
    let u64_val: u64 = 123456789;
    let mut bytes = encode(&u64_val).unwrap();
    let decimal_val: Decimal = decode(&mut bytes).unwrap();
    assert_eq!(decimal_val, Decimal::from(123456789u64));
}

#[cfg(feature = "bigdecimal")]
#[test]
fn test_i128_to_bigdecimal_cross_decode() {
    use bigdecimal::BigDecimal;

    // Positive integer
    let i128_val: i128 = 42;
    let mut bytes = encode(&i128_val).unwrap();
    let bigdec_val: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(bigdec_val, BigDecimal::from(42));

    // Negative integer
    let i128_val: i128 = -123;
    let mut bytes = encode(&i128_val).unwrap();
    let bigdec_val: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(bigdec_val, BigDecimal::from(-123));

    // Zero
    let i128_val: i128 = 0;
    let mut bytes = encode(&i128_val).unwrap();
    let bigdec_val: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(bigdec_val, BigDecimal::from(0));

    // Large positive
    let i128_val: i128 = 123456789012345678901234567890i128;
    let mut bytes = encode(&i128_val).unwrap();
    let bigdec_val: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(
        bigdec_val,
        BigDecimal::from(123456789012345678901234567890i128)
    );

    // Large negative
    let i128_val: i128 = -987654321098765432109876543210i128;
    let mut bytes = encode(&i128_val).unwrap();
    let bigdec_val: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(
        bigdec_val,
        BigDecimal::from(-987654321098765432109876543210i128)
    );
}

#[cfg(feature = "bigdecimal")]
#[test]
fn test_various_integer_types_to_bigdecimal() {
    use bigdecimal::BigDecimal;

    // u8
    let u8_val: u8 = 255;
    let mut bytes = encode(&u8_val).unwrap();
    let bigdec_val: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(bigdec_val, BigDecimal::from(255));

    // i16
    let i16_val: i16 = -30000;
    let mut bytes = encode(&i16_val).unwrap();
    let bigdec_val: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(bigdec_val, BigDecimal::from(-30000));

    // u128
    let u128_val: u128 = 999999999999999999u128;
    let mut bytes = encode(&u128_val).unwrap();
    let bigdec_val: BigDecimal = decode(&mut bytes).unwrap();
    assert_eq!(bigdec_val, BigDecimal::from(999999999999999999u128));
}

#[test]
fn test_small_integers_to_float() {
    // Test compact encoded integers (TAG_ZERO..TAG_U8_127)
    for i in 0..128 {
        let mut bytes = encode(&i).unwrap();
        let f32_val: f32 = decode(&mut bytes).unwrap();
        assert_eq!(f32_val, i as f32);

        let mut bytes = encode(&i).unwrap();
        let f64_val: f64 = decode(&mut bytes).unwrap();
        assert_eq!(f64_val, i as f64);
    }
}

#[test]
fn test_negative_small_integers_to_float() {
    // Test negative integers
    for i in -100..0 {
        let mut bytes = encode(&i).unwrap();
        let f32_val: f32 = decode(&mut bytes).unwrap();
        assert_eq!(f32_val, i as f32);

        let mut bytes = encode(&i).unwrap();
        let f64_val: f64 = decode(&mut bytes).unwrap();
        assert_eq!(f64_val, i as f64);
    }
}
