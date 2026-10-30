//! Name resolution and type checking: from a parsed statement to a [`LogicalPlan`].
//!
//! The binder covers single-table statements: `SELECT` with `WHERE`, `ORDER BY`, `LIMIT` and
//! `OFFSET`, and `INSERT`, `UPDATE`, `DELETE` with `RETURNING`. Joins, `GROUP BY`, aggregates,
//! `DISTINCT`, subqueries, DDL and session statements are refused with `0A000` until the plan IR
//! grows operators for them; refusing is the documented behaviour, never a wrong plan.
//!
//! Typing follows PostgreSQL where the subset accepts the syntax, with these differences:
//! every operand type mismatch reports `42804` (PostgreSQL reports `42883` for operators), and
//! `||` accepts only `text || text` and `bytea || bytea`.
//!
//! Integer constants fold at bind time, so `2147483647 + 1` fails with `22003` and `1 / 0` with
//! `22012` instead of failing when a row is read.

use std::fmt;

use crate::MAX_DEPTH;
use crate::ast::{
    self, AsOf, BinaryOp, Expr, ExprKind, FunctionArgs, Literal, ObjectName, Query, SelectItem,
    Statement, TrimSide, UnaryOp,
};
use crate::error::Span;
use crate::plan::{
    AsOfPlan, BoundStatement, CatalogView, ColumnInfo, ExprNode, InsertSource, LogicalPlan,
    OutputColumn, SortKey, SqlType, TableInfo, TypedExpr, Value,
};

/// The class of a binding failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindErrorKind {
    /// Unknown table (`42P01`), also a qualifier that names no table in `FROM`.
    UndefinedTable,
    /// Unknown column (`42703`).
    UndefinedColumn,
    /// Operand or assignment of the wrong type (`42804`).
    DatatypeMismatch,
    /// A constant or a folded constant outside the range of its type (`22003`).
    NumericOutOfRange,
    /// Constant division or modulo by zero (`22012`).
    DivisionByZero,
    /// A `NULL` for a `NOT NULL` column (`23502`).
    NotNullViolation,
    /// Valid but outside what the binder handles (`0A000`, `LSQL-0002`).
    Unsupported,
    /// Wrong number of values (`42601`).
    Syntax,
    /// A column named twice in one list (`42701`).
    DuplicateColumn,
    /// A string literal that is not a valid value of its type (`22P02`).
    InvalidText,
    /// A parameter used with two different types (`42P08`).
    InconsistentParamTypes,
    /// A parameter no context gives a type (`42P18`).
    IndeterminateParam,
    /// A parameter number that is zero or too large (`42P02`).
    UndefinedParameter,
    /// `LIMIT` below zero (`2201W`).
    NegativeLimit,
    /// `OFFSET` below zero (`2201X`).
    NegativeOffset,
    /// A cast between types with no conversion (`42846`).
    CannotCoerce,
    /// A function called with arguments it does not take (`42883`).
    UndefinedFunction,
    /// A field name that is not valid, as in `extract` (`22023`).
    InvalidParameterValue,
    /// A schema other than `public` (`3F000`).
    InvalidSchema,
    /// An `ORDER BY` name that matches two different output columns (`42702`).
    AmbiguousColumn,
    /// An `ORDER BY` position outside the select list (`42P10`).
    InvalidColumnReference,
    /// The statement nests deeper than [`MAX_DEPTH`] (`54001`).
    TooLarge,
}

/// A binding failure with the place it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindError {
    kind: BindErrorKind,
    message: String,
    span: Span,
}

impl BindError {
    fn new(kind: BindErrorKind, message: impl Into<String>, span: Span) -> Self {
        BindError {
            kind,
            message: message.into(),
            span,
        }
    }

    /// The class of failure.
    pub fn kind(&self) -> BindErrorKind {
        self.kind
    }

    /// The description.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Where in the statement text the failure was found; empty for a tree built by hand.
    pub fn span(&self) -> Span {
        self.span
    }

    /// The PostgreSQL error code to report.
    pub fn sqlstate(&self) -> &'static str {
        match self.kind {
            BindErrorKind::UndefinedTable => "42P01",
            BindErrorKind::UndefinedColumn => "42703",
            BindErrorKind::DatatypeMismatch => "42804",
            BindErrorKind::NumericOutOfRange => "22003",
            BindErrorKind::DivisionByZero => "22012",
            BindErrorKind::NotNullViolation => "23502",
            BindErrorKind::Unsupported => "0A000",
            BindErrorKind::Syntax => "42601",
            BindErrorKind::DuplicateColumn => "42701",
            BindErrorKind::InvalidText => "22P02",
            BindErrorKind::InconsistentParamTypes => "42P08",
            BindErrorKind::IndeterminateParam => "42P18",
            BindErrorKind::UndefinedParameter => "42P02",
            BindErrorKind::NegativeLimit => "2201W",
            BindErrorKind::NegativeOffset => "2201X",
            BindErrorKind::CannotCoerce => "42846",
            BindErrorKind::UndefinedFunction => "42883",
            BindErrorKind::InvalidParameterValue => "22023",
            BindErrorKind::InvalidSchema => "3F000",
            BindErrorKind::InvalidColumnReference => "42P10",
            BindErrorKind::AmbiguousColumn => "42702",
            BindErrorKind::TooLarge => "54001",
        }
    }

    /// The stable Lattice code, where `docs/sql-subset.md` defines one for this class.
    pub fn lattice_code(&self) -> Option<&'static str> {
        match self.kind {
            BindErrorKind::Syntax => Some("LSQL-0001"),
            BindErrorKind::Unsupported => Some("LSQL-0002"),
            BindErrorKind::UndefinedTable | BindErrorKind::UndefinedColumn => Some("LSQL-0003"),
            BindErrorKind::NotNullViolation => Some("LSQL-0004"),
            _ => None,
        }
    }
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(code) = self.lattice_code() {
            write!(f, "{code} ")?;
        }
        write!(f, "{} ({})", self.message, self.sqlstate())
    }
}

impl std::error::Error for BindError {}

type R<T> = Result<T, BindError>;

/// Binds one statement against the catalog.
pub fn bind(stmt: &Statement, catalog: &dyn CatalogView) -> Result<LogicalPlan, BindError> {
    bind_statement(stmt, catalog).map(|b| b.plan)
}

/// Binds one statement and reports the types it gave the `$n` placeholders.
pub fn bind_statement(
    stmt: &Statement,
    catalog: &dyn CatalogView,
) -> Result<BoundStatement, BindError> {
    let mut b = Binder {
        catalog,
        params: Vec::new(),
        depth: 0,
    };
    let plan = b.statement(stmt)?;
    let mut param_types = Vec::with_capacity(b.params.len());
    for (i, p) in b.params.iter().enumerate() {
        match p {
            Some(t) => param_types.push(*t),
            None => {
                return Err(BindError::new(
                    BindErrorKind::IndeterminateParam,
                    format!("could not determine data type of parameter ${}", i + 1),
                    Span::default(),
                ));
            }
        }
    }
    Ok(BoundStatement {
        plan,
        catalog_version: catalog.version(),
        param_types,
    })
}

