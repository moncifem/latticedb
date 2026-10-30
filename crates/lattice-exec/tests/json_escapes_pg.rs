//! Escapes inside JSON strings, checked against PostgreSQL 17: a surrogate escape must be paired (a high
//! one followed by a low one), a low one alone or a high one followed by anything else is refused with
//! `22P02`, and a NUL escape is refused with `22P05`. The documents are assembled from pieces so that no
//! escape is typed as a literal in this file: `T` is plain text, `U` is a backslash-u escape of four hex
//! digits.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{admin, fixture, text};

enum Piece {
    T(&'static str),
    U(&'static str),
}
use Piece::{T, U};

fn document(pieces: &[Piece]) -> String {
    let backslash = char::from(92u8);
    let mut out = String::new();
    for piece in pieces {
        match piece {
            T(t) => out.push_str(t),
            U(hex) => {
                out.push(backslash);
                out.push('u');
                out.push_str(hex);
            }
        }
    }
    out
}

/// (the whole JSON document as pieces, `t` when PostgreSQL accepts it, or `ERROR <sqlstate>`)
#[rustfmt::skip]
const CASES: [(&[Piece], &str); 21] = [
    (&[T("\""), U("D83D"), T("\"")], "ERROR 22P02"),
    (&[T("\""), U("DE00"), T("\"")], "ERROR 22P02"),
    (&[T("\""), U("0000"), T("\"")], "ERROR 22P05"),
    (&[T("\""), U("d83d"), U("de00"), T("\"")], "t"),
    (&[T("\""), U("DE00"), U("D83D"), T("\"")], "ERROR 22P02"),
    (&[T("\""), U("D83D"), U("D83D"), T("\"")], "ERROR 22P02"),
    (&[T("\""), U("D83D"), T("x\"")], "ERROR 22P02"),
    (&[T("\""), U("D83D"), T("\\n\"")], "ERROR 22P02"),
    (&[T("\"a"), U("0000"), T("b\"")], "ERROR 22P05"),
    (&[T("{\""), U("0000"), T("\":1}")], "ERROR 22P05"),
    (&[T("{\"a\":\""), U("0000"), T("\"}")], "ERROR 22P05"),
    (&[T("[\""), U("D83D"), T("\"]")], "ERROR 22P02"),
    (&[T("\""), U("D800"), U("DC00"), T("\"")], "t"),
    (&[T("\""), U("DBFF"), U("DFFF"), T("\"")], "t"),
    (&[T("\""), U("DBFF"), U("DBFF"), T("\"")], "ERROR 22P02"),
    (&[T("\""), U("D7FF"), T("\"")], "t"),
    (&[T("\""), U("E000"), T("\"")], "t"),
    (&[T("\""), U("DFFF"), T("\"")], "ERROR 22P02"),
    (&[T("\""), U("0001"), T("\"")], "t"),
    (&[T("\""), U("D83D"), U("DC00"), U("D83D"), U("DC00"), T("\"")], "t"),
    (&[T("\""), U("D83D"), U("de00"), T("\"")], "t"),
];

#[test]
fn surrogate_and_nul_escapes_agree_with_postgresql() {
    let mut f = fixture();
    let mut s = admin();
    for (pieces, expected) in CASES {
        let json = document(pieces);
        let sql = format!("SELECT ('{json}'::jsonb) IS NOT NULL");
        let got = match s.execute(&mut f.db, &sql) {
            Ok(r) => text(&r)[0][0].clone(),
            Err(e) => format!("ERROR {}", e.sqlstate()),
        };
        assert_eq!(got, expected, "{sql}");
    }
}
