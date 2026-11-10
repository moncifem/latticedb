//! `greatest` and `least` checked against PostgreSQL 16 at UTC: `NULL` arguments are skipped and the
//! result is `NULL` only when every argument is, mixed numeric types unify, text compares bytewise,
//! dates and timestamps compare together, `NaN` counts as the largest float, and a failing argument
//! fails the call. Each entry lists what PostgreSQL answered (`NULL` is an empty field). Left out:
//! `greatest()` with no argument, a syntax error (`42601`) in PostgreSQL and an arity error (`42883`)
//! here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};

const CASES: &[(&str, &[&str])] = &[
    ("SELECT greatest(1, 2, 3)", &["3"]),
    ("SELECT least(1, 2, 3)", &["1"]),
    ("SELECT greatest(3, 2, 1)", &["3"]),
    ("SELECT least(3, 2, 1)", &["1"]),
    ("SELECT greatest(1)", &["1"]),
    ("SELECT least(1)", &["1"]),
    ("SELECT greatest(1, NULL)", &["1"]),
    ("SELECT greatest(NULL, 1)", &["1"]),
    ("SELECT least(NULL, 1, NULL)", &["1"]),
    ("SELECT greatest(NULL, NULL)", &[""]),
    ("SELECT least(NULL::int, NULL)", &[""]),
    ("SELECT greatest(1, 2.5)", &["2.5"]),
    ("SELECT least(1, 2.5)", &["1"]),
    ("SELECT greatest(1.5, 2)", &["2"]),
    ("SELECT greatest(1::bigint, 2)", &["2"]),
    ("SELECT least(1, 2::float8)", &["1"]),
    ("SELECT greatest(1.0, 1)", &["1.0"]),
    ("SELECT greatest(-1, -2)", &["-1"]),
    ("SELECT least(-1, -2)", &["-2"]),
    ("SELECT greatest('a', 'b')", &["b"]),
    ("SELECT least('a', 'b')", &["a"]),
    ("SELECT greatest('b', 'a', 'c', 'B')", &["c"]),
    ("SELECT least('b', 'a', 'c', 'B')", &["B"]),
    ("SELECT greatest('', 'a')", &["a"]),
    ("SELECT greatest(true, false)", &["t"]),
    ("SELECT least(true, false)", &["f"]),
    (
        "SELECT greatest('2024-01-01'::date, '2024-06-01'::date)",
        &["2024-06-01"],
    ),
    (
        "SELECT least('2024-01-01'::date, '2024-06-01'::date)",
        &["2024-01-01"],
    ),
    (
        "SELECT greatest('2024-01-01 00:00:00+00'::timestamptz, '2023-01-01 00:00:00+00'::timestamptz)",
        &["2024-01-01 00:00:00+00"],
    ),
    (
        "SELECT least('2024-01-01 00:00:00+00'::timestamptz, '2023-01-01 00:00:00+00'::timestamptz)",
        &["2023-01-01 00:00:00+00"],
    ),
    (
        "SELECT greatest('2024-01-01'::date, '2024-01-01 12:00:00+00'::timestamptz)",
        &["2024-01-01 12:00:00+00"],
    ),
    ("SELECT greatest(1, 'x')", &["ERR 22P02"]),
    ("SELECT greatest(1, true)", &["ERR 42804"]),
    ("SELECT greatest('1', 2)", &["2"]),
    ("SELECT greatest(1e0::float8, 'NaN'::float8)", &["NaN"]),
    ("SELECT least(1e0::float8, 'NaN'::float8)", &["1"]),
    ("SELECT greatest('Infinity'::float8, 1)", &["Infinity"]),
    ("SELECT least('-Infinity'::float8, 1)", &["-Infinity"]),
    (
        "SELECT greatest('\\x01'::bytea, '\\x02'::bytea)",
        &["\\x02"],
    ),
    ("SELECT greatest(1, 2, 3) + 1", &["4"]),
    ("SELECT greatest(1, 2) = 2", &["t"]),
    ("SELECT greatest(1/1, 2)", &["2"]),
    ("SELECT greatest(1, 1/0)", &["ERR 22012"]),
    ("SELECT least(1, 2/0)", &["ERR 22012"]),
    (
        "SELECT greatest(2147483647, 9223372036854775807)",
        &["9223372036854775807"],
    ),
    ("SELECT least(1, 2, 3, 4, 5, 0, 7)", &["0"]),
    ("SELECT greatest(0, -0.0)", &["0"]),
];

#[test]
fn greatest_and_least_answer_as_postgresql_does() {
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
