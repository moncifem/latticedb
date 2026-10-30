//! Tests for the limits expressions honour: `LIKE` polls the statement, and values are bounded before
//! they are built.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use std::cell::Cell;

use lattice_engines::limits::LimitError;
use proptest::prelude::*;

use super::*;

/// Cancels at the `cancel_at`th poll and refuses any value over `budget`; records the largest
/// request so a test can show how much memory the evaluation was allowed to ask for.
struct Limited {
    polls: Cell<u32>,
    cancel_at: u32,
    budget: u64,
    largest: Cell<u64>,
}

impl Limited {
    fn new(cancel_at: u32, budget: u64) -> Self {
        Limited {
            polls: Cell::new(0),
            cancel_at,
            budget,
            largest: Cell::new(0),
        }
    }
}

impl Bounds for Limited {
    fn poll(&self) -> Result<(), ExecError> {
        self.polls.set(self.polls.get() + 1);
        if self.polls.get() >= self.cancel_at {
            return Err(LimitError::Cancelled.into());
        }
        Ok(())
    }
    fn admit(&self, bytes: u64) -> Result<(), ExecError> {
        self.largest.set(self.largest.get().max(bytes));
        if bytes > self.budget {
            return Err(LimitError::MemoryExceeded {
                requested: bytes,
                used: 0,
                limit: self.budget,
            }
            .into());
        }
        Ok(())
    }
}

#[test]
fn like_polls_the_limits_and_stops_a_pathological_pattern() {
    let text = "a".repeat(40_000);
    let pattern = format!("%{}b", "a".repeat(20_000));
    // Without the poll this is about 4e8 comparisons; the third poll cancels it within ~3000.
    let limits = Limited::new(3, u64::MAX);
    let e = like(&text, &pattern, &limits).unwrap_err();
    assert_eq!(e.sqlstate(), "57014");
    assert_eq!(limits.polls.get(), 3, "stopped at the poll, not after");
    // A match that finishes in a few steps never needs a poll.
    let quiet = Limited::new(1, u64::MAX);
    assert!(like("alpha", "a%a", &quiet).unwrap());
    assert_eq!(quiet.polls.get(), 0);
}

#[test]
fn like_refuses_operands_that_do_not_fit_the_budget() {
    let limits = Limited::new(u32::MAX, 1000);
    let e = like(&"a".repeat(500), "%", &limits).unwrap_err();
    assert_eq!(e.sqlstate(), "53200");
}

#[derive(Clone, Copy, Debug)]
enum RefTok {
    Any,
    One,
    Lit(char),
}

/// A direct recursive matcher over the pattern's tokens, written without the production
/// algorithm: each `%` tries every split of the remaining text.
fn reference(text: &[char], pat: &[RefTok]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some((RefTok::Any, rest)) => (0..=text.len()).any(|i| reference(&text[i..], rest)),
        Some((RefTok::One, rest)) => !text.is_empty() && reference(&text[1..], rest),
        Some((RefTok::Lit(c), rest)) => text.first() == Some(c) && reference(&text[1..], rest),
    }
}

fn pattern_text(toks: &[RefTok]) -> String {
    toks.iter()
        .map(|t| match t {
            RefTok::Any => "%".to_string(),
            RefTok::One => "_".to_string(),
            RefTok::Lit(c @ ('%' | '_' | '\\')) => format!("\\{c}"),
            RefTok::Lit(c) => c.to_string(),
        })
        .collect()
}

proptest! {
    #[test]
    fn like_agrees_with_the_recursive_reference(
        text in prop::collection::vec(prop::sample::select(vec!['a', 'b', '%', '_', 'é']), 0..9),
        toks in prop::collection::vec(
            prop_oneof![
                Just(RefTok::Any),
                Just(RefTok::One),
                prop::sample::select(vec!['a', 'b', '%', '_', '\\', 'é']).prop_map(RefTok::Lit),
            ],
            0..8,
        ),
    ) {
        let s: String = text.iter().collect();
        let got = like(&s, &pattern_text(&toks), &Unbounded).unwrap();
        prop_assert_eq!(got, reference(&text, &toks), "{:?} LIKE {:?}", s, pattern_text(&toks));
    }
}

fn leaf() -> TypedExpr {
    TypedExpr {
        kind: ExprNode::Column {
            index: 0,
            name: "s".into(),
        },
        ty: SqlType::Text,
    }
}

fn concat_tree(depth: u32) -> TypedExpr {
    if depth == 0 {
        return leaf();
    }
    TypedExpr {
        kind: ExprNode::Binary {
            op: BinaryOp::Concat,
            left: Box::new(concat_tree(depth - 1)),
            right: Box::new(concat_tree(depth - 1)),
        },
        ty: SqlType::Text,
    }
}

fn run(e: &TypedExpr, row: &[Datum], bounds: &dyn Bounds) -> Result<Datum, ExecError> {
    eval(
        e,
        row,
        &EvalCtx {
            bounds,
            params: &[],
            now_micros: 0,
        },
    )
}

#[test]
fn a_balanced_concatenation_tree_fails_before_it_builds_a_huge_value() {
    // 2^16 leaves of 1 KiB would make a 64 MiB value; the budget stops it far earlier.
    let tree = concat_tree(16);
    let row = [Datum::Text("x".repeat(1024))];
    let limits = Limited::new(u32::MAX, 1 << 20);
    let e = run(&tree, &row, &limits).unwrap_err();
    assert_eq!(e.sqlstate(), "53200");
    assert!(
        limits.largest.get() <= 2 << 20,
        "asked for {} bytes",
        limits.largest.get()
    );
    // With no budget the fixed value cap still applies.
    let e = run(&tree, &row, &Unbounded).unwrap_err();
    assert_eq!((e.sqlstate(), e.kind()), ("54000", crate::ErrorKind::Limit));
    // The same tree over bytea.
    let row = [Datum::Bytes(vec![7; 1024])];
    let e = run(&tree, &row, &Unbounded).unwrap_err();
    assert_eq!(e.sqlstate(), "54000");
}

#[test]
fn concatenation_below_the_cap_still_works() {
    let tree = concat_tree(4);
    let row = [Datum::Text("ab".into())];
    let got = run(&tree, &row, &Limited::new(u32::MAX, 1 << 20)).unwrap();
    assert_eq!(got, Datum::Text("ab".repeat(16)));
}

#[test]
fn case_mapping_is_admitted_before_it_runs() {
    let e = TypedExpr {
        kind: ExprNode::Function {
            name: "upper".into(),
            args: vec![leaf()],
        },
        ty: SqlType::Text,
    };
    let row = [Datum::Text("ß".repeat(1000))];
    let small = run(&e, &row, &Limited::new(u32::MAX, 100)).unwrap_err();
    assert_eq!(small.sqlstate(), "53200");
    assert_eq!(
        run(&e, &row, &Unbounded).unwrap(),
        Datum::Text("SS".repeat(1000))
    );
}
