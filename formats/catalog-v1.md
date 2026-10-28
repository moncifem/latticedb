# Format: catalog changes v1

Status: draft. The payload of a change set that changes the catalog (which tables exist).

```
magic 4 bytes = "LCAT"
version u8 = 1
operation count u32 (at least 1)
then, for each operation:
  kind u8: 1 create, 2 rename, 3 drop
  table u32 (little endian)
  if kind is 1 or 2: name length u32 (1 to 255), then the UTF-8 name
```

Rules:

- Table ids are never reused, even after a drop, so an old id in a row change set always means the table
  it meant when it was written.
- A name is unique among live tables. Operations in one change set apply in order, so a rename may free a
  name that a later operation in the same change set takes.
- A change set applies entirely or not at all. The catalog keeps every version and can be read as of any
  earlier LSN.
- A decoder checks every length against the bytes that remain, and rejects trailing bytes, an unknown
  version or kind, an empty list and an invalid name. Other engines' payloads start with a different magic
  and are skipped; a catalog change set that does not decode or apply stops recovery as corruption.

Columns and types are not part of this version; they arrive with the typed tables (WP22).
