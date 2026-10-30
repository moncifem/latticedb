//! `IS [NOT] TRUE | FALSE | UNKNOWN` and `IS [NOT] DISTINCT FROM` checked against PostgreSQL 16: every
//! boolean against every test, equal, different and `NULL` operands of several types, precedence next
//! to arithmetic and `AND`, syntax errors and a failing operand. Each entry lists what PostgreSQL
//! answered (`NULL` is an empty field). Left out: two operand type errors (`42883` in PostgreSQL,
//! `42804` here, the documented binder difference) and `a IS DISTINCT FROM b IS TRUE`, which
//! PostgreSQL refuses as a syntax error and this parser reads left to right.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};

const CASES: &[(&str, &[&str])] = &[
    ("SELECT true IS TRUE", &["t"]),
    ("SELECT true IS NOT TRUE", &["f"]),
    ("SELECT true IS FALSE", &["f"]),
    ("SELECT true IS NOT FALSE", &["t"]),
    ("SELECT true IS UNKNOWN", &["f"]),
    ("SELECT true IS NOT UNKNOWN", &["t"]),
    ("SELECT false IS TRUE", &["f"]),
    ("SELECT false IS NOT TRUE", &["t"]),
    ("SELECT false IS FALSE", &["t"]),
    ("SELECT false IS NOT FALSE", &["f"]),
    ("SELECT false IS UNKNOWN", &["f"]),
    ("SELECT false IS NOT UNKNOWN", &["t"]),
    ("SELECT NULL::boolean IS TRUE", &["f"]),
    ("SELECT NULL::boolean IS NOT TRUE", &["t"]),
    ("SELECT NULL::boolean IS FALSE", &["f"]),
    ("SELECT NULL::boolean IS NOT FALSE", &["t"]),
    ("SELECT NULL::boolean IS UNKNOWN", &["t"]),
    ("SELECT NULL::boolean IS NOT UNKNOWN", &["f"]),
    ("SELECT 1 IS DISTINCT FROM 1", &["f"]),
    ("SELECT 1 IS NOT DISTINCT FROM 1", &["t"]),
    ("SELECT 1 IS DISTINCT FROM 2", &["t"]),
    ("SELECT 1 IS NOT DISTINCT FROM 2", &["f"]),
    ("SELECT 1 IS DISTINCT FROM NULL::int", &["t"]),
    ("SELECT 1 IS NOT DISTINCT FROM NULL::int", &["f"]),
    ("SELECT NULL::int IS DISTINCT FROM 1", &["t"]),
    ("SELECT NULL::int IS NOT DISTINCT FROM 1", &["f"]),
    ("SELECT NULL::int IS DISTINCT FROM NULL::int", &["f"]),
    ("SELECT NULL::int IS NOT DISTINCT FROM NULL::int", &["t"]),
    ("SELECT 'a' IS DISTINCT FROM 'a'", &["f"]),
    ("SELECT 'a' IS NOT DISTINCT FROM 'a'", &["t"]),
    ("SELECT 'a' IS DISTINCT FROM 'b'", &["t"]),
    ("SELECT 'a' IS NOT DISTINCT FROM 'b'", &["f"]),
    ("SELECT 'a' IS DISTINCT FROM NULL::text", &["t"]),
    ("SELECT 'a' IS NOT DISTINCT FROM NULL::text", &["f"]),
    ("SELECT NULL::text IS DISTINCT FROM NULL::text", &["f"]),
    ("SELECT NULL::text IS NOT DISTINCT FROM NULL::text", &["t"]),
    ("SELECT true IS DISTINCT FROM false", &["t"]),
    ("SELECT true IS NOT DISTINCT FROM false", &["f"]),
    ("SELECT true IS DISTINCT FROM NULL::boolean", &["t"]),
    ("SELECT true IS NOT DISTINCT FROM NULL::boolean", &["f"]),
    ("SELECT 1.5 IS DISTINCT FROM 1.5", &["f"]),
    ("SELECT 1.5 IS NOT DISTINCT FROM 1.5", &["t"]),
    ("SELECT 1 IS DISTINCT FROM 1.0", &["f"]),
    ("SELECT 1 IS NOT DISTINCT FROM 1.0", &["t"]),
    ("SELECT 1 IS DISTINCT FROM 1.5", &["t"]),
    ("SELECT 1 IS NOT DISTINCT FROM 1.5", &["f"]),
    (
        "SELECT '2024-01-01'::date IS DISTINCT FROM '2024-01-01'::date",
        &["f"],
    ),
    (
        "SELECT '2024-01-01'::date IS NOT DISTINCT FROM '2024-01-01'::date",
        &["t"],
    ),
    (
        "SELECT '2024-01-01'::date IS DISTINCT FROM NULL::date",
        &["t"],
    ),
    (
        "SELECT '2024-01-01'::date IS NOT DISTINCT FROM NULL::date",
        &["f"],
    ),
    ("SELECT 1 IS DISTINCT FROM 'x'", &["ERR 22P02"]),
    ("SELECT 1 IS NOT DISTINCT FROM 'x'", &["ERR 22P02"]),
    (
        "SELECT 1 IS DISTINCT FROM 2 AND 3 IS DISTINCT FROM 3",
        &["f"],
    ),
    ("SELECT (1 IS DISTINCT FROM NULL) = true", &["t"]),
    ("SELECT NOT 1 IS DISTINCT FROM 1", &["t"]),
    ("SELECT 1 + 1 IS DISTINCT FROM 2", &["f"]),
    ("SELECT 1 IS DISTINCT FROM 1 + 1", &["t"]),
    ("SELECT 1 = 1 IS TRUE", &["t"]),
    ("SELECT NULL = 1 IS NOT TRUE", &["t"]),
    ("SELECT NULL = 1 IS UNKNOWN", &["t"]),
    ("SELECT 5 IS TRUE", &["ERR 42804"]),
    ("SELECT 'a' IS TRUE", &["ERR 22P02"]),
    ("SELECT NULL IS TRUE", &["f"]),
    ("SELECT NULL IS UNKNOWN", &["t"]),
    ("SELECT NULL IS NOT UNKNOWN", &["f"]),
    ("SELECT 1 IS DISTINCT FROM", &["ERR 42601"]),
    ("SELECT 1 IS DISTINCT 2", &["ERR 42601"]),
    ("SELECT 1 IS NOT", &["ERR 42601"]),
    ("SELECT 1 / 0 IS DISTINCT FROM 1", &["ERR 22012"]),
    ("SELECT 1 IS DISTINCT FROM 1 / 0", &["ERR 22012"]),
    ("SELECT NULL IS DISTINCT FROM 1 / 0", &["ERR 22012"]),
];

#[test]
fn truth_tests_and_distinct_from_answer_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in CASES {
        let got: Vec<String> = match s.execute(&mut f.db, sql) {
            Ok(r) => text(&r)
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|c| if c == "NULL" { "" } else { c.as_str() })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect(),
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}
