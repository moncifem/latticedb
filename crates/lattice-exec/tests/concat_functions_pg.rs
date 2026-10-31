//! `concat` and `concat_ws`, checked against PostgreSQL 16: any argument types in their output form
//! (a boolean is `t`), `NULL` arguments skipped, a `NULL` separator giving `NULL`, and one argument.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};

const STEPS: &[(&str, &[&str])] = &[
    ("SELECT concat('a',1,true,NULL,2.50,'x')", &["a1t2.50x"]),
    ("SELECT concat(NULL), concat(NULL,NULL)", &["|"]),
    (
        "SELECT concat_ws('-','a',NULL,'b'), concat_ws(NULL,'a','b')",
        &["a-b|"],
    ),
    (
        "SELECT concat_ws(',',1,2.50,true,DATE '2024-03-05')",
        &["1,2.50,t,2024-03-05"],
    ),
    (
        "SELECT concat_ws('-',NULL,NULL), concat_ws('',1,2), concat_ws('-','a')",
        &["|12|a"],
    ),
    (
        "SELECT concat(1.5::float8,'x',NULL), concat_ws('-',1.5::float8,NULL,'a')",
        &["1.5x|1.5-a"],
    ),
    ("SELECT concat('\\x41'::bytea,'z')", &["\\x41z"]),
];

#[test]
fn concat_functions_answer_as_postgresql_does() {
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
