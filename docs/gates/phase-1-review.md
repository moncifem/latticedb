# Phase 1 gate review package

Status: **open, decision pending.** This is the record the gate review fills in. The criteria and
their measured status live in [phase-1.md](phase-1.md); this page holds what a review adds to them.

## Decision

The gate is **open**: the criteria below are not closed, and the gate is not signed.

### Waiver: Phase 2 work may start before the gate is signed

Granted by the project owner on 2026-10-28. Reason: the open criteria need calendar time (seven nightly
runs, fuzz hours), not engineering effort, and waiting would leave about a week idle.

Conditions, all of which stand until the gate is signed:

- Phase 2 work happens on branches and lands on `main` only with the same checks as any other change.
- A failing Phase 1 nightly, weekly or gate job takes priority over Phase 2 work and is fixed first.
- Nothing in Phase 2 may weaken a Phase 1 check, loosen a threshold or edit the Phase 1 gate rows to
  make them look better.
- No Phase 2 release (tag) before the Phase 1 gate is signed. Nightly pre-releases continue.
- Phase 2 exit criteria are not claimed before the Phase 1 gate is signed.
- If a Phase 1 criterion is found to be unmet in a way that invalidates Phase 2 work, Phase 2 stops
  until it is fixed.

The open items include calendar-time evidence, unfinished engineering, and a maintainer decision.
The table records those separately; collecting an evidence package does not sign the gate.

| Open criterion | What closes it | Needs |
|---|---|---|
| E1-2 seven nights at 1,000,000 seeds | seven consecutive green nightly runs | time |
| E1-6 24 hours of fuzzing per target | cumulative fuzz time per target, kept as a corpus across nights | time, a persisted corpus |
| E1-13 loom | not applicable today: the kernel and pipeline are single-threaded, so there is no shared-memory hand-off to model; a loom test is required with the first threaded runtime component | waiver, owner core maintainers, expiry at the first threaded component |
| E1-14 traceability | I3, I7, I10, I11, I14 belong to later phases and are waived with an owner and expiry | waiver |
| E1-15 scorecard | thresholds and a seven-run trend exist; automatic nightly ledger ingestion and the remaining scorecard sections are incomplete | engineering |

Signatures: two maintainers sign when the table is empty or every row has a waiver. Not signed.

## Waivers

The Phase 2 start waiver above is granted. The deferrals below are only proposals for future-component
evidence; they do not relax invariant correctness. A maintainer must decide their scope at review. Proposed for E1-14, because the invariant is
about a component that does not exist until the named phase. Each waiver expires at that phase's gate,
where the invariant needs its checker in the simulator and a row in the traceability matrix.

| Invariant | Why it cannot be checked now | Owner | Expires at the gate of |
|---|---|---|---|
| I3 snapshot isolation | early row snapshots exist; full Phase 2 fault/soak evidence remains | core maintainers | Phase 2 |
| I7 no policy bypass | needs the policy layer and its read and write paths | identity and policy maintainers | Phase 4 |
| I10 shredding | needs entity keys and derived data | identity and policy maintainers | Phase 4 (blob content at Phase 5) |
| I11 blob integrity | needs the blob engine | engines maintainers | Phase 5 |
| I14 time answers are exact | needs the temporal index to compare with a log scan | engines maintainers | Phase 7 |

I5, I8 and I13 have named tests today but no simulator checker; they are listed in the traceability
matrix and are not waived.

## North-star re-run

| Scenario | Result |
|---|---|
| NS1 crash safety | passes at the log level over every fault profile (`north_star.rs`) |
| NS2 to NS8 | pending specs with owning phases, listed by `cargo xtask scorecard` |

## Risk register changes

| Id | Change |
|---|---|
| R2 simulation discipline | Mitigations held: 21 planted bugs detected, determinism checked at 10,000 seeds per profile. An independent review still found two kernel bugs the simulator could not reach because the workload did not drive the manual transaction lifecycle; the workload now does. Early signal stays: any bug found outside the simulator |
| R8 performance | A `ci-ubuntu` runner baseline and nightly regression gate now exist. Shared-runner noise requires a 3x slowdown threshold, so smaller regressions can still go undetected |
| R13 (new) evidence that waits on time | Seven-night and 24-hour criteria cannot be met by effort. Mitigation: start the clocks early (nightly and weekly jobs run now). Results are kept as workflow runs and a persisted fuzz corpus; recording nightly runs in the ledger is not done yet |
| R14 (new) recovery memory | Streaming recovery is implemented and used by the row store. Legacy collecting opens remain, derived state still grows with history, and checkpoint-based recovery with measured bounds is incomplete |
| R15 (new) release pipeline drift | Third-party actions are pinned by hash and bumped by Dependabot. Mitigation: re-run the nightly build after every action bump |

## Evidence index

- Independent review (two rounds) and its outcomes: [phase-1.md](phase-1.md).
- Mutation results: log, kernel, hlc, record, changeset, crc, txn, scrub, lsl with no surviving mutants in the recorded scope. New runtime/engine code is not covered by that historical score.
- Runtime comparison: [Linux run 40789753956](https://github.com/moncifem/latticedb/actions/runs/40789753956), raw JSON and limitations in ADR 0011.
- Review bundle: `cargo xtask gate --phase 1 --output target/gates/review-1`; this collects evidence and leaves every criterion pending review.
- Models: `tla/`, run by `tla/check-all.sh` on TLC 1.7.4.
- Release: nightly pre-release and image, provenance verifiable with `gh attestation verify`.
