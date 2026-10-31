//! `mod(a, b)`, checked against PostgreSQL 16: the sign follows the dividend, integer and numeric
//! operands, `NULL`, and a zero divisor (`22012`). PostgreSQL has `mod` only for integers and
//! numeric; here a double is accepted too, as `%` already does (a documented difference).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};

const STEPS: &[(&str, &[&str])] = &[
    (
        "SELECT mod(7,3), mod(-7,3), mod(7,-3), mod(7.5,2), mod(NULL,2), mod(5,2.5), mod(7::bigint,3), mod(-7.5,2)",
        &["1|-1|1|1.5||0.0|1|-1.5"],
    ),
    ("SELECT mod(1,0)", &["ERR 22012"]),
    ("SELECT mod(-9223372036854775808,-1)", &["0"]),
];

#[test]
fn mod_answers_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in STEPS {
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
