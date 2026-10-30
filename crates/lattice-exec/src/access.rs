//! Access-path choice for a scan: which stored rows can possibly satisfy a predicate.
//!
//! The rules are deterministic and cost-free: there are no statistics. A predicate is a conjunction
//! (`AND`) of terms. The terms of the form `column <op> constant` (`=`, `<`, `<=`, `>`, `>=`,
//! `BETWEEN`) narrow the set of rows the executor has to read:
//!
//! 1. every column of a `UNIQUE` constraint, or of the primary key, is compared with `=`: one stored
//!    row at most (a point lookup);
//! 2. the leading primary key columns are compared with `=` and the next one by a range: a key range;
//! 3. otherwise the whole table is read.
//!
//! The narrowed set is always a **superset** of the rows that satisfy the predicate, and the executor
//! still evaluates the whole predicate on every row it reads, so an index scan returns exactly the rows
//! (in the same order) a full scan returns. The choice is made only when that also holds for errors:
//! every term of the predicate must be unable to fail (comparisons of columns and constants, logic,
//! `IS NULL`, `IN`, `BETWEEN`, and constant expressions that already evaluated), so reading fewer rows
//! cannot hide an error that a full scan would have raised on a row the narrowed read skips.
//!
//! A constant is used only when its value has exactly the type of the column, so the byte order of the
//! key encoding agrees with the comparison the executor makes. `NULL` constants, floating point, numeric
//! and JSON columns never narrow a scan.

use std::ops::Bound;

use lattice_core::bugs::{self, Bug};
use lattice_sql::ast::{BinaryOp, UnaryOp};
use lattice_sql::plan::{ExprNode, SqlType, TypedExpr};

use crate::eval::{EvalCtx, eval};
use crate::schema::{TableDef, push_key};
use crate::value::Datum;

/// How a scan read its table; see [`crate::Database::last_access_paths`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessPath {
    /// Every row of the table was read.
    FullScan,
    /// One row, by its complete primary key.
    PrimaryKeyPoint,
    /// A range of primary keys (a key prefix, a bound on a key column, or both).
    PrimaryKeyRange,
    /// One row, through the entry of the `UNIQUE` constraint with this position.
    UniqueLookup(usize),
}

/// What a scan will read, before the table is touched.
pub(crate) enum Access {
    Full,
    /// No stored row can match (an inverted range).
    Nothing,
    /// A non-empty key range.
    Keys {
        path: AccessPath,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    },
    /// The primary key found in the entry of this constraint.
    Unique {
        constraint: usize,
        entry: Vec<u8>,
    },
}

impl Access {
    pub(crate) fn path(&self) -> AccessPath {
        match self {
            Access::Full => AccessPath::FullScan,
            Access::Nothing => AccessPath::PrimaryKeyRange,
            Access::Keys { path, .. } => *path,
            Access::Unique { constraint, .. } => AccessPath::UniqueLookup(*constraint),
        }
    }
}

/// Deepest expression looked at; the binder refuses trees deeper than 256 levels.
const MAX_DEPTH: u32 = 250;

/// `column <op> constant`, normalized so the column is on the left.
struct Term {
    column: usize,
    op: BinaryOp,
    value: Datum,
}

