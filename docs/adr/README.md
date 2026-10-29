# Architecture decision records

| Id | Title | Status |
|---|---|---|
| [0001](0001-rust.md) | ADR 0001: Rust as the implementation language | accepted |
| [0002](0002-native-engine-pg-wire.md) | ADR 0002: Native Rust engine with PostgreSQL wire compatibility | accepted |
| [0003](0003-inbound-only-core.md) | ADR 0003: The core makes no outbound connections | accepted |
| [0004](0004-optional-models.md) | ADR 0004: Models are an optional plugin, off by default | accepted |
| [0005](0005-layering.md) | ADR 0005: Layered crate map enforced by tooling | accepted |
| [0006](0006-license.md) | ADR 0006: Apache-2.0 licence | accepted |
| [0007](0007-wal-format-v1.md) | ADR 0007: WAL and change set format v1 | accepted |
| [0008](0008-sync-kernel-and-runtime.md) | ADR 0008: Synchronous kernel behind seams; runtime decision deferred | accepted |
| [0009](0009-benchmarks-crate.md) | ADR 0009: Benchmarks live in their own crate and may read the wall clock | accepted |
| [0010](0010-text-crdt-spike.md) | ADR 0010: Text CRDT: own spike behind a trait, decision revisited in Phase 3 | accepted |
| [0011](0011-engine-runtime.md) | ADR 0011: Engine runtime | accepted |
| [0012](0012-row-write-payload.md) | ADR 0012: Row writes travel in the change set payload | accepted |
| [0013](0013-sql-planner-and-executor.md) | ADR 0013: SQL planner and executor | proposed |
| [0014](0014-derived-row-indexes.md) | ADR 0014: Secondary indexes are derived from committed rows | accepted |
| [0018](0018-schema-change-strategy.md) | ADR 0018: Online schema changes through shadow generations | proposed |
| [0019](0019-catalog-managed-indexes.md) | ADR 0019: Managed index definitions in atomic catalog and row changes | proposed |
| [0020](0020-pgwire-scram-sha256.md) | ADR 0020: PostgreSQL SCRAM-SHA-256 primitives with explicit authentication phases | accepted |
| [0021](0021-pgwire-scram-tls-integration.md) | ADR 0021: Require TLS for configured SCRAM sessions | accepted |
| [0022](0022-wal-capability-admission.md) | ADR 0022: Required engine capabilities at WAL admission | accepted |
| [0023](0023-change-feed-event-format.md) | ADR 0023: Change feed event and cursor encoding | proposed |
| [0024](0024-sync-protocol-framing.md) | ADR 0024: Sync protocol framing and messages | proposed |
