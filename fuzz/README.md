# fuzz

`cargo-fuzz` targets for the parsers and decoders that read untrusted bytes: `changeset_decode`, `id_parse`, `log_open`, `lsl_parse`, `pgwire`, `sql_parse`, `wal_record`, `row_ops`, `catalog_decode`, `typed_row_decode`, `feed_decode`, `sync_frame`, `plugin_jsonl`, `mcp_message`, `outbox_decode`, `crdt_text_decode`, `crdt_tree_decode` and `conflict_decode`.

```sh
cd fuzz
mkdir -p corpus/wal_record
cargo +nightly fuzz run wal_record corpus/wal_record seeds/wal_record
```

- `seeds/<target>/` holds small, valid and near-valid inputs committed to the repository. The scheduled runs read them next to the generated, uncommitted `corpus/<target>/`.
- Every fixed crash adds its minimized input to `seeds/<target>/` and a regression test beside the code it broke.
- The nightly matrix gives each target ten minutes; the weekly and manual `fuzz-24h` workflows have their own budgets. All three list every target here except `trivial`, and `python3 tools/check_fuzz_matrices.py` (run by the pull request gate) fails when a target is missing from any of them or when `seeds/<target>/` holds no file. The same gate runs `cargo check --manifest-path fuzz/Cargo.toml --bins --locked`.
- Scheduled fuzz jobs use the pinned toolchain `nightly-2026-10-27` and `cargo-fuzz@0.13.2`.

## Plugin JSONL target

`plugin_jsonl` feeds arbitrary bytes to the agent plugin contract decoder (`lattice_sdk::contract`). A line
that decodes must re-encode to a canonical line that decodes to the same message; the same bytes are also
run through the bounded line framer. Seeds in `seeds/plugin_jsonl/` cover each message family, a wrong
version, an unknown field, a duplicate key, a float, a lone surrogate, CRLF, invalid UTF-8 and a truncated
stream. The target is registered in the nightly, weekly and manual 24-hour matrices.

## Sync frame target

`sync_frame` feeds arbitrary bytes to the sync protocol decoder (`lattice_api::sync`). A frame that
decodes must re-encode to exactly the bytes it came from, and is incomplete one byte short. The input is
also decoded as a stream and with a small negotiated payload limit. Seeds in `seeds/sync_frame/` cover
every message type, a two-frame stream, a trailing byte, an unknown type, an over-long length, a
truncated frame, a zero version and an undefined error code. The target is registered in the nightly,
weekly and manual 24-hour matrices; registration is not evidence that those runs have completed.

## Row codec targets

- `row_ops` decodes versioned row-write batches, then requires canonical byte-exact re-encoding and an equal second decode.
- `catalog_decode` makes the same checks for the catalog payload codec. This exercises decoding, not catalog lifecycle validation or WAL recovery.
- `typed_row_decode` uses the first input byte to choose a bounded physical layout and decodes the remaining bytes as a typed row. Selector `0` covers tagged overflow without columns; `1` covers optional bool/int/text/bytes/ref/enum/list cells; `2` covers a required bool; `3..=20` cover nested lists around the codec depth limit. Successful rows must re-encode to the exact input payload and decode to the same value.

These three targets are registered in the nightly, weekly and manual 24-hour matrices. Their small seeds include independent
little-endian encodings, required and optional cells, nested overflow, depth boundaries, malformed
lengths and an unsupported version. The checked-in Python encoder uses only `struct`, never the Rust
codecs. Reproduce the seed bytes without writing them:

```sh
cd fuzz
python3 seeds/generate_row_codec_seeds.py --check
```

No local fuzz execution is needed to build-check these targets with `cargo check --bins` inside
`fuzz/`. Linux CI runs cargo-fuzz with the GNU target; a native build check on another platform is
not evidence of a Linux sanitizer build or a completed fuzz duration. Crash discoveries still need
minimized seed inputs and focused regressions beside the affected decoder.

## Change feed target

`feed_decode` feeds arbitrary bytes to the change feed event decoder and the cursor decoder
(`lattice_core::feed`). Anything that decodes must re-encode to exactly the input bytes, which checks
that only canonical encodings are accepted. Seeds in `seeds/feed_decode/` cover each operation, a turn,
the extreme cursor and index values, the origin cursor, and malformed inputs (lsn 0, index past the set,
wrong version, trailing byte, oversized length, truncation). The target is registered in the nightly,
weekly and manual 24-hour matrices.

## MCP message target

`mcp_message` feeds arbitrary bytes to the MCP server (`lattice_sdk::mcp::Server::handle`) as a caller who
may only read and subscribe under scope `a`. Any answer must itself be a bounded JSON object, and the
backend asserts that no call outside the grant, and no write, ever reaches it. Each input runs against a
stateless connection and against one that opened with `initialize`. The bytes also go to the bounded
JSON reader alone. Seeds in `seeds/mcp_message/` cover ping, initialize, initialized, discover,
tools/list, a missing and a mistyped `_meta`, an allowed read, a denied write, a subscribe, a batch, scope traversal, an unsupported
version, JSON-RPC text inside an argument, a duplicate key, deep nesting and invalid UTF-8. The target is
registered in the nightly, weekly and manual 24-hour matrices.

## Outbox decode target

`outbox_decode` feeds arbitrary bytes to `lattice_sdk::outbox::Outbox::from_bytes`. Input that decodes must
re-encode to exactly the same bytes (one canonical form), restore again to the same queue, and drain through
`poll` and `on_receipts`. Seeds in `seeds/outbox_decode/` cover an empty queue, each record kind, fenced turns,
sparse counters, a bad checksum, truncation, a wrong version and magic, a trailing byte, unsorted counters, a
counter beyond the next one, an item on a fenced turn, an unknown kind and invalid UTF-8. The target is registered
in the nightly, weekly and manual 24-hour matrices.

## CRDT operation and conflict decode targets

- `crdt_text_decode` decodes a text CRDT operation (`lattice_core::crdt_text::Op`).
- `crdt_tree_decode` decodes a tree move (`lattice_core::crdt_tree::Op`, exactly 41 bytes).
- `conflict_decode` decodes a workflow conflict entity (`lattice_core::workflow::ConflictEntity`).

Each decoder accepts only canonical bytes, so an input that decodes must re-encode to exactly the input;
`conflict_decode` also decodes the input a second time and expects an equal value. Seeds in
`seeds/crdt_text_decode/`, `seeds/crdt_tree_decode/` and `seeds/conflict_decode/` are written by
`seeds/generate_crdt_workflow_seeds.py` from the documented byte layouts, with `struct` and not the Rust
encoders (`python3 seeds/generate_crdt_workflow_seeds.py --check` verifies them). They cover each valid
operation kind, the extremes of the tree position, a resolved and an open conflict, and refusals: wrong
version, unknown kind or flag, empty or invalid UTF-8 text, a non-causal anchor, an offset past the chunk, a zero
id, reserved tree nodes, wrong lengths, a trailing byte, truncation, a zero ordinal, a single proposal,
unsorted proposals, options that do not match the proposals and a resolution that is not an option. The
targets are registered in the nightly, weekly and manual 24-hour matrices; registration is not evidence that
those runs have completed.
