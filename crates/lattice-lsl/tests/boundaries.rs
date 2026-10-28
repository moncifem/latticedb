//! Boundary cases found by mutation testing: each pins down an edge where an off-by-one or a
//! swapped operator in the parser, checker, compatibility rules, validator or lenses would otherwise
//! go unnoticed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_lsl::check::{Compiled, check};
use lattice_lsl::compat::compare;
use lattice_lsl::lens::{Lens, Step};
use lattice_lsl::parser::{MAX_TYPE_DEPTH, parse};
use lattice_lsl::validate::{Value, validate};
use std::collections::BTreeMap;

fn compile(src: &str) -> Compiled {
    check(&parse(src).unwrap()).unwrap()
}

fn backward(old: &str, new: &str) -> bool {
    compare(&compile(old), &compile(new)).backward
}

fn obj(fields: &[(&str, Value)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[test]
fn a_single_value_range_is_not_empty_but_a_reversed_one_is() {
    assert!(check(&parse("type T { x: int in 5..=5 }").unwrap()).is_ok());
    assert!(check(&parse("type T { x: int in 6..=5 }").unwrap()).is_err());
}

#[test]
fn comments_whitespace_and_signed_numbers_lex_correctly() {
    let src = "// header\ntype T {\n  // field note\n  x: int in -5..=5 // trailing\n}\n";
    let c = compile(src);
    assert!(validate(&c, "T", &obj(&[("x", Value::Int(-5))])).is_ok());
    assert!(validate(&c, "T", &obj(&[("x", Value::Int(6))])).is_err());
    // A lone minus is not a number, and a negative `max` is rejected.
    assert!(parse("type T { x: int in - ..=5 }").is_err());
    assert!(parse("type T { x: text max -1 }").is_err());
    assert!(parse("type T { x: text max 0 }").is_ok());
    assert!(check(&parse("type T { x: text max 0 }").unwrap()).is_err());
    assert!(parse("type T { x: int in 1.5 }").is_err());
}

#[test]
fn type_nesting_is_bounded_at_exactly_the_limit() {
    let nest = |n: usize| {
        let mut ty = "bool".to_string();
        for _ in 0..n {
            ty = format!("list<{ty}>");
        }
        format!("type T {{ x: {ty} }}")
    };
    // Lists of lists fail the checker, so ask the parser alone.
    assert!(parse(&nest(MAX_TYPE_DEPTH)).is_ok());
    assert!(parse(&nest(MAX_TYPE_DEPTH + 1)).is_err());
}

#[test]
fn the_schema_size_limit_is_inclusive() {
    let pad = |n: usize| format!("{}type T {{ }}", " ".repeat(n));
    let limit = lattice_lsl::parser::MAX_SCHEMA_BYTES;
    assert!(parse(&pad(limit - "type T { }".len())).is_ok());
    assert!(parse(&pad(limit - "type T { }".len() + 1)).is_err());
}

#[test]
fn validation_limits_are_inclusive() {
    let c = compile("type T { b: bytes max 2  l: list<int> max 2  s: text max 2 }");
    let ok = obj(&[
        ("b", Value::Bytes(vec![1, 2])),
        ("l", Value::List(vec![Value::Int(1), Value::Int(2)])),
        ("s", Value::Text("ab".into())),
    ]);
    assert!(validate(&c, "T", &ok).is_ok());
    let too_many = obj(&[
        ("b", Value::Bytes(vec![1, 2, 3])),
        ("l", Value::List(vec![Value::Int(1); 3])),
        ("s", Value::Text("abc".into())),
    ]);
    assert_eq!(validate(&c, "T", &too_many).unwrap_err().len(), 3);
}

#[test]
fn compatibility_covers_every_type_shape() {
    // Same shape is compatible, a wider limit is a compatible relaxation, a narrower one is not.
    assert!(backward("type T { x: bool }", "type T { x: bool }"));
    assert!(!backward("type T { x: bool }", "type T { x: text }"));
    assert!(backward(
        "type T { x: int in 0..=5 }",
        "type T { x: int in 0..=5 }"
    ));
    assert!(backward(
        "type T { x: int in 1..=4 }",
        "type T { x: int in 0..=5 }"
    ));
    assert!(!backward(
        "type T { x: int in 0..=5 }",
        "type T { x: int in 1..=5 }"
    ));
    assert!(!backward(
        "type T { x: int in 0..=5 }",
        "type T { x: int in 0..=4 }"
    ));
    assert!(backward(
        "type T { x: text max 3 }",
        "type T { x: text max 3 }"
    ));
    assert!(!backward(
        "type T { x: text max 4 }",
        "type T { x: text max 3 }"
    ));
    assert!(backward(
        "type A { } type T { x: ref<A> }",
        "type A { } type T { x: ref<A> }"
    ));
    assert!(!backward(
        "type A { } type B { } type T { x: ref<A> }",
        "type A { } type B { } type T { x: ref<B> }"
    ));
    let list = |m: &str| format!("type T {{ x: list<text> {m} }}");
    assert!(backward(&list("max 3"), &list("max 3")));
    assert!(backward(&list("max 3"), &list("max 4")));
    assert!(!backward(&list("max 4"), &list("max 3")));
    assert!(!backward(&list("max 3"), "type T { x: list<int> max 3 }"));
    assert!(backward(&list(""), &list("")));
    assert!(!backward(&list(""), &list("max 3")));
}

fn lens(step: Step) -> Lens {
    Lens { steps: vec![step] }
}

#[test]
fn lens_steps_apply_only_to_their_own_type() {
    let up_down = |step: Step| {
        let l = lens(step);
        let v = obj(&[("a", Value::Int(1))]);
        (
            l.up("Other", v.clone()).unwrap() == v,
            l.down("Other", v.clone()).unwrap() == v,
        )
    };
    let rename = Step::Rename {
        ty: "T".into(),
        from: "a".into(),
        to: "b".into(),
    };
    let add = Step::AddField {
        ty: "T".into(),
        field: "z".into(),
        default: Value::Int(0),
    };
    let drop = Step::DropField {
        ty: "T".into(),
        field: "a".into(),
        restore: Some(Value::Int(0)),
    };
    for step in [rename, add, drop] {
        assert_eq!(up_down(step.clone()), (true, true), "{step:?}");
    }
}

#[test]
fn stray_punctuation_is_reported_where_it_is() {
    for (src, ch, col) in [
        ("type T { x: bool / }", "'/'", 18),
        ("/ type T { }", "'/'", 1),
        ("type T { x: int in 1.5 }", "'.'", 21),
        ("type T { x: int in - }", "'-'", 20),
    ] {
        let e = parse(src).unwrap_err();
        assert_eq!(
            (e.line, e.col, e.message.as_str()),
            (1, col, format!("unexpected character {ch}").as_str()),
            "{src}"
        );
    }
}

#[test]
fn lens_down_steps_leave_other_types_alone_even_when_the_field_exists() {
    let v = obj(&[
        ("a", Value::Int(1)),
        ("b", Value::Int(2)),
        ("z", Value::Int(3)),
    ]);
    for step in [
        Step::AddField {
            ty: "T".into(),
            field: "z".into(),
            default: Value::Int(0),
        },
        Step::DropField {
            ty: "T".into(),
            field: "q".into(),
            restore: Some(Value::Int(0)),
        },
        Step::DropField {
            ty: "T".into(),
            field: "q".into(),
            restore: None,
        },
        Step::Rename {
            ty: "T".into(),
            from: "a".into(),
            to: "b".into(),
        },
    ] {
        assert_eq!(
            lens(step.clone()).down("Other", v.clone()).unwrap(),
            v,
            "{step:?}"
        );
        assert_eq!(
            lens(step.clone()).up("Other", v.clone()).unwrap(),
            v,
            "{step:?}"
        );
    }
}
