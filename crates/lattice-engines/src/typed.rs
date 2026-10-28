//! Typed rows: a schema type becomes a physical table with one column per field, never a generic
//! attribute and value table, plus an overflow area for fields this schema version does not know.
//!
//! Fields unknown to the encoding layout survive a round trip in the overflow area. Decoding must
//! use the layout that wrote the row; schema-version dispatch and online migration are future work.
//! This module is the codec between a schema-less [`Value`] and row-store bytes.
//!
//! Layout (little endian): a version byte, a presence bitmap with one bit per column, then the cell of
//! every present column in column order, then the overflow: a `u32` count and, per extra field, a `u16`
//! name length, the name and a tagged value. Decoding is strict.

use lattice_lsl::ast::Ty;
use lattice_lsl::check::Compiled;
use lattice_lsl::validate::Value;
use std::collections::BTreeMap;

const VERSION: u8 = 1;
const MAX_DEPTH: usize = 16;

/// The physical type of a column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColType {
    /// One byte.
    Bool,
    /// Eight bytes, signed.
    Int,
    /// Length-prefixed UTF-8.
    Text,
    /// Length-prefixed bytes.
    Bytes,
    /// The id of another entity, as text.
    Ref,
    /// An index into the variant list, two bytes.
    Enum(Vec<String>),
    /// A count and that many cells.
    List(Box<ColType>),
}

/// One column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The field name.
    pub name: String,
    /// The physical type.
    pub ty: ColType,
    /// Whether a row may leave it out.
    pub nullable: bool,
}

/// The physical shape of one schema type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableLayout {
    /// The schema type name.
    pub type_name: String,
    /// Columns in a fixed order: sorted by name.
    pub columns: Vec<Column>,
}

/// Why a row did not encode or decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedError {
    /// The value is not an object.
    NotAnObject,
    /// A required column has no value.
    Missing(String),
    /// A value does not have the column's type.
    Mismatch(String),
    /// An enum value is not one of the variants.
    UnknownVariant(String),
    /// An overflow field repeats a column name.
    OverflowClash(String),
    /// Nesting deeper than the limit.
    TooDeep,
    /// A length does not fit the format.
    TooLarge,
    /// The bytes end inside a field.
    Truncated,
    /// A version this code cannot read.
    UnsupportedVersion(u8),
    /// Bytes remain after the overflow area.
    Trailing,
    /// A tag, flag or index that means nothing.
    Corrupt,
}

fn col_type(ty: &Ty, c: &Compiled) -> ColType {
    match ty {
        Ty::Bool => ColType::Bool,
        Ty::Int(_) => ColType::Int,
        Ty::Text(_) => ColType::Text,
        Ty::Bytes(_) => ColType::Bytes,
        Ty::Ref(_) => ColType::Ref,
        Ty::Enum(name) => ColType::Enum(c.enums.get(name).cloned().unwrap_or_default()),
        Ty::List(inner, _) => ColType::List(Box::new(col_type(inner, c))),
    }
}

/// The layout of every type in a checked schema.
pub fn layouts(c: &Compiled) -> BTreeMap<String, TableLayout> {
    c.types
        .iter()
        .map(|(name, fields)| {
            let columns = fields
                .iter()
                .map(|(field, f)| Column {
                    name: field.clone(),
                    ty: col_type(&f.ty, c),
                    nullable: f.optional,
                })
                .collect();
            (
                name.clone(),
                TableLayout {
                    type_name: name.clone(),
                    columns,
                },
            )
        })
        .collect()
}

fn put_len(out: &mut Vec<u8>, n: usize) -> Result<(), TypedError> {
    out.extend_from_slice(
        &u32::try_from(n)
            .map_err(|_| TypedError::TooLarge)?
            .to_le_bytes(),
    );
    Ok(())
}

