//! Benchmarks with stored baselines and a regression gate.
//!
//! Layer dev. Must not: be a normal dependency of any non-dev crate.
//!
//! Results are nanoseconds per operation (the median of several timed runs). The gate compares a
//! fresh run with a stored baseline from the same hardware profile and fails when any benchmark got
//! slower by more than the tolerance. The comparison is a pure function so it can be tested without
//! measuring anything, including the check that an injected 20 percent slowdown is caught.
#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod gate;
pub mod suite;

pub use gate::{Regression, compare, parse_results, render_results};
