# Format: WAL v1

Status: accepted (ADR 0007). Little endian everywhere.

## Layout

A log is a directory of segment files named `<first-lsn, 20 digits>.seg`. Each segment is:

```
segment header (16 bytes): magic "LATLOG01" (8) | format version u32 = 1 | reserved u32 = 0
record*
```

A record is:

```
payload length u32 | crc32c u32 | lsn u64 | type u8 | payload (length bytes)
```

* `crc32c` (Castagnoli, reflected polynomial 0x82F63B78, init and final xor 0xFFFFFFFF) covers, in order:
  the length (4 bytes), the lsn (8), the type (1) and the payload. The crc field itself is not covered.
* LSNs start at 1 and increase by exactly 1 across records and across segments.
* Maximum payload is 16 MiB; a larger length is corruption.
* Record types: `1` ChangeSet, `2` TurnBegin, `3` Checkpoint. Readers keep unknown types as opaque
  records, so adding a type does not break older readers.
* Checkpoint payload, 20 bytes little endian: `through_lsn` u64 (the LSN just before the checkpoint),
  `change_sets` u64 (change sets committed so far), `state_crc` u32 (CRC32C over the committed ids in
  ascending order, each 16 bytes, then the turn table in ascending session order, each entry
  `session` u64 and `turn` u64). A reader that understands checkpoints recomputes these from the
  records before it and refuses the log when they differ. It still reads the whole log.

## Recovery rule

Scan records from the start. At the first record that is incomplete, fails its checksum, or has an
unexpected LSN:

* in the last segment, if **no valid record with a higher LSN follows anywhere in the file**, the rest
  is a torn tail and is truncated; otherwise the log is corrupt and recovery refuses to continue;
* in any other segment the log is corrupt.

**Tightening (issue 441).** The frame at the first bad position counts as evidence too, so the rule
above is narrowed for one shape. A frame there that is complete and passes its checksum but carries the
wrong LSN (a gap, a repeat, a lower LSN, or a first record that does not match the segment name) is not
a torn write, which can only leave an incomplete or checksum-failing frame. It means acknowledged
records were lost or reordered, so recovery **refuses** even when no higher valid record follows and
the frame is the last one in the file, and never truncates it. Earlier text and earlier versions
truncated that case, silently losing the record. This is a change to the recovery rule, not to the
layout: no bytes change. `lattice scrub` reports it as mid-log damage.

### Operator procedure when recovery refuses

The error names the segment, the byte offset and both LSNs, for example
`segment <name> has a valid record with lsn 3 where 2 was expected at offset <offset>: refusing to truncate`.
Recovery never repairs this by itself.

1. Stop every process using the log and copy the whole log directory somewhere safe first.
2. Either restore the log from a backup that predates the damage, or accept the loss: truncate the named
   segment file at exactly the reported offset, which discards that record and everything after it in
   that segment. Do this only if losing those records is acceptable; the missing LSN was already
   acknowledged data that the storage layer lost.
3. Run `lattice doctor` / scrub on the result before restarting.

A `lattice` command that does this safely is tracked in issue 475 (cli: log repair command for refused
recovery).

### Known ambiguity (fail safe)

The "valid record follows" test inspects raw bytes, so an opaque payload that embeds a valid-looking
record with a higher LSN makes an otherwise repairable torn tail look like mid-log corruption. Recovery
then **refuses** rather than truncates: it can reject a repairable log but cannot silently drop data.
The scan only checksums candidates whose LSN and length are plausible, which bounds its cost. A
stronger framing (segment footers or a durable high-water mark) is a v2 candidate.

A newest segment shorter than its header is a torn creation and is rewritten. A segment whose first
LSN does not follow the previous segment's last LSN means a segment is missing: corrupt.

### Power loss that does not keep a prefix (issue 442)

`fsync` is the only ordering barrier, so a power cut may persist the unsynced bytes of a file page by
page in any order, or persist the file's new size before its data. Every acknowledged record was
fsynced, so these shapes only ever damage bytes that were never acknowledged. Recovery's behaviour
for each, derived from the rules above and pinned by the `non-prefix` simulator profile and the tests
in `crash_shape_tests.rs`:

| Shape | What the newest segment holds | Recovery |
|---|---|---|
| A later page persisted, an earlier one lost (zeros) | damage, then a complete valid record | **Refuses** (`damaged at offset N but valid records follow`) and leaves the file untouched. This is the fail-safe above: the log is repairable but recovery cannot prove it |
| Prefix of the batch, then zeros (zero-filled tail) | intact records, then zeros | Torn tail: truncated at the first bad frame |
| A new segment's header lost to a zero fill or garbage, 16 bytes or more | a complete but invalid header, no records | **Refuses** (`segment <name> header: ...`) and leaves the file untouched. No acknowledged record can be in it, because the header is fsynced before any record is accepted into the segment, but only a valid staged copy proves a rewrite, so recovery does not rewrite it |
| A new segment's header shorter than 16 bytes | a torn creation | Rewritten (unchanged) |

Recovery loses no acknowledged record in any of these. The two refusals leave a log that does not open
until an operator acts, which is the cost of the fail-safe; an automatic rewrite or a per-batch
footer is a v2 candidate and is not implemented. The operator procedure above covers the first
refusal (truncate at the reported offset). For the second, stop every process, copy the directory,
and remove the named segment file if it holds nothing but its header: the previous segment is
complete and the next append recreates the segment. Both procedures are exercised by tests, and the
simulator applies them from the refusal message alone before it checks that every acknowledged
record is still present. Reserved bytes of a version 1 header are not validated by the reader, so a
header torn only in its last four bytes still opens.

## Durability protocol

* **Open**: create the log directory and make each new component durable by `fsync` of its parent; take
  an exclusive lock (`<dir>/LOCK`) so a second writer is refused for the lifetime of the log.
* **Commit**: append a batch, `fsync` the segment, then acknowledge. The writer rejects a record larger
  than the maximum payload before it takes an LSN, so it never acknowledges what recovery would reject.
* **Rotate**: write the new segment's header, `fsync` the file, `fsync` the directory, and only then
  accept records into it.
* **Failure**: a failed write or `fsync` poisons the log until recovery.
* **Recovery**: after repairing a tail, recovery `fsync`s every segment and the directory **before** it
  returns anything as committed. Bytes that merely survived a process crash are visible but not
  necessarily on disk, and must not be acknowledged on the strength of being visible.

## Versioning and compatibility

The version is in the segment header. A reader refuses versions newer than it understands. New record
types may be added without a version bump; a changed layout needs v2, an RFC and a migration.

## Golden files

`corpus/wal-v1-basic.golden` is produced by an independent Python implementation of this document and
read by `lattice-core` in CI (byte-exact decode and re-encode).
