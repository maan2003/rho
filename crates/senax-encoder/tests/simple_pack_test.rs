use senax_encoder::{pack, unpack, Pack, Unpack};

#[derive(Pack, Unpack, PartialEq, Debug)]
struct SimpleStruct {
    id: u32,
    name: String,
    active: bool,
}

#[derive(Pack, Unpack, PartialEq, Debug)]
struct TupleStruct(u32, String, bool);

#[derive(Pack, Unpack, PartialEq, Debug)]
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
fn test_primitive_pack_unpack() {
    // Test bool
    let bool_val = true;
    let packed_bool = pack(&bool_val).unwrap();
    let mut reader = packed_bool;
    let unpacked_bool: bool = unpack(&mut reader).unwrap();
    assert_eq!(bool_val, unpacked_bool);

    // Test u32
    let u32_val = 42u32;
    let packed_u32 = pack(&u32_val).unwrap();
    let mut reader = packed_u32;
    let unpacked_u32: u32 = unpack(&mut reader).unwrap();
    assert_eq!(u32_val, unpacked_u32);

    // Test String
    let string_val = "hello".to_string();
    let packed_string = pack(&string_val).unwrap();
    let mut reader = packed_string;
    let unpacked_string: String = unpack(&mut reader).unwrap();
    assert_eq!(string_val, unpacked_string);
}