/// Largest `$n` accepted, as in PostgreSQL.
const MAX_PARAMS: u32 = 65_535;

/// The table a statement reads, and the names that can qualify its columns.
struct Scope {
    info: TableInfo,
    /// The alias if there is one, else the table name.
    qualifier: String,
}

impl Scope {
    /// Whether this is the scope of a `SELECT` without `FROM`.
    fn is_row_source_empty(&self) -> bool {
        self.qualifier.is_empty()
    }

    fn empty() -> Scope {
        Scope {
            info: TableInfo {
                id: 0,
                name: String::new(),
                columns: Vec::new(),
                indexes: Vec::new(),
            },
            qualifier: String::new(),
        }
    }
}

struct Binder<'a> {
    catalog: &'a dyn CatalogView,
    params: Vec<Option<SqlType>>,
    depth: usize,
}

fn err<T>(kind: BindErrorKind, message: impl Into<String>, span: Span) -> R<T> {
    Err(BindError::new(kind, message, span))
}

fn unsupported<T>(what: &str, span: Span) -> R<T> {
    err(BindErrorKind::Unsupported, what, span)
}

fn typed(kind: ExprNode, ty: SqlType) -> TypedExpr {
    TypedExpr { kind, ty }
}

fn konst(v: Value, ty: SqlType) -> TypedExpr {
    typed(ExprNode::Const(v), ty)
}

fn int_range(ty: SqlType) -> Option<(i128, i128)> {
    match ty {
        SqlType::SmallInt => Some((i128::from(i16::MIN), i128::from(i16::MAX))),
        SqlType::Integer => Some((i128::from(i32::MIN), i128::from(i32::MAX))),
        SqlType::BigInt => Some((i128::from(i64::MIN), i128::from(i64::MAX))),
        _ => None,
    }
}

fn is_integer(ty: SqlType) -> bool {
    int_range(ty).is_some()
}

fn mismatch<T>(what: impl Into<String>, span: Span) -> R<T> {
    err(BindErrorKind::DatatypeMismatch, what, span)
}

/// Whether `s` is a decimal number as `numeric` input accepts it.
fn is_decimal(s: &str) -> bool {
    let s = s.trim();
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(at) => (s.get(..at).unwrap_or(""), s.get(at + 1..)),
        None => (s, None),
    };
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((a, b)) => {
            (a.is_empty() || digits(a))
                && (b.is_empty() || digits(b))
                && !(a.is_empty() && b.is_empty())
        }
        None => digits(mantissa),
    };
    let exp_ok = match exp {
        None => true,
        Some(e) => digits(e.strip_prefix(['+', '-']).unwrap_or(e)),
    };
    mantissa_ok && exp_ok
}

