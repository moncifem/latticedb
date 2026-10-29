//! Source spans and parse errors.

use std::fmt;

/// A byte range in the input text.
///
/// Two spans always compare equal, so an AST compares by content and not by where it was written.
/// That is what lets `parse(print(ast)) == ast` hold although printing moves everything around.
#[derive(Clone, Copy, Debug, Default)]
pub struct Span {
    /// First byte of the range.
    pub start: usize,
    /// One past the last byte of the range.
    pub end: usize,
}

impl PartialEq for Span {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Span {}

impl Span {
    /// A span covering `start..end`.
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    /// The smallest span covering both.
    pub fn join(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// The text this span covers, if it lies on character boundaries of `input`.
    pub fn slice(self, input: &str) -> Option<&str> {
        input.get(self.start..self.end)
    }
}

/// The class of a parse failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The text is not valid SQL (SQLSTATE `42601`, `LSQL-0001`).
    Syntax,
    /// Valid PostgreSQL that is outside the declared subset (SQLSTATE `0A000`, `LSQL-0002`).
    Unsupported,
    /// The input exceeds a size or nesting limit (SQLSTATE `54001`).
    TooLarge,
}

/// A parse failure with the place it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    kind: ErrorKind,
    message: String,
    span: Span,
}

impl ParseError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>, span: Span) -> Self {
        ParseError {
            kind,
            message: message.into(),
            span,
        }
    }

    pub(crate) fn syntax(message: impl Into<String>, span: Span) -> Self {
        Self::new(ErrorKind::Syntax, message, span)
    }

    pub(crate) fn unsupported(feature: impl Into<String>, span: Span) -> Self {
        Self::new(ErrorKind::Unsupported, feature, span)
    }

    pub(crate) fn too_large(message: impl Into<String>, span: Span) -> Self {
        Self::new(ErrorKind::TooLarge, message, span)
    }

    /// The class of failure.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The description. For [`ErrorKind::Unsupported`] this is the name of the unsupported feature.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Where in the input the failure was detected.
    pub fn span(&self) -> Span {
        self.span
    }

    /// The PostgreSQL error code to report.
    pub fn sqlstate(&self) -> &'static str {
        match self.kind {
            ErrorKind::Syntax => "42601",
            ErrorKind::Unsupported => "0A000",
            ErrorKind::TooLarge => "54001",
        }
    }

    /// The stable Lattice code, where `docs/sql-subset.md` defines one for this class.
    pub fn lattice_code(&self) -> Option<&'static str> {
        match self.kind {
            ErrorKind::Syntax => Some("LSQL-0001"),
            ErrorKind::Unsupported => Some("LSQL-0002"),
            ErrorKind::TooLarge => None,
        }
    }

    /// The one-based line and column (in characters) of the start of the error in `input`.
    pub fn line_col(&self, input: &str) -> (usize, usize) {
        let mut line = 1;
        let mut col = 1;
        for (at, c) in input.char_indices() {
            if at >= self.span.start {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ErrorKind::Syntax => write!(f, "LSQL-0001 syntax error: {}", self.message)?,
            ErrorKind::Unsupported => {
                write!(f, "LSQL-0002 unsupported feature: {}", self.message)?;
            }
            ErrorKind::TooLarge => write!(f, "limit exceeded: {}", self.message)?,
        }
        write!(f, " (at byte {})", self.span.start)
    }
}

impl std::error::Error for ParseError {}
