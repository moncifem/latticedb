# Format: proposed typed catalog/index compound v2

Status: draft, RFC 0001 / ADR 0015 proposed. This does not revise accepted LCIX v1 or accept a format.

The framing is `LCIX`, version byte 2, followed by the same bounded blocks as LCIX v1:
length-prefixed table-catalog bytes, index-operation count and operations, and a length-prefixed row
block. All integers remain little endian. Maximum payload and operation counts remain unchanged.

Version 2 is canonical only when its nested catalog is LCAT v2 containing a schema definition.
LCIX v1 continues to require LCAT v1. Ordinary table/index changes retain their accepted v1 bytes.
The nested LCAT count is bounded before decoding, and unknown versions, trailing bytes, invalid
schema mappings and invalid typed writes are refused, not treated as opaque data.

LCIX v2 requires accepted managed-compound capability 1 **and proposed typed capability 4** in its
actual containing segment (mask 5). Standalone LCAT v2 and typed LROW writes require typed capability
4. LCIX v1 with typed-row mutations also requires both bits, without changing its byte representation.
Bit 1/value 2 remains unknown/reserved. The full RowStore owns schema/catalog/index/row publication
through one Kernel writer and validates all semantic transitions before append and during replay.

CatalogStore intentionally does not advertise capability 4: it projects metadata but does not run
row-schema validation. Legacy callback mode still advertises zero. The frozen 45926c7 reader proof
must show refusal of capabilities 4/5 before callbacks or repair. Independent Python fixtures pin
both typed masks and standalone/compound schema framing; successful execution evidence is reported
separately. No checkpoint format, online migration or complete SQL type coverage is implied.
