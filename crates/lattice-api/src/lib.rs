//! Protocol front doors: PG wire, S3, sync, OIDC endpoints, MCP, gRPC.
//!
//! Layer L6. Must not: Contain business rules.
#![forbid(unsafe_code)]

pub mod pgwire;
pub mod session;

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 6;

/// Bounded PostgreSQL cancellation registry and query tokens.
pub mod cancel;
