# Releasing

## Channels

| Channel | What it is | Cut by | Where |
|---|---|---|---|
| Nightly | `main` once a day, only if it moved and passes the full gate | `nightly-build` workflow | `nightly` pre-release, `ghcr.io/moncifem/latticedb:nightly` |
| Alpha, release candidate | `vX.Y.Z-alpha.N`, `vX.Y.Z-rc.N`: a tagged commit on `main` | pushing a tag | GitHub pre-release, image tagged with the version |
| Stable | `vX.Y.Z` | pushing a tag | GitHub release, image tagged with the version and `latest` |

Nightly is unsupported. Nothing before 1.0 promises format or API stability; each release states
format changes in its notes.

Scheduled verification is separate from packaging: the `nightly` and `weekly` workflows run the
simulator, fuzzing, model checking and heavier suites and report failures; they publish nothing.

## What a release contains

- `lattice-<version>-<target>.tar.gz` for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and
  `aarch64-apple-darwin`, each holding `lattice`, `latticed`, `LICENSE`, `NOTICE` and `README.md`.
- `SHA256SUMS` and CycloneDX SBOMs (`latticed.cdx.json`, `lattice-cli.cdx.json`).
- A build provenance attestation for every archive and for the container image.
- A multi-architecture image (`linux/amd64`, `linux/arm64`) at `ghcr.io/moncifem/latticedb`.

Verify a download:

```bash
gh attestation verify lattice-0.1.0-alpha.1-x86_64-unknown-linux-gnu.tar.gz --repo moncifem/latticedb
sha256sum --check --ignore-missing SHA256SUMS
```

## Cutting a release

1. Pick the version. The workspace version in `Cargo.toml` is the single source of truth.
2. In one pull request: set the version, run `python3 tools/changelog.py compile --version X.Y.Z --write`
   (it moves the `[Unreleased]` entries and every file in `changelog.d/` under a new `## [X.Y.Z] - date`
   heading and deletes the fragments; run it without `--write` first to preview), read the result, and
   update the README status if it changed.
3. After it merges, rehearse: run the `release` workflow by hand from `main`. It checks the version and
   changelog, runs the full gate and builds every archive without publishing.
4. Tag the merge commit and push the tag:

   ```bash
   git tag -s vX.Y.Z -m "Lattice X.Y.Z"
   git push origin vX.Y.Z
   ```

   The workflow refuses a tag that does not match the workspace version, is not on `main`, or has no
   changelog section.
5. Check the release page, the attestation and `docker run --rm ghcr.io/moncifem/latticedb:X.Y.Z --version`.

A bad release is superseded by a new patch version; tags are never moved or deleted.

## Packages

Binaries and the container image are published now. Crates are not published to crates.io:
every crate is `publish = false` until the library APIs are meant to be depended on from outside this
repository, which is a decision for the first minor version after the Phase 2 gate. Package
managers (Homebrew, Linux distributions) come after the first stable release.


## Phase 2 nightly verification

The verification workflow configures Miri for the deterministic `lattice-engines` library tests as
well as ABI/core libraries. AddressSanitizer includes engine and API library tests alongside the
existing unit and simulator suites. Real socket/process tests are not Miri targets. These workflow
entries describe coverage to execute, not evidence that a nightly run or a phase gate has passed.

The row fault volume suite is `crates/lattice-engines/tests/row_faults.rs` (not `row_sim.rs`). Nightly
partitions its original workload into five profiles × four shards: each shard runs 2500 distinct
seeds for 150 steps, preserving 10,000 seeds per profile and 50,000 total histories. Shards cover the
half-open ranges `0..2500`, `2500..5000`, `5000..7500`, and `7500..10000` for every profile, with no
new random source or environment reads in the test. The full ignored test remains available for a
manual aggregate run. The existing independent transaction-envelope oracle, fault-exercise checks,
censoring rules and terminal recovery checks are shared by full and sharded execution.