impl Binder<'_> {
    fn statement(&mut self, stmt: &Statement) -> R<LogicalPlan> {
        match stmt {
            Statement::Query(q) => self.query(q, None),
            Statement::Insert(i) => self.insert(i),
            Statement::Update(u) => self.update(u),
            Statement::Delete(d) => self.delete(d),
            _ => unsupported(
                "binding of DDL, transaction and session statements",
                Span::default(),
            ),
        }
    }

    // ----- tables -----

    fn lookup(&self, name: &ObjectName) -> R<TableInfo> {
        let span = name
            .0
            .iter()
            .map(|i| i.span)
            .reduce(Span::join)
            .unwrap_or_default();
        let table = match name.0.as_slice() {
            [t] => &t.value,
            [schema, t] => {
                if schema.value != "public" {
                    return err(
                        BindErrorKind::InvalidSchema,
                        format!("schema \"{}\" does not exist", schema.value),
                        schema.span,
                    );
                }
                &t.value
            }
            _ => return unsupported("table names with more than two parts", span),
        };
        match self.catalog.table(table) {
            Some(t) => Ok(t),
            None => err(
                BindErrorKind::UndefinedTable,
                format!("relation \"{table}\" does not exist"),
                span,
            ),
        }
    }

    fn scope(&self, name: &ObjectName, alias: Option<&ast::Ident>) -> R<Scope> {
        let info = self.lookup(name)?;
        let qualifier = alias.map_or_else(|| info.name.clone(), |a| a.value.clone());
        Ok(Scope { info, qualifier })
    }

    // ----- parameters and coercion -----

    fn set_param(&mut self, n: u32, ty: SqlType, span: Span) -> R<()> {
        if n == 0 || n > MAX_PARAMS {
            return err(
                BindErrorKind::UndefinedParameter,
                format!("there is no parameter ${n}"),
                span,
            );
        }
        let at = (n - 1) as usize;
        if self.params.len() <= at {
            self.params.resize(at + 1, None);
        }
        match self.params.get_mut(at) {
            Some(slot @ None) => {
                *slot = Some(ty);
                Ok(())
            }
            Some(Some(prev)) if *prev == ty => Ok(()),
            Some(Some(prev)) => err(
                BindErrorKind::InconsistentParamTypes,
                format!("inconsistent types deduced for parameter ${n}: {prev} versus {ty}"),
                span,
            ),
            None => Ok(()),
        }
    }

    /// Gives an untyped expression the type `to`. A typed expression must already have it.
    fn coerce(&mut self, e: TypedExpr, to: SqlType, span: Span) -> R<TypedExpr> {
        if e.ty == to {
            return Ok(e);
        }
        if e.ty != SqlType::Unknown {
            return mismatch(format!("expected type {to}, found {}", e.ty), span);
        }
        match e.kind {
            ExprNode::Const(Value::Null) => Ok(konst(Value::Null, to)),
            ExprNode::Const(Value::Text(s)) => {
                check_text_literal(&s, to, span)?;
                Ok(konst(Value::Text(s), to))
            }
            ExprNode::Param(n) => {
                self.set_param(n, to, span)?;
                Ok(typed(ExprNode::Param(n), to))
            }
            _ => mismatch(format!("expected type {to}"), span),
        }
    }

    /// Types an expression that nothing else constrains: `text`, as PostgreSQL does.
    fn finish(&mut self, e: TypedExpr, span: Span) -> R<TypedExpr> {
        if e.ty == SqlType::Unknown {
            let to = self.default_type(&e);
            self.coerce(e, to, span)
        } else {
            Ok(e)
        }
    }

    /// The type an unconstrained untyped operand takes: its parameter's earlier type, else `text`.
    fn default_type(&self, e: &TypedExpr) -> SqlType {
        self.known_param(e).unwrap_or(SqlType::Text)
    }

    /// The type an earlier use gave the parameter `e`, if `e` is a parameter.
    fn known_param(&self, e: &TypedExpr) -> Option<SqlType> {
        let ExprNode::Param(n) = &e.kind else {
            return None;
        };
        (*n as usize)
            .checked_sub(1)
            .and_then(|at| self.params.get(at))
            .copied()
            .flatten()
    }

    /// Brings expressions to one comparable type; returns it. Numeric types promote to the widest,
    /// `date` with `timestamptz` to `timestamptz`, untyped operands take the common type.
    fn unify(&mut self, es: Vec<TypedExpr>, span: Span) -> R<(Vec<TypedExpr>, SqlType)> {
        // A parameter that already has a type is an operand of that type, not an untyped one.
        let mut es = es;
        let mut param_typed = false;
        for e in &mut es {
            if e.ty == SqlType::Unknown
                && let Some(t) = self.known_param(e)
            {
                e.ty = t;
                param_typed = true;
            }
        }
        let mut common: Option<SqlType> = None;
        for e in &es {
            if e.ty == SqlType::Unknown {
                continue;
            }
            common = Some(match common {
                None => e.ty,
                Some(c) => match merge(c, e.ty, span) {
                    Ok(t) => t,
                    Err(_) if param_typed => {
                        return err(
                            BindErrorKind::InconsistentParamTypes,
                            format!(
                                "inconsistent types deduced for a parameter: {c} versus {}",
                                e.ty
                            ),
                            span,
                        );
                    }
                    Err(e) => return Err(e),
                },
            });
        }
        let mut out = Vec::with_capacity(es.len());
        for e in es {
            out.push(if e.ty == SqlType::Unknown {
                let to = common.unwrap_or_else(|| self.default_type(&e));
                self.coerce(e, to, span)?
            } else {
                e
            });
        }
        Ok((out, common.unwrap_or(SqlType::Text)))
    }

    /// Checks that `e` can be stored in a column of type `to`, and types it if it is untyped.
    fn assign(&mut self, e: TypedExpr, col: &ColumnInfo, span: Span) -> R<TypedExpr> {
        let to = col.ty;
        if matches!(e.kind, ExprNode::Const(Value::Null)) && col.not_null {
            return err(
                BindErrorKind::NotNullViolation,
                format!(
                    "null value in column \"{}\" violates not-null constraint",
                    col.name
                ),
                span,
            );
        }
        if e.ty == SqlType::Unknown {
            return self.coerce(e, to, span);
        }
        if e.ty == to {
            check_const_range(&e, to, span)?;
            return Ok(e);
        }
        let ok = (e.ty.is_numeric() && to.is_numeric())
            || (e.ty == SqlType::Date && to == SqlType::Timestamptz);
        if !ok {
            return mismatch(
                format!(
                    "column \"{}\" is of type {to} but expression is of type {}",
                    col.name, e.ty
                ),
                span,
            );
        }
        check_const_range(&e, to, span)?;
        Ok(e)
    }

    fn require_bool(&mut self, e: TypedExpr, span: Span) -> R<TypedExpr> {
        let e = if e.ty == SqlType::Unknown {
            self.coerce(e, SqlType::Boolean, span)?
        } else {
            e
        };
        if e.ty != SqlType::Boolean {
            return mismatch(
                format!("argument must be type boolean, not type {}", e.ty),
                span,
            );
        }
        Ok(e)
    }

    fn require_text(&mut self, e: TypedExpr, span: Span) -> R<TypedExpr> {
        let e = if e.ty == SqlType::Unknown {
            self.coerce(e, SqlType::Text, span)?
        } else {
            e
        };
        if e.ty != SqlType::Text {
            return mismatch(format!("expected text, found {}", e.ty), span);
        }
        Ok(e)
    }

    fn require_integer(&mut self, e: TypedExpr, span: Span) -> R<TypedExpr> {
        let e = if e.ty == SqlType::Unknown {
            self.coerce(e, SqlType::Integer, span)?
        } else {
            e
        };
        if !is_integer(e.ty) {
            return mismatch(format!("expected an integer, found {}", e.ty), span);
        }
        Ok(e)
    }

    fn require_numeric(&mut self, e: TypedExpr, span: Span) -> R<TypedExpr> {
        let e = if e.ty == SqlType::Unknown {
            self.coerce(e, SqlType::Numeric, span)?
        } else {
            e
        };
        if !e.ty.is_numeric() {
            return mismatch(format!("expected a number, found {}", e.ty), span);
        }
        Ok(e)
    }

    fn require_time(&mut self, e: TypedExpr, span: Span) -> R<TypedExpr> {
        let e = if e.ty == SqlType::Unknown {
            self.coerce(e, SqlType::Timestamptz, span)?
        } else {
            e
        };
        if !matches!(e.ty, SqlType::Timestamptz | SqlType::Date) {
            return mismatch(
                format!("expected a timestamp or date, found {}", e.ty),
                span,
            );
        }
        Ok(e)
    }

    // ----- expressions -----

    fn expr(&mut self, e: &Expr, scope: &Scope) -> R<TypedExpr> {
        self.depth += 1;
        let out = if self.depth > MAX_DEPTH + 8 {
            err(
                BindErrorKind::TooLarge,
                "expression nests too deeply",
                e.span,
            )
        } else {
            self.expr_inner(e, scope)
        };
        self.depth -= 1;
        out
    }

    fn expr_inner(&mut self, e: &Expr, scope: &Scope) -> R<TypedExpr> {
        let span = e.span;
        match &e.kind {
            ExprKind::Literal(l) => Ok(literal(l)),
            ExprKind::Param(n) => {
                if *n == 0 || *n > MAX_PARAMS {
                    return err(
                        BindErrorKind::UndefinedParameter,
                        format!("there is no parameter ${n}"),
                        span,
                    );
                }
                // Typed only when a context asks, so a second use with another type is reported
                // as inconsistent parameter types (42P08).
                Ok(typed(ExprNode::Param(*n), SqlType::Unknown))
            }
            ExprKind::Column(parts) => column(parts, scope, span),
            ExprKind::Unary { op, expr } => {
                let inner = self.expr(expr, scope)?;
                match op {
                    UnaryOp::Not => {
                        let inner = self.require_bool(inner, span)?;
                        Ok(typed(
                            ExprNode::Unary {
                                op: *op,
                                expr: Box::new(inner),
                            },
                            SqlType::Boolean,
                        ))
                    }
                    UnaryOp::Plus | UnaryOp::Minus => {
                        let inner = self.require_numeric(inner, span)?;
                        if *op == UnaryOp::Minus
                            && let Some(folded) = negate_const(&inner, span)?
                        {
                            return Ok(folded);
                        }
                        let ty = inner.ty;
                        Ok(typed(
                            ExprNode::Unary {
                                op: *op,
                                expr: Box::new(inner),
                            },
                            ty,
                        ))
                    }
                }
            }
            ExprKind::Binary { op, left, right } => {
                let l = self.expr(left, scope)?;
                let r = self.expr(right, scope)?;
                self.binary(*op, l, r, span)
            }
            ExprKind::IsNull { expr, negated } => {
                let inner = self.expr(expr, scope)?;
                let inner = self.finish(inner, span)?;
                Ok(typed(
                    ExprNode::IsNull {
                        expr: Box::new(inner),
                        negated: *negated,
                    },
                    SqlType::Boolean,
                ))
            }
            ExprKind::InList {
                expr,
                list,
                negated,
            } => {
                let mut all = vec![self.expr(expr, scope)?];
                for item in list {
                    all.push(self.expr(item, scope)?);
                }
                let (mut all, _) = self.unify(all, span)?;
                let first = all.remove(0);
                Ok(typed(
                    ExprNode::InList {
                        expr: Box::new(first),
                        list: all,
                        negated: *negated,
                    },
                    SqlType::Boolean,
                ))
            }
            ExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let all = vec![
                    self.expr(expr, scope)?,
                    self.expr(low, scope)?,
                    self.expr(high, scope)?,
                ];
                let (all, _) = self.unify(all, span)?;
                let mut it = all.into_iter();
                match (it.next(), it.next(), it.next()) {
                    (Some(x), Some(lo), Some(hi)) => Ok(typed(
                        ExprNode::Between {
                            expr: Box::new(x),
                            low: Box::new(lo),
                            high: Box::new(hi),
                            negated: *negated,
                        },
                        SqlType::Boolean,
                    )),
                    _ => unsupported("malformed BETWEEN", span),
                }
            }
            ExprKind::Like {
                expr,
                pattern,
                negated,
            } => {
                let x = self.expr(expr, scope)?;
                let x = self.require_text(x, span)?;
                let p = self.expr(pattern, scope)?;
                let p = self.require_text(p, span)?;
                Ok(typed(
                    ExprNode::Like {
                        expr: Box::new(x),
                        pattern: Box::new(p),
                        negated: *negated,
                    },
                    SqlType::Boolean,
                ))
            }
            ExprKind::Cast { expr, data_type } => {
                if matches!(
                    data_type,
                    ast::DataType::Numeric(Some(_)) | ast::DataType::Varchar(Some(_))
                ) {
                    return unsupported("CAST to a type with a precision, scale or length", span);
                }
                let to = SqlType::from_data_type(*data_type);
                let inner = self.expr(expr, scope)?;
                self.cast(inner, to, span)
            }
            ExprKind::Function(f) => self.function(f, scope, span),
            ExprKind::Subquery(_) | ExprKind::InSubquery { .. } => unsupported("subqueries", span),
            ExprKind::Position { needle, haystack } => {
                let n = self.expr(needle, scope)?;
                let n = self.require_text(n, span)?;
                let h = self.expr(haystack, scope)?;
                let h = self.require_text(h, span)?;
                Ok(typed(
                    ExprNode::Function {
                        name: "position".to_string(),
                        args: vec![n, h],
                    },
                    SqlType::Integer,
                ))
            }
            ExprKind::Trim { side, chars, expr } => {
                let x = self.expr(expr, scope)?;
                let mut args = vec![self.require_text(x, span)?];
                if let Some(c) = chars {
                    let c = self.expr(c, scope)?;
                    args.push(self.require_text(c, span)?);
                }
                let name = match side {
                    TrimSide::Both => "trim_both",
                    TrimSide::Leading => "trim_leading",
                    TrimSide::Trailing => "trim_trailing",
                };
                Ok(typed(
                    ExprNode::Function {
                        name: name.to_string(),
                        args,
                    },
                    SqlType::Text,
                ))
            }
            ExprKind::Extract { field, expr } => {
                const FIELDS: &[&str] = &[
                    "year", "quarter", "month", "week", "day", "dow", "doy", "hour", "minute",
                    "second", "epoch",
                ];
                if !FIELDS.contains(&field.value.as_str()) {
                    return err(
                        BindErrorKind::InvalidParameterValue,
                        format!("unit \"{}\" not recognized for extract", field.value),
                        field.span,
                    );
                }
                let x = self.expr(expr, scope)?;
                let x = self.require_time(x, span)?;
                Ok(typed(
                    ExprNode::Function {
                        name: "extract".to_string(),
                        args: vec![konst(Value::Text(field.value.clone()), SqlType::Text), x],
                    },
                    SqlType::Numeric,
                ))
            }
        }
    }

    fn cast(&mut self, inner: TypedExpr, to: SqlType, span: Span) -> R<TypedExpr> {
        if inner.ty == SqlType::Unknown {
            return match inner.kind {
                ExprNode::Const(Value::Null) => Ok(konst(Value::Null, to)),
                ExprNode::Param(_) => self.coerce(inner, to, span),
                _ => {
                    // A string literal keeps its text and converts when the cast runs.
                    let text = self.coerce(inner, SqlType::Text, span)?;
                    Ok(typed(
                        ExprNode::Cast {
                            expr: Box::new(text),
                        },
                        to,
                    ))
                }
            };
        }
        if inner.ty == to {
            return Ok(inner);
        }
        let from = inner.ty;
        let ok = (from.is_numeric() && to.is_numeric())
            || to == SqlType::Text
            || from == SqlType::Text
            || matches!(
                (from, to),
                (SqlType::Date, SqlType::Timestamptz)
                    | (SqlType::Timestamptz, SqlType::Date)
                    | (SqlType::Boolean, SqlType::Integer)
                    | (SqlType::Integer, SqlType::Boolean)
            );
        if !ok {
            return err(
                BindErrorKind::CannotCoerce,
                format!("cannot cast type {from} to {to}"),
                span,
            );
        }
        Ok(typed(
            ExprNode::Cast {
                expr: Box::new(inner),
            },
            to,
        ))
    }

    fn binary(&mut self, op: BinaryOp, l: TypedExpr, r: TypedExpr, span: Span) -> R<TypedExpr> {
        use BinaryOp::{
            Add, And, Concat, Contains, Div, Eq, Gt, GtEq, JsonGet, JsonGetText, Lt, LtEq, Mod,
            Mul, NotEq, Or, Sub,
        };
        let (l, r, ty) = match op {
            And | Or => (
                self.require_bool(l, span)?,
                self.require_bool(r, span)?,
                SqlType::Boolean,
            ),
            Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                let (mut v, _) = self.unify(vec![l, r], span)?;
                let r = v.pop();
                let l = v.pop();
                match (l, r) {
                    (Some(l), Some(r)) => (l, r, SqlType::Boolean),
                    _ => return unsupported("malformed comparison", span),
                }
            }
            Concat => {
                let (mut v, c) = self.unify(vec![l, r], span)?;
                if !matches!(c, SqlType::Text | SqlType::Bytea) {
                    return mismatch(format!("operator || is not defined for {c}"), span);
                }
                let r = v.pop();
                let l = v.pop();
                match (l, r) {
                    (Some(l), Some(r)) => (l, r, c),
                    _ => return unsupported("malformed concatenation", span),
                }
            }
            Add | Sub | Mul | Div | Mod => {
                let (mut v, c) = self.unify(vec![l, r], span)?;
                if !c.is_numeric() {
                    return mismatch(format!("arithmetic is not defined for {c}"), span);
                }
                let r = v.pop();
                let l = v.pop();
                let (Some(l), Some(r)) = (l, r) else {
                    return unsupported("malformed arithmetic", span);
                };
                if let Some(folded) = fold_arithmetic(op, &l, &r, c, span)? {
                    return Ok(folded);
                }
                (l, r, c)
            }
            JsonGet | JsonGetText => {
                let l = self.coerce_or_expect(l, SqlType::Jsonb, span)?;
                let r = if r.ty == SqlType::Unknown {
                    self.coerce(r, SqlType::Text, span)?
                } else {
                    r
                };
                if !(r.ty == SqlType::Text || is_integer(r.ty)) {
                    return mismatch(
                        format!("jsonb key must be text or integer, not {}", r.ty),
                        span,
                    );
                }
                let ty = if op == JsonGet {
                    SqlType::Jsonb
                } else {
                    SqlType::Text
                };
                (l, r, ty)
            }
            Contains => (
                self.coerce_or_expect(l, SqlType::Jsonb, span)?,
                self.coerce_or_expect(r, SqlType::Jsonb, span)?,
                SqlType::Boolean,
            ),
        };
        Ok(typed(
            ExprNode::Binary {
                op,
                left: Box::new(l),
                right: Box::new(r),
            },
            ty,
        ))
    }

    /// Types an untyped operand as `ty`, or checks a typed one already has it.
    fn coerce_or_expect(&mut self, e: TypedExpr, ty: SqlType, span: Span) -> R<TypedExpr> {
        self.coerce(e, ty, span)
    }

    fn function(&mut self, f: &ast::Function, scope: &Scope, span: Span) -> R<TypedExpr> {
        let name = f.name.as_str();
        if matches!(name, "count" | "sum" | "min" | "max" | "avg") {
            return unsupported("aggregate functions", span);
        }
        let list = match &f.args {
            FunctionArgs::Star => return unsupported("aggregate functions", span),
            FunctionArgs::List(l) => l,
        };
        let mut args = Vec::with_capacity(list.len());
        for a in list {
            args.push(self.expr(a, scope)?);
        }
        let n = args.len();
        let bad_arity = |expected: &str| -> R<TypedExpr> {
            err(
                BindErrorKind::UndefinedFunction,
                format!("function {name} takes {expected} argument(s), not {n}"),
                span,
            )
        };
        let call = |args: Vec<TypedExpr>, ty: SqlType| {
            typed(
                ExprNode::Function {
                    name: name.to_string(),
                    args,
                },
                ty,
            )
        };
        match name {
            "coalesce" => {
                if n == 0 {
                    return bad_arity("at least 1");
                }
                let (args, ty) = self.unify(args, span)?;
                Ok(call(args, ty))
            }
            "nullif" => {
                if n != 2 {
                    return bad_arity("2");
                }
                let (args, ty) = self.unify(args, span)?;
                Ok(call(args, ty))
            }
            "lower" | "upper" => {
                if n != 1 {
                    return bad_arity("1");
                }
                let a = self.require_text(take(args), span)?;
                Ok(call(vec![a], SqlType::Text))
            }
            "length" => {
                if n != 1 {
                    return bad_arity("1");
                }
                let a = take(args);
                let a = if a.ty == SqlType::Unknown {
                    self.coerce(a, SqlType::Text, span)?
                } else {
                    a
                };
                if !matches!(a.ty, SqlType::Text | SqlType::Bytea) {
                    return mismatch(format!("length is not defined for {}", a.ty), span);
                }
                Ok(call(vec![a], SqlType::Integer))
            }
            "substring" => {
                if !(2..=3).contains(&n) {
                    return bad_arity("2 or 3");
                }
                let mut it = args.into_iter();
                let mut out = Vec::with_capacity(n);
                if let Some(s) = it.next() {
                    out.push(self.require_text(s, span)?);
                }
                for a in it {
                    out.push(self.require_integer(a, span)?);
                }
                Ok(call(out, SqlType::Text))
            }
            "abs" => {
                if n != 1 {
                    return bad_arity("1");
                }
                let a = self.require_numeric(take(args), span)?;
                let ty = a.ty;
                Ok(call(vec![a], ty))
            }
            "round" => {
                if !(1..=2).contains(&n) {
                    return bad_arity("1 or 2");
                }
                let mut it = args.into_iter();
                let mut out = Vec::with_capacity(n);
                if let Some(x) = it.next() {
                    out.push(self.require_numeric(x, span)?);
                }
                for d in it {
                    out.push(self.require_integer(d, span)?);
                }
                let ty = match out.first().map(|a| a.ty) {
                    Some(SqlType::DoublePrecision) => SqlType::DoublePrecision,
                    _ => SqlType::Numeric,
                };
                Ok(call(out, ty))
            }
            "now" => {
                if n != 0 {
                    return bad_arity("0");
                }
                Ok(call(Vec::new(), SqlType::Timestamptz))
            }
            "date_trunc" => {
                if n != 2 {
                    return bad_arity("2");
                }
                let mut it = args.into_iter();
                let (Some(field), Some(ts)) = (it.next(), it.next()) else {
                    return bad_arity("2");
                };
                let field = self.require_text(field, span)?;
                if let ExprNode::Const(Value::Text(s)) = &field.kind {
                    const UNITS: &[&str] = &[
                        "second", "minute", "hour", "day", "week", "month", "quarter", "year",
                    ];
                    if !UNITS.contains(&s.to_ascii_lowercase().as_str()) {
                        return err(
                            BindErrorKind::InvalidParameterValue,
                            format!("unit \"{s}\" not recognized for date_trunc"),
                            span,
                        );
                    }
                }
                let ts = self.require_time(ts, span)?;
                Ok(call(vec![field, ts], SqlType::Timestamptz))
            }
            _ => err(
                BindErrorKind::UndefinedFunction,
                format!("function {name} does not exist"),
                span,
            ),
        }
    }

    // ----- queries -----

    fn lookup_table_factor(&self, f: &ast::TableFactor) -> R<Scope> {
        self.scope(&f.name, f.alias.as_ref())
    }

    /// Binds a `SELECT`. `targets` are the column types an `INSERT ... SELECT` stores into.
    fn query(&mut self, q: &Query, targets: Option<&[ColumnInfo]>) -> R<LogicalPlan> {
        if q.distinct {
            return unsupported("DISTINCT", Span::default());
        }
        if !q.group_by.is_empty() || q.having.is_some() {
            return unsupported("GROUP BY and HAVING", Span::default());
        }
        let (scope, mut plan) = match q.from.as_slice() {
            // No FROM clause: one row with no columns, so only constants and functions bind.
            [] => (Scope::empty(), LogicalPlan::OneRow),
            [one] if one.joins.is_empty() => {
                let scope = self.lookup_table_factor(&one.relation)?;
                let as_of = match &one.relation.as_of {
                    Some(a) => Some(self.as_of(a)?),
                    None => None,
                };
                let plan = LogicalPlan::Scan {
                    table: scope.info.name.clone(),
                    table_id: scope.info.id,
                    as_of,
                };
                (scope, plan)
            }
            _ => return unsupported("joins and multi-table FROM", Span::default()),
        };
        if let Some(w) = &q.selection {
            let p = self.expr(w, &scope)?;
            let predicate = self.require_bool(p, w.span)?;
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }

        let mut columns = self.select_list(&q.projection, &scope)?;
        if let Some(targets) = targets {
            for (c, t) in columns.iter_mut().zip(targets) {
                let e = std::mem::replace(&mut c.expr, konst(Value::Null, SqlType::Unknown));
                c.expr = self.assign(e, t, Span::default())?;
            }
        }
        for c in &mut columns {
            let e = std::mem::replace(&mut c.expr, konst(Value::Null, SqlType::Unknown));
            c.expr = self.finish(e, Span::default())?;
        }

        if !q.order_by.is_empty() {
            let mut keys = Vec::with_capacity(q.order_by.len());
            for item in &q.order_by {
                let expr = self.order_key(&item.expr, &columns, &scope)?;
                let expr = self.finish(expr, item.expr.span)?;
                keys.push(SortKey {
                    expr,
                    desc: item.desc,
                    nulls_first: item.nulls_first.unwrap_or(item.desc),
                });
            }
            plan = LogicalPlan::Sort {
                input: Box::new(plan),
                keys,
            };
        }
        plan = LogicalPlan::Project {
            input: Box::new(plan),
            columns,
        };
        if q.limit.is_some() || q.offset.is_some() {
            let limit = match &q.limit {
                Some(e) => Some(self.row_count(e, BindErrorKind::NegativeLimit)?),
                None => None,
            };
            let offset = match &q.offset {
                Some(e) => Some(self.row_count(e, BindErrorKind::NegativeOffset)?),
                None => None,
            };
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                limit,
                offset,
            };
        }
        Ok(plan)
    }

    fn row_count(&mut self, e: &Expr, negative: BindErrorKind) -> R<TypedExpr> {
        let b = self.expr(e, &Scope::empty())?;
        let b = if b.ty == SqlType::Unknown {
            self.coerce(b, SqlType::BigInt, e.span)?
        } else {
            b
        };
        if !is_integer(b.ty) {
            return mismatch(
                format!("argument of LIMIT/OFFSET must be an integer, not {}", b.ty),
                e.span,
            );
        }
        if let ExprNode::Const(Value::Int(v)) = &b.kind
            && *v < 0
        {
            return err(negative, "LIMIT/OFFSET must not be negative", e.span);
        }
        Ok(b)
    }

    fn as_of(&mut self, a: &AsOf) -> R<AsOfPlan> {
        let empty = Scope::empty();
        match a {
            AsOf::Changeset(e) => {
                let b = self.expr(e, &empty)?;
                let b = if b.ty == SqlType::Unknown {
                    self.coerce(b, SqlType::Text, e.span)?
                } else {
                    b
                };
                if b.ty != SqlType::Text && !is_integer(b.ty) {
                    return mismatch(
                        format!("a changeset id is text or an integer, not {}", b.ty),
                        e.span,
                    );
                }
                Ok(AsOfPlan::Changeset(b))
            }
            AsOf::Timestamp(e) => {
                let b = self.expr(e, &empty)?;
                let b = self.require_time(b, e.span)?;
                Ok(AsOfPlan::Timestamp(b))
            }
        }
    }

    fn select_list(&mut self, items: &[SelectItem], scope: &Scope) -> R<Vec<OutputColumn>> {
        let mut out = Vec::new();
        for item in items {
            match item {
                SelectItem::Wildcard if scope.is_row_source_empty() => {
                    return err(
                        BindErrorKind::Syntax,
                        "SELECT * with no tables specified is not valid",
                        Span::default(),
                    );
                }
                SelectItem::Wildcard => out.extend(all_columns(scope)),
                SelectItem::QualifiedWildcard(q) => {
                    if q.value != scope.qualifier {
                        return err(
                            BindErrorKind::UndefinedTable,
                            format!("missing FROM-clause entry for table \"{}\"", q.value),
                            q.span,
                        );
                    }
                    out.extend(all_columns(scope));
                }
                SelectItem::Expr { expr, alias } => {
                    let bound = self.expr(expr, scope)?;
                    let name = alias
                        .as_ref()
                        .map_or_else(|| output_name(expr), |a| a.value.clone());
                    out.push(OutputColumn { expr: bound, name });
                }
            }
        }
        Ok(out)
    }

    /// An `ORDER BY` key: an output name, a one-based position, or an expression over the table.
    fn order_key(&mut self, e: &Expr, outputs: &[OutputColumn], scope: &Scope) -> R<TypedExpr> {
        match &e.kind {
            ExprKind::Column(parts) if parts.len() == 1 => {
                if let Some(first) = parts.first() {
                    let mut hits = outputs.iter().filter(|c| c.name == first.value);
                    if let Some(c) = hits.next() {
                        if hits.any(|o| o.expr != c.expr) {
                            return err(
                                BindErrorKind::AmbiguousColumn,
                                format!("ORDER BY \"{}\" is ambiguous", first.value),
                                e.span,
                            );
                        }
                        return Ok(c.expr.clone());
                    }
                }
            }
            ExprKind::Literal(Literal::Number(n)) => {
                if let Ok(pos) = n.parse::<usize>() {
                    return match pos.checked_sub(1).and_then(|i| outputs.get(i)) {
                        Some(c) => Ok(c.expr.clone()),
                        None => err(
                            BindErrorKind::InvalidColumnReference,
                            format!("ORDER BY position {pos} is not in select list"),
                            e.span,
                        ),
                    };
                }
            }
            _ => {}
        }
        self.expr(e, scope)
    }

    // ----- data changes -----

    fn returning(&mut self, items: &[SelectItem], scope: &Scope) -> R<Vec<OutputColumn>> {
        let mut cols = self.select_list(items, scope)?;
        for c in &mut cols {
            let e = std::mem::replace(&mut c.expr, konst(Value::Null, SqlType::Unknown));
            c.expr = self.finish(e, Span::default())?;
        }
        Ok(cols)
    }

    fn insert(&mut self, i: &ast::Insert) -> R<LogicalPlan> {
        let scope = self.scope(&i.table, None)?;
        let cols = scope.info.columns.clone();
        let listed: Option<Vec<usize>> = if i.columns.is_empty() {
            None
        } else {
            let mut t: Vec<usize> = Vec::with_capacity(i.columns.len());
            for c in &i.columns {
                let Some(at) = cols.iter().position(|x| x.name == c.value) else {
                    return err(
                        BindErrorKind::UndefinedColumn,
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            c.value, scope.info.name
                        ),
                        c.span,
                    );
                };
                if t.contains(&at) {
                    return err(
                        BindErrorKind::DuplicateColumn,
                        format!("column \"{}\" specified more than once", c.value),
                        c.span,
                    );
                }
                t.push(at);
            }
            Some(t)
        };
        let too_many = || {
            err(
                BindErrorKind::Syntax,
                "INSERT has more expressions than target columns",
                Span::default(),
            )
        };
        let (targets, source) = match &i.source {
            ast::InsertSource::Values(rows) => {
                // Without a column list the values fill the leading columns.
                let targets = match listed {
                    Some(t) => t,
                    None => {
                        let width = rows.first().map_or(0, Vec::len);
                        if width > cols.len() {
                            return too_many();
                        }
                        (0..width).collect()
                    }
                };
                let target_cols: Vec<ColumnInfo> = targets
                    .iter()
                    .filter_map(|&t| cols.get(t).cloned())
                    .collect();
                let empty = Scope::empty();
                let mut out = Vec::with_capacity(rows.len());
                for row in rows {
                    let at = row.first().map(|e| e.span).unwrap_or_default();
                    if row.len() > target_cols.len() {
                        return err(
                            BindErrorKind::Syntax,
                            "INSERT has more expressions than target columns",
                            at,
                        );
                    }
                    if row.len() < target_cols.len() {
                        return err(
                            BindErrorKind::Syntax,
                            "INSERT has more target columns than expressions",
                            at,
                        );
                    }
                    let mut bound = Vec::with_capacity(row.len());
                    for (e, col) in row.iter().zip(&target_cols) {
                        let b = self.expr(e, &empty)?;
                        bound.push(self.assign(b, col, e.span)?);
                    }
                    out.push(bound);
                }
                (targets, InsertSource::Values(out))
            }
            ast::InsertSource::Query(q) => {
                let target_cols: Vec<ColumnInfo> = match &listed {
                    Some(t) => t.iter().filter_map(|&t| cols.get(t).cloned()).collect(),
                    None => cols.clone(),
                };
                let plan = self.query(q, Some(&target_cols))?;
                let got = plan.output().map_or(0, <[OutputColumn]>::len);
                let targets = match listed {
                    Some(t) => {
                        if got != t.len() {
                            return err(
                                BindErrorKind::Syntax,
                                "INSERT has a different number of expressions than target columns",
                                Span::default(),
                            );
                        }
                        t
                    }
                    None => {
                        if got > cols.len() {
                            return too_many();
                        }
                        (0..got).collect()
                    }
                };
                (targets, InsertSource::Query(Box::new(plan)))
            }
        };
        for (at, c) in cols.iter().enumerate() {
            if c.not_null && !c.has_default && !targets.contains(&at) {
                return err(
                    BindErrorKind::NotNullViolation,
                    format!(
                        "null value in column \"{}\" violates not-null constraint",
                        c.name
                    ),
                    Span::default(),
                );
            }
        }
        Ok(LogicalPlan::Insert {
            table: scope.info.name.clone(),
            table_id: scope.info.id,
            targets,
            source,
            returning: self.returning(&i.returning, &scope)?,
        })
    }
    fn update(&mut self, u: &ast::Update) -> R<LogicalPlan> {
        let scope = self.scope(&u.table, u.alias.as_ref())?;
        let mut assignments: Vec<(usize, TypedExpr)> = Vec::with_capacity(u.assignments.len());
        for a in &u.assignments {
            let Some(at) = scope
                .info
                .columns
                .iter()
                .position(|c| c.name == a.column.value)
            else {
                return err(
                    BindErrorKind::UndefinedColumn,
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        a.column.value, scope.info.name
                    ),
                    a.column.span,
                );
            };
            if assignments.iter().any(|(i, _)| *i == at) {
                // PostgreSQL reports a repeated assignment target as a syntax error.
                return err(
                    BindErrorKind::Syntax,
                    format!("column \"{}\" specified more than once", a.column.value),
                    a.column.span,
                );
            }
            let v = self.expr(&a.value, &scope)?;
            let Some(col) = scope.info.columns.get(at) else {
                return unsupported("column position out of range", a.column.span);
            };
            let col = col.clone();
            assignments.push((at, self.assign(v, &col, a.value.span)?));
        }
        let input = self.filtered_scan(&scope, u.selection.as_ref())?;
        Ok(LogicalPlan::Update {
            table: scope.info.name.clone(),
            table_id: scope.info.id,
            input: Box::new(input),
            assignments,
            returning: self.returning(&u.returning, &scope)?,
        })
    }

    fn delete(&mut self, d: &ast::Delete) -> R<LogicalPlan> {
        let scope = self.scope(&d.table, d.alias.as_ref())?;
        let input = self.filtered_scan(&scope, d.selection.as_ref())?;
        Ok(LogicalPlan::Delete {
            table: scope.info.name.clone(),
            table_id: scope.info.id,
            input: Box::new(input),
            returning: self.returning(&d.returning, &scope)?,
        })
    }

    fn filtered_scan(&mut self, scope: &Scope, selection: Option<&Expr>) -> R<LogicalPlan> {
        let scan = LogicalPlan::Scan {
            table: scope.info.name.clone(),
            table_id: scope.info.id,
            as_of: None,
        };
        match selection {
            None => Ok(scan),
            Some(w) => {
                let p = self.expr(w, scope)?;
                let predicate = self.require_bool(p, w.span)?;
                Ok(LogicalPlan::Filter {
                    input: Box::new(scan),
                    predicate,
                })
            }
        }
    }
}

