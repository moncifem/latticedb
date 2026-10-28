# lattice-bench

Layer dev. Benchmarks for the hot paths (record encoding, group commit, recovery scan, clock, schema
parsing, CRDT merge), stored baselines per hardware profile, and the regression gate.

Run `cargo xtask bench` to measure and compare against the baseline of the current profile, and
`cargo xtask bench --write-baseline` to record a new one. Wall-clock measurement is the whole point of
this crate, so it is on the allowlist in `tools/allow-banned.toml`.

Must not: be a normal dependency of any non-dev crate.


The optional [Linux runtime spike](RUNTIME_SPIKE.md) compares portable, Tokio and io_uring adapters
through the unchanged kernel. It has a separate manual CI job and does not select a production runtime.
Results and their limits are recorded in [ADR 0011](../../docs/adr/0011-engine-runtime.md).