Each job first verifies that its exact named ignored test exists and selects one test; a stale
filter cannot turn zero executed tests into a passing shard. Raw selection and run logs are retained
as `evidence-row-faults-<profile>-<shard>` artifacts. The scorecard and scheduled failure reporter wait
for all row shards; job conclusions record failures. These raw row logs are not normalized simulation
seed totals in the scorecard. Bit-rot-censored histories remain incomplete and are not reported as
verified histories.

For a specific shard (shell test names replace profile hyphens with underscores):

```bash
cargo test -p lattice-engines --release --test row_faults gate_shards::crash_heavy_2 -- --ignored --exact --list
cargo test -p lattice-engines --release --test row_faults gate_shards::crash_heavy_2 -- --ignored --exact --nocapture
```

Run high-volume simulation, Miri and sanitizer jobs on hosted CI rather than the development laptop.
The Phase 1 sign-off and Phase 2 exit requirements remain independent of this pipeline configuration.

### Manual package mutation diagnostics

`mutation-crate.yml` defaults to the whole selected package and at most 16 nonempty
round-robin shards. `max_shards` accepts 1, 2, 4, 8 or 16; `filter_regex` optionally
selects mutant names for a focused diagnostic. For example:

```sh
gh workflow run mutation-crate.yml -f package=lattice-core -f max_shards=1 \
  -f 'filter_regex=Pipeline<I, C, R>::(drain|queued)'
```

The planner rejects unknown workspace packages and empty selections. Artifacts preserve
full and selected listings, exact assignments, SHA256 digests, run/attempt/source identity,
tool version and argument arrays. Every shard regenerates and checks its universe and
assignment, then runs `cargo test --package PACKAGE` before mutation execution. Baseline
command, status and logs are separate records; raw mutation outcomes and logs are uploaded
on failure as well. A started command without a terminal status is incomplete evidence.
Cargo-mutants stays pinned to 27.1.0 with a 120-second mutation timeout, two workers and a
350-minute job budget. A failed package baseline prevents mutation execution.

Filtered results establish evidence only for the recorded selection. This manual helper
is outside weekly ingestion and cannot establish a global passing mutation score. Its
explicit package baseline does not retroactively establish a matching workspace baseline
for older weekly runs; inspect the actual baseline and mutant test arguments separately.


Mutation diagnostic plans and shard outputs live under `runner.temp`, in run/attempt-specific
paths, outside the restored Cargo build cache. A stale cached `target/focused-plan` or
`target/focused-shard` cannot become current evidence or prevent a new output directory from
being created. The helper still refuses pre-existing output paths, and downloaded plans retain
source/run/attempt/digest validation. Run 40896129210 stopped at planning because the old output
path was restored from cache; it produced no mutation result and is not replacement proof.

### Cancellation registry concurrency models

