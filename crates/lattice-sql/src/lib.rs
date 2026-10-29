//! Lexer, parser and AST for the SQL subset declared in `docs/sql-subset.md`.
//!
//! Layer L2. Must not: do I/O or execute queries. Binding to a logical plan is here; it reaches the
//! database only through the [`plan::CatalogView`] trait.
//!
//! ```
//! let stmt = lattice_sql::parse_statement("SELECT id FROM notes WHERE score >= 20").unwrap();
//! assert_eq!(stmt.to_string(), "SELECT id FROM notes WHERE (score >= 20)");
//!
//! let err = lattice_sql::parse_statement("SELECT * FROM a RIGHT JOIN b ON a.x = b.x").unwrap_err();
//! assert_eq!(err.sqlstate(), "0A000");
//! assert_eq!(err.message(), "RIGHT JOIN");
//! ```
//!
//! The parser is strict and bounded. Anything outside the subset fails with an error that names the
//! unsupported feature, input over [`MAX_INPUT_LEN`], [`MAX_TOKENS`] or [`MAX_DEPTH`] or [`MAX_CALL_DEPTH`] is refused, and
//! no input makes it panic. Every AST node prints back through `Display` to text that parses to an
//! equal AST (spans are ignored by `==`).
#![forbid(unsafe_code)]

pub mod ast;
mod display;
pub mod error;
mod lexer;
mod parser;

pub use ast::Statement;
pub use error::{ErrorKind, ParseError, Span};
pub use parser::{parse_statement, parse_statements};

/// Longest accepted input, in bytes.
pub const MAX_INPUT_LEN: usize = 1 << 20;
/// Most tokens accepted in one input.
pub const MAX_TOKENS: usize = 200_000;
/// Deepest tree of expressions and subqueries. Long left-leaning chains such as `a OR b OR ...`
/// count one level per term.
pub const MAX_DEPTH: usize = 256;
/// Deepest recursion of the parser. It is larger than [`MAX_DEPTH`] so that the printed form of any
/// accepted tree, which adds parentheses around every operator, parses back.
pub const MAX_CALL_DEPTH: usize = 2 * MAX_DEPTH + 8;
/// Longest identifier, in bytes (as in PostgreSQL).
pub const MAX_IDENT_LEN: usize = 63;

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 2;

pub mod binder;
pub mod plan;
