# Changelog

All notable changes are recorded here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0 the on-disk format and APIs can change
between minor versions; format changes are called out explicitly.

## [Unreleased]

### Added

- Validated mutation and eval scorecard observations, with explicit tested/planned coverage, mutation
  denominator exclusions, exact eval gate fields and retained weekly producer provenance. Legacy eval
  reports without complete thresholds remain unverified.

- Unsigned scorecards with ten observed-or-missing sections, bounded ledger JSON interchange,
  retained nightly producer provenance and completed-fuzz summaries. Benchmark ledger entries now
  include the actual comparison verdict; missing measures remain explicit.

- Ten distinct storage defect witnesses with independent semantic oracles, clean controls and guard
  cleanup checks; the existing four-defect randomized workload remains unchanged.

- Real-process NS1 row recovery checks with observed commit receipts, complete-batch history oracles,
  index/log scrubbers and a second recovery after eight writer terminations.

- Phase review evidence packages from `cargo xtask gate`, with declared criteria, source provenance,
  invariant traceability and optional ledger diagnostics, without inferring or signing a gate pass.

- A row/index fault-history oracle covering all five disk profiles, actual injected-fault quotas,
  deterministic replay and exhaustive rotation/recovery cut points. Rot-censored histories are
  reported separately from verified histories; this does not claim the Phase 2 soak gate.

- Typed-row layouts and a canonical codec with an overflow area for fields unknown to the encoding
  layout. Decoding requires the original layout; catalog integration and schema migration remain open.

- Secondary index primitives derived from row writes, rebuilt during recovery and readable at old
  snapshots, with consistency checks and a planted stale-index defect to verify the checker.

### Changed

- Optimize test builds of workspace crates so the deterministic simulator runs faster in every
  platform gate, retaining debug assertions, overflow checks, test cases and seed budgets.

### Fixed

- Keep CLI test process launches separate from in-process ledger writes, preventing inherited WAL
  locks from making parallel macOS tests fail intermittently.
- Restore the catalog test lint gate and retain a separate CI run for each push to `main`, while
  superseded pull request runs can still be cancelled.

## [0.1.0-alpha.0]

Not published. This is the version under development; the first published build will be
`0.1.0-alpha.1`, cut when the Phase 1 gate is met.

### Added

- Deterministic simulation with storage and network fault injection, and simulator checks for the invariants that apply at the log level (I1, I2, I4, I6, I9, I12). The rest (I3, I5, I7, I8, I10, I11, I13, I14) have tests or belong to later phases; see the traceability matrix.
- Write-ahead log (format v1) with group commit, segment rotation, torn-tail recovery and a bounded
  commit pipeline with backpressure.
- Kernel with typestate transactions, change set dedupe, turn fencing, hybrid logical clock
  timestamps, checkpoints verified on recovery, and a reader cursor over the durable log.
- Read-only log scrubber and `lattice doctor`.
- Results ledger stored in Lattice's own log, `lattice ledger show` and `cargo xtask scorecard`.
- Schema language (LSL): parser, checker, `lattice lsl check|fmt|compat`, reversible migration lenses.
- CRDT building blocks with convergence tests over a faulty network.
- 21 planted bugs the simulator must detect; mutation testing of the core crates.
- TLA+ models for commit, hybrid logical clock, turn fencing and set convergence.
- Fuzz targets for the decoders, the schema parser and log recovery.
- Release pipeline: nightly pre-release, tagged releases, archives for Linux x86_64 and arm64 and
  macOS arm64, checksums, SBOM, build provenance and a container image on GHCR.

### Fixed

- Transactions from another kernel could be promoted when kernels shared an RNG seed.
- A checkpoint taken with an unapplied change set made recovery reject the log.
- `read_from` returned a short history instead of an error when the log was damaged, or when its leading segment was missing.
- A kernel with the same seed and first id could still take another kernel's transaction; in-flight transactions are now matched by id, position and content checksum.
- `lattice ledger check` read a fractional count as its integer prefix and could overflow on a huge one.
- Nightly could publish a different commit than the one it tested; one resolved commit is now used throughout, and the container image waits for the release.

[Unreleased]: https://github.com/moncifem/latticedb/compare/v0.1.0-alpha.1...HEAD
[0.1.0-alpha.0]: https://github.com/moncifem/latticedb/commits/main
