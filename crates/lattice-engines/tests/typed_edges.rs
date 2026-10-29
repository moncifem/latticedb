//! Edges of the typed row codec found by mutation testing: the nesting limit in both directions,
//! every value tag of the overflow area, bitmap padding, missing columns and repeated overflow names.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_engines::typed::{ColType, Column, TableLayout, TypedError, decode_row, encode_row};
use lattice_lsl::validate::Value;
use std::collections::BTreeMap;

const MAX_DEPTH: usize = 16;

fn layout(columns: Vec<(&str, ColType, bool)>) -> TableLayout {
    TableLayout {
        type_name: "T".into(),
        columns: columns
            .into_iter()
            .map(|(name, ty, nullable)| Column {
                name: name.into(),
                ty,
                nullable,
            })
            .collect(),
    }
}

fn object(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<BTreeMap<_, _>>(),
    )
}

/// `levels` lists around one integer; the integer sits `levels` deep.
fn nested_list(levels: usize) -> Value {
    (0..levels).fold(Value::Int(7), |inner, _| Value::List(vec![inner]))
}

fn nested_type(levels: usize) -> ColType {
    (0..levels).fold(ColType::Int, |inner, _| ColType::List(Box::new(inner)))
}

/// A row with no columns and one overflow field `x` whose raw tagged value is `tagged`.
fn empty_layout_row_with_extra(tagged: &[u8]) -> Vec<u8> {
    let mut bytes = vec![1u8]; // version
    bytes.extend_from_slice(&1u32.to_le_bytes()); // one overflow field
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.push(b'x');
    bytes.extend_from_slice(tagged);
    bytes
}

