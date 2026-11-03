//! `JOIN ... ON`, `INNER JOIN`, `CROSS JOIN` and comma-separated `FROM` lists in one session, checked
//! against PostgreSQL 16: equality and non-equality conditions, a self join through aliases, three
//! tables, `NULL` keys that match nothing, `*` and `t.*` over a join, and aggregates, `DISTINCT` and
//! `WHERE` above one. Errors: an unqualified column both tables have (`42702`) and one table name used
//! twice (`42712`). `LEFT JOIN`: unmatched rows, a condition in `ON` against one in `WHERE`, `count` of the
//! right side, chains and an inner join after a left one.
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
        "CREATE TABLE a (id INT PRIMARY KEY, x INT, t TEXT)",
        &["CREATE TABLE"],
    ),
    (
        "CREATE TABLE b (id INT PRIMARY KEY, a_id INT, y INT)",
        &["CREATE TABLE"],
    ),
    (
        "CREATE TABLE c (id INT PRIMARY KEY, b_id INT)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO a VALUES (1,10,'p'),(2,20,'q'),(3,NULL,'r')",
        &["INSERT 0 3"],
    ),
    (
        "INSERT INTO b VALUES (1,1,100),(2,1,200),(3,2,300),(4,NULL,400),(5,9,500)",
        &["INSERT 0 5"],
    ),
    ("INSERT INTO c VALUES (1,1),(2,3),(3,3)", &["INSERT 0 3"]),
    (
        "SELECT a.id, b.id FROM a JOIN b ON a.id = b.a_id ORDER BY 1,2",
        &["1|1", "1|2", "2|3"],
    ),
    (
        "SELECT a.t, b.y FROM a INNER JOIN b ON b.a_id = a.id WHERE b.y > 100 ORDER BY 2",
        &["p|200", "q|300"],
    ),
    (
        "SELECT a.id, b.id FROM a, b WHERE a.id = b.a_id ORDER BY 1,2",
        &["1|1", "1|2", "2|3"],
    ),
    ("SELECT count(*) FROM a CROSS JOIN b", &["15"]),
    ("SELECT count(*) FROM a, b", &["15"]),
    (
        "SELECT a.id, b.id, c.id FROM a JOIN b ON a.id=b.a_id JOIN c ON c.b_id=b.id ORDER BY 1,2,3",
        &["1|1|1", "2|3|2", "2|3|3"],
    ),
    (
        "SELECT x.id, y.id FROM a x JOIN a y ON x.x < y.x ORDER BY 1,2",
        &["1|2"],
    ),
    (
        "SELECT a.id, b.y FROM a JOIN b ON a.x = b.y/10 ORDER BY 1,2",
        &["1|100", "2|200"],
    ),
    (
        "SELECT a.id, b.id FROM a JOIN b ON a.id = b.a_id AND b.y > 100 ORDER BY 1,2",
        &["1|2", "2|3"],
    ),
    (
        "SELECT a.id, sum(b.y) FROM a JOIN b ON a.id=b.a_id GROUP BY a.id ORDER BY 1",
        &["1|300", "2|300"],
    ),
    (
        "SELECT DISTINCT a.t FROM a JOIN b ON a.id=b.a_id ORDER BY 1",
        &["p", "q"],
    ),
    (
        "SELECT * FROM a JOIN b ON a.id = b.a_id ORDER BY a.id, b.id",
        &["1|10|p|1|1|100", "1|10|p|2|1|200", "2|20|q|3|2|300"],
    ),
    (
        "SELECT a.* FROM a JOIN b ON a.id = b.a_id ORDER BY a.id, b.id",
        &["1|10|p", "1|10|p", "2|20|q"],
    ),
    ("SELECT id FROM a JOIN b ON a.id = b.a_id", &["ERR 42702"]),
    ("SELECT 1 FROM a JOIN a ON true", &["ERR 42712"]),
    ("SELECT 1 FROM a, b JOIN a ON true", &["ERR 42712"]),
    (
        "SELECT a.id FROM a LEFT JOIN b ON a.id = b.a_id WHERE a.x > 10 ORDER BY 1",
        &["2"],
    ),
    (
        "SELECT b.* FROM a LEFT JOIN b ON a.id = b.a_id WHERE a.id = 3",
        &["||"],
    ),
    (
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.id = b.a_id ORDER BY 1,2",
        &["1|1", "1|2", "2|3", "3|"],
    ),
    (
        "SELECT a.id, b.id FROM a LEFT OUTER JOIN b ON a.id = b.a_id AND b.y > 100 ORDER BY 1,2",
        &["1|2", "2|3", "3|"],
    ),
    (
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.id = b.a_id WHERE b.id IS NULL ORDER BY 1",
        &["3|"],
    ),
    (
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.id = b.a_id WHERE b.y > 100 ORDER BY 1,2",
        &["1|2", "2|3"],
    ),
    (
        "SELECT a.id, count(b.id) FROM a LEFT JOIN b ON a.id = b.a_id GROUP BY a.id ORDER BY 1",
        &["1|2", "2|1", "3|0"],
    ),
    (
        "SELECT a.id, b.id, c.id FROM a LEFT JOIN b ON a.id=b.a_id LEFT JOIN c ON c.b_id=b.id ORDER BY 1,2,3",
        &["1|1|1", "1|2|", "2|3|2", "2|3|3", "3||"],
    ),
    (
        "SELECT a.id, b.id, c.id FROM a LEFT JOIN b ON a.id=b.a_id JOIN c ON c.b_id=b.id ORDER BY 1,2,3",
        &["1|1|1", "2|3|2", "2|3|3"],
    ),
    (
        "SELECT * FROM a LEFT JOIN b ON false ORDER BY a.id",
        &["1|10|p|||", "2|20|q|||", "3||r|||"],
    ),
    (
        "SELECT a.id FROM a LEFT JOIN b ON a.id = 99 ORDER BY 1",
        &["1", "2", "3"],
    ),
];

#[test]
fn inner_and_cross_joins_answer_as_postgresql_does() {
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
