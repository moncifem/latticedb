//! Shared wire types and codecs of the sync protocol (`formats/sync-protocol-v1.md`).
//!
//! Layer L1. Must not: do I/O, read a clock, hold session state or depend on another workspace
//! crate. This crate exists so the server side (`lattice-api`) and the client side
//! (`lattice-sdk`) decode the same bytes with the same code: `layers.toml` forbids either of them
//! from depending on the other, and a copy of the types in each would drift.
//!
//! - [`sync`]: the frame types, limits, error codes, version negotiation and the strict, bounded
//!   frame codec.
//! - [`websocket`]: the RFC 6455 subset the transport needs (handshake, frame codec for both
//!   directions, and the sans-I/O connection state machine).
#![forbid(unsafe_code)]

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 1;

pub mod sync;
pub mod websocket;
