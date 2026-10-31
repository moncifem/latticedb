//! `SELECT DISTINCT` in one session, checked against PostgreSQL 16: one column and several, `NULL`s
//! equal to each other, expressions, `*`, constants, aggregates and `GROUP BY` under it, `ORDER BY`
//! with `NULLS FIRST/LAST`, `LIMIT` and `OFFSET`, empty inputs, and the rule that an `ORDER BY`
//! expression must appear in the select list (`42P10`). Each step lists what PostgreSQL answered (`NULL`
//! is an empty field) in the order of an explicit `ORDER BY`, because the order of distinct rows without
//! one is not defined. Left out: `SELECT ALL` and `DISTINCT ON` (not built) and an `INSERT ... SELECT
//! DISTINCT` whose row order PostgreSQL does not fix.
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
        "CREATE TABLE gr (id INT PRIMARY KEY, g INT, h TEXT, v INT, n NUMERIC, f DOUBLE PRECISION)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO gr VALUES (1, 1, 'a', 10, 1.5, 1.0), (2, 1, 'b', 20, 1.50, NULL), (3, 2, 'a', 10, NULL, 3.0), (4, 2, 'a', NULL, 4.0, 3.0), (5, NULL, 'b', 50, 5.0, 5.0), (6, NULL, 'b', 50, 5.00, NULL), (7, 3, NULL, 70, 7.5, 7.0), (8, 3, NULL, NULL, NULL, NULL)",
        &["INSERT 0 8"],
    ),
    (
        "CREATE TABLE em (id INT PRIMARY KEY, g INT)",
        &["CREATE TABLE"],
    ),
    ("SELECT DISTINCT g FROM gr ORDER BY g", &["1", "2", "3", ""]),
    (
        "SELECT DISTINCT g FROM gr ORDER BY g DESC",
        &["", "3", "2", "1"],
    ),
    (
        "SELECT DISTINCT g FROM gr ORDER BY g NULLS FIRST",
        &["", "1", "2", "3"],
    ),
    ("SELECT DISTINCT h FROM gr ORDER BY h", &["a", "b", ""]),
    (
        "SELECT DISTINCT g, h FROM gr ORDER BY g, h",
        &["1|a", "1|b", "2|a", "3|", "|b"],
    ),
    (
        "SELECT DISTINCT g, h FROM gr ORDER BY h NULLS FIRST, g NULLS FIRST",
        &["3|", "1|a", "2|a", "|b", "1|b"],
    ),
    (
        "SELECT DISTINCT v FROM gr ORDER BY v",
        &["10", "20", "50", "70", ""],
    ),
    (
        "SELECT DISTINCT n FROM gr ORDER BY n",
        &["1.5", "4.0", "5.0", "7.5", ""],
    ),
    (
        "SELECT DISTINCT f FROM gr ORDER BY f",
        &["1", "3", "5", "7", ""],
    ),
    (
        "SELECT DISTINCT g + 1 FROM gr ORDER BY 1",
        &["2", "3", "4", ""],
    ),
    (
        "SELECT DISTINCT g + 1 AS k FROM gr ORDER BY k",
        &["2", "3", "4", ""],
    ),
    (
        "SELECT DISTINCT g + 1 FROM gr ORDER BY g + 1",
        &["2", "3", "4", ""],
    ),
    (
        "SELECT DISTINCT upper(h) FROM gr ORDER BY 1",
        &["A", "B", ""],
    ),
    (
        "SELECT DISTINCT g FROM gr ORDER BY 1 DESC LIMIT 2",
        &["", "3"],
    ),
    (
        "SELECT DISTINCT g FROM gr ORDER BY g LIMIT 2 OFFSET 1",
        &["2", "3"],
    ),
    ("SELECT DISTINCT g FROM gr ORDER BY g LIMIT 0", &[]),
    (
        "SELECT DISTINCT g FROM gr WHERE v IS NOT NULL ORDER BY g",
        &["1", "2", "3", ""],
    ),
    ("SELECT DISTINCT g FROM gr WHERE id > 100", &[]),
    ("SELECT DISTINCT g FROM em", &[]),
    ("SELECT DISTINCT 1 FROM gr", &["1"]),
    ("SELECT DISTINCT 1, 'a' FROM gr", &["1|a"]),
    ("SELECT DISTINCT NULL FROM gr", &[""]),
    (
        "SELECT DISTINCT * FROM gr ORDER BY id",
        &[
            "1|1|a|10|1.5|1",
            "2|1|b|20|1.50|",
            "3|2|a|10||3",
            "4|2|a||4.0|3",
            "5||b|50|5.0|5",
            "6||b|50|5.00|",
            "7|3||70|7.5|7",
            "8|3||||",
        ],
    ),
    (
        "SELECT DISTINCT g, v FROM gr ORDER BY 1, 2",
        &["1|10", "1|20", "2|10", "2|", "3|70", "3|", "|50"],
    ),
    (
        "SELECT DISTINCT g AS a, g AS b FROM gr ORDER BY a",
        &["1|1", "2|2", "3|3", "|"],
    ),
    ("SELECT DISTINCT g FROM gr ORDER BY v", &["ERR 42P10"]),
    ("SELECT DISTINCT g FROM gr ORDER BY g + 1", &["ERR 42P10"]),
    ("SELECT DISTINCT g FROM gr ORDER BY id", &["ERR 42P10"]),
    ("SELECT DISTINCT g FROM gr ORDER BY 2", &["ERR 42P10"]),
    ("SELECT DISTINCT g FROM gr ORDER BY nope", &["ERR 42703"]),
    ("SELECT DISTINCT count(*) FROM gr", &["8"]),
    (
        "SELECT DISTINCT count(*) FROM gr GROUP BY g ORDER BY 1",
        &["2"],
    ),
    (
        "SELECT DISTINCT g, count(*) FROM gr GROUP BY g ORDER BY g",
        &["1|2", "2|2", "3|2", "|2"],
    ),
    (
        "SELECT DISTINCT h, count(*) FROM gr GROUP BY h ORDER BY h",
        &["a|3", "b|3", "|2"],
    ),
    (
        "SELECT DISTINCT sum(v) > 10 FROM gr GROUP BY g ORDER BY 1",
        &["f", "t"],
    ),
    (
        "SELECT DISTINCT count(*) FROM gr GROUP BY g ORDER BY count(*) DESC",
        &["2"],
    ),
    (
        "SELECT DISTINCT g FROM gr GROUP BY g, h ORDER BY g",
        &["1", "2", "3", ""],
    ),
    (
        "SELECT DISTINCT max(v) FROM gr GROUP BY g HAVING count(*) > 1 ORDER BY 1",
        &["10", "20", "50", "70"],
    ),
    ("SELECT DISTINCT count(*) FROM gr WHERE id > 100", &["0"]),
    (
        "SELECT DISTINCT g, h FROM gr ORDER BY g, h LIMIT 3",
        &["1|a", "1|b", "2|a"],
    ),
    (
        "SELECT DISTINCT lower(h), g FROM gr ORDER BY 1, 2",
        &["a|1", "a|2", "b|1", "b|", "|3"],
    ),
    ("SELECT DISTINCT id % 2 FROM gr ORDER BY 1", &["0", "1"]),
    ("SELECT DISTINCT g IS NULL FROM gr ORDER BY 1", &["f", "t"]),
    (
        "SELECT DISTINCT (g IS NULL), (h IS NULL) FROM gr ORDER BY 1, 2",
        &["f|f", "f|t", "t|f"],
    ),
];

#[test]
fn select_distinct_answers_as_postgresql_does() {
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