fn take(args: Vec<TypedExpr>) -> TypedExpr {
    args.into_iter()
        .next()
        .unwrap_or_else(|| konst(Value::Null, SqlType::Unknown))
}

fn merge(a: SqlType, b: SqlType, span: Span) -> R<SqlType> {
    if a == b {
        return Ok(a);
    }
    match (a.numeric_rank(), b.numeric_rank()) {
        (Some(x), Some(y)) => Ok(if x >= y { a } else { b }),
        _ => match (a, b) {
            (SqlType::Date, SqlType::Timestamptz) | (SqlType::Timestamptz, SqlType::Date) => {
                Ok(SqlType::Timestamptz)
            }
            _ => mismatch(format!("types {a} and {b} cannot be matched"), span),
        },
    }
}

fn literal(l: &Literal) -> TypedExpr {
    match l {
        Literal::Null => konst(Value::Null, SqlType::Unknown),
        Literal::Boolean(b) => konst(Value::Bool(*b), SqlType::Boolean),
        Literal::String(s) => konst(Value::Text(s.clone()), SqlType::Unknown),
        Literal::Number(n) => {
            if n.bytes().all(|b| b.is_ascii_digit())
                && let Ok(v) = n.parse::<i64>()
            {
                let ty = if i32::try_from(v).is_ok() {
                    SqlType::Integer
                } else {
                    SqlType::BigInt
                };
                return konst(Value::Int(v), ty);
            }
            konst(Value::Number(n.clone()), SqlType::Numeric)
        }
    }
}

