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
