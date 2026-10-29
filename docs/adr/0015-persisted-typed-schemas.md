# ADR 0015: Persist schemas with typed rows' writer-layout versions

Status: proposed

## Context

P2-01/P2-02 require catalog-owned typed tables and durable layouts. Typed rows previously needed a
caller-supplied layout; replaying old bytes with reordered columns or enum variants could change their
meaning. RFC 0001 proposes the format extension and remains in its required comment period.

## Decision

Proposed, not accepted: persist canonical checked LSL, immutable schema versions and stable per-type
table ids in LCAT v2. Use one RowStore WAL/kernel for catalog and rows. Typed row envelopes reference
the writer-schema version, so restart and historical reads recover the original interpretation.
Reserve physical ids from all row history, including tombstones; validate typed values before append
and fence writing transactions if the catalog changed. Keep legacy v1 catalog encoding unchanged.

Do not merge the format implementation or mark this ADR accepted before the RFC review period ends
and its compatibility and safety findings are resolved.

## Consequences

Old rows remain readable under their writer constraints without requiring caller layouts at reopen.
New schemas do not implicitly migrate old data. The draft catalog/envelope formats, independent
Python golden files, semantic replay refusal tests and power-cut regressions provide review evidence.
The raw-history id-collision found during independent review has a fix and regression coverage.

There is no safe automatic downgrade, online backfill or completed Phase 2 gate. Existing persisted
managed indexes and vacuum are integrated through the shared writer; they are not online migration.
The RFC records backup/rollback restrictions and mandatory capability refusal by older consumers.

Schema validation is enforced at both `TypedStore` and the shared `RowStore` commit/replay boundary.
Raw tables keep their byte API. Proposed typed capability 4 is required for schema adoption and typed
writes; default core-only, catalog-only and callback readers refuse upgraded history before replay.
A deliberately lying explicit-capability consumer can still inject invalid data; engine recovery
validates its typed semantics rather than trusting the advertised bit alone.

## Resumed proposal integration

The owner authorized integration of the preserved proposal with guarded WAL and managed indexes;
this does not accept the format or waive the comment period. RFC 0001 now proposes a distinct typed
capability and an explicitly separate LCIX v2 nested-schema extension. The earlier core-only/old-client
escape is a compatibility finding to resolve with enforced admission, not an accepted limitation.
The integration must preserve the original typed behavior and prove frozen-reader refusal.
