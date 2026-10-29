//! Printing the tree back as SQL.
//!
//! The output is canonical: keywords in upper case, every compound expression wrapped in
//! parentheses (so precedence never has to be reconstructed), names quoted only when they must be.
//! Parsing the output yields an equal tree.

use std::fmt::{self, Display, Formatter, Write};

use crate::ast::{
    AlterAction, AlterTable, AsOf, BinaryOp, ColumnDef, CreateIndex, CreateTable, DataType, Delete,
    Expr, ExprKind, Function, FunctionArgs, Ident, Insert, InsertSource, IsolationLevel, Join,
    JoinKind, Literal, ObjectName, OrderByItem, Query, SelectItem, SetValue, Statement,
    TableConstraint, TableFactor, TableWithJoins, TrimSide, UnaryOp, Update,
};
use crate::lexer::is_reserved;

fn simple_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !is_reserved(s)
}

fn write_name(f: &mut Formatter<'_>, s: &str) -> fmt::Result {
    if simple_name(s) {
        f.write_str(s)
    } else {
        f.write_char('"')?;
        for c in s.chars() {
            if c == '"' {
                f.write_char('"')?;
            }
            f.write_char(c)?;
        }
        f.write_char('"')
    }
}

fn write_string(f: &mut Formatter<'_>, s: &str) -> fmt::Result {
    f.write_char('\'')?;
    for c in s.chars() {
        if c == '\'' {
            f.write_char('\'')?;
        }
        f.write_char(c)?;
    }
    f.write_char('\'')
}

fn write_list<T: Display>(f: &mut Formatter<'_>, items: &[T]) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

impl Display for Ident {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write_name(f, &self.value)
    }
}

impl Display for ObjectName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (i, part) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_char('.')?;
            }
            write!(f, "{part}")?;
        }
        Ok(())
    }
}

impl Display for Statement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Query(q) => write!(f, "{q}"),
            Statement::Insert(s) => write!(f, "{s}"),
            Statement::Update(s) => write!(f, "{s}"),
            Statement::Delete(s) => write!(f, "{s}"),
            Statement::CreateTable(s) => write!(f, "{s}"),
            Statement::AlterTable(s) => write!(f, "{s}"),
            Statement::DropTable { name, if_exists } => {
                write!(
                    f,
                    "DROP TABLE {}{name}",
                    if *if_exists { "IF EXISTS " } else { "" }
                )
            }
            Statement::CreateIndex(s) => write!(f, "{s}"),
            Statement::DropIndex { name, if_exists } => {
                write!(
                    f,
                    "DROP INDEX {}{name}",
                    if *if_exists { "IF EXISTS " } else { "" }
                )
            }
            Statement::Begin { isolation: None } => f.write_str("BEGIN"),
            Statement::Begin {
                isolation: Some(level),
            } => write!(f, "BEGIN ISOLATION LEVEL {level}"),
            Statement::Commit => f.write_str("COMMIT"),
            Statement::Rollback => f.write_str("ROLLBACK"),
            Statement::SetTransaction { isolation } => {
                write!(f, "SET TRANSACTION ISOLATION LEVEL {isolation}")
            }
            Statement::Set { name, value } => {
                write!(f, "SET {name} = ")?;
                match value {
                    None => f.write_str("DEFAULT"),
                    Some(v) => write!(f, "{v}"),
                }
            }
            Statement::Show { name } => write!(f, "SHOW {name}"),
        }
    }
}

impl Display for IsolationLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IsolationLevel::RepeatableRead => "REPEATABLE READ",
            IsolationLevel::Serializable => "SERIALIZABLE",
        })
    }
}

impl Display for SetValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SetValue::Number(n) | SetValue::Word(n) => f.write_str(n),
            SetValue::String(s) => write_string(f, s),
        }
    }
}

impl Display for Query {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("SELECT ")?;
        if self.distinct {
            f.write_str("DISTINCT ")?;
        }
        write_list(f, &self.projection)?;
        if !self.from.is_empty() {
            f.write_str(" FROM ")?;
            write_list(f, &self.from)?;
        }
        if let Some(e) = &self.selection {
            write!(f, " WHERE {e}")?;
        }
        if !self.group_by.is_empty() {
            f.write_str(" GROUP BY ")?;
            write_list(f, &self.group_by)?;
        }
        if let Some(e) = &self.having {
            write!(f, " HAVING {e}")?;
        }
        if !self.order_by.is_empty() {
            f.write_str(" ORDER BY ")?;
            write_list(f, &self.order_by)?;
        }
        if let Some(e) = &self.limit {
            write!(f, " LIMIT {e}")?;
        }
        if let Some(e) = &self.offset {
            write!(f, " OFFSET {e}")?;
        }
        Ok(())
    }
}

