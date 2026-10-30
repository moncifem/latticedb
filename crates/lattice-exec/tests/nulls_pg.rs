//! Ordering, `NULL` logic, `LIMIT` and scalar expressions over a table with `NULL`s, checked against
//! PostgreSQL 16. Rows are written `a|b` with `NULL` as an empty field, as `psql` prints them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, ok, rows};

const SETUP: &[&str] = &[
    "CREATE TABLE t (id INT PRIMARY KEY, g INT, v INT, s TEXT)",
    "INSERT INTO t VALUES (1, 1, 10, 'b'), (2, 1, NULL, 'a'), (3, 2, 5, NULL), (4, 2, 5, 'c'), (5, NULL, 7, 'a'), (6, NULL, NULL, NULL)",
];

/// Each query with the rows PostgreSQL returned for it.
const CASES: &[(&str, &[&str])] = &[
    (
        "SELECT id FROM t ORDER BY v",
        &["3", "4", "5", "1", "2", "6"],
    ),
    (
        "SELECT id FROM t ORDER BY v DESC, id",
        &["2", "6", "1", "5", "3", "4"],
    ),
    (
        "SELECT id FROM t ORDER BY v NULLS FIRST, id",
        &["2", "6", "3", "4", "5", "1"],
    ),
    (
        "SELECT id FROM t ORDER BY v DESC NULLS LAST, id",
        &["1", "5", "3", "4", "2", "6"],
    ),
    (
        "SELECT id FROM t ORDER BY s, id",
        &["2", "5", "1", "4", "3", "6"],
    ),
    ("SELECT id FROM t WHERE v = NULL", &[]),
    ("SELECT id FROM t WHERE v IS NULL ORDER BY id", &["2", "6"]),
    (
        "SELECT id FROM t WHERE v IS NOT NULL ORDER BY id",
        &["1", "3", "4", "5"],
    ),
    (
        "SELECT id FROM t WHERE v NOT IN (5) ORDER BY id",
        &["1", "5"],
    ),
    (
        "SELECT id FROM t WHERE v IN (5, NULL) ORDER BY id",
        &["3", "4"],
    ),
    ("SELECT id FROM t WHERE v NOT IN (5, NULL) ORDER BY id", &[]),
    (
        "SELECT id FROM t WHERE NOT (v > 5) ORDER BY id",
        &["3", "4"],
    ),
    (
        "SELECT id FROM t WHERE v BETWEEN 5 AND 10 ORDER BY id",
        &["1", "3", "4", "5"],
    ),
    (
        "SELECT id FROM t WHERE v NOT BETWEEN 5 AND 7 ORDER BY id",
        &["1"],
    ),
    (
        "SELECT id FROM t WHERE v > 5 OR s = 'a' ORDER BY id",
        &["1", "2", "5"],
    ),
    (
        "SELECT id FROM t WHERE v > 5 AND s = 'a' ORDER BY id",
        &["5"],
    ),
    (
        "SELECT id, coalesce(v, -1) FROM t ORDER BY id",
        &["1|10", "2|-1", "3|5", "4|5", "5|7", "6|-1"],
    ),
    (
        "SELECT id, nullif(v, 5) FROM t ORDER BY id",
        &["1|10", "2|", "3|", "4|", "5|7", "6|"],
    ),
    ("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1", &["2", "3"]),
    (
        "SELECT id FROM t ORDER BY id LIMIT NULL",
        &["1", "2", "3", "4", "5", "6"],
    ),
    ("SELECT id FROM t ORDER BY id OFFSET 4", &["5", "6"]),
    (
        "SELECT id, v + 1, v * 2, -v FROM t ORDER BY id",
        &[
            "1|11|20|-10",
            "2|||",
            "3|6|10|-5",
            "4|6|10|-5",
            "5|8|14|-7",
            "6|||",
        ],
    ),
];

#[test]
fn queries_over_nulls_return_what_postgresql_returns() {
    let mut f = fixture();
    let mut s = common::admin();
    for sql in SETUP {
        ok(&mut f.db, &mut s, sql);
    }
    for (sql, want) in CASES {
        let got: Vec<String> = rows(&mut f.db, &mut s, sql)
            .iter()
            .map(|r| {
                r.iter()
                    .map(|c| if c == "NULL" { "" } else { c.as_str() })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        assert_eq!(&got, want, "{sql}");
    }
}
