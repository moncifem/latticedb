//! The logical plan for single-table statements.
//!
//! A plan is plain data produced by [`crate::binder::bind`]: names are resolved to column positions
//! and every expression carries its type. There is no execution here and no dependency on a storage
//! engine; the [`CatalogView`] trait is how a host tells the binder what tables exist.
//!
//! Plans print through `Display` as an indented tree, one operator per line, children below their
//! parent. The text is for review and golden tests, not a stable interface.
//!
//! Variant and field names follow the SQL they stand for, so they are not documented one by one.
#![allow(missing_docs)]

use std::fmt::{self, Display, Formatter, Write as _};

use crate::ast::{BinaryOp, DataType, UnaryOp};

/// The type of a bound expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlType {
    Boolean,
    SmallInt,
    Integer,
    BigInt,
    /// Exact decimal. Precision and scale are not tracked by the binder.
    Numeric,
    DoublePrecision,
    /// `text` and `varchar(n)`; the length limit is a column constraint checked at execution.
    Text,
    Bytea,
    Timestamptz,
    Date,
    Uuid,
    Jsonb,
    /// A `NULL`, untyped string literal or parameter that no context has typed yet. It never
    /// survives in a finished plan.
    Unknown,
}

impl SqlType {
    /// The type a declared column type has in expressions.
    pub fn from_data_type(t: DataType) -> SqlType {
        match t {
            DataType::Boolean => SqlType::Boolean,
            DataType::SmallInt => SqlType::SmallInt,
            DataType::Integer => SqlType::Integer,
            DataType::BigInt => SqlType::BigInt,
            DataType::DoublePrecision => SqlType::DoublePrecision,
            DataType::Numeric(_) => SqlType::Numeric,
            DataType::Text | DataType::Varchar(_) => SqlType::Text,
            DataType::Bytea => SqlType::Bytea,
            DataType::Timestamptz => SqlType::Timestamptz,
            DataType::Date => SqlType::Date,
            DataType::Uuid => SqlType::Uuid,
            DataType::Jsonb => SqlType::Jsonb,
        }
    }

    /// Position in the numeric promotion order, or `None` for a non-numeric type.
    pub(crate) fn numeric_rank(self) -> Option<u8> {
        match self {
            SqlType::SmallInt => Some(1),
            SqlType::Integer => Some(2),
            SqlType::BigInt => Some(3),
            SqlType::Numeric => Some(4),
            SqlType::DoublePrecision => Some(5),
            _ => None,
        }
    }

    /// Whether the type is one of the numeric types.
    pub fn is_numeric(self) -> bool {
        self.numeric_rank().is_some()
    }
}

impl Display for SqlType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SqlType::Boolean => "boolean",
            SqlType::SmallInt => "smallint",
            SqlType::Integer => "integer",
            SqlType::BigInt => "bigint",
            SqlType::Numeric => "numeric",
            SqlType::DoublePrecision => "double precision",
            SqlType::Text => "text",
            SqlType::Bytea => "bytea",
            SqlType::Timestamptz => "timestamptz",
            SqlType::Date => "date",
            SqlType::Uuid => "uuid",
            SqlType::Jsonb => "jsonb",
            SqlType::Unknown => "unknown",
        })
    }
}

/// One column of a table as the catalog describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub ty: SqlType,
    pub not_null: bool,
    /// The column has a `DEFAULT`, so an insert may leave it out.
    pub has_default: bool,
}

/// An index on a table. The binder does not choose indexes; they are listed so a planner can.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// A table as the catalog describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
}

/// What the binder needs to know about the database. Implemented by the host; this crate has no
/// storage dependency.
pub trait CatalogView {
    /// The table called `name` (already case-folded), or `None`.
    fn table(&self, name: &str) -> Option<TableInfo>;
}

/// A constant.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// An integer of `smallint`, `integer` or `bigint` type.
    Int(i64),
    /// A decimal number as written, for `numeric` and `double precision`.
    Number(String),
    Text(String),
}

