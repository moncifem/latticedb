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

The bounded NS1 host integration test runs two clean controls and eight real subprocess kill/reopen
schedules with `cargo test -p lattice-bench --test row_process_kill -- --nocapture`. It uses real OS
processes, pipe readers and deadlines at this crate's approved host boundary, while the row engine
still uses its `StdIo`, `SystemClock` and `OsRng` seams. It verifies observed receipts, complete WAL
transaction envelopes, historical rows and indexes. Process termination preserves the OS page cache;
this complements simulated faults and does not test physical power loss or establish soak coverage.
