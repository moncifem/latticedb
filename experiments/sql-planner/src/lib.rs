//! Isolated ADR0013 experiment; none of these interfaces are production SQL contracts.
pub mod ast;
pub mod custom;
pub mod execute;
#[cfg(feature = "datafusion")]
pub mod fusion;
#[cfg(feature = "parser")]
pub mod parsed;
pub mod storage;

use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub state: String,
    pub code: String,
    pub detail: String,
}
impl Error {
    pub fn new(state: &str, code: &str, detail: impl Into<String>) -> Self {
        Self {
            state: state.into(),
            code: code.into(),
            detail: detail.into(),
        }
    }
    pub fn syntax(detail: impl Into<String>) -> Self {
        Self::new("42601", "LSQL-0001", detail)
    }
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::new("0A000", "LSQL-0002", detail)
    }
    pub fn storage(detail: impl Into<String>) -> Self {
        Self::new("XX000", "SPIKE-STORAGE", detail)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.state, self.code, self.detail)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

/// The intentionally small statement/error record subset of the negative sqllogictest fixture.
pub fn negatives() -> Vec<(String, String, String)> {
    include_str!("../fixtures/negative.slt")
        .split("\n\n")
        .filter_map(|block| {
            let mut lines = block
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.starts_with('#'));
            let header = lines.next()?;
            let words = header.split_whitespace().collect::<Vec<_>>();
            assert_eq!(words.len(), 4);
            assert_eq!(&words[..2], &["statement", "error"]);
            Some((
                lines.collect::<Vec<_>>().join("\n"),
                words[2].to_string(),
                words[3].to_string(),
            ))
        })
        .collect()
}
