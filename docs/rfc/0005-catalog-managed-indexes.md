# RFC 0005: Persist managed index definitions with atomic row changes

Status: accepted
Comment period ends: waived by the project owner on 2026-10-29
Decision ADR: ADR 0019

## Motivation

ADR 0014 derives index entries from committed rows but requires callers to supply extraction callbacks
again after every open. A callback cannot be persisted as code or a pointer. Default recovery must
recover declarative definitions, their visibility history and the rows they index from the same WAL.

The owner accepted RFC 0005 / ADR 0019 and explicitly waived this format's wait on 2026-10-29 after
independent review, the accepted WAL guard and bounded engine/frozen-reader proof disclosure. Merge
remains conditional on exact final-head green CI; [the decision record](../evidence/managed-index-owner-decision-2026-10-29.md)
preserves that narrow scope. Neither Phase 1 sign-off nor Phase 2 exit is implied.

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
remaining risks; this format does not silently impose a new row count or claim those risks resolved.

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
miss its row mutations. This integration requires the accepted RFC 0008 WAL guard;
RFC 0008 / ADR 0022 is accepted and merged in PR 222. This managed format was separately accepted
by the owner on 2026-10-29; mixed-version access remains unsupported.**
Changing only the engine magic or catalog version cannot fix an old kernel's unknown-payload policy.

The owner selected enforced old-reader refusal on 2026-10-29. Acceptance and LCIX activation now
require the separately reviewed, implemented and accepted WAL version/capability admission mechanism.
That prerequisite is supplied by accepted RFC 0008 / ADR 0022, merged in PR 222. Operational-only
coordinated upgrade was not selected; it cannot satisfy this prerequisite.
Do not point old clients at an upgraded directory. Upgrade all writers/readers together, take a
verified backup before the first LCIX write, retain the old binary and pre-upgrade backup, and restore
that backup for rollback. No downgrade converter or in-place downgrade is supplied. WAL is retained,
but retained bytes alone do not make an old reader understand a new compound payload.

The dormant typed/checkpoint format proposals remain independent, unaccepted and unchanged. This
work does not revive those branches or decide their formats. Composing managed definitions with a
future checkpoint requires a new engine tag and complete schema/index history; none is implemented.

## Evidence and remaining validation

The focused public-store tests include independently framed LCIX examples (with nested unchanged
LCAT/LROW), exact encoder output, all-prefix truncations, arbitrary inputs, and overflow/unknown-tag
refusal. Lifecycle tests cover synchronous build over existing rows, ordinary maintenance, historical
lookups, drop/recreate, compound rows plus metadata, metadata-only writers and reopen without callbacks.
A power-cut sweep uses every mutating-operation position observed in its completed control and checks
all-or-none publication, acknowledgements, exact rows, indexes and a second reopen.

A dedicated planted recovery defect omits the managed definitions/entries while retaining the catalog.
A hard-coded expected lookup and the catalog-driven scrubber detect the semantic mismatch. Existing
kernel/row/vacuum defect sets are unchanged; the registry uses u64 to give the new defect a distinct bit.

Independent review, the separately accepted WAL guard, exact frozen-reader provenance and finite
engine proof support the owner's format acceptance. Final-head CI remains required before merge.
The accepted format does not establish arbitrary-size or online-build safety: larger workload/soak
evidence, synchronous build/recovery cost and retained-state memory/admission-budget measurements
remain engineering work. Global fixture metadata pins the accepted bytes; structural corpus
integration alone did not supply owner acceptance.

## Accepted compatibility decision

The owner-required enforced-refusal mechanism is supplied by separate accepted RFC 0008. The existing
v1 segment header is `LATLOG01`, u32 version `1`, then a reserved u32. Old `check_header` validates
the version but ignores the reserved word. Therefore a reserved-word bit, unknown record type,
unknown ChangeSet payload or directory sidecar alone cannot stop an already shipped old reader.
Neither an LCIX magic change nor an engine-only marker provides enforcement.

Accepted RFC 0008 / ADR 0022 defines the separate guarded header and explicit required capabilities,
including managed compound replay. Its implementation is merged in PR 222. This RFC does not redefine
those header assignments; the required integration contract is:

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

The WAL admission mechanism is separately implemented and accepted in RFC 0008 / ADR 0022 and
merged through PR 222. PR 209 integrates it with the real row/catalog readers and compound writer.
Backup verification and restore-based rollback remain required; operational-only upgrade is not an
acceptance shortcut. On 2026-10-29 the owner separately accepted RFC 0005 / ADR 0019 and explicitly
waived this format's seven-full-day wait after independent review and bounded engine/frozen-reader
proof disclosure. The [narrow decision record](../evidence/managed-index-owner-decision-2026-10-29.md)
records that response. Merge still requires exact final-head green CI; no checkpoint format,
release or phase gate is accepted by this decision.

## Global accepted corpus

The independent examples are now in `formats/corpus/` with `.golden.toml` accepted metadata and the
[global layout specification](../../formats/catalog-index-v1.md). Engine tests consume those exact
bytes. The structural corpus check covers their metadata; codec tests pin the independent bytes and
literal semantics. The owner accepted these format bytes without changing WAL v1. The mandatory
WAL guard was accepted separately; neither corpus metadata nor a test result supplies acceptance by
itself. Compound row writes also retain their bound host policy from PR 215.
Follow-up (issues 234 and 307, awaiting review): `RowStore::commit_with_catalog` now asks the
transaction's `PolicyEvaluator::authorize_ddl` first, denied by default; `AllowAll` (so `begin()`)
grants it, and `RowStore::commit_catalog` and `CatalogStore::commit`/`commit_managed` remain
host-only paths without an actor. This is a store hook, not SQL catalog authorization.


## Accepted guard integration and actual-engine evidence

The initial integration used guard/measurement head `f9f0307b42766a01261670bc51a23b1dd88b5581`.
PR 222 has since merged accepted RFC 0008 / ADR 0022; this update rebases the existing PR 209 onto
actual main parent `6a500f8` rather than the former stacked branch. Managed RowStore and the catalog projection explicitly advertise `MANAGED_COMPOUND`;
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
with source/binary/toolchain hashes and before/after byte manifests. Hosted run 40916386232 passed
25 semantic tests and 15 finite engine histories / 30 old-reader probes at reviewed head `d0edc12`,
actual merge source `aa134abf47a15cd433375c0025514c13bd83db2e`; their trees are identical. There were
20 byte-unchanged guarded refusals, ten legacy probes and two permitted raw legacy repairs. See the
[compact proof/provenance](../evidence/managed-index-40916386232.md). The separate 4,648-history core
matrix is broader admission evidence, not 4,648 managed-index histories. Current replays require
fresh final-head checks; this engine sweep does not claim arbitrary workload or large-soak coverage.

The owner accepted the reviewed managed format and implementation and waived its own wait;
publication remains conditional on final-head green CI. This supplies no converter for earlier
unguarded draft directories and does not decide future checkpoint composition. Such draft
directories refuse and require restoration/recreation rather than silent adoption.
