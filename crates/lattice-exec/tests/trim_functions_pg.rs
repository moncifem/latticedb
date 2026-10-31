//! `btrim`, `ltrim` and `rtrim`, checked against PostgreSQL 16: the default space, a set of
//! characters to remove, an empty set, multi-byte characters and `NULL`.
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
        "SELECT btrim('  a  ')||'|', ltrim('  a  ')||'|', rtrim('  a  ')||'|'",
        &["a||a  ||  a|"],
    ),
    (
        "SELECT btrim('xyaxy','xy'), ltrim('xyaxy','xy'), rtrim('xyaxy','xy')",
        &["a|axy|xya"],
    ),
    (
        "SELECT btrim('a',''), ltrim(NULL), btrim('é','é')",
        &["a||"],
    ),
];

#[test]
fn trim_functions_answer_as_postgresql_does() {
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