fn column(parts: &[ast::Ident], scope: &Scope, span: Span) -> R<TypedExpr> {
    let (qualifier, name) = match parts {
        [c] => (None, c),
        [q, c] => (Some(q), c),
        _ => return unsupported("schema-qualified column references", span),
    };
    if let Some(q) = qualifier
        && q.value != scope.qualifier
    {
        return err(
            BindErrorKind::UndefinedTable,
            format!("missing FROM-clause entry for table \"{}\"", q.value),
            q.span,
        );
    }
    match scope
        .info
        .columns
        .iter()
        .enumerate()
        .find(|(_, c)| c.name == name.value)
    {
        Some((index, c)) => Ok(typed(
            ExprNode::Column {
                index,
                name: c.name.clone(),
            },
            c.ty,
        )),
        None => err(
            BindErrorKind::UndefinedColumn,
            format!("column \"{}\" does not exist", name.value),
            name.span,
        ),
    }
}

fn all_columns(scope: &Scope) -> Vec<OutputColumn> {
    scope
        .info
        .columns
        .iter()
        .enumerate()
        .map(|(index, c)| OutputColumn {
            expr: typed(
                ExprNode::Column {
                    index,
                    name: c.name.clone(),
                },
                c.ty,
            ),
            name: c.name.clone(),
        })
        .collect()
}

