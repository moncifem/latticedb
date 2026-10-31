//! Numeric literals stored into a `double precision` column, checked against PostgreSQL 16: a
//! literal past the 38 digits of `numeric` (`1e308`, a 42-digit number) takes the double route, and
//! one past the double range (`1e309`) is `22003`.
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
        "CREATE TABLE ft (id INT PRIMARY KEY, f DOUBLE PRECISION)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO ft VALUES (1, 1e308), (2, -1e308), (3, 123456789012345678901234567890123456789012.5), (4, 1.5)",
        &["INSERT 0 4"],
    ),
    (
        "SELECT id, f FROM ft ORDER BY id",
        &["1|1e+308", "2|-1e+308", "3|1.2345678901234568e+41", "4|1.5"],
    ),
    ("INSERT INTO ft VALUES (6, 1e309)", &["ERR 22003"]),
    ("INSERT INTO ft VALUES (7, 1e-400)", &["ERR 22003"]),
    (
        "INSERT INTO ft VALUES (8, 1e-320), (9, 0e-400), (10, 0.0)",
        &["INSERT 0 3"],
    ),
    ("SELECT '1e-400'::float8", &["ERR 22003"]),
    ("UPDATE ft SET f = 2e300 WHERE id = 4", &["UPDATE 1"]),
    ("SELECT f FROM ft WHERE id = 4", &["2e+300"]),
];

#[test]
fn float_literals_reach_double_columns_as_postgresql_does() {
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
