//! `CASE` (searched and simple) checked against PostgreSQL 16 in one session: the first true branch
//! wins, a `NULL` condition is not true, no `ELSE` gives `NULL`, the result types unify, the simple form
//! compares with `=` (so `NULL` never matches), only the chosen branch is evaluated when its operands
//! read table columns, and `CASE` works in `WHERE`, `ORDER BY`, `UPDATE`, `INSERT` and `RETURNING`.
//! Each entry lists what PostgreSQL answered (`NULL` is an empty field). Left out: three statements
//! whose skipped branch is the constant `1 / 0` (the binder folds constant integer arithmetic, so they
//! fail here and not in PostgreSQL, a documented difference) and `CASE NULL WHEN 1` (`42883` there,
//! `42804` here).
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
        "CREATE TABLE tv (id INT PRIMARY KEY, a INT, b INT, s TEXT)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO tv VALUES (1, 1, 1, 'x'), (2, 1, 2, 'y'), (3, NULL, 1, NULL), (4, 1, NULL, 'x'), (5, NULL, NULL, 'z'), (6, 2, NULL, 'y')",
        &["INSERT 0 6"],
    ),
    ("SELECT CASE WHEN true THEN 1 ELSE 2 END", &["1"]),
    ("SELECT CASE WHEN false THEN 1 ELSE 2 END", &["2"]),
    ("SELECT CASE WHEN NULL THEN 1 ELSE 2 END", &["2"]),
    ("SELECT CASE WHEN false THEN 1 END", &[""]),
    ("SELECT CASE WHEN true THEN NULL ELSE 2 END", &[""]),
    (
        "SELECT CASE WHEN 1 = 1 THEN 'a' WHEN 1 = 1 THEN 'b' END",
        &["a"],
    ),
    (
        "SELECT CASE WHEN 1 = 2 THEN 'a' WHEN 2 = 2 THEN 'b' ELSE 'c' END",
        &["b"],
    ),
    (
        "SELECT CASE WHEN 1 = 2 THEN 'a' WHEN 2 = 3 THEN 'b' ELSE 'c' END",
        &["c"],
    ),
    (
        "SELECT CASE 1 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END",
        &["one"],
    ),
    (
        "SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END",
        &["two"],
    ),
    (
        "SELECT CASE 3 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END",
        &["many"],
    ),
    (
        "SELECT CASE 1 WHEN NULL THEN 'n' ELSE 'other' END",
        &["other"],
    ),
    (
        "SELECT CASE NULL WHEN NULL THEN 'n' ELSE 'other' END",
        &["other"],
    ),
    ("SELECT CASE 'a' WHEN 'a' THEN 1 ELSE 0 END", &["1"]),
    (
        "SELECT CASE WHEN 1 / 0 > 0 THEN 1 ELSE 2 END",
        &["ERR 22012"],
    ),
    (
        "SELECT CASE WHEN false THEN 1 WHEN 1 / 0 > 0 THEN 2 END",
        &["ERR 22012"],
    ),
    ("SELECT CASE WHEN true THEN 1 ELSE 2.5 END", &["1"]),
    ("SELECT CASE WHEN false THEN 1 ELSE 2.5 END", &["2.5"]),
    ("SELECT CASE WHEN true THEN 1 ELSE 2.5::float8 END", &["1"]),
    ("SELECT CASE WHEN false THEN 1::bigint ELSE 2 END", &["2"]),
    ("SELECT CASE WHEN true THEN 'a' ELSE 1 END", &["ERR 22P02"]),
    ("SELECT CASE WHEN true THEN 1 ELSE 'a' END", &["ERR 22P02"]),
    ("SELECT CASE WHEN true THEN 1 ELSE true END", &["ERR 42804"]),
    ("SELECT CASE WHEN true THEN 'a' ELSE 'b'::text END", &["a"]),
    (
        "SELECT CASE WHEN true THEN '2024-01-01'::date ELSE '2024-02-01'::date END",
        &["2024-01-01"],
    ),
    ("SELECT CASE WHEN true THEN NULL ELSE NULL END", &[""]),
    ("SELECT CASE WHEN 1 THEN 1 END", &["ERR 42804"]),
    ("SELECT CASE WHEN 'x' THEN 1 END", &["ERR 22P02"]),
    ("SELECT CASE WHEN true THEN 1 END + 1", &["2"]),
    ("SELECT 1 + CASE WHEN true THEN 1 ELSE 2 END", &["2"]),
    (
        "SELECT CASE WHEN CASE WHEN true THEN false ELSE true END THEN 1 ELSE 2 END",
        &["2"],
    ),
    (
        "SELECT CASE WHEN true THEN CASE WHEN false THEN 1 ELSE 2 END ELSE 3 END",
        &["2"],
    ),
    ("SELECT CASE", &["ERR 42601"]),
    ("SELECT CASE END", &["ERR 42601"]),
    ("SELECT CASE WHEN true THEN 1", &["ERR 42601"]),
    ("SELECT CASE WHEN true 1 END", &["ERR 42601"]),
    ("SELECT CASE WHEN true THEN END", &["ERR 42601"]),
    ("SELECT CASE ELSE 1 END", &["ERR 42601"]),
    ("SELECT CASE 1 END", &["ERR 42601"]),
    ("SELECT case when true then 1 else 2 end", &["1"]),
    ("SELECT CASE WHEN true THEN 1 ELSE 2 END IS NULL", &["f"]),
    (
        "SELECT CASE WHEN NULL IS NULL THEN 'null' ELSE 'not' END",
        &["null"],
    ),
    ("SELECT coalesce(CASE WHEN false THEN 1 END, 5)", &["5"]),
    ("SELECT CASE WHEN true THEN 1 ELSE 2 END::text", &["1"]),
    (
        "SELECT CASE WHEN true THEN 'a' || 'b' ELSE 'c' END",
        &["ab"],
    ),
    (
        "SELECT length(CASE WHEN true THEN 'abc' ELSE 'd' END)",
        &["3"],
    ),
    ("SELECT 1", &["1"]),
    (
        "SELECT id, CASE WHEN a IS NULL THEN 0 ELSE a END FROM tv ORDER BY id",
        &["1|1", "2|1", "3|0", "4|1", "5|0", "6|2"],
    ),
    (
        "SELECT id, CASE WHEN a = b THEN 'eq' WHEN a <> b THEN 'ne' ELSE 'unknown' END FROM tv ORDER BY id",
        &[
            "1|eq",
            "2|ne",
            "3|unknown",
            "4|unknown",
            "5|unknown",
            "6|unknown",
        ],
    ),
    (
        "SELECT id, CASE WHEN a = b THEN 'eq' WHEN a <> b THEN 'ne' END FROM tv ORDER BY id",
        &["1|eq", "2|ne", "3|", "4|", "5|", "6|"],
    ),
    (
        "SELECT id, CASE a WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END FROM tv ORDER BY id",
        &["1|one", "2|one", "3|other", "4|one", "5|other", "6|two"],
    ),
    (
        "SELECT id, CASE s WHEN 'x' THEN 1 WHEN 'y' THEN 2 END FROM tv ORDER BY id",
        &["1|1", "2|2", "3|", "4|1", "5|", "6|2"],
    ),
    (
        "SELECT id, CASE a WHEN b THEN 'same' ELSE 'diff' END FROM tv ORDER BY id",
        &["1|same", "2|diff", "3|diff", "4|diff", "5|diff", "6|diff"],
    ),
    (
        "SELECT id FROM tv WHERE CASE WHEN a IS NULL THEN false ELSE a > 1 END ORDER BY id",
        &["6"],
    ),
    (
        "SELECT id FROM tv WHERE CASE WHEN a IS NULL THEN true ELSE false END ORDER BY id",
        &["3", "5"],
    ),
    (
        "SELECT id FROM tv ORDER BY CASE WHEN a IS NULL THEN 1 ELSE 0 END, id DESC",
        &["6", "4", "2", "1", "5", "3"],
    ),
    (
        "SELECT id, CASE WHEN b = 0 THEN NULL ELSE 10 / b END FROM tv WHERE b IS NOT NULL ORDER BY id",
        &["1|10", "2|5", "3|10"],
    ),
    (
        "SELECT id, CASE WHEN b IS NULL OR b = 0 THEN 0 ELSE 10 / b END FROM tv ORDER BY id",
        &["1|10", "2|5", "3|10", "4|0", "5|0", "6|0"],
    ),
    (
        "SELECT id, CASE WHEN id < 3 THEN 10 / (id - 1) ELSE 0 END FROM tv ORDER BY id",
        &["ERR 22012"],
    ),
    (
        "SELECT id, CASE WHEN id > 3 THEN 10 / (id - 1) ELSE 0 END FROM tv ORDER BY id",
        &["1|0", "2|0", "3|0", "4|3", "5|2", "6|2"],
    ),
    (
        "SELECT id, CASE WHEN id = 1 THEN a ELSE b END FROM tv ORDER BY id",
        &["1|1", "2|2", "3|1", "4|", "5|", "6|"],
    ),
    (
        "SELECT id, CASE WHEN a IS NULL THEN s ELSE 'a' END FROM tv ORDER BY id",
        &["1|a", "2|a", "3|", "4|a", "5|z", "6|a"],
    ),
    (
        "SELECT id, CASE WHEN a IS NULL THEN b ELSE a END * 2 FROM tv ORDER BY id",
        &["1|2", "2|2", "3|2", "4|2", "5|", "6|4"],
    ),
    (
        "SELECT id, CASE WHEN a IS NULL THEN 1.5 ELSE a END FROM tv ORDER BY id",
        &["1|1", "2|1", "3|1.5", "4|1", "5|1.5", "6|2"],
    ),
    (
        "SELECT id FROM tv WHERE CASE WHEN a = 1 THEN b = 1 ELSE s = 'z' END ORDER BY id",
        &["1", "5"],
    ),
    (
        "SELECT id FROM tv WHERE id = CASE WHEN true THEN 3 END",
        &["3"],
    ),
    (
        "SELECT id FROM tv WHERE id = CASE WHEN a IS NULL THEN 3 ELSE 1 END ORDER BY id",
        &["1", "3"],
    ),
    (
        "UPDATE tv SET b = CASE WHEN b IS NULL THEN 0 ELSE b + 100 END RETURNING id, b",
        &["1|101", "2|102", "3|101", "4|0", "5|0", "6|0", "UPDATE 6"],
    ),
    (
        "INSERT INTO tv VALUES (7, CASE WHEN true THEN 5 END, 5, CASE WHEN false THEN 'q' ELSE 'r' END) RETURNING id, a, s",
        &["7|5|r", "INSERT 0 1"],
    ),
    (
        "DELETE FROM tv WHERE CASE WHEN a IS NULL THEN true ELSE false END RETURNING id",
        &["3", "5", "DELETE 2"],
    ),
];

#[test]
fn case_answers_as_postgresql_does() {
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
                if r.columns.is_empty()
                    || sql.starts_with("INSERT")
                    || sql.starts_with("UPDATE")
                    || sql.starts_with("DELETE")
                {
                    lines.push(r.tag);
                }
                lines
            }
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}