/// The name PostgreSQL gives an unaliased output column.
fn output_name(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Column(parts) => parts.last().map_or_else(unnamed, |p| p.value.clone()),
        ExprKind::Function(f) => f.name.clone(),
        ExprKind::Cast { expr, .. } => match &expr.kind {
            ExprKind::Column(_) | ExprKind::Function(_) | ExprKind::Cast { .. } => {
                output_name(expr)
            }
            _ => unnamed(),
        },
        ExprKind::Position { .. } => "position".to_string(),
        ExprKind::Trim { .. } => "btrim".to_string(),
        ExprKind::Extract { .. } => "extract".to_string(),
        _ => unnamed(),
    }
}

fn unnamed() -> String {
    "?column?".to_string()
}

/// Checks a string literal can be a value of `to`. Types with no cheap check accept any text.
fn check_text_literal(s: &str, to: SqlType, span: Span) -> R<()> {
    let bad = || {
        err(
            BindErrorKind::InvalidText,
            format!("invalid input syntax for type {to}: \"{s}\""),
            span,
        )
    };
    match to {
        SqlType::Boolean => {
            let t = s.trim().to_ascii_lowercase();
            if matches!(
                t.as_str(),
                "t" | "true" | "y" | "yes" | "on" | "1" | "f" | "false" | "n" | "no" | "off" | "0"
            ) {
                Ok(())
            } else {
                bad()
            }
        }
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => {
            let t = s.trim();
            let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return bad();
            }
            match (t.parse::<i128>(), int_range(to)) {
                (Ok(v), Some((lo, hi))) if v >= lo && v <= hi => Ok(()),
                _ => err(
                    BindErrorKind::NumericOutOfRange,
                    format!("value \"{s}\" is out of range for type {to}"),
                    span,
                ),
            }
        }
        SqlType::Numeric | SqlType::DoublePrecision => {
            if is_decimal(s) {
                Ok(())
            } else {
                bad()
            }
        }
        _ => Ok(()),
    }
}

