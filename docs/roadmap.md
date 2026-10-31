# Roadmap

Where the project is, phase by phase. The sources of truth are the phase pages in [docs/gates](gates),
the [specification](spec/README.md), the [milestones](https://github.com/moncifem/latticedb/milestones)
and the [project board](https://github.com/users/moncifem/projects/1). This page summarises them; if it
disagrees with a gate page, the gate page is right and this page needs a fix.

Status below is what `main` does at `68843883` on 2026-10-31. Items marked *not on main* are planned, not built.

## Status

| Phase | Goal | State | Gate |
|---|---|---|---|
| 0 Repository and conventions | Workspace, CI, governance | Done (gate record dated 2026-10-28) | [phase-0](gates/phase-0.md) |
| 1 Foundations and evaluation | Durable log, simulation, schema language, verification instruments | Built; evidence still accumulating (nightly history, review package). Gate not signed | Open: [phase-1](gates/phase-1.md), [review](gates/phase-1-review.md) |
| 2 Storage engines and SQL | MVCC row engine, SQL over the PostgreSQL wire | In progress. On main: single-table `SELECT` (with `ORDER BY`, `LIMIT` and `OFFSET`), `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `DROP TABLE`, index scans for primary key and `UNIQUE` predicates, durable table definitions across restart (RFC 0012 and ADR 0028 accepted; the seven-day comment hold was waived by the owner in chat, see #712), SCRAM over TLS and loopback trust, the `psql` suite plus node-postgres, pgx and pg8000 smoke scripts in CI (no psycopg: LGPL), the container image, the `lattice account` tool, `lattice log repair` (torn tail only, with a verified backup; interior damage and wrong-LSN frames refused, #441). *Not on main:* joins, aggregates, `GROUP BY`, `DISTINCT`, subqueries (#380, #592 to #597), the driver matrix (#413), binary formats (#608), `ALTER TABLE` and online schema change, branches, checkpoints | Open: [phase-2](gates/phase-2.md) |
| 3 Sync, CRDTs and SDKs | Live subscriptions, offline merge, SDKs, agent plugin contract | Started. On main: CRDT building blocks, change-feed and sync codecs, principal-bound subscriptions, plugin contract v1, MCP tools `lattice_read`, `lattice_write` and `lattice_subscribe` over the SQL executor, served over HTTP at `POST /mcp` on the gateway port (bearer token; TLS and tokens required off loopback; `--gateway-allowed-host`; with the Host, Origin, JSON-only and bounded-body checks of #580), the `lattice mcp` stdio proxy (loopback plaintext only), `lattice watch table/NAME` on a loopback gateway with a cursor file (#583), SDK outbox, reconnect backoff and a pure reconnect supervisor (`lattice-sdk::reconnect`) that `lattice watch` retries through, a two-principal MCP end-to-end test (`lattice-e2e`), an experimental WebSocket gateway (`latticed --gateway-listen`, TLS on the gateway port, bearer tokens with reload; read only, rows only), experimental TypeScript and Python codec clients. *Not on main:* the stdio proxy over TLS or a remote gateway (the CLI has no TLS client), the per-principal long-poll cap for MCP (#529), the gateway idle deadline (#578), the remaining #580 acceptance items (its Host, Origin and token checks are on main; the issue stays open until its list is closed), the `lattice watch` TLS client and a live-gateway test (#583), the SDK driver with its outbox pump (#620), live queries, collaborative documents | Open: [phase-3](gates/phase-3.md) |
| 4 to 12 | Identity and policy, blobs and S3, schema v1 and packs, search, intelligence, role packs, cluster, optional models, 1.0 | Not started | None yet |

Phase 2 runs ahead of the Phase 1 gate under the waiver in [docs/gates/phase-1-review.md](gates/phase-1-review.md)
(granted 2026-10-28). That waiver covers Phase 2 only: no Phase 2 release tag and no Phase 2 exit-criteria claim
before the Phase 1 gate is signed. Phase 3 started on 2026-10-29 under the same conditions as that waiver,
as [recorded in its gate page](gates/phase-3.md); no Phase 3 release tag and no Phase 3 exit-criteria claim
before the earlier gates are signed.

## What you can do today

Start `latticed --data-dir DIR --development-policy allow-all` (loopback trust) or the SCRAM-over-TLS
mode, connect with `psql` or another PostgreSQL client, and run single-table SQL against a write-ahead log.
Tables and their column definitions come back after a restart. The `lattice account` tool provisions the
credential files that `latticed` reads: SCRAM verifiers, account policies and gateway bearer tokens.

The experimental sync gateway runs when `--gateway-listen` is given, and serves the MCP tools at `POST /mcp`
on the same port. Plaintext is loopback only; a non-loopback bind needs `--tls-cert`, `--tls-key` and
`--gateway-tokens`. The Docker dev stack in [dev](../dev) runs the published image with SCRAM over TLS on
`127.0.0.1` for maintainers and their agents. Its gateway and `POST /mcp` are opt-in: `LATTICE_DEV_GATEWAY_PORT` publishes them over TLS on `127.0.0.1` (`0b282aab`, PR #800), and the image needs gateway TLS (a build at or after `4b297405`, or `LATTICE_DEV_IMAGE`). A shell helper and schema for a shared
agent task board run over `psql` in [integrations/board](../integrations/board/CONVENTIONS.md). See the
[README](../README.md) and the [alpha guide](alpha.md) for commands and limits.

## What comes next

1. Alpha 0.1 (milestone): `v0.1.0-alpha.1` is tagged. The milestone still holds the install script for the
   release archives (#585), the quickstart from the built archive (#586), the release checklist and rehearsal
   (#591, #568), `lattice watch` TLS client and live-gateway test (#583; the loopback network loop is on `main`, `6cb2bce6`), the gateway idle deadline (#578; TLS on the gateway landed in `4b297405`), the browser and DNS-rebinding defences
   for MCP over HTTP (#580, part 2 of #527; its Host, Origin and token checks and `--gateway-allowed-host` (#774) are on main, the rest of its acceptance list is open), the per-principal output and executor budgets (#529 part 2; the connection and long-poll caps of part 1 are on main, PR 861), the two-agent demo (#534; the harness (#540, on main as `8a7bde20`) and its two-principal MCP test (`475b3a01`) are on main), the
   security disclosure refresh (#590) and the alpha documentation (#589). Tags after `v0.1.0-alpha.1` are the
   owner's decision (#568).
2. SQL: the `kill -9` restart proof with the real binary (#574), the driver matrix and binary formats
   (#413, #607, #608, #710), then joins, aggregates, `GROUP BY`, `DISTINCT` and subqueries (#380, #592 to #597).
   The in-house planner is chosen ([ADR 0025](adr/0025-sql-planner-in-house.md)).
3. Alpha 0.2 (dogfooding milestone): the commit-lint scope fix (#736), a CI job for the dev stack (#737; `dev-stack.yml`
   is on main and is not a required check) and the dev stack follow-ups (#738; the account remove and drain
   changes are on main in `7f1320d7`).
4. Phase 1 evidence: more nightly passes, mutation score, Linux runtime numbers, two-maintainer sign-off.
5. Draft formats (plugin contract, change feed, sync protocol) go through an RFC, golden files and an
   independent review before they are frozen.

## Work tracks

- Phase milestones hold one issue per testable work package, created when the previous phase nears its
  gate. Phases 4 to 12 therefore show only their phase issue for now.
- The *Engineering platform* milestone holds merge safety, governance and verification tooling.
- The *Alpha 0.1* milestone holds the alpha work after the first pre-release. Its definition and critical path
  are in #567, its release checklist in #568.
- The *Alpha 0.2: dogfooding* milestone holds the fixes that the maintainers' own use of the dev stack and the
  agent board turns up.

## Keeping this current

Every pull request that changes what works, how to run it or the status updates the README, the gate
page for its phase, the changelog and, when a phase changes state, this page. The owner signs gates; no
page claims a gate passed before that.
