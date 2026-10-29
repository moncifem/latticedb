# Format: change feed v1

Status: draft. Implemented by `lattice_core::feed`. No RFC or ADR has reviewed it yet and nothing writes
or reads it outside tests, so no frozen golden file exists.

The change feed turns each committed change set into one event per entity change. An event is addressed
by a cursor. This document specifies the cursor, the event encoding and the ordering rules. How a
change set payload is split into entity changes belongs to the engine that wrote it; the feed model takes
the list of changes as input.

## Cursor

A cursor is `(lsn, index)`:

* `lsn` is the LSN of the WAL record that holds the change set (`wal-v1.md`).
* `index` is the zero-based position of the entity change inside that change set, in payload order.

Cursors compare by `lsn`, then by `index`. The origin cursor `(0, 0)` precedes every event; no event has
LSN 0. A subscriber that has seen the event at cursor `c` resumes with the events strictly after `c`.

### Encoding (12 bytes)

```
lsn   u64 big endian
index u32 big endian
```

Big endian is deliberate: comparing the 12 bytes as unsigned strings gives the cursor order, so a store
can range-scan on the encoded form. Every other integer in this format is little endian, as in
`changeset-v1.md`.

A decoder rejects any length other than 12, and rejects LSN 0 with a non-zero index.

### Why the order is stable

* The log assigns LSNs in commit order, starting at 1 and increasing by exactly 1 across records and
  segments; recovery never reuses one (`wal-v1.md`). The feed therefore orders events exactly as the
  changes were committed, and the LSN is part of the durable record, so the same event has the same
  cursor before and after a restart.
* The index is a function of the change set payload alone, so every reader derives the same index for the
  same event.
* Records that produce no events (TurnBegin, Checkpoint, a change set with no entity changes) consume
  LSNs. Cursors therefore have gaps. A gap is not a loss; consumers compare cursors by value and never
  count them.

### Across a checkpoint, vacuum or log truncation

* A checkpoint is a WAL record with its own LSN. It does not renumber anything, so cursors before and
  after it compare as before.
* Row-store vacuum (`RowStore::vacuum`) prunes row versions in memory and does not touch the log, so it
  cannot change a cursor.
* Cursors are values, not log offsets. If a later release drops old log segments, the retained records
  keep their LSNs and a cursor into dropped history still compares correctly with every other cursor. It
  can no longer be served from the log, and a future subscription layer must report that explicitly
  rather than resume at a different position. No such truncation exists today, and this document does not
  specify the subscription error.

## Event encoding

```
version        u8 = 1
cursor         12 bytes, as above
set_len        u32 little endian: number of events the change set produced, at least 1
change_set     16 bytes (big endian ULID)
hlc            16 bytes: wall_ms u64 | counter u32 | node u32 (little endian)
author         16 bytes (big endian)
turn flag      u8: 0 none, 1 present; if present: session u64 | turn u64 (little endian)
entity kind    u32 little endian: table or collection of the entity
key length     u16 little endian, at most 4096, then that many key bytes
operation      u8: 0 delete, 1 upsert
               if 1: value length u32 little endian, at most 16777216, then that many value bytes
```

The provenance fields (`change_set`, `hlc`, `author`, turn) repeat on every event of a change set, so an
event can be consumed on its own. `set_len` lets a consumer see where a change set ends; the last event
of a set has `index = set_len - 1`.

### Decoder rules

A decoder must:

* check every length against the bytes that remain before slicing or allocating, and against the bounds
  above;
* reject a version other than 1, a turn flag other than 0 or 1, an operation other than 0 or 1, LSN 0,
  `index >= set_len`, and bytes after the last field;
* accept exactly the bytes the encoder produces (canonical form): there is one encoding per event, so
  decoding then encoding returns the input.

An encoder refuses keys and values above the bounds, an LSN of 0, and `index >= set_len`.

## Worked examples

All bytes are hexadecimal.

### Upsert in the middle of a change set

Event 1 of 3 of the change set logged at LSN 258 (0x0102), change set id `a0` repeated 16 times, HLC
`(wall_ms 5, counter 6, node 7)`, author 9, no turn, entity kind 2, key `"k"`, upsert of `"vv"`:

```
01                                  version
0000000000000102 00000001           cursor: lsn 258, index 1
03000000                            set_len 3
a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0   change set id
0500000000000000 06000000 07000000  hlc
00000000000000000000000000000009    author
00                                  no turn
02000000                            entity kind 2
0100 6b                             key length 1, "k"
01 02000000 7676                    upsert, value length 2, "vv"
```

The 80 bytes are the annotated rows joined without separators. The unit test
`upsert_event_matches_hand_written_bytes` checks them.

### Delete with a turn

Only event of the change set at LSN 1, id `01` repeated, zero HLC, author 0, turn `(session 0x11,
turn 0x22)`, kind 0, empty key, delete:

```
01
0000000000000001 00000000           cursor: lsn 1, index 0
01000000                            set_len 1
01010101010101010101010101010101    change set id
00000000000000000000000000000000    hlc
00000000000000000000000000000000    author
01 1100000000000000 2200000000000000   turn present
00000000                            entity kind 0
0000                                key length 0
00                                  delete
```

### Cursor order

| cursor | bytes |
|---|---|
| origin | `000000000000000000000000` |
| (1, 0) | `000000000000000100000000` |
| (1, 1) | `000000000000000100000001` |
| (2, 0) | `000000000000000200000000` |
| (258, 4294967295) | `0000000000000102ffffffff` |

Each row is bytewise smaller than the next. Two events in LSN 7 and a later event in LSN 9 sort as
`(7,0) < (7,1) < (9,0)`, whatever LSNs the skipped records used.

### Rejected inputs

| input | reason |
|---|---|
| cursor of 11 or 13 bytes | length |
| cursor `00..00 01` (LSN 0, index 1) | non-canonical origin |
| event with version `02` | unknown version |
| event with LSN 0 | no event is before the origin |
| event with index 2 and `set_len` 2 | index out of range |
| event with turn flag `02` | unknown flag |
| event with operation `02` | unknown operation |
| upsert value length `ffffffff` | above the 16 MiB bound |
| upsert value length 1000 with 2 bytes left | truncated; nothing is allocated |
| a valid event followed by `00` | trailing byte |

## Versioning and compatibility

The version byte is 1. A reader refuses other versions. A change adds a new version byte value and a new
section here; version 1 bytes never change meaning.

## Checksums

None. The feed is derived from WAL records, which carry their own CRC; a transport adds its own framing
and integrity checks.

## Golden files

None yet. They arrive with the RFC and ADR that freeze this format. The seeds in
`fuzz/seeds/feed_decode/` hold the independent byte vectors used by the unit tests and the fuzz target.
