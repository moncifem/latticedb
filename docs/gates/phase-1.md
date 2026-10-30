# Phase 1 status (gate NOT passed)

Updated: 2026-10-29. Phase 1 is partly built. This page says exactly what exists, what was measured, what
an independent review found, and what is missing. The gate in `docs/spec/01-foundations.md` is **not**
met.

## Independent review and what happened to it

The review of the first Phase 1 build found 31 issues, several of them real correctness bugs that the
simulator at that time could not see. All were triaged; the harness was strengthened first so the fixes
are exercised.

| Finding | Outcome |
|---|---|
| Typestate let one kernel promote another kernel's transaction; prepared transactions bypassed later fences and id reservations; HLCs could be assigned out of log order | Fixed: transactions carry an owner token and `sync` and `apply` also require the id to be in flight at the claimed LSN (the token comes from the injected RNG, so kernels with the same seed share it); duplicate and fence checks and HLC assignment happen at the log append (the linearization point); `sync` verifies durability per transaction |
| A repeated id in one batch was acknowledged even when the original's fsync failed | Fixed: repeats share the original's fate; planted bug `BatchDuplicateAckedEarly` proves the harness catches the old behaviour |
| Writer could acknowledge records the decoder rejects (over 16 MiB); `ChangeSet::encode` silently dropped parents above 65,535 | Fixed: size checked before an LSN is taken; `encode` is fallible |
| Recovery adopted visible-but-unsynced bytes; WAL directory creation lacked parent durability; two writers could share a log | Fixed: recovery fsyncs before returning, parents are synced, an exclusive `Io::lock` is held for the log's lifetime. Planted bugs `SkipRecoverySync` and `NoParentDirSync` are caught |
| Recovery truncated a checksum-valid last record with a skipped or repeated LSN as if it were a torn tail (issue 441) | Fixed: recovery refuses it without touching the file and scrub reports mid-log damage. Genuine torn tails (incomplete or checksum-failing frames) are still repaired. A log that earlier versions "repaired" this way had already lost the record; it now fails loudly. Planted bug `GapTailTruncated` is caught by the focused refusal witnesses (the simulator never writes a gap) |
| Power-loss shapes that are not a prefix of the unsynced bytes (out-of-order page persistence, zero-filled tails, a zero-filled new segment header) were never generated, so recovery was only tested against prefix tears (issue 442) | Modelled: the `non-prefix` fault profile of `SimIo` produces them; focused tests in `lattice-core` derive the expected outcome of each shape from the bytes written (truncate a torn tail, refuse when a valid record follows the damage, refuse a complete but invalid newest header, rewrite a header shorter than 16 bytes) and the scenario applies the documented operator procedure after a refusal and still requires every acknowledged record. Recovery itself is unchanged: such a power loss can leave a log that refuses to open until an operator follows `formats/wal-v1.md` (the refusal loses no acknowledged data). Planted bug `ZeroFilledTailKept` is caught only by the new profile. Nightly runs the profile (4 shards of 250,000 seeds); no nightly result is recorded yet |
| Recovery skipped malformed known records | Fixed: corruption error |
| Entropy failure silently produced zero ids | Fixed: the process aborts |
| Torn-tail heuristic can mistake an embedded record for later data; CPU amplification | Bounded (plausibility filter before checksums) and **documented** as a conservative refusal (`formats/wal-v1.md`, ADR 0007). Not eliminated |
| LSL stack overflow on deep nesting; `..` range edge case; compat verdict ignored new types | Fixed: input size and nesting limits, checked range arithmetic, new types make a change forward-incompatible |
| HLC exhaustion regressed; drift bound only on `observe` | Fixed: exhaustion is an error; the doc now says only `observe` enforces drift |
| Harness blind spots: crashes only between steps, 19 percent of runs cut short and uncounted, one rot event disabled checks for the rest of a run, truncate modelled as durable, writes all-or-nothing, trace hash too coarse, oracle checked payloads only, no in-doubt retries | Fixed: power cuts inside durability protocols (including recovery), a terminal recovery on a fault-free disk for every run, censored runs counted and capped by quotas, rot exempts only data that existed when it struck, unsynced truncate and partial appends modelled, trace hashes all data, whole-envelope oracle, in-doubt and in-batch duplicate workloads |
| Evalkit: interval estimated a different statistic than the point estimate; dataset hash collision; gate accepted NaN, empty or inconsistent evidence; nDCG ignored omitted judgments; report escaping | Fixed (tests for each, including the exact collision found) |
| `Commit.tla` assumes the disputed guarantees | Not extended. Header states its limits. It did not parse until it was first run through TLC; it now passes |
| Hash chain cannot see suffix deletion | Fixed: `verify_with_head`; the limitation is documented |
| "Parser never panics" claim | Corrected: input and nesting are bounded and property-tested |

