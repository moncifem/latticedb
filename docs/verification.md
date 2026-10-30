# Verification with Kani

[Kani](https://github.com/model-checking/kani) is a bounded model checker for Rust. A harness
(`#[kani::proof]`) runs a function on symbolic inputs and checks that no assertion, panic,
overflow or out-of-bounds access can happen, for every input inside the stated bounds.

**A bounded proof is not a full proof.** Each harness below holds for the inputs it covers and says
nothing about larger ones. The unwind bound, which must exceed the largest loop in the harness, is
recorded with `#[kani::unwind(N)]` next to each harness, together with the input bound in its doc
comment. These proofs complement, and do not replace, the unit, property, fuzz and simulation tests.

Each harness file is a `proofs.rs` beside the source it checks (`feed/proofs.rs`,
`crdt_tree/proofs.rs`, `sync/proofs.rs`, `contract/proofs.rs`), declared by a two line
`#[cfg(kani)] mod proofs;` at the end of the source file, so editing a proof does not touch the
risk-area source. They are **not compiled by any ordinary build, test or lint run**; only the `kani`
workflow compiles them. The `kani` cfg is declared in the workspace `unexpected_cfgs` lint.

Some harnesses carry `kani::cover!` statements. They fail the run if the success path they name
(a frame decodes, a line is returned) is unreachable inside the bound, so a proof cannot pass
because every input is refused.

## Running

The `kani` workflow (`.github/workflows/kani.yml`) runs weekly and on demand; it is not part of the
pull-request gate because it is heavy. It installs the pinned `kani-verifier` with
`cargo install --locked`, caches the install, and runs `cargo kani -p <package>` for
`lattice-core`, `lattice-api` and `lattice-sdk` with `--harness-timeout 20m`. To run one locally:

```bash
cargo install --locked kani-verifier --version 0.68.0 && cargo kani setup
cargo kani -p lattice-core --harness cursor_round_trips
```

## Proofs

| Harness | File | Input bound | Unwind | Establishes |
| --- | --- | --- | --- | --- |
| `cursor_round_trips` | `lattice-core/src/feed.rs` | all `u64` lsn, all `u32` index | 14 | `Cursor::new` then `to_bytes` then `from_bytes` returns the same cursor for every nonzero lsn; the origin round trips; lsn 0 is refused by `new`. |
| `cursor_order_matches_lsn_index_and_bytes` | `lattice-core/src/feed.rs` | all pairs of nonzero cursors | 14 | Cursor order equals `(lsn, index)` order, and equals the order of the encoded 12 bytes compared as unsigned strings. |
| `cursor_decode_never_panics` | `lattice-core/src/feed.rs` | slices of 0 to 14 bytes | 16 | `Cursor::from_bytes` never panics; it accepts only 12 byte inputs and re-encodes what it accepts to the same bytes. |
| `event_decode_never_panics` | `lattice-core/src/feed.rs` | slices of 0 to 80 bytes | 82 | `ChangeEvent::decode` never panics, including past the fixed header into the key and operation tag. |
| `frame_decode_never_panics` | `lattice-api/src/sync.rs` | slices of 0 to 7 bytes | 9 | `sync::decode` never panics, and a returned frame length is within the input and at least the header. At 7 bytes no complete frame fits (the smallest, a ping, is 13), so this covers the header and refusal paths only and the length claim holds vacuously. Not proven, see #425. |
| `frame_decode_with_limit_never_panics` | `lattice-api/src/sync.rs` | 0 to 7 bytes, any payload limit | 9 | The same for `decode_with_limit`, and the payload length never exceeds the limit or `MAX_PAYLOAD`. Same 7 byte limit: header and refusal paths only, the length claims hold vacuously. Not proven, see #425. |
| `framer_bounds_hold` | `lattice-sdk/src/contract.rs` | 3 bytes in two arbitrary pushes | 5 | `Framer::push` never panics, yields one result per newline, returns no line holding a newline or longer than its input, and never buffers more than `MAX_LINE_BYTES`. Not proven, see #425. |
| `framer_finish_resets` | `lattice-sdk/src/contract.rs` | 0 to 3 bytes | 5 | `Framer::finish` never panics and leaves the framer empty and not skipping. Not proven, see #425. |
| `move_into_own_subtree_is_a_no_op` | `lattice-core/src/crdt_tree.rs` | two-node chain, one later move, any position | 8 | A move of a node under itself or its descendant is `Skipped`, leaves the snapshot unchanged and the tree acyclic. Not proven, see #424. |
| `would_cycle_matches_ancestry` | `lattice-core/src/crdt_tree.rs` | the same chain, all node pairs 0 to 3 | 8 | `would_cycle(node, parent)` is true exactly when `node` is `parent` or an ancestor of it. Not proven, see #424. |

## What these do not establish

- Larger inputs. Of the verified harnesses, the cursor decoder is proved for inputs of up to 14 bytes
  and the change event decoder for up to 80 bytes. Maximum-size keys, values, texts and event lists
  are beyond the bound; they stay with the property tests and the fuzz targets. The sync frame
  decoder is not proven at any size (#425).
- The line splitter's over-long-line branch. `MAX_LINE_BYTES` is 1 MiB, far beyond any bound Kani can
  unroll, so `framer_bounds_hold` shows the invariants on short inputs only; the limit itself is
  covered by unit tests.
- Convergence of the tree CRDT under reordering and duplication. The tree proof covers the cycle check
  on a two-node chain, not deep trees or the undo/redo replay; the property tests and the simulator
  cover convergence.
- Anything outside the harnessed functions, such as storage, the write-ahead log or concurrency.

## Status of the runs

**Verified on CI: 4 of 10 harnesses. Not proven: 6 of 10.** The 4 verified harnesses are in
`lattice-core`: `cursor_round_trips`, `cursor_order_matches_lsn_index_and_bytes`,
`cursor_decode_never_panics` and `event_decode_never_panics`. Each reported `VERIFICATION:- SUCCESSFUL`
in each of the last four dispatches of the workflow (runs 41001589752, 41005577558, 41017313757 and 41042663372,
from the branch of PR 407). No other claim is made.

The other 6 are written but not proven. No counterexample was found for any of them; they did not
finish:

| Harness | Package | What CI showed |
| --- | --- | --- |
| `move_into_own_subtree_is_a_no_op` | `lattice-core` | CBMC timed out (20 minutes) in all three runs, also after the targets were enumerated instead of symbolic |
| `would_cycle_matches_ancestry` | `lattice-core` | CBMC timed out (20 minutes) in all three runs, also after every node pair was enumerated |
| `frame_decode_never_panics` | `lattice-api` | 24 and 14 byte bounds: CBMC timed out; 7 byte bound: job killed by the runner ("shutdown signal", exit 143) while solving |
| `frame_decode_with_limit_never_panics` | `lattice-api` | same |
| `framer_bounds_hold` | `lattice-sdk` | job killed by the runner (exit 143) while solving, at 6 and at 3 bytes |
| `framer_finish_resets` | `lattice-sdk` | same |

The cause of the kills is not established. The solver stage was reached with a small formula (about
8,000 verification conditions at 3 bytes), so the bound alone does not explain it; the logs are full
of `assume(false)` lines from `alloc` `raw_vec`, which points at the `Vec` and `BTreeMap` code paths
of the code under test. Redesigning the tree harnesses over fixed arrays was considered and not done:
it would prove a model of the algorithm, not `Tree`. These harnesses need a different approach (for
example stubbing the allocator-heavy parts or splitting the harness) and are tracked in #424 (tree) and #425 (sync frame and framer).

The workflow follows from this. Job `verified` runs only the 4 verified harnesses (weekly and on
demand), fails unless it sees exactly 4 `SUCCESSFUL` and no `FAILED`, so a green run means those
proofs ran. Job `extended` runs everything on manual dispatch with `extended: true` and is expected
to fail until the 6 are fixed. Each job tees the Kani output to a log, uploads it as an artifact
(30 days, also on failure) and prints the verdict lines in the job summary.

| Package | Last run | Result |
| --- | --- | --- |
| `lattice-core`, 4 feed harnesses (job `verified`) | 41042663372 (branch of PR 407) | 4 of 4 `SUCCESSFUL`, 0 failures |
| `lattice-core`, 2 tree harnesses | 41017313757 | not proven: timed out |
| `lattice-api` | 41017313757 | not proven: runner killed |
| `lattice-sdk` | 41017313757 | not proven: runner killed |

There is no cheap compile check: the harnesses need the `kani` crate's `proof` and `unwind` attributes
and `any`, `assume` and `cover!`, which only Kani's own compiler driver supplies. A stand-in
proc-macro crate injected with `RUSTFLAGS='--cfg kani'` would be one more thing to maintain and would
not check Kani's semantics.

Toolchain: the workflow pins `kani-verifier` 0.68.0, which per the Kani releases page upgrades Kani's
bundled nightly to nightly-2026-08-21. Kani builds with that nightly, not with the repository's
pinned 1.99.1. Whether that nightly reports a rustc of at least 1.99.1 was not verified. Cargo refuses
a package whose `rust-version` is above the compiler, so the workflow lowers the `rust-version`
value (to 1.85, the minimum for edition 2024) in the root `Cargo.toml` `[workspace.package]` of its throwaway checkout before
proving, and fails if exactly one such line is not found (the member crates inherit it, so the line
must stay defined); that is harmless if the nightly is
new enough and the only change that lets the proofs build if it is older. The `--harness-timeout` flag
and `cargo kani -p` were not run against 0.68.0.

## Demonstration that a proof can fail

Not run yet. Because the tree harnesses do not finish, the cursor variant at the end of this section is the one that can be demonstrated first.

The check is that a deliberately broken variant of a function makes its harness fail. The planned
variant, not committed and not yet run, drops the cycle check from `Tree::perform` in
`crates/lattice-core/src/crdt_tree.rs`:

```diff
-        let valid = parent_known
-            && (bugs::active(Bug::TreeCycleCheckSkipped) || !self.would_cycle(op.node, op.parent));
+        let valid = parent_known;
```

With it, `move_into_own_subtree_is_a_no_op` must fail: the move of node 2 under itself is applied,
so the outcome is `Applied` and the first assertion is violated. Record the Kani output here when the
demonstration has been run:

```bash
# apply the diff above locally, then
cargo kani -p lattice-core --harness move_into_own_subtree_is_a_no_op   # expected: VERIFICATION:- FAILED
```

A second variant for the cursor proofs: change `to_be_bytes` to `to_le_bytes` for the lsn in
`Cursor::to_bytes`; `cursor_order_matches_lsn_index_and_bytes` must fail, and
`cursor_round_trips` too, because the decoder reads big endian.

## Scheduled fuzz targets and the SQL reference check

These two checks are not Kani proofs; they are recorded here because the same files guard them.

- **Fuzz matrix.** Every bin in `fuzz/Cargo.toml` except `trivial` is listed in the nightly, weekly and
  manual 24-hour matrices, and `tools/check_fuzz_matrices.py` (run by the pull request gate) also
  fails when `fuzz/seeds/<target>/` has no file, because the scheduled jobs pass that directory to
  cargo-fuzz. The checker covers 18 targets. Three were added with the Phase 3 decoders:
  `crdt_text_decode`, `crdt_tree_decode` and `conflict_decode`; each requires that an input that decodes
  re-encodes to the exact input bytes. Their 34 seeds come from `fuzz/seeds/generate_crdt_workflow_seeds.py`
  (Python `struct`, not the Rust encoders; `--check` verifies the tracked bytes). A one-off local read of
  every seed through the three decoders gave the intended accept or refusal for each. Registering a target
  is not evidence of a completed fuzz run: no scheduled run of the three new targets has been observed yet.
- **Ignored volume tests.** A test marked `#[ignore]` never runs in `cargo test`. The convergence and
  fencing volume tests (`convergence_at_gate_volume`, `superseded_turns_are_fenced_across_profiles_volume`,
  100,000 seeds per profile) were ignored and started by no workflow, although their comments said
  scheduled CI would run them. `nightly.yml` now runs each as its own job (`volume-convergence`,
  `volume-fencing`; 240-minute limit, listed in `report-failure`), with `--ignored` and the test name,
  and uploads the log as `volume-*`. Their run time on a hosted runner has not been measured, so a
  timeout on the first nights is possible, and they use fixed seeds, so every night repeats the same
  100,000. `tools/check_ignored_tests.py` (pull request gate) finds every attribute that contains
  `ignore` (`#[ignore]`, `cfg_attr`, multi-line, raw-string reasons) and fails when such a test is not
  a `phase N:` specification for a phase still listed in `pending_phases`, is not started with
  `--ignored` by a workflow that runs unattended (schedule, push or pull request trigger; the job is
  not `if: false` or gated on a manual input), and has no reasoned entry in `tools/ignored-tests.toml`.
  It also fails when it finds no ignored test, or an attribute it cannot classify. It does not follow
  `needs`, matrices or reusable workflows. The row fault volume tests are listed because nightly covers
  them through the `row_fault_runner` example.
- **SQL reference check.** `.github/workflows/sql-reference.yml` is the single definition of the check of
  the sqllogictest scripts against PostgreSQL 17 (pinned by digest). Pull requests that touch the scripts
  or `tools/slt_reference.py` run it, and `nightly.yml` calls it as a reusable workflow, so the digest, the
  service setup and the `sql-reference-report` artifact are no longer duplicated. The nightly run now
  uploads that report too. The first nightly run through the reusable workflow has not been observed.

- **Fuzz limits and corpus.** Until this change no scheduled run set `-timeout` or `-max_len`, so
  libFuzzer used 1200 s per input (longer than the 600 s nightly run) and 4096 bytes (below the 1 MiB
  limits of the SQL, pgwire, sync, WebSocket and plugin decoders), and the nightly job started from
  `fuzz/seeds/` every night. `fuzz/settings.toml` now gives every target a `-max_len` with its basis,
  and all three workflows pass `-timeout=20 -rss_limit_mb=2048`; the pull request gate fails when a
  target lacks an entry or a `fuzz run` omits the flags. The nightly job restores a corpus per target
  from `actions/cache`, merges it with `-merge=1` after the run and saves it only from the scheduled
  run on the default branch. None of this has run on a schedule yet: the timeout and size values are
  choices, no hang has been reproduced through the nightly job, and the cache hit rate and corpus
  growth are unobserved. The nightly fuzz job now has a 60-minute limit; that figure is a guess,
  because no run has timed its cold build (the job has no rust-cache) or the merge.