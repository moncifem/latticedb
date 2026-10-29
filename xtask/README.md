# xtask

Repository automation: `cargo xtask <command>` (see `cargo xtask help`). Runs the same checks as the
pull-request workflow (`cargo xtask ci`), the structural checks (layers, unsafe, banned-lint allowlist,
corpus, config formats, links, quarantine), the backlog, ADR and RFC tooling, and traceability.

Must not: be used by any other crate. It is tooling and is the only non-library crate allowed to read
the environment and spawn processes.


`cargo xtask gate --phase 1 --output target/gates/review-1` collects a new evidence package with
its source revision and working-tree status, copied spec/status/review documents, the complete exit
criterion list, and the invariant traceability output. `--ledger LEDGER_ROOT` snapshots only ledger
segments, hashes and preserves that raw snapshot, then runs `doctor` against it and `ledger check`
against a separate verification copy. Recovery repairs are visible by comparing segment hashes;
neither the source ledger nor the raw snapshot is repaired.
Failed observations fail the command after writing their diagnostics. The output directory must be
new, so old evidence cannot silently survive a re-run.

Document copies, declared criteria and hashes use the same captured bytes. Failed provenance or a
source revision/document change during collection fails the command.

Every criterion starts unverified and the review remains pending: the package does not infer a pass
from status prose or sign a gate. Long-running CI/fuzz/benchmark evidence and maintainer decisions
must still be attached and reviewed.

The report checks the revision, copied document hashes, and before/after Git status.
`provenance_passed` covers the revision and copied documents only; source-code
freshness remains explicitly unverified because status cannot detect edits within
an already dirty file. Review and rerun from a quiescent checkout before signing.

`cargo xtask scorecard [LEDGER_ROOT]` renders all ten Phase 1 scorecard sections before
checking the existing simulation thresholds. It requires Python 3 (standard library only).
A report distinguishes retained observations, declared source inventories, invalid evidence,
and missing measures; it remains unsigned and does not complete P1-77. Source names do not
prove invariant execution, microbenchmarks do not prove commit latency, and local unit tests
do not establish product eval gates. Mutation scores, eval confidence intervals, reviewed
risk status, north-star passes and product dogfood measures remain missing until measured.

For machine-readable reports, run `python3 tools/scorecard.py report --ledger DIR --output OUT`.
This writes `scorecard.json`, `scorecard.md`, and a JSON history export. Reading uses a temporary
copy of regular WAL segments and rejects recovery repairs without modifying the source.
It is not an atomic snapshot of a concurrent writer; stop writers before collecting.
`lattice ledger export --dir DIR` itself retains the existing ledger reader's recovery behavior.
`lattice ledger record --dir DIR --kind KIND --json FILE` accepts an object of at most 64 KiB
and a bounded ASCII kind, using the existing opaque JSON payload format.

Nightly preserves per-shard simulation ledgers, completed benchmark comparison verdicts,
actual libFuzzer logs, and immutable job metadata. An always-run collector merges retained
history into a single-writer Lattice ledger, deduplicating repository/run/attempt/job/record
keys. The collector does not alter producer exit codes or workload budgets. Cancelled,
failed, timed-out and incomplete fuzz runs contribute no completed fuzz time. Older benchmark
records without verdicts remain readable as measurements only. Writing a new baseline or
failing to load a baseline does not create comparison-pass evidence.

History artifacts expire after 90 days. Acceptance checks the producer workflow ID/path,
repository/head repository, branch, completed run and exact run/attempt/revision manifest.
A mismatched artifact fails collection. These checks authenticate the workflow artifact
boundary; retained payloads remain producer observations rather than independent attestations.
History may contain gaps, reruns and cancelled collectors, so totals are observed lower bounds,
never lifetime budgets. Reports retain both per-result provenance and artifact origins.

Focused checks: `python3 -m unittest discover -s tools -p 'test_scorecard.py'`, CLI integration
tests, and benchmark runner integration tests. Nightly runs the Python checks before ingestion.

