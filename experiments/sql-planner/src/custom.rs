//! Hand parser for an explicitly bounded spike grammar; not the complete target SQL subset.
use crate::{Error, Result, ast::*};
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Number(i64),
    Text(String),
    Param(usize),
    Symbol(String),
}
struct Parser {
    tokens: Vec<Token>,
    n: usize,
}
fn lex(sql: &str) -> Result<Vec<Token>> {
    let mut chars = sql.chars().peekable();
    let mut out = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '-' && chars.peek() == Some(&'-') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        if c == '\'' {
            let mut s = String::new();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                        s.push('\'');
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    s.push(c);
                }
            }
            if !closed {
                return Err(Error::syntax("unterminated string"));
            }
            out.push(Token::Text(s));
        } else if c.is_ascii_digit() {
            let mut n = c.to_string();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                n.push(chars.next().unwrap());
            }
            out.push(Token::Number(
                n.parse().map_err(|_| Error::syntax("integer overflow"))?,
            ));
        } else if c == '$' {
            let mut n = String::new();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                n.push(chars.next().unwrap());
            }
            let n = n
                .parse::<usize>()
                .map_err(|_| Error::syntax("invalid parameter"))?;
            if n == 0 {
                return Err(Error::syntax("parameters start at $1"));
            }
            out.push(Token::Param(n));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let mut s = c.to_ascii_lowercase().to_string();
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
            {
                s.push(chars.next().unwrap().to_ascii_lowercase());
            }
            out.push(Token::Word(s));
        } else if "(),;*+=<>-".contains(c) {
            let mut s = c.to_string();
            if "<>".contains(c) && chars.peek() == Some(&'=') {
                s.push(chars.next().unwrap());
            }
            out.push(Token::Symbol(s));
        } else {
            return Err(Error::syntax(format!("unrecognized character {c}")));
        }
    }
    Ok(out)
}
impl Parser {
    fn peek(&self, s: &str) -> bool {
        self.tokens
            .get(self.n)
            .is_some_and(|t| matches!(t,Token::Word(w)|Token::Symbol(w) if w==s))
    }
    fn eat(&mut self, s: &str) -> bool {
        if self.peek(s) {
            self.n += 1;
            true
        } else {
            false
        }
    }
    fn need(&mut self, s: &str) -> Result<()> {
        if self.eat(s) {
            Ok(())
        } else {
            Err(Error::syntax(format!("expected {s} at token {}", self.n)))
        }
    }
    fn ident(&mut self) -> Result<String> {
        match self.tokens.get(self.n).cloned() {
            Some(Token::Word(s)) => {
                self.n += 1;
                Ok(s)
            }
            _ => Err(Error::syntax("expected identifier")),
        }
    }
    fn atom(&mut self) -> Result<Expr> {
        let t = self
            .tokens
            .get(self.n)
            .cloned()
            .ok_or_else(|| Error::syntax("missing expression"))?;
        self.n += 1;
        match t {
            Token::Number(n) => Ok(Expr::Lit(Value::Int(n))),
            Token::Text(s) => Ok(Expr::Lit(Value::Text(s))),
            Token::Param(n) => Ok(Expr::Param(n)),
            Token::Symbol(s) if s == "(" => {
                let e = self.expr(0)?;
                self.need(")")?;
                Ok(e)
            }
            Token::Word(s) if s == "count" => {
                self.need("(")?;
                self.need("*")?;
                self.need(")")?;
                Ok(Expr::Count)
            }
            Token::Word(s) if s == "null" => Ok(Expr::Lit(Value::Null)),
            Token::Word(s) => {
                if self.peek("(") {
                    return Err(Error::unsupported(format!("function {s}")));
                }
                Ok(Expr::Col(s))
            }
            _ => Err(Error::syntax("invalid expression")),
        }
    }
    fn expr(&mut self, min: u8) -> Result<Expr> {
        let mut left = self.atom()?;
        while let Some(Token::Word(op) | Token::Symbol(op)) = self.tokens.get(self.n) {
            let prec = match op.as_str() {
                "or" => 1,
                "and" => 2,
                "=" | ">" | "<" | ">=" | "<=" => 3,
                "+" | "-" => 4,
                _ => 0,
            };
            if prec == 0 || prec < min {
                break;
            }
            let op = op.clone();
            self.n += 1;
            let right = self.expr(prec + 1)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }
    fn exprs(&mut self) -> Result<Vec<Expr>> {
        let mut out = vec![self.expr(0)?];
        while self.eat(",") {
            out.push(self.expr(0)?);
        }
        Ok(out)
    }
    fn columns(&mut self) -> Result<Vec<Expr>> {
        let mut out = Vec::new();
        loop {
            out.push(self.expr(0)?);
            if self.eat("as") {
                self.ident()?;
            }
            if !self.eat(",") {
                break;
            }
        }
        Ok(out)
    }
    fn plan(&mut self) -> Result<Plan> {
        if self.eat("begin") {
            return Ok(Plan::Begin);
        }
        if self.eat("commit") {
            return Ok(Plan::Commit);
        }
        if self.eat("rollback") {
            return Ok(Plan::Rollback);
        }
        if self.eat("select") {
            let columns = self.columns()?;
            self.need("from")?;
            let table = self.ident()?;
            let historical = if self.eat("as") {
                self.need("of")?;
                if self.eat("changeset") {
                    match self.tokens.get(self.n).cloned() {
                        Some(Token::Text(s) | Token::Word(s)) => {
                            self.n += 1;
                            Some(Historical::ChangeSet(s))
                        }
                        _ => return Err(Error::syntax("expected change set id")),
                    }
                } else {
                    self.need("timestamp")?;
                    match self.tokens.get(self.n).cloned() {
                        Some(Token::Number(n)) if n >= 0 => {
                            self.n += 1;
                            Some(Historical::Timestamp(n as u64))
                        }
                        _ => {
                            return Err(Error::syntax(
                                "spike timestamp is a fixture HLC millisecond",
                            ));
                        }
                    }
                }
            } else {
                None
            };
            let join = if self.eat("inner") {
                self.need("join")?;
                let t = self.ident()?;
                self.need("on")?;
                Some((t, self.expr(0)?))
            } else if self.eat("join") {
                let t = self.ident()?;
                self.need("on")?;
                Some((t, self.expr(0)?))
            } else {
                None
            };
            let filter = if self.eat("where") {
                Some(self.expr(0)?)
            } else {
                None
            };
            let group = if self.eat("group") {
                self.need("by")?;
                self.exprs()?
            } else {
                vec![]
            };
            let having = if self.eat("having") {
                Some(self.expr(0)?)
            } else {
                None
            };
            let order = if self.eat("order") {
                self.need("by")?;
                self.exprs()?
            } else {
                vec![]
            };
            let limit = if self.eat("limit") {
                match self.tokens.get(self.n) {
                    Some(Token::Number(n)) if *n >= 0 => {
                        let n = *n as usize;
                        self.n += 1;
                        Some(n)
                    }
                    _ => return Err(Error::syntax("expected LIMIT integer")),
                }
            } else {
                None
            };
            Ok(Plan::Select(Select {
                columns,
                table,
                join,
                historical,
                filter,
                group,
                having,
                order,
                limit,
            }))
        } else if self.eat("update") {
            let table = self.ident()?;
            self.need("set")?;
            let mut assignments = Vec::new();
            loop {
                let k = self.ident()?;
                self.need("=")?;
                assignments.push((k, self.expr(0)?));
                if !self.eat(",") {
                    break;
                }
            }
            let filter = if self.eat("where") {
                Some(self.expr(0)?)
            } else {
                None
            };
            let returning = if self.eat("returning") {
                self.columns()?
            } else {
                vec![]
            };
            Ok(Plan::Update {
                table,
                assignments,
                filter,
                returning,
            })
        } else if self.eat("insert") {
            self.need("into")?;
            let table = self.ident()?;
            self.need("(")?;
            let mut columns = vec![self.ident()?];
            while self.eat(",") {
                columns.push(self.ident()?);
            }
            self.need(")")?;
            self.need("values")?;
            self.need("(")?;
            let values = self.exprs()?;
            self.need(")")?;
            let returning = if self.eat("returning") {
                self.columns()?
            } else {
                vec![]
            };
            Ok(Plan::Insert {
                table,
                columns,
                values,
                returning,
            })
        } else {
            Err(Error::unsupported("statement outside spike grammar"))
        }
    }
}
pub fn parse(sql: &str) -> Result<Plan> {
    let mut p = Parser {
        tokens: lex(sql)?,
        n: 0,
    };
    let plan = p.plan()?;
    p.eat(";");
    if p.n != p.tokens.len() {
        return Err(Error::unsupported(format!(
            "unconsumed SQL at token {}",
            p.n
        )));
    }
    Ok(plan)
}

/// Token-level syntax extension shared by external parsers; it does not plan or execute SQL.
/// Only `FROM name AS OF CHANGESET 'id'` / numeric fixture timestamp is stripped.
pub fn strip_historical(sql: &str) -> Result<(String, Option<Historical>)> {
    let mut tokens = lex(sql)?;
    let mut found = None;
    let mut n = 0;
    while n + 3 < tokens.len() {
        if matches!(&tokens[n],Token::Word(w)if w=="as")
            && matches!(&tokens[n+1],Token::Word(w)if w=="of")
        {
            if found.is_some() || n < 2 || !matches!(&tokens[n-2],Token::Word(w)if w=="from") {
                return Err(Error::unsupported("AS OF only after one FROM table"));
            }
            found = Some(match (&tokens[n + 2], &tokens[n + 3]) {
                (Token::Word(kind), Token::Text(id) | Token::Word(id)) if kind == "changeset" => {
                    Historical::ChangeSet(id.clone())
                }
                (Token::Word(kind), Token::Number(n)) if kind == "timestamp" && *n >= 0 => {
                    Historical::Timestamp(*n as u64)
                }
                _ => return Err(Error::syntax("invalid AS OF reference")),
            });
            tokens.drain(n..n + 4);
        } else {
            n += 1;
        }
    }
    let cleaned = tokens
        .into_iter()
        .map(|t| match t {
            Token::Word(w) | Token::Symbol(w) => w,
            Token::Number(n) => n.to_string(),
            Token::Param(n) => format!("${n}"),
            Token::Text(t) => format!("'{}'", t.replace('\'', "''")),
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok((cleaned, found))
}