/// Picks the access path for `predicate` over `def`.
pub(crate) fn choose(def: &TableDef, predicate: &TypedExpr, ctx: &EvalCtx<'_>) -> Access {
    if !total(predicate, ctx, 0) {
        return Access::Full;
    }
    let mut terms = Vec::new();
    collect(predicate, ctx, &mut terms, 0);
    terms.retain(|t| key_compatible(def, t));
    if terms.is_empty() {
        return Access::Full;
    }
    let eq = |column: usize| {
        terms
            .iter()
            .find(|t| t.column == column && t.op == BinaryOp::Eq)
            .map(|t| &t.value)
    };

    // A unique constraint is used when it is fully pinned and the primary key is not.
    let pk_pinned = !def.primary_key.is_empty() && def.primary_key.iter().all(|c| eq(*c).is_some());
    if !pk_pinned {
        for (k, cols) in def.uniques.iter().enumerate() {
            if cols.iter().all(|c| eq(*c).is_some()) {
                let mut entry = Vec::new();
                let encoded = cols
                    .iter()
                    .all(|c| eq(*c).is_some_and(|d| push_key(&mut entry, d).is_ok()));
                if encoded {
                    return Access::Unique {
                        constraint: k,
                        entry,
                    };
                }
            }
        }
    }
    if def.primary_key.is_empty() {
        return Access::Full;
    }

    let mut prefix = Vec::new();
    let mut pinned = 0;
    for c in &def.primary_key {
        match eq(*c) {
            Some(d) if push_key(&mut prefix, d).is_ok() => pinned += 1,
            _ => break,
        }
    }
    if pinned == def.primary_key.len() {
        return Access::Keys {
            path: AccessPath::PrimaryKeyPoint,
            lo: Bound::Included(prefix.clone()),
            hi: Bound::Included(prefix),
        };
    }

    // The key column after the pinned prefix may carry a range. Every bound becomes a byte key:
    // `lo` is the first key to read, `hi` the first key not to read.
    let next = def.primary_key.get(pinned).copied();
    let mut lo = prefix.clone();
    let mut hi = successor(&prefix);
    let mut empty = false;
    let mut narrowed = pinned > 0;
    if let Some(column) = next {
        for t in terms.iter().filter(|t| t.column == column) {
            let mut at = prefix.clone();
            if push_key(&mut at, &t.value).is_err() {
                continue;
            }
            match t.op {
                BinaryOp::GtEq => lo = lo.max(at),
                BinaryOp::Gt => match successor(&at) {
                    Some(next_key) => lo = lo.max(next_key),
                    None => empty = true,
                },
                BinaryOp::Lt => hi = tighter(hi, Some(at)),
                BinaryOp::LtEq => {
                    // Defect for the planted-bug witness: the boundary key itself is dropped.
                    let bound = if bugs::active(Bug::ExecIndexUpperBoundOpen) {
                        Some(at)
                    } else {
                        successor(&at)
                    };
                    hi = tighter(hi, bound);
                }
                _ => continue,
            }
            narrowed = true;
        }
    }
    if !narrowed {
        return Access::Full;
    }
    let hi = match hi {
        Some(h) => {
            if h <= lo {
                empty = true;
            }
            Bound::Excluded(h)
        }
        None => Bound::Unbounded,
    };
    if empty {
        return Access::Nothing;
    }
    Access::Keys {
        path: AccessPath::PrimaryKeyRange,
        lo: Bound::Included(lo),
        hi,
    }
}

/// The smaller of two exclusive upper bounds, where `None` is unbounded.
fn tighter(a: Option<Vec<u8>>, b: Option<Vec<u8>>) -> Option<Vec<u8>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// The first key after every key that starts with `prefix`; `None` when there is none.
fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if let Some(bumped) = last.checked_add(1) {
            out.push(bumped);
            return Some(out);
        }
    }
    None
}

/// Whether the constant has exactly the column's type, so key order is comparison order.
fn key_compatible(def: &TableDef, t: &Term) -> bool {
    let Some(col) = def.columns.get(t.column) else {
        return false;
    };
    matches!(
        (col.ty, &t.value),
        (
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt,
            Datum::Int(_)
        ) | (SqlType::Boolean, Datum::Bool(_))
            | (SqlType::Text, Datum::Text(_))
            | (SqlType::Bytea, Datum::Bytes(_))
            | (SqlType::Uuid, Datum::Uuid(_))
            | (SqlType::Date, Datum::Date(_))
            | (SqlType::Timestamptz, Datum::Timestamp(_))
    )
}

