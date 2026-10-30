//! The identity of one connection, fixed when authentication completes.
//!
//! A [`Principal`] is created only by this crate: from an account found in the credential store
//! ([`crate::auth::Accounts`]) after a verified SCRAM proof, or as the fixed development principal of
//! loopback trust mode. The client-supplied startup `user` string is never the identity; it is only
//! compared against the account the exchange was selected for. See ADR 0026.
use std::fmt;

/// How a principal was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalKind {
    /// An account of the credential store that completed a SCRAM exchange.
    Account,
    /// The fixed principal of loopback trust mode. It stands for "a local developer", not a user.
    LocalDevelopment,
}

/// Name of the fixed development principal.
pub const LOCAL_DEVELOPMENT_NAME: &str = "local-development";

/// The authenticated identity of a connection. Immutable once created.
#[derive(Clone, PartialEq, Eq)]
pub struct Principal {
    kind: PrincipalKind,
    name: String,
}

impl Principal {
    /// The fixed principal of loopback trust mode. Use it only where the host has explicitly
    /// configured development trust; it carries no proof of who is connected.
    pub fn local_development() -> Self {
        Self {
            kind: PrincipalKind::LocalDevelopment,
            name: LOCAL_DEVELOPMENT_NAME.to_string(),
        }
    }

    /// An account principal. Crate-private: only the credential store may name one, and it passes
    /// its own key, not a client string.
    pub(crate) fn account(name: &str) -> Self {
        Self {
            kind: PrincipalKind::Account,
            name: name.to_string(),
        }
    }

    /// How this principal was established.
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }

    /// The account name from the credential store, or [`LOCAL_DEVELOPMENT_NAME`].
    pub fn name(&self) -> &str {
        &self.name
    }
}

// The same text is used in logs and error messages; it holds no secret.
impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            PrincipalKind::Account => write!(f, "account:{}", self.name),
            PrincipalKind::LocalDevelopment => f.write_str(LOCAL_DEVELOPMENT_NAME),
        }
    }
}

impl fmt::Debug for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Principal({self})")
    }
}

/// What a [`QueryHandler`](crate::session::QueryHandler) is told about the request besides the SQL.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct QueryContext<'a> {
    /// The authenticated identity of the connection running the statement.
    pub principal: &'a Principal,
}

impl<'a> QueryContext<'a> {
    /// A context for `principal`.
    pub fn new(principal: &'a Principal) -> Self {
        Self { principal }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_debug_name_the_kind_and_nothing_else() {
        let account = Principal::account("alice");
        assert_eq!(account.to_string(), "account:alice");
        assert_eq!(format!("{account:?}"), "Principal(account:alice)");
        assert_eq!(account.kind(), PrincipalKind::Account);
        let dev = Principal::local_development();
        assert_eq!(dev.to_string(), "local-development");
        assert_eq!(dev.kind(), PrincipalKind::LocalDevelopment);
        // An account that is named like the development principal is still an account.
        assert_ne!(Principal::account("local-development"), dev);
    }
}
