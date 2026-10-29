use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Expr {
    Col(String),
    Lit(Value),
    Param(usize),
    Count,
    Binary(Box<Expr>, String, Box<Expr>),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Historical {
    ChangeSet(String),
    Timestamp(u64),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Select {
    pub columns: Vec<Expr>,
    pub table: String,
    pub join: Option<(String, Expr)>,
    pub historical: Option<Historical>,
    pub filter: Option<Expr>,
    pub group: Vec<Expr>,
    pub having: Option<Expr>,
    pub order: Vec<Expr>,
    pub limit: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Plan {
    Select(Select),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
        returning: Vec<Expr>,
    },
    Insert {
        table: String,
        columns: Vec<String>,
        values: Vec<Expr>,
        returning: Vec<Expr>,
    },
    Begin,
    Commit,
    Rollback,
}
impl Expr {
    pub fn count(&self) -> bool {
        match self {
            Self::Count => true,
            Self::Binary(a, _, b) => a.count() || b.count(),
            _ => false,
        }
    }
}
