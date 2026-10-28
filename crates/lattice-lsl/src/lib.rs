//! Lattice Schema Language v0: parser, type checker, validator, compatibility checker, formatter.
//!
//! Layer L2. Must not: do I/O or depend on the kernel.
//!
//! ```text
//! enum Priority { low, medium, high }
//! facet Estimable { points: int in 0..=100 optional  priority: Priority }
//! type Epic { title: text max 120 }
//! type Story = Estimable { title: text max 200  epic: ref<Epic> optional  acceptance: list<text> max 20 }
//! ```
//!
//! The pipeline is `parse` (text to [`ast::Schema`]) then `check` (names, composition, refinements,
//! flattening facets into [`check::Compiled`]) then `validate` (a value against a type) and
//! `compat::compare` (what a schema change does to existing data).
#![forbid(unsafe_code)]

pub mod ast;
pub mod check;
pub mod compat;
pub mod format;
pub mod lens;
pub mod parser;
pub mod validate;

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 2;

/// A located problem in schema text or a schema.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{line}:{col}: {message}")]
pub struct Diagnostic {
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub col: u32,
    /// What is wrong.
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(line: u32, col: u32, message: impl Into<String>) -> Self {
        Self {
            line,
            col,
            message: message.into(),
        }
    }
}
