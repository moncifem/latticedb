//! Expression evaluation over one row, with PostgreSQL's three-valued logic.
//!
//! Evaluation is total: every failure is an [`ExecError`] (overflow `22003`, division by zero
//! `22012`, bad text `22P02`), nothing panics, and recursion is bounded by [`MAX_EVAL_DEPTH`].

use std::cmp::Ordering;

use lattice_sql::ast::{BinaryOp, UnaryOp};
use lattice_sql::plan::{ExprNode, SqlType, TypedExpr, Value};

use crate::decimal::Decimal;
use crate::error::ExecError;
use crate::value::{
    Datum, check_int, civil_from_days, compare, date_to_micros, days_from_civil, int_bounds,
    micros_to_date, parse_text,
};

#[cfg(test)]
#[path = "eval_bounds_tests.rs"]
mod bounds_tests;

/// Deepest expression tree evaluated. The binder refuses anything deeper than 256 levels.
pub(crate) const MAX_EVAL_DEPTH: usize = 600;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Largest text or bytea value an expression may build. PostgreSQL allows 1 GB; this subset keeps a
/// value small enough that a tree of concatenations fails before it can exhaust the process.
pub(crate) const MAX_VALUE_BYTES: u64 = 16 << 20;

/// Characters `LIKE` matches between two polls of the statement limits.
const LIKE_POLL_STEPS: u32 = 1024;

/// The statement limits an expression polls while it runs.
pub(crate) trait Bounds {
    /// Fails with a limit error once the statement is cancelled or past its deadline.
    fn poll(&self) -> Result<(), ExecError>;
    /// Fails with `53200` unless `bytes` more would fit in the statement's memory budget now. The
    /// bytes are not kept: an intermediate value is freed when the expression finishes with it.
    fn admit(&self, bytes: u64) -> Result<(), ExecError>;
    /// Reserves `bytes` until the returned guard is dropped, failing with `53200` if they do not fit.
    /// Holds a finished operand while its sibling subtree runs, so the two are bounded together.
    fn hold(&self, bytes: u64) -> Result<Box<dyn Held + '_>, ExecError>;
}

/// A reservation made by [`Bounds::hold`]; dropping it releases the bytes.
pub(crate) trait Held {}

impl Held for () {}

/// Operands smaller than this are not held while the sibling runs: the reservation costs a clock
/// read, and the uncharged total is at most this size times the nesting depth.
const HOLD_MIN_BYTES: u64 = 4096;

/// No limits, for tests that exercise values rather than governance.
#[cfg(test)]
pub(crate) struct Unbounded;

#[cfg(test)]
impl Bounds for Unbounded {
    fn poll(&self) -> Result<(), ExecError> {
        Ok(())
    }
    fn admit(&self, _bytes: u64) -> Result<(), ExecError> {
        Ok(())
    }
    fn hold(&self, _bytes: u64) -> Result<Box<dyn Held + '_>, ExecError> {
        Ok(Box::new(()))
    }
}

/// Refuses a value over [`MAX_VALUE_BYTES`] or over the memory budget, before it is built.
fn admit_value(bounds: &dyn Bounds, bytes: u64) -> Result<(), ExecError> {
    if bytes > MAX_VALUE_BYTES {
        return Err(value_too_large(bytes));
    }
    bounds.admit(bytes)
}

fn value_too_large(bytes: u64) -> ExecError {
    ExecError::new(
        crate::error::ErrorKind::Limit,
        "54000",
        format!("value of {bytes} bytes exceeds the limit of {MAX_VALUE_BYTES} bytes"),
    )
}

/// What an expression may read besides the row.
pub(crate) struct EvalCtx<'a> {
    /// The statement's limits, polled by long loops and checked before large values are built.
    pub bounds: &'a dyn Bounds,
    /// Values of `$1`, `$2`, ...
    pub params: &'a [Datum],
    /// The statement's `now()`, in microseconds since the epoch.
    pub now_micros: i64,
}

/// Evaluates `e` over `row`.
pub(crate) fn eval(e: &TypedExpr, row: &[Datum], ctx: &EvalCtx<'_>) -> Result<Datum, ExecError> {
    go(e, row, ctx, 0)
}

/// Whether a predicate holds: only `true` keeps a row.
pub(crate) fn truthy(d: &Datum) -> bool {
    matches!(d, Datum::Bool(true))
}

