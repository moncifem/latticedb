//! `timestamptz` and `date` output, casts and comparisons checked against PostgreSQL 16 with the session
//! time zone at UTC: the text form (fraction digits without trailing zeros, years padded to four
//! digits, the offset suffix), rounding of a seventh fraction digit, the ends of the range, and dates
//! before 1970. Each entry lists what PostgreSQL answered.
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
        "SELECT '2024-03-15 10:20:30+00'::timestamptz",
        &["2024-03-15 10:20:30+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30+00'::timestamptz::text",
        &["2024-03-15 10:20:30+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.5+00'::timestamptz",
        &["2024-03-15 10:20:30.5+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.5+00'::timestamptz::text",
        &["2024-03-15 10:20:30.5+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.5+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.123456+00'::timestamptz",
        &["2024-03-15 10:20:30.123456+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.123456+00'::timestamptz::text",
        &["2024-03-15 10:20:30.123456+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.123456+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.100000+00'::timestamptz",
        &["2024-03-15 10:20:30.1+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.100000+00'::timestamptz::text",
        &["2024-03-15 10:20:30.1+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.100000+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.000001+00'::timestamptz",
        &["2024-03-15 10:20:30.000001+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.000001+00'::timestamptz::text",
        &["2024-03-15 10:20:30.000001+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.000001+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 00:00:00+00'::timestamptz",
        &["2024-03-15 00:00:00+00"],
    ),
    (
        "SELECT '2024-03-15 00:00:00+00'::timestamptz::text",
        &["2024-03-15 00:00:00+00"],
    ),
    (
        "SELECT '2024-03-15 00:00:00+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 23:59:59.999999+00'::timestamptz",
        &["2024-03-15 23:59:59.999999+00"],
    ),
    (
        "SELECT '2024-03-15 23:59:59.999999+00'::timestamptz::text",
        &["2024-03-15 23:59:59.999999+00"],
    ),
    (
        "SELECT '2024-03-15 23:59:59.999999+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '0001-01-01 00:00:00+00'::timestamptz",
        &["0001-01-01 00:00:00+00"],
    ),
    (
        "SELECT '0001-01-01 00:00:00+00'::timestamptz::text",
        &["0001-01-01 00:00:00+00"],
    ),
    (
        "SELECT '0001-01-01 00:00:00+00'::timestamptz::date",
        &["0001-01-01"],
    ),
    (
        "SELECT '0099-12-31 23:59:59+00'::timestamptz",
        &["0099-12-31 23:59:59+00"],
    ),
    (
        "SELECT '0099-12-31 23:59:59+00'::timestamptz::text",
        &["0099-12-31 23:59:59+00"],
    ),
    (
        "SELECT '0099-12-31 23:59:59+00'::timestamptz::date",
        &["0099-12-31"],
    ),
    (
        "SELECT '0999-06-15 12:00:00+00'::timestamptz",
        &["0999-06-15 12:00:00+00"],
    ),
    (
        "SELECT '0999-06-15 12:00:00+00'::timestamptz::text",
        &["0999-06-15 12:00:00+00"],
    ),
    (
        "SELECT '0999-06-15 12:00:00+00'::timestamptz::date",
        &["0999-06-15"],
    ),
    (
        "SELECT '1000-01-01 00:00:00+00'::timestamptz",
        &["1000-01-01 00:00:00+00"],
    ),
    (
        "SELECT '1000-01-01 00:00:00+00'::timestamptz::text",
        &["1000-01-01 00:00:00+00"],
    ),
    (
        "SELECT '1000-01-01 00:00:00+00'::timestamptz::date",
        &["1000-01-01"],
    ),
    (
        "SELECT '9999-12-31 23:59:59.999999+00'::timestamptz",
        &["9999-12-31 23:59:59.999999+00"],
    ),
    (
        "SELECT '9999-12-31 23:59:59.999999+00'::timestamptz::text",
        &["9999-12-31 23:59:59.999999+00"],
    ),
    (
        "SELECT '9999-12-31 23:59:59.999999+00'::timestamptz::date",
        &["9999-12-31"],
    ),
    (
        "SELECT '1969-12-31 23:59:59.999999+00'::timestamptz",
        &["1969-12-31 23:59:59.999999+00"],
    ),
    (
        "SELECT '1969-12-31 23:59:59.999999+00'::timestamptz::text",
        &["1969-12-31 23:59:59.999999+00"],
    ),
    (
        "SELECT '1969-12-31 23:59:59.999999+00'::timestamptz::date",
        &["1969-12-31"],
    ),
    (
        "SELECT '1970-01-01 00:00:00+00'::timestamptz",
        &["1970-01-01 00:00:00+00"],
    ),
    (
        "SELECT '1970-01-01 00:00:00+00'::timestamptz::text",
        &["1970-01-01 00:00:00+00"],
    ),
    (
        "SELECT '1970-01-01 00:00:00+00'::timestamptz::date",
        &["1970-01-01"],
    ),
    (
        "SELECT '1970-01-01 00:00:00.000001+00'::timestamptz",
        &["1970-01-01 00:00:00.000001+00"],
    ),
    (
        "SELECT '1970-01-01 00:00:00.000001+00'::timestamptz::text",
        &["1970-01-01 00:00:00.000001+00"],
    ),
    (
        "SELECT '1970-01-01 00:00:00.000001+00'::timestamptz::date",
        &["1970-01-01"],
    ),
    (
        "SELECT '1900-02-28 12:00:00+00'::timestamptz",
        &["1900-02-28 12:00:00+00"],
    ),
    (
        "SELECT '1900-02-28 12:00:00+00'::timestamptz::text",
        &["1900-02-28 12:00:00+00"],
    ),
    (
        "SELECT '1900-02-28 12:00:00+00'::timestamptz::date",
        &["1900-02-28"],
    ),
    (
        "SELECT '2000-02-29 12:00:00+00'::timestamptz",
        &["2000-02-29 12:00:00+00"],
    ),
    (
        "SELECT '2000-02-29 12:00:00+00'::timestamptz::text",
        &["2000-02-29 12:00:00+00"],
    ),
    (
        "SELECT '2000-02-29 12:00:00+00'::timestamptz::date",
        &["2000-02-29"],
    ),
    (
        "SELECT '2024-03-15 10:20:30+05:30'::timestamptz",
        &["2024-03-15 04:50:30+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30+05:30'::timestamptz::text",
        &["2024-03-15 04:50:30+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30+05:30'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 10:20:30-08'::timestamptz",
        &["2024-03-15 18:20:30+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30-08'::timestamptz::text",
        &["2024-03-15 18:20:30+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30-08'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15T10:20:30Z'::timestamptz",
        &["2024-03-15 10:20:30+00"],
    ),
    (
        "SELECT '2024-03-15T10:20:30Z'::timestamptz::text",
        &["2024-03-15 10:20:30+00"],
    ),
    (
        "SELECT '2024-03-15T10:20:30Z'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 24:00:00+00'::timestamptz",
        &["2024-03-16 00:00:00+00"],
    ),
    (
        "SELECT '2024-03-15 24:00:00+00'::timestamptz::text",
        &["2024-03-16 00:00:00+00"],
    ),
    (
        "SELECT '2024-03-15 24:00:00+00'::timestamptz::date",
        &["2024-03-16"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.9999995+00'::timestamptz",
        &["2024-03-15 10:20:31+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.9999995+00'::timestamptz::text",
        &["2024-03-15 10:20:31+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.9999995+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.9999994+00'::timestamptz",
        &["2024-03-15 10:20:30.999999+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.9999994+00'::timestamptz::text",
        &["2024-03-15 10:20:30.999999+00"],
    ),
    (
        "SELECT '2024-03-15 10:20:30.9999994+00'::timestamptz::date",
        &["2024-03-15"],
    ),
    (
        "SELECT '2024-12-31 23:59:59.9999995+00'::timestamptz",
        &["2025-01-01 00:00:00+00"],
    ),
    (
        "SELECT '2024-12-31 23:59:59.9999995+00'::timestamptz::text",
        &["2025-01-01 00:00:00+00"],
    ),
    (
        "SELECT '2024-12-31 23:59:59.9999995+00'::timestamptz::date",
        &["2025-01-01"],
    ),
    (
        "SELECT '1969-12-31 23:59:59.9999995+00'::timestamptz",
        &["1970-01-01 00:00:00+00"],
    ),
    (
        "SELECT '1969-12-31 23:59:59.9999995+00'::timestamptz::text",
        &["1970-01-01 00:00:00+00"],
    ),
    (
        "SELECT '1969-12-31 23:59:59.9999995+00'::timestamptz::date",
        &["1970-01-01"],
    ),
    ("SELECT '2024-03-15'::date", &["2024-03-15"]),
    (
        "SELECT '2024-03-15'::date::timestamptz",
        &["2024-03-15 00:00:00+00"],
    ),
    ("SELECT '2024-03-15'::date::text", &["2024-03-15"]),
    ("SELECT '0001-01-01'::date", &["0001-01-01"]),
    (
        "SELECT '0001-01-01'::date::timestamptz",
        &["0001-01-01 00:00:00+00"],
    ),
    ("SELECT '0001-01-01'::date::text", &["0001-01-01"]),
    ("SELECT '9999-12-31'::date", &["9999-12-31"]),
    (
        "SELECT '9999-12-31'::date::timestamptz",
        &["9999-12-31 00:00:00+00"],
    ),
    ("SELECT '9999-12-31'::date::text", &["9999-12-31"]),
    ("SELECT '1969-12-31'::date", &["1969-12-31"]),
    (
        "SELECT '1969-12-31'::date::timestamptz",
        &["1969-12-31 00:00:00+00"],
    ),
    ("SELECT '1969-12-31'::date::text", &["1969-12-31"]),
    ("SELECT '0099-01-01'::date", &["0099-01-01"]),
    (
        "SELECT '0099-01-01'::date::timestamptz",
        &["0099-01-01 00:00:00+00"],
    ),
    ("SELECT '0099-01-01'::date::text", &["0099-01-01"]),
    ("SELECT '1900-01-01'::date", &["1900-01-01"]),
    (
        "SELECT '1900-01-01'::date::timestamptz",
        &["1900-01-01 00:00:00+00"],
    ),
    ("SELECT '1900-01-01'::date::text", &["1900-01-01"]),
    (
        "SELECT '2024-03-15 10:20:30+00'::timestamptz < '2024-03-15 10:20:30.000001+00'::timestamptz",
        &["t"],
    ),
    (
        "SELECT '2024-03-15 10:20:30+00'::timestamptz = '2024-03-15 12:20:30+02'::timestamptz",
        &["t"],
    ),
    (
        "SELECT '2024-03-15 10:20:30+00'::timestamptz > '1969-01-01 00:00:00+00'::timestamptz",
        &["t"],
    ),
    (
        "SELECT '9999-12-31 23:59:59.999999+00'::timestamptz > '0001-01-01 00:00:00+00'::timestamptz",
        &["t"],
    ),
];

#[test]
fn timestamps_and_dates_print_and_compare_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in CASES {
        let got: Vec<String> = match s.execute(&mut f.db, sql) {
            Ok(r) => text(&r).iter().map(|row| row.join("|")).collect(),
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}
