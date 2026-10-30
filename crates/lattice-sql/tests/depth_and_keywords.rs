//! Regressions for the parser depth, round-trip and keyword review items (issue 232).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_sql::{ErrorKind, MAX_DEPTH, ParseError, parse_statement};
use proptest::prelude::*;

fn err(sql: &str) -> ParseError {
    match parse_statement(sql) {
        Ok(s) => panic!("`{sql}` should be refused but parsed as {s:?}"),
        Err(e) => e,
    }
}

fn round_trips(sql: &str) {
    let stmt = parse_statement(sql).unwrap_or_else(|e| panic!("`{sql}` should parse: {e}"));
    let printed = stmt.to_string();
    assert_eq!(parse_statement(&printed).as_ref(), Ok(&stmt), "{printed}");
}

fn unsupported(sql: &str, feature: &str) {
    let e = err(sql);
    assert_eq!(e.kind(), ErrorKind::Unsupported, "{sql}: {e}");
    assert!(e.message().contains(feature), "{sql}: {e}");
}

#[test]
fn not_chains_round_trip() {
    round_trips(&format!("SELECT {}a", "NOT ".repeat(40)));
    round_trips(&format!("SELECT {}1", "- ".repeat(40)));
}

#[test]
fn long_left_chains_round_trip() {
    for n in [62, 63, 64, 100] {
        round_trips(&format!("SELECT a {}", "+ 1 ".repeat(n)));
        round_trips(&format!("SELECT a {}", "OR b ".repeat(n)));
    }
}

#[test]
fn long_chains_are_accepted_up_to_a_high_limit() {
    for op in ["OR", "AND", "||", "+"] {
        let ok = format!("SELECT a {}", format!("{op} b ").repeat(100));
        assert!(parse_statement(&ok).is_ok(), "{op}");
        let big = format!("SELECT a {}", format!("{op} b ").repeat(MAX_DEPTH * 2));
        assert_eq!(err(&big).kind(), ErrorKind::TooLarge, "{op}");
    }
}

#[test]
fn isnull_and_current_keywords_are_refused_explicitly() {
    unsupported("SELECT a ISNULL FROM t", "ISNULL");
    unsupported("SELECT a NOTNULL FROM t", "NOTNULL");
    unsupported("SELECT current_date", "CURRENT_DATE");
    unsupported("SELECT current_timestamp FROM t", "CURRENT_TIMESTAMP");
    unsupported("SELECT current_user", "CURRENT_USER");
    unsupported("SELECT session_user", "SESSION_USER");
    unsupported("SELECT localtime", "LOCALTIME");
    unsupported("SELECT localtimestamp", "LOCALTIMESTAMP");
    // quoted, they are ordinary names and survive printing
    round_trips("SELECT \"current_date\", \"isnull\" FROM t");
    assert_eq!(
        err("SELECT a current_date FROM t").kind(),
        ErrorKind::Syntax
    );
}

#[test]
fn parameter_followed_by_letters_is_a_lex_error() {
    assert_eq!(err("SELECT $1abc").kind(), ErrorKind::Syntax);
    assert_eq!(err("SELECT $1abc").span().start, 7);
    assert!(parse_statement("SELECT $1 a").is_ok());
}

#[test]
fn comparisons_do_not_chain() {
    for sql in ["SELECT a = b = c", "SELECT a < b > c", "SELECT a <> b <= c"] {
        assert_eq!(err(sql).kind(), ErrorKind::Syntax, "{sql}");
    }
    round_trips("SELECT (a = b) = c");
    round_trips("SELECT a = b AND b = c");
    round_trips("SELECT a + 1 = b + 2");
}

#[test]
fn earlier_syntax_error_wins_over_later_lex_error() {
    let e = err("SELECT a FROM FROM 'open");
    assert_eq!(e.kind(), ErrorKind::Syntax);
    assert_eq!(e.span().start, 14);
    let e = err("SELECT 'open");
    assert_eq!(e.span().start, 7);
    let e = err("SELECT a FROM t WHERE x = 1abc");
    assert_eq!(e.span().start, 26);
}

#[test]
fn unsupported_postgres_forms_are_named() {
    unsupported("DROP TABLE t CASCADE", "CASCADE");
    unsupported("DROP INDEX i CASCADE", "CASCADE");
    unsupported("CREATE INDEX CONCURRENTLY i ON t (a)", "CONCURRENTLY");
    unsupported("SELECT a BETWEEN SYMMETRIC 1 AND 2", "BETWEEN SYMMETRIC");
    unsupported("SELECT a FROM t LIMIT 1 OFFSET 2 ROWS", "OFFSET");
}

proptest! {
    // The default reads `PROPTEST_CASES` (256 when unset); `with_cases` would ignore the knob.
    #![proptest_config(ProptestConfig::default())]

    #[test]
    fn unary_and_binary_chains_round_trip(
        nots in 0usize..120,
        terms in 1usize..120,
        op in prop::sample::select(vec!["OR", "AND", "||", "+", "-", "*"]),
    ) {
        let sql = format!("SELECT {}a {}", "NOT ".repeat(nots), format!("{op} b ").repeat(terms));
        match parse_statement(&sql) {
            Ok(stmt) => {
                let again = parse_statement(&stmt.to_string());
                prop_assert_eq!(again.as_ref(), Ok(&stmt));
            }
            Err(e) => prop_assert_eq!(e.kind(), ErrorKind::TooLarge),
        }
    }
}
