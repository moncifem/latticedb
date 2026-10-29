# Independent LCIX v1 examples

The four files are hand-framed little-endian examples, without invoking Rust encoders:
`LCIX`, byte version 1, u32-length LCAT block (zero means absent), u32 index-op count,
index operations, u32-length LROW block (zero means absent). Existing LCAT v1 bytes are unchanged.

`compound.bin` contains table 7 `items`, index 9 `by_value` over WholeValue, put `(7,a)=x`
and delete `(7,b)`. It is 96 bytes. `drop.bin` drops index 9. `slice.bin` creates index 12
`prefix` on table 7 with Slice(offset=1,length=2). `legacy-catalog.bin` is the unchanged
23-byte LCAT v1 nested table block. Codec tests require exact encoding and decoding of these
independent bytes. These draft fixtures do not constitute format acceptance.