impl Display for SelectItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Wildcard => f.write_char('*'),
            SelectItem::QualifiedWildcard(t) => write!(f, "{t}.*"),
            SelectItem::Expr { expr, alias: None } => write!(f, "{expr}"),
            SelectItem::Expr {
                expr,
                alias: Some(a),
            } => write!(f, "{expr} AS {a}"),
        }
    }
}

impl Display for TableWithJoins {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.relation)?;
        for join in &self.joins {
            write!(f, " {join}")?;
        }
        Ok(())
    }
}

impl Display for TableFactor {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some(as_of) = &self.as_of {
            write!(f, " {as_of}")?;
        }
        if let Some(alias) = &self.alias {
            write!(f, " AS {alias}")?;
        }
        Ok(())
    }
}

impl Display for AsOf {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            AsOf::Changeset(e) => write!(f, "AS OF CHANGESET {e}"),
            AsOf::Timestamp(e) => write!(f, "AS OF TIMESTAMP {e}"),
        }
    }
}

impl Display for Join {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Cross => "CROSS JOIN",
        };
        write!(f, "{kind} {}", self.relation)?;
        if let Some(on) = &self.on {
            write!(f, " ON {on}")?;
        }
        Ok(())
    }
}

impl Display for OrderByItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if self.desc {
            f.write_str(" DESC")?;
        }
        match self.nulls_first {
            Some(true) => f.write_str(" NULLS FIRST"),
            Some(false) => f.write_str(" NULLS LAST"),
            None => Ok(()),
        }
    }
}

fn write_returning(f: &mut Formatter<'_>, items: &[SelectItem]) -> fmt::Result {
    if items.is_empty() {
        return Ok(());
    }
    f.write_str(" RETURNING ")?;
    write_list(f, items)
}

impl Display for Insert {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "INSERT INTO {}", self.table)?;
        if !self.columns.is_empty() {
            f.write_str(" (")?;
            write_list(f, &self.columns)?;
            f.write_char(')')?;
        }
        match &self.source {
            InsertSource::Values(rows) => {
                f.write_str(" VALUES ")?;
                for (i, row) in rows.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_char('(')?;
                    write_list(f, row)?;
                    f.write_char(')')?;
                }
            }
            InsertSource::Query(q) => write!(f, " {q}")?,
        }
        write_returning(f, &self.returning)
    }
}

impl Display for Update {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "UPDATE {}", self.table)?;
        if let Some(a) = &self.alias {
            write!(f, " AS {a}")?;
        }
        f.write_str(" SET ")?;
        for (i, a) in self.assignments.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{} = {}", a.column, a.value)?;
        }
        if let Some(e) = &self.selection {
            write!(f, " WHERE {e}")?;
        }
        write_returning(f, &self.returning)
    }
}

impl Display for Delete {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "DELETE FROM {}", self.table)?;
        if let Some(a) = &self.alias {
            write!(f, " AS {a}")?;
        }
        if let Some(e) = &self.selection {
            write!(f, " WHERE {e}")?;
        }
        write_returning(f, &self.returning)
    }
}

impl Display for CreateTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE TABLE ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(f, "{} (", self.name)?;
        write_list(f, &self.columns)?;
        for c in &self.constraints {
            write!(f, ", {c}")?;
        }
        f.write_char(')')
    }
}

impl Display for ColumnDef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.data_type)?;
        if self.primary_key {
            f.write_str(" PRIMARY KEY")?;
        }
        if self.not_null {
            f.write_str(" NOT NULL")?;
        }
        if self.unique {
            f.write_str(" UNIQUE")?;
        }
        if let Some(d) = &self.default {
            write!(f, " DEFAULT {d}")?;
        }
        Ok(())
    }
}

impl Display for TableConstraint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let (what, cols) = match self {
            TableConstraint::PrimaryKey(c) => ("PRIMARY KEY", c),
            TableConstraint::Unique(c) => ("UNIQUE", c),
        };
        write!(f, "{what} (")?;
        write_list(f, cols)?;
        f.write_char(')')
    }
}

impl Display for AlterTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "ALTER TABLE {} ", self.name)?;
        match &self.action {
            AlterAction::AddColumn(c) => write!(f, "ADD COLUMN {c}"),
            AlterAction::DropColumn(c) => write!(f, "DROP COLUMN {c}"),
            AlterAction::RenameColumn { from, to } => write!(f, "RENAME COLUMN {from} TO {to}"),
            AlterAction::RenameTable(to) => write!(f, "RENAME TO {to}"),
        }
    }
}

