//! Plugin conformance kit: the contract checks and a reference plugin that passes them.
//!
//! `suite` has no dependency on `reference`: a third-party plugin implements `PluginUnderTest`
//! (feed one line, return the lines it produced) and calls `suite::run_all`. Copy this directory or
//! include it with `#[path]` from your own integration test.
#![allow(dead_code, unreachable_pub)]

pub mod reference;
pub mod suite;
