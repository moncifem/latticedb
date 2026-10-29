# Format: guarded WAL admission v2

Status: accepted, [RFC 0008](../docs/rfc/0008-wal-capability-admission.md) and
[ADR 0022](../docs/adr/0022-wal-capability-admission.md). The owner accepted this WAL admission format
and waived its wait on 2026-10-29. LCIX format acceptance/hold and engine activation remain separate.

## Layout

The header is 32 bytes: ASCII `LATCAP02` (8), little-endian u32 version 2, u16 length 32,
u16 reserved zero, u64 required capabilities, u32 reserved zero, then u32 CRC32C of the preceding
28 bytes. Bit 0 is managed compound replay; zero/unknown bits refuse. Records keep the
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
`corpus/wal-admission-v1-{legacy,rows}.golden`, wrong-length/reserved/bad-CRC examples, a
capability-drop chain (201-byte guarded segment followed by a 16-byte legacy header), and all 32
strict guarded-header prefixes are independently framed by
`tools/wal-admission/make_fixtures.py` using a separate bitwise CRC implementation. The compound
contains literal table 7 `items`, index 9 `by_value`, `(7,a)=x` and delete `(7,b)`; the v1 rows
fixture contains only the same LROW mutations. Decoder tests pin fields, exact bytes, corrupt-bit
and truncated-prefix refusal. The hosted job builds a frozen real old RowStore against its original
lockfile, checks the literal legacy row and guarded refusal, then checks every exported raw/recovered
fault-matrix snapshot with before/after filename and byte-hash manifests. Guarded refusal and
recovered legacy opens must not mutate bytes; permitted raw legacy-tail repair is recorded. Completed source-specific hosted evidence and limits are recorded in
[the compatibility/measurement report](../docs/evidence/wal-admission-40908208450.md). The accepted
WAL fixture metadata alone did not accept its nested LCIX illustration. The owner separately
accepted RFC 0005 / ADR 0019 on 2026-10-29; their reviewed engine integration and current-head checks
remain separate from this header fixture.