impl Display for CreateIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE ")?;
        if self.unique {
            f.write_str("UNIQUE ")?;
        }
        f.write_str("INDEX ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        if let Some(n) = &self.name {
            write!(f, "{n} ")?;
        }
        write!(f, "ON {} (", self.table)?;
        write_list(f, &self.columns)?;
        f.write_char(')')
    }
}

impl Display for DataType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Boolean => f.write_str("boolean"),
            DataType::SmallInt => f.write_str("smallint"),
            DataType::Integer => f.write_str("integer"),
            DataType::BigInt => f.write_str("bigint"),
            DataType::DoublePrecision => f.write_str("double precision"),
            DataType::Numeric(None) => f.write_str("numeric"),
            DataType::Numeric(Some((p, None))) => write!(f, "numeric({p})"),
            DataType::Numeric(Some((p, Some(s)))) => write!(f, "numeric({p}, {s})"),
            DataType::Text => f.write_str("text"),
            DataType::Varchar(None) => f.write_str("varchar"),
            DataType::Varchar(Some(n)) => write!(f, "varchar({n})"),
            DataType::Bytea => f.write_str("bytea"),
            DataType::Timestamptz => f.write_str("timestamptz"),
            DataType::Date => f.write_str("date"),
            DataType::Uuid => f.write_str("uuid"),
            DataType::Jsonb => f.write_str("jsonb"),
        }
    }
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ExprKind::Literal(l) => write!(f, "{l}"),
            ExprKind::Param(n) => write!(f, "${n}"),
            ExprKind::Column(parts) => {
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        f.write_char('.')?;
                    }
                    write!(f, "{p}")?;
                }
                Ok(())
            }
            ExprKind::Unary { op, expr } => {
                let op = match op {
                    UnaryOp::Plus => "+",
                    UnaryOp::Minus => "-",
                    UnaryOp::Not => "NOT ",
                };
                write!(f, "({op}{expr})")
            }
            ExprKind::Binary { op, left, right } => {
                write!(f, "({left} {} {right})", binary_op(*op))
            }
            ExprKind::IsNull { expr, negated } => {
                write!(f, "({expr} IS {}NULL)", if *negated { "NOT " } else { "" })
            }
            ExprKind::InList {
                expr,
                list,
                negated,
            } => {
                write!(f, "({expr} {}IN (", not(*negated))?;
                write_list(f, list)?;
                f.write_str("))")
            }
            ExprKind::InSubquery {
                expr,
                query,
                negated,
            } => write!(f, "({expr} {}IN ({query}))", not(*negated)),
            ExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => write!(f, "({expr} {}BETWEEN {low} AND {high})", not(*negated)),
            ExprKind::Like {
                expr,
                pattern,
                negated,
            } => write!(f, "({expr} {}LIKE {pattern})", not(*negated)),
            ExprKind::Function(func) => write!(f, "{func}"),
            ExprKind::Cast { expr, data_type } => write!(f, "CAST({expr} AS {data_type})"),
            ExprKind::Subquery(q) => write!(f, "({q})"),
            ExprKind::Position { needle, haystack } => {
                write!(f, "position({needle} IN {haystack})")
            }
            ExprKind::Trim { side, chars, expr } => {
                let side = match side {
                    TrimSide::Both => "BOTH",
                    TrimSide::Leading => "LEADING",
                    TrimSide::Trailing => "TRAILING",
                };
                write!(f, "trim({side} ")?;
                if let Some(c) = chars {
                    write!(f, "{c} ")?;
                }
                write!(f, "FROM {expr})")
            }
            ExprKind::Extract { field, expr } => write!(f, "extract({field} FROM {expr})"),
        }
    }
}

fn not(negated: bool) -> &'static str {
    if negated { "NOT " } else { "" }
}

fn binary_op(op: BinaryOp) -> &'static str {
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

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Number(n) => f.write_str(n),
            Literal::String(s) => write_string(f, s),
            Literal::Boolean(true) => f.write_str("TRUE"),
            Literal::Boolean(false) => f.write_str("FALSE"),
            Literal::Null => f.write_str("NULL"),
        }
    }
}

impl Display for Function {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}(", self.name)?;
        match &self.args {
            FunctionArgs::Star => f.write_char('*')?,
            FunctionArgs::List(args) => {
                if self.distinct {
                    f.write_str("DISTINCT ")?;
                }
                write_list(f, args)?;
            }
        }
        f.write_char(')')
    }
}
