# Format: workflow conflict entity v1

Status: DRAFT, experimental, unaccepted. Implemented by `lattice_core::workflow` (WP47, P3-09). No
RFC, ADR, frozen golden file or independent review exists yet, and nothing persists or transmits this
encoding. An RFC and ADR, a golden file under `formats/corpus` and a review are required before any
caller writes these bytes to the log or sends them to a peer. Do not promise cross-version
compatibility.

A conflict entity records competing workflow transitions of one entity that the server could not
accept together (see the arbitration rules in `docs/ARCHITECTURE.md`, section 15.4a). The encoding is
canonical: one conflict has exactly one encoding, and the decoder accepts only that encoding.

## Layout

All integers are little-endian unless marked big-endian. A `name` is `u16 length` (1 to 64) followed by
that many UTF-8 bytes.

| Field | Size | Notes |
|---|---|---|
| version | 1 | `1` |
| entity | 16 | `u128`, big-endian |
| ordinal | 4 | at least 1; 1 for the entity's first conflict |
| base | name | the state every proposal was made from |
| proposal count | 2 | 2 to 16 |
| proposals | repeated | see below |
| option count | 2 | `proposals` distinct targets plus one |
| options | repeated | `u8 tag` (0 = revert, 1 = choose) and, for choose, a `name` |
| resolution flag | 1 | 0 open, 1 closed |
| resolution | optional | an option (as above) then `u64 actor` |

A proposal is: `hlc` (16 bytes: `u64 wall_ms`, `u32 counter`, `u32 node`), `u64 request id`, `name from`,
`name to`, `u64 actor`.

## Canonical rules (the decoder rejects anything else)

* Proposals are strictly increasing by `(hlc, request id, from, to, actor)`, so there are no duplicates.
* Options are exactly: revert, then one choose per distinct proposal target, in increasing name order.
* A resolution choice is one of the options.
* No bytes follow the value. Any other version byte is refused. Lengths are bounded, so a hostile input
  cannot make the decoder allocate more than a conflict of 16 proposals.

`ConflictEntity::encode` applies the same checks and returns an error instead of writing bytes the
decoder would refuse.

## Worked example

Entity 7, ordinal 1, base `todo`, two open proposals: request 2 by actor 2 (`todo` to `blocked`, HLC
wall 90) and request 1 by actor 1 (`todo` to `doing`, HLC wall 100). 143 bytes:

```
01                                  version
00000000000000000000000000000007    entity (big-endian)
01000000                            ordinal 1
0400 746f646f                       base "todo"
0200                                2 proposals
  5a00000000000000 00000000 02000000   hlc wall 90, counter 0, node 2
  0200000000000000                     request id 2
  0400 746f646f                        from "todo"
  0700 626c6f636b6564                  to "blocked"
  0200000000000000                     actor 2
  6400000000000000 00000000 01000000   hlc wall 100, counter 0, node 1
  0100000000000000                     request id 1
  0400 746f646f                        from "todo"
  0500 646f696e67                      to "doing"
  0100000000000000                     actor 1
0300                                3 options
  00                                  revert
  01 0700 626c6f636b6564              choose "blocked"
  01 0500 646f696e67                  choose "doing"
00                                  open
```

As one hex string:

```
0100000000000000000000000000000007010000000400746f646f02005a00000000000000000000000200000002000000000000000400746f646f0700626c6f636b656402000000000000006400000000000000000000000100000001000000000000000400746f646f0500646f696e670100000000000000030000010700626c6f636b6564010500646f696e6700
```

The unit test `worked_example_matches_the_format_document` decodes these bytes and checks the result
against a conflict built by the arbiter. The expected bytes were produced separately from the encoder.
