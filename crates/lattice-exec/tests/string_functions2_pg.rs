//! `reverse`, `starts_with`, `ascii`, `chr` and `substr`, checked against PostgreSQL 16: multi-byte
//! characters, empty strings, `NULL`, and the `chr` errors (`22023` for a negative number, `54000` for
//! zero, a surrogate and a number past U+10FFFF).
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
        "SELECT reverse('abc'), reverse(''), reverse('añb')",
        &["cba||bña"],
    ),
    (
        "SELECT starts_with('abc','ab'), starts_with('abc',''), starts_with('a','abc')",
        &["t|t|f"],
    ),
    (
        "SELECT ascii('a'), ascii(''), ascii('é'), ascii('€')",
        &["97|0|233|8364"],
    ),
    ("SELECT chr(97), chr(233), chr(8364)", &["a|é|€"]),
    (
        "SELECT substr('hello',2,3), substr('hello',2)",
        &["ell|ello"],
    ),
    (
        "SELECT reverse(NULL), ascii(NULL), chr(NULL), starts_with('a', NULL)",
        &["|||"],
    ),
    ("SELECT chr(0)", &["ERR 54000"]),
    ("SELECT chr(-1)", &["ERR 22023"]),
    ("SELECT chr(1114112)", &["ERR 54000"]),
    ("SELECT chr(55296)", &["ERR 54000"]),
];

#[test]
fn string_functions_answer_as_postgresql_does() {
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
