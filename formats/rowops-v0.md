# Format: row operations v0

Status: draft (ADR 0012). The payload of a change set that carries row writes.

```
version u8 = 1
operation count u32 (1 to 65,535)
then, for each operation:
  kind u8: 1 put, 2 delete
  table u32 (little endian)
  key length u16 (0 to 4,096), then the key bytes
  if kind is put: value length u32 (0 to 8,388,608), then the value bytes
```

All integers are little endian. Keys and values are opaque bytes here.

Rules for a decoder:

- Every length is checked against the bytes that remain before anything is allocated or sliced.
- Bytes after the last operation are an error.
- Version other than 1, kind other than 1 or 2, a count of zero or above the limit, a key or value above
  its limit are all errors.
- The same key may appear more than once; when the change set is applied the last operation on it wins.

The decoder is exercised by a property test over arbitrary bytes and by cutting a valid encoding at every
length. A fuzz target is added with the row engine.
