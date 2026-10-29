# Public API snapshots

The public API of each library crate is committed as a text snapshot in `api-snapshots/<crate>.txt`
(`lattice-abi`, `lattice-seams`, `lattice-lsl`, `lattice-sim`, `lattice-core`, `lattice-engines`,
`lattice-api`, `lattice-sql`, `lattice-sdk`, `lattice-evalkit`). The `public-api` job in
`.github/workflows/pr.yml` regenerates them and fails when they differ, so a breaking or
accidental API change is an explicit diff in review.

## Policy

- A pull request that does not change a crate's public API does not touch its snapshot.
- A pull request that changes the API (add, rename, remove or change a public item) updates the
  snapshot in the same pull request and mentions the API change in its description.
- Removing or changing an existing item is a breaking change: call it out in `CHANGELOG.md`.
- Never edit a snapshot by hand; regenerate it.

## Commands

```bash
tools/check_public_api.sh                       # check all crates that have a snapshot
tools/check_public_api.sh lattice-core          # check one crate
tools/check_public_api.sh --update lattice-core # rewrite one snapshot
tools/check_public_api.sh --update              # rewrite all snapshots
```

A crate without a snapshot file is skipped with a warning; `--update <crate>` creates it.

## Toolchain pins

- `cargo-public-api` 0.52.0: `cargo install cargo-public-api --version 0.52.0 --locked`.
- Nightly `nightly-2026-10-27`, used only to build rustdoc JSON (install with
  `rustup toolchain install nightly-2026-10-27 --profile minimal`). Override with `API_NIGHTLY`
  (for example `API_NIGHTLY=nightly`) when working with another nightly; the output must stay the
  same, so a differing snapshot from a newer nightly is a reason to bump the pin in one place,
  not to commit the newer output ad hoc.

To bump a pin, change `CPA_VERSION` / `API_NIGHTLY` in `tools/check_public_api.sh` and the matching
versions in the `public-api` job, regenerate every snapshot and commit them together.

Snapshots list the default-feature API without blanket and auto-trait impls (`-ss`) and
without function parameter names, so renaming a parameter does not show as a change.

## Keeping snapshots current

Updating the snapshot is part of any pull request that changes a public API. Several open pull
requests change APIs at the same time, so a merge conflict in `api-snapshots/` is expected: resolve it
by rebasing and re-running `tools/check_public_api.sh --update` for the affected crates, never by
merging the text by hand.
