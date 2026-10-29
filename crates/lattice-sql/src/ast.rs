//! The syntax tree of the declared SQL subset.
//!
//! The tree is plain data. Names are case-folded as PostgreSQL does (unquoted names to lower case,
//! quoted names kept). Identifiers and expressions carry a [`Span`] for error messages; spans never
//! take part in `==`.
//!
//! Variant and field names follow the SQL they stand for, so they are not documented one by one.
#![allow(missing_docs)]

use crate::error::Span;

/// A name: an unquoted word folded to lower case, or the contents of a quoted identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub value: String,
    pub span: Span,
}

impl Ident {
    /// An identifier with no source position.
    pub fn new(value: impl Into<String>) -> Self {
        Ident {
            value: value.into(),
            span: Span::default(),
        }
    }
}

/// A table or index name, one part (`t`) or two (`schema.t`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectName(pub Vec<Ident>);

/// One SQL statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Statement {
    Query(Box<Query>),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    CreateTable(CreateTable),
    AlterTable(AlterTable),
    DropTable {
        name: ObjectName,
        if_exists: bool,
    },
    CreateIndex(CreateIndex),
    DropIndex {
        name: ObjectName,
        if_exists: bool,
    },
    /// `BEGIN` or `START TRANSACTION`.
    Begin {
        isolation: Option<IsolationLevel>,
    },
    Commit,
    Rollback,
    /// `SET TRANSACTION ISOLATION LEVEL ...`
    SetTransaction {
        isolation: IsolationLevel,
    },
    /// `SET name = value`; `None` stands for `DEFAULT`.
    Set {
        name: Ident,
        value: Option<SetValue>,
    },
    Show {
        name: Ident,
    },
}

/// Isolation levels the subset accepts as requests for snapshot isolation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationLevel {
    RepeatableRead,
    Serializable,
}

/// The value of a `SET` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetValue {
    /// A number, with its sign when negative, as written.
    Number(String),
    String(String),
    /// A bare word such as `on` or `utf8`, folded to lower case.
    Word(String),
}

/// A `SELECT` query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub from: Vec<TableWithJoins>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectItem {
    /// `*`
    Wildcard,
    /// `t.*`
    QualifiedWildcard(Ident),
    Expr {
        expr: Expr,
        alias: Option<Ident>,
    },
}

/// A table reference followed by the joins applied to it, left to right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableWithJoins {
    pub relation: TableFactor,
    pub joins: Vec<Join>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableFactor {
    pub name: ObjectName,
    pub as_of: Option<AsOf>,
    pub alias: Option<Ident>,
}

/// A snapshot selector after a table reference. The operand is a number, string or parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsOf {
    Changeset(Box<Expr>),
    Timestamp(Box<Expr>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Join {
    pub kind: JoinKind,
    pub relation: TableFactor,
    /// Present for `Inner` and `Left`, absent for `Cross`.
    pub on: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub desc: bool,
    /// `Some(true)` for `NULLS FIRST`, `Some(false)` for `NULLS LAST`.
    pub nulls_first: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insert {
    pub table: ObjectName,
    pub columns: Vec<Ident>,
    pub source: InsertSource,
    pub returning: Vec<SelectItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertSource {
    /// `VALUES (..), (..)`; every row has the same length.
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub table: ObjectName,
    pub alias: Option<Ident>,
    pub assignments: Vec<Assignment>,
    pub selection: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub column: Ident,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delete {
    pub table: ObjectName,
    pub alias: Option<Ident>,
    pub selection: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateTable {
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: Ident,
    pub data_type: DataType,
    pub not_null: bool,
    pub primary_key: bool,
    pub unique: bool,
    /// A constant expression: a literal, optionally signed or cast.
    pub default: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableConstraint {
    PrimaryKey(Vec<Ident>),
    Unique(Vec<Ident>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlterTable {
    pub name: ObjectName,
    pub action: AlterAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlterAction {
    AddColumn(ColumnDef),
    DropColumn(Ident),
    RenameColumn { from: Ident, to: Ident },
    RenameTable(Ident),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateIndex {
    pub name: Option<Ident>,
    pub unique: bool,
    pub if_not_exists: bool,
    pub table: ObjectName,
    pub columns: Vec<Ident>,
}

/// The column types of the subset (`docs/sql-subset.md`, Types).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Boolean,
    SmallInt,
    Integer,
    BigInt,
    DoublePrecision,
    /// `numeric`, `numeric(p)` or `numeric(p, s)`; `p` is at most 38.
    Numeric(Option<(u8, Option<u8>)>),
    Text,
    Varchar(Option<u32>),
    Bytea,
    Timestamptz,
    Date,
    Uuid,
    Jsonb,
}

/// An expression with the place it was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    /// An expression with no source position.
    pub fn new(kind: ExprKind) -> Self {
        Expr {
            kind,
            span: Span::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprKind {
    Literal(Literal),
    /// `$n`, one-based.
    Param(u32),
    /// `col`, `t.col` or `schema.t.col`.
    Column(Vec<Ident>),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `expr [NOT] IN (a, b, ..)`; the list is never empty.
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        query: Box<Query>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    Function(Function),
    /// `CAST(expr AS type)` and `expr::type`.
    Cast {
        expr: Box<Expr>,
        data_type: DataType,
    },
    /// A scalar subquery.
    Subquery(Box<Query>),
    /// `position(needle IN haystack)`
    Position {
        needle: Box<Expr>,
        haystack: Box<Expr>,
    },
    /// `trim([LEADING | TRAILING | BOTH] [chars FROM] expr)`; `trim(x)` is `BOTH`.
    Trim {
        side: TrimSide,
        chars: Option<Box<Expr>>,
        expr: Box<Expr>,
    },
    /// `extract(field FROM expr)`
    Extract {
        field: Ident,
        expr: Box<Expr>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal {
    /// A number as written: digits with an optional fraction and exponent, never signed.
    Number(String),
    String(String),
    Boolean(bool),
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    /// `||`
    Concat,
    /// `->`
    JsonGet,
    /// `->>`
    JsonGetText,
    /// `@>`
    Contains,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimSide {
    Both,
    Leading,
    Trailing,
}

/// A call of one of the functions the subset allows (`docs/sql-subset.md`, Functions).
///
/// `position`, `trim` and `extract` have their own [`ExprKind`] variants because of their syntax;
/// the parser never produces a `Function` with those names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    /// The lower-case function name.
    pub name: String,
    /// `count(DISTINCT x)`.
    pub distinct: bool,
    pub args: FunctionArgs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionArgs {
    /// `count(*)`
    Star,
    List(Vec<Expr>),
}
