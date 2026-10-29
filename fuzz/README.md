# fuzz

`cargo-fuzz` targets for the parsers and decoders that read untrusted bytes: `changeset_decode`, `id_parse`, `log_open`, `lsl_parse`, `pgwire`, `sql_parse` and `wal_record`.

```sh
cd fuzz
cargo +nightly fuzz run wal_record corpus/wal_record seeds/wal_record
```

- `seeds/<target>/` holds small, valid and near-valid inputs committed to the repository. The scheduled runs read them next to the generated, uncommitted `corpus/<target>/`.
- Every fixed crash adds its minimized input to `seeds/<target>/` and a regression test beside the code it broke.
- The nightly run gives each target ten minutes, the weekly run five hours, and the manual `fuzz-24h` workflow several hours in five seeded shards.