/// A bound expression.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedExpr {
    pub kind: ExprNode,
    pub ty: SqlType,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprNode {
    /// A column of the table being read, by position in the table's column list.
    Column {
        index: usize,
        name: String,
    },
    Const(Value),
    /// `$n`, one-based. The type is in [`TypedExpr::ty`] and in [`BoundStatement::param_types`].
    Param(u32),
    Unary {
        op: UnaryOp,
        expr: Box<TypedExpr>,
    },
    /// Operands keep their own numeric types; the executor promotes to the wider one.
    Binary {
        op: BinaryOp,
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    IsNull {
        expr: Box<TypedExpr>,
        negated: bool,
    },
    InList {
        expr: Box<TypedExpr>,
        list: Vec<TypedExpr>,
        negated: bool,
    },
    Between {
        expr: Box<TypedExpr>,
        low: Box<TypedExpr>,
        high: Box<TypedExpr>,
        negated: bool,
    },
    Like {
        expr: Box<TypedExpr>,
        pattern: Box<TypedExpr>,
        negated: bool,
    },
    Cast {
        expr: Box<TypedExpr>,
    },
    /// A scalar function: `coalesce`, `nullif`, `lower`, `upper`, `length`, `substring`, `position`,
    /// `trim`, `abs`, `round`, `now`, `date_trunc`, `extract`.
    Function {
        name: String,
        args: Vec<TypedExpr>,
    },
}

/// An output column: an expression and the name the client sees.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputColumn {
    pub expr: TypedExpr,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    pub expr: TypedExpr,
    pub desc: bool,
    /// Resolved: `NULLS LAST` for ascending and `NULLS FIRST` for descending when not written.
    pub nulls_first: bool,
}

/// The snapshot a scan reads, from `AS OF`.
#[derive(Clone, Debug, PartialEq)]
pub enum AsOfPlan {
    Changeset(TypedExpr),
    Timestamp(TypedExpr),
}

/// Where an insert gets its rows.
#[derive(Clone, Debug, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<TypedExpr>>),
    Query(Box<LogicalPlan>),
}

/// The relational and write operators of the single-table subset.
#[derive(Clone, Debug, PartialEq)]
pub enum LogicalPlan {
    /// Every column of the table, in catalog order.
    Scan {
        table: String,
        as_of: Option<AsOfPlan>,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: TypedExpr,
    },
    Project {
        input: Box<LogicalPlan>,
        columns: Vec<OutputColumn>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Box<LogicalPlan>,
        limit: Option<TypedExpr>,
        offset: Option<TypedExpr>,
    },
    /// Targets are table column positions, in the order the source rows list them. Columns left
    /// out take their default or `NULL`.
    Insert {
        table: String,
        targets: Vec<usize>,
        source: InsertSource,
        returning: Vec<OutputColumn>,
    },
    /// `input` yields the rows to change (a scan, filtered).
    Update {
        table: String,
        input: Box<LogicalPlan>,
        assignments: Vec<(usize, TypedExpr)>,
        returning: Vec<OutputColumn>,
    },
    Delete {
        table: String,
        input: Box<LogicalPlan>,
        returning: Vec<OutputColumn>,
    },
}

/// A plan with the types the binder gave the `$n` placeholders.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundStatement {
    pub plan: LogicalPlan,
    /// Type of `$1`, `$2`, ... up to the highest placeholder used.
    pub param_types: Vec<SqlType>,
}

impl LogicalPlan {
    /// The columns a query plan produces, or `None` for a write without `RETURNING` or a plan
    /// that has no projection.
    pub fn output(&self) -> Option<&[OutputColumn]> {
        match self {
            LogicalPlan::Project { columns, .. } => Some(columns),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. } => input.output(),
            LogicalPlan::Insert { returning, .. }
            | LogicalPlan::Update { returning, .. }
            | LogicalPlan::Delete { returning, .. } => {
                if returning.is_empty() {
                    None
                } else {
                    Some(returning)
                }
            }
            LogicalPlan::Scan { .. } => None,
        }
    }

    fn write_tree(&self, out: &mut String, depth: usize) -> fmt::Result {
        let pad = "  ".repeat(depth);
        match self {
            LogicalPlan::Scan { table, as_of } => {
                write!(out, "{pad}Scan {table}")?;
                if let Some(a) = as_of {
                    write!(out, " {a}")?;
                }
                out.push('\n');
            }
            LogicalPlan::Filter { input, predicate } => {
                writeln!(out, "{pad}Filter {predicate}")?;
                input.write_tree(out, depth + 1)?;
            }
            LogicalPlan::Project { input, columns } => {
                writeln!(out, "{pad}Project [{}]", OutputList(columns))?;
                input.write_tree(out, depth + 1)?;
            }
            LogicalPlan::Sort { input, keys } => {
                let keys: Vec<String> = keys
                    .iter()
                    .map(|k| {
                        format!(
                            "{} {} NULLS {}",
                            k.expr,
                            if k.desc { "DESC" } else { "ASC" },
                            if k.nulls_first { "FIRST" } else { "LAST" }
                        )
                    })
                    .collect();
                writeln!(out, "{pad}Sort [{}]", keys.join(", "))?;
                input.write_tree(out, depth + 1)?;
            }
            LogicalPlan::Limit {
                input,
                limit,
                offset,
            } => {
                write!(out, "{pad}Limit")?;
                if let Some(l) = limit {
                    write!(out, " {l}")?;
                }
                if let Some(o) = offset {
                    write!(out, " OFFSET {o}")?;
                }
                out.push('\n');
                input.write_tree(out, depth + 1)?;
            }
            LogicalPlan::Insert {
                table,
                targets,
                source,
                returning,
            } => {
                let t: Vec<String> = targets.iter().map(|i| format!("#{i}")).collect();
                write!(out, "{pad}Insert {table} ({})", t.join(", "))?;
                write_returning(out, returning)?;
                out.push('\n');
                match source {
                    InsertSource::Values(rows) => {
                        for row in rows {
                            let r: Vec<String> = row.iter().map(ToString::to_string).collect();
                            writeln!(out, "{pad}  Values ({})", r.join(", "))?;
                        }
                    }
                    InsertSource::Query(q) => q.write_tree(out, depth + 1)?,
                }
            }
            LogicalPlan::Update {
                table,
                input,
                assignments,
                returning,
            } => {
                let a: Vec<String> = assignments
                    .iter()
                    .map(|(i, e)| format!("#{i} = {e}"))
                    .collect();
                write!(out, "{pad}Update {table} SET {}", a.join(", "))?;
                write_returning(out, returning)?;
                out.push('\n');
                input.write_tree(out, depth + 1)?;
            }
            LogicalPlan::Delete {
                table,
                input,
                returning,
            } => {
                write!(out, "{pad}Delete {table}")?;
                write_returning(out, returning)?;
                out.push('\n');
                input.write_tree(out, depth + 1)?;
            }
        }
        Ok(())
    }
}

