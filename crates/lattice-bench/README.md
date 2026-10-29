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

## Row-engine benchmarks

`cargo run --release -p lattice-bench --example row_bench -- --profile my-machine --write-baseline`
records a local row-engine baseline. Run it from inside the intended worktree; output paths are resolved from that worktree at runtime. Repeat with the same options, omitting `--write-baseline`, to
apply the existing regression comparator (25 percent tolerance by default). Options are `--rows`
(default 1024), `--reads` (10000 per read case), `--samples` (5) and `--tolerance` (0.25). Use a
profile name unique to the machine, and keep hardware/power settings stable. The runner checks the
workload, OS, architecture and debug-assertion metadata; it does not identify the physical CPU or authenticate the
machine name. A different workload or case set is refused rather than silently compared.

All baseline JSON, metadata and latest raw samples are written beneath the existing gitignored
`benches/baselines/local/` directory as `row-<profile>*.json`. No measured row baseline is committed,
and this manual runner does not extend the scheduled benchmark gate or imply a passed phase gate.

Cases report median nanoseconds per operation for point reads, range scans of up to sixteen keys,
index equality lookups, durable single-row commits, reopen at one/four/sixteen retained versions
per key, and vacuum per removed row version. Read results include their normal result allocation
and release costs; range scans also include cloning the two supplied key bounds. Commit timers
exclude staging each transaction but include the real WAL append/fsync and row/index application.
The index key rotates between sixteen buckets on every retained version, so vacuum exercises
historical index entries as well as rows. Vacuum timing includes index and commit-metadata pruning, and its denominator counts removed row
versions, including neither index versions nor already-retained visible versions.

The backend is `StdIo`, not simulated I/O. Every sample is prepared and fully checked outside its
measurement; each reopen is checked after its timer stops, with actual WAL bytes, segment counts,
retained row versions and acknowledged change sets in the raw report. The recovery series keeps
key cardinality and row width fixed while row history, WAL bytes and deduplication state grow.
Reopens occur in the same process with shared warm/uncontrolled filesystem cache. These are not
cold-cache/fresh-process measurements or recovery-memory/RSS bounds. Baseline recording alone is
not a passing regression comparison. No production database performance claim follows from this
microbenchmark.

For a tiny functional smoke, use `--rows 4 --reads 8 --samples 1 --profile smoke --write-baseline`.
The runner's unit tests use this bounded workload; long benchmark campaigns belong on a controlled
host rather than the development laptop.
