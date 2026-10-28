# Lattice

An open source database for software teams. Work items, documents, specs and designs live in one
store, searchable by text, meaning and time. Agents work in it as teammates with scoped permissions.

> **Status: pre-alpha.** There is no server to run yet. What exists is the storage kernel and the
> test machinery around it. Do not use it for data you care about.

## What works today

- A write-ahead log with checksums, crash recovery and idempotent commits
  ([format](formats/wal-v1.md)).
- A deterministic simulator that injects crashes, torn writes, disk errors and bit rot, and checks the
  kernel's invariants after every recovery.
- A verifier for data directories: `lattice doctor`.
- A schema language parser and checker (LSL), conflict-free data types and a benchmark suite with a
  regression gate.

Phase-by-phase status is in [`docs/gates`](docs/gates). It lists what is missing as well as what is done.

## Try it

You need a recent stable Rust toolchain (pinned in `rust-toolchain.toml`).

```bash
cargo xtask ci                      # the checks every pull request must pass
cargo xtask sim --seeds 1000        # run the crash simulator
cargo run -p lattice-cli -- doctor --dir path/to/data
cargo run -p lattice-cli -- lsl compat old.lsl new.lsl   # is a schema change safe for existing data?
```

## How it is built

Rust, as a workspace of small crates in strict layers (`layers.toml`, enforced by `cargo xtask
check-layers`). The deterministic core never touches the clock, disk or network directly; it goes
through seams, which is what lets the simulator replay any failure from a seed.

- Design: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- Plan and exit criteria for each phase: [`docs/spec`](docs/spec/README.md)
- Decisions and their reasons: [`docs/adr`](docs/adr/README.md)
- Engineering practice: [`docs/ENGINEERING.md`](docs/ENGINEERING.md)

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md) first. Work is tracked on the
[project board](https://github.com/users/moncifem/projects/1), with
[milestones](https://github.com/moncifem/latticedb/milestones) per phase. Report security issues as
described in [`SECURITY.md`](SECURITY.md).

## License

Apache-2.0. See [`LICENSE`](LICENSE).
