//! `ILIKE` and `NOT ILIKE` over ASCII text checked against PostgreSQL 16 in one session: case folding on
//! both sides, `%` and `_`, escapes with `\\`, `NULL` operands, an expression pattern, a column against
//! itself. Each entry lists what PostgreSQL answered (`NULL` is an empty field). Left out: operand type
//! errors (`42883` there, `42804` here, the documented binder difference) and `ESCAPE` (refused here).
//! Non-ASCII letters are not tested: this server folds case with Unicode `lower` on both sides, while
//! PostgreSQL follows the database locale (only ASCII under the `C` collation).
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
        "CREATE TABLE il (id INT PRIMARY KEY, s TEXT)",
        &["CREATE TABLE"],
    ),
    (
        "INSERT INTO il VALUES (1, 'Hello'), (2, 'hello'), (3, 'HELLO'), (4, 'World'), (5, NULL), (6, ''), (7, 'a%b'), (8, 'A_B'), (9, 'abc')",
        &["INSERT 0 9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE 'hello' ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'hello' ORDER BY id",
        &["4", "6", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE 'HELLO' ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'HELLO' ORDER BY id",
        &["4", "6", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE 'h%' ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'h%' ORDER BY id",
        &["4", "6", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE 'H%' ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'H%' ORDER BY id",
        &["4", "6", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE '%LL%' ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE '%LL%' ORDER BY id",
        &["4", "6", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE '_ello' ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE '_ello' ORDER BY id",
        &["4", "6", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE '%O' ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE '%O' ORDER BY id",
        &["4", "6", "7", "8", "9"],
    ),
    ("SELECT id FROM il WHERE s ILIKE 'w%d' ORDER BY id", &["4"]),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'w%d' ORDER BY id",
        &["1", "2", "3", "6", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE '%' ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "8", "9"],
    ),
    ("SELECT id FROM il WHERE s NOT ILIKE '%' ORDER BY id", &[]),
    ("SELECT id FROM il WHERE s ILIKE '' ORDER BY id", &["6"]),
    (
        "SELECT id FROM il WHERE s NOT ILIKE '' ORDER BY id",
        &["1", "2", "3", "4", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE 'a%B' ORDER BY id",
        &["7", "8"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'a%B' ORDER BY id",
        &["1", "2", "3", "4", "6", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE 'a_b' ORDER BY id",
        &["7", "8"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'a_b' ORDER BY id",
        &["1", "2", "3", "4", "6", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE 'A\\%B' ORDER BY id",
        &["7"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'A\\%B' ORDER BY id",
        &["1", "2", "3", "4", "6", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE 'a\\_b' ORDER BY id",
        &["8"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'a\\_b' ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE '%l_o' ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "SELECT id FROM il WHERE s NOT ILIKE '%l_o' ORDER BY id",
        &["4", "6", "7", "8", "9"],
    ),
    ("SELECT id FROM il WHERE s ILIKE 'hell' ORDER BY id", &[]),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'hell' ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "8", "9"],
    ),
    ("SELECT id FROM il WHERE s ILIKE '%X%' ORDER BY id", &[]),
    (
        "SELECT id FROM il WHERE s NOT ILIKE '%X%' ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "8", "9"],
    ),
    ("SELECT id FROM il WHERE s ILIKE 'abc' ORDER BY id", &["9"]),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'abc' ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "8"],
    ),
    ("SELECT id FROM il WHERE s ILIKE 'ABC' ORDER BY id", &["9"]),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'ABC' ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "8"],
    ),
    ("SELECT id FROM il WHERE s ILIKE 'A%C' ORDER BY id", &["9"]),
    (
        "SELECT id FROM il WHERE s NOT ILIKE 'A%C' ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "8"],
    ),
    ("SELECT 'ABC' ILIKE 'abc'", &["t"]),
    ("SELECT 'abc' ILIKE 'ABC'", &["t"]),
    ("SELECT 'abc' NOT ILIKE 'ABC'", &["f"]),
    ("SELECT NULL ILIKE 'a'", &[""]),
    ("SELECT 'a' ILIKE NULL", &[""]),
    ("SELECT 'a' ILIKE NULL::text", &[""]),
    ("SELECT 'Ab' ILIKE 'a' || 'B'", &["t"]),
    ("SELECT 'ab' ILIKE 'A%' AND 'x' ILIKE 'X'", &["t"]),
    (
        "SELECT id FROM il WHERE lower(s) = 'hello' OR s ILIKE 'W%' ORDER BY id",
        &["1", "2", "3", "4"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE s ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "8", "9"],
    ),
    (
        "SELECT id FROM il WHERE s ILIKE upper(s) ORDER BY id",
        &["1", "2", "3", "4", "6", "7", "8", "9"],
    ),
];

#[test]
fn ilike_answers_as_postgresql_does() {
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
                if r.columns.is_empty() {
                    lines.push(r.tag);
                }
                lines
            }
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}
