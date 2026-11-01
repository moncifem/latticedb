//! `lpad`, `rpad`, `split_part` and `initcap`, checked against PostgreSQL 16: truncation, an empty
//! fill, multi-byte characters, negative field numbers, an empty delimiter, `NULL`, and `split_part`
//! with field zero (`22023`).
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
        "SELECT lpad('hi',5,'xy'), lpad('hi',1,'x'), lpad('hi',0), lpad('hi',-1), lpad('hi',5), lpad('hi',5,'')",
        &["xyxhi|h|||   hi|hi"],
    ),
    (
        "SELECT rpad('hi',5,'xy'), rpad('world',3), rpad('é',3,'ñ')",
        &["hixyx|wor|éññ"],
    ),
    ("SELECT lpad('ab', 3, 'é')", &["éab"]),
    (
        "SELECT split_part('a,b,c',',',2), split_part('a,b,c',',',-1), split_part('a,b,c',',',4), split_part('a,,c',',',2)",
        &["b|c||"],
    ),
    (
        "SELECT split_part('a,b,c',',',-2), split_part('a,b,c',',',-4)",
        &["b|"],
    ),
    (
        "SELECT split_part('abc','',1), split_part('abc','',-1), split_part('abc','',2), split_part('abc','',-2)",
        &["abc|abc||"],
    ),
    (
        "SELECT initcap('hello wORLD'), initcap('a-b c_d e1f'), initcap(''), initcap('élan vital')",
        &["Hello World|A-B C_D E1f||Élan Vital"],
    ),
    (
        "SELECT lpad(NULL, 3), rpad('a', NULL), split_part('a', NULL, 1), initcap(NULL)",
        &["|||"],
    ),
    ("SELECT split_part('a,b',',',0)", &["ERR 22023"]),
    ("SELECT lpad('a',1000000000,'x')", &["ERR 54000"]),
];

#[test]
fn padding_and_splitting_answer_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in STEPS {
        let got: Vec<String> = match s.execute(&mut f.db, sql) {
            Ok(r) => text(&r)
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|c| if c == "NULL" { "" } else { c.as_str() })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect(),
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}
