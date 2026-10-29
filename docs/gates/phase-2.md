# Phase 2 status (gate NOT passed)

Phase 2 is in progress under the owner-confirmed waiver in
[the Phase 1 review package](phase-1-review.md). The Phase 1 gate remains open; this page does not claim
a completed Phase 2 gate. Requirements and exit criteria are in
[the Phase 2 specification](../spec/02-storage-and-sql.md).

## Implemented slices

- MVCC row versions, immutable snapshots, first-committer-wins write conflicts and documented write skew.
- A persistent row store whose row changes travel through the kernel WAL and replay on recovery.
- A versioned catalog, a row-write codec and their seeded power-cut tests.
- Secondary index primitives maintained internally with row writes and rebuilt from row history.
  Exact-key lookup and consistency checks work at current and historical snapshots. Definitions are
  caller-supplied on each open, not catalog-managed yet (ADR 0014).
- A declared SQL subset. This is a target contract, not an implemented SQL interface.

## Verification for the index slice (P2-07)

The row simulation compares rows and index lookups against a map oracle at remembered snapshots over
120 seeds. It detects all four row planted bugs, including stale index entries. The existing 60-seed
power-cut workload opens the store with indexes, verifies acknowledged rows after terminal recovery,
and checks that every index agrees with the recovered rows. Focused tests cover zero-byte and empty
keys, update/delete history after reopen, wrong stored primary-key values and duplicate index ids.
These tests do not establish the Phase 2 soak-volume criteria. Additional bounded fault-profile
evidence is recorded below.

## Bounded row fault evidence (P2-05, P2-06, P2-07)

`cargo test -p lattice-engines --test row_faults -- --nocapture` runs 48 seeded histories of
72 workload steps under each existing disk profile. Its independent oracle matches recovered WAL
transactions to submitted write envelopes and acknowledged LSNs, permits either outcome for in-doubt
commits, refuses durable rejected writes, and compares current/historical rows and index lookups with
that submitted history. Raw commit metadata, unique ids and increasing timestamps are checked too.

| Profile | Runs | Terminal-verified histories | Rot-censored histories | Cut-short workloads |
|---|---:|---:|---:|---:|
| clean | 48 | 48 | 0 | 0 |
| crash-heavy | 48 | 48 | 0 | 0 |
| disk-faulty | 48 | 48 | 0 | 0 |
| hostile | 48 | 48 | 0 | 0 |
| everything | 48 | 42 | 6 | 0 |
| Total | 240 | 234 | 6 | 0 |

A history stops at its first destructive bit-rot event and is counted as incomplete, never as a pass.
Across the whole workload, including censored histories, the run observed 8,131 acknowledgements and
2,742 power cuts inside operation sequences. These counts describe exercised work; they must not be
read as 8,131 independently verified acknowledgements. Disk-faulty observed 40 injected append errors,
267 fsync errors and 924 ENOSPC outcomes; everything observed 37, 156 and 1,115 respectively. A partial
append counts only when a nonempty strict prefix actually becomes visible, rather than trusting the
profile name.

Separate tests replay identical seeds exactly, detect all four existing storage planted bugs, and sweep
every observed mutating commit/rotation and recovery boundary across 20 workloads. The sweep always
replays again after an interrupted recovery. The focused suite passed five tests, with one ignored
10,000-seed-per-profile volume test; that ignored budget has not been run for this evidence.

This is bounded correctness and fault-coverage evidence. It does not satisfy the million-seed,
seven-night soak requirements. Its four-mutant scope is separate from the ten-defect witness suite
below. Typed/catalog histories and broader soak evidence remain separate work.

## Ten storage defect witnesses (E2-4)

`cargo test -p lattice-engines --test storage_bugs` runs a clean control for every witness, then
requires each of ten distinct guarded storage defects to fail its intended semantic oracle. Setup
errors and panics fail the test rather than counting as detection. Guard cleanup is checked on unwind.
The 21 kernel defects are separate; the mask-capacity test verifies all 31 guard bits are distinct.

| Planted storage defect | Detection |
|---|---|
| Skip write conflicts | A stale writer must lose to the committed writer |
| Apply only part of a change set | The complete multi-row write envelope must be visible |
| Drop deletes during replay | A deleted row must remain absent after restart |
| Keep stale index entries | Old equality results and the scrubber must agree with current rows |
| Overwrite MVCC history | A retained snapshot must keep the old row value |
| Skip visible tombstones | A current deletion must not expose an older live value |
| Ignore transaction-local writes | Reads must see buffered updates, inserts and deletes |
| Include an excluded scan bound | Bounded scans must match an independent ordered map |
| Omit new index entries | Equality results and scrubbed entries must include every primary key |
| Match index prefixes as equality | Empty, zero-byte, absent and prefix-neighbor probes must match exact values |

All ten witnesses have passing clean controls and detect their respective defect. This completes the
ten-defect slice of E2-4; it does not establish soak coverage, branch fault histories, NS1 row-engine
acceptance, or the Phase 2 gate. The randomized row workloads retain their existing four-defect
contract; the new deterministic suite explicitly covers all ten through public storage operations.

## NS1 with real log and row processes (E2-7)

`cargo test -p lattice-bench --test row_process_kill -- --nocapture` runs two completed-burst
controls and eight seeded kill schedules against real files and child processes. Children publish
submission messages before commit and acknowledgement receipts only after commit returns and stdout
is flushed. The controller confirms each writer is still alive, kills it, drains complete protocol
messages, and independently matches recovered WAL batches to submitted envelopes and receipt LSNs.
It compares current and historical rows and exact index lookups with its folded model, requires clean
log/index scrubbers, then repeats recovery. Mixed batches include inserts, updates, deletes and a
unique transaction marker across multiple tables.

