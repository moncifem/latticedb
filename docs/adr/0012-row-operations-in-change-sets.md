# ADR 0012: Row operations travel inside the change set payload

Status: accepted

## Context

A change set's payload is opaque bytes to the kernel (format `changeset-v1`). The row engine needs a
defined way to turn a committed change set into row writes, and it must do so identically on every
replay: after a crash, from the log alone, with no side channel. Whatever the encoding, it becomes part
of the on-disk format as soon as a log is written with it.

## Decision

The payload of a change set that carries row writes is a list of operations, defined in
`formats/rowops-v0.md`: a version byte, an operation count, then for each operation a kind (put or
delete), a numeric table id, a key and, for a put, a value. Keys and values are opaque bytes to this
layer; their typing comes from the catalog (work packages WP21 and WP22) and not from the encoding.

The decoder is total: any byte string decodes to a list of operations or to an error. It never panics,
never allocates from an unchecked length, and rejects trailing bytes, an unknown version, an unknown
kind, an empty list, and sizes above the stated limits. The row engine applies a change set only if its
payload decodes; a change set that does not decode is recorded in the log but is not applied, and is
reported by the scrubber.

Table ids are numbers rather than names so a rename in the catalog never rewrites history.

## Consequences

- The format is `v0`: it may change until the Phase 2 gate, but once a log written with it exists it
  must stay readable, so a change after that point goes through the freeze-corpus and RFC process.
- Row writes and everything else in the system share one atomic unit, the change set, which is what
  makes invariants I2 and I3 provable across the log and the row engine.
- Other payload kinds (blobs, graph edges, text) will need their own encodings. A kind byte at the
  front of the payload is not added now; each engine will define how it recognises its own payloads
  when the second one arrives. This is the one open question this ADR leaves.
