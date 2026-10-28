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

Not yet specified: maximum key and value sizes, whether an empty write list is valid, and a distinct error
for a payload that does not start with the magic (today it is reported as truncated).
