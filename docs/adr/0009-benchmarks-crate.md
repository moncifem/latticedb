# ADR 0009: Benchmarks live in their own crate and may read the wall clock

Status: accepted

## Context

Benchmarks have to measure real elapsed time, which the determinism lints forbid everywhere except a
short allowlist (`tools/allow-banned.toml`). Mixing timing code into `lattice-testkit` would put wall
clock reads next to code that must stay deterministic.

## Decision

Benchmarks live in `lattice-bench`, a dev-layer crate that is on the allowlist. It depends on the
crates it measures and is never a normal dependency of anything. Results are nanoseconds per operation
(median of several rounds). The gate compares against a baseline stored for a named hardware profile
and fails on a slowdown above the tolerance or on a benchmark that disappeared.

## Consequences

Wall-clock numbers vary between machines, so baselines are only committed for named reference
hardware; the default `local` profile is git-ignored. The commit benchmarks run on simulated storage and
measure kernel CPU work, not fsync latency.
