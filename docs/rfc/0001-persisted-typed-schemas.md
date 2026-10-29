# RFC 0001: Persisted typed schemas and original-layout row decoding

Status: draft
Comment period ends: 2026-11-05
Decision ADR: proposed ADR 0015 (not accepted)

## Motivation

P2-01/P2-02 need typed physical tables whose layouts survive restart. A caller-supplied layout can
change column or enum ordering and decode a valid old value incorrectly. The catalog and row engines
also need one WAL owner so a schema installation has one durable outcome.

This PR remains open for at least seven days under the repository RFC process. The implementation is
reviewable branch code, not an accepted format or a Phase 2 exit claim. The Phase 1 gate stays open.

## Proposal

- Extend catalog payloads with LCAT v2 DefineSchema: canonical checked LSL, a never-reused schema
  version and an exact sorted mapping from every schema type to a stable physical table id.
- Retain byte-identical LCAT v1 writes for legacy create/rename/drop operations. Readers accept both.
- Replay catalog and row change sets through RowStore's one kernel/WAL writer. Validate catalog
  semantics before append and during recovery; malformed known catalog payloads stop recovery.
- Use one typed physical table per type, not an entity/attribute/value table. Reserve all physical
  table ids found in row history, including deleted and previously uncataloged raw tables.
- Store typed values as LTYP v1 envelopes carrying the immutable writer-schema version. Decode with
  the original layout and validate stored values against its constraints, rather than the newest one.
- Fence writing transactions when the catalog changes after begin; preserve read-only snapshots.
  Bind typed transactions to their store/recovery generation.

The existing typed-cell codec remains unchanged. Full layouts and constraints are persisted as checked
LSL plus their deterministic compilation, not serialized Rust implementation structs. Independent
Python goldens pin the catalog payload and row envelope. See the draft formats in `formats/`.

## Alternatives

Keeping layouts outside the log makes restarts depend on out-of-band state and cannot preserve old
row interpretation. Rewriting all old values during schema install makes the change an online
migration, with backfill/dual-write obligations not solved by this slice. Generic EAV storage does not
meet P2-02. Serializing compiler structs would make private Rust changes an accidental disk format.

## Compatibility and migration

The WAL record and row-operation outer framing remain unchanged. New full row engines accept LCAT v1
and proposed v2 and validate every typed row at commit and historical replay. Proposed capability
`TYPED_SCHEMA = 4` (bit 2) is distinct from accepted `MANAGED_COMPOUND = 1` (bit 0); bit 1/value 2
remains unknown/reserved so the accepted negative admission fixture retains its exact meaning.

Standalone LCAT v2 requires capability 4. Accepted LCIX v1 retains nested LCAT v1 only. Proposed
LCIX v2 allows nested LCAT v2 and requires capability 5 (typed plus managed). LCIX v1 containing
writes to typed tables also requires capability 4 in addition to its managed bit. LROW writes to
typed tables require capability 4. The actual segment context is checked during replay, not inferred
from a decoded operation alone. Legacy LCAT v1/LROW/LCIX v1 bytes are not rewritten.

The default RowStore advertises capabilities 1 and 4 only with its real schema decoder, raw/compound
write validators and historical replay validators in place. A nonempty callback list still advertises
zero. CatalogStore remains a metadata projection supporting only capability 1; it refuses typed history
and typed schema writes because it does not validate typed row semantics. Default core-only opens
advertise zero and cannot reopen upgraded directories. A deliberately lying explicit-capability engine
can still inject CRC-valid semantic corruption; historical validation refuses it. Admission is a
contract with trusted consumers, not a security boundary against arbitrary byte-writing processes.
There is no automatic downgrade.

Only disposable pre-alpha test data should be used during review. Before later adoption, retain a
consistent backup readable by the old binary. Rollback restores that backup; it does not reinterpret
LTYP values or erase catalog records. Unknown envelope versions, writer schemas and invalid known
payloads fail rather than selecting a guessed layout. A downgrade converter and server-level
capability negotiation are not implemented here.

Schema installation does not backfill rows or synthesize newly required fields. Online migration,
layout changes with a dual-write window and checkpoint-based recovery remain separate work. Current
managed indexes and vacuum must coexist with typed history; they do not implement online migration.

## Resumed integration proposal (2026-10-29)

The owner authorized resuming and integrating the existing proposal, not accepting its formats or
waiving review. Preserve the original six commits for provenance: `035038e`, `acc70f5`, `48012cb`,
`515767f`, `6ea7a49`, `0d7cc69`. The original branch and worktree remain untouched.

The integration targets the current accepted guarded-WAL and managed-index implementation. It
proposes a distinct typed-schema capability at bit 2 (value 4); accepted managed compounds keep
bit 0 (value 1). Typed schema adoption and typed writes must require the new capability. Only an
engine that actually decodes and validates typed schemas and rows may advertise it; legacy callback
mode remains at zero. Default core-only and catalog-only consumers must refuse typed history.

LCAT v1 and accepted LCIX v1 bytes/meaning stay unchanged. LCIX v1 continues to nest only LCAT v1.
A separately proposed LCIX v2 permits a nested LCAT v2 schema change and requires both capabilities.
Standalone LCAT v2 and LTYP row writes require the typed capability. Typed validation covers raw
row commits, compound commits and historical replay through RowStore's one kernel writer.

Independent fixtures and an actual frozen `45926c7fc1f6fa71a7a69ac858c805ab1d9679b0` reader
must establish refusal before callbacks or repair; source implementation alone is not execution
evidence. This is planned integration work until its tests and hosted proof are recorded. Existing
writer-layout dispatch, id reservation, schema fencing, policy checks and strict HLC behavior must
remain intact. This does not add the complete SQL type system or select a checkpoint format.