fn write_returning(out: &mut String, returning: &[OutputColumn]) -> fmt::Result {
    if !returning.is_empty() {
        write!(out, " RETURNING [{}]", OutputList(returning))?;
    }
    Ok(())
}

struct OutputList<'a>(&'a [OutputColumn]);

impl Display for OutputList<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (i, c) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            let e = c.expr.to_string();
            if e == c.name {
                f.write_str(&e)?;
            } else {
                write!(f, "{e} AS {}", c.name)?;
            }
        }
        Ok(())
    }
}

impl Display for LogicalPlan {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write_tree(&mut s, 0)?;
        f.write_str(&s)
    }
}

impl Display for Value {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(true) => f.write_str("TRUE"),
            Value::Bool(false) => f.write_str("FALSE"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Number(n) => f.write_str(n),
            Value::Text(t) => write!(f, "'{}'", t.replace('\'', "''")),
        }
    }
}

fn op_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Or => "OR",
        BinaryOp::And => "AND",
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::GtEq => ">=",
        BinaryOp::Concat => "||",
        BinaryOp::JsonGet => "->",
        BinaryOp::JsonGetText => "->>",
        BinaryOp::Contains => "@>",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
    }
}

fn not(negated: bool) -> &'static str {
    if negated { " NOT" } else { "" }
}

impl Display for TypedExpr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ExprNode::Column { name, .. } => f.write_str(name),
            ExprNode::Const(v) => write!(f, "{v}"),
            ExprNode::Param(n) => write!(f, "${n}"),
            ExprNode::Unary { op, expr } => match op {
                UnaryOp::Plus => write!(f, "(+{expr})"),
                UnaryOp::Minus => write!(f, "(-{expr})"),
                UnaryOp::Not => write!(f, "(NOT {expr})"),
            },
            ExprNode::Binary { op, left, right } => {
                write!(f, "({left} {} {right})", op_text(*op))
            }
            ExprNode::IsNull { expr, negated } => {
                write!(f, "({expr} IS{} NULL)", not(*negated))
            }
            ExprNode::InList {
                expr,
                list,
                negated,
            } => {
                let l: Vec<String> = list.iter().map(ToString::to_string).collect();
                write!(f, "({expr}{} IN ({}))", not(*negated), l.join(", "))
            }
            ExprNode::Between {
                expr,
                low,
                high,
                negated,
            } => write!(f, "({expr}{} BETWEEN {low} AND {high})", not(*negated)),
            ExprNode::Like {
                expr,
                pattern,
                negated,
            } => write!(f, "({expr}{} LIKE {pattern})", not(*negated)),
            ExprNode::Cast { expr } => write!(f, "CAST({expr} AS {})", self.ty),
            ExprNode::Function { name, args } => {
                let a: Vec<String> = args.iter().map(ToString::to_string).collect();
                write!(f, "{name}({})", a.join(", "))
            }
        }
    }
}

impl Display for AsOfPlan {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            AsOfPlan::Changeset(e) => write!(f, "AS OF CHANGESET {e}"),
            AsOfPlan::Timestamp(e) => write!(f, "AS OF TIMESTAMP {e}"),
        }
    }
}
