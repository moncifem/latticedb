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
