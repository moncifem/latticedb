# RFC 0005: Persist managed index definitions with atomic row changes

Status: draft
Comment period ends: 2026-11-05 (at least seven full days after this RFC's PR opens)
Decision ADR: proposed ADR 0019

## Motivation

ADR 0014 derives index entries from committed rows but requires callers to supply extraction callbacks
again after every open. A callback cannot be persisted as code or a pointer. Default recovery must
recover declarative definitions, their visibility history and the rows they index from the same WAL.

This proposal is unaccepted. Independent format review, the full comment period, compatibility policy
and the evidence below must finish before merge. Neither Phase 1 sign-off nor Phase 2 exit is implied.

## Proposal

Retain LCAT v1 and LROW v1 byte encodings unchanged. Do not reinterpret LCAT v2, reserved by the separate
unaccepted typed-catalog proposal. A ChangeSet whose payload starts `LCIX` uses this independent v1
compound encoding. All integer fields below are little endian:

1. Four bytes `LCIX`, then version byte 1.
2. u32 byte length and canonical LCAT v1 table operations; length zero means no table operations.
3. u32 index-operation count, at most 4096. A create is tag 1, u32 index id, u32 table id, u32 UTF-8
   name length, name bytes, then an extraction expression. A drop is tag 2 and u32 index id.
4. u32 byte length and canonical LROW v1 mutations; length zero means no rows. An encoded empty LROW
   block is refused as a noncanonical alternate spelling.

At least one table or index operation is required. Ordinary row-only transactions continue to emit
LROW v1. Total compound payload is bounded by the existing WAL payload limit; the surrounding
ChangeSet adds its own overhead and the kernel may reject a payload that exceeds the record limit.
Counts, names, versions, expression tags, lengths, overflow and trailing bytes are checked strictly.
Names are nonempty and at most 255 UTF-8 bytes. The nested LCAT header's table-operation count is
checked against 4096 before decoding its operation values. This restriction belongs to LCIX nesting;
standalone legacy LCAT decoding is unchanged. Active definitions are bounded to 4096. Used index ids,
catalog versions and historical rows remain retained derived state and can grow.

Nested LROW mutations do not inherit the 4096 metadata-operation limit. They retain the existing
byte-bounded row decoder, whose in-memory vectors, allocation capacity and decoded values can exceed
the encoded byte size. The record-byte ceiling alone therefore does not establish a peak allocator
bound. A separate row-count/admission-budget policy and measured allocation amplification remain
acceptance risks; this draft does not silently impose a new row count or claim those risks resolved.

The initial expressions are tag 0 `WholeValue` (all physical value bytes) and tag 1 `Slice` followed
by u32 offset and length. A slice with an overflowing end is invalid. A value shorter than the end
has no index entry; a zero-length slice has the explicitly empty key when its offset fits. These
expressions interpret bytes, not an assumed typed-row schema. A typed-field expression would need
explicit writer-layout dispatch and a separately reviewed format extension.

`Catalog` retains definitions by commit LSN. Index ids are never reused, even after dropping an index
or creating and dropping one within a single change. A live name is unique within its table. A name
can be recreated with a new id. All final live indexes must reference final live catalog tables;
removing an indexed table requires dropping its indexes in the same compound change.

`RowStore::commit_with_catalog` checks the transaction's row conflicts, vacuum floor and all metadata
before appending one ChangeSet. A new definition synchronously builds entries from currently visible
rows; the same transaction's writes then update those entries. Drops create entry tombstones.
Rows, index entries and definition history publish at the one durable LSN. An ambiguous WAL failure
makes the store stale until reopen; a post-commit apply failure also prevents continued use.

This synchronous build can block the caller and uses temporary memory proportional to the entries
being built. It is not an online index build. RFC 0004 separately proposes online backfill, durable
frontiers, pinning and cutover. This RFC introduces no background worker or checkpoint interpretation.

Default `RowStore::open` folds legacy table changes, compound changes and ordinary row changes in
one streaming pass. It rebuilds entries without caller callbacks. `CatalogStore` folds the compound
metadata too; its metadata-only API does not expose a live row index. Its committed definitions are
built when a RowStore next replays the log. A nonempty `open_with_indexes` callback list is explicit
legacy mode and refuses any LCIX compound payload; it cannot silently override persisted definitions.

Index maintenance preserves old entry versions. Lookup and consistency checks at an older readable
snapshot use the definitions visible then. Vacuum still determines the row/index floor; this proposal
does not prune catalog history or kernel deduplication ids. The index checker computes expected entries
from the authoritative catalog at the requested LSN, not from the index registry being checked.
Before comparing entries it also checks current and historical effective definitions against the
catalog, so an empty table cannot hide a missing registry that would omit later write maintenance.

## Invariants and recovery

- I2/I3: a compound ChangeSet is adopted or absent as a complete unit. Unacknowledged but complete
  durable changes may be adopted. No acknowledged row or definition change may vanish.
- I9: recovered entries, including primary-key values, match catalog-defined extraction from rows.
  A missing rebuilt definition must not erase the checker's expected result with it.
- First-committer-wins and expired-snapshot checks run before WAL append. Invalid metadata leaves
  both WAL bytes and visible state unchanged.
- Retried recovery reproduces the same row/catalog/index histories. Dropped names never cause an old
  index id to mean a new extraction expression.
- CRC-valid payloads claiming LCIX but failing its parser or semantic metadata checks refuse open;
  they are not silently skipped as unknown row data by the new reader.

## Alternatives

Persisting function pointers is invalid across processes and unsafe. Requiring callers to guess the
old definition on every open preserves the original recovery hazard. Reusing LCAT v2 collides with
an existing unaccepted proposal. Separate metadata and row ChangeSets cannot express one atomic
metadata-plus-row publication. An online build state machine is broader work tracked by RFC 0004.

## Compatibility and migration

**Older RowStore binaries skip unknown engine payloads, including unguarded LCIX. They may silently
miss its row mutations. The stacked implementation now requires the draft RFC 0008 WAL guard;
neither format is accepted, and mixed-version access remains unsupported.**
Changing only the engine magic or catalog version cannot fix an old kernel's unknown-payload policy.

The owner selected enforced old-reader refusal on 2026-10-29. Acceptance and LCIX activation now
require a separately reviewed, implemented and accepted WAL version/capability admission mechanism.
Operational-only coordinated upgrade was not selected; it cannot satisfy this prerequisite.
Do not point old clients at an upgraded directory. Upgrade all writers/readers together, take a
verified backup before the first LCIX write, retain the old binary and pre-upgrade backup, and restore
that backup for rollback. No downgrade converter or in-place downgrade is supplied. WAL is retained,
but retained bytes alone do not make an old reader understand a new compound payload.

The dormant typed/checkpoint format proposals remain independent, unaccepted and unchanged. This
work does not revive those branches or decide their formats. Composing managed definitions with a
future checkpoint requires a new engine tag and complete schema/index history; none is implemented.

## Evidence and remaining acceptance

The focused public-store tests include independently framed LCIX examples (with nested unchanged
LCAT/LROW), exact encoder output, all-prefix truncations, arbitrary inputs, and overflow/unknown-tag
refusal. Lifecycle tests cover synchronous build over existing rows, ordinary maintenance, historical
lookups, drop/recreate, compound rows plus metadata, metadata-only writers and reopen without callbacks.
A power-cut sweep uses every mutating-operation position observed in its completed control and checks
all-or-none publication, acknowledgements, exact rows, indexes and a second reopen.

A dedicated planted recovery defect omits the managed definitions/entries while retaining the catalog.
A hard-coded expected lookup and the catalog-driven scrubber detect the semantic mismatch. Existing
kernel/row/vacuum defect sets are unchanged; the registry uses u64 to give the new defect a distinct bit.

The code is a draft implementation, not acceptance evidence for arbitrary data sizes or online builds.
Before acceptance: independent review; exact-head required CI; implemented and independently accepted WAL admission;
final freeze review under the accepted format policy; larger fault/soak evidence; and measured
build/recovery cost and retained-state memory. Independent proposal fixtures now live in the global corpus as described below; structural
integration does not constitute format acceptance.

## Concrete compatibility decision required before acceptance

The owner-required enforced-refusal mechanism needs a separate WAL admission proposal. The existing
v1 segment header is `LATLOG01`, u32 version `1`, then a reserved u32. Old `check_header` validates
the version but ignores the reserved word. Therefore a reserved-word bit, unknown record type,
unknown ChangeSet payload or directory sidecar alone cannot stop an already shipped old reader.
Neither an LCIX magic change nor an engine-only marker provides enforcement.

A concrete candidate is a new segment-header version with explicit required capabilities, including
managed compound changes. No version number, bit assignment or revised header bytes are reserved or
implemented by this RFC. A separate WAL RFC/ADR must define and independently review them. Its minimum
contract is:

1. A writer must install a guarded segment and durably sync its header and directory entry before
   appending its first LCIX record. Nonempty v1 segments remain byte-unchanged; only an empty tail
   may be promoted in place under RFC 0008's crash protocol. Every later segment
   in that upgraded history retains the guard, including segments containing ordinary LROW writes.
2. A reader must reject an unsupported header version or required capability before interpreting,
   repairing or truncating that segment. It must not expose a partially replayed RowStore. A frozen
   pre-upgrade reader must reject a complete guarded directory and leave its bytes unchanged.
3. Rotation/header-sync/directory-sync/LCIX-append/commit-sync failures need an exhaustive bounded
   power-cut matrix. A durable LCIX record must never survive in an unguarded segment. A durable
   guarded empty segment may conservatively prevent downgrade even before any LCIX write succeeds.
   A torn guard creation may be repaired only when no guarded record could have been acknowledged.
4. Normal rollover, reopen, callback readers, metadata-only CatalogStore, raw kernel tools, retained
   histories and future checkpoint recovery must agree on admission. Missing guarded segments and
   unknown capabilities must refuse; capability support cannot be inferred merely from a decoder
   accepting the header. Required support must reach the actual engine reader.
5. Independent fixtures must include legacy headers, complete supported guarded headers, unknown
   versions/capabilities, every truncated guarded-header prefix, and a full guarded LCIX history.
   Compatibility tests must run the frozen old reader, not a new-reader imitation. Future header
   extension bytes must never be silently treated as record data.

This is broader than an engine-only compatibility fix and is intentionally not implemented here.
The owner explicitly selected separately reviewed WAL admission before LCIX activation on
2026-10-29 and rejected operational-only upgrade. The operational alternative is retained above
for design history, not as an available acceptance shortcut. Backup verification and restore-based
rollback still apply alongside the mandatory guard. This selection accepts no header bytes, feature
bits, checkpoint format, release or phase gate. The seven-full-day hold remains in force; no waiver
was given. This PR stays draft until the separate guard is implemented and accepted, the hold ends,
and independent format review plus exact-head required checks pass.

## Global proposal corpus

The independent examples are now in `formats/corpus/` with `.golden.toml` draft metadata and the
[global layout specification](../../formats/catalog-index-v1.md). Engine tests consume those exact
bytes. The structural corpus check covers their metadata; codec tests pin the independent bytes and
literal semantics. This supplies repository-wide proposal fixtures without changing WAL v1 or
promoting RFC 0005/ADR 0019 to accepted. Final format acceptance and the selected mandatory WAL admission implementation
remain separate requirements. Compound row writes also retain their bound host policy from PR 215;
metadata APIs remain privileged host operations and do not implement SQL catalog authorization.


## Stacked guard integration and actual-engine evidence

PR 209 is stacked on the exact guard/measurement head `f9f0307b42766a01261670bc51a23b1dd88b5581`
from PR 222. Managed RowStore and the catalog projection explicitly advertise `MANAGED_COMPOUND`;
nonempty legacy callback opens advertise zero support. The streaming callback receives the real
segment recovery context and refuses any LCIX record lacking capability mask `0x01` before applying metadata or rows.
Compound commits bind that bit to `Kernel::commit_with_capabilities` after the existing bound actor
policy, conflict, floor and metadata checks, and before append. Ordinary LROW/LCAT writes preserve
their bytes and keep the Log's cumulative capability when a prior upgrade requires it.

The engine consumes the independent global guarded LCIX fixture and verifies literal rows, index
definitions and historical entries. Independent regressions strip only the header from that fixture
to prove valid unguarded LCIX refuses without repair; callback and raw zero-support readers refuse
guarded histories, while unchanged legacy rows remain readable. Mixed legacy LCAT/LROW replay also
retains callback table lookup after empty managed-definition versions; an independent regression
caught and corrected the missing callback fallback. A clean/mutated planted guard-omission
witness exercises actual engine commits. Existing drop/recreate, atomic publication, historical
policy and malformed-payload checks now run through the guarded path.

A bounded first-upgrade cut sweep covers empty-tail promotion and nonempty legacy-prefix rotation,
requiring ACK survival, all-or-none row/catalog/index publication, retained historical prefix reads,
zero-support refusal whenever a guard persists, and stable second recovery. Hosted-only export
passes every raw/recovered pair to the actual frozen `743b0fb` reader using its original lockfile,
with source/binary/toolchain hashes and before/after byte manifests. Those hosted results remain
pending until exact-head completion. Core-level partial-append/ENOSPC/fsync/rollover evidence remains
in PR 222; this engine sweep does not claim an exhaustive arbitrary workload or large soak.

This integration does not activate LCIX on main, accept either format, waive either review hold,
provide a converter for earlier unguarded draft directories, or decide future checkpoint composition.
Such draft directories now refuse and require restoration/recreation rather than silent adoption.
