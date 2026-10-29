//! Turns SQL text into tokens.
//!
//! The lexer follows PostgreSQL for the parts the subset uses: unquoted words fold to lower case,
//! `''` and `""` escape quotes, comments nest, and a run of operator characters is one operator.
//! Spellings the subset does not have (escape strings, dollar quoting, other operators) are refused
//! here with the name of the feature.

use crate::error::{ParseError, Span};
use crate::{MAX_IDENT_LEN, MAX_INPUT_LEN, MAX_TOKENS};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    Word { text: String, quoted: bool },
    Number(String),
    Str(String),
    Param(u32),
    LParen,
    RParen,
    Comma,
    Semi,
    Dot,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Concat,
    Arrow,
    ArrowText,
    Contains,
    DoubleColon,
    Eof,
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub(crate) tok: Tok,
    pub(crate) span: Span,
}

/// Words that cannot be used as a bare name or alias. The printer quotes them.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "full",
    "group",
    "having",
    "ilike",
    "in",
    "inner",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "lateral",
    "leading",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "natural",
    "not",
    "notnull",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "over",
    "primary",
    "references",
    "returning",
    "right",
    "select",
    "session_user",
    "set",
    "similar",
    "some",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "when",
    "where",
    "window",
    "with",
];

pub(crate) fn is_reserved(word: &str) -> bool {
    RESERVED.binary_search(&word).is_ok()
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || (!c.is_ascii() && c.is_alphabetic())
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == '$'
}

fn is_op_char(c: char) -> bool {
    "+-*/<>=~!@#%^&|?".contains(c)
}

struct Lexer<'a> {
    input: &'a str,
    pos: usize,
}

#[cfg(test)]
pub(crate) fn lex(input: &str) -> Result<Vec<Token>, ParseError> {
    match lex_partial(input) {
        (_, Some(e)) => Err(e),
        (tokens, None) => Ok(tokens),
    }
}

/// Tokens of `input`, plus the first lexical error if there was one. On an error the tokens stop
/// before it and end with an `Eof` at the error position, so the parser can report an earlier
/// syntax error first.
pub(crate) fn lex_partial(input: &str) -> (Vec<Token>, Option<ParseError>) {
    let mut tokens = Vec::new();
    if input.len() > MAX_INPUT_LEN {
        let e = ParseError::too_large(
            format!(
                "input is {} bytes, the limit is {MAX_INPUT_LEN}",
                input.len()
            ),
            Span::new(0, 0),
        );
        return (tokens, Some(e));
    }
    let mut lexer = Lexer { input, pos: 0 };
    loop {
        let before = lexer.pos;
        let step = lexer.skip_trivia().and_then(|()| {
            let start = lexer.pos;
            lexer.next_tok().map(|t| (start, t))
        });
        let (start, tok) = match step {
            Ok(v) => v,
            Err(e) => {
                let at = e.span().start.min(input.len()).max(before);
                tokens.push(Token {
                    tok: Tok::Eof,
                    span: Span::new(at, at),
                });
                return (tokens, Some(e));
            }
        };
        let Some(tok) = tok else {
            tokens.push(Token {
                tok: Tok::Eof,
                span: Span::new(input.len(), input.len()),
            });
            return (tokens, None);
        };
        if tokens.len() >= MAX_TOKENS {
            let e = ParseError::too_large(
                format!("more than {MAX_TOKENS} tokens"),
                Span::new(start, lexer.pos),
            );
            tokens.push(Token {
                tok: Tok::Eof,
                span: Span::new(start, start),
            });
            return (tokens, Some(e));
        }
        tokens.push(Token {
            tok,
            span: Span::new(start, lexer.pos),
        });
    }
}

