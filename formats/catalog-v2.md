# Format: catalog changes v2

Status: draft. Extends [catalog v1](catalog-v1.md) with persisted LSL schemas and stable physical typed
layout ownership (P2-01/P2-02). Readers accept v1 and v2. Writers retain the byte-identical v1 encoding
for payloads containing only create/rename/drop operations; a payload with a schema operation uses v2.

The `LCAT` magic, operation count and kinds 1–3 have their v1 encodings. Kind 4 is:

```
kind u8 = 4
schema version u64 (little endian, nonzero, increasing, never reused)
source length u32 (bounded by the LSL parser input limit)
canonical LSL source UTF-8 bytes
physical table count u32 (at least 1)
then, in increasing table-id order:
  table id u32
  name length u32 (1 to 255)
  UTF-8 schema type name
```

A v2 payload must contain at least one kind-4 operation. Repeated/out-of-order physical ids, duplicate
type names, trailing bytes, invalid UTF-8 or an unsupported version are errors. Parsing/checking of LSL
and semantic table-map validation happen before logging and on replay. A CRC-valid catalog payload
that cannot decode or apply stops recovery; it is never skipped as another engine's data.

A schema operation maps every type in its checked schema exactly once to a physical table id. An
existing live type keeps its id. A new id must never have appeared before, including ids of dropped
tables. Every type in one schema operation becomes visible at the same catalog LSN or none does.
Installing a schema does not implicitly drop types omitted from its source.

Each schema version retains the checked constraints, flattened per-type columns and original LSL for
all time. The source is part of the catalog, not a caller-supplied requirement at reopen. Typed row
values identify their writer schema version using [the typed envelope](typed-envelope-v1.md). Schema
installation is not an online backfill or migration; old rows retain their writer layout.

The committed `formats/corpus` examples are produced by
`formats/generate_typed_catalog_golden.py`, using only Python's standard library and no Rust encoder.
The typed-store integration suite decodes and re-encodes them byte-exactly.

Physical table allocation also excludes ids seen in raw row history, even if no catalog entry exists
or all rows were deleted. Adopting an existing untyped catalog table with any row history is rejected
before appending a schema record. Recovery rebuilds physical-id reservations from every row write.