`cancel-loom.yml` runs on API changes, by manual dispatch and as part of nightly.
It follows [Loom’s conditional instrumentation pattern](https://docs.rs/loom/0.7.2/loom/);
the model explores Loom-supported executions, not a complete processor memory-model proof. It selects
exactly four models before execution, retains selection/run logs and fails on any model failure.
`RUSTFLAGS="--cfg loom" cargo test --manifest-path tools/cancel-loom/Cargo.toml --locked --test cancel_loom` substitutes Loom's Arc and
atomic flag in the actual registry token code. Default builds retain standard synchronization.
The tests use a Loom mutex for the registry's documented external serialization contract.

The bounded two-worker models enumerate interleavings without a permutation, time or preemption
cap: query replacement versus cancellation, unregister versus query start, and stale keys versus
a reused slot. A negative control deliberately loses a cancel action and must expose the violation.
This checks the actual registry and token implementation; it does not model sockets, TLS, the host's
closing/admission latch, or concurrent engine access. A branch/CI timeout is a failure, not completed
model evidence. Run model enumeration on hosted CI rather than the development laptop.


The standalone `tools/cancel-loom` package compiles the actual registry source file by path; it
contains no copied registry implementation. Its tool-only lockfile keeps Loom's tracing dependencies
outside the shipped workspace graph, where the `lazy_static` ban remains unchanged. A dedicated locked
dependency audit applies to the model tool; this isolation is not a waiver for runtime dependencies.


## Daily million-history row-fault campaign

This configuration supersedes the initial 50,000-history nightly described above. The existing
20 bounded named tests and full ignored 10,000-seed-per-profile aggregate remain available for
manual regressions; their oracle and budgets are retained. E2-1's configured nightly workload is
now **1,000,000 observed profile/seed histories**, split into five profiles × eight shards × 25,000
histories, each with a 150-step budget. A numeric seed is reused across different fault profiles;
the distinct workload identity is the `(profile, seed)` pair. This is not 1,000,000 histories per
profile, 1,000,000 fully verified histories, seven successful nights, or a signed gate.

A host planning job freezes one UTC epoch day for the entire run, including queued shards crossing
midnight. For day `D`, each profile covers the half-open range `D*200000..(D+1)*200000`; shard `S`
covers `D*200000+S*25000..D*200000+(S+1)*25000`. Consecutive days have adjacent, non-overlapping
ranges within each profile. Integer overflow is rejected. Retrying a failed shard may reuse the
original plan day; the plan and execution attempt numbers remain separate in retained metadata.
Manual runs on the same day intentionally replay that day's window and do not count as extra nights.

The host-only `lattice-bench` example `row_fault_runner` parses explicit command-line inputs and
imports the actual deterministic `row_faults.rs` oracle. No date, environment, or argument reads
enter the engine workload. Its compiled preflight selection must match the complete 40-shard
campaign plan before execution, and successful execution must report exactly 25,000 terminal
observations. Empty, partial or malformed reports fail; a panic leaves counts unavailable rather
than inventing a completed score. `--plan-only` performs no histories. The explicit
`--diagnostic-seeds 1..128` mode is marked diagnostic-only and cannot be presented as nightly volume.

Each shard retains the complete plan, its compiled selection, runner binary SHA256, stdout/stderr
and JSON results under `evidence-row-faults-<profile>-<shard>`. Results include repository, revision,
run/attempt, UTC day, profile, shard, endpoints, step budget and observed/verified/rot-censored/
cut-short counts. Full-step verified histories are reported separately. Bit rot remains censored;
cut-short histories still receive the original terminal oracle check. These raw row results are
not silently added to the scorecard's existing simulation totals. The collector and failure reporter
wait for both the planning job and row matrix. The per-shard timeout remains 120 minutes.

The earlier 2500-history shards completed in 72–351 seconds including build in audited run
[40881355427](https://github.com/moncifem/latticedb/actions/runs/40881355427). Scaling to 25,000 histories
per shard is a capacity estimate, not a measured completion guarantee for this new campaign.
Actual million-history execution and seven-calendar-night evidence remain pending until observed.

At most eight row-fault shards execute together. All forty selections still run; this bounds
incremental runner contention with pull-request checks without reducing the million-history plan.
The central seed day remains frozen across queued waves and midnight boundaries.

### Weekly matching-scope mutation baselines

Each weekly mutation shard now tests an explicit clean baseline before mutation execution.
The dependent group runs `cargo test --workspace`, matching its workspace mutant tests.
The self group runs `cargo test --package PACKAGE` separately for every unique package in
that shard's actual assignment; combining these packages could change feature unification.
These commands use default features, as do the recorded mutation commands. A failed or
interrupted baseline prevents the shard from starting mutation execution.

The shard artifact retains `baseline.json` and `baselines/N.log`. The proof binds exact
source/run/attempt, group/shard, plan and universe digests, assigned mutant identities,
argument arrays, command state, exit codes and raw-log SHA256 digests. A command left in
`pending` or `started` state is incomplete evidence. The consumer validates every expected
scope and log before allowing a score; cargo-mutants' internal baseline must also pass.

Older artifacts without this proof retain observed caught, survived, timed-out and unviable
counts, but their matching baseline scope remains unverified and their score is unavailable.
New validation cannot establish a clean workspace baseline retrospectively for an older run.
This change does not alter the mutation universe, exclusions, budgets or schedule. The
manual focused workflow has its own package-only diagnostic baseline and remains outside
weekly score ingestion. Mocked fixture validation is not a completed hosted baseline or
mutation campaign; inspect fresh raw artifacts before making a measured coverage claim.

### Planner experiment semantic confirmation

The isolated `sql-planner-spike` comparison can be called through `io-probe.yml` with
`sql_planner=true` and `semantic_only=true` on its experiment branch. This explicit mode builds only
custom/parser, then checks conformance, simulator traces, PostgreSQL semantic cases and process-kill
recovery. Reports identify `semantic_confirmation`, exclude DataFusion execution, skip the four-query
performance comparison and suppress C4/C5 assessment. It supplies no clean-build or ADR-acceptance
claim. Default full measurement still selects all three candidates; cached functional runs remain
explicit and cannot replace clean-target C5 evidence. See `experiments/sql-planner/SEMANTICS_AUDIT.md`
for bounded coverage and remaining gaps.

### Candidate sync codec fuzz coverage

The `sync_frame` target is registered in nightly (ten minutes), weekly (five hours), and the manual
24-hour workflow (five shards of 5h40m). Those entries enable future execution; no completed fuzz
campaign or phase-gate evidence follows from adding the matrix rows. Codec bytes remain experimental
under RFC 0010 / proposed ADR 0024. The owner authorized experimental publication after final green
CI while keeping the format proposed; encoding acceptance, freeze and runtime activation remain
separate decisions.

### WAL admission evidence

The `wal-admission` pull-request workflow retains separate `evidence-wal-admission` and
`evidence-wal-admission-measurement` artifacts. It compiles the actual immutable old source with its
original lockfile and reviewed host harness; verifies source cleanliness and binary/input hashes;
and records raw/recovered cut manifests. The separate release-build measurement records warmed-cache
Log admission/open times, GNU time process RSS and bounded prefix controls. The
[completed source-specific record](evidence/wal-admission-40908208450.md) distinguishes PR head from
the actual tested merge source. Historical proof survives later rebases; inspect fresh-head checks
separately. The owner's [specific WAL acceptance/waiver](evidence/wal-admission-owner-decision-2026-10-29.md)
authorizes PR 222 only after final-head green CI. Jobs alone do not accept a format or waive a wait;
PR 209 acceptance/hold and LCIX activation remain separate.


### Managed-index integration proof

The `managed-engine-admission` job in `wal-admission` runs the 26-test managed selection (25 normal
semantic tests plus one separately selected hosted export), exports every observed first-upgrade
cut's raw/recovered state and runs the immutable actual old RowStore against each snapshot. The
`evidence-managed-engine-admission` artifact retains selection/semantic/cut logs, frozen build inputs,
toolchain text and exact before/after manifests. The
[completed 15-history/30-probe record](evidence/managed-index-40916386232.md) preserves its historical
source and finite scope. Fresh final-head checks are required after replay. The owner's
[specific managed-format acceptance/waiver](evidence/managed-index-owner-decision-2026-10-29.md) is
separate from the WAL guard decision and grants no checkpoint, release or phase-gate approval.

### Manual paired foundation diagnostic

`foundation-comparison.yml` builds the original Phase 1 reference and fixed candidate on the same
host, with one warmup each and nine alternating pairs at scale 5. Raw ledgers, commands, source/binary
hashes and failure receipts accompany diagnostic ratio/dispersion summaries. It changes no regression
budget and supplies no historical SQL baseline or ADR C4 verdict. Hosted execution is pending; see
[the protocol](foundation-comparison.md) before dispatching or interpreting results.

### Merge procedure checks that stay with the owner

`tools/merge_pr.py` checks required status contexts, the head it validated and branch freshness. It
does not check that the pull request comes from this repository rather than a fork, and it does not
read review state (an approving review, the `reviewed` label on risky paths, an unresolved change
request). Those are owner-procedure checks: before queueing a merge, confirm the head repository is
`moncifem/latticedb` and that the review requirements for the touched paths are met. A green merger
run is not evidence of either.
