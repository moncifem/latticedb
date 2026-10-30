# ADR 0023: Change feed event and cursor encoding

Status: proposed

## Context

P3-01 requires a stable cursor and typed entity events over committed change sets. PR #263 merged a
pure model and candidate codec without its required RFC/ADR review package. Existing implementation
is not evidence of format acceptance. [RFC 0009](../rfc/0009-change-feed-event-format.md) opens that
review explicitly; its comment period ends 2026-11-05 and no owner waiver is assumed.

## Decision

Propose the existing [candidate layout](../../formats/change-feed-v1.md), preserving its bytes: a
12-byte big-endian `(lsn u64, index u32)` cursor and version-one event carrying cursor, complete set
length, ChangeSet provenance and one bounded delete/upsert. Event integer endianness follows the
format table, including big-endian IDs/author and little-endian lengths/HLC/turn/entity kind.

Origin is only `(0, 0)` and cannot be an event. Reject index outside the set, keys above 4096 bytes,
values above 16 MiB, unknown tags, truncation and trailing bytes. Do not infer authorization,
transactional delivery or durable provenance from successful decoding. The producer must supply the
actual durable LSN and deterministic complete entity list; no engine/stream integration is added.

This is a proposed decision, not an accepted or frozen format. Owner disposition and independent
format review remain pending. No Phase 1/2/3 gate or release decision follows from this proposal.

## Consequences

Cursor byte order matches logical order within a log, with gaps for non-event records. Checkpoints and
vacuum do not renumber retained records. The enclosing subscription must define stream identity,
history-unavailable errors, filtering, atomic delivery, replay and admission; this cursor cannot by
itself disambiguate unrelated/restored logs. Bound aggregate memory separately from per-event limits.

The 4096 byte key bound is also enforced where rows are written: the row store refuses a put with a longer
key (see [rowops-v1](../../formats/rowops-v1.md)), so new history always fits the feed. A log from before
that rule can hold one. A reader who may not read that row is unaffected; one who may read it is given
the events before it, then an error that names the blocked cursor, and reads again from that cursor to skip
it, so no other event is lost. This tightens what the store accepts
and changes no encoded byte; it still needs the independent format review noted above.

Handwritten vectors and synthetic ordering/codec tests exist; actual restart/replay and subscription
proof, scheduled fuzz execution and frozen independent golden fixtures remain outstanding. Before
activation or freezing, resolve those gaps and compatibility expectations with consumers. Preserve
accepted version meanings; unsupported versions must be refused by future enclosing protocols before
effects. No WAL rewrite or migration tooling is introduced by this documentation repair.
