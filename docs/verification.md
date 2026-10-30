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
| `frame_decode_never_panics` | `lattice-api/src/sync.rs` | slices of 0 to 24 bytes | 26 | `sync::decode` never panics, and a returned frame length is within the input and at least the header. |
| `frame_decode_with_limit_never_panics` | `lattice-api/src/sync.rs` | 0 to 24 bytes, any payload limit | 26 | The same for `decode_with_limit`, and the payload length never exceeds the limit or `MAX_PAYLOAD`. |
| `framer_bounds_hold` | `lattice-sdk/src/contract.rs` | 6 bytes in two arbitrary pushes | 10 | `Framer::push` never panics, yields one result per newline, returns no line holding a newline or longer than its input, and never buffers more than `MAX_LINE_BYTES`. |
| `framer_finish_resets` | `lattice-sdk/src/contract.rs` | 0 to 6 bytes | 10 | `Framer::finish` never panics and leaves the framer empty and not skipping. |
| `move_into_own_subtree_is_a_no_op` | `lattice-core/src/crdt_tree.rs` | two-node chain, one later move, any position | 8 | A move of a node under itself or its descendant is `Skipped`, leaves the snapshot unchanged and the tree acyclic. |
| `would_cycle_matches_ancestry` | `lattice-core/src/crdt_tree.rs` | the same chain, all node pairs 0 to 3 | 8 | `would_cycle(node, parent)` is true exactly when `node` is `parent` or an ancestor of it. |

## What these do not establish

- Larger inputs. The decoders are proved up to 24 to 80 bytes. Maximum-size keys, values, texts and
  event lists are beyond the bound; they stay with the property tests and the fuzz targets.
- The line splitter's over-long-line branch. `MAX_LINE_BYTES` is 1 MiB, far beyond any bound Kani can
  unroll, so `framer_bounds_hold` shows the invariants on short inputs only; the limit itself is
  covered by unit tests.
- Convergence of the tree CRDT under reordering and duplication. The tree proof covers the cycle check
  on a two-node chain, not deep trees or the undo/redo replay; the property tests and the simulator
  cover convergence.
- Anything outside the harnessed functions, such as storage, the write-ahead log or concurrency.

## Status of the runs

The harnesses have never been compiled or run. Kani was not executed on the machine they were written
on (it is heavy, and the owner's rule is to leave long jobs to CI), and because they sit behind
`cfg(kani)` no normal check, test or clippy run compiles them. The first run of the `kani` workflow is
the first compile and the first execution. A harness that fails to compile, fails, or exceeds its 20
minute timeout there needs fixing or a lower bound. `event_decode_never_panics` (80 bytes, unwind 82) is
the likeliest to need one; going below 73 bytes (the smallest valid event) makes its `cover!` of a
successful decode unreachable, so it would then only show that refusals do not panic.

There is no cheap compile check: the harnesses need the `kani` crate's `proof` and `unwind` attributes
and `any`, `assume` and `cover!`, which only Kani's own compiler driver supplies. A stand-in
proc-macro crate injected with `RUSTFLAGS='--cfg kani'` would be one more thing to maintain and would
not check Kani's semantics.

Toolchain: the workflow pins `kani-verifier` 0.68.0, which per the Kani releases page upgrades Kani's
bundled nightly to nightly-2026-08-21. Kani builds with that nightly, not with the repository's
pinned 1.99.1. Whether that nightly reports a rustc of at least 1.99.1 was not verified. Cargo refuses
a package whose `rust-version` is above the compiler, so the workflow lowers the `rust-version`
value (to 1.80) in the root `Cargo.toml` `[workspace.package]` of its throwaway checkout before
proving, and fails if exactly one such line is not found (the member crates inherit it, so the line
must stay defined); that is harmless if the nightly is
new enough and the only change that lets the proofs build if it is older. The `--harness-timeout` flag
and `cargo kani -p` were not run against 0.68.0.

## Demonstration that a proof can fail

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
- **SQL reference check.** `.github/workflows/sql-reference.yml` is the single definition of the check of
  the sqllogictest scripts against PostgreSQL 17 (pinned by digest). Pull requests that touch the scripts
  or `tools/slt_reference.py` run it, and `nightly.yml` calls it as a reusable workflow, so the digest, the
  service setup and the `sql-reference-report` artifact are no longer duplicated. The nightly run now
  uploads that report too. The first nightly run through the reusable workflow has not been observed.
