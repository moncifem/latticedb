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
  persisted in the catalog (ADR 0019, PR #209), not supplied by the caller on each open.
- The declared SQL subset (`docs/sql-subset.md`) for single-table statements: parser, binder, plan and executor (PRs #196, #318, #401), with durable table definitions across restart (RFC 0012; #573, #560). Joins, aggregates, GROUP BY, DISTINCT and subqueries are not implemented.

## Status on main, 2026-10-30

- Durable single-table SQL. `CREATE TABLE` writes its definition in the same change set under WAL capability bit 3 and LCIX v3 (RFC 0012, ADR 0028; commits 475acb62, 375716bf, 7ef7a76d). Rows, tables and column definitions come back after a restart (psql scripts `restart_a` and `restart_b`).
- RFC 0012 and ADR 0028 are accepted (Option A, 2026-10-30). The seven-day comment hold was first kept (commit 705d7cb9, 2026-10-30 15:51 UTC: "not waived"). The owner then waived it in chat on 2026-10-30. The waiver is recorded in `docs/gates/phase-1-review.md` (475acb62, 16:00 UTC), in RFC 0012 and in ADR 0028 (375716bf, 20:01 UTC). The repository does not record the time of the chat answer. Code that writes the format merged on 2026-10-30 (475acb62, 0aa405ce, 375716bf, 7ef7a76d). The sequence is tracked in #712.
- Executor: single-table `SELECT` with `ORDER BY`, `LIMIT` and `OFFSET`, `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `DROP TABLE`, transaction control, and index scans for primary key and `UNIQUE` predicates. Known differences from PostgreSQL are listed in `tests/drivers/psql/known` and `docs/sql-subset.md`.
- Server: PostgreSQL wire v3 with simple and extended query and text results; SCRAM-SHA-256 over TLS and loopback trust; cancellation; a SIGTERM and SIGINT stop (service mode, #584); the container image with its smoke test (#587); the `lattice account` credential tool (#582).
- Not on `main`: joins, `GROUP BY`, `DISTINCT`, subqueries and expressions over aggregates (#380, #593 to #597; ungrouped `count`, `sum`, `avg`, `min`, `max` run, #592); `ALTER TABLE` and online schema change (#609, #610); SQL `AS OF` and branches (#598 to #601); checkpoints (#602 to #605); binary formats (#608); the driver matrix (#413, #576, #607, #710); the `kill -9` proof with the real binary (#574).

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

## Audit follow-ups: limited index lookup and the row key bound (issues 443 and 444)

`RowStore::lookup_at_limited` now returns `RowError::NoSuchIndex` for an index that was never used, is not
yet created at the snapshot or was dropped, exactly as `lookup_at` does; before, it answered an empty list
that a caller would read as "no matching rows". `tests/unknown_index.rs` runs the same five cases through
both functions and fails against the old behavior.

The row store refuses a put whose key is over 4096 bytes (`RowError::KeyTooLong`, SQLSTATE `54000` through
`lattice-exec`), the same bound as the change feed, so one oversize key can no longer make
`events_since_as` fail for every reader. A log written before the rule may still hold one. A reader who
may not read that row is unaffected. A reader who may read it is delivered the events before it (the read
stops there, so a short result is not the end of the feed); a read that starts at the blocked event fails
with `FeedReadError::KeyTooLarge` naming its cursor, and reading again from that cursor skips exactly that
event. The test asserts the whole sequence a, c, d with no loss. Deletes are not bounded, so such a row
can be removed. Witnessed with `Bug::RowOversizeKeyAccepted` (`tests/key_limit.rs`). In SQL, `put_row`
checks the primary key and each unique entry key, so a long text or bytea value fails at its statement with
`54000`; the store check is the backstop. The format note is in `formats/rowops-v1.md` and ADR 0023; this
limit is format-adjacent and needs independent review.

Same class, not fixed here: `FeedError::ValueTooLarge` (a value over 16 MiB) still makes
`events_since_as` fail for every reader, including those who may not read the row. Tracked in issue 488.

## Still missing (2026-10-30)

Typed tables from LSL facets (#612, #114); checkpoint-based recovery and its format decision (#602 to #605); SQL joins, aggregates, `GROUP BY`, `DISTINCT` and subqueries (#380, #592 to #597); the driver matrix and binary formats (#413, #576, #607, #608, #710); branches and SQL time travel (#598 to #601); online schema change and `ALTER TABLE` (#609, #610); the vacuum policy (#611); the Phase 2 simulation, benchmark and north-star evidence (#574, #605, #614); and the Phase 1 nightly history and final gate review. The planner decision is ADR 0025. Partial acceptance is tracked on [the project board](https://github.com/users/moncifem/projects/1).


## Merged development adapters and vacuum follow-ups (2026-10-29)

PRs [197](https://github.com/moncifem/latticedb/pull/197),
[199](https://github.com/moncifem/latticedb/pull/199),
[200](https://github.com/moncifem/latticedb/pull/200) and
[201](https://github.com/moncifem/latticedb/pull/201) merged after all six required checks passed.

- `latticed` drives the existing simple/extended sans-I/O session over bounded TCP workers, with
  complete-frame read deadlines, write timeouts and explicit shutdown. Six independent socket tests
  passed locally; portability testing caught and fixed macOS accepted-socket nonblocking inheritance.
  When PRs 200 and 201 merged, the handler only answered `SELECT 1`, used plaintext/trust authentication
  and opened no database; authentication, TLS, cancellation routing, query execution and driver
  conformance were open then. Later sections record the TLS/SCRAM integration and the data-directory
  SQL listener (PR 412); driver conformance is still open (#413).
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
When this section was written nothing executed SQL against storage and the listener handler was still
`SELECT 1`; the executor and the data-directory listener are recorded in the two sections below.

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
persisting ids and names only. When this section was written, the `QueryHandler` adapter in `lattice-api`
was not wired into `latticed`; PR 412 wired it (next section).

**Known side channel and policy rules (interim, revisit with Phase 4).** A duplicate key or `UNIQUE`
value is `23505` even when a hidden row holds it, as in PostgreSQL row-level security; the message
carries only the constraint name, and a test pins that. `UNIQUE` entries live in derived physical tables
whose id has bit 31 set; evaluators must follow the owning table (`lattice_exec::owning_table`), and a
test covers a restrictive policy that does. `RETURNING` does not re-check the read policy. A scan is not
polled while the engine materialises row references. Staged writes are not charged to the statement
budget but are capped per transaction (#463): `ExecLimits::transaction_bytes`, 256 MiB by default,
counts two copies of every staged key and value plus 96 bytes of bookkeeping each (unit tests pin the
factor, the overhead, the delete charge and the rewrite replacement), across the statements of a
block, and the crossing write is `53200` and fails the block. It excludes the record encoded at
`COMMIT` and the engine's version copies, so peak memory at commit can exceed the cap. Other tests: a
block of single-row inserts stops after a few rows under an 8 KiB cap, 200 rewrites of one row stay
under 4 KiB, a 100-row autocommit insert under a 2 KiB cap fails, and a cap lowered mid-block applies to
what the block already staged. The 256 MiB default (`latticed --transaction-bytes`) is a chosen
ceiling, not a measured one. `SET timezone`, `client_encoding` and `datestyle` accept only UTC, UTF8 and
`ISO, MDY` and answer `22023` otherwise (#464). On the wire, a startup `TimeZone` or `DateStyle` is
ignored and the effective value reported in `ParameterStatus` (with `server_encoding`,
`integer_datetimes` and `standard_conforming_strings`); a startup `client_encoding` other than UTF8 is
a `FATAL 22023` before authentication. `SET TIME ZONE` to non-UTC after connect still fails (a documented
difference that affects, for example, Django with a non-UTC `TIME_ZONE`). No driver was run against this. Expression evaluation is bounded (#461, #462): `LIKE` polls the deadline and
cancellation every 1,024 steps (a test cancels a 40,000 x 20,000 pattern at the third poll, and a
session test times one out on the tick clock), and `||`/`lower`/`upper` refuse a result over 16 MiB
(`54000`) or over the remaining statement budget (`53200`) before building it (a 2^16-leaf balanced
tree over a 1 KiB value fails at the first oversized node). `lower`/`upper` are admitted at 3x their
input (tested with U+0390, which uppercases to 6 bytes from 2) and a bytea-to-text cast at 2x. A left
`||` operand of 4 KiB or more is held while the right subtree runs (a test shows `x || lower(y)`
failing `53200` where each step alone fits); smaller ones are not, so peak memory is bounded by the
budget only up to 4 KiB per nesting level. `LIKE` stays O(text x pattern); no
`repeat`/`lpad` functions exist.

**Ungrouped aggregates (issue 592, part).** `count`, `sum`, `avg`, `min` and `max` without `GROUP BY` run through a new `Aggregate` plan operator that yields one row. Evidence: 111 statements compared with PostgreSQL 16 in `crates/lattice-exec/tests/aggregates_pg.rs`, and 62 PostgreSQL-verified sqllogictest records that were listed as failing now pass. Planted bug `AggregateSkipsNull` (group `Bug::EXEC_AGGREGATE`, `NULL` arguments reach the fold) is caught by a witness in the same file. Not run locally: mutation testing. Not built: expressions over aggregates, `ORDER BY` an aggregate, `count(DISTINCT x)`, `GROUP BY` and `HAVING` (#593).

**What does not run.** Joins, `GROUP BY`, `DISTINCT`, subqueries, expressions over aggregates, `ALTER TABLE`, indexes
(`CREATE INDEX`, and no scan uses an index), the `jsonb` operators, and DDL inside a transaction block.
Column definitions are recovered after a restart from the definition `CREATE TABLE` stores with the table
(RFC 0012 / ADR 0028, issue 573; see "SQL definitions in the executor" below); `ALTER TABLE` and column names
with ASCII capitals are not supported, and the stored row layout is not a frozen format.

**sqllogictest.** All 21 scripts in `crates/lattice-testkit/tests/slt/` (669 records) run unchanged through
the executor: **548 pass and 121 fail** (read from a local run of the executor-side test, `cargo test -p
lattice-exec --test slt`; the CI run on the pull request confirms it; the PostgreSQL reference job is
separate and was not run). `SELECT` without `FROM` now runs (a one-row source in the plan), which moved
`arithmetic_errors.test` to 43 of 44 (every overflow `22003`, division by zero `22012` and cast `22P02` record
except one aggregate), `nulls_functions.test` to 20 of 26, `strings.test` to 35 of 42 and
`types_and_casts.test` to 12 of 38. `changes.test` (14) and `unsupported.test` (64) pass in full;
`transactions_visibility.test` 54 of 65, `constraints.test` 38 of 45 and `basics.test` 7 of 9, for example.
`types_and_casts.test` stays low only because its table is declared with `numeric(p, s)`, which the executor
refuses (26 records, 28 with `constraints.test`); a Lattice-side variant of that script or enforcing
`numeric(p, s)` is still open (#466).
The failures are listed one by one, with a reason, in `crates/lattice-exec/tests/slt_expected_failures.txt`.
88 are statements refused with `0A000` (aggregates 5, joins 25, `GROUP BY` and `HAVING` 16, subqueries 12,
`DISTINCT` 6, `ALTER TABLE`/`CREATE INDEX` 6, `numeric(p, s)` 2, and 16 whose expected error is not reached
because an earlier part of the statement is refused). The other 33 are not `0A000`: they depend on a
`CREATE TABLE` with `numeric(p, s)` (refused with `0A000`, so the table does not exist, 28 of them in
`types_and_casts.test` and `constraints.test`) or on `ALTER TABLE` or `CREATE UNIQUE INDEX` having run. Each of
those 33 now names its expected SQLSTATE in the list (`not-0A000(42P01):` and so on) and the test asserts
it, so a cascade that turns into a different error or a wrong result fails. The test also fails if an
unlisted record fails or a listed one passes. Running the date records found one real executor defect,
fixed here: text shaped like a date or timestamp but naming a field that does not exist (`2023-02-29`,
`25:00:00`) returned `22007` instead of PostgreSQL's `22008`. This is **not** the 100 percent P2-13 asks
for, and no differential run against PostgreSQL was made for this executor.

**Evidence run locally for `lattice-exec` and the `lattice-api` adapter only, at default proptest case
counts:** unit, operator, session, model (random workloads against an in-memory oracle, with a
negative control), planted-engine-bug, no-panic and sqllogictest tests, `clippy -D warnings`, `cargo xtask
check-layers`. Not run locally: the workspace tests, Miri, sanitizers, mutation testing, larger proptest
case counts, benchmarks and the simulator workloads; those are left to CI. No Phase 2 exit criterion is
claimed, and P2-10, P2-13, P2-17 and P2-18 remain open.

## Table id allocation (issue 445)

`CREATE TABLE` took the largest live id plus one and retried 64 times on a used id, so after 64
create/drop cycles every later `CREATE TABLE` failed with an internal error, also after a restart.
`Catalog::next_table_id` now answers from the catalog's record of every id ever created (live or dropped,
rebuilt from the log), and the executor uses it. When no id up to `MAX_TABLE_ID` is left the statement fails
with SQLSTATE `53400`. Tests run 130 create/drop cycles with a reopen, a create after dropping the highest
id, and the exhausted case. Not changed: managed index ids and the in-memory row id counter for tables
without a primary key (`alloc_rowid`), which can still reuse the id of a deleted newest row after a restart.

## SQL served by `latticed` from a data directory (P2-17, P2-18, issue 345)

PR [412](https://github.com/moncifem/latticedb/pull/412): `latticed --data-dir DIR` opens a write-ahead log in `DIR`, replays it, and serves the `lattice-exec`
executor over the existing bounded pgwire listener (loopback trust or SCRAM over TLS). Without
`--data-dir` the `SELECT 1` placeholder remains. See the [listener guide](../../crates/latticed/README.md).

**What is checked, through the real binary** (`crates/latticed/tests/sql_binary.rs`, a plain pgwire
client written in the test, no driver): create, insert, update, delete and select; `RETURNING`; two
connections with snapshot isolation; `23505`, `23502`, `42P01` and `25P02`; a connection dropped
mid-transaction commits nothing; `42501` for DDL by a `deny-all` development policy and, under SCRAM over
verified TLS, by a `deny-all` account and by an account the policy file does not list (no allow-all
fallback); for those principals the catalog is hidden, so DML and `Parse`/`Describe` on an existing table
are `42P01` with the message of a missing table (checked against both, in `lattice-exec`, `lattice-api` and
through the binary); a client's `SET statement_timeout` is clamped to `--statement-timeout-max-ms`; a second
server on the same directory is refused; a committed row is in the log after a clean stop and after
SIGKILL (the test looks for the row's bytes in the log files, since SQL cannot read it back, see below);
the extended protocol with text parameters (Parse, Bind, Execute, Sync), including a `22P02` bad parameter; option validation. `lattice-api` has unit tests for the policy file and the database thread.

**What survives a restart.** Rows, table names and, since issue 573, column definitions are recovered from
the log, so a restarted server answers `SELECT` on a recovered table and keeps enforcing its constraints
(see "SQL definitions in the executor" below). When this section was first written the columns were not
recovered and "create a table, insert, select and see the data survive a crash" from issue 345 was not met;
the restart proof with the real binary and the simulator is issue 574 and is not claimed here.

**Limits.** One database thread runs statements serially; a connection waits at most the statement
ceiling plus 5 s and then gets `57014` (the statement is cancelled and skipped if not started, and an open
transaction block is failed on the database side so a later `COMMIT` answers `ROLLBACK`; the proxy's abort (a mark in
per-connection shared state, not a queue entry; the queue is bounded and no send blocks, #465), the
thread's skip-path aborts for `Execute` and for `Describe`, the abort for a failed or timed-out `Parse`/
`Describe`, and the discard of a late `BEGIN` are each pinned by a unit test that fails without it; a block failed by an
error the wire session raises itself is failed on the database side too (`QueryHandler::abort_transaction`,
#419): cancel before dispatch, binary `Bind`, unknown portal or statement, duplicate or excess `Parse` and a
busy `describe` are each followed by `COMMIT` in a test, and removing the hook fails them), but a client that disconnects mid-statement does
not stop it, `COMMIT`, DDL and an autocommit statement's commit are not cancelled once dequeued, so a
`57014` after the ceiling means the outcome is unknown (an autocommit statement may have committed), and
the backlog of abandoned work can reach `max-connections x ceiling`; a dead database thread is detected (`57P01`, non-zero exit);
the SIGKILL test shows the row's bytes reached the log files, not that they were `fsync`ed; a statement cannot be interrupted inside the
engine's scan materialisation or a sort comparison (see the `lattice-exec` README). Row policy has two
grants, `allow-all` and `deny-all`. Binary result formats are not exercised. No driver conformance is claimed.

**Not run locally, left to CI:** the workspace gate, Miri, sanitizers, mutation testing and fuzzing.
This touches authentication-adjacent and serving code and needs an independent review before merge.
No Phase 2 exit criterion is claimed; P2-17 and P2-18 remain open.

## Index scans in the executor (P2-07, P2-10, issue 381)

`lattice-exec` chooses an access path by fixed rules (no statistics): a `UNIQUE` constraint pinned by `=`
is one entry lookup, a fully pinned primary key is a point read, and `=` on leading key columns with a
range on the next is a key range; everything else is a full scan. Only `integer`, `text`, `boolean`,
`bytea`, `uuid`, `date` and `timestamptz` constants of exactly the column's type are used; the whole
predicate is still evaluated on every row read, and the choice is made only when every term of the
predicate is certain to succeed, so rows, order and errors equal a full scan. `UPDATE` and `DELETE` use the
same paths. `Database::last_access_paths()` reports the choice of the last statement.

Evidence (`crates/lattice-exec/tests/index_scan.rs`, run locally at default proptest case counts and once
at 1,500 cases): a differential property test (generated tables with `NULL`s, ties, `bigint` extremes and
out-of-range or non-integer constants, workloads of inserts, updates and deletes, live reads, `AS OF
CHANGESET` reads, reads inside an open transaction with its own writes, and an actor policy that hides
half the rows) requires each narrowed `SELECT` to equal the same predicate forced to a full scan; the
same for composite text keys (prefix keys such as `a` and `ab`) and for `UPDATE`/`DELETE ... RETURNING`
followed by a table comparison; fixed cases for the chosen path of each rule, parameters, a failing term
that must keep the full scan, cancellation and memory limits in the index loop, and a policy that hides
entry tables. Two planted bugs (`ExecIndexUpperBoundOpen`, `ExecIndexPolicySkipped`, group
`Bug::EXEC_INDEX`) are each caught by a witness test, and a hand-made mutation of the lower bound was
caught by the suite.

**Not done.** The executor creates no managed secondary index (`CREATE INDEX` is `0A000`), so `lookup_at`
and the LCIX definitions are not used yet; `IN` lists, `ORDER BY` from key order, `LIMIT` early stop,
reverse scans, an `EXPLAIN` statement and a streaming range read are open. The range read still
materialises row references before its first poll, like a full scan. Left to CI: the workspace gate,
Miri, mutation testing of `access.rs`, and the larger proptest case counts. No Phase 2 exit criterion is
claimed; P2-07 and P2-10 remain open.

## WAL capability bit 3, SQL definitions (issue 555)

`lattice-core::admission` defines `SQL_DEFINITIONS = 8` and `KNOWN_CAPABILITIES` is now `1 | 8`
(RFC 0012 Option A, ADR 0028; value 2 stays unknown and value 4 stays unassigned). Header encode and
decode accept masks 8 and 9; every build before this change refuses a segment that carries bit 3 before
any callback, read of record bytes or repair, which the new tests pin for readers supporting 0 and 1.
Tests also cover the cumulative mask surviving rollover and a restart, a writer that never asks for the
bit never writing it, and an append requiring the bit under a narrower supported set refusing without
consuming an LSN. Independent fixtures `wal-admission-v2-{sql-definitions,managed-sql-definitions,
unassigned-capability}.golden` (masks 8, 9 and 4) come from `tools/wal-admission/make_fixtures.py` with
its own CRC32C; the existing fixtures are byte-identical. `Bug::SqlDefinitionsBitDropped` (group
`Bug::ADMISSION`) is caught by a literal required-mask witness with a clean control.

**Not done.** The frozen old-reader hosted job (issue 559) is separate and not run here. Since issue 572 the
engine sets and lists the bit (see "SQL definitions in the engine" below).
The owner waived the RFC 0012 seven-day comment hold in chat on 2026-10-30. No phase-gate claim.

## SQL table definition codec (RFC 0012 work package 2, issue 556)

`lattice-engines::sqldef` is the structural codec for the durable SQL table definition block: types, a canonical
encoder, a strict bounded decoder and the structural validator for definition version 1 / layout rule 1, plus the
definition-operation block (count, ascending unique table ids, at most 1,024 per envelope). The format is
[`formats/sql-definition-v1.md`](../../formats/sql-definition-v1.md), status proposed: RFC 0012 Option A is accepted by
the owner, who waived the seven-day comment hold in chat on 2026-10-30.

Evidence: independent goldens (59 files from `formats/generate_sql_definition_golden.py`, Python `struct` only, one
rejection per rule) checked against literal expected outcomes in `crates/lattice-engines/tests/sqldef_golden.rs`;
property tests over generated valid definitions (round trip, canonical re-encode, every strict prefix refused,
single-bit mutation either refused or re-encoding to the mutated bytes, arbitrary bytes never panic); planted bug
`SqlDefColumnsSorted` (group `Bug::SQLDEF`) caught by the layout witness; the fuzz target `sqldef_decode` (seeds in
`fuzz/seeds/sqldef_decode`, limits in `fuzz/settings.toml`) is in the nightly, weekly and 24-hour matrices.

**Not done.** The fuzz target was neither built nor run here; its runs, the 10,000-case and nightly property
volumes, mutation testing of `sqldef.rs` and the independent review of this format are left to CI and to the review
that follows. No Phase 2 exit criterion is claimed. The codec is now called by the catalog and the row store (next
section).

## SQL definitions in the engine (RFC 0012 work packages 3 and 4, issue 572)

`lattice-engines::catalog` encodes and decodes the LCIX version 3 envelope (version 1 plus a definition block before
the row block; a change without definitions stays version 1, byte for byte). `Catalog` keeps one definition per table id
(defining LSN, dropped LSN, shared structure) and enforces on commit and on replay, all or nothing: the table is live after
the same change, it has no earlier definition (the id is never reused), the structure is valid. `RowStore::commit_with_definitions`
writes it under required capabilities 1 and 8 in one append, so the guard is durable before the record; recovery and
`CatalogStore` list both bits, refuse a version 3 payload in a segment without mask 9, and refuse the directory on any invalid
block. The first definition in a directory that holds records and lacks bit 3 fails with `FormatUpgradeRequired` unless
`RowStore::set_allow_format_upgrade(true)`; a new empty directory needs nothing.

Evidence (all in `cargo test -p lattice-engines --test sql_definitions`, 21 tests): an old reader (zero support, managed-only,
callback mode) refuses a directory with a definition before any callback and without changing a byte, with the header mask read
from the bytes as a literal 9; a change without definitions requests only bit 0; replay refusals for a version 3 envelope
with no definition, a definition for a missing or dropped table, a second definition and an unguarded segment; independent
LCIX version 3 vectors (3 accepted, 9 refused, strict prefixes and single-bit flips) from `formats/generate_sql_definition_golden.py`;
a `CatalogStore` projection with `AS OF` history after drop and rename; the upgrade setting in a rows-only and a managed directory;
and a power-cut sweep over 20 workloads (five shapes of table creation, with and without a legacy prefix, large and rolling
segments) cut at every mutating storage operation: the outcome is no table or the table, its definition and its rows, never
one without the others, acknowledged changes are kept, no definition survives without bit 3 in a header, and a second recovery
agrees. Planted bugs (group `Bug::SQL_DEFINITIONS`, each with a clean control): `DefinitionDroppedOnReplay`, `GuardBitNotRequested`
(also caught by the sweep), `SecondDefinitionReplaces`, `DefinitionForMissingTableAccepted`, `DefinitionOfExistingTableAccepted`,
`DefinitionDropIgnored`, `FormatUpgradeUnchecked` (seven). The fuzz target `sqldef_decode` also decodes whole envelopes (selector 2 mod 3, new seeds).

**Not done.** The executor now calls this (issue 573, next section). Not implemented from RFC 0012: the commit-time liveness check for row writes to a
defined table and its marker tables (rule 5), the catalog-wide caps (owner decision 7 is open), vacuum pruning of dropped
definitions, and `adopt_table`: definition-only adoption of an existing table is not implemented and is refused
(`CatalogError::DefinitionNotInCreate`), so a definition can only arrive with the `Create` that the DDL grant judged;
`TableGrantPolicy::authorize_ddl` also checks every definition by the created name or the existing id. The
engine does not check rows against the definition (it cannot name SQL types); the executor does. The frozen-reader hosted job,
the fuzz run, mutation testing of the new code, the 10,000-case and nightly property volumes and the independent review of the
final diff are left to CI and the review that follows. No Phase 2 exit criterion, phase gate or release is claimed.

## psql script suite against the real binary (P2-17, issue 575)

`tests/drivers/psql/` holds psql scripts that a CI job (`driver-psql`, path-filtered to `latticed`,
`lattice-api`, `lattice-exec` and the suite itself) runs against the release-built `latticed --data-dir`.
`run.sh` starts the server (the stdin FIFO and `shutdown` path of the README quickstart), feeds each
`NN_*.sql` to psql, and diffs stdout and stderr with the checked-in `NN_*.expected`. `scram_tls.sh` builds
a throwaway CA, a server certificate and a SCRAM verifier (openssl, and PBKDF2/HMAC from the Python
standard library following RFC 5802 and 7677, not latticed's code) and starts `--auth scram-tls`.
`selftest.sh` checks the runner can fail. The job fails when no script matched, when the server did not
start or did not exit 0 after `shutdown`, when a case in `known/` starts to pass or changes to other output, and when the self-test
cannot turn the run red.

The expected files were written by hand from PostgreSQL's documented psql output and command tags, not
recorded from `latticed`. As an independent check, each of them (the known cases included) was also run
once against a real PostgreSQL 17.11 server with psql 17.11 and matched byte for byte. That check is not
part of CI, which needs no PostgreSQL server.

| Script | Covers | Result |
|---|---|---|
| `01_ddl_dml` | `CREATE`/`DROP TABLE`, multi-row `INSERT`, `UPDATE`, `DELETE`, `RETURNING`, command tags with row counts | pass |
| `02_select` | predicates, `NULL` logic, `IN`/`BETWEEN`/`LIKE`, `ORDER BY ... NULLS`, `LIMIT`/`OFFSET`, `coalesce`, `nullif`, operators | pass |
| `03_transactions` | commit, rollback, the aborted state (`25P02`) and `COMMIT` answering `ROLLBACK`, recovery of the connection | pass |
| `04_errors` | `42601`, `42P01`, `42703`, `42P07`, `23502`, `23505`, `22001`, `22003`, `22012`, `22P02` | pass |
| `05_extended` | psql `\bind`: Parse/Bind/Execute with text parameters, `RETURNING`, errors, an aborted block | pass |
| `06_types` | smallint, bigint, double, text, varchar, bytea, date, uuid, timestamptz round trip; range, date, uuid errors | pass |
| `scram_tls.sh` | SCRAM-SHA-256 over TLS with `sslmode=verify-full`: correct password runs SQL; wrong password and unknown account get the same `FATAL: password authentication failed`; untrusted CA, wrong host name and no TLS are refused | pass |
| `restart_a`, `restart_b` | write, then `shutdown`; a second start on the same data directory returns the rows, accepts a new row and drops the table (issue 573; before it `restart_b` was a known failure answering `42P01`) | pass |

Run locally (not in CI yet at the time of writing): a debug build of `latticed` at `47484d3d` with psql
18.6 (libpq) and with psql 17.11, loopback and SCRAM over TLS, plus the self-test, all as listed above.
The CI job prints the exact client version into its step summary; the first CI result and its psql
version (expected: the client on `ubuntu-latest`, PostgreSQL 16) are recorded here when the run exists.

**Known gaps, listed and not hidden.** Each is a case in `tests/drivers/psql/known/` that must still
equal the latticed output pinned next to it in a `.latticed` file, checked by hand against the issue. The run turns red when a case starts to match PostgreSQL, and also when its output changes to anything else:

- `COMMIT` or `ROLLBACK` outside a block and `BEGIN` inside one send no `WARNING` (`25P01`, `25001`)
  (`known/03_transaction_misuse`, #674).
- An unknown type name answers `0A000` instead of `42704` (`known/04_unknown_type`, #675).

**Not covered, and why.**

- Cancellation by a psql signal (Ctrl-C): `latticed` has no sleep function and no statement whose
  duration a script controls, so a signal test would race the statement. Cooperative cancellation is
  covered by the socket tests in `lattice-api` and `latticed`.
- The SQLSTATE `28P01` of a refused login: psql shows the SQLSTATE of a query error but not of a
  connection failure, so `psql_scram_tls_wrong_password_fails_with_28P01` checks libpq's
  `FATAL:  password authentication failed` and psql exit status 2, not the code.
- `date - date` and `date + integer` answer `42804` ("arithmetic is not defined for date"); the subset
  does not list date arithmetic, so no script asserts it.
- Binary result formats, `PREPARE`/`EXECUTE` (refused by the executor), `COPY`, JDBC and the driver
  matrix (#413). The psql suite is one client; the three drivers in the next section are text mode only.
  No Phase 2 exit criterion is claimed.

## Driver smoke scripts in text mode (issue 576)

`tests/drivers/smoke.sh` runs one script per driver against a built `latticed --data-dir`, each on its own
server and data directory over loopback trust without TLS: `node/smoke.mjs` (node-postgres `pg` 8.23.1) and
`go/main.go` (pgx v5.11.0). Both run the same steps: connect; `SELECT` of a NULL, a bool, an int and a text;
`CREATE TABLE`; INSERT, SELECT, UPDATE and DELETE with parameters, including NULL; a committed and a
rolled-back transaction; an error with its SQLSTATE (`23505`, `42601`); `DROP TABLE`; a clean close. Each
prints one JSON line per step. `smoke_compare.py` (standard library) compares the lines with `expected.jsonl`,
which was written by hand from PostgreSQL's behaviour and from each driver's documented type mapping (pg returns
bigint as a string), never from a run of latticed. It also checks that the expected output covers the tags
parameter, null, bool, int, text and error, and that a run with its last step cut off fails. The lockfiles pin
the drivers: `node/package-lock.json` (installed with `npm ci --ignore-scripts`) and `go/go.sum`. CI job
`driver-smoke` (`.github/workflows/driver-smoke.yml`) installs from them and runs the same script.

| Driver | Setting needed for text mode | Result | Pinned known failures (in `known.jsonl`, with the issue) |
|---|---|---|---|
| pg 8.23.1 | none: values use the extended protocol with text results; bigint comes back as a string | 16 of 16 steps pass | none |
| pgx v5.11.0 | `default_query_exec_mode=exec` (`QueryExecModeExec` in the script) | 16 of 17 steps pass | the default query mode fails with `expected 0 arguments, got 1`, because the server describes no parameter types (#703) |

Each known failure is a step whose output must equal the pinned latticed output. The run fails when it changes
to anything else, and when it starts to match PostgreSQL (then it moves out of `known.jsonl`). A driver script
that exits non-zero, a server that does not stop with status 0 after `shutdown`, or a missing step fails the
job. A latticed that dies before it opens its stdin fails the run at once; it does not hang.

**Python is not in this suite.** The first candidate, psycopg 3, is LGPL-3.0, and the licence allow list in
`docs/dependency-policy.md` does not allow it. Its findings stay on record in #608 (psycopg's default `%s`
placeholders send binary parameters, refused with `0A000`) and #382 (its default connection opens a
transaction block before `CREATE TABLE`, refused with `25001`). The permissively licensed replacement, pg8000
(BSD-3), is tracked in #710.

Run locally, not in CI yet: a debug build of `latticed` rebased on current `main` (built with `CARGO_TARGET_DIR`
outside the checkout and `CARGO_BUILD_JOBS=2`); node 26.13.0 (CI: 24) and go 1.27.1 (CI: the toolchain that
`go.mod` asks for). Both scripts matched their expected output, with the known case xfail. The negative checks
were run by hand: a single changed value and a missing step each fail the comparison, and a fake latticed that
exits at start fails the run. The first CI result and the exact versions it prints are recorded here when that
run exists.

Not covered, and why: JDBC and binary formats (#413, #608); restart and reconnect from drivers (the psql restart
scripts cover the server restart); TLS and SCRAM for these drivers (the psql suite covers SCRAM over TLS); pgx's
`simple_protocol` mode and its default mode once #703 is fixed (not run yet); cancellation of a statement from a
driver; Python (#710). Each of these is listed here rather than claimed. No Phase 2 exit criterion is claimed.

## SQL definitions in the executor (RFC 0012 work package 5, issue 573)

`CREATE TABLE` builds the table's definition (column names, types with the `varchar` limit, `NOT NULL`, primary key,
`UNIQUE` constraints in declaration order, constant defaults, constraint names), validates it with the engine's structural
rules, and commits it with `RowStore::commit_with_definitions` in the same change set as the `Create`. `Database::open`
rebuilds the executor's schema from `Catalog::definitions` and the catalog names, re-checks what the engine cannot (numeric
and `jsonb` defaults) and refuses to open on a definition it cannot serve. The `UNIQUE` marker tables come from the position
in the record. `Database::attach_table`, `detach_table`, the in-process fingerprint, `ExecHost::unattached_tables` and the
`latticed` startup report are removed. `--allow-format-upgrade on` (`ExecHost::start_dir_with`,
`Database::set_allow_format_upgrade`) is the host setting for a directory written before definitions existed; without it
`CREATE TABLE` there answers `55000` and writes nothing, and a new directory needs nothing. `DROP TABLE` marks the definition
dropped and keeps it for `AS OF` reads; a recreated name gets a new id and its own definition.

Evidence (`cargo test -p lattice-exec --features lattice-core/planted-bugs --test definitions`, 12 tests, plus the existing
executor tests): a table with every column type, defaults, a unique `varchar` and a primary key reads back identically after
a reopen, and its schema equals the one before; primary key, `UNIQUE` (including NULLs), `NOT NULL` and the `varchar` limit
are still enforced; two tables with the same columns and `UNIQUE` constraints declared in opposite order keep their own marker
mapping; a drop removes the definition from the current catalog and keeps it as of the earlier LSN; the upgrade setting is
needed for a legacy directory and not for a new one; a stored `jsonb` default that is not JSON refuses the open; a power cut at
every mutating storage operation of `CREATE TABLE` (4 shapes: with and without an earlier table, clean and crash-heavy disk)
leaves the table whole or absent, keeps an acknowledged table, and frees the name when it is absent. The model test
(`tests/model.rs`) gained a `Reopen` operation and still agrees with its oracle. Planted bug `ExecDefinitionNotPersisted`
(group `Bug::EXEC_DEFINITIONS`) is caught by the restart check. `latticed` tests (`sql_binary`, `service`) and the psql
scripts `restart_a` and `restart_b` now expect the rows and constraints after a restart; the README quickstart smoke job
checks the same.

**Behaviour changes and limits, listed and not hidden.** A quoted column name with an ASCII capital (`"Id"`) is refused with
`0A000`; PostgreSQL accepts it, and `identifiers_pg` records the difference, because the record format
(`formats/sql-definition-v1.md`) forbids capitals in column names. A numeric default is stored in canonical form, so
`DEFAULT 1.50` reads `1.5` from the first statement on. `DEFAULT NULL` is the same as no default. A table created by an earlier
build keeps its name but has no definition and is not addressable; adoption is not implemented. `ALTER TABLE` is out of scope.
Not claimed: the restart proof with `kill -9` seeds against the real binary and the simulator workload (issue 574),
power-loss durability, a phase gate or a release. The 10,000-case property volume, the mutation run, the real-binary psql job
and the quickstart smoke job are left to CI.
