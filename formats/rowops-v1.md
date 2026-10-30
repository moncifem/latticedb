# Format: row writes v1

Status: draft (ADR 0012). The payload of a change set that writes rows.

```
magic 4 bytes = "LROW"
version u8 = 1
write count u32
then, for each write:
  table u32
  key length u32, then the key bytes
  kind u8: 0 delete, 1 put
  if kind is 1: value length u32, then the value bytes
```

All integers are little endian. Keys and values are opaque bytes.

A decoder must:

- check every length against the bytes that remain before slicing or allocating;
- reject a version other than 1, a kind other than 0 or 1, and bytes after the last write;
- apply writes in order, so when one change set writes the same key twice the last write wins.

**Key bound (write side).** The layout carries a key of up to `u32::MAX` bytes, but the row store refuses
to commit a put whose key is longer than 4096 bytes (`MAX_KEY_BYTES`, the same number as the change feed's
key bound in [change-feed-v1.md](change-feed-v1.md)), with `RowError::KeyTooLong` and nothing appended.
Deletes are not checked. The bytes of the format do not change, and a decoder still accepts a longer key:
a log written before this rule may hold one. The change feed then withholds only that row, see
`lattice_engines::feed_emit`. Value sizes remain bounded only by the payload.

Not yet specified: the maximum value size, and whether an empty write list is valid.
