# ADR 0007: WAL and change set format v1

Status: accepted

## Context

The log format is the hardest thing to change later. It must detect torn writes and bit rot, tell a
torn tail from mid-log corruption, and be readable by an independent implementation.

## Decision

Segment files with a magic and version header; records with length, CRC32C (Castagnoli, covering
length, lsn, type and payload), contiguous LSNs; recovery truncates only a tail with no valid record
after it, otherwise refuses. The format is specified in `formats/wal-v1.md` and a golden file written by
an independent Python implementation is read by CI. CRC32C was chosen over xxh3 for hardware support
and well-known test vectors; it is a corruption detector, not a security primitive.

## Consequences

Bit rot in the very last durable record is indistinguishable from a torn tail and is truncated; only
replication (Phase 10) can recover it. Recovery cost is linear in log size until checkpoints land
(Phase 2). The tail test is conservative: an opaque payload embedding a valid-looking later record makes
recovery refuse (never silently drop). Its scan only checksums candidates with a plausible LSN and
length, which bounds the work. Recovery fsyncs what it adopts before returning it, and a log takes an
exclusive lock so two writers cannot interleave.

## Amendment (issue 441)

The decision text "recovery truncates only a tail with no valid record after it" is tightened. A
complete, checksum-valid frame with an unexpected LSN at the first bad position is itself evidence of
lost data, so recovery now refuses it even as the last frame, where it used to truncate it. Incomplete
and checksum-failing tails are still truncated. The layout is unchanged. Status of this ADR is
unchanged; the rule is in `formats/wal-v1.md`.
