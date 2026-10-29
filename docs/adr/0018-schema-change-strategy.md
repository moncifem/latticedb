# ADR 0018: Online schema changes through shadow generations

Status: proposed

Requirement P2-16. Design discussion: [RFC 0004](../rfc/0004-online-schema-change.md).
This ADR is not accepted; implementation and verification remain pending.

## Context

Add-column, drop-column and index builds must keep accepting writers while existing data is
converted. Current main has versioned catalog names, MVCC row history/vacuum, a typed-row codec,
LSL migration lenses and derived runtime-supplied indexes. Persisted writer-schema dispatch,
catalog-managed index definitions, durable migration state and pin coordination are not yet
implemented. A live scan plus a catalog rename would permit concurrent writes or deletes to be
lost, old backfill values to overwrite newer values, or vacuum to remove a worker's source history.

The kernel commits one opaque payload per change set. Independent control and row commits cannot
atomically establish dual writes, persist a frontier with the copied data, or publish a cutover.
A suitable versioned compound envelope requires a separate format/compatibility review. The
pending checkpoint proposals are not accepted or selected by this decision.

## Decision

Propose the protocol in RFC 0004, subject to independent design review and its implementation
prerequisites. The owner waived the merge waiting period on 2026-10-29; this ADR remains proposed:

- Prepare a hidden target generation with canonical checked schema/lens descriptions. Log the
  source snapshot, target ids, migration pin and dual-write rule in one atomic writer step.
- Keep source authoritative during bounded backfill. User writes, deletes and derived index
  changes update both sides in the same user change set. Schema epochs are rechecked at commit.
- Install base-snapshot candidates conditionally by their source version. A later backfill
  publication LSN must never override a newer dual-written source version or tombstone.
- Commit copied data and a stable exclusive-key frontier together. Replay resolves ambiguous
  outcomes and restores the pin/rule before writers or vacuum resume.
- Verify complete transformed row/index equivalence at a pinned barrier; continue dual writes,
  then atomically publish one target epoch. Old-schema writers retry and old snapshots retain
  the source generation. Retire old objects only after reader/retention/branch pins allow it.
- Before cutover, abort through a new logged transition. After cutover, rollback requires a new
  migration; a lossy drop-column lens and a frozen source are not an automatic reverse path.

Add-column defaults must be deterministic and admitted by the checked target schema. Drop-column
retains historical bytes/layouts for old snapshots. Initial index backfill covers non-unique
indexes; unique admission and a reverse-dual-write rollback window remain explicit open questions.
Memory accounting, cancellation, snapshot pins and stale-plan checks require actual executor/store
integration; exposing primitives alone does not enforce this protocol.

## Consequences

The design avoids holding a writer lock for the full scan, at the cost of temporary target storage,
dual-write work, durable progress metadata and retained source history. Preparation, batch commits
and cutover still serialize with writes; their latency and resource ceilings need measurements.
Long-lived pins can delay vacuum and retirement, so their cost must be visible and admission bounded.

Acceptance requires the independent map-model, fault-cut, planted-defect, exact-format compatibility
and hosted memory/latency evidence specified in RFC 0004. No such online-schema-change evidence is
claimed here. The RFC introduces no encoded format and this ADR does not authorize format changes,
accept pending schema/checkpoint proposals, close WP31, or sign a phase gate.
