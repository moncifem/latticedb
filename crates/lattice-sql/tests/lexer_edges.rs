//! Lexer edges found by mutation testing: operator runs that end in a sign, comments that cut an
//! operator short, non-ASCII identifiers, a single colon and the prefixed string literals.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use lattice_sql::{ErrorKind, parse_statement};

fn same(a: &str, b: &str) {
    assert_eq!(
        parse_statement(a).unwrap_or_else(|e| panic!("`{a}`: {e}")),
        parse_statement(b).unwrap_or_else(|e| panic!("`{b}`: {e}")),
        "`{a}` parses like `{b}`"
    );
}

#[test]
fn a_trailing_sign_after_a_plain_operator_is_split_off() {
    same("SELECT 1 =-1", "SELECT 1 = -1");
    same("SELECT 1 >-1", "SELECT 1 > -1");
    same("SELECT 1 <-1", "SELECT 1 < -1");
    same("SELECT 1 <=-1", "SELECT 1 <= -1");
    same("SELECT 1 >=+1", "SELECT 1 >= +1");
    same("SELECT 1 +-1", "SELECT 1 + -1");
    same("SELECT 1 *-1", "SELECT 1 * -1");
}

#[test]
fn an_operator_with_a_user_defined_character_keeps_its_sign() {
    let e = parse_statement("SELECT 1 @-1").unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
    assert_eq!(e.message(), "operator `@-`");
    let e = parse_statement("SELECT 1 @ 1").unwrap_err();
    assert_eq!(e.message(), "operator `@`");
}

#[test]
fn a_comment_start_ends_an_operator_run() {
    same("SELECT 1 +--note\n 2", "SELECT 1 + 2");
    same("SELECT 1 +/* note */ 2", "SELECT 1 + 2");
    same("SELECT 1 <=/* note */ 2", "SELECT 1 <= 2");
    same("SELECT 1 =--note\n 1", "SELECT 1 = 1");
}

#[test]
fn non_ascii_letters_start_names_and_other_symbols_do_not() {
    same("SELECT café FROM t", "SELECT \"café\" FROM t");
    let e = parse_statement("SELECT 1 ☃").unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Syntax);
    assert_eq!(e.span().start, "SELECT 1 ".len());
}

#[test]
fn a_single_colon_is_a_syntax_error_and_a_double_colon_is_a_cast() {
    let e = parse_statement("SELECT 1 : 2").unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Syntax);
    assert_eq!(e.span().start, "SELECT 1 ".len());
    same("SELECT 1::int", "SELECT CAST(1 AS int)");
}

#[test]
fn prefixed_string_literals_are_refused_with_the_prefix_in_the_span() {
    let e = parse_statement("SELECT e'a'").unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
    assert_eq!(e.span().start, 7);
    assert_eq!(e.span().end, 9);
}

#[test]
fn a_slash_or_star_inside_an_operator_run_does_not_cut_it_unless_it_starts_a_comment() {
    for (sql, op) in [("SELECT 1 <>* 2", "<>*"), ("SELECT 1 <// 2", "<//")] {
        let e = parse_statement(sql).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Unsupported, "{sql}");
        assert_eq!(e.message(), format!("operator `{op}`"), "{sql}");
    }
}
