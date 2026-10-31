# Lattice

Lattice is an open-source database for software teams, written in Rust. It keeps one shared, permissioned
history of a team's work items, docs, specs, designs and deployments. People and AI agents work inside the
same permissions, and every change stays on the record. The core makes no model calls.

**Status: pre-alpha. Use test data only.** No phase gate is signed, and nothing here is production ready.

- The alpha pre-release `v0.1.0-alpha.1` is published. It is a pre-release for test
  data only, with no phase-gate claim. Its data directories and the change-feed and sync encodings are not
  stable. See the [alpha guide](../alpha.md).
- Phase 0 is done. Phase 1 is built and its gate is open. Phase 2 work proceeds under a documented waiver
  that forbids release tags and exit-criteria claims until the Phase 1 gate is signed
  ([gate review](../gates/phase-1-review.md)). Phase 3 has started, and its gate is open. See the
  [roadmap](../roadmap.md) and the [gate pages](../gates).

## What works today

- A checksummed write-ahead log with crash recovery, group commit and a scrubber.
- A PostgreSQL-compatible server, `latticed`, speaking wire protocol v3. It runs in development trust mode
  on loopback, or with SCRAM-SHA-256 over TLS and per-table grants.
- Single-table SQL: `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `DROP TABLE` and transaction
  control. Joins, aggregates, `GROUP BY`, `DISTINCT` and subqueries are refused with SQLSTATE `0A000`.
- Tables and their column definitions are recovered after a restart.
- Library pieces that are not yet a product feature: the schema language (LSL) tools, CRDT modules and sync
  codecs. The experimental WebSocket gateway serves the MCP tools over `POST /mcp` on the same port, and the
  `lattice mcp` stdio proxy reaches that endpoint on loopback. See [Agents and live collaboration](agents.md).

## Read next

- [Getting started](getting-started.md): build, run, connect.
- [Architecture overview](architecture.md): the crate layers and what each one does.
- [SQL subset](sql.md): what the SQL accepts and refuses today.
- [Agents and live collaboration](agents.md): what an agent client can use today.
- [Security model](security.md) and [Verification](verification.md).
- [How to contribute](../../CONTRIBUTING.md) and the [glossary](../glossary.md).

Source and issues: [github.com/moncifem/latticedb](https://github.com/moncifem/latticedb).
