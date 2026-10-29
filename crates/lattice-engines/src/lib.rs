//! Storage engines: log, row, blob, graph, vector, text, temporal.
//!
//! Layer L4. Must not: Bypass lattice-core transactions.
#![forbid(unsafe_code)]

#[cfg(feature = "row")]
pub mod catalog;
#[cfg(feature = "row")]
pub mod index;
#[cfg(feature = "row")]
pub mod mvcc;
#[cfg(feature = "row")]
pub mod rowops;
#[cfg(feature = "row")]
pub mod store;
#[cfg(feature = "row")]
pub mod typed;

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 4;

/// Row-store policy enforcement and cooperative statement-limit primitives.
pub mod limits;