fn has_column(e: &TypedExpr, depth: u32) -> bool {
    if depth > MAX_DEPTH {
        return true;
    }
    match &e.kind {
        ExprNode::Column { .. } => true,
        ExprNode::Const(_) | ExprNode::Param(_) => false,
        ExprNode::Unary { expr, .. } | ExprNode::Cast { expr } | ExprNode::IsNull { expr, .. } => {
            has_column(expr, depth + 1)
        }
        ExprNode::Binary { left, right, .. } => {
            has_column(left, depth + 1) || has_column(right, depth + 1)
        }
        ExprNode::InList { expr, list, .. } => {
            has_column(expr, depth + 1) || list.iter().any(|x| has_column(x, depth + 1))
        }
        ExprNode::Between {
            expr, low, high, ..
        } => {
            has_column(expr, depth + 1) || has_column(low, depth + 1) || has_column(high, depth + 1)
        }
        ExprNode::Like { expr, pattern, .. } => {
            has_column(expr, depth + 1) || has_column(pattern, depth + 1)
        }
        ExprNode::Function { args, .. } => args.iter().any(|x| has_column(x, depth + 1)),
    }
}

/// The value of an expression that reads no column, or `None` if it has a column or fails.
fn constant(e: &TypedExpr, ctx: &EvalCtx<'_>) -> Option<Datum> {
    if has_column(e, 0) {
        return None;
    }
    eval(e, &[], ctx).ok()
}

/// Whether evaluating `e` on any row is certain to succeed (it may be `NULL`).
fn total(e: &TypedExpr, ctx: &EvalCtx<'_>, depth: u32) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let t = |x: &TypedExpr| total(x, ctx, depth + 1);
    match &e.kind {
        ExprNode::Column { .. } | ExprNode::Const(_) | ExprNode::Param(_) => true,
        ExprNode::Unary {
            op: UnaryOp::Not,
            expr,
        } => t(expr),
        ExprNode::IsNull { expr, .. } => t(expr),
        ExprNode::Binary {
            op:
                BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq,
            left,
            right,
        } => t(left) && t(right),
        ExprNode::InList { expr, list, .. } => t(expr) && list.iter().all(t),
        ExprNode::Between {
            expr, low, high, ..
        } => t(expr) && t(low) && t(high),
        // A cast, function, arithmetic or pattern can fail on some value: only a constant that
        // already evaluated is certain to succeed on every row.
        _ => constant(e, ctx).is_some(),
    }
}

fn flip(op: BinaryOp) -> Option<BinaryOp> {
    Some(match op {
        BinaryOp::Eq => BinaryOp::Eq,
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        _ => return None,
    })
}

fn column_of(e: &TypedExpr) -> Option<usize> {
    match &e.kind {
        ExprNode::Column { index, .. } => Some(*index),
        _ => None,
    }
}

/// Gathers the `column <op> constant` terms of the top-level conjunction.
fn collect(e: &TypedExpr, ctx: &EvalCtx<'_>, out: &mut Vec<Term>, depth: u32) {
    if depth > MAX_DEPTH {
        return;
    }
    match &e.kind {
        ExprNode::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect(left, ctx, out, depth + 1);
            collect(right, ctx, out, depth + 1);
        }
        ExprNode::Binary { op, left, right } => {
            let (column, op, other) = match (column_of(left), column_of(right)) {
                (Some(c), None) => (c, *op, right),
                (None, Some(c)) => match flip(*op) {
                    Some(f) => (c, f, left),
                    None => return,
                },
                _ => return,
            };
            if !matches!(
                op,
                BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
            ) {
                return;
            }
            if let Some(value) = constant(other, ctx)
                && !value.is_null()
            {
                out.push(Term { column, op, value });
            }
        }
        ExprNode::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            let Some(column) = column_of(expr) else {
                return;
            };
            if let Some(value) = constant(low, ctx)
                && !value.is_null()
            {
                out.push(Term {
                    column,
                    op: BinaryOp::GtEq,
                    value,
                });
            }
            if let Some(value) = constant(high, ctx)
                && !value.is_null()
            {
                out.push(Term {
                    column,
                    op: BinaryOp::LtEq,
                    value,
                });
            }
        }
        _ => {}
    }
}
