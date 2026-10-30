//! Ungrouped aggregates (`count`, `sum`, `avg`, `min`, `max`) in one session, checked against PostgreSQL 16 at
//! UTC: every function over every type it accepts, `NULL` arguments, empty inputs, `bigint` overflow of
//! `sum`, `numeric` scale of `avg`, float results, expressions inside an aggregate, constants next to
//! aggregates, `LIMIT` and `OFFSET`, `INSERT ... SELECT` of an aggregate row, and the type and grouping
//! errors. Each step lists what PostgreSQL answered (`NULL` is an empty field). Left out, because this
//! server does not run them yet or answers with a different code: expressions over aggregates
//! (`sum(i) + 1`), `ORDER BY` an aggregate, `DISTINCT` and `GROUP BY` and `HAVING`, `min` and `max` of
//! `bytea` (here an error as in PostgreSQL, but listed with the unsupported group), `sum(*)` and
//! `count()` (other error codes), and `ORDER BY` a column (`42803` there, `42703` here).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};
use lattice_core::bugs::Bug;

const STEPS: &[(&str, &[&str])] = &[
    (
        "CREATE TABLE ag (id INT PRIMARY KEY, i INT, s SMALLINT, b BIGINT, n NUMERIC, f DOUBLE PRECISION, t TEXT, d DATE, ts TIMESTAMPTZ, bo BOOLEAN, y BYTEA)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO ag VALUES (1, 10, 1, 9223372036854775800, 1.50, 1.5, 'b', '2024-03-15', '2024-03-15 10:00:00+00', true, '\\x01'), (2, NULL, 2, 5, 2.25, NULL, 'a', '2024-01-01', '2024-01-01 00:00:00+00', false, '\\x02'), (3, 20, NULL, NULL, NULL, 2.5, NULL, NULL, NULL, NULL, NULL), (4, 5, 3, 100, 0.10, -1.0, 'c', '2025-01-01', '2023-12-31 23:59:59.5+00', true, '\\x00ff')",
        &["INSERT 0 4"],
    ),
    (
        "CREATE TABLE em (id INT PRIMARY KEY, i INT, n NUMERIC, f DOUBLE PRECISION, t TEXT)",
        &["CREATE TABLE"],
    ),
    ("SELECT count(i) FROM ag", &["3"]),
    ("SELECT sum(i) FROM ag", &["35"]),
    ("SELECT avg(i) FROM ag", &["11.6666666666666667"]),
    ("SELECT min(i) FROM ag", &["5"]),
    ("SELECT max(i) FROM ag", &["20"]),
    ("SELECT count(s) FROM ag", &["3"]),
    ("SELECT sum(s) FROM ag", &["6"]),
    ("SELECT avg(s) FROM ag", &["2.0000000000000000"]),
    ("SELECT min(s) FROM ag", &["1"]),
    ("SELECT max(s) FROM ag", &["3"]),
    ("SELECT count(b) FROM ag", &["3"]),
    ("SELECT sum(b) FROM ag", &["9223372036854775905"]),
    ("SELECT avg(b) FROM ag", &["3074457345618258635"]),
    ("SELECT min(b) FROM ag", &["5"]),
    ("SELECT max(b) FROM ag", &["9223372036854775800"]),
    ("SELECT count(n) FROM ag", &["3"]),
    ("SELECT sum(n) FROM ag", &["3.85"]),
    ("SELECT avg(n) FROM ag", &["1.28333333333333333333"]),
    ("SELECT min(n) FROM ag", &["0.10"]),
    ("SELECT max(n) FROM ag", &["2.25"]),
    ("SELECT count(f) FROM ag", &["3"]),
    ("SELECT sum(f) FROM ag", &["3"]),
    ("SELECT avg(f) FROM ag", &["1"]),
    ("SELECT min(f) FROM ag", &["-1"]),
    ("SELECT max(f) FROM ag", &["2.5"]),
    ("SELECT count(t) FROM ag", &["3"]),
    ("SELECT sum(t) FROM ag", &["ERR 42883"]),
    ("SELECT avg(t) FROM ag", &["ERR 42883"]),
    ("SELECT min(t) FROM ag", &["a"]),
    ("SELECT max(t) FROM ag", &["c"]),
    ("SELECT count(d) FROM ag", &["3"]),
    ("SELECT sum(d) FROM ag", &["ERR 42883"]),
    ("SELECT avg(d) FROM ag", &["ERR 42883"]),
    ("SELECT min(d) FROM ag", &["2024-01-01"]),
    ("SELECT max(d) FROM ag", &["2025-01-01"]),
    ("SELECT count(ts) FROM ag", &["3"]),
    ("SELECT sum(ts) FROM ag", &["ERR 42883"]),
    ("SELECT avg(ts) FROM ag", &["ERR 42883"]),
    ("SELECT min(ts) FROM ag", &["2023-12-31 23:59:59.5+00"]),
    ("SELECT max(ts) FROM ag", &["2024-03-15 10:00:00+00"]),
    ("SELECT count(y) FROM ag", &["3"]),
    ("SELECT sum(y) FROM ag", &["ERR 42883"]),
    ("SELECT avg(y) FROM ag", &["ERR 42883"]),
    ("SELECT min(y) FROM ag", &["ERR 42883"]),
    ("SELECT max(y) FROM ag", &["ERR 42883"]),
    ("SELECT count(*) FROM ag", &["4"]),
    (
        "SELECT count(*), count(i), count(n), count(t) FROM ag",
        &["4|3|3|3"],
    ),
    ("SELECT count(*) FROM ag WHERE i > 5", &["2"]),
    ("SELECT count(*) FROM ag WHERE i > 500", &["0"]),
    ("SELECT count(i) FROM ag WHERE i > 500", &["0"]),
    ("SELECT sum(i) FROM ag WHERE i > 500", &[""]),
    ("SELECT avg(i) FROM ag WHERE i > 500", &[""]),
    ("SELECT min(i), max(i) FROM ag WHERE i > 500", &["|"]),
    (
        "SELECT count(*), sum(i), min(t), max(n) FROM ag WHERE id = 99",
        &["0|||"],
    ),
    (
        "SELECT sum(i), avg(i), min(i), max(i), count(i) FROM ag",
        &["35|11.6666666666666667|5|20|3"],
    ),
    ("SELECT sum(i) AS total FROM ag", &["35"]),
    ("SELECT count(*) AS c, max(t) AS m FROM ag", &["4|c"]),
    (
        "SELECT sum(b) FROM ag WHERE id = 1",
        &["9223372036854775800"],
    ),
    (
        "SELECT sum(b) FROM ag WHERE id IN (1, 4)",
        &["9223372036854775900"],
    ),
    ("SELECT sum(i * 2) FROM ag", &["70"]),
    ("SELECT sum(i + s) FROM ag", &["19"]),
    ("SELECT avg(i * 1.0) FROM ag", &["11.6666666666666667"]),
    ("SELECT sum(n * 2) FROM ag", &["7.70"]),
    ("SELECT max(i + 1) FROM ag", &["21"]),
    ("SELECT min(lower(t)) FROM ag", &["a"]),
    ("SELECT count(i = 10) FROM ag", &["3"]),
    ("SELECT max(id) FROM ag", &["4"]),
    ("SELECT avg(id) FROM ag", &["2.5000000000000000"]),
    ("SELECT sum(id) FROM ag", &["10"]),
    ("SELECT sum(bo) FROM ag", &["ERR 42883"]),
    ("SELECT min(bo) FROM ag", &["ERR 42883"]),
    ("SELECT count(bo) FROM ag", &["3"]),
    ("SELECT avg(t) FROM ag", &["ERR 42883"]),
    ("SELECT sum(t) FROM ag", &["ERR 42883"]),
    ("SELECT sum(d) FROM ag", &["ERR 42883"]),
    ("SELECT sum(i), id FROM ag", &["ERR 42803"]),
    ("SELECT count(*), i FROM ag", &["ERR 42803"]),
    ("SELECT *, count(*) FROM ag", &["ERR 42803"]),
    ("SELECT i, sum(i) FROM ag", &["ERR 42803"]),
    ("SELECT lower(t), count(*) FROM ag", &["ERR 42803"]),
    ("SELECT count(*) FROM em", &["0"]),
    ("SELECT sum(i) FROM em", &[""]),
    ("SELECT avg(n) FROM em", &[""]),
    ("SELECT max(t) FROM em", &[""]),
    ("SELECT min(f) FROM em", &[""]),
    ("SELECT count(i) FROM em", &["0"]),
    ("SELECT count(*) FROM ag LIMIT 1", &["4"]),
    ("SELECT count(*) FROM ag LIMIT 0", &[]),
    ("SELECT count(*) FROM ag OFFSET 1", &[]),
    ("SELECT count(*) FROM ag ORDER BY 1", &["4"]),
    ("SELECT count(*) AS c FROM ag ORDER BY c", &["4"]),
    ("SELECT count(i, s) FROM ag", &["ERR 42883"]),
    ("SELECT max(f) FROM ag", &["2.5"]),
    ("SELECT min(f) FROM ag", &["-1"]),
    ("SELECT avg(f) FROM ag", &["1"]),
    ("SELECT sum(f) FROM ag", &["3"]),
    ("SELECT sum(f) FROM ag WHERE f > 1", &["4"]),
    ("SELECT avg(f) FROM ag WHERE f > 1", &["2"]),
    ("SELECT count(*) FROM ag WHERE i IS NULL", &["1"]),
    ("SELECT min(ts) FROM ag", &["2023-12-31 23:59:59.5+00"]),
    (
        "SELECT max(d) FROM ag WHERE d < '2025-01-01'",
        &["2024-03-15"],
    ),
    (
        "SELECT min(n), max(n), sum(n), avg(n) FROM ag",
        &["0.10|2.25|3.85|1.28333333333333333333"],
    ),
    ("SELECT avg(s) FROM ag", &["2.0000000000000000"]),
    ("SELECT sum(s) FROM ag", &["6"]),
    (
        "SELECT avg(b) FROM ag WHERE id = 4",
        &["100.0000000000000000"],
    ),
    (
        "INSERT INTO em SELECT 1, sum(i), avg(n), max(f), min(t) FROM ag RETURNING id, i, n, f, t",
        &["1|35|1.28333333333333333333|2.5|a", "INSERT 0 1"],
    ),
    ("SELECT count(*) FROM em", &["1"]),
    ("SELECT sum(i) FROM ag WHERE false", &[""]),
];