fn put_cell(
    out: &mut Vec<u8>,
    ty: &ColType,
    v: &Value,
    name: &str,
    depth: usize,
) -> Result<(), TypedError> {
    if depth > MAX_DEPTH {
        return Err(TypedError::TooDeep);
    }
    let bad = || TypedError::Mismatch(name.to_string());
    match (ty, v) {
        (ColType::Bool, Value::Bool(b)) => out.push(u8::from(*b)),
        (ColType::Int, Value::Int(i)) => out.extend_from_slice(&i.to_le_bytes()),
        (ColType::Text, Value::Text(s)) | (ColType::Ref, Value::Ref(s)) => {
            put_len(out, s.len())?;
            out.extend_from_slice(s.as_bytes());
        }
        (ColType::Bytes, Value::Bytes(b)) => {
            put_len(out, b.len())?;
            out.extend_from_slice(b);
        }
        (ColType::Enum(variants), Value::Variant(s)) => {
            let idx = variants
                .iter()
                .position(|x| x == s)
                .ok_or_else(|| TypedError::UnknownVariant(s.clone()))?;
            out.extend_from_slice(
                &u16::try_from(idx)
                    .map_err(|_| TypedError::TooLarge)?
                    .to_le_bytes(),
            );
        }
        (ColType::List(inner), Value::List(items)) => {
            put_len(out, items.len())?;
            for item in items {
                put_cell(out, inner, item, name, depth + 1)?;
            }
        }
        _ => return Err(bad()),
    }
    Ok(())
}

fn put_tagged(out: &mut Vec<u8>, v: &Value, depth: usize) -> Result<(), TypedError> {
    if depth > MAX_DEPTH {
        return Err(TypedError::TooDeep);
    }
    match v {
        Value::Bool(b) => out.extend_from_slice(&[0, u8::from(*b)]),
        Value::Int(i) => {
            out.push(1);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(2);
            put_len(out, s.len())?;
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(3);
            put_len(out, b.len())?;
            out.extend_from_slice(b);
        }
        Value::List(items) => {
            out.push(4);
            put_len(out, items.len())?;
            for item in items {
                put_tagged(out, item, depth + 1)?;
            }
        }
        Value::Ref(s) => {
            out.push(5);
            put_len(out, s.len())?;
            out.extend_from_slice(s.as_bytes());
        }
        Value::Variant(s) => {
            out.push(6);
            put_len(out, s.len())?;
            out.extend_from_slice(s.as_bytes());
        }
        Value::Object(map) => {
            out.push(7);
            put_len(out, map.len())?;
            for (k, item) in map {
                put_name(out, k)?;
                put_tagged(out, item, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn put_name(out: &mut Vec<u8>, name: &str) -> Result<(), TypedError> {
    out.extend_from_slice(
        &u16::try_from(name.len())
            .map_err(|_| TypedError::TooLarge)?
            .to_le_bytes(),
    );
    out.extend_from_slice(name.as_bytes());
    Ok(())
}

/// Encodes an object as a row of `layout`. Fields the layout has no column for go to the overflow area.
pub fn encode_row(layout: &TableLayout, value: &Value) -> Result<Vec<u8>, TypedError> {
    let Value::Object(map) = value else {
        return Err(TypedError::NotAnObject);
    };
    let n = layout.columns.len();
    let mut bitmap = vec![0u8; n.div_ceil(8)];
    let mut cells = Vec::new();
    for (i, col) in layout.columns.iter().enumerate() {
        match map.get(&col.name) {
            Some(v) => {
                if let Some(byte) = bitmap.get_mut(i / 8) {
                    *byte |= 1 << (i % 8);
                }
                put_cell(&mut cells, &col.ty, v, &col.name, 0)?;
            }
            None if col.nullable => {}
            None => return Err(TypedError::Missing(col.name.clone())),
        }
    }
    let mut out = vec![VERSION];
    out.extend_from_slice(&bitmap);
    out.extend_from_slice(&cells);
    let extras: Vec<(&String, &Value)> = map
        .iter()
        .filter(|(k, _)| !layout.columns.iter().any(|c| &c.name == *k))
        .collect();
    put_len(&mut out, extras.len())?;
    for (k, v) in extras {
        put_name(&mut out, k)?;
        put_tagged(&mut out, v, 0)?;
    }
    Ok(out)
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], TypedError> {
        if self.rest.len() < n {
            return Err(TypedError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, TypedError> {
        self.take(1)?.first().copied().ok_or(TypedError::Truncated)
    }

    fn u16(&mut self) -> Result<u16, TypedError> {
        let b: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| TypedError::Truncated)?;
        Ok(u16::from_le_bytes(b))
    }

    fn u32(&mut self) -> Result<u32, TypedError> {
        let b: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| TypedError::Truncated)?;
        Ok(u32::from_le_bytes(b))
    }

    fn i64(&mut self) -> Result<i64, TypedError> {
        let b: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| TypedError::Truncated)?;
        Ok(i64::from_le_bytes(b))
    }

    fn string(&mut self) -> Result<String, TypedError> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| TypedError::Corrupt)
    }

    fn name(&mut self) -> Result<String, TypedError> {
        let n = usize::from(self.u16()?);
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| TypedError::Corrupt)
    }
}

fn get_cell(r: &mut Reader<'_>, ty: &ColType, depth: usize) -> Result<Value, TypedError> {
    if depth > MAX_DEPTH {
        return Err(TypedError::TooDeep);
    }
    Ok(match ty {
        ColType::Bool => match r.u8()? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err(TypedError::Corrupt),
        },
        ColType::Int => Value::Int(r.i64()?),
        ColType::Text => Value::Text(r.string()?),
        ColType::Ref => Value::Ref(r.string()?),
        ColType::Bytes => {
            let n = r.u32()? as usize;
            Value::Bytes(r.take(n)?.to_vec())
        }
        ColType::Enum(variants) => {
            let idx = usize::from(r.u16()?);
            Value::Variant(variants.get(idx).cloned().ok_or(TypedError::Corrupt)?)
        }
        ColType::List(inner) => {
            let n = r.u32()?;
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(get_cell(r, inner, depth + 1)?);
            }
            Value::List(items)
        }
    })
}

