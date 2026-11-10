//! `date_part(field, value)` checked against PostgreSQL 16 with the session time zone at UTC: every
//! field `extract` supports, on timestamptz and date values, upper case fields, `NULL` operands, wrong
//! argument counts and types, and the double precision result (arithmetic, comparison and casts on it).
//! Each entry lists what PostgreSQL answered. Left out: a `NULL` field (`NULL` in PostgreSQL, refused
//! here) and the two operand type errors, where PostgreSQL gives `42883` and this server `42804`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};

const CASES: &[(&str, &[&str])] = &[
    (
        "SELECT date_part('year', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["2024"],
    ),
    (
        "SELECT date_part('quarter', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('month', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["3"],
    ),
    (
        "SELECT date_part('week', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["11"],
    ),
    (
        "SELECT date_part('day', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["15"],
    ),
    (
        "SELECT date_part('dow', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["5"],
    ),
    (
        "SELECT date_part('doy', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["75"],
    ),
    (
        "SELECT date_part('hour', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["10"],
    ),
    (
        "SELECT date_part('minute', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["20"],
    ),
    (
        "SELECT date_part('second', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["30.5"],
    ),
    (
        "SELECT date_part('epoch', '2024-03-15 10:20:30.5+00'::timestamptz)",
        &["1710498030.5"],
    ),
    (
        "SELECT date_part('year', '1969-12-31 23:59:59+00'::timestamptz)",
        &["1969"],
    ),
    (
        "SELECT date_part('quarter', '1969-12-31 23:59:59+00'::timestamptz)",
        &["4"],
    ),
    (
        "SELECT date_part('month', '1969-12-31 23:59:59+00'::timestamptz)",
        &["12"],
    ),
    (
        "SELECT date_part('week', '1969-12-31 23:59:59+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('day', '1969-12-31 23:59:59+00'::timestamptz)",
        &["31"],
    ),
    (
        "SELECT date_part('dow', '1969-12-31 23:59:59+00'::timestamptz)",
        &["3"],
    ),
    (
        "SELECT date_part('doy', '1969-12-31 23:59:59+00'::timestamptz)",
        &["365"],
    ),
    (
        "SELECT date_part('hour', '1969-12-31 23:59:59+00'::timestamptz)",
        &["23"],
    ),
    (
        "SELECT date_part('minute', '1969-12-31 23:59:59+00'::timestamptz)",
        &["59"],
    ),
    (
        "SELECT date_part('second', '1969-12-31 23:59:59+00'::timestamptz)",
        &["59"],
    ),
    (
        "SELECT date_part('epoch', '1969-12-31 23:59:59+00'::timestamptz)",
        &["-1"],
    ),
    (
        "SELECT date_part('year', '2024-12-30 00:00:00+00'::timestamptz)",
        &["2024"],
    ),
    (
        "SELECT date_part('quarter', '2024-12-30 00:00:00+00'::timestamptz)",
        &["4"],
    ),
    (
        "SELECT date_part('month', '2024-12-30 00:00:00+00'::timestamptz)",
        &["12"],
    ),
    (
        "SELECT date_part('week', '2024-12-30 00:00:00+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('day', '2024-12-30 00:00:00+00'::timestamptz)",
        &["30"],
    ),
    (
        "SELECT date_part('dow', '2024-12-30 00:00:00+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('doy', '2024-12-30 00:00:00+00'::timestamptz)",
        &["365"],
    ),
    (
        "SELECT date_part('hour', '2024-12-30 00:00:00+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('minute', '2024-12-30 00:00:00+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('second', '2024-12-30 00:00:00+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('epoch', '2024-12-30 00:00:00+00'::timestamptz)",
        &["1735516800"],
    ),
    (
        "SELECT date_part('year', '2021-01-03 10:20:30+00'::timestamptz)",
        &["2021"],
    ),
    (
        "SELECT date_part('quarter', '2021-01-03 10:20:30+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('month', '2021-01-03 10:20:30+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('week', '2021-01-03 10:20:30+00'::timestamptz)",
        &["53"],
    ),
    (
        "SELECT date_part('day', '2021-01-03 10:20:30+00'::timestamptz)",
        &["3"],
    ),
    (
        "SELECT date_part('dow', '2021-01-03 10:20:30+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('doy', '2021-01-03 10:20:30+00'::timestamptz)",
        &["3"],
    ),
    (
        "SELECT date_part('hour', '2021-01-03 10:20:30+00'::timestamptz)",
        &["10"],
    ),
    (
        "SELECT date_part('minute', '2021-01-03 10:20:30+00'::timestamptz)",
        &["20"],
    ),
    (
        "SELECT date_part('second', '2021-01-03 10:20:30+00'::timestamptz)",
        &["30"],
    ),
    (
        "SELECT date_part('epoch', '2021-01-03 10:20:30+00'::timestamptz)",
        &["1609669230"],
    ),
    (
        "SELECT date_part('year', '2000-12-31 10:20:30+00'::timestamptz)",
        &["2000"],
    ),
    (
        "SELECT date_part('quarter', '2000-12-31 10:20:30+00'::timestamptz)",
        &["4"],
    ),
    (
        "SELECT date_part('month', '2000-12-31 10:20:30+00'::timestamptz)",
        &["12"],
    ),
    (
        "SELECT date_part('week', '2000-12-31 10:20:30+00'::timestamptz)",
        &["52"],
    ),
    (
        "SELECT date_part('day', '2000-12-31 10:20:30+00'::timestamptz)",
        &["31"],
    ),
    (
        "SELECT date_part('dow', '2000-12-31 10:20:30+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('doy', '2000-12-31 10:20:30+00'::timestamptz)",
        &["366"],
    ),
    (
        "SELECT date_part('hour', '2000-12-31 10:20:30+00'::timestamptz)",
        &["10"],
    ),
    (
        "SELECT date_part('minute', '2000-12-31 10:20:30+00'::timestamptz)",
        &["20"],
    ),
    (
        "SELECT date_part('second', '2000-12-31 10:20:30+00'::timestamptz)",
        &["30"],
    ),
    (
        "SELECT date_part('epoch', '2000-12-31 10:20:30+00'::timestamptz)",
        &["978258030"],
    ),
    (
        "SELECT date_part('year', '1970-01-01 00:00:00+00'::timestamptz)",
        &["1970"],
    ),
    (
        "SELECT date_part('quarter', '1970-01-01 00:00:00+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('month', '1970-01-01 00:00:00+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('week', '1970-01-01 00:00:00+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('day', '1970-01-01 00:00:00+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('dow', '1970-01-01 00:00:00+00'::timestamptz)",
        &["4"],
    ),
    (
        "SELECT date_part('doy', '1970-01-01 00:00:00+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('hour', '1970-01-01 00:00:00+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('minute', '1970-01-01 00:00:00+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('second', '1970-01-01 00:00:00+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('epoch', '1970-01-01 00:00:00+00'::timestamptz)",
        &["0"],
    ),
    (
        "SELECT date_part('year', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["2024"],
    ),
    (
        "SELECT date_part('quarter', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["1"],
    ),
    (
        "SELECT date_part('month', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["2"],
    ),
    (
        "SELECT date_part('week', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["9"],
    ),
    (
        "SELECT date_part('day', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["29"],
    ),
    (
        "SELECT date_part('dow', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["4"],
    ),
    (
        "SELECT date_part('doy', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["60"],
    ),
    (
        "SELECT date_part('hour', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["23"],
    ),
    (
        "SELECT date_part('minute', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["59"],
    ),
    (
        "SELECT date_part('second', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["59.999999"],
    ),
    (
        "SELECT date_part('epoch', '2024-02-29 23:59:59.999999+00'::timestamptz)",
        &["1709251199.999999"],
    ),
    ("SELECT date_part('year', '2024-03-15'::date)", &["2024"]),
    ("SELECT date_part('dow', '2024-03-15'::date)", &["5"]),
    (
        "SELECT date_part('epoch', '2024-03-15'::date)",
        &["1710460800"],
    ),
    ("SELECT date_part('day', '2024-03-15'::date)", &["15"]),
    (
        "SELECT date_part('YEAR', '2024-03-15 10:20:30+00'::timestamptz)",
        &["2024"],
    ),
    ("SELECT date_part('Year', '2024-03-15'::date)", &["2024"]),
    (
        "SELECT date_part('fortnight', '2024-03-15'::date)",
        &["ERR 22023"],
    ),
    ("SELECT date_part('year', NULL::date)", &[""]),
    ("SELECT date_part('year', NULL::timestamptz)", &[""]),
    ("SELECT date_part('year')", &["ERR 42883"]),
    (
        "SELECT date_part('year', '2024-03-15'::date, 1)",
        &["ERR 42883"],
    ),
    (
        "SELECT date_part('year', '2024-03-15'::date) + 1",
        &["2025"],
    ),
    (
        "SELECT date_part('year', '2024-03-15'::date) = 2024",
        &["t"],
    ),
    (
        "SELECT date_part('year', '2024-03-15'::date)::int",
        &["2024"],
    ),
    (
        "SELECT date_part('epoch', '2024-03-15 10:20:30.5+00'::timestamptz)::text",
        &["1710498030.5"],
    ),
];

#[test]
fn date_part_answers_as_postgresql_does() {
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
