//! Validating a value against a checked type. Invariant I8: nothing this accepts violates the
//! schema, and nothing is accepted that has an unknown field, a wrong type or a broken refinement.

use crate::ast::Ty;
use crate::check::{Compiled, Field};
use std::collections::BTreeMap;

/// A schema-less value, as clients and the engine see payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Text.
    Text(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// A list.
    List(Vec<Value>),
    /// A reference to another entity, by id text.
    Ref(String),
    /// An enum variant name.
    Variant(String),
    /// An object of named fields.
    Object(BTreeMap<String, Value>),
}

/// One problem found in a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Path to the offending part, like `acceptance[2]`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

fn bad(path: &str, message: impl Into<String>) -> Violation {
    Violation {
        path: path.to_string(),
        message: message.into(),
    }
}

fn check_value(c: &Compiled, ty: &Ty, v: &Value, path: &str, out: &mut Vec<Violation>) {
    match (ty, v) {
        (Ty::Bool, Value::Bool(_)) | (Ty::Ref(_), Value::Ref(_)) => {}
        (Ty::Int(range), Value::Int(n)) => {
            if let Some((lo, hi)) = range
                && !(*lo..=*hi).contains(n)
            {
                out.push(bad(path, format!("{n} is outside {lo}..={hi}")));
            }
        }
        (Ty::Text(max), Value::Text(s)) => {
            if let Some(m) = max
                && s.chars().count() as u64 > *m
            {
                out.push(bad(path, format!("text is longer than {m} characters")));
            }
        }
        (Ty::Bytes(max), Value::Bytes(b)) => {
            if let Some(m) = max
                && b.len() as u64 > *m
            {
                out.push(bad(path, format!("bytes are longer than {m}")));
            }
        }
        (Ty::List(inner, max), Value::List(items)) => {
            if let Some(m) = max
                && items.len() as u64 > *m
            {
                out.push(bad(path, format!("list has more than {m} items")));
            }
            for (i, item) in items.iter().enumerate() {
                check_value(c, inner, item, &format!("{path}[{i}]"), out);
            }
        }
        (Ty::Enum(name), Value::Variant(variant)) => match c.enums.get(name) {
            Some(vs) if vs.contains(variant) => {}
            Some(_) => out.push(bad(path, format!("`{variant}` is not a variant of {name}"))),
            None => out.push(bad(path, format!("unknown enum {name}"))),
        },
        (expected, got) => out.push(bad(path, format!("expected {expected:?}, got {got:?}"))),
    }
}

/// Validates `value` as an instance of type `type_name`. Returns every violation.
pub fn validate(c: &Compiled, type_name: &str, value: &Value) -> Result<(), Vec<Violation>> {
    let Some(fields) = c.types.get(type_name) else {
        return Err(vec![bad("", format!("unknown type `{type_name}`"))]);
    };
    let Value::Object(obj) = value else {
        return Err(vec![bad("", "a value of an entity type must be an object")]);
    };
    let mut out = Vec::new();
    for (name, Field { ty, optional }) in fields {
        match obj.get(name) {
            Some(v) => check_value(c, ty, v, name, &mut out),
            None if !optional => out.push(bad(name, "required field is missing")),
            None => {}
        }
    }
    for name in obj.keys().filter(|k| !fields.contains_key(*k)) {
        out.push(bad(name, "unknown field"));
    }
    if out.is_empty() { Ok(()) } else { Err(out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::check;
    use crate::parser::parse;

    fn schema() -> Compiled {
        check(
            &parse(
                "enum P { low, high }\ntype Epic { title: text }\ntype Story { title: text max 5  pts: int in 0..=10 optional  pri: P  epic: ref<Epic> optional  ac: list<text max 3> max 2 }",
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn obj(fields: &[(&str, Value)]) -> Value {
        Value::Object(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    fn good() -> Vec<(&'static str, Value)> {
        vec![
            ("title", Value::Text("hello".into())),
            ("pri", Value::Variant("low".into())),
            ("ac", Value::List(vec![Value::Text("abc".into())])),
        ]
    }

    #[test]
    fn inv_i8_accepts_a_valid_value_and_optional_fields() {
        let mut f = good();
        assert!(validate(&schema(), "Story", &obj(&f)).is_ok());
        f.push(("pts", Value::Int(10)));
        f.push(("epic", Value::Ref("01ABC".into())));
        assert!(validate(&schema(), "Story", &obj(&f)).is_ok());
    }

    #[test]
    fn inv_i8_rejects_each_kind_of_violation_with_a_path() {
        let c = schema();
        let with = |k: &str, v: Value| {
            let mut f: Vec<(&str, Value)> = good().into_iter().filter(|(n, _)| *n != k).collect();
            f.push((k, v));
            validate(&c, "Story", &obj(&f)).unwrap_err()
        };
        assert_eq!(
            with("title", Value::Text("toolong".into()))[0].path,
            "title"
        );
        assert_eq!(with("pts", Value::Int(11))[0].path, "pts");
        assert_eq!(with("pri", Value::Variant("nope".into()))[0].path, "pri");
        assert_eq!(with("pri", Value::Int(1))[0].path, "pri");
        assert_eq!(
            with("ac", Value::List(vec![Value::Text("abcd".into())]))[0].path,
            "ac[0]"
        );
        assert_eq!(
            with("ac", Value::List(vec![Value::Text("a".into()); 3]))[0].path,
            "ac"
        );
        let mut f = good();
        f.push(("extra", Value::Bool(true)));
        assert_eq!(
            validate(&c, "Story", &obj(&f)).unwrap_err()[0].message,
            "unknown field"
        );
        let missing: Vec<_> = good().into_iter().filter(|(n, _)| *n != "title").collect();
        assert_eq!(
            validate(&c, "Story", &obj(&missing)).unwrap_err()[0].message,
            "required field is missing"
        );
        assert!(validate(&c, "Nope", &obj(&[])).is_err());
        assert!(validate(&c, "Story", &Value::Int(1)).is_err());
    }
}
