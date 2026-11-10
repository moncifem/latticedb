//! `INSERT ... SELECT` and `UPDATE` with expressions in one session, checked against PostgreSQL 16:
//! column lists, defaults for omitted columns, `NOT NULL` and key violations halfway through a
//! statement, arity errors, `ORDER BY` and `LIMIT` in the source, `RETURNING`, a self-referencing source,
//! and updates that read the old row. Each step lists what PostgreSQL answered: rows then the command
//! tag for a data change, or `ERR` and the SQLSTATE. Left out: assigning an integer column to a text
//! column (`42804` here, a documented stricter rule), `count(*)`, and `DEFAULT`, subqueries, `FROM` and
//! `USING` in `UPDATE` and `DELETE` (not executed yet).
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
        "CREATE TABLE src (id INT PRIMARY KEY, a INT, b TEXT)",
        &["CREATE TABLE"],
    ),
    (
        "CREATE TABLE dst (id INT PRIMARY KEY, a INT NOT NULL, b TEXT, c INT DEFAULT 9)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO src VALUES (1, 10, 'x'), (2, 20, 'y'), (3, NULL, 'z'), (4, 40, NULL)",
        &["INSERT 0 4"],
    ),
    (
        "INSERT INTO dst SELECT id, a, b, 0 FROM src WHERE a IS NOT NULL",
        &["INSERT 0 3"],
    ),
    (
        "SELECT id, a, b, c FROM dst ORDER BY id",
        &["1|10|x|0", "2|20|y|0", "4|40||0"],
    ),
    (
        "INSERT INTO dst (id, a) SELECT id + 100, a FROM src WHERE id <= 2",
        &["INSERT 0 2"],
    ),
    (
        "SELECT id, a, b, c FROM dst ORDER BY id",
        &["1|10|x|0", "2|20|y|0", "4|40||0", "101|10||9", "102|20||9"],
    ),
    (
        "INSERT INTO dst SELECT id, a, b, 0 FROM src",
        &["ERR 23505"],
    ),
    (
        "INSERT INTO dst (id, a) SELECT id, a FROM src WHERE id = 1",
        &["ERR 23505"],
    ),
    (
        "INSERT INTO dst (id, a) SELECT id, a FROM src WHERE id = 999",
        &["INSERT 0 0"],
    ),
    ("INSERT INTO dst (id, a) SELECT id FROM src", &["ERR 42601"]),
    (
        "INSERT INTO dst (id, a) SELECT id, a, b FROM src",
        &["ERR 42601"],
    ),
    (
        "INSERT INTO dst (id) SELECT id FROM src WHERE id = 3",
        &["ERR 23502"],
    ),
    (
        "INSERT INTO dst (id, a, b) SELECT 500, 1, 'q' RETURNING id, a, b, c",
        &["500|1|q|9", "INSERT 0 1"],
    ),
    (
        "INSERT INTO dst (id, a) SELECT id + 600, a FROM src WHERE a IS NOT NULL ORDER BY id DESC LIMIT 2 RETURNING id",
        &["604", "602", "INSERT 0 2"],
    ),
    (
        "SELECT id FROM dst ORDER BY id",
        &["1", "2", "4", "101", "102", "500", "602", "604"],
    ),
    (
        "INSERT INTO dst (id, a) SELECT id + 700, a * 2 FROM src WHERE id = 1",
        &["INSERT 0 1"],
    ),
    (
        "INSERT INTO dst (id, a) SELECT id + 900, 'x' FROM src WHERE id = 1",
        &["ERR 22P02"],
    ),
    (
        "INSERT INTO dst (id, a) SELECT id + 1000, a FROM dst WHERE id = 1",
        &["INSERT 0 1"],
    ),
    (
        "INSERT INTO dst (id, a) SELECT id, a FROM dst",
        &["ERR 23505"],
    ),
    (
        "UPDATE dst SET a = a + 1, b = b || '!' WHERE id <= 2 RETURNING id, a, b",
        &["1|11|x!", "2|21|y!", "UPDATE 2"],
    ),
    (
        "UPDATE dst SET a = id * 2 WHERE id = 1 RETURNING a",
        &["2", "UPDATE 1"],
    ),
    (
        "UPDATE dst SET b = NULL WHERE id = 1 RETURNING b",
        &["", "UPDATE 1"],
    ),
    (
        "UPDATE dst SET c = c + a WHERE id = 2 RETURNING c",
        &["21", "UPDATE 1"],
    ),
    (
        "UPDATE dst SET a = a, id = id WHERE id = 2 RETURNING id, a",
        &["2|21", "UPDATE 1"],
    ),
    ("UPDATE dst SET a = NULL WHERE id = 2", &["ERR 23502"]),
];

#[test]
fn insert_select_and_update_answer_as_postgresql_does() {
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
