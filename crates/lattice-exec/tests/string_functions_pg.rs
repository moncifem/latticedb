//! `left`, `right`, `repeat` and `replace` checked against PostgreSQL 16: counts at, below and above
//! the string length, negative counts (all but that many characters from the other end), multibyte
//! characters, `NULL` and empty arguments, overlapping matches, an empty search string, and the size
//! limit on a built string. Each entry lists what PostgreSQL answered (`NULL` and an empty string
//! are both an empty field). Left out: operand type errors (`42883` in PostgreSQL, `42804` here, the
//! documented binder difference), a `bigint` count (refused there, accepted here), `repeat` with a
//! count that does not fit an integer, and `right(s, -2147483648)`, where PostgreSQL's negation
//! overflows and returns the whole string.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};

const CASES: &[(&str, &[&str])] = &[
    ("SELECT left('hello', 0)", &[""]),
    ("SELECT left('hello', 1)", &["h"]),
    ("SELECT left('hello', 2)", &["he"]),
    ("SELECT left('hello', 5)", &["hello"]),
    ("SELECT left('hello', 10)", &["hello"]),
    ("SELECT left('hello', -1)", &["hell"]),
    ("SELECT left('hello', -2)", &["hel"]),
    ("SELECT left('hello', -5)", &[""]),
    ("SELECT left('hello', -10)", &[""]),
    ("SELECT left('héllo', 0)", &[""]),
    ("SELECT left('héllo', 1)", &["h"]),
    ("SELECT left('héllo', 2)", &["hé"]),
    ("SELECT left('héllo', 5)", &["héllo"]),
    ("SELECT left('héllo', 10)", &["héllo"]),
    ("SELECT left('héllo', -1)", &["héll"]),
    ("SELECT left('héllo', -2)", &["hél"]),
    ("SELECT left('héllo', -5)", &[""]),
    ("SELECT left('héllo', -10)", &[""]),
    ("SELECT left('', 0)", &[""]),
    ("SELECT left('', 1)", &[""]),
    ("SELECT left('', 2)", &[""]),
    ("SELECT left('', 5)", &[""]),
    ("SELECT left('', 10)", &[""]),
    ("SELECT left('', -1)", &[""]),
    ("SELECT left('', -2)", &[""]),
    ("SELECT left('', -5)", &[""]),
    ("SELECT left('', -10)", &[""]),
    ("SELECT left('a', 0)", &[""]),
    ("SELECT left('a', 1)", &["a"]),
    ("SELECT left('a', 2)", &["a"]),
    ("SELECT left('a', 5)", &["a"]),
    ("SELECT left('a', 10)", &["a"]),
    ("SELECT left('a', -1)", &[""]),
    ("SELECT left('a', -2)", &[""]),
    ("SELECT left('a', -5)", &[""]),
    ("SELECT left('a', -10)", &[""]),
    ("SELECT right('hello', 0)", &[""]),
    ("SELECT right('hello', 1)", &["o"]),
    ("SELECT right('hello', 2)", &["lo"]),
    ("SELECT right('hello', 5)", &["hello"]),
    ("SELECT right('hello', 10)", &["hello"]),
    ("SELECT right('hello', -1)", &["ello"]),
    ("SELECT right('hello', -2)", &["llo"]),
    ("SELECT right('hello', -5)", &[""]),
    ("SELECT right('hello', -10)", &[""]),
    ("SELECT right('héllo', 0)", &[""]),
    ("SELECT right('héllo', 1)", &["o"]),
    ("SELECT right('héllo', 2)", &["lo"]),
    ("SELECT right('héllo', 5)", &["héllo"]),
    ("SELECT right('héllo', 10)", &["héllo"]),
    ("SELECT right('héllo', -1)", &["éllo"]),
    ("SELECT right('héllo', -2)", &["llo"]),
    ("SELECT right('héllo', -5)", &[""]),
    ("SELECT right('héllo', -10)", &[""]),
    ("SELECT right('', 0)", &[""]),
    ("SELECT right('', 1)", &[""]),
    ("SELECT right('', 2)", &[""]),
    ("SELECT right('', 5)", &[""]),
    ("SELECT right('', 10)", &[""]),
    ("SELECT right('', -1)", &[""]),
    ("SELECT right('', -2)", &[""]),
    ("SELECT right('', -5)", &[""]),
    ("SELECT right('', -10)", &[""]),
    ("SELECT right('a', 0)", &[""]),
    ("SELECT right('a', 1)", &["a"]),
    ("SELECT right('a', 2)", &["a"]),
    ("SELECT right('a', 5)", &["a"]),
    ("SELECT right('a', 10)", &["a"]),
    ("SELECT right('a', -1)", &[""]),
    ("SELECT right('a', -2)", &[""]),
    ("SELECT right('a', -5)", &[""]),
    ("SELECT right('a', -10)", &[""]),
    ("SELECT left(NULL, 1)", &[""]),
    ("SELECT left('abc', NULL)", &[""]),
    ("SELECT right('abc', NULL)", &[""]),
    ("SELECT left('abc')", &["ERR 42883"]),
    ("SELECT left('abc', 'x')", &["ERR 22P02"]),
    ("SELECT left('abc', 2147483647)", &["abc"]),
    ("SELECT left('abc', -2147483648)", &[""]),
    ("SELECT repeat('ab', 0)", &[""]),
    ("SELECT repeat('ab', 1)", &["ab"]),
    ("SELECT repeat('ab', 3)", &["ababab"]),
    ("SELECT repeat('', 5)", &[""]),
    ("SELECT repeat('é', 3)", &["ééé"]),
    ("SELECT repeat('ab', -1)", &[""]),
    ("SELECT repeat('ab', -100)", &[""]),
    ("SELECT repeat(NULL, 1)", &[""]),
    ("SELECT repeat('ab', NULL)", &[""]),
    ("SELECT length(repeat('ab', 1000))", &["2000"]),
    ("SELECT repeat('abc', 400000000)", &["ERR 54000"]),
    ("SELECT repeat('ab')", &["ERR 42883"]),
    ("SELECT replace('hello', 'l', 'L')", &["heLLo"]),
    ("SELECT replace('hello', 'll', '')", &["heo"]),
    ("SELECT replace('hello', '', 'x')", &["hello"]),
    ("SELECT replace('hello', 'x', 'y')", &["hello"]),
    ("SELECT replace('aaa', 'a', 'aa')", &["aaaaaa"]),
    ("SELECT replace('aaa', 'aa', 'b')", &["ba"]),
    ("SELECT replace('', 'a', 'b')", &[""]),
    ("SELECT replace('abc', 'abc', 'xyz')", &["xyz"]),
    ("SELECT replace('héllo', 'é', 'e')", &["hello"]),
    ("SELECT replace(NULL, 'a', 'b')", &[""]),
    ("SELECT replace('a', NULL, 'b')", &[""]),
    ("SELECT replace('a', 'a', NULL)", &[""]),
    ("SELECT replace('abab', 'ab', 'ba')", &["baba"]),
    ("SELECT replace('abcabc', 'bc', 'bcbc')", &["abcbcabcbc"]),
    ("SELECT replace('a', 'b')", &["ERR 42883"]),
    (
        "SELECT length(replace(repeat('a', 1000), 'a', repeat('b', 1000)))",
        &["1000000"],
    ),
    (
        "SELECT replace(repeat('a', 5000000), 'a', repeat('b', 5000))",
        &["ERR 54000"],
    ),
];

#[test]
fn string_functions_answer_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in CASES {
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