fn get_tagged(r: &mut Reader<'_>, depth: usize) -> Result<Value, TypedError> {
    if depth > MAX_DEPTH {
        return Err(TypedError::TooDeep);
    }
    Ok(match r.u8()? {
        0 => match r.u8()? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err(TypedError::Corrupt),
        },
        1 => Value::Int(r.i64()?),
        2 => Value::Text(r.string()?),
        3 => {
            let n = r.u32()? as usize;
            Value::Bytes(r.take(n)?.to_vec())
        }
        4 => {
            let n = r.u32()?;
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(get_tagged(r, depth + 1)?);
            }
            Value::List(items)
        }
        5 => Value::Ref(r.string()?),
        6 => Value::Variant(r.string()?),
        7 => {
            let n = r.u32()?;
            let mut map = BTreeMap::new();
            let mut previous: Option<String> = None;
            for _ in 0..n {
                let k = r.name()?;
                if previous.as_ref().is_some_and(|last| last >= &k) {
                    return Err(TypedError::Corrupt);
                }
                previous = Some(k.clone());
                if map.insert(k, get_tagged(r, depth + 1)?).is_some() {
                    return Err(TypedError::Corrupt);
                }
            }
            Value::Object(map)
        }
        _ => return Err(TypedError::Corrupt),
    })
}

