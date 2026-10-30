//! Executor errors: a SQLSTATE, a message and a coarse kind callers can match on.

use std::fmt;

use lattice_engines::limits::LimitError;
use lattice_engines::store::RowError;
use lattice_sql::binder::BindError;
use lattice_sql::error::ParseError;

use crate::decimal::NumError;

/// What went wrong, coarsely. The SQLSTATE carries the detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The text did not parse.
    Syntax,
    /// The statement did not bind (unknown names, type errors, unsupported syntax).
    Bind,
    /// A plan was bound against an older catalog than the one it runs on.
    StalePlan,
    /// A `NOT NULL`, primary key or `UNIQUE` constraint.
    Constraint,
    /// A value error while evaluating: overflow, division by zero, bad text.
    Evaluation,
    /// First-committer-wins conflict; the transaction can be retried.
    Conflict,
    /// The row policy refused a read or write.
    Permission,
    /// Cancelled, timed out or over the memory budget.
    Limit,
    /// The transaction is in the failed state until `ROLLBACK`.
    TransactionAborted,
    /// A feature outside what the executor runs.
    Unsupported,
    /// The snapshot (`AS OF`, or the transaction's own) is older than the retained history; retry on a
    /// newer snapshot.
    SnapshotTooOld,
    /// The storage engine failed.
    Storage,
}

/// A failed statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecError {
    kind: ErrorKind,
    sqlstate: &'static str,
    message: String,
}

impl ExecError {
    pub(crate) fn new(kind: ErrorKind, sqlstate: &'static str, message: impl Into<String>) -> Self {
        ExecError {
            kind,
            sqlstate,
            message: message.into(),
        }
    }

    pub(crate) fn unsupported(what: impl Into<String>) -> Self {
        ExecError::new(ErrorKind::Unsupported, "0A000", what)
    }

    pub(crate) fn eval(sqlstate: &'static str, message: impl Into<String>) -> Self {
        ExecError::new(ErrorKind::Evaluation, sqlstate, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        ExecError::new(ErrorKind::Storage, "XX000", message)
    }

    /// Stored data failed to decode. The detail stays out of the message a client sees.
    pub(crate) fn corrupt() -> Self {
        ExecError::new(ErrorKind::Storage, "XX001", "stored data is corrupt")
    }

    pub(crate) fn bind_type(message: impl Into<String>) -> Self {
        ExecError::new(ErrorKind::Bind, "42804", message)
    }

    /// The coarse kind.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The five character SQLSTATE.
    pub fn sqlstate(&self) -> &'static str {
        self.sqlstate
    }

    /// The description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.sqlstate)
    }
}

impl std::error::Error for ExecError {}

impl From<ParseError> for ExecError {
    fn from(e: ParseError) -> Self {
        let kind = if e.sqlstate() == "0A000" {
            ErrorKind::Unsupported
        } else {
            ErrorKind::Syntax
        };
        ExecError::new(kind, e.sqlstate(), e.to_string())
    }
}

impl From<BindError> for ExecError {
    fn from(e: BindError) -> Self {
        let kind = match e.sqlstate() {
            "0A000" => ErrorKind::Unsupported,
            "23502" => ErrorKind::Constraint,
            "22003" | "22012" | "22P02" => ErrorKind::Evaluation,
            _ => ErrorKind::Bind,
        };
        ExecError::new(kind, e.sqlstate(), e.to_string())
    }
}

impl From<NumError> for ExecError {
    fn from(e: NumError) -> Self {
        match e {
            NumError::Overflow => ExecError::eval("22003", "numeric value out of range"),
            NumError::DivisionByZero => ExecError::eval("22012", "division by zero"),
            NumError::Syntax => ExecError::eval("22P02", "invalid input syntax for type numeric"),
        }
    }
}

impl From<LimitError> for ExecError {
    fn from(e: LimitError) -> Self {
        match e {
            LimitError::PermissionDenied => {
                ExecError::new(ErrorKind::Permission, "42501", "row access denied")
            }
            LimitError::Cancelled => ExecError::new(
                ErrorKind::Limit,
                "57014",
                "canceling statement due to user request",
            ),
            LimitError::DeadlineExceeded => ExecError::new(
                ErrorKind::Limit,
                "57014",
                "canceling statement due to statement timeout",
            ),
            LimitError::DeadlineOverflow => {
                ExecError::new(ErrorKind::Limit, "22023", "statement timeout out of range")
            }
            LimitError::MemoryExceeded { .. } => {
                ExecError::new(ErrorKind::Limit, "53200", e.to_string())
            }
        }
    }
}

impl From<RowError> for ExecError {
    fn from(e: RowError) -> Self {
        match e {
            RowError::Conflict { .. } => ExecError::new(
                ErrorKind::Conflict,
                "40001",
                "could not serialize access due to concurrent update",
            ),
            RowError::SnapshotTooOld { .. } => ExecError::new(
                ErrorKind::SnapshotTooOld,
                "72000",
                "snapshot too old: the history it needs was vacuumed; retry on a newer snapshot",
            ),
            RowError::PermissionDenied => {
                ExecError::new(ErrorKind::Permission, "42501", "row write denied")
            }
            RowError::InDoubt(_) => {
                ExecError::new(ErrorKind::Storage, "40003", "the commit outcome is unknown")
            }
            // Engine detail (paths, offsets, record contents) is not for clients.
            RowError::Corrupt(_) => ExecError::corrupt(),
            _ => ExecError::new(ErrorKind::Storage, "XX000", "internal storage error"),
        }
    }
}
