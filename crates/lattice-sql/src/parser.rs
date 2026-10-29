//! Recursive-descent parser with precedence climbing for expressions.
//!
//! Every construct is accepted explicitly. A word the parser knows to be PostgreSQL syntax outside
//! the subset is refused as unsupported and named; anything else is a syntax error. Nesting is
//! bounded twice: `depth` counts active calls, and `td` tracks the depth of the tree being built so
//! that long left-leaning chains such as `1 + 1 + 1 + ...` cannot produce a tree too deep to print
//! or drop.

use crate::MAX_DEPTH;
use crate::ast::{
    AlterAction, AlterTable, AsOf, Assignment, BinaryOp, ColumnDef, CreateIndex, CreateTable,
    DataType, Delete, Expr, ExprKind, Function, FunctionArgs, Ident, Insert, InsertSource,
    IsolationLevel, Join, JoinKind, Literal, ObjectName, OrderByItem, Query, SelectItem, SetValue,
    Statement, TableConstraint, TableFactor, TableWithJoins, TrimSide, UnaryOp, Update,
};
use crate::error::{ParseError, Span};
use crate::lexer::{Tok, Token, is_reserved, lex};

type PResult<T> = Result<T, ParseError>;

/// Parses a script of zero or more statements separated by `;`.
pub fn parse_statements(input: &str) -> PResult<Vec<Statement>> {
    let mut p = Parser::new(lex(input)?);
    let mut out = Vec::new();
    loop {
        while p.eat(&Tok::Semi) {}
        if p.at_eof() {
            return Ok(out);
        }
        out.push(p.statement()?);
        if !p.at_eof() && !p.at(&Tok::Semi) {
            return Err(p.unexpected("`;` or end of input"));
        }
    }
}

/// Parses exactly one statement, optionally followed by `;`.
pub fn parse_statement(input: &str) -> PResult<Statement> {
    let mut p = Parser::new(lex(input)?);
    while p.eat(&Tok::Semi) {}
    if p.at_eof() {
        return Err(ParseError::syntax("empty statement", p.peek().span));
    }
    let stmt = p.statement()?;
    while p.eat(&Tok::Semi) {}
    if !p.at_eof() {
        return Err(p.unexpected("end of input (one statement only)"));
    }
    Ok(stmt)
}

/// PostgreSQL statements that the subset does not have.
const UNSUPPORTED_STATEMENTS: &[&str] = &[
    "abort",
    "analyse",
    "analyze",
    "call",
    "checkpoint",
    "close",
    "cluster",
    "comment",
    "copy",
    "deallocate",
    "declare",
    "discard",
    "do",
    "end",
    "execute",
    "explain",
    "fetch",
    "grant",
    "import",
    "listen",
    "load",
    "lock",
    "merge",
    "move",
    "notify",
    "prepare",
    "refresh",
    "reindex",
    "release",
    "reset",
    "revoke",
    "savepoint",
    "security",
    "table",
    "truncate",
    "unlisten",
    "vacuum",
    "values",
    "with",
];

/// Functions the subset allows (`docs/sql-subset.md`, Functions and operators).
const FUNCTIONS: &[&str] = &[
    "abs",
    "avg",
    "coalesce",
    "count",
    "date_trunc",
    "length",
    "lower",
    "max",
    "min",
    "now",
    "nullif",
    "round",
    "substring",
    "sum",
    "upper",
];

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    eof: Token,
    /// Active nested calls.
    depth: usize,
    /// Depth of the tree of the expression or query parsed last.
    td: usize,
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Word {
            text,
            quoted: false,
        } => format!("`{text}`"),
        Tok::Word { text, quoted: true } => format!("\"{text}\""),
        Tok::Number(n) => format!("number `{n}`"),
        Tok::Str(_) => "a string literal".to_string(),
        Tok::Param(n) => format!("`${n}`"),
        Tok::LParen => "`(`".to_string(),
        Tok::RParen => "`)`".to_string(),
        Tok::Comma => "`,`".to_string(),
        Tok::Semi => "`;`".to_string(),
        Tok::Dot => "`.`".to_string(),
        Tok::Star => "`*`".to_string(),
        Tok::Plus => "`+`".to_string(),
        Tok::Minus => "`-`".to_string(),
        Tok::Slash => "`/`".to_string(),
        Tok::Percent => "`%`".to_string(),
        Tok::Eq => "`=`".to_string(),
        Tok::NotEq => "`<>`".to_string(),
        Tok::Lt => "`<`".to_string(),
        Tok::Gt => "`>`".to_string(),
        Tok::LtEq => "`<=`".to_string(),
        Tok::GtEq => "`>=`".to_string(),
        Tok::Concat => "`||`".to_string(),
        Tok::Arrow => "`->`".to_string(),
        Tok::ArrowText => "`->>`".to_string(),
        Tok::Contains => "`@>`".to_string(),
        Tok::DoubleColon => "`::`".to_string(),
        Tok::Eof => "end of input".to_string(),
    }
}

