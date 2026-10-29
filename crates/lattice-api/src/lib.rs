//! Protocol front doors: PG wire, S3, sync, OIDC endpoints, MCP, gRPC.
//!
//! Layer L6. Must not: Contain business rules.
#![forbid(unsafe_code)]

pub mod pgwire;

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 6;
