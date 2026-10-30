# Roadmap

Where the project is, phase by phase. The sources of truth are the phase pages in [docs/gates](gates),
the [specification](spec/README.md), the [milestones](https://github.com/moncifem/latticedb/milestones)
and the [project board](https://github.com/users/moncifem/projects/1). This page summarises them; if it
disagrees with a gate page, the gate page is right and this page needs a fix.

## Status

| Phase | Goal | State | Gate |
|---|---|---|---|
| 0 Repository and conventions | Workspace, CI, governance | Done | [phase-0](gates/phase-0.md) |
| 1 Foundations and evaluation | Durable log, simulation, schema language, verification instruments | Built; evidence still accumulating (nightly history, review package) | Open: [phase-1](gates/phase-1.md), [review](gates/phase-1-review.md) |
| 2 Storage engines and SQL | MVCC row engine, SQL over the PostgreSQL wire | In progress: storage, wire, auth and parser exist; SQL planning and execution do not | Open: [phase-2](gates/phase-2.md) |
| 3 Sync, CRDTs and SDKs | Live subscriptions, offline merge, SDKs, agent plugin contract | Started: building blocks that do not need the server; experimental TypeScript and Python codec clients (`clients/typescript`, `clients/python`) exist | Open: [phase-3](gates/phase-3.md) |
| 4 to 12 | Identity and policy, blobs and S3, schema v1 and packs, search, intelligence, role packs, cluster, optional models, 1.0 | Not started | None yet |

Phases 2 and 3 run ahead of the earlier gates under a documented, owner-confirmed waiver. The waiver
forbids release tags and exit-criteria claims for those phases until the earlier gates are signed.

## What you can do today

Open a data directory with the kernel and the row engine, replay crashes in the simulator, check
schemas with the LSL tools, run the development listener and connect with a PostgreSQL client
(`SELECT 1` only). There is no storage-backed SQL yet. See the [README](../README.md) for commands.

## What comes next

1. Storage-backed SQL: planner decision, executor, wiring to the row store, driver conformance.
2. Phase 1 evidence: more nightly passes, mutation score, Linux runtime numbers, two-maintainer sign-off.
3. Phase 3 packages that need the server: subscriptions over a gateway, live queries, the SDK clients.
4. Draft formats (plugin contract, change feed, sync protocol) go through an RFC, golden files and an
   independent review before they are frozen.

## Work tracks

- Phase milestones hold one issue per testable work package, created when the previous phase nears its
  gate. Phases 4 to 12 therefore show only their phase issue for now.
- The *Engineering platform* milestone holds merge safety, governance and verification tooling.

## Keeping this current

Every pull request that changes what works, how to run it or the status updates the README, the gate
page for its phase, the changelog and, when a phase changes state, this page. The owner signs gates; no
page claims a gate passed before that.