fn go(e: &TypedExpr, row: &[Datum], ctx: &EvalCtx<'_>, depth: usize) -> Result<Datum, ExecError> {
    if depth > MAX_EVAL_DEPTH {
        return Err(ExecError::new(
            crate::error::ErrorKind::Bind,
            "54001",
            "expression too deeply nested",
        ));
    }
    let sub = |x: &TypedExpr| go(x, row, ctx, depth + 1);
    match &e.kind {
        ExprNode::Column { index, name } => row
            .get(*index)
            .cloned()
            .ok_or_else(|| ExecError::internal(format!("column {name} is outside the row"))),
        ExprNode::Const(v) => constant(v, e.ty),
        ExprNode::Param(n) => usize::try_from(*n)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| ctx.params.get(i))
            .cloned()
            .ok_or_else(|| {
                ExecError::new(
                    crate::error::ErrorKind::Bind,
                    "42P02",
                    format!("there is no parameter ${n}"),
                )
            }),
        ExprNode::Unary { op, expr } => {
            let v = sub(expr)?;
            unary(*op, v, e.ty)
        }
        ExprNode::Binary { op, left, right } => match op {
            BinaryOp::And => {
                let l = sub(left)?;
                if l == Datum::Bool(false) {
                    return Ok(l);
                }
                let r = sub(right)?;
                Ok(match (&l, &r) {
                    (_, Datum::Bool(false)) => r,
                    (Datum::Bool(true), _) => r,
                    (_, Datum::Bool(true)) => l,
                    _ => Datum::Null,
                })
            }
            BinaryOp::Or => {
                let l = sub(left)?;
                if l == Datum::Bool(true) {
                    return Ok(l);
                }
                let r = sub(right)?;
                Ok(match (&l, &r) {
                    (_, Datum::Bool(true)) => r,
                    (Datum::Bool(false), _) => r,
                    (_, Datum::Bool(false)) => l,
                    _ => Datum::Null,
                })
            }
            _ => {
                let l = sub(left)?;
                // A big text or bytea left operand stays alive while the right subtree runs, so
                // reserve it until then; both operands are then bounded by the budget together.
                let held = match (op, &l) {
                    (BinaryOp::Concat, Datum::Text(s)) if s.len() as u64 >= HOLD_MIN_BYTES => {
                        Some(ctx.bounds.hold(s.len() as u64)?)
                    }
                    (BinaryOp::Concat, Datum::Bytes(b)) if b.len() as u64 >= HOLD_MIN_BYTES => {
                        Some(ctx.bounds.hold(b.len() as u64)?)
                    }
                    _ => None,
                };
                let r = sub(right)?;
                drop(held);
                binary(*op, l, r, e.ty, ctx.bounds)
            }
        },
        ExprNode::IsNull { expr, negated } => {
            let v = sub(expr)?;
            Ok(Datum::Bool(v.is_null() != *negated))
        }
        ExprNode::InList {
            expr,
            list,
            negated,
        } => {
            let v = sub(expr)?;
            if v.is_null() {
                return Ok(Datum::Null);
            }
            let mut saw_null = false;
            for item in list {
                let x = sub(item)?;
                if x.is_null() {
                    saw_null = true;
                } else if compare(&v, &x) == Some(Ordering::Equal) {
                    return Ok(Datum::Bool(!*negated));
                }
            }
            Ok(if saw_null {
                Datum::Null
            } else {
                Datum::Bool(*negated)
            })
        }
        ExprNode::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = sub(expr)?;
            let lo = sub(low)?;
            let hi = sub(high)?;
            let ge = cmp_op(BinaryOp::GtEq, &v, &lo)?;
            let le = cmp_op(BinaryOp::LtEq, &v, &hi)?;
            let both = match (ge, le) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            Ok(match both {
                Some(b) => Datum::Bool(b != *negated),
                None => Datum::Null,
            })
        }
        ExprNode::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = sub(expr)?;
            let p = sub(pattern)?;
            match (v, p) {
                (Datum::Text(s), Datum::Text(p)) => {
                    Ok(Datum::Bool(like(&s, &p, ctx.bounds)? != *negated))
                }
                (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
                _ => Err(ExecError::bind_type("LIKE needs text operands")),
            }
        }
        ExprNode::Cast { expr } => {
            let v = sub(expr)?;
            // bytea prints as hex: two characters per byte plus the `\x` prefix.
            if let (Datum::Bytes(b), SqlType::Text) = (&v, e.ty) {
                admit_value(
                    ctx.bounds,
                    (b.len() as u64).saturating_mul(2).saturating_add(2),
                )?;
            }
            cast(v, e.ty)
        }
        ExprNode::Function { name, args } => {
            // `coalesce` and `nullif` look at their arguments lazily or specially; the rest are strict.
            match name.as_str() {
                "coalesce" => {
                    for a in args {
                        let v = sub(a)?;
                        if !v.is_null() {
                            return Ok(v);
                        }
                    }
                    Ok(Datum::Null)
                }
                "nullif" => {
                    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
                        return Err(ExecError::internal("nullif takes two arguments"));
                    };
                    let (x, y) = (sub(a)?, sub(b)?);
                    if x.is_null() || y.is_null() {
                        return Ok(x);
                    }
                    Ok(if compare(&x, &y) == Some(Ordering::Equal) {
                        Datum::Null
                    } else {
                        x
                    })
                }
                _ => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(sub(a)?);
                    }
                    if vals.iter().any(Datum::is_null) {
                        return Ok(Datum::Null);
                    }
                    function(name, &vals, e.ty, ctx)
                }
            }
        }
    }
}

fn constant(v: &Value, ty: SqlType) -> Result<Datum, ExecError> {
    Ok(match v {
        Value::Null => Datum::Null,
        Value::Bool(b) => Datum::Bool(*b),
        Value::Int(i) => match ty {
            SqlType::Numeric => Datum::Numeric(Decimal::from_i64(*i)),
            SqlType::DoublePrecision => Datum::Float(*i as f64),
            _ => Datum::Int(check_int(i128::from(*i), ty)?),
        },
        Value::Number(s) => match ty {
            SqlType::DoublePrecision => parse_text(s, SqlType::DoublePrecision)?,
            _ => parse_text(s, SqlType::Numeric)?,
        },
        Value::Text(s) => parse_text(s, ty)?,
    })
}

fn unary(op: UnaryOp, v: Datum, ty: SqlType) -> Result<Datum, ExecError> {
    if v.is_null() {
        return Ok(Datum::Null);
    }
    match (op, v) {
        (UnaryOp::Plus, v) => Ok(v),
        (UnaryOp::Not, Datum::Bool(b)) => Ok(Datum::Bool(!b)),
        (UnaryOp::Minus, Datum::Int(i)) => Ok(Datum::Int(check_int(-i128::from(i), ty)?)),
        (UnaryOp::Minus, Datum::Float(f)) => Ok(Datum::Float(-f)),
        (UnaryOp::Minus, Datum::Numeric(d)) => Ok(Datum::Numeric(d.neg()?)),
        _ => Err(ExecError::bind_type(
            "operator is not defined for this type",
        )),
    }
}