#[test]
fn aggregates_answer_as_postgresql_does() {
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

/// Switches a defect off again however the test ends.
struct Planted;

impl Planted {
    fn on(b: lattice_core::bugs::Bug) -> Self {
        lattice_core::bugs::enable(b);
        Planted
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        lattice_core::bugs::clear();
    }
}

/// `count(x)` skips `NULL`s: 3 of the 4 rows have a value, and `count(*)` counts all 4.
fn nulls_are_skipped() -> Result<(), String> {
    let mut f = fixture();
    let mut s = common::admin();
    common::ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE nn (id INT PRIMARY KEY, v INT)",
    );
    common::ok(
        &mut f.db,
        &mut s,
        "INSERT INTO nn VALUES (1, 10), (2, NULL), (3, 20), (4, 30)",
    );
    let got = common::rows(
        &mut f.db,
        &mut s,
        "SELECT count(v), count(*), sum(v) FROM nn",
    );
    if got != [["3", "4", "60"]] {
        return Err(format!("{got:?}"));
    }
    Ok(())
}

#[test]
fn an_aggregate_that_feeds_nulls_to_the_fold_is_caught() {
    assert_eq!(nulls_are_skipped(), Ok(()));
    let _g = Planted::on(Bug::AggregateSkipsNull);
    assert!(nulls_are_skipped().is_err());
}
