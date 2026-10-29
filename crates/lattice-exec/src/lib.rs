//! Executor for bound single-table SQL plans. Work in progress.
#![forbid(unsafe_code)]

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 5;
