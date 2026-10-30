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

## Phase 3 benchmarks

`cargo run --release -p lattice-bench --example phase3_bench -- --profile my-machine --write-baseline`
records a local baseline (WP53, issue #255, first slice); repeat without `--write-baseline` to compare
with the existing comparator (25 percent tolerance by default). Options: `--scale` (default 8, sizes
derive from it), `--samples` (5), `--tolerance`. The seed is fixed. Human-readable lines go to stderr
and one JSON report (`metadata` plus `cases` with `ns_per_op`, `ops_per_s` and `ops`) goes to stdout.
The baseline is `benches/baselines/local/phase3-<profile>.json` in the usual name-to-ns format.
Long runs belong on CI or a controlled host; a tiny smoke test runs in `cargo test -p lattice-bench --example phase3_bench`.

Cases: tree CRDT apply in order and late (undo and redo), late ops before and after `Tree::compact`
(compaction at the middle of the log; both trees must agree) and `tree_compact` per dropped op; the
text CRDT (`text_crdt_apply` replays a fixed-seed edit log, 40 percent deletes, 1250 visible
characters per unit of scale so 10,000 at the default scale as in ADR 0010; `text_crdt_compact` and
`text_crdt_compact_retained` with three revert points, per stored element); sequence CRDT insert
(`Rga`); set merge; register merge (`lww_register_merge` replaces the value on every merge,
`lww_register_merge_noop` is the no-change path); change-feed cursor and event encode and decode; sync
frame encode and decode (a 32-event frame and an ack); fan-out of one event through the real
`lattice_api::backpressure::Hub` to 1, 100 and 10,000 subscribers where every tenth subscriber never
drains (the run asserts every publish reached every subscriber and only those subscribers overflowed);
and `reconnect_storm_client`, one whole reconnect (hello, subscribe with a cursor, catch up with acks)
against `SubscriptionHost` and an in-memory 512-event log, with clients at the head, at the beginning
and at random cursors.

Metadata schema 2 records the CPU model (`sysctl` on macOS, `/proc/cpuinfo` on Linux, `unknown`
otherwise); it is reported but not compared, so a baseline from another CPU model still compares. Schema
2 changed two existing cases, so a schema 1 baseline is refused and must be re-recorded:
`fanout_offer_*` now measures `Hub::publish_shared` instead of a local queue, and `lww_register_merge`
now measures merges that change the register. Not covered: real sockets, TLS or threads, a reconnect
storm with stalled clients, persisted state. The manual `bench-baseline` workflow runs this program and
uploads its baseline as an artifact; no baseline is recorded and no scheduled comparison exists.