## What exists and is verified (evidence: `cargo xtask ci`, `cargo xtask sim`)

| Area | State |
|---|---|
| Ids, HLC, error taxonomy | Done, with property tests |
| Seams `Clock`, `Rng`, `Io` (now with locks), `Spawner`, `Net` and real `SystemClock`, `OsRng`, `StdIo`, `ThreadSpawner` | Done except a real `Net`. One `Io` conformance suite runs against real and simulated storage |
| Simulator | `SimIo` with crash, torn writes and garbage tails, unsynced truncates, partial appends, append and fsync errors, disk full, bit rot, lost directory and file entries, **power cuts inside operation sequences**, process-only restarts, locks; `SimNet`; trace hash over all data and outcomes |
| WAL and kernel | Segmented log, CRC32C, group commit, rotation, recovery with the torn-tail rule, poisoning, idempotency, fencing, typestate with owner binding and a compile-fail test |
| Format | `formats/wal-v1.md`, `changeset-v1.md`, `hlc.md`; golden file from an **independent Python implementation** decoded and re-encoded byte-exactly in CI |
| Invariants | I1 (whole envelope), I2 (log level), I4, I6, I12 as `Invariant` implementations; I13 hash chain primitive |
| CRDTs | LWW register and OR-set, merge laws and convergence property tests |
| LSL v0 | Parser (bounded), checker, validator, compat checker, formatter |
| Evalkit | Hash-checked datasets (BLAKE3, framed), P/R/F1 with a same-statistic bootstrap, ECE, nDCG, validated gate, escaped reports, mutation-based drift generator, gate self-test |
| Testing the tests | **21 planted bugs**, all caught under the expected invariant (most at seed 0); zero violations on the clean build. Adding them exposed two blind spots, now closed: the simulator did not model dirty pages dropped after a failed fsync, and nothing compared the kernel's durable LSN or derived state with the log during a run (invariant I9). The workload also drives the manual transaction lifecycle (log, checkpoint attempt, sync, apply), added after an independent review found the harness never did |

Measured on the strengthened harness: `cargo xtask sim --seeds 10000 --steps 150` ran **50,000**
scenarios (5 profiles x 10,000 seeds) with **0 violations**, 1,899,450 recoveries and **832,021 power cuts
inside durability protocols**. 159 workloads were cut short by faults (all still verified by the terminal
check) and 1,767 runs, all in the bit-rot profile, were **incomplete** (recovery refused a rotted log, so
no verdict): they are reported, capped by a quota test, and not counted as passes. This was measured on
the harness before the kernel fixes in PR 61; the nightly run is the current evidence.

## Exit criteria against the spec