fn cmp_op(op: BinaryOp, l: &Datum, r: &Datum) -> Result<Option<bool>, ExecError> {
    if l.is_null() || r.is_null() {
        return Ok(None);
    }
    let ord = compare(l, r).ok_or_else(|| ExecError::bind_type("cannot compare these values"))?;
    // PostgreSQL: NaN equals NaN here, which `compare` already encodes.
    Ok(Some(match op {
        BinaryOp::Eq => ord == Ordering::Equal,
        BinaryOp::NotEq => ord != Ordering::Equal,
        BinaryOp::Lt => ord == Ordering::Less,
        BinaryOp::LtEq => ord != Ordering::Greater,
        BinaryOp::Gt => ord == Ordering::Greater,
        BinaryOp::GtEq => ord != Ordering::Less,
        _ => return Err(ExecError::internal("not a comparison")),
    }))
}

fn binary(
    op: BinaryOp,
    l: Datum,
    r: Datum,
    ty: SqlType,
    bounds: &dyn Bounds,
) -> Result<Datum, ExecError> {
    use BinaryOp::{
        Add, Concat, Contains, Div, Eq, Gt, GtEq, JsonGet, JsonGetText, Lt, LtEq, Mod, Mul, NotEq,
        Sub,
    };
    match op {
        Eq | NotEq | Lt | LtEq | Gt | GtEq => Ok(match cmp_op(op, &l, &r)? {
            Some(b) => Datum::Bool(b),
            None => Datum::Null,
        }),
        Add | Sub | Mul | Div | Mod => {
            if l.is_null() || r.is_null() {
                return Ok(Datum::Null);
            }
            arithmetic(op, &l, &r, ty)
        }
        Concat => match (l, r) {
            (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
            (Datum::Text(mut a), Datum::Text(b)) => {
                admit_value(bounds, (a.len() as u64).saturating_add(b.len() as u64))?;
                a.push_str(&b);
                Ok(Datum::Text(a))
            }
            (Datum::Bytes(mut a), Datum::Bytes(b)) => {
                admit_value(bounds, (a.len() as u64).saturating_add(b.len() as u64))?;
                a.extend_from_slice(&b);
                Ok(Datum::Bytes(a))
            }
            _ => Err(ExecError::bind_type(
                "operator || is not defined for these types",
            )),
        },
        JsonGet | JsonGetText | Contains => Err(ExecError::unsupported(
            "jsonb operators (->, ->>, @>) are bound but not executed yet",
        )),
        _ => Err(ExecError::internal("unexpected logical operator")),
    }
}

fn to_decimal(d: &Datum) -> Result<Decimal, ExecError> {
    match d {
        Datum::Int(i) => Ok(Decimal::from_i64(*i)),
        Datum::Numeric(n) => Ok(*n),
        Datum::Float(f) => Ok(Decimal::from_f64(*f)?),
        _ => Err(ExecError::bind_type("arithmetic needs numbers")),
    }
}

fn to_float(d: &Datum) -> Result<f64, ExecError> {
    match d {
        Datum::Int(i) => Ok(*i as f64),
        Datum::Numeric(n) => Ok(n.to_f64()),
        Datum::Float(f) => Ok(*f),
        _ => Err(ExecError::bind_type("arithmetic needs numbers")),
    }
}

fn arithmetic(op: BinaryOp, l: &Datum, r: &Datum, ty: SqlType) -> Result<Datum, ExecError> {
    match ty {
        SqlType::DoublePrecision => {
            let (a, b) = (to_float(l)?, to_float(r)?);
            let v = match op {
                BinaryOp::Add => a + b,
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div => {
                    if b == 0.0 {
                        return Err(ExecError::eval("22012", "division by zero"));
                    }
                    a / b
                }
                _ => {
                    if b == 0.0 {
                        return Err(ExecError::eval("22012", "division by zero"));
                    }
                    a % b
                }
            };
            if v.is_infinite() && a.is_finite() && b.is_finite() {
                return Err(ExecError::eval("22003", "value out of range: overflow"));
            }
            Ok(Datum::Float(v))
        }
        SqlType::Numeric => {
            let (a, b) = (to_decimal(l)?, to_decimal(r)?);
            Ok(Datum::Numeric(match op {
                BinaryOp::Add => a.add(b)?,
                BinaryOp::Sub => a.sub(b)?,
                BinaryOp::Mul => a.mul(b)?,
                BinaryOp::Div => a.div(b)?,
                _ => a.rem(b)?,
            }))
        }
        _ => {
            let (Datum::Int(a), Datum::Int(b)) = (l, r) else {
                return Err(ExecError::bind_type("arithmetic needs integers"));
            };
            let (a, b) = (i128::from(*a), i128::from(*b));
            let v = match op {
                BinaryOp::Add => a + b,
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div | BinaryOp::Mod => {
                    if b == 0 {
                        return Err(ExecError::eval("22012", "division by zero"));
                    }
                    if op == BinaryOp::Div { a / b } else { a % b }
                }
                _ => return Err(ExecError::internal("not an arithmetic operator")),
            };
            Ok(Datum::Int(check_int(v, ty)?))
        }
    }
}

/// `LIKE` with `%`, `_` and backslash escapes; matching is by character.
pub(crate) fn like(text: &str, pattern: &str, bounds: &dyn Bounds) -> Result<bool, ExecError> {
    #[derive(Clone, Copy)]
    enum Tok {
        Any,
        One,
        Lit(char),
    }
    // The token and character copies are built from both operands (up to 8 bytes per input byte).
    bounds.admit(
        (text.len() as u64)
            .saturating_add(pattern.len() as u64)
            .saturating_mul(8),
    )?;
    let mut toks = Vec::new();
    let mut it = pattern.chars();
    while let Some(c) = it.next() {
        toks.push(match c {
            '%' => Tok::Any,
            '_' => Tok::One,
            '\\' => match it.next() {
                Some(n) => Tok::Lit(n),
                None => {
                    return Err(ExecError::eval(
                        "22025",
                        "LIKE pattern must not end with escape character",
                    ));
                }
            },
            other => Tok::Lit(other),
        });
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut t, mut p) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    let mut steps = 0u32;
    while t < chars.len() {
        steps += 1;
        if steps >= LIKE_POLL_STEPS {
            steps = 0;
            bounds.poll()?;
        }
        match toks.get(p) {
            Some(Tok::Any) => {
                star = Some((p, t));
                p += 1;
            }
            Some(Tok::One) => {
                t += 1;
                p += 1;
            }
            Some(Tok::Lit(c)) if chars.get(t) == Some(c) => {
                t += 1;
                p += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return Ok(false),
            },
        }
    }
    while matches!(toks.get(p), Some(Tok::Any)) {
        p += 1;
    }
    Ok(p == toks.len())
}

/// Converts a value to type `to` as `CAST` does.
pub(crate) fn cast(v: Datum, to: SqlType) -> Result<Datum, ExecError> {
    if v.is_null() {
        return Ok(Datum::Null);
    }
    let range = || ExecError::eval("22003", format!("value out of range for type {to}"));
    match (v, to) {
        (Datum::Bool(b), SqlType::Text) => {
            Ok(Datum::Text((if b { "true" } else { "false" }).into()))
        }
        (Datum::Bool(b), SqlType::Integer) => Ok(Datum::Int(i64::from(b))),
        (Datum::Bool(b), SqlType::Boolean) => Ok(Datum::Bool(b)),
        (Datum::Int(i), SqlType::Boolean) => Ok(Datum::Bool(i != 0)),
        (Datum::Text(s), t) => parse_text(&s, t),
        (d, SqlType::Text) => Ok(Datum::Text(d.to_pg_text().unwrap_or_default())),
        (Datum::Int(i), t) if int_bounds(t).is_some() => {
            Ok(Datum::Int(check_int(i128::from(i), t)?))
        }
        (Datum::Int(i), SqlType::Numeric) => Ok(Datum::Numeric(Decimal::from_i64(i))),
        (Datum::Int(i), SqlType::DoublePrecision) => Ok(Datum::Float(i as f64)),
        (Datum::Float(f), t) if int_bounds(t).is_some() => {
            if !f.is_finite() {
                return Err(range());
            }
            let r = f.round_ties_even();
            let lo = i64::MIN as f64;
            if r < lo || r >= -lo {
                return Err(range());
            }
            Ok(Datum::Int(check_int(r as i128, t)?))
        }
        (Datum::Float(f), SqlType::Numeric) => {
            Ok(Datum::Numeric(Decimal::from_f64(f).map_err(|_| range())?))
        }
        (Datum::Float(f), SqlType::DoublePrecision) => Ok(Datum::Float(f)),
        (Datum::Numeric(d), t) if int_bounds(t).is_some() => {
            Ok(Datum::Int(check_int(d.to_i128_round()?, t)?))
        }
        (Datum::Numeric(d), SqlType::Numeric) => Ok(Datum::Numeric(d)),
        (Datum::Numeric(d), SqlType::DoublePrecision) => Ok(Datum::Float(d.to_f64())),
        (Datum::Date(d), SqlType::Timestamptz) => {
            date_to_micros(d).map(Datum::Timestamp).ok_or_else(range)
        }
        (Datum::Date(d), SqlType::Date) => Ok(Datum::Date(d)),
        (Datum::Timestamp(t), SqlType::Date) => {
            micros_to_date(t).map(Datum::Date).ok_or_else(range)
        }
        (Datum::Timestamp(t), SqlType::Timestamptz) => Ok(Datum::Timestamp(t)),
        (d @ Datum::Bytes(_), SqlType::Bytea)
        | (d @ Datum::Uuid(_), SqlType::Uuid)
        | (d @ Datum::Json(_), SqlType::Jsonb) => Ok(d),
        _ => Err(ExecError::new(
            crate::error::ErrorKind::Bind,
            "42846",
            format!("cannot cast to type {to}"),
        )),
    }
}

fn text_arg(v: &Datum) -> Result<&str, ExecError> {
    match v {
        Datum::Text(s) => Ok(s),
        _ => Err(ExecError::bind_type("a text argument was expected")),
    }
}

fn int_arg(v: &Datum) -> Result<i64, ExecError> {
    match v {
        Datum::Int(i) => Ok(*i),
        _ => Err(ExecError::bind_type("an integer argument was expected")),
    }
}

fn function(name: &str, a: &[Datum], ty: SqlType, ctx: &EvalCtx<'_>) -> Result<Datum, ExecError> {
    let arg = |i: usize| {
        a.get(i)
            .ok_or_else(|| ExecError::internal(format!("{name}: missing argument {i}")))
    };
    match name {
        // Case mapping can lengthen a string (some characters expand), so admit three times its size.
        "lower" | "upper" => {
            let s = text_arg(arg(0)?)?;
            admit_value(ctx.bounds, (s.len() as u64).saturating_mul(3))?;
            let mapped = if name == "lower" {
                s.to_lowercase()
            } else {
                s.to_uppercase()
            };
            if mapped.len() as u64 > MAX_VALUE_BYTES {
                return Err(value_too_large(mapped.len() as u64));
            }
            Ok(Datum::Text(mapped))
        }
        "length" => match arg(0)? {
            Datum::Text(s) => Ok(Datum::Int(
                i64::try_from(s.chars().count()).unwrap_or(i64::MAX),
            )),
            Datum::Bytes(b) => Ok(Datum::Int(i64::try_from(b.len()).unwrap_or(i64::MAX))),
            _ => Err(ExecError::bind_type("length needs text or bytea")),
        },
        "substring" => {
            let s = text_arg(arg(0)?)?;
            let start = int_arg(arg(1)?)?;
            let len = a.get(2).map(int_arg).transpose()?;
            if len.is_some_and(|l| l < 0) {
                return Err(ExecError::eval(
                    "22011",
                    "negative substring length not allowed",
                ));
            }
            let end = len.map_or(i64::MAX, |l| start.saturating_add(l));
            let out: String = s
                .chars()
                .enumerate()
                .filter(|(i, _)| {
                    let pos = i64::try_from(*i).unwrap_or(i64::MAX).saturating_add(1);
                    pos >= start && pos < end
                })
                .map(|(_, c)| c)
                .collect();
            Ok(Datum::Text(out))
        }
        "position" => {
            let needle = text_arg(arg(0)?)?;
            let hay = text_arg(arg(1)?)?;
            Ok(Datum::Int(match hay.find(needle) {
                Some(byte) => {
                    let chars = hay.get(..byte).map_or(0, |p| p.chars().count());
                    i64::try_from(chars).unwrap_or(i64::MAX).saturating_add(1)
                }
                None => 0,
            }))
        }
        "trim_both" | "trim_leading" | "trim_trailing" => {
            let s = text_arg(arg(0)?)?;
            let set: Vec<char> = match a.get(1) {
                Some(c) => text_arg(c)?.chars().collect(),
                None => vec![' '],
            };
            let f = |c: char| set.contains(&c);
            let out = match name {
                "trim_both" => s.trim_matches(f),
                "trim_leading" => s.trim_start_matches(f),
                _ => s.trim_end_matches(f),
            };
            Ok(Datum::Text(out.to_string()))
        }
        "abs" => match arg(0)? {
            Datum::Int(i) => Ok(Datum::Int(check_int(i128::from(*i).abs(), ty)?)),
            Datum::Float(f) => Ok(Datum::Float(f.abs())),
            Datum::Numeric(d) => Ok(Datum::Numeric(d.abs()?)),
            _ => Err(ExecError::bind_type("abs needs a number")),
        },
        "round" => {
            let digits = a.get(1).map(int_arg).transpose()?.unwrap_or(0);
            if digits < 0 {
                return Err(ExecError::unsupported(
                    "round with negative digits is not executed yet",
                ));
            }
            let digits =
                u32::try_from(digits.min(i64::from(crate::decimal::MAX_SCALE))).unwrap_or(0);
            match (arg(0)?, ty) {
                (Datum::Float(f), SqlType::DoublePrecision) if a.len() == 1 => {
                    Ok(Datum::Float(f.round_ties_even()))
                }
                (v, t) => {
                    let d = to_decimal(v)?.round_to(digits)?;
                    Ok(if t == SqlType::DoublePrecision {
                        Datum::Float(d.to_f64())
                    } else {
                        Datum::Numeric(d)
                    })
                }
            }
        }
        "now" => Ok(Datum::Timestamp(ctx.now_micros)),
        "date_trunc" => {
            let unit = text_arg(arg(0)?)?.to_ascii_lowercase();
            let t = timestamp_arg(arg(1)?)?;
            Ok(Datum::Timestamp(date_trunc(&unit, t)?))
        }
        "extract" => {
            let field = text_arg(arg(0)?)?;
            let t = timestamp_arg(arg(1)?)?;
            Ok(Datum::Numeric(extract(field, t)?))
        }
        other => Err(ExecError::new(
            crate::error::ErrorKind::Bind,
            "42883",
            format!("function {other} does not exist"),
        )),
    }
}

fn timestamp_arg(v: &Datum) -> Result<i64, ExecError> {
    match v {
        Datum::Timestamp(t) => Ok(*t),
        Datum::Date(d) => {
            date_to_micros(*d).ok_or_else(|| ExecError::eval("22008", "date out of range"))
        }
        _ => Err(ExecError::bind_type("a date or timestamp was expected")),
    }
}

fn date_trunc(unit: &str, t: i64) -> Result<i64, ExecError> {
    let days = t.div_euclid(MICROS_PER_DAY);
    let (y, m, _) = civil_from_days(days);
    let out_of_range = || ExecError::eval("22008", "timestamp out of range");
    Ok(match unit {
        "second" => t.div_euclid(MICROS_PER_SECOND) * MICROS_PER_SECOND,
        "minute" => t.div_euclid(60 * MICROS_PER_SECOND) * 60 * MICROS_PER_SECOND,
        "hour" => t.div_euclid(3600 * MICROS_PER_SECOND) * 3600 * MICROS_PER_SECOND,
        "day" => days * MICROS_PER_DAY,
        "week" => {
            // Weeks start on Monday; 1970-01-01 was a Thursday.
            let since_monday = (days + 3).rem_euclid(7);
            (days - since_monday) * MICROS_PER_DAY
        }
        "month" => days_from_civil(y, m, 1) * MICROS_PER_DAY,
        "quarter" => days_from_civil(y, (m - 1) / 3 * 3 + 1, 1) * MICROS_PER_DAY,
        "year" => days_from_civil(y, 1, 1) * MICROS_PER_DAY,
        other => {
            return Err(ExecError::new(
                crate::error::ErrorKind::Bind,
                "22023",
                format!("unit \"{other}\" not recognized for date_trunc"),
            ));
        }
    })
    .and_then(|v: i64| {
        if civil_from_days(v.div_euclid(MICROS_PER_DAY)).0 < 1 {
            Err(out_of_range())
        } else {
            Ok(v)
        }
    })
}

fn extract(field: &str, t: i64) -> Result<Decimal, ExecError> {
    let days = t.div_euclid(MICROS_PER_DAY);
    let rem = t.rem_euclid(MICROS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let int = |v: i64| Decimal::from_i64(v);
    Ok(match field {
        "year" => int(y),
        "month" => int(m),
        "day" => int(d),
        "quarter" => int((m - 1) / 3 + 1),
        "hour" => int(rem / 3_600_000_000),
        "minute" => int(rem / 60_000_000 % 60),
        "second" => Decimal::new(i128::from(rem % 60_000_000), 6)?,
        "epoch" => Decimal::new(i128::from(t), 6)?,
        "dow" => int((days + 4).rem_euclid(7)),
        "doy" => int(days - days_from_civil(y, 1, 1) + 1),
        "week" => {
            let weekday = (days + 3).rem_euclid(7) + 1;
            let thursday = days + (4 - weekday);
            let (ty, _, _) = civil_from_days(thursday);
            int((thursday - days_from_civil(ty, 1, 1)) / 7 + 1)
        }
        other => {
            return Err(ExecError::new(
                crate::error::ErrorKind::Bind,
                "22023",
                format!("unit \"{other}\" not recognized for extract"),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;

    fn ctx() -> EvalCtx<'static> {
        EvalCtx {
            bounds: &Unbounded,
            params: &[],
            now_micros: 0,
        }
    }

    fn lit_int(v: i64) -> TypedExpr {
        TypedExpr {
            kind: ExprNode::Const(Value::Int(v)),
            ty: SqlType::Integer,
        }
    }

    fn lit_null(ty: SqlType) -> TypedExpr {
        TypedExpr {
            kind: ExprNode::Const(Value::Null),
            ty,
        }
    }

    fn lit_bool(b: Option<bool>) -> TypedExpr {
        TypedExpr {
            kind: match b {
                Some(b) => ExprNode::Const(Value::Bool(b)),
                None => ExprNode::Const(Value::Null),
            },
            ty: SqlType::Boolean,
        }
    }

    fn bin(op: BinaryOp, l: TypedExpr, r: TypedExpr, ty: SqlType) -> TypedExpr {
        TypedExpr {
            kind: ExprNode::Binary {
                op,
                left: Box::new(l),
                right: Box::new(r),
            },
            ty,
        }
    }

    #[test]
    fn three_valued_and_or() {
        let t = Some(true);
        let f = Some(false);
        let n = None;
        let cases = [
            (BinaryOp::And, t, n, Datum::Null),
            (BinaryOp::And, f, n, Datum::Bool(false)),
            (BinaryOp::And, n, f, Datum::Bool(false)),
            (BinaryOp::And, n, n, Datum::Null),
            (BinaryOp::And, t, t, Datum::Bool(true)),
            (BinaryOp::Or, t, n, Datum::Bool(true)),
            (BinaryOp::Or, n, t, Datum::Bool(true)),
            (BinaryOp::Or, f, n, Datum::Null),
            (BinaryOp::Or, f, f, Datum::Bool(false)),
        ];
        for (op, l, r, want) in cases {
            let e = bin(op, lit_bool(l), lit_bool(r), SqlType::Boolean);
            assert_eq!(eval(&e, &[], &ctx()).unwrap(), want, "{op:?} {l:?} {r:?}");
        }
    }

    #[test]
    fn null_comparison_is_null_and_is_null_is_two_valued() {
        let e = bin(
            BinaryOp::Eq,
            lit_int(1),
            lit_null(SqlType::Integer),
            SqlType::Boolean,
        );
        assert_eq!(eval(&e, &[], &ctx()).unwrap(), Datum::Null);
        let e = TypedExpr {
            kind: ExprNode::IsNull {
                expr: Box::new(lit_null(SqlType::Integer)),
                negated: false,
            },
            ty: SqlType::Boolean,
        };
        assert_eq!(eval(&e, &[], &ctx()).unwrap(), Datum::Bool(true));
    }

    #[test]
    fn integer_arithmetic_checks_its_type() {
        let big = |v| TypedExpr {
            kind: ExprNode::Const(Value::Int(v)),
            ty: SqlType::Integer,
        };
        // Column values are not folded by the binder, so the evaluator must catch overflow.
        let col = |i| TypedExpr {
            kind: ExprNode::Column {
                index: i,
                name: "c".into(),
            },
            ty: SqlType::Integer,
        };
        let row = [Datum::Int(i64::from(i32::MAX))];
        let e = bin(BinaryOp::Add, col(0), big(1), SqlType::Integer);
        assert_eq!(eval(&e, &row, &ctx()).unwrap_err().sqlstate(), "22003");
        let e = bin(BinaryOp::Div, col(0), big(0), SqlType::Integer);
        assert_eq!(eval(&e, &row, &ctx()).unwrap_err().sqlstate(), "22012");
        let e = bin(BinaryOp::Mod, col(0), big(0), SqlType::Integer);
        assert_eq!(eval(&e, &row, &ctx()).unwrap_err().sqlstate(), "22012");
        let e = bin(BinaryOp::Sub, col(0), big(1), SqlType::Integer);
        assert_eq!(
            eval(&e, &row, &ctx()).unwrap(),
            Datum::Int(i64::from(i32::MAX) - 1)
        );
        // -2^31 / -1 overflows integer.
        let row = [Datum::Int(i64::from(i32::MIN))];
        let e = bin(BinaryOp::Div, col(0), big(-1), SqlType::Integer);
        assert_eq!(eval(&e, &row, &ctx()).unwrap_err().sqlstate(), "22003");
        // Integer division truncates toward zero and the remainder takes the dividend's sign.
        let row = [Datum::Int(-7)];
        let e = bin(BinaryOp::Div, col(0), big(2), SqlType::Integer);
        assert_eq!(eval(&e, &row, &ctx()).unwrap(), Datum::Int(-3));
        let e = bin(BinaryOp::Mod, col(0), big(2), SqlType::Integer);
        assert_eq!(eval(&e, &row, &ctx()).unwrap(), Datum::Int(-1));
    }

    #[test]
    fn like_matches_postgres_patterns() {
        for (text, pat, want) in [
            ("alpha", "a%", true),
            ("alpha", "%ha", true),
            ("alpha", "a_pha", true),
            ("alpha", "a_ha", false),
            ("alpha", "%", true),
            ("", "%", true),
            ("", "_", false),
            ("a%b", "a\\%b", true),
            ("axb", "a\\%b", false),
            ("abcabc", "%abc", true),
            ("abcabd", "%abc", false),
            ("aXbXc", "%X%X%", true),
        ] {
            assert_eq!(
                like(text, pat, &Unbounded).unwrap(),
                want,
                "{text} LIKE {pat}"
            );
        }
        assert_eq!(
            like("a", "a\\", &Unbounded).unwrap_err().sqlstate(),
            "22025"
        );
    }

    #[test]
    fn casts_follow_postgres_rounding_and_ranges() {
        assert_eq!(
            cast(Datum::Float(2.5), SqlType::Integer).unwrap(),
            Datum::Int(2)
        );
        assert_eq!(
            cast(Datum::Float(3.5), SqlType::Integer).unwrap(),
            Datum::Int(4)
        );
        assert_eq!(
            cast(
                Datum::Numeric(Decimal::parse("2.5").unwrap()),
                SqlType::Integer
            )
            .unwrap(),
            Datum::Int(3)
        );
        assert_eq!(
            cast(Datum::Int(40_000), SqlType::SmallInt)
                .unwrap_err()
                .sqlstate(),
            "22003"
        );
        assert_eq!(
            cast(Datum::Float(f64::NAN), SqlType::Integer)
                .unwrap_err()
                .sqlstate(),
            "22003"
        );
        assert_eq!(
            cast(Datum::Text("12x".into()), SqlType::Integer)
                .unwrap_err()
                .sqlstate(),
            "22P02"
        );
        assert_eq!(
            cast(Datum::Bool(true), SqlType::Text).unwrap(),
            Datum::Text("true".into())
        );
        assert_eq!(cast(Datum::Null, SqlType::Integer).unwrap(), Datum::Null);
    }

    #[test]
    fn functions_are_strict_and_extract_matches_known_dates() {
        let t = crate::value::parse_timestamp("2024-03-15 10:20:30.25").unwrap();
        assert_eq!(extract("year", t).unwrap().to_string(), "2024");
        assert_eq!(extract("month", t).unwrap().to_string(), "3");
        assert_eq!(extract("second", t).unwrap().to_string(), "30.250000");
        // 2024-03-15 was a Friday.
        assert_eq!(extract("dow", t).unwrap().to_string(), "5");
        assert_eq!(extract("doy", t).unwrap().to_string(), "75");
        assert_eq!(extract("week", t).unwrap().to_string(), "11");
        assert_eq!(extract("quarter", t).unwrap().to_string(), "1");
        assert_eq!(
            crate::value::format_timestamp(date_trunc("week", t).unwrap()),
            "2024-03-11 00:00:00+00"
        );
        assert_eq!(
            crate::value::format_timestamp(date_trunc("quarter", t).unwrap()),
            "2024-01-01 00:00:00+00"
        );
        let r = function(
            "substring",
            &[Datum::Text("hello".into()), Datum::Int(2), Datum::Int(3)],
            SqlType::Text,
            &ctx(),
        );
        assert_eq!(r.unwrap(), Datum::Text("ell".into()));
        let r = function(
            "substring",
            &[Datum::Text("hello".into()), Datum::Int(-1), Datum::Int(3)],
            SqlType::Text,
            &ctx(),
        );
        assert_eq!(r.unwrap(), Datum::Text("h".into()));
        let r = function(
            "position",
            &[Datum::Text("lo".into()), Datum::Text("hello".into())],
            SqlType::Integer,
            &ctx(),
        );
        assert_eq!(r.unwrap(), Datum::Int(4));
        let r = function(
            "trim_both",
            &[Datum::Text("xxhixx".into()), Datum::Text("x".into())],
            SqlType::Text,
            &ctx(),
        );
        assert_eq!(r.unwrap(), Datum::Text("hi".into()));
    }

    #[test]
    fn deep_expressions_error_instead_of_overflowing_the_stack() {
        let mut e = lit_int(1);
        for _ in 0..(MAX_EVAL_DEPTH + 50) {
            e = TypedExpr {
                kind: ExprNode::Unary {
                    op: UnaryOp::Plus,
                    expr: Box::new(e),
                },
                ty: SqlType::Integer,
            };
        }
        assert_eq!(eval(&e, &[], &ctx()).unwrap_err().sqlstate(), "54001");
    }

    // Plans from the binder are well typed, but the evaluator must not rely on it: it is total over
    // trees with any mix of operators, types and arities.
    mod structural {
        use super::*;
        use proptest::prelude::*;

        fn ty() -> impl Strategy<Value = SqlType> {
            prop::sample::select(vec![
                SqlType::Boolean,
                SqlType::SmallInt,
                SqlType::Integer,
                SqlType::BigInt,
                SqlType::Numeric,
                SqlType::DoublePrecision,
                SqlType::Text,
                SqlType::Bytea,
                SqlType::Timestamptz,
                SqlType::Date,
                SqlType::Uuid,
                SqlType::Jsonb,
            ])
        }

        fn value() -> impl Strategy<Value = Value> {
            prop_oneof![
                Just(Value::Null),
                any::<bool>().prop_map(Value::Bool),
                prop_oneof![any::<i64>(), -3i64..4].prop_map(Value::Int),
                prop::sample::select(vec![
                    "0",
                    "1.5",
                    "-2",
                    "1e400",
                    "99999999999999999999999999999999999999",
                    ".",
                    "abc"
                ])
                .prop_map(|s| Value::Number(s.to_string())),
                "[ -~]{0,6}".prop_map(Value::Text),
            ]
        }

        fn datum() -> impl Strategy<Value = Datum> {
            prop_oneof![
                Just(Datum::Null),
                any::<bool>().prop_map(Datum::Bool),
                any::<i64>().prop_map(Datum::Int),
                any::<f64>().prop_map(Datum::Float),
                "[a-z%_]{0,4}".prop_map(Datum::Text),
                any::<i64>().prop_map(Datum::Timestamp),
                any::<i32>().prop_map(Datum::Date),
            ]
        }

        fn binary_op() -> impl Strategy<Value = BinaryOp> {
            prop::sample::select(vec![
                BinaryOp::Or,
                BinaryOp::And,
                BinaryOp::Eq,
                BinaryOp::NotEq,
                BinaryOp::Lt,
                BinaryOp::GtEq,
                BinaryOp::Concat,
                BinaryOp::JsonGet,
                BinaryOp::Contains,
                BinaryOp::Add,
                BinaryOp::Sub,
                BinaryOp::Mul,
                BinaryOp::Div,
                BinaryOp::Mod,
            ])
        }

        fn expr() -> BoxedStrategy<TypedExpr> {
            let leaf = prop_oneof![
                (0usize..6, ty()).prop_map(|(index, t)| TypedExpr {
                    kind: ExprNode::Column {
                        index,
                        name: "c".into()
                    },
                    ty: t,
                }),
                (value(), ty()).prop_map(|(v, t)| TypedExpr {
                    kind: ExprNode::Const(v),
                    ty: t
                }),
                (0u32..4, ty()).prop_map(|(n, t)| TypedExpr {
                    kind: ExprNode::Param(n),
                    ty: t
                }),
            ];
            leaf.prop_recursive(4, 40, 4, |inner| {
                let boxed = |e: TypedExpr| Box::new(e);
                prop_oneof![
                    (inner.clone(), ty()).prop_map(move |(e, t)| TypedExpr {
                        kind: ExprNode::Unary {
                            op: UnaryOp::Minus,
                            expr: boxed(e)
                        },
                        ty: t,
                    }),
                    (binary_op(), inner.clone(), inner.clone(), ty()).prop_map(|(op, l, r, t)| {
                        TypedExpr {
                            kind: ExprNode::Binary {
                                op,
                                left: Box::new(l),
                                right: Box::new(r),
                            },
                            ty: t,
                        }
                    }),
                    (inner.clone(), any::<bool>()).prop_map(|(e, negated)| TypedExpr {
                        kind: ExprNode::IsNull {
                            expr: Box::new(e),
                            negated
                        },
                        ty: SqlType::Boolean,
                    }),
                    (
                        inner.clone(),
                        prop::collection::vec(inner.clone(), 0..3),
                        any::<bool>()
                    )
                        .prop_map(|(e, list, negated)| TypedExpr {
                            kind: ExprNode::InList {
                                expr: Box::new(e),
                                list,
                                negated
                            },
                            ty: SqlType::Boolean,
                        }),
                    (inner.clone(), inner.clone(), inner.clone()).prop_map(|(e, l, h)| TypedExpr {
                        kind: ExprNode::Between {
                            expr: Box::new(e),
                            low: Box::new(l),
                            high: Box::new(h),
                            negated: false,
                        },
                        ty: SqlType::Boolean,
                    }),
                    (inner.clone(), inner.clone()).prop_map(|(e, p)| TypedExpr {
                        kind: ExprNode::Like {
                            expr: Box::new(e),
                            pattern: Box::new(p),
                            negated: true
                        },
                        ty: SqlType::Boolean,
                    }),
                    (inner.clone(), ty()).prop_map(|(e, t)| TypedExpr {
                        kind: ExprNode::Cast { expr: Box::new(e) },
                        ty: t,
                    }),
                    (
                        prop::sample::select(vec![
                            "coalesce",
                            "nullif",
                            "lower",
                            "upper",
                            "length",
                            "substring",
                            "position",
                            "trim_both",
                            "trim_leading",
                            "abs",
                            "round",
                            "now",
                            "date_trunc",
                            "extract",
                            "nope",
                        ]),
                        prop::collection::vec(inner, 0..4),
                        ty(),
                    )
                        .prop_map(|(name, args, t)| TypedExpr {
                            kind: ExprNode::Function {
                                name: name.to_string(),
                                args
                            },
                            ty: t,
                        }),
                ]
            })
            .boxed()
        }

        proptest! {
            #[test]
            fn evaluation_is_total_over_any_tree(
                e in expr(),
                row in prop::collection::vec(datum(), 0..5),
                params in prop::collection::vec(datum(), 0..3),
            ) {
                let ctx = EvalCtx { bounds: &Unbounded, params: &params, now_micros: 0 };
                // Ok or Err are both fine; reaching this line means nothing panicked.
                match eval(&e, &row, &ctx) {
                    Ok(_) => {}
                    Err(err) => prop_assert_eq!(err.sqlstate().len(), 5),
                }
            }
        }
    }
}
