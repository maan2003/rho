use senax_encoder::{Decoder, Encoder, Packer, Unpacker};
use senax_encoder_derive::{Decode, Encode, Pack, Unpack};

#[derive(Encode, Decode, Pack, Unpack)]
struct NormalStruct {
    #[senax(id = 1)]
    field1: i32,
}

#[derive(Encode, Decode, Pack, Unpack)]
#[senax(disable_encode)]
struct DisabledEncodeStruct {
    #[senax(id = 1)]
    field1: i32,
}

#[derive(Encode, Decode, Pack, Unpack)]
#[senax(disable_pack)]
struct DisabledPackStruct {
    #[senax(id = 1)]
    field1: i32,
}

#[derive(Encode, Decode, Pack, Unpack)]
#[senax(disable_encode, disable_pack)]
#[allow(unused)]
struct DisabledBothStruct {
    #[senax(id = 1)]
    field1: i32,
}

#[derive(Encode, Decode, Pack, Unpack)]
enum NormalEnum {
    #[senax(id = 1)]
    Variant1,
    #[senax(id = 2)]
    Variant2(i32),
}

#[derive(Encode, Decode, Pack, Unpack)]
#[senax(disable_encode)]
enum DisabledEncodeEnum {
    #[senax(id = 1)]
    Variant1,
    #[senax(id = 2)]
    Variant2(i32),
}

#[derive(Encode, Decode, Pack, Unpack)]
#[senax(disable_pack)]
enum DisabledPackEnum {
    #[senax(id = 1)]
    Variant1,
    #[senax(id = 2)]
    Variant2(i32),
}

#[test]
fn test_normal_struct_encode() {
    let s = NormalStruct { field1: 42 };
    let mut writer = bytes::BytesMut::new();
    s.encode(&mut writer).unwrap();
    assert!(!writer.is_empty());
}

#[test]
fn test_normal_struct_pack() {
    let s = NormalStruct { field1: 42 };
    let mut writer = bytes::BytesMut::new();
    s.pack(&mut writer).unwrap();
    assert!(!writer.is_empty());
}

// disable_encode tests
#[test]
#[should_panic(expected = "Encode trait is disabled")]
fn test_disabled_encode_struct_encode() {
    let s = DisabledEncodeStruct { field1: 42 };
    let mut writer = bytes::BytesMut::new();
    s.encode(&mut writer).unwrap();
}

#[test]
#[should_panic(expected = "Encode trait is disabled")]
fn test_disabled_encode_struct_is_default() {
    let s = DisabledEncodeStruct { field1: 42 };
    let _ = s.is_default();
}

#[test]
#[should_panic(expected = "Decode trait is disabled")]
fn test_disabled_encode_struct_decode() {
    let mut reader = bytes::Bytes::new();
    let _ = DisabledEncodeStruct::decode(&mut reader).unwrap();
}

#[test]
fn test_disabled_encode_struct_pack_works() {
    // Pack/Unpackは動作する
    let s = DisabledEncodeStruct { field1: 42 };
    let mut writer = bytes::BytesMut::new();
    s.pack(&mut writer).unwrap();
    assert!(!writer.is_empty());
}

// disable_pack tests
#[test]
#[should_panic(expected = "Pack trait is disabled")]
fn test_disabled_pack_struct_pack() {
    let s = DisabledPackStruct { field1: 42 };
    let mut writer = bytes::BytesMut::new();
    s.pack(&mut writer).unwrap();
}

#[test]
#[should_panic(expected = "Unpack trait is disabled")]
fn test_disabled_pack_struct_unpack() {
    let mut reader = bytes::Bytes::new();
    let _ = DisabledPackStruct::unpack(&mut reader).unwrap();
}

#[test]
fn test_disabled_pack_struct_encode_works() {
    let s = DisabledPackStruct { field1: 42 };
    let mut writer = bytes::BytesMut::new();
    s.encode(&mut writer).unwrap();
    assert!(!writer.is_empty());
}

// disable_both tests
#[test]
#[should_panic(expected = "Encode trait is disabled")]
fn test_disabled_both_struct_encode() {
    let s = DisabledBothStruct { field1: 42 };
    let mut writer = bytes::BytesMut::new();
    s.encode(&mut writer).unwrap();
}

#[test]
#[should_panic(expected = "Pack trait is disabled")]
fn test_disabled_both_struct_pack() {
    let s = DisabledBothStruct { field1: 42 };
    let mut writer = bytes::BytesMut::new();
    s.pack(&mut writer).unwrap();
}

// Enum tests
#[test]
fn test_normal_enum_encode() {
    let e = NormalEnum::Variant1;
    let mut writer = bytes::BytesMut::new();
    e.encode(&mut writer).unwrap();
    assert!(!writer.is_empty());
}

#[test]
#[should_panic(expected = "Encode trait is disabled")]
fn test_disabled_encode_enum_encode() {
    let e = DisabledEncodeEnum::Variant1;
    let mut writer = bytes::BytesMut::new();
    e.encode(&mut writer).unwrap();
}

#[test]
#[should_panic(expected = "Decode trait is disabled")]
fn test_disabled_encode_enum_decode() {
    let mut reader = bytes::Bytes::new();
    let _ = DisabledEncodeEnum::decode(&mut reader).unwrap();
}

#[test]
fn test_disabled_encode_enum_pack_works() {
    let e = DisabledEncodeEnum::Variant2(42);
    let mut writer = bytes::BytesMut::new();
    e.pack(&mut writer).unwrap();
    assert!(!writer.is_empty());
}

#[test]
#[should_panic(expected = "Pack trait is disabled")]
fn test_disabled_pack_enum_pack() {
    let e = DisabledPackEnum::Variant2(42);
    let mut writer = bytes::BytesMut::new();
    e.pack(&mut writer).unwrap();
}

#[test]
#[should_panic(expected = "Unpack trait is disabled")]
fn test_disabled_pack_enum_unpack() {
    let mut reader = bytes::Bytes::new();
    let _ = DisabledPackEnum::unpack(&mut reader).unwrap();
}

#[test]
fn test_disabled_pack_enum_encode_works() {
    let e = DisabledPackEnum::Variant1;
    let mut writer = bytes::BytesMut::new();
    e.encode(&mut writer).unwrap();
    assert!(!writer.is_empty());
}