| # | Criterion | Status |
|---|---|---|
| E1-1 | 10,000 seeds reproduce identical trace hashes | Met: `determinism_at_gate_volume` ran 10,000 seeds x 5 profiles, each twice, with identical traces (210 s in release); nightly repeats it and pull requests check 60 seeds per profile |
| E1-2 | 1,000,000 seeds per night for 7 nights | Not met yet: nightly runs 1,000,000 seeds per profile (5 profiles x 4 shards of 250,000, a new seed window each day) and fails on any violation; 2 of 7 nights passed with no violations (manual run 2026-10-28; first scheduled run 2026-10-29, every job green); the scheduled runs continue the count |
| E1-3 | At least 20 planted bugs, 95 percent detected, no false alarms | Met: 21 bugs, all detected, no false alarms |
| E1-4 | Recovery correct at every fsync boundary in 100 scenarios | Met: `inv_i1_hundred_workloads_swept_at_every_power_cut_point` cuts the power at every storage operation of 100 distinct workloads (4,729 cut points, all recovered with every acknowledged change set present); nightly repeats it, and pull requests sweep 6 workloads. The sweep and the five standard profiles only produce prefix-shaped tears; out-of-order and zero-filled power loss is covered by the separate `non-prefix` profile (issue 442) |
| E1-5 | CRDT laws at 10,000 cases (PR) and 1,000,000 (nightly) | Met: pull requests run every property test at 10,000 cases (`PROPTEST_CASES`), nightly runs the core, abi and lsl crates at 1,000,000. The text sequence CRDT (`Rga`) is a spike with convergence tests over a faulty network |
| E1-6 | 24 hours of fuzzing per target | Met: a manual campaign (`fuzz-24h.yml`) ran six targets (`changeset_decode`, `id_parse`, `log_open`, `lsl_parse`, `pgwire`, `wal_record`) in five seeded shards of 5 h 40 min each, 28 h 20 min per target, with no crash on any of the 30 jobs; that campaign ran with libFuzzer's default `-timeout` and `-max_len` (4096); the scheduled workflows now set both per target (`fuzz/settings.toml`) and the nightly job keeps its corpus, none of which has completed a scheduled run yet. The nightly (10 minutes) and weekly (5 hours) runs continue to extend the corpus |
| E1-7 | Mutation score at least 80 percent | No verified complete score for the current declared scope. [Run 40864349680](https://github.com/moncifem/latticedb/actions/runs/40864349680), source `a88d1ed1670ba0828c2c3689a83bfba7e7043a72`, finished with failure; its authenticated plan contains 1,448 mutations across 62 shards (32 dependent, 30 self). Interrupted shard outcomes and mismatched internal baseline test scope prevent treating the observed subset as a complete passing score. Matching-scope baseline proof is required by PR 258; old artifacts retain diagnostics without verified scores. Survivor triage remains open under #42. Survivors observed in that run were re-checked on current source: those in `log.rs` were mostly killed by later tests, two guarding a planted bug that is off in a clean build are excluded with that reason, and three in the simulator's disk-full counter are killed by a new test; none of this counts as a score until a fresh complete campaign passes. The separately recorded seven-regression proof below passes for its exact selection only. Earlier per-crate history follows. Earlier hlc, record, changeset, crc, txn, scrub, lsl, log, kernel, abi, evalkit, sim, seams, cli, bench and sdk have no surviving mutants (log and kernel: 198 detected of 198 viable, 53 of them as hangs; 39 unviable). Mutants that cannot change behaviour are excluded in `.cargo/mutants.toml`, each with its reason (disjoint-bit `or`, a redundant guard, a body that already equals its replacement); the benchmark timing loops in `lattice-bench/src/suite.rs` are excluded as a whole, so a change to what a benchmark measures is watched by the nightly baseline gate, not by mutation testing. abi and evalkit: 81 survivors found and killed with exact-value and golden tests (290 caught, 16 unviable, 0 missed). sim and seams: 81 survivors killed with model-pinning tests and goldens (191 caught, 0 missed after exclusions). cli, bench and sdk: 61 survivors killed with tests that run the real binaries (81 caught, 0 missed; benchmark timing loops excluded as unpinnable). `xtask` (repository tooling that CI runs on every change, not shipped) is out of scope: an interrupted run left about a third of its mutants surviving |
| E1-8 | Eval gate self-test, byte-identical reports, reproducible intervals | Met |
| E1-9 | Benchmark baselines and regression gate | Met: the gate fails without a baseline; `ci-ubuntu` is a baseline recorded on a GitHub-hosted runner (`bench-baseline` workflow) and nightly compares against it with a 3x tolerance because shared runners are noisy; the `local` profile covers one developer machine |
| E1-10 | Format ADR accepted, corpus read in CI | Met for the WAL and change set |
| E1-11 | TLC passes four models | Met for the abstractions: Commit, Hlc, Fence, SetConvergence pass TLC, and Fence, Hlc and SetConvergence were shown to fail when the protection is removed (fence guard, counter advance, tombstone rule). Each header states what it does not model; run nightly by `tla/check-all.sh` |
| E1-12 | Runtime ADR with benchmark data | Met: ADR 0011 records portable macOS/Linux measurements and the opt-in Linux portable/Tokio/io_uring comparison. [Run 40789753956](https://github.com/moncifem/latticedb/actions/runs/40789753956) passed all three backends, 36 measurement rows, conformance/recovery checks and the completion-error regression. Raw JSON is committed; the synchronous production runtime is unchanged |
| E1-13 | Unsafe discipline, Miri, loom | Partial: no unsafe outside `lattice-sys`. Its opt-in Linux runtime prototype has one reviewed submission block, covered by Linux conformance and a completion-error regression, not a Miri syscall proof. Miri still runs the abi/core tests; the threaded listener and cancellation tokens now need shared-memory evidence; bounded registry lifecycle models passed (scope below), and host admission/engine concurrency remain separate |
| E1-14 | Traceability matrix complete | Partial: I1, I2 (log level), I4, I5, I6, I8, I9, I12 and I13 each have a named test (`cargo xtask traceability`); I3 has early row snapshot tests but still needs full Phase 2 evidence. I7, I10, I11 and I14 belong to later phases |
| E1-15 | Scorecard from a results ledger | Partial: runs append to a ledger stored in Lattice's own log; `cargo xtask scorecard` prints the entries and the pending specs, then `lattice ledger check` fails when the latest simulation run has violations, no runs or mostly unverified runs and reports the streak and trend over the last seven simulation runs; nightly artifact/ledger ingestion and ten observed-or-missing sections are implemented, with retained-history provenance and completed-fuzz summaries. Integrated nightly validation is still pending; mutation/eval ingestion is implemented with exact producer fields, but completed project producer evidence, reviewed risk/coverage classifications and product dogfood measures remain incomplete |
| E1-16 | Gate review package | Review not met: `cargo xtask gate --phase 1` now collects declared criteria, source/document provenance, invariant traceability and optional raw/verified ledger snapshots. Every criterion stays unverified until reviewed; no signature or phase-pass decision is generated |

## Kani bounded proofs (not a gate criterion)

Kani harnesses for the change feed cursor, the sync frame decoder, the plugin line splitter and the
tree CRDT cycle check are merged (PR 296) and run only through the `kani` workflow (weekly and on
demand, results uploaded as artifacts). Bounded proofs: 4 of 10 harnesses verified on CI (the `lattice-core` feed cursor and event decoder
proofs, four consecutive runs); 6 not proven (the tree cycle check timed out, the `lattice-api`
sync frame and `lattice-sdk` framer harnesses were killed on the hosted runner; no counterexample was
found). Tracked in #424 and #425. The broken-variant demonstration has not been run. Details, bounds and run links are in
[docs/verification.md](../verification.md).

## Not built

Snapshot cursors over derived state and checkpoint-based recovery start (both need engine state from
phase 2), current-source mutation coverage of new engine/runtime modules, the
seven-night simulation history, and the final signed gate review.

North-star scenarios: NS1 runs at the log level in `crates/lattice-testkit/tests/north_star.rs`; NS2 to
NS8 exist there as pending specs with their owning phase and are listed by `cargo xtask scorecard`.


## Threaded cancellation model coverage (2026-10-29)

The development listener now uses shared-memory cancellation state, so the earlier lack-of-threading
rationale no longer closes this part of E1-13. Four Loom models use the actual registry/token code,
substituting instrumented Arc/AtomicBool and an external model mutex. The standalone model tool
compiles the actual source by path and keeps its dependencies outside the shipped workspace. They cover cancellation versus
query replacement, unregister versus query start, stale keys versus slot reuse, and a negative control
that must find a lost cancel action. Model selection is checked before CI execution; no execution or
preemption subset is configured. [Hosted run 40898872693](https://github.com/moncifem/latticedb/actions/runs/40898872693),
attempt 1 at `498050234d942ef4dfbfc8a53fa8bb8256aeb3b0`, passed all four selected models, including
the required negative control. The isolated tool dependency audit passed too. Its retained artifact
SHA256 was independently verified against GitHub metadata; see the [proof record](../evidence/verification-2026-10-29.json).

This does not model sockets, the host closing/admission latch, or engine concurrency and does not
sign the gate. The kernel and pipeline remain synchronous; their existing Miri/unsafe checks stand.


## Targeted recovery mutation follow-up (2026-10-29)

[Run 40900457911](https://github.com/moncifem/latticedb/actions/runs/40900457911), attempt 1 at
`83590cf893440698e844604bb4e76aed730fddd2`, tested seven selected default-feature `lattice-core`
mutations with cargo-mutants 27.1.0: **seven caught, zero missed, zero timed out, zero unviable**.
The explicit package baseline and internal unmutated baseline passed. These pin small-offset scan,
immediate-successor refusal, exact torn-tail repair counts and recovered durable-LSN behavior after
PRs 218/224. Plan and shard artifact digests were checked against GitHub metadata and are retained
in the proof record above. The earlier 40-selection result and failures remain documented in
[the metadata follow-up](../mutation-recovery-metadata.md).

This closes those seven measured gaps, not WP17 or the full E1-7 criterion. The full weekly campaign,
new runtime/engine coverage and the separate redundant-framing-precheck candidate remain open. No
mutation exclusion, timeout budget, simulation threshold or gate signature changed.

A [manual paired foundation diagnostic](../foundation-comparison.md) is available to compare the
original reference and fixed candidate on one host. Its new workflow has no measured result yet; it
does not change E1-9, sign a gate, or fill the missing Phase 1 SQL comparison in ADR 0013.
