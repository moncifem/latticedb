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

### Known ambiguity (fail safe)

The "valid record follows" test inspects raw bytes, so an opaque payload that embeds a valid-looking
record with a higher LSN makes an otherwise repairable torn tail look like mid-log corruption. Recovery
then **refuses** rather than truncates: it can reject a repairable log but cannot silently drop data.
The scan only checksums candidates whose LSN and length are plausible, which bounds its cost. A
stronger framing (segment footers or a durable high-water mark) is a v2 candidate.

A newest segment shorter than its header is a torn creation and is rewritten. A segment whose first
LSN does not follow the previous segment's last LSN means a segment is missing: corrupt.

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
