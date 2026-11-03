//! `||` with a text operand and any other type, checked against PostgreSQL 16: literals, integer,
//! numeric, boolean and date columns on either side, chains, and `NULL` operands (an empty field).
//! Two non-text operands (`1 || 2`) are an error in PostgreSQL (`42883`); here `42804`, a documented
//! difference.
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
        "CREATE TABLE ct (id INT PRIMARY KEY, t TEXT, n NUMERIC, b BOOLEAN, d DATE)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO ct VALUES (1,'x',1.50,true,'2024-03-05'),(2,NULL,NULL,NULL,NULL)",
        &["INSERT 0 2"],
    ),
    ("SELECT 'a' || 1", &["a1"]),
    ("SELECT 1 || 'a'", &["1a"]),
    ("SELECT t || id FROM ct ORDER BY id", &["x1", ""]),
    ("SELECT id || t FROM ct ORDER BY id", &["1x", ""]),
    (
        "SELECT t || n || b || d FROM ct ORDER BY id",
        &["x1.50true2024-03-05", ""],
    ),
    ("SELECT 'v' || n FROM ct ORDER BY id", &["v1.50", ""]),
    ("SELECT 'a' || true", &["atrue"]),
    ("SELECT NULL || 1, 1 || NULL, 'a' || NULL", &["||"]),
    ("SELECT id || id FROM ct", &["ERR 42804"]),
];

#[test]
fn concat_converts_the_other_operand_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in STEPS {
        let got: Vec<String> = match s.execute(&mut f.db, sql) {
            Ok(r) => {
                let mut lines: Vec<String> = text(&r)
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|c| if c == "NULL" { "" } else { c.as_str() })
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .collect();
                if r.columns.is_empty() || !sql.starts_with("SELECT") {
                    lines.push(r.tag);
                }
                lines
            }
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}