impl Lexer<'_> {
    fn rest(&self) -> &str {
        self.input.get(self.pos..).unwrap_or("")
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.input.as_bytes().get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(start, self.pos)
    }

    fn skip_trivia(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\n' | '\r' | '\u{c}' | '\u{b}') => {
                    self.bump();
                }
                Some('-') if self.peek_at(1) == Some(b'-') => {
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                Some('/') if self.peek_at(1) == Some(b'*') => {
                    let start = self.pos;
                    self.pos += 2;
                    let mut depth = 1usize;
                    while depth > 0 {
                        match (self.peek_at(0), self.peek_at(1)) {
                            (Some(b'/'), Some(b'*')) => {
                                depth += 1;
                                self.pos += 2;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                self.pos += 2;
                            }
                            (Some(_), _) => {
                                self.bump();
                            }
                            (None, _) => {
                                return Err(ParseError::syntax(
                                    "unterminated /* comment",
                                    self.span_from(start),
                                ));
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// The next token, or `None` at the end of the input.
    fn next_tok(&mut self) -> Result<Option<Tok>, ParseError> {
        let start = self.pos;
        let Some(c) = self.peek() else {
            return Ok(None);
        };
        let tok = match c {
            '\'' => Tok::Str(self.quoted('\'', "unterminated quoted string")?),
            '"' => {
                let text = self.quoted('"', "unterminated quoted identifier")?;
                if text.is_empty() {
                    return Err(ParseError::syntax(
                        "zero-length delimited identifier",
                        self.span_from(start),
                    ));
                }
                self.check_ident_len(&text, start)?;
                Tok::Word { text, quoted: true }
            }
            '(' => self.single(Tok::LParen),
            ')' => self.single(Tok::RParen),
            ',' => self.single(Tok::Comma),
            ';' => self.single(Tok::Semi),
            '.' if self.peek_at(1).is_some_and(|b| b.is_ascii_digit()) => self.number()?,
            '.' => self.single(Tok::Dot),
            '0'..='9' => self.number()?,
            ':' if self.peek_at(1) == Some(b':') => {
                self.pos += 2;
                Tok::DoubleColon
            }
            '$' => self.dollar()?,
            '[' | ']' => {
                self.bump();
                return Err(ParseError::unsupported("arrays", self.span_from(start)));
            }
            c if is_op_char(c) => self.operator()?,
            c if is_ident_start(c) => self.word()?,
            _ => {
                self.bump();
                return Err(ParseError::syntax(
                    format!("unexpected character `{c}`"),
                    self.span_from(start),
                ));
            }
        };
        Ok(Some(tok))
    }

    fn single(&mut self, tok: Tok) -> Tok {
        self.bump();
        tok
    }

    fn check_ident_len(&self, text: &str, start: usize) -> Result<(), ParseError> {
        if text.len() > MAX_IDENT_LEN {
            return Err(ParseError::too_large(
                format!("identifier is longer than {MAX_IDENT_LEN} bytes"),
                self.span_from(start),
            ));
        }
        Ok(())
    }

    /// A string or quoted identifier; the opening quote is the current character.
    fn quoted(&mut self, quote: char, unterminated: &str) -> Result<String, ParseError> {
        let start = self.pos;
        self.bump();
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(ParseError::syntax(unterminated, self.span_from(start))),
                Some(c) if c == quote => {
                    if self.peek() == Some(quote) {
                        self.bump();
                        out.push(quote);
                    } else {
                        return Ok(out);
                    }
                }
                Some(c) => out.push(c),
            }
        }
    }

    fn word(&mut self) -> Result<Tok, ParseError> {
        let start = self.pos;
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        let text = self
            .input
            .get(start..self.pos)
            .unwrap_or("")
            .to_ascii_lowercase();
        self.check_ident_len(&text, start)?;
        if self.peek_at(0) == Some(b'\'') && matches!(text.as_str(), "e" | "b" | "x" | "n" | "u") {
            return Err(ParseError::unsupported(
                "escape, bit-string and national string literals",
                Span::new(start, self.pos + 1),
            ));
        }
        Ok(Tok::Word {
            text,
            quoted: false,
        })
    }

    fn number(&mut self) -> Result<Tok, ParseError> {
        let start = self.pos;
        self.digits();
        if self.peek_at(0) == Some(b'.') {
            self.pos += 1;
            self.digits();
        }
        if matches!(self.peek_at(0), Some(b'e' | b'E')) {
            let sign = usize::from(matches!(self.peek_at(1), Some(b'+' | b'-')));
            if self.peek_at(1 + sign).is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1 + sign;
                self.digits();
            }
        }
        if self.peek().is_some_and(is_ident_start) {
            self.bump();
            return Err(ParseError::syntax(
                "trailing junk after numeric literal",
                self.span_from(start),
            ));
        }
        Ok(Tok::Number(
            self.input.get(start..self.pos).unwrap_or("").to_string(),
        ))
    }

    fn digits(&mut self) {
        while self.peek_at(0).is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
    }

    fn dollar(&mut self) -> Result<Tok, ParseError> {
        let start = self.pos;
        self.bump();
        if self.peek_at(0).is_some_and(|b| b.is_ascii_digit()) {
            self.digits();
            if self.peek().is_some_and(is_ident_continue) {
                return Err(ParseError::syntax(
                    "trailing junk after parameter number",
                    self.span_from(start),
                ));
            }
            let text = self.input.get(start + 1..self.pos).unwrap_or("");
            return match text.parse::<u32>() {
                Ok(n) if (1..=65_535).contains(&n) => Ok(Tok::Param(n)),
                _ => Err(ParseError::syntax(
                    "parameter number must be between 1 and 65535",
                    self.span_from(start),
                )),
            };
        }
        if self.peek().is_some_and(|c| c == '$' || is_ident_start(c)) {
            return Err(ParseError::unsupported(
                "dollar-quoted strings",
                self.span_from(start),
            ));
        }
        Err(ParseError::syntax(
            "unexpected character `$`",
            self.span_from(start),
        ))
    }

    fn operator(&mut self) -> Result<Tok, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if !is_op_char(c) {
                break;
            }
            // A comment start ends the operator.
            if self.pos > start
                && ((c == '-' && self.peek_at(1) == Some(b'-'))
                    || (c == '/' && self.peek_at(1) == Some(b'*')))
            {
                break;
            }
            self.bump();
        }
        let mut run = self.input.get(start..self.pos).unwrap_or("");
        // As in PostgreSQL, `=-1` is `=` then `-1`: a trailing sign is split off unless the
        // operator contains a character that only user-defined operators have.
        if run.len() > 1 && !run.contains(['~', '!', '@', '#', '%', '^', '&', '|', '?']) {
            while run.len() > 1 && run.ends_with(['+', '-']) {
                run = run.get(..run.len() - 1).unwrap_or(run);
            }
        }
        self.pos = start + run.len();
        let tok = match run {
            "+" => Tok::Plus,
            "-" => Tok::Minus,
            "*" => Tok::Star,
            "/" => Tok::Slash,
            "%" => Tok::Percent,
            "=" => Tok::Eq,
            "<>" | "!=" => Tok::NotEq,
            "<" => Tok::Lt,
            ">" => Tok::Gt,
            "<=" => Tok::LtEq,
            ">=" => Tok::GtEq,
            "||" => Tok::Concat,
            "->" => Tok::Arrow,
            "->>" => Tok::ArrowText,
            "@>" => Tok::Contains,
            other => {
                return Err(ParseError::unsupported(
                    format!("operator `{other}`"),
                    self.span_from(start),
                ));
            }
        };
        Ok(tok)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn toks(input: &str) -> Vec<Tok> {
        lex(input).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn reserved_list_is_sorted_for_binary_search() {
        assert!(RESERVED.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn words_fold_and_quoted_names_keep_case() {
        assert_eq!(
            toks(r#"Foo "Foo" "a""b""#),
            vec![
                Tok::Word {
                    text: "foo".into(),
                    quoted: false
                },
                Tok::Word {
                    text: "Foo".into(),
                    quoted: true
                },
                Tok::Word {
                    text: "a\"b".into(),
                    quoted: true
                },
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn trailing_sign_is_split_from_an_operator() {
        assert_eq!(
            toks("a=-1"),
            vec![
                Tok::Word {
                    text: "a".into(),
                    quoted: false
                },
                Tok::Eq,
                Tok::Minus,
                Tok::Number("1".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn numbers_and_comments() {
        assert_eq!(
            toks("1.5e-3 .5 /* a /* nested */ b */ 7 -- tail"),
            vec![
                Tok::Number("1.5e-3".into()),
                Tok::Number(".5".into()),
                Tok::Number("7".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn refused_spellings_are_named() {
        assert_eq!(
            lex("E'x'").unwrap_err().kind(),
            crate::ErrorKind::Unsupported
        );
        assert_eq!(lex("$$x$$").unwrap_err().message(), "dollar-quoted strings");
        assert_eq!(lex("a ~ b").unwrap_err().message(), "operator `~`");
        assert_eq!(lex("a[1]").unwrap_err().message(), "arrays");
        assert_eq!(lex("1abc").unwrap_err().kind(), crate::ErrorKind::Syntax);
        assert_eq!(lex("'open").unwrap_err().span().start, 0);
        assert_eq!(lex("$0").unwrap_err().kind(), crate::ErrorKind::Syntax);
    }
}