/// How an infix position binds, from loosest to tightest.
#[derive(Clone, Copy)]
enum Infix {
    None,
    Bin(BinaryOp, u8),
    Is,
    Pred { negated: bool, kind: Pred },
    Cast,
    Unsupported(&'static str),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pred {
    In,
    Between,
    Like,
}

const BP_OR: u8 = 1;
const BP_AND: u8 = 2;
const BP_NOT: u8 = 3;
const BP_IS: u8 = 4;
const BP_CMP: u8 = 5;
const BP_PRED: u8 = 6;
const BP_OTHER: u8 = 7;
const BP_ADD: u8 = 8;
const BP_MUL: u8 = 9;
const BP_UNARY: u8 = 10;
const BP_CAST: u8 = 11;

impl Infix {
    fn bp(self) -> u8 {
        match self {
            Infix::None => 0,
            Infix::Bin(_, bp) => bp,
            Infix::Is => BP_IS,
            Infix::Pred { .. } | Infix::Unsupported(_) => BP_PRED,
            Infix::Cast => BP_CAST,
        }
    }
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            eof: Token {
                tok: Tok::Eof,
                span: Span::default(),
            },
            depth: 0,
            td: 0,
        }
    }

    // ---- token access -------------------------------------------------------------------------

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&self.eof)
    }

    fn peek_n(&self, n: usize) -> &Token {
        self.tokens.get(self.pos + n).unwrap_or(&self.eof)
    }

    fn bump(&mut self) -> Span {
        let span = self.peek().span;
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        span
    }

    fn at_eof(&self) -> bool {
        self.peek().tok == Tok::Eof
    }

    fn at(&self, tok: &Tok) -> bool {
        &self.peek().tok == tok
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        let found = self.at(tok);
        if found {
            self.bump();
        }
        found
    }

    fn expect(&mut self, tok: &Tok, what: &str) -> PResult<Span> {
        if self.at(tok) {
            Ok(self.bump())
        } else {
            Err(self.unexpected(what))
        }
    }

    /// The text of the unquoted word at the cursor.
    fn word(&self) -> Option<&str> {
        self.word_at(0)
    }

    fn word_at(&self, n: usize) -> Option<&str> {
        match &self.peek_n(n).tok {
            Tok::Word {
                text,
                quoted: false,
            } => Some(text.as_str()),
            _ => None,
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.word() == Some(kw)
    }

    fn is_kw_at(&self, n: usize, kw: &str) -> bool {
        self.word_at(n) == Some(kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        let found = self.is_kw(kw);
        if found {
            self.bump();
        }
        found
    }

    fn expect_kw(&mut self, kw: &str) -> PResult<Span> {
        if self.is_kw(kw) {
            Ok(self.bump())
        } else {
            Err(self.unexpected(&kw.to_ascii_uppercase()))
        }
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        let t = self.peek();
        ParseError::syntax(
            format!("expected {expected}, found {}", describe(&t.tok)),
            t.span,
        )
    }

    fn unsupported_here(&self, feature: &str) -> ParseError {
        ParseError::unsupported(feature, self.peek().span)
    }

    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.depth += 1;
        let result = if self.depth > MAX_DEPTH {
            Err(ParseError::too_large(
                format!("nesting is deeper than {MAX_DEPTH}"),
                self.peek().span,
            ))
        } else {
            f(self)
        };
        self.depth = self.depth.saturating_sub(1);
        result
    }

    // ---- names --------------------------------------------------------------------------------

    fn ident(&mut self) -> PResult<Ident> {
        let t = self.peek();
        let span = t.span;
        match &t.tok {
            Tok::Word { text, quoted } if *quoted || !is_reserved(text) => {
                let value = text.clone();
                self.bump();
                Ok(Ident { value, span })
            }
            Tok::Word { text, .. } => Err(ParseError::syntax(
                format!("expected a name, found the keyword `{text}`"),
                span,
            )),
            _ => Err(self.unexpected("a name")),
        }
    }

    fn object_name(&mut self) -> PResult<ObjectName> {
        let mut parts = vec![self.ident()?];
        while self.at(&Tok::Dot) {
            if parts.len() == 2 {
                return Err(self.unsupported_here("names qualified by a database"));
            }
            self.bump();
            parts.push(self.ident()?);
        }
        Ok(ObjectName(parts))
    }

    fn ident_list(&mut self) -> PResult<Vec<Ident>> {
        let mut out = vec![self.ident()?];
        while self.eat(&Tok::Comma) {
            out.push(self.ident()?);
        }
        Ok(out)
    }

    fn paren_ident_list(&mut self) -> PResult<Vec<Ident>> {
        self.expect(&Tok::LParen, "`(`")?;
        let out = self.ident_list()?;
        self.expect(&Tok::RParen, "`)`")?;
        Ok(out)
    }

    /// An alias after a select item or table: `AS name`, or a bare unreserved or quoted name.
    fn alias(&mut self) -> PResult<Option<Ident>> {
        if self.eat_kw("as") {
            return Ok(Some(self.ident()?));
        }
        match &self.peek().tok {
            Tok::Word { text, quoted } if *quoted || !is_reserved(text) => Ok(Some(self.ident()?)),
            _ => Ok(None),
        }
    }

    // ---- statements ---------------------------------------------------------------------------

    fn statement(&mut self) -> PResult<Statement> {
        let span = self.peek().span;
        let Some(word) = self.word().map(str::to_string) else {
            if self.at(&Tok::LParen) {
                return Err(self.unsupported_here("parenthesized query"));
            }
            return Err(self.unexpected("a statement"));
        };
        match word.as_str() {
            "select" => Ok(Statement::Query(Box::new(self.query()?))),
            "insert" => self.insert(),
            "update" => self.update(),
            "delete" => self.delete(),
            "create" => self.create(),
            "alter" => self.alter(),
            "drop" => self.drop_stmt(),
            "begin" | "start" => self.begin(),
            "commit" => {
                self.bump();
                self.transaction_noise()?;
                Ok(Statement::Commit)
            }
            "rollback" => {
                self.bump();
                if self.is_kw("to") || self.is_kw("prepared") {
                    return Err(self.unsupported_here("ROLLBACK TO SAVEPOINT"));
                }
                self.transaction_noise()?;
                Ok(Statement::Rollback)
            }
            "set" => self.set(),
            "show" => self.show(),
            "with" => Err(ParseError::unsupported(
                "WITH (common table expressions)",
                span,
            )),
            "savepoint" | "release" => Err(ParseError::unsupported("SAVEPOINT", span)),
            other if UNSUPPORTED_STATEMENTS.contains(&other) => Err(ParseError::unsupported(
                format!("{} statement", other.to_ascii_uppercase()),
                span,
            )),
            other => Err(ParseError::syntax(
                format!("unrecognized statement `{other}`"),
                span,
            )),
        }
    }

    /// The optional `WORK` or `TRANSACTION` after `COMMIT`, `ROLLBACK` and `BEGIN`.
    fn transaction_noise(&mut self) -> PResult<()> {
        if !self.eat_kw("work") {
            self.eat_kw("transaction");
        }
        if self.is_kw("and") {
            return Err(self.unsupported_here("AND CHAIN"));
        }
        Ok(())
    }

    fn begin(&mut self) -> PResult<Statement> {
        if self.eat_kw("start") {
            self.expect_kw("transaction")?;
        } else {
            self.bump();
            self.transaction_noise()?;
        }
        let isolation = if self.is_kw("isolation") {
            Some(self.isolation_clause()?)
        } else {
            None
        };
        if self.is_kw("read") || self.is_kw("deferrable") || self.is_kw("not") {
            return Err(self.unsupported_here("transaction access modes"));
        }
        Ok(Statement::Begin { isolation })
    }

    /// `ISOLATION LEVEL REPEATABLE READ | SERIALIZABLE`.
    fn isolation_clause(&mut self) -> PResult<IsolationLevel> {
        self.expect_kw("isolation")?;
        self.expect_kw("level")?;
        let start = self.peek().span;
        if self.eat_kw("serializable") {
            return Ok(IsolationLevel::Serializable);
        }
        if self.eat_kw("repeatable") {
            self.expect_kw("read")?;
            return Ok(IsolationLevel::RepeatableRead);
        }
        if self.eat_kw("read") {
            let level = if self.eat_kw("committed") {
                "READ COMMITTED"
            } else if self.eat_kw("uncommitted") {
                "READ UNCOMMITTED"
            } else {
                return Err(self.unexpected("COMMITTED or UNCOMMITTED"));
            };
            return Err(ParseError::unsupported(
                format!("isolation level {level}"),
                start,
            ));
        }
        Err(self.unexpected("REPEATABLE READ or SERIALIZABLE"))
    }

    fn set(&mut self) -> PResult<Statement> {
        self.bump();
        if self.eat_kw("transaction") {
            if !self.is_kw("isolation") {
                return Err(self.unsupported_here("SET TRANSACTION modes other than isolation"));
            }
            let isolation = self.isolation_clause()?;
            return Ok(Statement::SetTransaction { isolation });
        }
        if self.is_kw("session") || self.is_kw("local") {
            return Err(self.unsupported_here("SET SESSION and SET LOCAL"));
        }
        let name = self.ident()?;
        if !self.eat(&Tok::Eq) && !self.eat_kw("to") {
            return Err(self.unexpected("`=` or TO"));
        }
        let value = match self.peek().tok.clone() {
            Tok::Word {
                text,
                quoted: false,
            } if text == "default" => {
                self.bump();
                None
            }
            Tok::Word {
                text,
                quoted: false,
            } => {
                self.bump();
                Some(SetValue::Word(text))
            }
            Tok::Word { text, quoted: true } | Tok::Str(text) => {
                self.bump();
                Some(SetValue::String(text))
            }
            Tok::Number(n) => {
                self.bump();
                Some(SetValue::Number(n))
            }
            Tok::Minus if matches!(self.peek_n(1).tok, Tok::Number(_)) => {
                self.bump();
                match self.peek().tok.clone() {
                    Tok::Number(n) => {
                        self.bump();
                        Some(SetValue::Number(format!("-{n}")))
                    }
                    _ => return Err(self.unexpected("a number")),
                }
            }
            _ => return Err(self.unexpected("a value")),
        };
        Ok(Statement::Set { name, value })
    }

    fn show(&mut self) -> PResult<Statement> {
        self.bump();
        if self.is_kw("all") {
            return Err(self.unsupported_here("SHOW ALL"));
        }
        Ok(Statement::Show {
            name: self.ident()?,
        })
    }

    // ---- queries ------------------------------------------------------------------------------

    fn query(&mut self) -> PResult<Query> {
        self.nested(Self::query_inner)
    }

    fn query_inner(&mut self) -> PResult<Query> {
        if self.is_kw("with") {
            return Err(self.unsupported_here("WITH (common table expressions)"));
        }
        if self.is_kw("values") {
            return Err(self.unsupported_here("VALUES as a query"));
        }
        let mut depth = 0usize;
        self.expect_kw("select")?;
        let distinct = if self.eat_kw("distinct") {
            if self.is_kw("on") {
                return Err(self.unsupported_here("DISTINCT ON"));
            }
            true
        } else {
            false
        };
        let projection = self.select_items(&mut depth)?;
        let mut from = Vec::new();
        if self.eat_kw("from") {
            loop {
                from.push(self.table_with_joins(&mut depth)?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let selection = if self.eat_kw("where") {
            Some(self.tracked_expr(&mut depth)?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.eat_kw("group") {
            self.expect_kw("by")?;
            loop {
                group_by.push(self.tracked_expr(&mut depth)?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let having = if self.eat_kw("having") {
            Some(self.tracked_expr(&mut depth)?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            loop {
                let expr = self.tracked_expr(&mut depth)?;
                let desc = if self.eat_kw("desc") {
                    true
                } else {
                    self.eat_kw("asc");
                    false
                };
                let nulls_first = if self.eat_kw("nulls") {
                    if self.eat_kw("first") {
                        Some(true)
                    } else if self.eat_kw("last") {
                        Some(false)
                    } else {
                        return Err(self.unexpected("FIRST or LAST"));
                    }
                } else {
                    None
                };
                order_by.push(OrderByItem {
                    expr,
                    desc,
                    nulls_first,
                });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = None;
        loop {
            if limit.is_none() && self.is_kw("limit") {
                self.bump();
                if self.is_kw("all") {
                    return Err(self.unsupported_here("LIMIT ALL"));
                }
                limit = Some(self.tracked_expr(&mut depth)?);
            } else if offset.is_none() && self.is_kw("offset") {
                self.bump();
                offset = Some(self.tracked_expr(&mut depth)?);
            } else {
                break;
            }
        }
        self.reject_query_suffix()?;
        if depth + 1 > MAX_DEPTH {
            return Err(ParseError::too_large(
                format!("nesting is deeper than {MAX_DEPTH}"),
                self.peek().span,
            ));
        }
        self.td = depth + 1;
        Ok(Query {
            distinct,
            projection,
            from,
            selection,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    /// Clauses and operators that may follow a query in PostgreSQL but are not in the subset.
    fn reject_query_suffix(&self) -> PResult<()> {
        let feature = match self.word() {
            Some("union" | "intersect" | "except") => "UNION, INTERSECT and EXCEPT",
            Some("for") => "row locking (FOR UPDATE)",
            Some("window") => "window definitions",
            Some("fetch") => "FETCH FIRST",
            Some("into") => "SELECT INTO",
            _ => return Ok(()),
        };
        Err(self.unsupported_here(feature))
    }

    /// Parses an expression and folds its tree depth into `depth`.
    fn tracked_expr(&mut self, depth: &mut usize) -> PResult<Expr> {
        let expr = self.expr()?;
        *depth = (*depth).max(self.td);
        Ok(expr)
    }

    fn select_items(&mut self, depth: &mut usize) -> PResult<Vec<SelectItem>> {
        let mut items = Vec::new();
        loop {
            items.push(self.select_item(depth)?);
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    fn select_item(&mut self, depth: &mut usize) -> PResult<SelectItem> {
        if self.eat(&Tok::Star) {
            return Ok(SelectItem::Wildcard);
        }
        if matches!(self.peek().tok, Tok::Word { .. })
            && self.peek_n(1).tok == Tok::Dot
            && self.peek_n(2).tok == Tok::Star
        {
            let name = self.ident()?;
            self.bump();
            self.bump();
            return Ok(SelectItem::QualifiedWildcard(name));
        }
        let expr = self.tracked_expr(depth)?;
        let alias = self.alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    fn returning(&mut self) -> PResult<Vec<SelectItem>> {
        if !self.eat_kw("returning") {
            return Ok(Vec::new());
        }
        let mut depth = 0;
        self.select_items(&mut depth)
    }

    fn table_with_joins(&mut self, depth: &mut usize) -> PResult<TableWithJoins> {
        let relation = self.table_factor(depth)?;
        let mut joins = Vec::new();
        loop {
            let kind = match self.word() {
                Some("join") => {
                    self.bump();
                    JoinKind::Inner
                }
                Some("inner") => {
                    self.bump();
                    self.expect_kw("join")?;
                    JoinKind::Inner
                }
                Some("left") => {
                    self.bump();
                    self.eat_kw("outer");
                    self.expect_kw("join")?;
                    JoinKind::Left
                }
                Some("cross") => {
                    self.bump();
                    self.expect_kw("join")?;
                    JoinKind::Cross
                }
                Some("right") => return Err(self.unsupported_here("RIGHT JOIN")),
                Some("full") => return Err(self.unsupported_here("FULL JOIN")),
                Some("natural") => return Err(self.unsupported_here("NATURAL JOIN")),
                _ => return Ok(TableWithJoins { relation, joins }),
            };
            let relation = self.table_factor(depth)?;
            let on = if kind == JoinKind::Cross {
                None
            } else {
                if self.is_kw("using") {
                    return Err(self.unsupported_here("JOIN ... USING"));
                }
                self.expect_kw("on")?;
                Some(self.tracked_expr(depth)?)
            };
            joins.push(Join { kind, relation, on });
        }
    }

    fn table_factor(&mut self, depth: &mut usize) -> PResult<TableFactor> {
        if self.at(&Tok::LParen) {
            return Err(self.unsupported_here("subqueries and parenthesized joins in FROM"));
        }
        if self.is_kw("lateral") {
            return Err(self.unsupported_here("LATERAL"));
        }
        let name = self.object_name()?;
        let mut as_of = self.as_of(depth)?;
        let alias = self.alias()?;
        if as_of.is_none() {
            as_of = self.as_of(depth)?;
        }
        Ok(TableFactor { name, as_of, alias })
    }

    /// `AS OF CHANGESET <id>` or `AS OF TIMESTAMP '<ts>'`.
    fn as_of(&mut self, depth: &mut usize) -> PResult<Option<AsOf>> {
        if !(self.is_kw("as")
            && self.is_kw_at(1, "of")
            && (self.is_kw_at(2, "changeset") || self.is_kw_at(2, "timestamp")))
        {
            return Ok(None);
        }
        self.bump();
        self.bump();
        let is_changeset = self.is_kw("changeset");
        self.bump();
        let span = self.peek().span;
        let kind = match self.peek().tok.clone() {
            Tok::Number(n) => ExprKind::Literal(Literal::Number(n)),
            Tok::Str(s) => ExprKind::Literal(Literal::String(s)),
            Tok::Param(n) => ExprKind::Param(n),
            _ => return Err(self.unexpected("a number, string literal or parameter")),
        };
        self.bump();
        *depth = (*depth).max(1);
        let operand = Box::new(Expr { kind, span });
        Ok(Some(if is_changeset {
            AsOf::Changeset(operand)
        } else {
            AsOf::Timestamp(operand)
        }))
    }

    // ---- data changes -------------------------------------------------------------------------

    fn insert(&mut self) -> PResult<Statement> {
        self.bump();
        self.expect_kw("into")?;
        let table = self.object_name()?;
        let mut columns = Vec::new();
        if self.at(&Tok::LParen) {
            if self.is_kw_at(1, "select") {
                return Err(self.unsupported_here("parenthesized query"));
            }
            columns = self.paren_ident_list()?;
        }
        let source = if self.is_kw("values") {
            self.bump();
            InsertSource::Values(self.value_rows()?)
        } else if self.is_kw("select") {
            InsertSource::Query(Box::new(self.query()?))
        } else if self.is_kw("default") {
            return Err(self.unsupported_here("DEFAULT VALUES"));
        } else if self.is_kw("with") {
            return Err(self.unsupported_here("WITH (common table expressions)"));
        } else {
            return Err(self.unexpected("VALUES or SELECT"));
        };
        if self.is_kw("on") {
            return Err(self.unsupported_here("INSERT ... ON CONFLICT"));
        }
        let returning = self.returning()?;
        Ok(Statement::Insert(Insert {
            table,
            columns,
            source,
            returning,
        }))
    }

    fn value_rows(&mut self) -> PResult<Vec<Vec<Expr>>> {
        let mut rows: Vec<Vec<Expr>> = Vec::new();
        loop {
            let open = self.expect(&Tok::LParen, "`(`")?;
            let mut row = vec![self.expr()?];
            while self.eat(&Tok::Comma) {
                row.push(self.expr()?);
            }
            self.expect(&Tok::RParen, "`)` or `,`")?;
            if let Some(first) = rows.first()
                && first.len() != row.len()
            {
                return Err(ParseError::syntax(
                    "VALUES lists must all be the same length",
                    open,
                ));
            }
            rows.push(row);
            if !self.eat(&Tok::Comma) {
                return Ok(rows);
            }
        }
    }

    fn update(&mut self) -> PResult<Statement> {
        self.bump();
        let table = self.object_name()?;
        let alias = self.alias()?;
        self.expect_kw("set")?;
        let mut assignments = Vec::new();
        loop {
            let column = self.ident()?;
            self.expect(&Tok::Eq, "`=`")?;
            let value = self.expr()?;
            assignments.push(Assignment { column, value });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if self.is_kw("from") {
            return Err(self.unsupported_here("UPDATE ... FROM"));
        }
        let selection = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.returning()?;
        Ok(Statement::Update(Update {
            table,
            alias,
            assignments,
            selection,
            returning,
        }))
    }

    fn delete(&mut self) -> PResult<Statement> {
        self.bump();
        self.expect_kw("from")?;
        let table = self.object_name()?;
        let alias = self.alias()?;
        if self.is_kw("using") {
            return Err(self.unsupported_here("DELETE ... USING"));
        }
        let selection = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.returning()?;
        Ok(Statement::Delete(Delete {
            table,
            alias,
            selection,
            returning,
        }))
    }

    // ---- definitions --------------------------------------------------------------------------

    fn create(&mut self) -> PResult<Statement> {
        let span = self.bump();
        let unique = self.eat_kw("unique");
        match self.word() {
            Some("table") if !unique => {
                self.bump();
                self.create_table()
            }
            Some("index") => {
                self.bump();
                self.create_index(unique)
            }
            Some(other) if !unique => Err(ParseError::unsupported(
                format!("CREATE {}", other.to_ascii_uppercase()),
                span.join(self.peek().span),
            )),
            _ => Err(self.unexpected("INDEX")),
        }
    }

    fn if_exists(&mut self) -> PResult<bool> {
        if self.eat_kw("if") {
            self.expect_kw("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn create_table(&mut self) -> PResult<Statement> {
        let if_not_exists = if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = self.object_name()?;
        if self.is_kw("as") {
            return Err(self.unsupported_here("CREATE TABLE ... AS"));
        }
        self.expect(&Tok::LParen, "`(`")?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            match self.word() {
                Some("primary") => {
                    self.bump();
                    self.expect_kw("key")?;
                    constraints.push(TableConstraint::PrimaryKey(self.paren_ident_list()?));
                }
                Some("unique") => {
                    self.bump();
                    constraints.push(TableConstraint::Unique(self.paren_ident_list()?));
                }
                Some("constraint") => return Err(self.unsupported_here("named constraints")),
                Some("foreign" | "references") => {
                    return Err(self.unsupported_here("foreign keys"));
                }
                Some("check") => return Err(self.unsupported_here("CHECK constraints")),
                _ => columns.push(self.column_def()?),
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "`)` or `,`")?;
        if columns.is_empty() {
            return Err(ParseError::syntax(
                "a table needs at least one column",
                name.0.first().map(|i| i.span).unwrap_or_default(),
            ));
        }
        Ok(Statement::CreateTable(CreateTable {
            name,
            if_not_exists,
            columns,
            constraints,
        }))
    }

    fn column_def(&mut self) -> PResult<ColumnDef> {
        let name = self.ident()?;
        let data_type = self.data_type()?;
        let mut def = ColumnDef {
            name,
            data_type,
            not_null: false,
            primary_key: false,
            unique: false,
            default: None,
        };
        loop {
            match self.word() {
                Some("not") => {
                    self.bump();
                    self.expect_kw("null")?;
                    def.not_null = true;
                }
                Some("primary") => {
                    self.bump();
                    self.expect_kw("key")?;
                    def.primary_key = true;
                }
                Some("unique") => {
                    self.bump();
                    def.unique = true;
                }
                Some("default") => {
                    let at = self.bump();
                    if def.default.is_some() {
                        return Err(ParseError::syntax("multiple DEFAULT clauses", at));
                    }
                    let value = self.expr_bp(BP_OTHER)?;
                    if !is_constant(&value) {
                        return Err(ParseError::unsupported(
                            "DEFAULT that is not a constant",
                            value.span,
                        ));
                    }
                    def.default = Some(value);
                }
                Some("references") => return Err(self.unsupported_here("foreign keys")),
                Some("check") => return Err(self.unsupported_here("CHECK constraints")),
                Some("constraint") => return Err(self.unsupported_here("named constraints")),
                Some("array") => return Err(self.unsupported_here("arrays")),
                Some("collate") => return Err(self.unsupported_here("COLLATE")),
                Some("generated") => return Err(self.unsupported_here("generated columns")),
                _ => return Ok(def),
            }
        }
    }

    fn create_index(&mut self, unique: bool) -> PResult<Statement> {
        let if_not_exists = if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = if self.is_kw("on") {
            None
        } else {
            Some(self.ident()?)
        };
        self.expect_kw("on")?;
        let table = self.object_name()?;
        if self.is_kw("using") {
            return Err(self.unsupported_here("index methods other than the default B-tree"));
        }
        let columns = self.paren_ident_list()?;
        Ok(Statement::CreateIndex(CreateIndex {
            name,
            unique,
            if_not_exists,
            table,
            columns,
        }))
    }

    fn alter(&mut self) -> PResult<Statement> {
        let span = self.bump();
        if !self.is_kw("table") {
            let what = self.word().unwrap_or("").to_ascii_uppercase();
            return Err(ParseError::unsupported(format!("ALTER {what}"), span));
        }
        self.bump();
        let name = self.object_name()?;
        let action = match self.word() {
            Some("add") => {
                self.bump();
                if matches!(
                    self.word(),
                    Some("constraint" | "primary" | "unique" | "foreign" | "check")
                ) {
                    return Err(self.unsupported_here("ADD CONSTRAINT"));
                }
                self.eat_kw("column");
                AlterAction::AddColumn(self.column_def()?)
            }
            Some("drop") => {
                self.bump();
                if self.is_kw("constraint") {
                    return Err(self.unsupported_here("DROP CONSTRAINT"));
                }
                self.eat_kw("column");
                AlterAction::DropColumn(self.ident()?)
            }
            Some("rename") => {
                self.bump();
                if self.eat_kw("to") {
                    AlterAction::RenameTable(self.ident()?)
                } else {
                    if self.is_kw("constraint") {
                        return Err(self.unsupported_here("RENAME CONSTRAINT"));
                    }
                    self.eat_kw("column");
                    let from = self.ident()?;
                    self.expect_kw("to")?;
                    AlterAction::RenameColumn {
                        from,
                        to: self.ident()?,
                    }
                }
            }
            Some("alter") => return Err(self.unsupported_here("ALTER COLUMN")),
            Some(other) => {
                let feature = format!("ALTER TABLE ... {}", other.to_ascii_uppercase());
                return Err(self.unsupported_here(&feature));
            }
            None => return Err(self.unexpected("ADD, DROP or RENAME")),
        };
        Ok(Statement::AlterTable(AlterTable { name, action }))
    }

    fn drop_stmt(&mut self) -> PResult<Statement> {
        let span = self.bump();
        match self.word() {
            Some("table") => {
                self.bump();
                let if_exists = self.if_exists()?;
                Ok(Statement::DropTable {
                    name: self.object_name()?,
                    if_exists,
                })
            }
            Some("index") => {
                self.bump();
                let if_exists = self.if_exists()?;
                Ok(Statement::DropIndex {
                    name: self.object_name()?,
                    if_exists,
                })
            }
            Some(other) => Err(ParseError::unsupported(
                format!("DROP {}", other.to_ascii_uppercase()),
                span,
            )),
            None => Err(self.unexpected("TABLE or INDEX")),
        }
    }

    fn unsigned(&mut self, what: &str) -> PResult<u32> {
        let span = self.peek().span;
        if let Tok::Number(text) = &self.peek().tok
            && let Ok(n) = text.parse::<u32>()
        {
            self.bump();
            return Ok(n);
        }
        Err(ParseError::syntax(format!("expected {what}"), span))
    }

    fn data_type(&mut self) -> PResult<DataType> {
        let span = self.peek().span;
        let Some(word) = self.word().map(str::to_string) else {
            if let Tok::Word { text, .. } = &self.peek().tok {
                return Err(ParseError::unsupported(format!("data type `{text}`"), span));
            }
            return Err(self.unexpected("a data type"));
        };
        self.bump();
        let ty = match word.as_str() {
            "boolean" | "bool" => DataType::Boolean,
            "smallint" | "int2" => DataType::SmallInt,
            "integer" | "int" | "int4" => DataType::Integer,
            "bigint" | "int8" => DataType::BigInt,
            "double" => {
                self.expect_kw("precision")?;
                DataType::DoublePrecision
            }
            "float8" => DataType::DoublePrecision,
            "numeric" | "decimal" => self.numeric_modifiers(span)?,
            "text" => DataType::Text,
            "varchar" => DataType::Varchar(self.varchar_length()?),
            "bytea" => DataType::Bytea,
            "timestamptz" => DataType::Timestamptz,
            "timestamp" => {
                if self.eat_kw("with") {
                    self.expect_kw("time")?;
                    self.expect_kw("zone")?;
                    DataType::Timestamptz
                } else {
                    return Err(ParseError::unsupported("timestamp without time zone", span));
                }
            }
            "date" => DataType::Date,
            "uuid" => DataType::Uuid,
            "jsonb" => DataType::Jsonb,
            "character" if self.is_kw("varying") => {
                self.bump();
                DataType::Varchar(self.varchar_length()?)
            }
            "array" => return Err(ParseError::unsupported("arrays", span)),
            other => {
                return Err(ParseError::unsupported(
                    format!("data type `{other}`"),
                    span,
                ));
            }
        };
        Ok(ty)
    }

    fn numeric_modifiers(&mut self, span: Span) -> PResult<DataType> {
        if !self.eat(&Tok::LParen) {
            return Ok(DataType::Numeric(None));
        }
        let p = self.unsigned("a precision")?;
        let s = if self.eat(&Tok::Comma) {
            Some(self.unsigned("a scale")?)
        } else {
            None
        };
        self.expect(&Tok::RParen, "`)`")?;
        if p == 0 {
            return Err(ParseError::syntax(
                "numeric precision must be at least 1",
                span,
            ));
        }
        if p > 38 {
            return Err(ParseError::unsupported("numeric precision above 38", span));
        }
        if s.is_some_and(|s| s > p) {
            return Err(ParseError::syntax(
                "numeric scale must not exceed its precision",
                span,
            ));
        }
        let to_u8 = |n: u32| u8::try_from(n).unwrap_or(u8::MAX);
        Ok(DataType::Numeric(Some((to_u8(p), s.map(to_u8)))))
    }

    fn varchar_length(&mut self) -> PResult<Option<u32>> {
        if !self.eat(&Tok::LParen) {
            return Ok(None);
        }
        let span = self.peek().span;
        let n = self.unsigned("a length")?;
        self.expect(&Tok::RParen, "`)`")?;
        if n == 0 || n > 10_485_760 {
            return Err(ParseError::syntax(
                "varchar length must be between 1 and 10485760",
                span,
            ));
        }
        Ok(Some(n))
    }

    // ---- expressions --------------------------------------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        self.expr_bp(0)
    }

    fn expr_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        self.nested(|p| p.expr_loop(min_bp))
    }

    /// Finishes a node whose children's deepest tree has depth `child_depth`.
    fn grow(&mut self, child_depth: usize, kind: ExprKind, span: Span) -> PResult<Expr> {
        let d = child_depth + 1;
        if d > MAX_DEPTH {
            return Err(ParseError::too_large(
                format!("nesting is deeper than {MAX_DEPTH}"),
                span,
            ));
        }
        self.td = d;
        Ok(Expr { kind, span })
    }

    fn leaf(&mut self, kind: ExprKind, span: Span) -> Expr {
        self.td = 1;
        Expr { kind, span }
    }

    fn infix(&self) -> Infix {
        match &self.peek().tok {
            Tok::Eq => Infix::Bin(BinaryOp::Eq, BP_CMP),
            Tok::NotEq => Infix::Bin(BinaryOp::NotEq, BP_CMP),
            Tok::Lt => Infix::Bin(BinaryOp::Lt, BP_CMP),
            Tok::LtEq => Infix::Bin(BinaryOp::LtEq, BP_CMP),
            Tok::Gt => Infix::Bin(BinaryOp::Gt, BP_CMP),
            Tok::GtEq => Infix::Bin(BinaryOp::GtEq, BP_CMP),
            Tok::Concat => Infix::Bin(BinaryOp::Concat, BP_OTHER),
            Tok::Arrow => Infix::Bin(BinaryOp::JsonGet, BP_OTHER),
            Tok::ArrowText => Infix::Bin(BinaryOp::JsonGetText, BP_OTHER),
            Tok::Contains => Infix::Bin(BinaryOp::Contains, BP_OTHER),
            Tok::Plus => Infix::Bin(BinaryOp::Add, BP_ADD),
            Tok::Minus => Infix::Bin(BinaryOp::Sub, BP_ADD),
            Tok::Star => Infix::Bin(BinaryOp::Mul, BP_MUL),
            Tok::Slash => Infix::Bin(BinaryOp::Div, BP_MUL),
            Tok::Percent => Infix::Bin(BinaryOp::Mod, BP_MUL),
            Tok::DoubleColon => Infix::Cast,
            Tok::Word {
                text,
                quoted: false,
            } => match text.as_str() {
                "or" => Infix::Bin(BinaryOp::Or, BP_OR),
                "and" => Infix::Bin(BinaryOp::And, BP_AND),
                "is" => Infix::Is,
                "in" => Infix::Pred {
                    negated: false,
                    kind: Pred::In,
                },
                "between" => Infix::Pred {
                    negated: false,
                    kind: Pred::Between,
                },
                "like" => Infix::Pred {
                    negated: false,
                    kind: Pred::Like,
                },
                "ilike" => Infix::Unsupported("ILIKE"),
                "similar" => Infix::Unsupported("SIMILAR TO"),
                "collate" => Infix::Unsupported("COLLATE"),
                "at" if self.is_kw_at(1, "time") => Infix::Unsupported("AT TIME ZONE"),
                "not" => match self.word_at(1) {
                    Some("in") => Infix::Pred {
                        negated: true,
                        kind: Pred::In,
                    },
                    Some("between") => Infix::Pred {
                        negated: true,
                        kind: Pred::Between,
                    },
                    Some("like") => Infix::Pred {
                        negated: true,
                        kind: Pred::Like,
                    },
                    Some("ilike") => Infix::Unsupported("ILIKE"),
                    Some("similar") => Infix::Unsupported("SIMILAR TO"),
                    _ => Infix::None,
                },
                _ => Infix::None,
            },
            _ => Infix::None,
        }
    }

    fn expr_loop(&mut self, min_bp: u8) -> PResult<Expr> {
        let mut left = self.prefix()?;
        let mut left_depth = self.td;
        loop {
            let infix = self.infix();
            let bp = infix.bp();
            if bp == 0 || bp < min_bp {
                break;
            }
            match infix {
                Infix::None => break,
                Infix::Unsupported(feature) => {
                    return Err(self.unsupported_here(feature));
                }
                Infix::Bin(op, bp) => {
                    self.bump();
                    let right = self.expr_bp(bp + 1)?;
                    let span = left.span.join(right.span);
                    let depth = left_depth.max(self.td);
                    left = self.grow(
                        depth,
                        ExprKind::Binary {
                            op,
                            left: Box::new(left),
                            right: Box::new(right),
                        },
                        span,
                    )?;
                }
                Infix::Is => {
                    self.bump();
                    let negated = self.eat_kw("not");
                    let end = self.peek().span;
                    if !self.eat_kw("null") {
                        return Err(match self.word() {
                            Some(w @ ("true" | "false" | "unknown" | "distinct")) => {
                                ParseError::unsupported(
                                    format!("IS {}", w.to_ascii_uppercase()),
                                    end,
                                )
                            }
                            _ => self.unexpected("NULL"),
                        });
                    }
                    let span = left.span.join(end);
                    left = self.grow(
                        left_depth,
                        ExprKind::IsNull {
                            expr: Box::new(left),
                            negated,
                        },
                        span,
                    )?;
                }
                Infix::Cast => {
                    self.bump();
                    let data_type = self.data_type()?;
                    let span = left.span.join(self.prev_span());
                    left = self.grow(
                        left_depth,
                        ExprKind::Cast {
                            expr: Box::new(left),
                            data_type,
                        },
                        span,
                    )?;
                }
                Infix::Pred { negated, kind } => {
                    if negated {
                        self.bump();
                    }
                    self.bump();
                    left = self.predicate(left, left_depth, negated, kind)?;
                }
            }
            left_depth = self.td;
        }
        self.td = left_depth;
        Ok(left)
    }

    fn prev_span(&self) -> Span {
        self.tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or_default()
    }

    fn predicate(
        &mut self,
        left: Expr,
        left_depth: usize,
        negated: bool,
        kind: Pred,
    ) -> PResult<Expr> {
        match kind {
            Pred::In => {
                self.expect(&Tok::LParen, "`(` after IN")?;
                if self.is_kw("select") || self.is_kw("with") || self.is_kw("values") {
                    let query = self.query()?;
                    let depth = left_depth.max(self.td);
                    let end = self.expect(&Tok::RParen, "`)`")?;
                    let span = left.span.join(end);
                    return self.grow(
                        depth,
                        ExprKind::InSubquery {
                            expr: Box::new(left),
                            query: Box::new(query),
                            negated,
                        },
                        span,
                    );
                }
                let mut depth = left_depth;
                let mut list = vec![self.expr()?];
                depth = depth.max(self.td);
                while self.eat(&Tok::Comma) {
                    list.push(self.expr()?);
                    depth = depth.max(self.td);
                }
                let end = self.expect(&Tok::RParen, "`)` or `,`")?;
                let span = left.span.join(end);
                self.grow(
                    depth,
                    ExprKind::InList {
                        expr: Box::new(left),
                        list,
                        negated,
                    },
                    span,
                )
            }
            Pred::Between => {
                let low = self.expr_bp(BP_OTHER)?;
                let mut depth = left_depth.max(self.td);
                self.expect_kw("and")?;
                let high = self.expr_bp(BP_OTHER)?;
                depth = depth.max(self.td);
                let span = left.span.join(high.span);
                self.grow(
                    depth,
                    ExprKind::Between {
                        expr: Box::new(left),
                        low: Box::new(low),
                        high: Box::new(high),
                        negated,
                    },
                    span,
                )
            }
            Pred::Like => {
                let pattern = self.expr_bp(BP_OTHER)?;
                if self.is_kw("escape") {
                    return Err(self.unsupported_here("LIKE ... ESCAPE"));
                }
                let depth = left_depth.max(self.td);
                let span = left.span.join(pattern.span);
                self.grow(
                    depth,
                    ExprKind::Like {
                        expr: Box::new(left),
                        pattern: Box::new(pattern),
                        negated,
                    },
                    span,
                )
            }
        }
    }

    fn prefix(&mut self) -> PResult<Expr> {
        let span = self.peek().span;
        let tok = self.peek().tok.clone();
        match tok {
            Tok::Number(n) => {
                self.bump();
                Ok(self.leaf(ExprKind::Literal(Literal::Number(n)), span))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(self.leaf(ExprKind::Literal(Literal::String(s)), span))
            }
            Tok::Param(n) => {
                self.bump();
                Ok(self.leaf(ExprKind::Param(n), span))
            }
            Tok::Minus | Tok::Plus => {
                self.bump();
                let op = if tok == Tok::Minus {
                    UnaryOp::Minus
                } else {
                    UnaryOp::Plus
                };
                let operand = self.expr_bp(BP_UNARY)?;
                let span = span.join(operand.span);
                self.grow(
                    self.td,
                    ExprKind::Unary {
                        op,
                        expr: Box::new(operand),
                    },
                    span,
                )
            }
            Tok::LParen => {
                self.bump();
                if self.is_kw("select") || self.is_kw("with") || self.is_kw("values") {
                    let query = self.query()?;
                    let end = self.expect(&Tok::RParen, "`)`")?;
                    return self.grow(self.td, ExprKind::Subquery(Box::new(query)), span.join(end));
                }
                let mut inner = self.expr()?;
                if self.at(&Tok::Comma) {
                    return Err(self.unsupported_here("row values"));
                }
                let end = self.expect(&Tok::RParen, "`)`")?;
                inner.span = span.join(end);
                Ok(inner)
            }
            Tok::Word { quoted: true, .. } => self.column_path(),
            Tok::Word { text, .. } => self.word_prefix(&text, span),
            _ => Err(self.unexpected("an expression")),
        }
    }

    fn word_prefix(&mut self, word: &str, span: Span) -> PResult<Expr> {
        match word {
            "true" | "false" => {
                self.bump();
                Ok(self.leaf(ExprKind::Literal(Literal::Boolean(word == "true")), span))
            }
            "null" => {
                self.bump();
                Ok(self.leaf(ExprKind::Literal(Literal::Null), span))
            }
            "not" => {
                self.bump();
                let operand = self.expr_bp(BP_NOT)?;
                let span = span.join(operand.span);
                self.grow(
                    self.td,
                    ExprKind::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(operand),
                    },
                    span,
                )
            }
            "case" => Err(ParseError::unsupported("CASE expressions", span)),
            "exists" => Err(ParseError::unsupported("EXISTS subqueries", span)),
            "any" | "some" | "all" => Err(ParseError::unsupported("ANY, SOME and ALL", span)),
            "array" => Err(ParseError::unsupported("arrays", span)),
            "lateral" => Err(ParseError::unsupported("LATERAL", span)),
            "interval" if matches!(self.peek_n(1).tok, Tok::Str(_)) => {
                Err(ParseError::unsupported("interval", span))
            }
            "cast" if self.peek_n(1).tok == Tok::LParen => self.cast_call(span),
            "position" if self.peek_n(1).tok == Tok::LParen => self.position_call(span),
            "trim" if self.peek_n(1).tok == Tok::LParen => self.trim_call(span),
            "extract" if self.peek_n(1).tok == Tok::LParen => self.extract_call(span),
            "date" | "timestamptz" | "uuid" | "bytea" | "jsonb"
                if matches!(self.peek_n(1).tok, Tok::Str(_)) =>
            {
                let data_type = self.data_type()?;
                let lit_span = self.peek().span;
                let Tok::Str(s) = self.peek().tok.clone() else {
                    return Err(self.unexpected("a string literal"));
                };
                self.bump();
                let inner = self.leaf(ExprKind::Literal(Literal::String(s)), lit_span);
                self.grow(
                    self.td,
                    ExprKind::Cast {
                        expr: Box::new(inner),
                        data_type,
                    },
                    span.join(lit_span),
                )
            }
            w if is_reserved(w) => Err(self.unexpected("an expression")),
            _ if self.peek_n(1).tok == Tok::LParen => self.function_call(word, span),
            _ => self.column_path(),
        }
    }

    fn column_path(&mut self) -> PResult<Expr> {
        let first = self.ident()?;
        let mut span = first.span;
        let mut parts = vec![first];
        while self.at(&Tok::Dot) {
            if parts.len() == 3 {
                return Err(self.unsupported_here("names qualified by a database"));
            }
            self.bump();
            let part = self.ident()?;
            span = span.join(part.span);
            parts.push(part);
        }
        if self.at(&Tok::LParen) {
            return Err(self.unsupported_here("schema-qualified function calls"));
        }
        Ok(self.leaf(ExprKind::Column(parts), span))
    }

    fn cast_call(&mut self, start: Span) -> PResult<Expr> {
        self.bump();
        self.bump();
        let expr = self.expr()?;
        let depth = self.td;
        self.expect_kw("as")?;
        let data_type = self.data_type()?;
        let end = self.expect(&Tok::RParen, "`)`")?;
        self.grow(
            depth,
            ExprKind::Cast {
                expr: Box::new(expr),
                data_type,
            },
            start.join(end),
        )
    }

    fn position_call(&mut self, start: Span) -> PResult<Expr> {
        self.bump();
        self.bump();
        let needle = self.expr_bp(BP_OTHER)?;
        let mut depth = self.td;
        self.expect_kw("in")?;
        let haystack = self.expr()?;
        depth = depth.max(self.td);
        let end = self.expect(&Tok::RParen, "`)`")?;
        self.grow(
            depth,
            ExprKind::Position {
                needle: Box::new(needle),
                haystack: Box::new(haystack),
            },
            start.join(end),
        )
    }

    fn trim_call(&mut self, start: Span) -> PResult<Expr> {
        self.bump();
        self.bump();
        let side = if self.eat_kw("leading") {
            TrimSide::Leading
        } else if self.eat_kw("trailing") {
            TrimSide::Trailing
        } else {
            self.eat_kw("both");
            TrimSide::Both
        };
        let mut chars = None;
        let mut depth = 0;
        let expr = if self.eat_kw("from") {
            self.expr()?
        } else {
            let first = self.expr()?;
            depth = self.td;
            if self.eat_kw("from") {
                chars = Some(Box::new(first));
                self.expr()?
            } else {
                first
            }
        };
        depth = depth.max(self.td);
        let end = self.expect(&Tok::RParen, "`)`")?;
        self.grow(
            depth,
            ExprKind::Trim {
                side,
                chars,
                expr: Box::new(expr),
            },
            start.join(end),
        )
    }

    fn extract_call(&mut self, start: Span) -> PResult<Expr> {
        self.bump();
        self.bump();
        let field = self.ident()?;
        self.expect_kw("from")?;
        let expr = self.expr()?;
        let depth = self.td;
        let end = self.expect(&Tok::RParen, "`)`")?;
        self.grow(
            depth,
            ExprKind::Extract {
                field,
                expr: Box::new(expr),
            },
            start.join(end),
        )
    }

    fn function_call(&mut self, name: &str, start: Span) -> PResult<Expr> {
        if !FUNCTIONS.contains(&name) {
            return Err(ParseError::unsupported(format!("function `{name}`"), start));
        }
        self.bump();
        self.bump();
        let mut distinct = false;
        let mut depth = 0;
        let args = if self.eat(&Tok::Star) {
            if name != "count" {
                return Err(ParseError::syntax(
                    format!("`{name}(*)` is not valid, only count(*) is"),
                    start,
                ));
            }
            FunctionArgs::Star
        } else {
            if self.is_kw("distinct") {
                if name != "count" {
                    return Err(self.unsupported_here("DISTINCT in aggregates other than count"));
                }
                self.bump();
                distinct = true;
            }
            let mut list = Vec::new();
            if !self.at(&Tok::RParen) {
                list.push(self.expr()?);
                depth = self.td;
                if name == "substring" && self.is_kw("from") {
                    self.bump();
                    list.push(self.expr()?);
                    depth = depth.max(self.td);
                    if self.eat_kw("for") {
                        list.push(self.expr()?);
                        depth = depth.max(self.td);
                    }
                } else {
                    while self.eat(&Tok::Comma) {
                        list.push(self.expr()?);
                        depth = depth.max(self.td);
                    }
                }
            }
            if distinct && list.len() != 1 {
                return Err(ParseError::syntax(
                    "count(DISTINCT ...) takes one argument",
                    start,
                ));
            }
            FunctionArgs::List(list)
        };
        if self.is_kw("order") {
            return Err(self.unsupported_here("ORDER BY inside an aggregate"));
        }
        let end = self.expect(&Tok::RParen, "`)` or `,`")?;
        if self.is_kw("over") {
            return Err(self.unsupported_here("window functions"));
        }
        if self.is_kw("filter") && self.peek_n(1).tok == Tok::LParen {
            return Err(self.unsupported_here("FILTER clauses"));
        }
        self.grow(
            depth,
            ExprKind::Function(Function {
                name: name.to_string(),
                distinct,
                args,
            }),
            start.join(end),
        )
    }
}

/// A literal, optionally signed or cast: what a column `DEFAULT` may be.
fn is_constant(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal(_) => true,
        ExprKind::Unary {
            op: UnaryOp::Minus | UnaryOp::Plus,
            expr,
        }
        | ExprKind::Cast { expr, .. } => is_constant(expr),
        _ => false,
    }
}
