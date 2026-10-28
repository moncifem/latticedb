# Phase 1 gate review package

Status: **open, decision pending.** This is the record the gate review fills in. The criteria and
their measured status live in [phase-1.md](phase-1.md); this page holds what a review adds to them.

## Decision

No-go to Phase 2 until the open criteria below close. Waiting items need calendar time, not work.

| Open criterion | What closes it | Needs |
|---|---|---|
| E1-2 seven nights at 1,000,000 seeds | seven consecutive green nightly runs | time |
| E1-6 24 hours of fuzzing per target | cumulative fuzz time per target, kept as a corpus across nights | time, a persisted corpus |
| E1-9 benchmark baselines | a baseline recorded on the CI runner class, not a laptop | decision on runner hardware |
| E1-12 runtime ADR with data | ADR 0011 is accepted with macOS data; the Linux numbers come from the weekly job and must be read into the ADR | one weekly run |
| E1-13 loom | not applicable today: the kernel and pipeline are single-threaded, so there is no shared-memory hand-off to model; a loom test is required with the first threaded runtime component | waiver, owner core maintainers, expiry at the first threaded component |
| E1-14 traceability | I3, I7, I10, I11, I14 belong to later phases and are waived with an owner and expiry | waiver |
| E1-15 scorecard | thresholds exist (`lattice ledger check`); a trend over the ledger remains | work |

Signatures: two maintainers sign when the table is empty or every row has a waiver. Not signed.

## Waivers

None granted yet; a maintainer grants them when signing. Proposed for E1-14, because the invariant is
about a component that does not exist until the named phase. Each waiver expires at that phase's gate,
where the invariant needs its checker in the simulator and a row in the traceability matrix.

| Invariant | Why it cannot be checked now | Owner | Expires at the gate of |
|---|---|---|---|
| I3 snapshot isolation | needs engines with snapshots (MVCC) | core maintainers | Phase 2 |
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
| R8 performance | Baselines exist only per machine. Raised to high until a baseline exists on the CI runner class |
| R13 (new) evidence that waits on time | Seven-night and 24-hour criteria cannot be met by effort. Mitigation: start the clocks early (nightly and weekly jobs run now). Results are kept as workflow runs and a persisted fuzz corpus; recording nightly runs in the ledger is not done yet |
| R14 (new) recovery memory | `Log::open` returns every record in memory, so recovery memory grows with the log. Mitigation: streaming recovery in Phase 2 before engines depend on it |
| R15 (new) release pipeline drift | Third-party actions are pinned by hash and bumped by Dependabot. Mitigation: re-run the nightly build after every action bump |

## Evidence index

- Independent review (two rounds) and its outcomes: [phase-1.md](phase-1.md).
- Mutation results: log, kernel, hlc, record, changeset, crc, txn, scrub, lsl with no surviving mutants.
- Models: `tla/`, run by `tla/check-all.sh` on TLC 1.7.4.
- Release: nightly pre-release and image, provenance verifiable with `gh attestation verify`.
