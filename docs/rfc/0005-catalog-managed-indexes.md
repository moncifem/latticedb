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

**Older RowStore binaries skip unknown engine payloads, including LCIX. They may silently miss its
row mutations. This draft does not provide automatic old-reader refusal or mixed-version safety.**
Changing only the engine magic or catalog version cannot fix an old kernel's unknown-payload policy.

Before accepting the format, the owner must choose and review either an enforced kernel/segment
capability mechanism or an operational coordinated-upgrade restriction. That choice is unresolved.
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
Before acceptance: independent review; exact-head required CI; a recorded compatibility decision;
freeze-corpus integration under the accepted format policy; larger fault/soak evidence; and measured
build/recovery cost and retained-state memory. Current test fixture files live with the engine tests,
within this task's scope, and do not replace the repository-wide format freeze process.
