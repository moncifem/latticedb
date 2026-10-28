# lattice-testkit

Layer dev. Test support for the deterministic core. Must not be a normal dependency of any non-dev
crate.

- `invariants`: checkers for the log and kernel invariants (I1, I2, I4, I6, I9, I12 and others) over an
  oracle of what was acknowledged.
- `scenario`: the seeded crash-and-recover workload runner and the fault profiles, used by
  `cargo xtask sim` and the tests.
- `bin/sim_runner`: command line front end for the runner, with `--ledger` to record a run.

Tests in `tests/` cover planted-bug detection, the exhaustive power-cut sweep, the scrubber, kernel
lifecycle edge cases, format refusal and the north-star scenarios.