/// Decodes a row of `layout` into an object; overflow fields are merged back in.
pub fn decode_row(layout: &TableLayout, bytes: &[u8]) -> Result<Value, TypedError> {
    let mut r = Reader { rest: bytes };
    match r.u8()? {
        VERSION => {}
        v => return Err(TypedError::UnsupportedVersion(v)),
    }
    let n = layout.columns.len();
    let bitmap = r.take(n.div_ceil(8))?.to_vec();
    let mut map = BTreeMap::new();
    for (i, col) in layout.columns.iter().enumerate() {
        let present = bitmap.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0);
        if present {
            map.insert(col.name.clone(), get_cell(&mut r, &col.ty, 0)?);
        } else if !col.nullable {
            return Err(TypedError::Missing(col.name.clone()));
        }
    }
    // Bits beyond the last column must be clear, so one row has one encoding.
    if !n.is_multiple_of(8) && bitmap.last().is_some_and(|b| b >> (n % 8) != 0) {
        return Err(TypedError::Corrupt);
    }
    let extras = r.u32()?;
    let mut previous: Option<String> = None;
    for _ in 0..extras {
        let k = r.name()?;
        if layout.columns.iter().any(|c| c.name == k) || map.contains_key(&k) {
            return Err(TypedError::OverflowClash(k));
        }
        if previous.as_ref().is_some_and(|last| last > &k) {
            return Err(TypedError::Corrupt);
        }
        previous = Some(k.clone());
        let v = get_tagged(&mut r, 0)?;
        map.insert(k, v);
    }
    if !r.rest.is_empty() {
        return Err(TypedError::Trailing);
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use lattice_lsl::check::check;
    use lattice_lsl::parser::parse;
    use proptest::prelude::*;

    const SCHEMA: &str = "enum Status { open, closed, blocked }
        facet Titled { title: text max 80 }
        type Issue = Titled { status: Status, points: int in 0..=100 optional, done: bool,
                              labels: list<text> max 10, blob: bytes optional, parent: ref<Issue> optional }";

    fn layout() -> TableLayout {
        let c = check(&parse(SCHEMA).unwrap()).unwrap();
        layouts(&c).remove("Issue").unwrap()
    }

    fn issue(status: &str, points: Option<i64>, labels: Vec<String>) -> Value {
        let mut m = BTreeMap::new();
        m.insert("title".into(), Value::Text("t".into()));
        m.insert("status".into(), Value::Variant(status.into()));
        m.insert("done".into(), Value::Bool(false));
        m.insert(
            "labels".into(),
            Value::List(labels.into_iter().map(Value::Text).collect()),
        );
        if let Some(p) = points {
            m.insert("points".into(), Value::Int(p));
        }
        Value::Object(m)
    }

    #[test]
    fn a_type_becomes_one_typed_column_per_field_and_never_an_attribute_value_table() {
        let l = layout();
        let names: Vec<&str> = l.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "blob", "done", "labels", "parent", "points", "status", "title"
            ],
            "one column per field, sorted, composed facet fields included"
        );
        let status = l.columns.iter().find(|c| c.name == "status").unwrap();
        assert_eq!(
            status.ty,
            ColType::Enum(vec!["open".into(), "closed".into(), "blocked".into()])
        );
        assert!(!status.nullable);
        assert!(
            l.columns
                .iter()
                .find(|c| c.name == "points")
                .unwrap()
                .nullable
        );
        assert!(
            !names
                .iter()
                .any(|n| ["attribute", "attr", "key", "value", "eav"].contains(n))
        );
    }

    #[test]
    fn a_row_round_trips_and_optional_fields_stay_absent() {
        let l = layout();
        let v = issue("blocked", None, vec!["a".into(), "b".into()]);
        let back = decode_row(&l, &encode_row(&l, &v).unwrap()).unwrap();
        assert_eq!(back, v);
        let Value::Object(m) = back else { panic!() };
        assert!(!m.contains_key("points"));
    }

    #[test]
    fn fields_this_schema_does_not_know_survive_in_the_overflow_area() {
        let l = layout();
        let mut v = issue("open", Some(3), vec![]);
        let Value::Object(m) = &mut v else { panic!() };
        m.insert(
            "added_later".into(),
            Value::List(vec![Value::Int(1), Value::Text("x".into())]),
        );
        m.insert(
            "nested".into(),
            Value::Object(BTreeMap::from([("k".into(), Value::Bool(true))])),
        );
        let bytes = encode_row(&l, &v).unwrap();
        assert_eq!(decode_row(&l, &bytes).unwrap(), v);
    }

    #[test]
    fn wrong_shapes_missing_fields_and_unknown_variants_are_refused() {
        let l = layout();
        assert_eq!(encode_row(&l, &Value::Int(1)), Err(TypedError::NotAnObject));
        let mut v = issue("open", None, vec![]);
        let Value::Object(m) = &mut v else { panic!() };
        m.remove("title");
        assert_eq!(encode_row(&l, &v), Err(TypedError::Missing("title".into())));
        assert_eq!(
            encode_row(&l, &issue("wontfix", None, vec![])),
            Err(TypedError::UnknownVariant("wontfix".into()))
        );
        let mut w = issue("open", None, vec![]);
        let Value::Object(m) = &mut w else { panic!() };
        m.insert("done".into(), Value::Int(1));
        assert_eq!(encode_row(&l, &w), Err(TypedError::Mismatch("done".into())));
    }

    #[test]
    fn deep_nesting_in_the_overflow_is_refused_not_a_stack_overflow() {
        let l = layout();
        let mut deep = Value::Bool(true);
        for _ in 0..40 {
            deep = Value::List(vec![deep]);
        }
        let mut v = issue("open", None, vec![]);
        let Value::Object(m) = &mut v else { panic!() };
        m.insert("deep".into(), deep);
        assert_eq!(encode_row(&l, &v), Err(TypedError::TooDeep));
    }

    fn arb_issue() -> impl Strategy<Value = Value> {
        (
            prop_oneof![Just("open"), Just("closed"), Just("blocked")],
            proptest::option::of(0i64..100),
            proptest::collection::vec("[a-z]{0,6}", 0..4),
            proptest::option::of("[a-z]{1,5}"),
            proptest::collection::vec(any::<u8>(), 0..8),
        )
            .prop_map(|(status, points, labels, extra, blob)| {
                let mut v = issue(status, points, labels);
                let Value::Object(m) = &mut v else {
                    unreachable!()
                };
                m.insert("blob".into(), Value::Bytes(blob));
                if let Some(e) = extra {
                    m.insert(format!("x_{e}"), Value::Text(e));
                }
                v
            })
    }

    proptest! {
        #[test]
        fn what_is_encoded_decodes_to_the_same_value(v in arb_issue()) {
            let l = layout();
            prop_assert_eq!(decode_row(&l, &encode_row(&l, &v).unwrap()).unwrap(), v);
        }

        #[test]
        fn every_truncation_is_an_error(v in arb_issue()) {
            let l = layout();
            let bytes = encode_row(&l, &v).unwrap();
            for cut in 0..bytes.len() {
                prop_assert!(decode_row(&l, &bytes[..cut]).is_err(), "cut at {}", cut);
            }
        }

        #[test]
        fn arbitrary_bytes_never_panic_and_a_success_re_encodes_identically(bytes in proptest::collection::vec(any::<u8>(), 0..120)) {
            let l = layout();
            if let Ok(v) = decode_row(&l, &bytes) {
                prop_assert_eq!(encode_row(&l, &v).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn noncanonical_overflow_and_nested_object_order_is_rejected() {
        let layout = TableLayout {
            type_name: "T".into(),
            columns: vec![],
        };
        let mut reversed = vec![VERSION];
        put_len(&mut reversed, 2).unwrap();
        for name in ["b", "a"] {
            put_name(&mut reversed, name).unwrap();
            put_tagged(&mut reversed, &Value::Bool(true), 0).unwrap();
        }
        assert_eq!(decode_row(&layout, &reversed), Err(TypedError::Corrupt));

        let mut nested = vec![VERSION];
        put_len(&mut nested, 1).unwrap();
        put_name(&mut nested, "extra").unwrap();
        nested.push(7); // tagged object
        put_len(&mut nested, 2).unwrap();
        for name in ["b", "a"] {
            put_name(&mut nested, name).unwrap();
            put_tagged(&mut nested, &Value::Bool(true), 1).unwrap();
        }
        assert_eq!(decode_row(&layout, &nested), Err(TypedError::Corrupt));
    }

    #[test]
    fn typed_list_encoding_enforces_the_decoder_depth_limit() {
        let mut ty = ColType::Bool;
        let mut value = Value::Bool(true);
        for _ in 0..=MAX_DEPTH {
            ty = ColType::List(Box::new(ty));
            value = Value::List(vec![value]);
        }
        let layout = TableLayout {
            type_name: "T".into(),
            columns: vec![Column {
                name: "nested".into(),
                ty,
                nullable: false,
            }],
        };
        let row = Value::Object(BTreeMap::from([("nested".into(), value)]));
        assert_eq!(encode_row(&layout, &row), Err(TypedError::TooDeep));
    }
}
