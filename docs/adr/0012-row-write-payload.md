# ADR 0012: Row writes travel in the change set payload

Status: accepted

## Context

The kernel treats a change set's payload as opaque bytes. The row engine needs to turn a committed change
set into row writes the same way on every replay: after a crash, from the log alone, with no side channel.
Several engines will share one log, so each must be able to recognise its own payloads and ignore the rest.
Whatever the encoding is, it becomes part of the on-disk format as soon as a log is written with it.

## Decision

A change set that writes rows carries a payload described in `formats/rowops-v1.md`: the magic `LROW`, a
version byte, a write count, then for each write a table id, a key and either a delete or a put with a value.
Other engines will use other magics, so `is_row_payload` is a four byte check and recovery skips payloads
that belong to someone else.

Table ids are numbers, not names, so renaming a table in the catalog never rewrites history. Keys and
values are opaque bytes at this layer; their meaning comes from the catalog.

Decoding is strict. Truncation, an unknown version or kind, and bytes after the last write are errors, and
an error never turns into a different set of writes. A change set whose row payload does not decode stops
recovery with a corruption error rather than being skipped, because skipping it would silently lose
acknowledged writes.

Applying a change set is one step at one LSN, so a snapshot sees all of its writes or none (I3).

## Consequences

- The version byte lets the format change while old logs stay readable; until the Phase 2 gate the format is
  a draft, and after it a change goes through the freeze corpus and the RFC process.
- Known gaps, accepted for now and tracked in the format document: a payload with the wrong magic is
  reported as `Truncated` (the wrong word, same outcome); sizes are bounded only by `u32` and by the log's
  own payload ceiling, with no per-key or per-value limit; an empty write list is accepted.
- The decoder is checked by a round trip, by cutting a valid payload at every length, and by arbitrary bytes
  never panicking. A fuzz target is still to be added.