fn check_const_range(e: &TypedExpr, to: SqlType, span: Span) -> R<()> {
    if let (ExprNode::Const(Value::Int(v)), Some((lo, hi))) = (&e.kind, int_range(to)) {
        let v = i128::from(*v);
        if v < lo || v > hi {
            return err(
                BindErrorKind::NumericOutOfRange,
                format!("value {v} is out of range for type {to}"),
                span,
            );
        }
    }
    Ok(())
}

fn negate_const(e: &TypedExpr, span: Span) -> R<Option<TypedExpr>> {
    match &e.kind {
        ExprNode::Const(Value::Int(v)) => {
            let neg = -i128::from(*v);
            let (lo, hi) = int_range(e.ty).unwrap_or((i128::MIN, i128::MAX));
            if neg < lo || neg > hi {
                return err(
                    BindErrorKind::NumericOutOfRange,
                    format!("{} out of range", e.ty),
                    span,
                );
            }
            match i64::try_from(neg) {
                Ok(n) => Ok(Some(konst(Value::Int(n), e.ty))),
                Err(_) => Ok(None),
            }
        }
        ExprNode::Const(Value::Number(n)) => {
            let text = match n.strip_prefix('-') {
                Some(rest) => rest.to_string(),
                None => format!("-{n}"),
            };
            Ok(Some(konst(Value::Number(text), e.ty)))
        }
        _ => Ok(None),
    }
}

