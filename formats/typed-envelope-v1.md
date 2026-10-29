# Format: typed row envelope v1

Status: draft. The byte value written by the typed store into a physical row table.

```
magic 4 bytes = "LTYP"
version u8 = 1
writer schema version u64 (little endian)
cells encoded by typed row values v1, using that schema's layout for this table id
```

The physical table id is part of the row key and is not repeated in the value. Its writer schema
version must resolve to an immutable catalog definition that includes that physical table. The
[cells codec](typed-rows-v1.md) is unchanged. The catalog persists the complete layout and constraints;
callers do not supply the old layout when reopening the store.

Decode dispatches by the row's writer schema, not the newest schema. This keeps existing values
readable when later layouts reorder columns or enums. It does not synthesize newly required columns
in old values, backfill data, or promise online schema migration. New writes validate against their
transaction's checked schema; a catalog change before commit makes a writing transaction retry.

Unknown magic/versions, missing writer schemas, table/schema mismatches, corrupt cell encodings and
stored values violating writer constraints are fail-stop typed read errors. Reopen verifies live
values in typed tables. Historical values are checked when read at their snapshot.

The committed `formats/corpus` examples are produced by
`formats/generate_typed_catalog_golden.py`, using only Python's standard library and no Rust encoder.
The typed-store integration suite decodes and re-encodes them byte-exactly.

Physical table allocation also excludes ids seen in raw row history, even if no catalog entry exists
or all rows were deleted. Adopting an existing untyped catalog table with any row history is rejected
before appending a schema record. Recovery rebuilds physical-id reservations from every row write.
