# Changelog

All notable changes are recorded here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0 the on-disk format and APIs can change
between minor versions; format changes are called out explicitly.

## [Unreleased]

## [0.1.0-alpha.0]

Not published. This is the version under development; the first published build will be
`0.1.0-alpha.1`, cut when the Phase 1 gate is met.

### Added

- Deterministic simulation with storage and network fault injection, and invariant checkers I1 to I14.
- Write-ahead log (format v1) with group commit, segment rotation, torn-tail recovery and a bounded
  commit pipeline with backpressure.
- Kernel with typestate transactions, change set dedupe, turn fencing, hybrid logical clock
  timestamps, checkpoints verified on recovery, and a reader cursor over the durable log.
- Read-only log scrubber and `lattice doctor`.
- Results ledger stored in Lattice's own log, `lattice ledger show` and `cargo xtask scorecard`.
- Schema language (LSL): parser, checker, `lattice lsl check|fmt|compat`, reversible migration lenses.
- CRDT building blocks with convergence tests over a faulty network.
- 20 planted bugs the simulator must detect; mutation testing of the core crates.
- TLA+ models for commit, hybrid logical clock, turn fencing and set convergence.
- Fuzz targets for the decoders, the schema parser and log recovery.
- Release pipeline: nightly pre-release, tagged releases, archives for Linux x86_64 and arm64 and
  macOS arm64, checksums, SBOM, build provenance and a container image on GHCR.

### Fixed

- Transactions from another kernel could be promoted when kernels shared an RNG seed.
- A checkpoint taken with an unapplied change set made recovery reject the log.
- `read_from` returned a short history instead of an error when the log was damaged.

[Unreleased]: https://github.com/moncifem/latticedb/compare/v0.1.0-alpha.1...HEAD
[0.1.0-alpha.0]: https://github.com/moncifem/latticedb/commits/main