/// Folds `a op b` when both are integer constants; `ty` is the common numeric type.
fn fold_arithmetic(
    op: BinaryOp,
    l: &TypedExpr,
    r: &TypedExpr,
    ty: SqlType,
    span: Span,
) -> R<Option<TypedExpr>> {
    let (ExprNode::Const(Value::Int(a)), ExprNode::Const(Value::Int(b))) = (&l.kind, &r.kind)
    else {
        return Ok(None);
    };
    if !is_integer(ty) {
        return Ok(None);
    }
    let (a, b) = (i128::from(*a), i128::from(*b));
    let v = match op {
        BinaryOp::Add => a + b,
        BinaryOp::Sub => a - b,
        BinaryOp::Mul => a * b,
        BinaryOp::Div | BinaryOp::Mod => {
            if b == 0 {
                return err(BindErrorKind::DivisionByZero, "division by zero", span);
            }
            if op == BinaryOp::Div { a / b } else { a % b }
        }
        _ => return Ok(None),
    };
    let (lo, hi) = int_range(ty).unwrap_or((i128::MIN, i128::MAX));
    if v < lo || v > hi {
        return err(
            BindErrorKind::NumericOutOfRange,
            format!("{ty} out of range"),
            span,
        );
    }
    match i64::try_from(v) {
        Ok(n) => Ok(Some(konst(Value::Int(n), ty))),
        Err(_) => Ok(None),
    }
}
