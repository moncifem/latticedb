# fuzz

`cargo-fuzz` targets for the parsers and decoders that read untrusted bytes: `changeset_decode`, `id_parse`, `log_open`, `lsl_parse`, `pgwire`, `sql_parse`, `wal_record`, `row_ops`, `catalog_decode`, `typed_row_decode`, `feed_decode`, `sync_frame`, `plugin_jsonl`, `mcp_message`, `outbox_decode`, `crdt_text_decode`, `crdt_tree_decode`, `conflict_decode`, `scram_exchange`, `credential_file`, `tls_pem` and `subscribe_host`.

```sh
cd fuzz
mkdir -p corpus/wal_record
cargo +nightly fuzz run wal_record corpus/wal_record seeds/wal_record
```

- `seeds/<target>/` holds small, valid and near-valid inputs committed to the repository. The scheduled runs read them next to the generated, uncommitted `corpus/<target>/`.
- Every fixed crash adds its minimized input to `seeds/<target>/` and a regression test beside the code it broke.
- The nightly matrix gives each target ten minutes; the weekly and manual `fuzz-24h` workflows have their own budgets. All three list every target here except `trivial`, and `python3 tools/check_fuzz_matrices.py` (run by the pull request gate) fails when a target is missing from any of them or when `seeds/<target>/` holds no file. The same gate runs `cargo check --manifest-path fuzz/Cargo.toml --bins --locked`.
- Scheduled fuzz jobs use the pinned toolchain `nightly-2026-10-27` and `cargo-fuzz@0.13.2`.
- `settings.toml` holds the libFuzzer limits of the scheduled runs: a per-input `-timeout` (20 s) and `-rss_limit_mb` (2048) for all targets and a `-max_len` per target with the reason for it. libFuzzer's own defaults are a 1200 s timeout, longer than the 600 s nightly run, and a 4096-byte `-max_len`, below the limit of most decoders here. Each workflow builds the flags with `python3 tools/check_fuzz_matrices.py flags <target>`; the same script fails the pull request gate when a target has no entry or no stated basis, or when a `fuzz run` does not pass `$FUZZ_FLAGS`. The values are choices, not measurements: no scheduled run has recorded the slowest honest input under the sanitizer, and a timeout that turns out too tight is a finding to fix in this file. For codecs that accept more than 1 MiB (16 MiB records and values), `-max_len` stays near 1 MiB, so the over-limit branch is reached through a length field, not a payload of that size.
- The nightly job keeps a corpus per target between nights with `actions/cache`. The scheduled run on the default branch is the only one that saves; it merges the grown corpus and `seeds/` with `-merge=1` (only inputs that add coverage survive) and drops the largest inputs beyond 16 MiB per target. A manual run from another branch restores the corpus and does not save. The weekly and manual 24-hour workflows keep their own caches. A cache entry can be evicted, in which case a night starts from `seeds/` alone, as it did before.

## Plugin JSONL target

`plugin_jsonl` feeds arbitrary bytes to the agent plugin contract decoder (`lattice_sdk::contract`). A line
that decodes must re-encode to a canonical line that decodes to the same message; the same bytes are also
run through the bounded line framer. Seeds in `seeds/plugin_jsonl/` cover each message family, a wrong
version, an unknown field, a duplicate key, a float, a lone surrogate, CRLF, invalid UTF-8 and a truncated
stream. The target is registered in the nightly, weekly and manual 24-hour matrices.

## Sync frame target

`sync_frame` feeds arbitrary bytes to the sync protocol decoder (`lattice_api::sync`, which now lives in `lattice-proto`). A frame that
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

## WebSocket frame target

`websocket_frame` feeds arbitrary bytes to the WebSocket codec (`lattice_api::websocket`, which now lives in `lattice-proto`). A client or server frame that decodes must re-encode to exactly the bytes it came from (shortest length form, mask key read from the frame) and is incomplete one byte short. The input is also run through a `Connection` with small frame and message caps, once as a stream and once byte by byte through a caller-side buffer (the unconsumed buffer stays within 14 bytes plus the frame cap), and through the upgrade request parser. Seeds in `seeds/websocket_frame/` cover the RFC 6455 examples, fragmentation with an interleaved ping, split UTF-8, invalid UTF-8, close payloads, reserved bits and opcodes, non-minimal and oversized lengths, a continuation without a start, a sync Hello inside a binary frame and several upgrade requests. The target is registered in the nightly, weekly and manual 24-hour matrices; registration is not evidence that those runs have completed.

The `upgrade_*` seeds and the codec tests use the public sample `Sec-WebSocket-Key` from RFC 6455 section 1.3. The secret scanner reports it as a generic API key, so `.gitleaks.toml` allows that one exact value and keeps every default rule.

## Bearer token target

`bearer_verify` feeds `header NUL file` (or just a header) to `lattice_api::bearer`. The header is checked against a fixed store: only the one stored token, byte for byte after a case-insensitive `Bearer ` scheme, may authenticate, at three clock values. The file part, when it is valid UTF-8, goes to `CredentialStore::parse`; a store that parses must render back to exactly the same bytes and answer the header without panicking. Seeds in `seeds/bearer_verify/` cover the valid token, a lower-case scheme, a non-canonical base64 tail, an upper-case id, an unknown id, a wrong or short secret, other schemes, extra spaces, an empty token, and files with a repeated id, a missing final newline, a wrong version, a revoked or expired entry, an empty account, a trailing field, three active tokens for one account and a blank line. `crates/latticed/tests/bearer_seeds.rs` replays the seeds in the pull request gate. The target is registered in the nightly, weekly and manual 24-hour matrices; registration is not evidence that those runs have completed.

## TLS PEM and subscription host targets

`tls_pem` splits its input at the first NUL byte (or in half when there is none) into a certificate chain and a private key, and passes them to `latticed::TlsConfig::from_pem`. An input whose chain is over 64 KiB or whose key is over 16 KiB must be refused; any other input may be accepted or refused, and neither outcome may panic. The seeds in `seeds/tls_pem/` are a public self-signed test certificate followed by truncated or garbled key blocks. No complete private key is committed, so the secret scanner's private-key rule does not match them.

`subscribe_host` reads a control byte, a tuning byte and then sync frames. The control byte picks the behaviour of an in-memory source (honest, unavailable, too old or out of order) and the length of its log; the tuning byte picks the batch size, the subscription limit and the frames per pump, each inside the range the host accepts. Frames are decoded with `lattice_api::sync::decode` and passed to `SubscriptionHost::handle` and `poll`. Every frame the host sends must encode within the payload limit. A delivered batch must hold one to `batch_events` events whose cursors are each strictly after the one before, and the host's last cursor for a subscription must match what the client was sent. Every read must be made for the connection's principal, and a closed host must answer nothing. The seeds in `seeds/subscribe_host/` cover the honest path with resume and acknowledgement, each source failure, and each protocol rule the host enforces.

Both targets are registered in the nightly, weekly and manual 24-hour matrices. Registration is not evidence that those runs have completed.

