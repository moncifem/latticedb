# Format: catalog-managed index compound changes v1

Status: accepted, RFC [0005](../docs/rfc/0005-catalog-managed-indexes.md), ADR
[0019](../docs/adr/0019-catalog-managed-indexes.md). The owner accepted these bytes and waived this
format's wait on 2026-10-29; exact final-head green CI is still required before PR 209 merges.
Old-reader refusal depends on accepted WAL admission and actual engine capability/context checks.

## Layout

All integers are unsigned little endian. The entire payload is a ChangeSet body under the existing
WAL byte limit. The enclosing ChangeSet adds overhead, so a payload at that limit can still be
rejected by the kernel. Fields occur in this order:

| Field | Encoding |
|---|---|
| Magic/version | Four ASCII bytes `LCIX`, then byte `1` |
| Table block | u32 byte length, then canonical [LCAT v1](catalog-v1.md) bytes; zero omits block |
| Index operations | u32 count, 0 through 4096, then operations below |
| Row block | u32 byte length, then canonical [LROW v1](rowops-v1.md) bytes; zero omits block |

Create operation: byte `1`, u32 index id, u32 table id, u32 name byte length, UTF-8 name,
then extraction expression. Drop operation: byte `2`, u32 index id. Names contain 1 through
255 UTF-8 bytes. Expression byte `0` means WholeValue; byte `1` means Slice and is followed by
u32 offset and u32 length. Overflowing offset + length is invalid. Short values have no entry;
a zero-length slice yields the empty key if its offset fits. Expressions address physical bytes.

At least one table or index operation is required. A nonzero nested table block must declare
1 through 4096 operations; its header count is checked before allocating decoded operations.
An encoded empty LROW block is refused; omit it with length zero. Nested row mutations retain the
legacy byte-bounded decoder without a 4096-row cap. Allocation amplification remains unresolved.
Reject unknown tags/versions, noncanonical empty blocks, invalid names, lengths beyond the remaining
bytes, overflow, and trailing bytes. Ordinary row-only writes remain LROW v1.

## Versioning and compatibility

LCAT v1 and LROW v1 remain unchanged. LCIX does not claim the reserved, unaccepted typed-catalog v2.
Index ids cannot be reused, including invisible create/drop within one ChangeSet. Names are unique
within each live table. Final live indexes must reference final live tables. Atomic catalog and row
publication preserves historical definitions and entries; callback mode refuses LCIX on reopen.

Older RowStore binaries silently skip unguarded LCIX, including its rows. The writer must request
capability 0x01 from the accepted RFC 0008 / ADR 0022 WAL guard before appending LCIX. Managed readers
declare actual support and refuse unguarded LCIX; legacy callbacks remain at zero support. The guard
is implemented and merged through PR 222. Corpus inclusion alone does not provide this enforcement
or permit mixed-version access. The owner separately accepted RFC 0005 / ADR 0019
and waived its wait; [the decision record](../docs/evidence/managed-index-owner-decision-2026-10-29.md)
preserves that scope.

## Checksums

No additional checksum is embedded in LCIX. The existing ChangeSet/WAL framing provides its CRC.
Malformed or semantically invalid CRC-valid LCIX refuses recovery in the new reader.

## Golden files

The hand-framed little-endian examples were produced independently of the Rust codec. Public codec
tests decode literal expected values and require exact encoder equality to these bytes:

| Corpus file | Literal contents |
|---|---|
| [catalog-index-v1-compound.golden](corpus/catalog-index-v1-compound.golden) | 96 bytes: create table 7 `items`, create index 9 `by_value` WholeValue, put `(7,a)=x`, delete `(7,b)` |
| [catalog-index-v1-drop.golden](corpus/catalog-index-v1-drop.golden) | Drop index 9; no table or row block |
| [catalog-index-v1-slice.golden](corpus/catalog-index-v1-slice.golden) | Create index 12 `prefix` on table 7, Slice offset 1 length 2 |
| [catalog-v1-table.golden](corpus/catalog-v1-table.golden) | Unchanged 23-byte LCAT v1 table-7 `items` create, also nested in compound example |

Each has a `.golden.toml` companion with format, version and explicit accepted status. Files are retained
as append-only accepted examples; a later incompatible format needs separately named fixtures.
These samples complement all-prefix truncation, arbitrary-input and semantic recovery tests. They do
not prove safe downgrade, bounded allocator usage, long-running soak, or arbitrary-size build cost.