The observed controls submitted, acknowledged and recovered 32 batches. Across eight killed bursts,
70 batches were submitted, 62 acknowledged and 70 recovered. Every observed acknowledgement survived;
each recovered unacknowledged batch was complete. These counts are one bounded run, not fixed timing
expectations. Seeds fix receipt thresholds and jitter; OS interruption points remain nondeterministic.

This is NS1 evidence for the log and raw row/index engines. Process termination preserves the OS page
cache and is not power loss. The simulated fault suites above supply complementary power-cut evidence.
The tests do not guarantee that every kill landed inside a commit, prove soak volume, or cover the new
typed-schema/checkpoint formats still in review. Branch histories remain pending.

## Model-based row tests (P2-03, P2-04, P2-07)

`cargo test -p lattice-engines --test model_based` drives the row store and a small in-memory reference
model with the same generated operation sequences: up to four open transactions with writes, deletes,
reads, commits and aborts, clock moves (including backwards), remembered snapshots, `AS OF CHANGESET`,
`AS OF TIMESTAMP` (with counter and node tiebreaks), range scans with every bound kind, and clean
reopens. After every step it compares rows, scans, three secondary indexes, conflict LSNs and commit
metadata; at the end it re-reads every historical snapshot. Eight deliberately wrong model variants
(for example no conflict check, `<` instead of `<=` for timestamps, an excluded scan end read as
included) are each caught within 600 generated histories. In the deterministic 300-history coverage
run the generator produced 587 commits, 24 conflicts, 304 reopens, 354 `AS OF CHANGESET` and 243
non-empty `AS OF TIMESTAMP` lookups. It ran clean at 10,000 cases per property on one machine. The model
does not cover crashes or injected faults (see the fault suites above) and the bug-injection
(planted bug) suite is separate.

## Still missing

Catalog-backed index definitions; completed typed-table integration; checkpoint-based recovery with
memory/time measurements; vacuum; SQL parser, planner and executor; PostgreSQL wire and driver
conformance; branches and SQL time travel; online schema changes; and complete Phase 2 simulation,
benchmark and north-star evidence. The Phase 1 nightly/fuzz history and final gate review also remain
open. Track partial acceptance on [the roadmap](https://github.com/users/moncifem/projects/1).


## Merged development adapters and vacuum follow-ups (2026-10-29)

PRs [197](https://github.com/moncifem/latticedb/pull/197),
[199](https://github.com/moncifem/latticedb/pull/199),
[200](https://github.com/moncifem/latticedb/pull/200) and
[201](https://github.com/moncifem/latticedb/pull/201) merged after all six required checks passed.

- `latticed` drives the existing simple/extended sans-I/O session over bounded TCP workers, with
  complete-frame read deadlines, write timeouts and explicit shutdown. Six independent socket tests
  passed locally; portability testing caught and fixed macOS accepted-socket nonblocking inheritance.
  The handler only answers `SELECT 1`, uses plaintext/trust authentication and opens no database.
  Authentication, TLS, cancellation routing, query execution and driver conformance remain open.
- `lattice-api::cancel` has a bounded RNG-seam credential registry, expiry and one-way per-query
  tokens. Its fixed-capacity arithmetic-mask lookup is not a formal platform timing proof. The
  registry is not connected to the TCP listener or query executor yet.
- `lattice-engines::limits` provides per-row read/write policy hooks, explicit allow/deny policies,
  seam-clock deadlines, cancellation and live-byte reservation accounting. Unit/property tests
  validate these primitives. The subsequent row-store integration is recorded below; query
  abort/rollback and allocator RSS enforcement remain open. WP32 remains open.
- Vacuum now prunes row commit-time/id metadata below its floor while preserving kernel dedupe and
  all WAL bytes. Public row/index/table/transaction reads return `SnapshotTooOld` below the floor,
  including absent-key and transaction-local reads; expired read-only commits also fail. Timestamp
  resolution clamps to the floor, and threshold maintenance respects explicit retention/oldest-open
  inputs. Replay-then-vacuum equivalence and byte-unchanged WAL tests pass. Recovery still restores
  retained WAL history; benchmark acceptance and complete P2-19 evidence remain open.

These changes do not sign Phase 1 or establish a Phase 2 exit criterion. Earlier missing-work lists
above are historical snapshots: vacuum and the TCP development adapter are now implemented to the
scope described here; the SQL parser exists, while the planner/executor and wire-driver gate do not.


## Row-store policy integration (2026-10-29)

PR [215](https://github.com/moncifem/latticedb/pull/215) binds a host-supplied evaluator to each
row transaction and requires an explicit evaluator for snapshot reads. Point reads, scans, index
lookups and transaction-local reads filter denied rows. Commit checks every final old-to-new
transition after conflict/floor checks and before encoding, RNG use or WAL append; any denial
rejects the whole batch. Recovery and scrub retain all authoritative rows. `begin()` explicitly
selects the Phase 2 allow-all policy; a host requiring restrictions uses `begin_with_policy`.

Focused tests cover actor separation, generated actor/reopen models, whole-batch denial with
unchanged WAL/RNG witnesses and both planted policy bypasses. Independent review found no blocking
defect. Evaluator identity remains bound, but interior-mutable authorization state is controlled by
the trusted host and is not snapshotted. This is not authenticated PostgreSQL session isolation,
SQL grants, executor deadline/memory enforcement or completion of P2-17/P2-18.
