# fuzz

`cargo-fuzz` targets for the parsers and decoders that read untrusted bytes: `changeset_decode`, `id_parse`, `log_open`, `lsl_parse`, `pgwire`, `sql_parse`, `wal_record`, `row_ops`, `catalog_decode` and `typed_row_decode`.

```sh
cd fuzz
mkdir -p corpus/wal_record
cargo +nightly fuzz run wal_record corpus/wal_record seeds/wal_record
```

- `seeds/<target>/` holds small, valid and near-valid inputs committed to the repository. The scheduled runs read them next to the generated, uncommitted `corpus/<target>/`.
- Every fixed crash adds its minimized input to `seeds/<target>/` and a regression test beside the code it broke.
- The nightly matrix gives its configured targets ten minutes. The weekly and manual `fuzz-24h` workflows have separate target matrices and budgets; adding a nightly target does not add it to those runs.

## Row codec targets

- `row_ops` decodes versioned row-write batches, then requires canonical byte-exact re-encoding and an equal second decode.
- `catalog_decode` makes the same checks for the catalog payload codec. This exercises decoding, not catalog lifecycle validation or WAL recovery.
- `typed_row_decode` uses the first input byte to choose a bounded physical layout and decodes the remaining bytes as a typed row. Selector `0` covers tagged overflow without columns; `1` covers optional bool/int/text/bytes/ref/enum/list cells; `2` covers a required bool; `3..=20` cover nested lists around the codec depth limit. Successful rows must re-encode to the exact input payload and decode to the same value.

These three targets are registered in the nightly matrix. Their small seeds include independent
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
