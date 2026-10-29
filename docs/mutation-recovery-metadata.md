# Recovery metadata mutation follow-up

Part of #42; requirement P1-69.

[Focused run 40896914688](https://github.com/moncifem/latticedb/actions/runs/40896914688),
attempt 1, used source `a73dab2231ee849989dad1b311a7d2bfc34554c2` and cargo-mutants 27.1.0.
The two nonoverlapping round-robin shards selected 40 mutations: **34 caught, five missed,
one timed out, zero unviable**. Explicit package and internal unmutated baselines passed.
This is a selected default-feature core-package result, not a project mutation score.

Both producer ZIP digests were verified against GitHub artifact metadata:

| Shard | Artifact id | SHA256 |
|---|---:|---|
| focused-mutation-shard-0 | 12907332376 | `db4dc1755e234cff328863ff49e807e56bfc2a9e4289789fb0643a91edf18a1e` |
| focused-mutation-shard-1 | 12907747287 | `9633b63d56dfb516a4c9b7ff2c605e3fec9d282ffa7578d137d42d883dbb0fcd` |

The small-offset scan condition and immediate-successor regressions from PR 218 catch their
selected mutations in this expanded run. Four newly observed survivors change the arithmetic
used for `RecoveryReport::truncated_bytes` and the recovered durable LSN. Those public values
were not asserted exactly in the prior tests. A fifth survivor changes a framing precheck inside
`valid_record_after`; its redundancy with the decoder remains a separate review candidate.
No exclusion is added here, and a timeout is not reported as a caught result.

`recovery_metadata::repaired_tail_reports_exact_bytes_and_the_durable_prefix` now uses the public
Log API with three independently numbered records and a nine-byte torn suffix. It pins exactly
three recovered records, one segment, nine removed bytes, durable LSN 3 and next LSN 4. It checks
the preserved payloads and prefix bytes, then appends record 4 and checks that durable state and
visibility advance only on sync. No production implementation changes are needed.

The focused integration test and strict lint pass locally. Fresh hosted mutation verification of
these four metadata survivors remains pending; passing ordinary tests does not establish a kill.
The earlier failed planning run 40896129210 produced no mutation evidence. Its cache-output fix
is merged as PR 221, and this completed run used run/attempt-specific temporary output paths.


## Completed focused verification

[Run 40900457911](https://github.com/moncifem/latticedb/actions/runs/40900457911), attempt 1 at
`83590cf893440698e844604bb4e76aed730fddd2`, tested seven selected regressions with cargo-mutants
27.1.0: **seven caught, zero missed, zero timed out, zero unviable**. This includes all four metadata
arithmetic survivors above and the three prior scan/successor cases. Both package baselines passed.
The [compact proof record](evidence/verification-2026-10-29.json) preserves exact source and independently
verified plan/shard ZIP digests. The separate framing precheck candidate and full weekly campaign
remain unresolved; no complete project score or phase-gate pass follows from this selection.
