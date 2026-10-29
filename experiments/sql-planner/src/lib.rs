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

/// Version the experiment transport separately from the internal AST/trace serde representation.
pub const RESULT_CODEC: &str = "json-scalars-v1";
pub fn scalar_rows(rows: &[Vec<ast::Value>]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|row| {
                serde_json::Value::Array(
                    row.iter()
                        .map(|value| match value {
                            ast::Value::Null => serde_json::Value::Null,
                            ast::Value::Int(n) => serde_json::Value::from(*n),
                            ast::Value::Text(text) => serde_json::Value::from(text.clone()),
                            ast::Value::Bool(b) => serde_json::Value::from(*b),
                        })
                        .collect(),
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    #[test]
    fn scalar_codec_preserves_null_boolean_signed_integer_and_escaped_text() {
        let rows = vec![
            vec![
                ast::Value::Null,
                ast::Value::Bool(true),
                ast::Value::Int(i64::MIN),
                ast::Value::Int(i64::MAX),
                ast::Value::Text("quote\"\nλ".into()),
            ],
            vec![],
        ];
        let encoded = serde_json::to_string(&scalar_rows(&rows)).unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            decoded,
            serde_json::json!([
                [
                    null,
                    true,
                    -9223372036854775808i64,
                    9223372036854775807i64,
                    "quote\"\nλ"
                ],
                []
            ])
        );
        assert_eq!(decoded[0][1].as_bool(), Some(true));
        assert_eq!(decoded[0][2].as_i64(), Some(i64::MIN));
    }
}
