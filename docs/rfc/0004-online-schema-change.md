# RFC 0004: Online schema changes with shadow generations

Status: draft
Comment period ends: 2026-11-05 (at least seven full days after this RFC's PR opens)
Decision ADR: proposed ADR 0018, not accepted

## Motivation

P2-16 and [WP31](https://github.com/moncifem/latticedb/issues/123) require add-column,
drop-column and index backfill while writes continue, with no lost or double-applied write after
any crash. A row scan followed by a catalog switch is insufficient: an update or deletion during
the scan can be overwritten by an older backfill value, and an unlogged scan cursor can skip data
when recovery resumes.

This document proposes a protocol, not an implemented API or an accepted format. Current main has
versioned table names in `catalog.rs`, typed row encoding in `typed.rs`, deterministic migration
lenses in `lattice-lsl`, derived runtime-supplied indexes under ADR 0014, and an in-memory MVCC
vacuum floor. It does not persist writer-schema dispatch or online migration state. The catalog
currently records create/rename/drop; it cannot yet atomically publish the control records below
alongside row mutations. Catalog-managed indexes and pending typed/checkpoint proposals require
separate review. This RFC accepts none of them. Phase 1 remains unsigned under its Phase 2 waiver.

## Proposal

### Logical identity, generations and eligibility

Keep a stable logical table identity and create a new, hidden physical generation for the target
layout. Physical table, column and index ids are never reused. A snapshot selects a catalog/schema
version, then the corresponding generation and its original writer layout; it must not decode old
bytes using the newest layout. Preserve historical layouts, overflow fields, row versions and
change-set provenance until their snapshots are no longer retained. A dropped column is hidden by
the new schema; its historical bytes do not disappear at cutover.

Each plan contains canonical old/new checked LSL schemas, their hashes, a checked lens and its
version, exact target object ids, and explicit limits. No executable closure, ambient clock,
random default or current policy result is serialized. Add-column uses a deterministic default or
optional absence admitted by the new schema. Drop-column may be lossy forward conversion; it does
not imply an inverse or a downgrade guarantee. These initial transformations preserve primary keys; changing keys needs
a separate conflict and identity design. A name reused later gets a new column identity.
Reject unsupported or unbounded transformations before starting. During the dual-write window,
writes must validate under both required layouts; a conversion/constraint error rejects the whole
write rather than acknowledging only its old-layout half.

For a non-unique index build, the shadow object is an index generation over the same source rows,
not an authoritative second copy of the table. Its extractor is a versioned catalog description,
not ADR 0014's process-local function pointer. The target stays invisible to query planning until
cutover. Unique constraints and their concurrent duplicate-admission policy need a further decision
before a unique index can use this protocol.

Only one migration per logical table is admitted initially. A second plan, including a conflicting
index build, receives a retryable conflict. Disjoint tables may progress independently. Writer
transactions capture a schema epoch and revalidate it when committing; a stale epoch is rejected
for retry before any WAL append. Preparation and cutover occupy short serialized writer steps;
the full scan does not hold a table lock. Actual latency and maximum step costs remain to be measured.

### Durable state machine

All names below are proposed semantic records, not byte tags or existing methods. The final codec,
version bounds, independent golden corpus and compatibility rules need their own implementation
review. The log remains the source of truth. A manifest or in-memory job queue alone is insufficient.

| Phase | Durable information and writer behavior |
| --- | --- |
| Prepared at LSN P | Migration id, plan/hash, source/target epochs and ids, source snapshot P, empty frontier, limits and a recovery-restored snapshot pin. The same atomic publication installs the dual-write rule before the next writer commits. |
| Backfilling | Source remains authoritative. Every subsequent user change set updates source and shadow atomically at the same commit LSN/id, including tombstones. Bounded batches install source-snapshot values conditionally and advance a durable key frontier. |
| Ready at LSN R | All source keys through an explicit end-of-scan marker were covered. A bounded verification job validates schema conversion and row/index equivalence at a pinned barrier snapshot R. Later commits still dual-write. A count or checksum alone cannot establish equivalence. |
| Active at LSN C | One writer step checks the unchanged plan/epoch and Ready proof, then publishes target schema/generation/index visibility. Later writes use the target only. Old-epoch transactions must retry. |
| Retired | No open snapshot, retained history or branch pin can name the old generation. A logged retirement permits bounded physical reclamation; WAL history is retained. |
| Aborted / Failed | Before activation, a logged abort removes the dual-write obligation and releases worker pins. Failure retains diagnostic state and the source; it is not permission to expose a partial target. |

The migration pin is registered atomically with preparation and reconstructed before vacuum can run
on recovery. Its snapshot P must be at or above the current floor; already-vacuumed history cannot
be recovered by declaring a pin. Vacuum uses the minimum of retention, open-reader, migration and
branch pins. The current caller-supplied `oldest_open` parameter is not a durable pin registry: that
integration is a prerequisite. Branches are not implemented on current main; until their pin
semantics exist, a migration must refuse any configuration whose retained branch history it cannot
account for. Releasing a pin never lowers an already-advanced floor.

The kernel currently accepts one opaque payload per change set. Implementing this protocol therefore
requires a reviewed compound engine/control envelope (or an equally atomic versioned mechanism),
not separate commits for a row, index, frontier and phase transition. Acknowledgement follows the
ordinary durable kernel commit. Recovery must fail on a recognized but undecodable migration record;
it must not silently skip a rule that affects subsequent row writes.

### Backfill and concurrent writes

Scan source snapshot P in stable primary-key order using bounded cursors. Current range APIs
materialize result vectors, so bounded row/index snapshot cursors are another implementation
prerequisite; a batch-size setting alone does not bound such a full scan. The durable frontier is
an exclusive last-completed key with explicit beginning/end sentinels, independent of a wall-clock
batch duration. The scanner pins P until all source-snapshot reads are complete. New rows after P,
including keys behind the frontier, are handled by dual writes and need not appear in the base scan.

A backfill candidate carries its source version identity. A target row also records the source
version that produced it. Within the serialized commit step, install a candidate only if the target
has not already incorporated a later source version; a later deletion must prevent resurrection.
A candidate is a transformation of source data, not a new user mutation or a second application of
its business effects. A plain `Table::apply` at the backfill commit LSN cannot implement this guard:
that LSN is later even when the candidate's source value is older. The proposed backfill operation
must distinguish source version order from physical batch publication order.

Publish all accepted target installations and that batch's frontier in one atomic change set.
Do not advance the frontier when a limit, conversion, conflict or I/O failure prevents the batch
from committing. A restart replays the committed frontier and target state, then retries the next
range. Batch identity must bind the migration id, source snapshot, plan hash and exact input range;
reusing an id with different payload bytes is forbidden. On an ambiguous commit, recover the log
before deciding whether to retry; a duplicate acknowledgement alone is not a new frontier update.
Do not keep an unbounded in-memory set of every copied row just to deduplicate retries.

The target's logical state at C must equal the old authoritative state at C transformed through
the plan, including concurrent updates/deletes, overflow data and rows inserted after P. Shadow
indexes must equal a rebuild from the corresponding transformed rows at that snapshot. Backfill
LSNs are not substituted for the source change-set ids exposed by row history. Query/catalog
snapshot selection hides target construction versions before C; old snapshots continue using the
source generation and its writer schemas.

For example, P sees key K at source version 10. A user update commits version 12 and dual-writes
it while backfill is scanning K. The later backfill batch may publish at LSN 20, but its candidate
still represents version 10 and must not replace version 12. The same rule applies if version 12
is a deletion. Conversely, a user write after a committed base installation must update the target
normally, exactly once in the user change set.

### Cutover, old transactions and retirement

Ready verification pins a consistent snapshot R and compares complete projected row/key/value
content and index entries with an independent fold of the source log. With unchanged primary keys, row comparison can merge bounded ordered cursors; index
comparison also needs bounded ordered cursors or explicitly budgeted external sorting. Verification
must not materialize the entire source and target under a nominal batch limit. It records the checked plan,
source/target epochs, covered frontier and verification barrier, not an unchecked boolean supplied
by a worker. Any verification mismatch keeps the source active. All commits after R must still
follow the same dual-write rule; another schema/plan change invalidates readiness. No unlocked gap
between checking readiness and publishing C is allowed.

Cutover is one catalog visibility event. A statement at a snapshot before C uses the old schema;
a statement at or after C uses the new schema and complete target. A transaction cannot mix the two
mid-statement. A pre-C writer attempting to commit after C gets a schema-epoch conflict and retries;
it cannot write only the frozen source. Prepared statements and cached plans must bind/check epochs.
Old read-only transactions can finish at their pinned snapshots without keeping writers on the old
layout after C. A new reader below the retention floor receives `SnapshotTooOld`, not partial data.

Retirement requires both the oldest live pin and the retention floor to reach C, plus proof that
no retained catalog/schema/branch reference needs the old objects. Release the base-scan pin once
readiness no longer needs P, but retain verification/old-reader pins for their actual lifetimes.
Drop work is chunked and resumable; a crash after marking Retired but before deleting bytes resumes
cleanup. Tombstoned ids remain reserved. The WAL is not pruned by migration or retirement.

### Failure, cancellation and rollback

| Interruption | Required recovery result |
| --- | --- |
| Before preparation is durable | Source only; unreferenced staging objects may be reclaimed. No active dual-write rule or pin. |
| After P but before the first batch | Restore the plan, pin and dual-write rule before accepting writers; resume from the beginning. |
| During a batch or its frontier publication | Recover the complete batch and frontier, or neither. Repeat no external side effect and skip no source key. |
| During a dual write | Both source and target plus derived indexes share the user change set's outcome; acknowledged source-only writes are forbidden. |
| After Ready, before C | Source stays authoritative and dual writes continue. Revalidate the durable barrier proof before cutover. |
| During C | Recover exactly one active epoch. A torn or unacknowledged C is resolved through normal log recovery, not a local flag. |
| After C, before retirement | Target is authoritative; old snapshots still route to retained source history. Resume cleanup only after pin/retention checks. |
| During retirement | Logical visibility stays on target; replay resumes idempotent deletion without id reuse or WAL deletion. |

A worker polls cancellation/deadline between bounded keys and before submitting a batch, and charges
memory before allocating buffers. Memory-limit primitives alone do not provide this integration.
An interrupted uncommitted batch leaves no frontier advancement. Worker cancellation pauses work;
removing the migration requires an explicit logged abort. Once commit outcome is ambiguous, cancellation
cannot promise rollback: resolve it through recovery first.

Before C, abort is forward progress in the log: the source remains authoritative, shadow objects
become unreachable, and pins are released when no reader/worker still uses them. Never rewind the
WAL or assume a dropped column's lens has an inverse. After C, rollback is a new migration with
its own validation and backfill; the frozen source has missed later target writes. A fast reverse
cutover would require an explicitly designed and verified reverse-dual-write window. This proposal
does not provide one. Cleanup of a failed or aborted target must also respect any worker pin.

### Invariants and observability

The protocol must preserve I1/I2 durability and atomic change-set outcome, I3 snapshot visibility,
I9 index/catalog rebuild equivalence, and the existing conflict/idempotency rules. Migration-specific
obligations are: one authoritative generation per catalog snapshot; no acknowledged source-only
write during dual-write; no older backfill overwriting a later source version; durable frontier
never outrunning durable data; pins surviving recovery before vacuum; and no retirement while any
permitted historical read can still reference the old generation.

Expose phase, source/target epochs, P/R/C, last completed key, rows/bytes processed, retry/conflict
counts, pin/floor positions and charged/peak memory. Keys, source values, policy data and error payloads
must not leak in public diagnostics. Persist enough bounded progress metadata for recovery; detailed
per-row debugging is not a required growing journal. Report stalled pins and cancelled/failed jobs
without declaring them complete. No timing, RSS, concurrency or crash measurements exist for this
proposal yet.

### Verification required before implementation acceptance

- A small independent map model folds source user mutations and catalog epochs. Generate prepare,
  bounded backfill, insert/update/delete, old/new snapshot read, cutover, abort, vacuum, crash and
  restart steps; compare every retained snapshot, writer-schema result and index with that model.
- Schedule a write/delete for a key before scanning it, after scanning but before its batch commit,
  and after batch commit. Include keys ahead of and behind the frontier, empty tables, repeated keys,
  add-column defaults, dropped fields/overflow, multiple updates and new rows during backfill.
- Cut simulated power at every mutating I/O step of P, each batch/frontier, user dual-write, Ready,
  C, abort and retirement. Match whole submitted envelopes and every acknowledgement after replay;
  include unknown outcomes, failed sync, bit rot/refusal and two consecutive recoveries.
- Hold old snapshots and migration pins across vacuum attempts, restart and C; check exact-floor
  reads, `SnapshotTooOld` below it, schema-epoch conflicts and retirement admission. A restart must
  restore pins before a competing vacuum step. Include eventual branches once available.
- Add planted defects for skipped shadow write, missing tombstone, frontier-before-data, older-base
  overwrite, lost recovery pin, early catalog activation and premature retirement. Require clean
  controls and semantic mismatches against the independent model, not merely a panic or setup error.
- Independently encode positive/negative format fixtures once the compound codec is proposed. Check
  old-reader refusal, strict bounds, trailing bytes, unsupported versions and replay equivalence;
  fuzz the decoders without exposing arbitrary code or ambient inputs.
- Measure writer latency during backfill, bounded per-batch memory, recovery/resume time, retained
  versions and pin/retirement cost on hosted CI. Compare runs with the same source workload; publish
  raw artifacts and exact revisions. No performance target is passed by this design document.

## Alternatives

A stop-the-world rewrite is simpler but blocks writers for the whole dataset. A one-pass live scan
without a pinned base, dual writes or conditional installation loses concurrent changes. Lazy read
conversion reduces initial work but leaves indefinite mixed-layout and index costs and does not by
itself finish the required backfill. Separate logs/commits for the two generations cannot provide
I2. Reinterpreting existing row/catalog bytes would make recovery depend on software version; this
proposal instead requires explicit reviewed versioning before any persisted protocol is written.

## Compatibility and migration

No existing on-disk bytes, APIs, WAL records or SQL grammar change in this docs-only proposal.
Implementation needs a versioned migration/control envelope, persisted schema dispatch and index
identity, frozen codecs and independent review. Existing LCAT/LROW tags must not silently acquire
new meanings. Unsupported readers must refuse the new mandatory record/version before writing;
retained WAL alone is not a downgrade guarantee once they cannot decode its control records.

Do not accept ADR 0018 or merge a format implementation before this RFC's full seven-day comment
period, prerequisite format decisions and independent review. The owner must decide on pending
checkpoint formats separately. This proposal does not close WP31 or establish the Phase 2 gate.

## Open questions

1. Which compound payload/version coordinates catalog control and row/index changes, and what exact
   limits bound schema, lens, source-version and frontier encodings? This RFC defines no byte tags.
2. What schema-epoch error/SQLSTATE and retry contract should prepared statements and old writers use?
3. Where is the authoritative pin registry, and how do branch history/retention and checkpoints retain
   migration state before an accepted checkpoint codec can safely skip WAL prefixes?
4. What admission limits bound concurrent migrations, target disk growth and stalled-pin lifetime?
   A timeout cannot invalidate an acknowledged old snapshot silently.
5. Which transformations can be proved total and bounded at preparation, and which need an explicit
   data-validation phase? Unique-index admission and reverse-dual-write rollback remain undecided.
6. What complete verification can be performed incrementally without blocking writers, and how is
   its proof invalidated if any planned source or target definition changes?
