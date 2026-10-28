# Contributing

1. Read `docs/conventions.md` and the phase you are working on in `docs/spec`.
2. Pick an open issue (see `docs/project-management.md`), or open one. Link the issue in the pull request (for example `Part of #44`).
3. Branch from `main`, keep the change small, and run `cargo xtask ci` before opening a pull request.
4. When a change alters what works, how to run it, or the project status, update `README.md` in the same pull request.
5. Use Conventional Commits with the crate or directory name as scope, for example
   `feat(lattice-core): add commit pipeline`. When a commit implements a spec requirement, name it in a `Requirement: P1-07` trailer.
6. Fill in the pull-request template: invariants touched, tests added, eval or benchmark deltas,
   format change flag.
7. Format or wire changes need an RFC (`docs/rfc`) and an ADR (`docs/adr`).

By contributing you agree that your work is licensed under the Apache-2.0 licence of this repository.

## Setup

```bash
rustup toolchain install 1.99.1 --component rustfmt clippy
cargo install cargo-deny typos-cli --locked
cargo xtask setup      # installs the git hooks (format, layers, unsafe, commit message)
cargo xtask ci         # everything the pull-request workflow runs
```

A dev container is provided in `.devcontainer/`.
