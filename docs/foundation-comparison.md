# Same-host foundation comparison diagnostic

Status: manual diagnostic, not yet measured by this workflow. It does not evaluate ADR 0013 C4 or
change any criterion, Phase 1 tolerance, phase gate or planner decision. In particular, it cannot
manufacture the historical Phase 1 SQL baseline that does not exist.

`foundation-comparison.yml` builds the actual original foundation benchmark at
`96d409f7ecfc8b36418edcf1ceb2478030f68236` (the 2026-10-28 hosted baseline source) and candidate
`45926c7fc1f6fa71a7a69ac858c805ab1d9679b0` on one GitHub-hosted Ubuntu runner. It preserves both source
checkouts, original lockfiles/toolchain files and release profiles. Builds are `--locked --release`,
`-p lattice-bench --bin bench_runner`, default features, two jobs, incremental compilation off and empty
Rust flags, with separate clean target directories. This is not a clean-build timing comparison.

The runner records actual rustc/Cargo versions, host CPU/platform, workflow run/attempt/controller
revision and hashes of every tracked source file, both lockfiles/toolchain files and each executed
binary. It refuses mismatched revisions, dirty checkouts or changed source. No production or benchmark
logic is replaced. Both binaries run at scale 5 against `ci-ubuntu`, with the existing tolerance 2.0
(3x baseline) unchanged. That existing gate verdict is distinct from paired timing ratios; even a
successful diagnostic does not declare no regression.

One warmup per binary precedes nine measured pairs. Pair zero runs baseline then candidate; pair one
reverses the order, alternating thereafter. The workload set must be exactly: single CPU commit,
batch-of-16 CPU commit per item, HLC tick, LSL parse/check, ORSet merge, 100-byte WAL codec and per-record
kernel reopen. These are synthetic CPU/SimIo foundation workloads, not raw RowStore point/range tests
or SQL on the issue-tracker dataset. Distinct implementations/optimizations at the two revisions are
intentional; the workload names, scale and input meanings must remain comparable on source review.

Each warmup/sample has a fresh raw ledger, stdout/stderr, exit status, command/cwd, timestamps and
hashes. The independent Python reader validates WAL framing/CRC32C and the ChangeSet envelope before
accepting the exact profile, scale, seven positive finite timings and one fresh record. Nonzero exits,
timeouts and malformed/missing results remain in the artifact and make the diagnostic incomplete;
there is no successful subset selection. Interrupted jobs retain available receipts/logs through an
always-run upload. Build failures retain available metadata/logs and a failure report.

For complete measured coverage the summary reports all nine candidate/baseline ratios per workload
through min/median/max, median absolute deviation and sample standard deviation, alongside both arms'
ns/op distributions. It sets `adr_c4_status=not_evaluated` regardless of ratios. Shared virtual hosts
can still vary in load/frequency during a run; pairing reduces between-host confounding without proving
statistical equivalence or defining an acceptance threshold. No confidence guarantee is implied.

Dispatch `foundation-comparison.yml` manually once it is available on the default branch. Preserve its
run ID, source SHA, artifact ID/digest, metadata and all raw sample receipts when reporting observations.
A branch dispatch may be unavailable until the workflow has reached the default branch. Heavy Rust
builds and benchmark execution run only on hosted CI; local checks cover the Python validator and YAML.
