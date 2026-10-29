# Format: guarded WAL admission v2

Status: draft, [RFC 0008](../docs/rfc/0008-wal-capability-admission.md) and proposed
[ADR 0022](../docs/adr/0022-wal-capability-admission.md). Neither encoding nor LCIX activation is accepted.

## Layout

The candidate header is 32 bytes: ASCII `LATCAP02` (8), little-endian u32 version 2, u16 length 32,
u16 reserved zero, u64 required capabilities, u32 reserved zero, then u32 CRC32C of the preceding
28 bytes. Candidate bit 0 is managed compound replay; zero/unknown bits refuse. Records keep the
unchanged v1 encoding and start at offset 32. Existing v1 headers/records remain byte-unchanged.

## Versioning and compatibility

Legacy readers refuse the distinct magic. Explicit engine support is required even in a kernel
that understands the header. Preflight the full chain before callbacks/repair, preserve cumulative
requirements, and sync guard file/directory before appending a protected record. A short newest
creation may be repaired only before any guarded record could be acknowledged. Check the RFC for
empty-tail promotion, rollback and capability propagation requirements. New engine activation is
not enabled by this implementation branch or by corpus inclusion.

## Checksums

Header CRC32C uses the same Castagnoli polynomial/init/final XOR as [WAL v1](wal-v1.md), over exactly
the first 28 bytes. Record CRC coverage remains unchanged. Corrupt complete headers refuse repair.

## Golden files

`corpus/wal-admission-v2-{header,compound,unknown-capability,unknown-version}.golden` and
`corpus/wal-admission-v1-{legacy,rows}.golden` are independently framed by
`tools/wal-admission/make_fixtures.py` using a separate bitwise CRC implementation. The compound
contains literal table 7 `items`, index 9 `by_value`, `(7,a)=x` and delete `(7,b)`; the v1 rows
fixture contains only the same LROW mutations. Decoder tests pin fields, exact bytes, corrupt-bit
and truncated-prefix refusal. The hosted job builds a frozen real old RowStore against its original
lockfile, checks the literal legacy row and guarded refusal, and compares all fixture filenames and
byte hashes before/after. Hosted evidence is pending until its exact-head job succeeds.
