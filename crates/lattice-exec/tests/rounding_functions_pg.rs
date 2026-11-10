//! `ceil`, `ceiling`, `floor`, `trunc` and `sign` over integer, numeric and double precision columns,
//! checked against PostgreSQL 16: negative and fractional values, zero, `NULL`, a value past the
//! numeric range of an integer (`1e20`), negative zero and the double route an integer argument takes.
//! Left out: `trunc` with a digit count.
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
        "CREATE TABLE m (id INT PRIMARY KEY, i INT, n NUMERIC, f DOUBLE PRECISION)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO m VALUES (1, 5, 1.2, 1.2), (2, -5, -1.2, -1.2), (3, 0, 0.0, 0.0), (4, NULL, NULL, NULL), (5, 7, -0.5, -0.5), (6, 2, 2.50, 2.5), (7, 3, 100, 1e20)",
        &["INSERT 0 7"],
    ),
    (
        "SELECT id, ceil(i), ceil(n), ceil(f) FROM m ORDER BY id",
        &[
            "1|5|2|2",
            "2|-5|-1|-1",
            "3|0|0|0",
            "4|||",
            "5|7|0|-0",
            "6|2|3|3",
            "7|3|100|1e+20",
        ],
    ),
    (
        "SELECT id, floor(i), floor(n), floor(f) FROM m ORDER BY id",
        &[
            "1|5|1|1",
            "2|-5|-2|-2",
            "3|0|0|0",
            "4|||",
            "5|7|-1|-1",
            "6|2|2|2",
            "7|3|100|1e+20",
        ],
    ),
    (
        "SELECT id, trunc(i), trunc(n), trunc(f) FROM m ORDER BY id",
        &[
            "1|5|1|1",
            "2|-5|-1|-1",
            "3|0|0|0",
            "4|||",
            "5|7|0|-0",
            "6|2|2|2",
            "7|3|100|1e+20",
        ],
    ),
    (
        "SELECT id, sign(i), sign(n), sign(f) FROM m ORDER BY id",
        &[
            "1|1|1|1",
            "2|-1|-1|-1",
            "3|0|0|0",
            "4|||",
            "5|1|-1|-1",
            "6|1|1|1",
            "7|1|1|1",
        ],
    ),
    (
        "SELECT ceiling(1.0), ceiling(-0.2), floor(-0.0), sign(-0.0::float8)",
        &["1|0|0|0"],
    ),
    (
        "SELECT ceil(2.5) + 1, floor(i) + 0.5 FROM m WHERE id = 1",
        &["4|5.5"],
    ),
    (
        "SELECT sign('NaN'::float8), ceil('Infinity'::float8)",
        &["0|Infinity"],
    ),
];

#[test]
fn rounding_functions_answer_as_postgresql_does() {
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