#[test]
fn typed_list_columns_nest_to_the_limit_and_no_further() {
    for levels in [MAX_DEPTH - 1, MAX_DEPTH] {
        let l = layout(vec![("c", nested_type(levels), false)]);
        let row = object(vec![("c", nested_list(levels))]);
        let bytes = encode_row(&l, &row).unwrap();
        assert_eq!(decode_row(&l, &bytes).unwrap(), row, "{levels} levels");
    }
    let levels = MAX_DEPTH + 1;
    let l = layout(vec![("c", nested_type(levels), false)]);
    let row = object(vec![("c", nested_list(levels))]);
    assert_eq!(encode_row(&l, &row), Err(TypedError::TooDeep));
    // The same row written by hand: version, presence bitmap, then one count of 1 per level and the integer.
    let mut bytes = vec![1u8, 1u8];
    for _ in 0..levels {
        bytes.extend_from_slice(&1u32.to_le_bytes());
    }
    bytes.extend_from_slice(&7i64.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(decode_row(&l, &bytes), Err(TypedError::TooDeep));
}

#[test]
fn overflow_values_nest_to_the_limit_and_no_further() {
    let l = layout(vec![]);
    for levels in [MAX_DEPTH - 1, MAX_DEPTH] {
        let row = object(vec![("x", nested_list(levels))]);
        let bytes = encode_row(&l, &row).unwrap();
        assert_eq!(decode_row(&l, &bytes).unwrap(), row, "{levels} levels");
    }
    let levels = MAX_DEPTH + 1;
    assert_eq!(
        encode_row(&l, &object(vec![("x", nested_list(levels))])),
        Err(TypedError::TooDeep)
    );
    let mut tagged = Vec::new();
    for _ in 0..levels {
        tagged.push(4u8);
        tagged.extend_from_slice(&1u32.to_le_bytes());
    }
    tagged.push(1);
    tagged.extend_from_slice(&7i64.to_le_bytes());
    assert_eq!(
        decode_row(&l, &empty_layout_row_with_extra(&tagged)),
        Err(TypedError::TooDeep)
    );
}

#[test]
fn overflow_objects_nest_to_the_limit_and_no_further() {
    let l = layout(vec![]);
    let nest =
        |levels: usize| (0..levels).fold(Value::Int(7), |inner, _| object(vec![("k", inner)]));
    for levels in [MAX_DEPTH - 1, MAX_DEPTH] {
        let row = object(vec![("x", nest(levels))]);
        let bytes = encode_row(&l, &row).unwrap();
        assert_eq!(decode_row(&l, &bytes).unwrap(), row, "{levels} levels");
    }
    let levels = MAX_DEPTH + 1;
    assert_eq!(
        encode_row(&l, &object(vec![("x", nest(levels))])),
        Err(TypedError::TooDeep)
    );
    let mut tagged = Vec::new();
    for _ in 0..levels {
        tagged.push(7u8);
        tagged.extend_from_slice(&1u32.to_le_bytes());
        tagged.extend_from_slice(&1u16.to_le_bytes());
        tagged.push(b'k');
    }
    tagged.push(1);
    tagged.extend_from_slice(&7i64.to_le_bytes());
    assert_eq!(
        decode_row(&l, &empty_layout_row_with_extra(&tagged)),
        Err(TypedError::TooDeep)
    );
}

#[test]
fn every_overflow_value_kind_round_trips() {
    let l = layout(vec![]);
    let row = object(vec![
        ("a_true", Value::Bool(true)),
        ("b_false", Value::Bool(false)),
        ("c_int", Value::Int(-9)),
        ("d_text", Value::Text("hi".into())),
        ("e_bytes", Value::Bytes(vec![1, 2, 3])),
        ("f_ref", Value::Ref("01ARZ3NDEKTSV4RRFFQ69G5FAV".into())),
        ("g_variant", Value::Variant("open".into())),
        (
            "h_list",
            Value::List(vec![Value::Int(1), Value::Text("two".into())]),
        ),
        ("i_object", object(vec![("n", Value::Bool(true))])),
    ]);
    let bytes = encode_row(&l, &row).unwrap();
    assert_eq!(decode_row(&l, &bytes).unwrap(), row);
}

#[test]
fn typed_column_kinds_round_trip_including_true_and_bytes() {
    let l = layout(vec![
        ("flag", ColType::Bool, false),
        ("blob", ColType::Bytes, false),
        ("ref", ColType::Ref, false),
        ("pick", ColType::Enum(vec!["a".into(), "b".into()]), false),
    ]);
    let row = object(vec![
        ("flag", Value::Bool(true)),
        ("blob", Value::Bytes(vec![9, 8])),
        ("ref", Value::Ref("r".into())),
        ("pick", Value::Variant("b".into())),
    ]);
    let bytes = encode_row(&l, &row).unwrap();
    assert_eq!(decode_row(&l, &bytes).unwrap(), row);
}

#[test]
fn a_missing_required_column_is_reported_by_name() {
    let l = layout(vec![
        ("need", ColType::Int, false),
        ("opt", ColType::Int, true),
    ]);
    let bytes = [1u8, 0b0000_0000, 0, 0, 0, 0]; // version, empty bitmap, no overflow fields
    assert_eq!(
        decode_row(&l, &bytes),
        Err(TypedError::Missing("need".into()))
    );
    let ok = [1u8, 0b0000_0000, 0, 0, 0, 0];
    let nullable_only = layout(vec![("opt", ColType::Int, true)]);
    assert!(decode_row(&nullable_only, &ok).is_ok());
}

#[test]
fn bitmap_padding_bits_must_be_clear_only_when_there_is_padding() {
    // Eight columns fill the bitmap byte exactly: every bit is a real column and may be set.
    let eight = layout(
        (0..8)
            .map(|i| {
                (
                    ["a", "b", "c", "d", "e", "f", "g", "h"][i],
                    ColType::Bool,
                    true,
                )
            })
            .collect(),
    );
    let row = object(vec![("a", Value::Bool(true)), ("h", Value::Bool(false))]);
    let bytes = encode_row(&eight, &row).unwrap();
    assert_eq!(decode_row(&eight, &bytes).unwrap(), row);
    // Three columns leave five padding bits; a set padding bit is corrupt.
    let three = layout(vec![
        ("a", ColType::Bool, true),
        ("b", ColType::Bool, true),
        ("c", ColType::Bool, true),
    ]);
    let mut bytes = encode_row(&three, &object(vec![("a", Value::Bool(true))])).unwrap();
    assert!(decode_row(&three, &bytes).is_ok());
    bytes[1] |= 0b0000_1000;
    assert_eq!(decode_row(&three, &bytes), Err(TypedError::Corrupt));
}

#[test]
fn a_repeated_overflow_name_is_a_clash_not_corruption() {
    let l = layout(vec![]);
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&2u32.to_le_bytes());
    for _ in 0..2 {
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(b'x');
        bytes.extend_from_slice(&[0, 1]); // a tagged true
    }
    assert_eq!(
        decode_row(&l, &bytes),
        Err(TypedError::OverflowClash("x".into()))
    );
}
