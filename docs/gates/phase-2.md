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
SQL grants, executor deadline/memory enforcement or completion of P2-17/P2-18. The limit
primitives are now wired into `RowStore::scan_at_limited`/`lookup_at_limited` (polled per row,
charged per returned row); the plain `scan_at`/`lookup_at` and the index candidate list remain
unbounded, and no executor uses them yet.

Principal propagation (#357, [ADR 0026](../adr/0026-principal-propagation.md), proposed): the
authenticated SCRAM account, or the fixed `local-development` principal in loopback trust mode, now
reaches every `QueryHandler` call in a `QueryContext`. Integration tests through the TCP, TLS and SCRAM
listener show the handler sees the account that proved itself and nothing before the proof. The row
policy is still not selected from that principal and `RowStore::begin()` still binds allow-all, so #357
stays open until the executor wires a per-principal evaluator and an unconfigured handler fails closed.

## Observed row nightly and native smoke (2026-10-29)

[Manual nightly 40881355427](https://github.com/moncifem/latticedb/actions/runs/40881355427)
completed at source `743b0fb52a495a3f875490268d604ed543be9e64` with 57 successful jobs and
one skipped job. Its 20 exact named row-fault shards covered four disjoint 2,500-seed intervals
per profile, at 150 configured steps: five profiles × 10,000 = 50,000 observed runs.
47,679 were verified; 2,321 in the bit-rot `everything` profile were incomplete and are not claimed
as complete-history verification. The 146 cut-short terminal histories are included in the
verified total. See the [profile table and reproducible provenance audit](../evidence/row-nightly-40881355427.md)
and [compact source/artifact digest record](../evidence/row-nightly-40881355427.json).

A separate native macOS ARM64 nightly archive at the same source passed the recorded startup,
`SELECT 1`, unsupported-SQL `0A000`, and stdin shutdown smoke. Its archive checksum and GitHub
build attestation source were verified. Linux archives were not executed in this smoke.

This evidence predates PRs [204](https://github.com/moncifem/latticedb/pull/204) and
[215](https://github.com/moncifem/latticedb/pull/215). It establishes neither coverage of those
changes nor seven consecutive scheduled nights, million-seed nightly volume, a completed Phase 2
soak requirement, or either phase's gate sign-off.


## TLS and configured SCRAM integration (2026-10-29)

PR [223](https://github.com/moncifem/latticedb/pull/223) implements the host integration proposed in
accepted RFC 0007 / ADR 0021. It requires TLS before configured SCRAM startup, bounds verifier/account input,
uses one absolute startup/TLS/authentication deadline and emits no successful startup or cancel key
before a known account proves possession. Trust remains an explicit loopback development mode.
The server requires an externally provisioned persistent 32-byte decoy secret: an independent review
found that regenerating it on load exposed account existence through salt stability across restart.
The fix and independent HMAC/two-instance regression resolve that finding for an unchanged secret;
secret rotation remains observable and is documented. There is no constant-time account-lookup claim.

Verified local logs report 97 focused tests (74 API, three host, nine TLS/SCRAM/CLI and eleven existing socket
cases) and strict touched-crate Clippy. Independent security review and a narrow fix rereview found
no remaining blocker. On 2026-10-29 the owner explicitly accepted RFC 0007 / ADR 0021 and waived
this integration's seven-day wait, separately from the earlier primitives waiver. The WAL-format and
PR 209 holds remain unchanged. External-driver conformance,
binary result formats, storage-backed SQL execution and authenticated row-policy actor binding remain
open; this integration does not complete P2-11/P2-12 or sign either phase gate.


## Guarded WAL admission evidence (2026-10-29)

PR [222](https://github.com/moncifem/latticedb/pull/222) implements accepted RFC 0008 / ADR 0022:
explicit engine capabilities, complete header-chain preflight, durable guard installation before
protected append, cumulative requirements through rollover and actual segment context on replay.
Default/raw readers advertise zero support; the separate managed-index PR 209 must explicitly
integrate the guard before activation. Legacy v1 record/segment bytes remain unchanged. Required
bounded `Io::read_prefix` is a deliberate source API break for these unpublished seam adapters.

[Hosted run 40908208450](https://github.com/moncifem/latticedb/actions/runs/40908208450) succeeded for
PR head `f9f0307b42766a01261670bc51a23b1dd88b5581`, actually built/executed at merge source
`fb11b8f657643e250adb2da0d78f4f14ded555cd`. The frozen `743b0fb` reader checked 4,648 bounded fault
histories as 9,296 raw/recovered snapshots plus two static controls. Guarded and already-recovered
snapshots were unchanged; 267 raw unguarded torn tails were permissibly repaired. This is finite
compatibility evidence, not arbitrary-workload or large-soak coverage.

The separate release-build measurement retained 90 samples and 18 warmups across 1/32/256 segments
and 64/65,536-byte values. At 256 tiny legacy segments, median open increased by 1.937 ms / 69.1%;
at 256 larger segments, by 2.190 ms / 4.75%. These are warmed-cache Log admission/open observations,
including filesystem metadata, locks and synchronization, not pure CPU decoding. Every new-reader
prefix request/return/Vec-capacity maximum was 32 bytes, with one full-file read per segment. Median
process RSS at 256 larger segments was 2,352 KiB frozen versus 2,488 KiB new. Buffer counters and
process RSS do not establish a whole-memory bound or a checkpoint/RowStore rebuild result. See the
[method, full tables and limits](../evidence/wal-admission-40908208450.md) and
[compact observations/provenance](../evidence/wal-admission-40908208450.json).

Later rebases require their own fresh checks; these historical results do not relabel the tested
source. On 2026-10-29 the owner explicitly accepted RFC 0008 / ADR 0022 and waived this guard's
seven-full-day wait after the measured-cost disclosure, conditional on final-head green CI before
merge. The [narrow decision record](../evidence/wal-admission-owner-decision-2026-10-29.md) preserves
that scope. This does not waive PR 209's separate hold, accept its managed-index format, activate
LCIX on main, accept a checkpoint format, assign a release, establish a performance threshold,
or sign either phase gate.


## Persisted managed-index recovery (2026-10-29)

PR [209](https://github.com/moncifem/latticedb/pull/209) persists physical-byte index definitions and
lifecycle in atomic LCIX row/catalog changes. Default managed opens explicitly declare capability
0x01 and rebuild definitions/entries from streamed history without callbacks. Nonempty callback opens
remain legacy-only and advertise zero support. Compound writers request the guard; replay refuses
unguarded LCIX. Bound actor policy, floor, conflict and metadata checks precede ID/RNG/WAL. Historical
lookups preserve dropped/recreated definition timelines; scrub compares both authoritative definitions
and entries. Creation is synchronous and may block; function pointers are never serialized.

Independent review found no new blocking code defect. [Run 40916386232](https://github.com/moncifem/latticedb/actions/runs/40916386232)
passed 25 semantic tests and a separately selected hosted export at reviewed head `d0edc12`, actual
merge source `aa134abf47a15cd433375c0025514c13bd83db2e` with identical tree. Its 15 finite engine
histories yielded 30 frozen old-reader probes: 20 byte-unchanged guarded refusals, ten legacy probes,
and two allowed raw legacy repairs. It also retained two static controls. The
[reviewed scope and provenance](../evidence/managed-index-40916386232.md) and
[compact verified archive/probe record](../evidence/managed-index-40916386232.json) distinguish this
from the separate 4,648-history core guard proof and Log-only performance measurements. Historical
runs do not relabel later heads; final current-head required checks and hosted proof remain required.

On 2026-10-29 the owner explicitly accepted RFC 0005 / ADR 0019 and waived this format's own
seven-full-day wait, separately from RFC 0008 / ADR 0022. The
[narrow decision record](../evidence/managed-index-owner-decision-2026-10-29.md) makes merge conditional
on exact final-head green CI. Synchronous build performance, retained-state memory and row-decoder
admission/allocation budgets remain unresolved. This supplies no mixed-version access, in-place
downgrade, online backfill, accepted checkpoint composition, release or phase gate sign-off.


## Guard and managed-index review follow-ups (issues 234 and 307)

PRs 222 and 209 were already merged on main when this follow-up started; the drafts were not stale.
Main already wrote LCIX only through `commit_with_capabilities(MANAGED_COMPOUND)`, opened managed mode
with support for bit 0 and callback mode with zero, and had tests for an unguarded LCIX refusal
(`valid_literal_compound_without_a_segment_guard_is_refused_without_repair`), zero-support refusal
(`legacy_callback_and_zero_support_readers_refuse_guarded_history_before_replay`) and an omitted
guard (`an_omitted_guard_on_actual_compound_commit_is_caught_by_engine_recovery`). Added here:

- Issue 307: the empty-tail guard rewrite stages the new header in a fresh `.guard` file before the
  truncate. The crash-point sweep (SimIo power cuts at every mutating operation of the rewrite, a
  disk that persists a truncate at once, clean, crash-heavy and garbage-tail profiles, four seeds)
  passes, and with `Bug::GuardRewriteUnstaged` (the old in-place rewrite) it loses the durable bit.
  Recovery also repairs an invalid (zero-filled or garbage) newest header from a valid stage, refuses
  when no valid stage exists, when the stage does not decode, or when the invalid header sits over
  records; only files named 20 digits plus `.guard` are cleaned up. This amends RFC 0008's promotion
  step; the amendment awaits the owner. The hosted guarded-upgrade matrix was not re-run locally.
- Issue 234: recovery validates each catalog record against the recovered catalog by borrow and
  applies it once; a test counts `Catalog::clone` calls during replay (zero; the old code made one per
  record). That is all that is measured: live tables and indexes are still cloned per change,
  `check_change` runs twice per record, and per-version snapshots are still stored, so no linear
  replay cost is claimed.
- DDL through `commit_with_catalog` now needs `PolicyEvaluator::authorize_ddl`, denied by default and
  checked before any other error (`AllowAll` grants it, so `begin()` transactions can run DDL;
  `commit_catalog`, `CatalogStore::commit` and `commit_managed` are host-only paths with no actor).
  Witnessed with `Bug::DdlPolicySkipped`. Index builds still read every row; the DDL grant is the
  control.
- A test replays a hand-framed pre-guard log with LCAT records through the managed, catalog-only and
  callback readers and checks the bytes are unchanged; another documents that a rule-breaking LCAT
  record still refuses recovery.

The bit-0-only format cannot yet exercise a rewrite from A to A|B in production; the sweep widens the
known-capability set per thread in tests only. Local verification covered focused tests and Clippy
for `lattice-core` and `lattice-engines`; the workspace gate, mutation, Miri and the ignored
hosted matrices are left to CI. The registry now has 41 bits.

## Strict HLC history resolver (P2-14 foundation)

`RowStore::try_snapshot_as_of_hlc` adds a checked full-HLC predecessor lookup: expired requests fail
with `SnapshotTooOld`; `snapshot_as_of_hlc` is now the same strict lookup and `snapshot_as_of_hlc_clamped` is the explicit
opt-in clamp. The focused
`strict_hlc` tests cover literal wall/counter/node boundaries, gaps created by non-row WAL records,
empty/pre-first/future snapshots, retention floors, recovery and reapplied vacuum. A separate
`Bug::HLC_SNAPSHOT` witness exercises the clamping defect without changing `Bug::ALL` or
`Bug::STORAGE`. This adds no SQL timestamp parser, SQLSTATE mapper, branch support or phase-gate claim.

Local focused verification passed five strict-HLC tests (including the planted defect witness), four
existing vacuum regressions and the 38-bit registry uniqueness check. Touched engine/core Clippy passed.
This is bounded API evidence, not full SQL conformance or soak-volume evidence.

## SQL binder and planner decision (P2-09, P2-10)

`lattice-sql` now binds single-table `SELECT` (with `WHERE`, `ORDER BY`, `LIMIT`, `OFFSET`, `AS OF`),
`INSERT`, `UPDATE` and `DELETE` with `RETURNING` into a typed logical plan, against an abstract
catalog view. Joins, `GROUP BY`, aggregates, `DISTINCT`, subqueries and DDL are refused with `0A000`.
Nothing executes SQL against storage yet, and the listener handler is still `SELECT 1`.

The binder tests (table of goldens, error-code table, the five sqllogictest scripts as bind inputs, a
property test that binding never panics and leaves no untyped expression) were run locally for
`lattice-sql` only, at default proptest case counts. Mutation, Miri and larger case counts were left to CI.

[ADR 0025](../adr/0025-sql-planner-in-house.md) is **accepted** (2026-10-29, tech lead under the owner's
delegation): the planner is built in house, with a scope freeze (no window functions, correlated
subqueries or recursive CTEs), the PostgreSQL differential harness as the semantics authority, and a
numeric reopen trigger. ADR 0013 stays proposed because its effort-days (C1) and Phase 1 SQL
baseline (C4) are unresolved. **Open before the Phase 2 gate review:** binary size, memory bounds and
effort-days are not measured and must be recorded on this page. P2-10 is not complete and the subset
document stays draft.

## SQL executor for the single-table subset (P2-10, P2-13, P2-17, P2-18)

PR [336](https://github.com/moncifem/latticedb/pull/336) adds `lattice-exec`
([README](../../crates/lattice-exec/README.md)). **What runs.** Bound `SELECT` (`WHERE`, `ORDER BY`,
`LIMIT`, `OFFSET`, `AS OF`), `INSERT` (values and `SELECT`), `UPDATE` and `DELETE` with `RETURNING`
execute against snapshot-isolated row-store transactions: `BEGIN`, `COMMIT`, `ROLLBACK` at session
level, first-committer-wins as `40001`, `NOT NULL` and key violations as `23502` and `23505`, integer
overflow `22003`, division by zero `22012`. Plans bound at an older catalog version are refused. The
actor's row policy applies to scans and commits; the statement deadline, memory budget and cancellation
are polled for every row. `CREATE TABLE` and `DROP TABLE` register tables, with the engine catalog
persisting ids and names only. A `QueryHandler` adapter exists in `lattice-api` and is **not wired**
into `latticed`, which still answers `SELECT 1`.

**What does not run.** Joins, `GROUP BY`, aggregates, `DISTINCT`, subqueries, `ALTER TABLE`, indexes
(`CREATE INDEX`, and no scan uses an index), the `jsonb` operators, and DDL inside a transaction block.
Column definitions are in memory and are not recovered after a restart (the host re-attaches them), and
the stored row layout is not a frozen format.

**sqllogictest.** The five scripts in `crates/lattice-testkit/tests/slt/` run unchanged through the
executor. By record: `basics.test` 7 passed and 2 failed, `joins.test` 7 and 4, `aggregates.test` 9 and
5, `changes.test` 14 and 0, `transactions.test` 15 and 1; 52 passed, 12 failed. All 12 failures are
statements the binder refuses with `0A000` (aggregates, joins, `GROUP BY`, `DISTINCT`) and are listed
with reasons in `crates/lattice-exec/tests/slt_expected_failures.txt`; the test fails if a listed
record starts passing. This is **not** the 100 percent P2-13 asks for, and no differential run against
PostgreSQL was made for this executor.

**Evidence run locally for `lattice-exec` and the `lattice-api` adapter only, at default proptest case
counts:** unit, operator, session, model (random workloads against an in-memory oracle, with a
negative control), planted-engine-bug, no-panic and sqllogictest tests, `clippy -D warnings`, `cargo xtask
check-layers`. Not run locally: the workspace tests, Miri, sanitizers, mutation testing, larger proptest
case counts, benchmarks and the simulator workloads; those are left to CI. No Phase 2 exit criterion is
claimed, and P2-10, P2-13, P2-17 and P2-18 remain open.
