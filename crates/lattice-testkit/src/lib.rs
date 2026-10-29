//! Test support: oracles, generators, invariant checkers and the deterministic scenario runner.
//!
//! Layer dev. Must not: be a normal dependency of any non-dev crate.
#![forbid(unsafe_code)]

pub mod invariants;
pub mod scenario;
pub mod slt;

/// Returns the sum of `values`, the trivial example kept from Phase 0.
pub fn example_sum(values: &[u64]) -> u64 {
    values.iter().sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_sum_adds() {
        assert_eq!(example_sum(&[1, 2, 3]), 6);
    }
}
