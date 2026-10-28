# Format: typed row values v1

Status: draft. The value stored for a row whose table comes from a schema type (WP22).

```
version u8 = 1
presence bitmap: one bit per column, in column order, least significant bit first, padded with zero bits
cells: for each present column, in column order, one cell
overflow count u32
then, per overflow field: name length u16, the UTF-8 name, a tagged value
```

Columns are the type's fields (composed facet fields included) sorted by name. Cell encodings:

| Type | Cell |
|---|---|
| bool | one byte, 0 or 1 |
| int | 8 bytes, signed, little endian |
| text, ref | length u32, then UTF-8 |
| bytes | length u32, then the bytes |
| enum | variant index u16 into the enum's variant list in declaration order |
| list | count u32, then that many cells of the item type |

Tagged values (overflow only): a tag byte (0 bool, 1 int, 2 text, 3 bytes, 4 list, 5 ref, 6 variant,
7 object) followed by the same encodings; lists and objects nest up to 16 levels.

Rules:

- A required column must be present; an optional one is simply absent.
- Fields unknown to the encoding layout are written to the overflow area and read back unchanged.
  Decoding requires the same layout that wrote the row. The codec version is not a schema version;
  dispatch by persisted layout version and schema migration are not implemented yet.
- Overflow fields and tagged object fields are ordered by name, with no duplicates. An overflow field
  must not repeat a column name. Non-canonical field ordering is rejected. Padding bits in the bitmap must be zero, so one row has
  exactly one encoding. Typed and tagged values both enforce the 16-level nesting limit.
- Decoders check every length against the remaining bytes and reject trailing bytes, an unknown version,
  an enum index outside the variant list and invalid UTF-8.
- Value constraints (ranges, `max`, enum membership beyond the index) are checked by schema validation,
  not by this encoding.
