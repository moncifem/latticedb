# Glossary

Terms used across the Lattice documents, with a pointer to the page that defines them. Where a term
names a design that is only proposed, the entry says so. The [phase gates](gates/phase-1.md) record what
is implemented; this page does not.

<!-- glossary-terms: the wiki mirror lists each "## " heading below. Keep one heading per term. -->

## ADR

Architecture decision record. One file in [`adr/`](adr/README.md) per significant decision, with a status
(proposed, accepted, superseded). A format or wire change needs an ADR and an [RFC](#rfc).

## Capability guard

The WAL admission rule from [RFC 0008](rfc/0008-wal-capability-admission.md) and
[ADR 0022](adr/0022-wal-capability-admission.md): a log segment header can name the engine capabilities a
reader must support, and a reader that lacks one refuses to open the log instead of reading it wrongly.
It exists so an older binary cannot silently misread data written with a newer feature.

## Change feed

The ordered stream of events derived from committed change sets, one event per entity change. A
subscriber holds a [cursor](#cursor) and resumes after it. The encoding in
[`formats/change-feed-v1.md`](../formats/change-feed-v1.md) is an experimental candidate and is not
accepted or frozen.

## Change set

The unit of commit: all the entity changes of one transaction, written as one WAL record (type
ChangeSet) and therefore visible together or not at all. See [`formats/wal-v1.md`](../formats/wal-v1.md).

## Checkpoint

A WAL record that summarises applied state up to a given [LSN](#lsn) (its `through_lsn`) together with
the dedupe set and the turn table, so recovery does not have to replay older records. The kernel refuses
a checkpoint while anything is logged but unapplied. The checkpoint format for the row store is still
under review.

## CRDT

Conflict-free replicated data type: a structure whose concurrent edits merge to the same result on every
replica without coordination. Lattice has registers, sets and a text sequence type
(`lattice_core::crdt_text`); see [ADR 0010](adr/0010-text-crdt-spike.md) for the Phase 1 decision and the
Phase 3 measurements.

## Cursor

A position in the [change feed](#change-feed): a pair `(lsn, index)` of the WAL record holding the change
set and the zero-based position of the entity change inside it. Cursors compare by LSN, then index. A
cursor older than the vacuum floor is refused (`CursorTooOld`).

## Dedupe

Idempotent commit. The kernel remembers the identifiers of change sets it has already applied, so a
retried commit (for example after a lost reply) is reported as a duplicate and not applied twice.

## Fencing

Rejecting writes from a superseded writer. When a newer [turn](#turn) takes over a session, later writes
tagged with the old turn are refused ("fenced"). The kernel repeats the duplicate and fence checks at
the moment a change set is linearized, so a prepared transaction cannot slip past a fence.

## Gate

The status page for a delivery phase, [`gates/phase-N.md`](gates/phase-1.md): a table of exit criteria
with the measured evidence for each. Only the project owner signs a gate. Phases run in order.

## HLC

Hybrid logical clock. A timestamp `(wall_ms, counter, node)` ordered lexicographically, strictly
increasing per node whatever the wall clock does, bounded in drift, and after both inputs when merged.
See [`formats/hlc.md`](../formats/hlc.md).

## Kernel

The part of `lattice-core` that owns the log, the clock, the dedupe set and the turn fences, and drives
transactions through typestate steps from begin to commit. See [ARCHITECTURE](ARCHITECTURE.md).

## LSL

Lattice Schema Language, the schema language of the project (parser, checker, compatibility check and
typed-row codec in `lattice-lsl`). Grammar: [`formats/lsl-grammar-v0.md`](../formats/lsl-grammar-v0.md).

## LSN

Log sequence number. Position of a record in the [WAL](#wal). LSNs start at 1 and increase by exactly 1
across records and segments.

## MVCC

Multi-version concurrency control. The row store keeps versions of each row keyed by the change set that
wrote it, so readers see a consistent snapshot and `AS OF` queries can read an earlier one. Isolation is
snapshot isolation with first-committer-wins write conflict detection; see
[`formats/isolation.md`](../formats/isolation.md) and [the SQL subset](sql-subset.md).

## Planted bug

A deliberate defect compiled in behind a feature flag (`planted-bugs` in `lattice-core`, `Bug::ROW` for
rows) to prove that the simulator or a test can fail. Every new invariant gets one. See
[conventions](conventions.md).

## RFC

Request for comments. A proposal in [`rfc/`](rfc/README.md) for a format or wire change, with a
comment period, written before the matching [ADR](#adr) is accepted.

## Scrub

A pass that re-reads stored data and checks the invariants, reporting findings. A finding is mapped to
the invariant it really violates; physical damage is expected under bit-rot fault profiles and is not by
itself an invariant violation.

## Sans-I/O

A design where a protocol state machine takes bytes in and returns bytes and actions out, and does no
network or file I/O itself. The PostgreSQL session in `lattice-api` is built this way, so tests can drive
it without sockets.

## Simulator

The deterministic simulation harness (`lattice-sim`) that runs the kernel over fake I/O, clock and random
number generator under fault profiles, from a seed, and checks invariants. A failing seed reproduces
exactly. Real time, files and threads are banned in deterministic crates.

## Turn

A numbered span of exclusive writing by one session, recorded in the WAL as a TurnBegin record. A newer
turn [fences](#fencing) the older one. The architecture uses turns so a person can steer an agent and
the agent's late output is rejected rather than interleaved.

## Typestate

The pattern where the type of a transaction value encodes its step, so that calling a step out of order
does not compile. The kernel's transactions are built this way.

## Vacuum

Removal of row versions no snapshot can still read. After vacuum, a read older than the floor fails with
`SnapshotTooOld` and a [cursor](#cursor) below it with `CursorTooOld`.

## WAL

Write-ahead log. The append-only, checksummed (crc32c) log of segment files named by first LSN, with
record types ChangeSet, TurnBegin and Checkpoint. Format: [`formats/wal-v1.md`](../formats/wal-v1.md).