Mutation and eval observations are supported by `tools/scorecard_metrics.py`. Nightly reads the
latest retained completed **weekly workflow's** `mutants` artifact from the same repository/ref,
checking the configured weekly workflow ID/path, head repository, exact run/revision, completed
mutation job and artifact identity. Future weekly artifacts include an exact run/attempt manifest.
Legacy artifacts are accepted only for attempt one; ambiguous rerun artifacts are rejected.
The weekly run may have failed elsewhere: its conclusion and the mutation job conclusion are
retained independently. Existing weekly commands, budgets and failure propagation are unchanged.

Mutation ingestion supports the cargo-mutants 27 JSON schema (`outcomes.json` plus the generated
`mutants.json` universe); unknown versions, inconsistent counters, duplicate outcomes and malformed
archives fail collection. Reports show per-package/source-file planned, tested, unrun, killed,
survived, timeout, unviable and unclassified counts, plus missing artifact groups. The score is
`killed / (killed + survived)`: timeout, unviable, unclassified and unrun cases are excluded from
that denominator, never counted as kills. A partial/cancelled run, failed/missing baseline or empty
denominator has no verified module score. Timeouts and survivors preclude an observed clean module.
No module observation implies that unreported modules, other mutation groups or the project passed.
Raw JSON SHA-256 hashes and immutable run/job/artifact provenance travel with each observation.

For an actual external Evalkit suite result, call the additive `report::to_evidence_json` producer
API with its Dataset, provider, exact thresholds and GateReport. It checks the dataset content hash,
sample-count bounds and existing gate convention, preserves round-trip floating-point values,
and includes dataset name/version/split/provenance plus each metric's confidence and tolerance.
The legacy `report::to_json` output and signature stay unchanged.

Import an actual report explicitly:

```sh
python3 tools/scorecard_metrics.py ingest-eval --ledger DIR --lattice /absolute/path/to/lattice \
  --report actual-report.json --source actual-producer.json
```

The source file must identify repository, run_id, attempt, job_id and revision. This explicit local
association is labelled caller supplied; it is not authenticated as a GitHub artifact. The importer
retains a hash of the exact input and deduplicates it, checks finite values/interval/sample counts,
and recomputes each metric and suite verdict from the exact bounds/tolerances. This verifies the
reported gate fields, not independent execution of the provider or statistical recomputation of
its intervals. Old reports missing confidence/tolerance/threshold provenance remain visible with
an **unverified** gate verdict. Invalid reports fail ingestion/rendering. No example suite, fixture,
or unit-test output is automatically ingested as a project eval result. Suites without real reports
remain missing, and P1-77 remains incomplete until its actual evidence and review requirements hold.

A completed nightly's retained producer evidence can be replayed without rerunning its workloads.
Dispatch the registered `io-probe` workflow with `scorecard_source_run` set to its positive numeric
run ID. An empty input keeps the normal I/O probe behavior; the explicit replay mode runs only a
bounded ledger CLI build and evidence collection. It does not restart the original workflow.

The replay requires the configured nightly workflow ID/path, trusted repository/head repository,
main ref, completed source and jobs, exact source SHA, and one matching artifact per producer job.
Current per-producer artifacts identify their run but carry no attempt manifests, so replay accepts
only an unambiguous first source attempt. It rejects rerun sources rather than assigning old
artifacts a new attempt. Each downloaded archive's SHA-256 must match GitHub's artifact digest;
archive expansion, paths and entry counts are bounded. Missing, expired or mismatched artifacts fail.

`tools/scorecard_replay.py` uses the same supported raw `gh api --paginate --slurp` then separate
`jq` normalization as nightly. It restores the source ledger, checks replay idempotence and existing
simulation thresholds, and retains all original source conclusions. A clean committed replay
checkout, its separate code revision and the ledger CLI binary hash are recorded. Replay artifacts
are named `scorecard-replay-*`, so they cannot accidentally become normal retained history. Reports
remain unsigned; successful collection does not change the original workflow outcome or sign a gate.
